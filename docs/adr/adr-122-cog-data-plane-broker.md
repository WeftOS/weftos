# ADR-122: Cog data plane: a broker in the node daemon

- **Status**: Accepted (2026-10-25: the owner asked to start B1, taking the recommended
  answer to each open question; see "Decisions on the open questions"). Card
  `venq-cog-broker-adr`; implementation on cards `venq-cogbus-b1` .. `b6`.
- **Deciders**: owner
- **Builds on**: ADR-099 (governed placement, node facts), ADR-100 (cog workload kind),
  ADR-120 (exposures: announce, see, switch off, log), ADR-114 (`weftos://` names),
  ADR-117 (display bindings subscribe to mesh data), ADR-031 (JSON `KernelMessage` on the
  mesh; RVF deferred), ADR-111 (SenML sensor profile).
- **Plan**: [`docs/plans/ventuno-q-cog-plan.md`](../plans/ventuno-q-cog-plan.md) sections
  4.1–4.5 and owner decisions 1 and 2 (2026-10-25).
- **Prior decisions**: the decision-record store could not be consulted when this was
  written (2026-10-25, Ruflo unavailable), so earlier recorded decisions on this topic are
  unknown here. The ADRs above were read instead.

## Context

A cog has one way to hand data to anything else: `POST /api/v1/store/ingest` to the ingest
bridge (`crates/clawft-weave/src/cog_ingest_serve.rs`) with an 8-float vector. The bridge
rate-limits and forwards to the project store as a signed `MeshIpcEnvelope`
(`crates/clawft-kernel/src/mesh_ipc.rs`). That is enough for a radar summary and nothing
else:

- no cog can subscribe to another cog's output, so every consumer re-opens the device
  (the ESP32 UDP :5006 contention is the known case);
- camera frames, PCM audio, CAN traffic and SDR IQ samples have no path at all;
- the bridge binds `127.0.0.1:80`, one daemon per host wins, and an unprivileged daemon
  cannot bind it (the Ventuno's `board-telemetry` cog gets "connection refused" today);
- nothing records who reads what.

P1 (f854f9611) already gives cogs a declaration: `[[outputs]]` and `[[inputs]]` in
`cog.toml` with a topic kind, a registered schema id (`contracts/cogs/`), a class
(`reading`, `stream`, `blob`) and a reader policy. This ADR decides what carries those
declarations at run time.

## Decision

### 1. The broker is a service of the node daemon

`weaver` runs the broker, called `cogbus`, on every node that hosts cogs. It is not a cog
and not a separate system service. The daemon already holds the node key, the mesh
connection, governance and the workload registry, so the broker reuses them instead of
building a second network stack and a second permission model (owner decision 1).

### 2. Names

A live topic is `<kind>/<source_id>`, for example `audio.pcm/venq-mic-0`. `kind` is the
declared topic kind; `source_id` is the installed sensor or source, stable if the part moves
to another node (`docs/architecture/sensor-node.md`). Outside the node the same topic is

```text
weftos://<mesh>/sensors/<source_id>/topics/<kind>
```

which adds a `topics/<kind>` sub-path to ADR-114's `sensors` kind (today "none"). Sources
that are not sensors (an NPU service, an assistant) use `services/<name>/topics/<kind>`.
ADR-114 is Proposed, so the sub-paths are added there when this is accepted.

### 3. Three classes, three carriers

| Class | Carrier on the node | Limit |
|---|---|---|
| `reading` | a message through the broker socket | 64 KiB per message; larger is a blob |
| `stream` | a shared-memory ring per topic: one writer, many readers, each reader with its own cursor | ring size declared per output (default 4 MiB) |
| `blob` | written once to the node's content store, addressed by BLAKE3; messages carry the hash, size, media type and TTL | per-node quota (default 2 GiB), TTL-evicted |

Every message and ring record carries `source_id`, `node_id`, `t_ns` (the producer's capture
clock) and `seq`; the broker adds its own receive time beside `t_ns`, never instead of it.
A gap in `seq` is how a reader sees a drop.

### 4. Control socket and identity

The broker listens on a Unix socket in the daemon's runtime dir (`<runtime>/cogbus.sock`,
mode 0660, owned by the daemon user and a `cogbus` group). The workload runtime starts each
cog with two environment variables, `WEFTOS_COGBUS` (the socket path) and
`WEFTOS_COGBUS_TOKEN` (a per-instance token), the same way it hands out
`COGNITUM_COG_TOKEN` today. The token names the cog instance and its manifest; the broker
also checks `SO_PEERCRED` against the uid the runtime started it as. A cog can only declare,
publish and subscribe as itself.

Frames on the socket are length-prefixed JSON, matching the mesh's JSON `KernelMessage`
(ADR-031). A stream ring is handed to the writer and to each reader as a `memfd` passed over
the socket (`SCM_RIGHTS`), so no stream byte goes through the socket.

### 5. What a cog may do

- **Publish** only topics in its `[[outputs]]`, with the declared schema and class. The
  broker validates schema ids, not every payload; the producer owns payload validity, and
  the schema's golden vectors are how that is tested.
- **Subscribe** only to topics matching its `[[inputs]]` kind and schema.
- **Cross-project** subscriptions need an explicit grant; the default is the cog's own
  project.
- Every declare, subscribe, refusal and unsubscribe is a governance decision
  (`cogbus.publish` / `cogbus.subscribe` as `GovernanceRequest` actions,
  `crates/clawft-kernel/src/governance.rs`) and lands in the node's activity log (ADR-120
  §5). Individual messages are not chain events.

### 6. Reader policies

| Policy | Behaviour |
|---|---|
| `drop-oldest` (default) | the reader's cursor skips forward; the writer never waits |
| `sample` | the reader gets the newest record when it asks; nothing queues |
| `block` | the writer waits for the slowest `block` reader, up to a deadline (default 50 ms); past it that reader becomes `drop-oldest`, the switch is logged and the reader is told |

No reader can stall a writer indefinitely. `block` exists for local pipelines that must not
lose data (a recorder) and is refused across nodes.

### 7. Raw data stays where it is made

Raw frames (`video.frame-ref.v1` blobs) and IQ streams (`iq.cs16.v1`) are served to
consumers on the same node. A cross-node subscription to them is refused unless the
subscription asks for raw data and the exposure allows it (owner decision 2). What leaves a
node by default is derived readings: detections, transcripts, spectrum summaries, SenML.

### 8. Across the mesh

A topic is offered to other nodes only as an ADR-120 exposure of kind `topic`, with an
`audience` (default `local`) and limits (rate, bandwidth). Nothing is exposed by default.

- A remote subscriber's broker sends the subscribe to the source node's broker as a
  `MeshIpcEnvelope`, and the source broker runs the same governance check against the
  exposure and the peer's grant.
- Readings travel as one mesh message each.
- Streams travel in chunks with credit-based flow control: the receiver grants credits,
  and the sender stops when they run out. A slow link drops by the subscriber's policy
  at the source; it does not queue without bound.
- Blobs travel only when fetched by hash.
- ADR-117 display bindings (`bind ... ← weftos://<mesh>/sensors/<id>`) resolve through
  this path; the leaf is a remote subscriber.

### 9. The ingest bridge becomes a consumer

The bridge's HTTP contract (`POST /api/v1/store/ingest` with `COGNITUM_COG_TOKEN`) keeps
working for released cogs, but as a compatibility listener inside the broker: each post is
published as a reading on `store.vector/<cog_id>`, and a built-in store sink subscribes to
`store.vector/*` and forwards to the project store as today. The port-80 bind becomes
optional (`bridge.bind`), and new cogs never need it. The bridge cog's signed cross-node
`POST /ingest` stays until mesh topics (§8) replace it.

B4 as built (2026-10-25, `crates/clawft-weave/src/cogbus_ingest.rs`): the topic is
`store.vector/<instance_id>` rather than `<cog_id>`, because the store keys vectors by
instance and two placements of one cog in two projects on one node would otherwise contend
for one topic (an instance id that is not a valid source id becomes `i-<16 hex>` of its
BLAKE3). The schema is `store.vector.v1`. The bridge publishes as the broker-internal writer
`@ingest-bridge` and waits for the store sink's answer, so the HTTP reply still carries the
store's counts. `bridge.bind` may be `"off"`, and `bridge.fallback_bind` names a loopback
listener used when the primary bind fails.

### 10. What can be seen

- `weaver cogbus list` shows topics, schema, class, writer, readers, rates and drop counts.
- The node's dashboard report gains a topics section beside exposures (ADR-120 §6).
- Placement admits a consumer only where its declared input topics are reachable: published
  on that node, or exposed to it within its bandwidth.

### 11. Failure

- A broker restart (daemon restart) drops connections; cogs reconnect with the same token
  and redeclare. Rings are recreated; readers resume at the writer's head and see the `seq`
  gap.
- A writer that dies leaves its ring readable until a new writer for the same topic
  declares.
- A cog that publishes undeclared topics or exceeds its declared rate is refused and
  logged; repeated refusals stop the cog through the workload runtime.

## Alternatives considered

| Option | Why not (as the broker) |
|---|---|
| Separate broker daemon | a second process with its own permission model, mesh link and lifecycle (owner decision 1) |
| MQTT (Mosquitto) | mature and simple, but its own auth and ACLs beside governance, no fd passing for zero-copy streams, and every reading becomes a broker hop with its own retained state |
| Zenoh | the closest fit: pub/sub, shared memory and routing in one Rust library (Apache-2.0/EPL-2.0). It would duplicate the mesh's routing, discovery and trust, which WeftOS already has. Worth revisiting as the stream transport only |
| NATS | a separate server and a network hop for local data |
| D-Bus | not meant for sustained streams; message size and throughput limits |
| Raw cog-to-cog sockets | every pair negotiates its own format; no governance, no discovery, no drop accounting |

For the stream rings themselves, `iceoryx2` (Rust, Apache-2.0/MIT, zero-copy shared-memory
IPC) is evaluated in B2 before writing our own `memfd` ring. The decision record for that
choice is the B2 result.

## Consequences

- Cogs get a client library (Rust first; a thin C ABI for firmware-side bridges later)
  that wraps declare, publish, subscribe and ring access. Existing cogs keep working
  through §9 without changes.
- The broker is the one place device-owner cogs (`audio-in`, `camera`, `mcu-bridge`,
  `sdr-pluto`) fan out to consumers, which is what makes the one-owner-per-device rule
  (plan §4.1) workable.
- Governance and the activity log see every subscription, so "who reads the microphone"
  has an answer.
- The daemon carries more load. The broker's memory and CPU are reported in node facts so
  placement can see them.

## Phases

| Phase | Work | Done when |
|---|---|---|
| B1 | socket, identity, declare/publish/subscribe for readings, `cogbus list` | two cogs exchange readings; an undeclared publish and a wrong-schema subscribe are refused and logged |
| B2 | stream rings (iceoryx2 evaluated first), reader policies | 16 kHz PCM between two cogs for an hour with no drops; a slow `drop-oldest` reader does not stall the writer; `block` falls back at its deadline |
| B3 | governance actions, activity log, dashboard section | each subscribe and refusal is visible in the activity log and dashboard |
| B4 | ingest compatibility listener and store sink | released cogs posting to the bridge reach the store unchanged; the Ventuno's `board-telemetry` reaches the store without port 80 |
| B5 | blob store by hash, TTL and quota | a frame handle resolves locally and expires on TTL |
| B6 | mesh topics as ADR-120 exposures, credits, raw-data refusal | a PG cog subscribes to a Ventuno reading topic; a raw IQ subscription from PG is refused unless exposed and requested |

B1–B4 are the plan's P2. B5–B6 are P6.

## Decisions on the open questions (2026-10-25)

The owner took the recommended option for each:

1. **Stream rings**: evaluate iceoryx2 first in B2; write our own `memfd` ring only if it does not fit.
2. **Reading encoding on the socket**: length-prefixed JSON, matching the mesh today.
3. **Audit depth**: governance decisions and the activity log for declare, subscribe and
   refuse; no chain event per subscription.
4. **Defaults**: 4 MiB per stream ring and 2 GiB of blobs per node.
