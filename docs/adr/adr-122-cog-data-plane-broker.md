# ADR-122: Cog data plane: a broker in the node daemon

- **Status**: Accepted (2026-10-25: the owner asked to start B1, taking the recommended
  answer to each open question; see "Decisions on the open questions"). Card
  `venq-cog-broker-adr`; implementation on cards `venq-cogbus-b1` .. `b6`.
- **Updated**: 2026-10-25 (B1 and B4 shipped; known gaps recorded with closing cards; section 12 adds service calls for the NPU service, card `d9272837`; gaps 2 and 3 closed in code, pending the owner-run host setup; B5 blob store, section 13)
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
publish and subscribe as itself. How the uid and the group are real is §4a.

Frames on the socket are length-prefixed JSON, matching the mesh's JSON `KernelMessage`
(ADR-031). A stream ring is handed to the writer and to each reader as a `memfd` passed over
the socket (`SCM_RIGHTS`), so no stream byte goes through the socket.

### 4a. Per-cog uid and the `cogbus` group (gaps 2 and 3)

A user daemon cannot switch uids; the machine service (root, or holding `CAP_SETUID` and
`CAP_SETGID`) can. The runtime therefore has two modes, decided at each placement and shown
in `weaver cogbus list`, never implied.

| | `per-cog-uid` | `shared-uid` |
|---|---|---|
| when | the daemon can switch uids, `cogbus-isolation.json` has a `uid_range`, and the group exists | anything else: a user daemon, no range, no group, range exhausted |
| the cog runs as | the lowest free uid of the range, gid `cogbus`, supplementary groups exactly `[cogbus]` (the daemon's are dropped) | the daemon's uid, as before |
| broker check | the instance is registered with that uid; any other peer uid is refused `uid_mismatch` and logged by the activity log (`record_refusal`), with the instance named | the peer must be a process of the daemon's user |
| `require_cog_uid` | n/a | refuses a placement whose cog declares `[[outputs]]` or `[[inputs]]` (`admission`: "require_cog_uid is on and this cog would share a uid"). Cogs that do not use the bus are not refused. Default off |

- **Allocation.** `attach` takes a uid from the pool; `register` binds it to the instance. A
  placement that fails between the two gives it back at once (the pending guard drops it). A
  stop keeps it, because the instance's data directory is owned by that uid and a restart
  must not meet another cog there. Unload, teardown and revocation free it. The pool is in
  memory: a daemon restart forgets instances, and so their uids.
- **Spawn.** The identity travels in `HostContract.run_as` to the native adapter, which
  stages the data directory as that uid and launches with `setgroups`, `setgid`, `setuid`
  in that order (`Supervised::spawn`, uid 0 still refused). An adapter-level `run_as` stays
  the fallback.
- **Socket.** Created `0660`, never wider. If the `cogbus` group exists the daemon gives
  the socket to it (`chown :cogbus`); with the machine service that is `root:cogbus 0660`.
  If the group is absent or the daemon may not assign it, the group stays the daemon's own and
  the report says why (`socket_note`). The socket's directory and its parents must be `o+x`
  for the cog uids to reach it; `scripts/cogbus-uid-setup.sh status` reports any that are not.
- **Host setup is owner-run.** Creating the group, reserving a uid range and writing
  `cogbus-isolation.json` are `scripts/cogbus-uid-setup.sh apply|undo|status`: a dry run
  unless `--yes`, a manifest of exactly what it created, and an undo that removes only that.
  The daemon never edits the host.

```json
{ "require_cog_uid": false, "uid_range": [60000, 60999], "group": "cogbus" }
```

`cogbus-isolation.json` lives in the runtime dir, owner-only like `cogbus-policy.json`; a
file that does not parse or is group- or world-writable is ignored with a warning and cogs
run shared.

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

### 12. Service calls

Some hardware is called, not streamed (plan 4.5: the NPU). The broker carries one
request/reply primitive, with the same checks as a reading and nothing else added. Serve,
call and reply are requests on a session that `hello` authenticated, so the peer-uid check of
§4a applies to them exactly as to a publish; a connection refused for its uid is closed
before any `serve` or `call` is read.

- **Name.** A service is a topic whose kind starts with `svc.`: `svc.infer/<node>`. Over
  the mesh it is `weftos://<mesh>/services/<name>/topics/<kind>` (section 2).
- **Declaration.** The provider lists the kind in `[[outputs]]` (class `reading`), each
  caller in `[[inputs]]`, both with the same registered schema (`infer.v1` for the NPU
  service). Placement, the manifest check and the project rule therefore already apply.
  `publish` and `subscribe` on a `svc.*` kind are refused (`unsupported`).
- **Wire.** `serve {topic_kind, source_id, schema}` makes the connection the provider of
  the topic (one instance per topic; a second is `writer_conflict`; the same instance
  reconnecting replaces its own connection). `call {topic, schema, body, timeout_ms}` is
  routed to it as a pushed `{"type":"call", call, from, timeout_ms, body, ...}` frame;
  `reply {call, body | error}` answers it. The caller's `call` request gets its answer
  (`result.body`) only then, so the client's request id is the call id on the caller's
  side and the broker-assigned `call` is the one on the provider's side.
- **Deadlines.** Every call has a deadline (`timeout_ms`, default 30 s, at most 10 min;
  queueing in the provider counts). At the deadline the broker answers the caller
  `timeout` and forgets the call; a later `reply` is refused (`bad_request`). The provider
  sees the remaining time in `timeout_ms` and should drop what it has not started.
  The server sweeps deadlines every 50 ms.
- **Failure.** A provider connection that closes, or an instance that is revoked, fails
  its pending calls with `unavailable`; a call to a topic nobody serves is `unavailable`
  at once. A provider `error` reaches the caller as `service_error` with the provider's
  message. A body over 64 KiB, either way, is `too_large`; larger input or output is a blob handle (§13).
- **Limits.** 16 calls pending per caller instance and 64 per service (`busy`), so one
  caller cannot fill the provider's queue. The provider's own queue and priorities are its
  business; the call body carries them (`infer.v1` has `priority`).
- **Governance.** `serve` asks `cogbus.serve` once; every `call` asks `cogbus.call` with
  the topic as `tool`, through the same single attributed `GovernanceRequest` site as the
  other actions (section 5). Refusals count toward the repeated-refusal stop; replies do
  not ask governance, since the call they answer was permitted. `cogbus-policy.json`
  deny rules and cross-project grants name the two actions like the others.
- **Not carried.** Streaming replies (a long generation) and cancellation. A reply is
  one body; the caller picks a `max_tokens` that fits. A blob handle in the reply (§13) carries a large result.

### 13. Blobs (B5)

A payload over the 64 KiB body limit is a blob: bytes in the node's content store, addressed
by BLAKE3, and a `blob.v1` handle (`hash`, `size`, `media_type`, `expires_ms`) in the message.

- **Store.** `<runtime dir>/cogbus-blobs/<hash>.blob` plus `<hash>.meta`; a restarted daemon
  finds what is still alive and drops the rest. Node quota 2 GiB (decision 4), 512 MiB per
  instance, 256 MiB per blob. Room is made only by dropping expired blobs: a live blob is never
  evicted for a newer one, so a handle that was handed out stays good until its TTL. Declared
  upload sizes are reserved when the upload begins, so concurrent uploads cannot overshoot.
- **TTL.** Default 10 min, at most 24 h. Every read drops an expired blob and a 30 s sweep
  drops the rest. Putting bytes already stored extends the expiry and never shortens it.
- **Wire.** Frames stay 64 KiB, so bytes move in slices: `blob_begin {media_type, size,
  ttl_ms}` returns an `upload` id, `blob_chunk {upload, offset, data (base64, at most 32
  KiB), last}` is strictly sequential and the last one returns the handle, `blob_get {hash,
  offset, len}` returns a slice and `eof`. Four uploads per instance may be open; an upload
  idle for 60 s is dropped. The client library has `put_blob` and `get_blob`.
- **Checks.** The same as every other op, on the hello-authenticated session (token, peer
  uid, section 4a). Put needs an `[[outputs]]` entry of class `blob` or schema `blob.v1` in the
  manifest, and asks governance once per upload (`cogbus.blob.put`, topic `blob/<media type>`).
  Get asks governance on its first slice (`cogbus.blob.get`, topic `blob/<hash>`). A blob is
  readable by its owner, by instances of the owner's project (the node bucket for a
  project-less owner), and by instances it was handed to. A handle is handed on by travelling:
  in a service call (to the provider), in a reply (to the caller) or in a published reading
  (to the subscribers that reading reaches, which already passed the project and grant rules).
  Only the owner's or a holder's handles pass; a service cannot hand on a handle it never held.
  Refusals count toward the repeated-refusal stop like the others.
- **Replies.** A reply body is still at most 64 KiB; a reply that carries a handle is small
  and the caller fetches the result. This is how `image.sd15` results and camera frames reach
  their readers, and a caller can pass a large input the same way (a handle in the call).
- **Not carried.** Mesh-wide handles (B6: a handle resolves on the node that stores it), blob
  streaming without the 32 KiB slices, and a per-project quota.

## Known gaps (2026-10-25, after B1 and B4)

What the shipped broker does not do yet, what closes each gap, and where it is tracked.
None of these may be dropped silently: a gap leaves this table only when its card's
acceptance test passes, and the row is then marked closed with the commit.

| # | Gap | Risk today | Closes with | Card |
|---|---|---|---|---|
| 1 | Only native cogs get the socket; container and VM cogs cannot reach the broker | such cogs fall back to the ingest HTTP path only | socket bind-mounted into containers, uid recorded per route; VM routes via an authenticated relay or declared unsupported | `cogbus-gap-containers` (after gap 2) |
| 2 | **Closed for the code** (`cogbus-gap-uid`, 0865db260 and the root-path proof below; §4a). A privileged daemon with a uid range runs each native cog as its own uid and the broker registers it. Proven as root on the aarch64 VM: a cog launched via setgroups/setgid/setuid as range uid 61000 was accepted, the same token from uid 61001 was refused `uid_mismatch` and logged. A user daemon cannot switch uids, so its cogs are marked `shared-uid` and `require_cog_uid` can refuse them | on a user daemon one cog that steals another's token can still act as it; that is the user daemon's limit, not a defect | the machine service runs privileged with `cogbus-isolation.json` (owner runs `scripts/cogbus-uid-setup.sh apply --yes`) | `cogbus-gap-uid` |
| 3 | **Closed for the code** (same card). Proven as root on the VM: the socket came up `root:cogbus 0660`, cogs get `cogbus` as their only supplementary group, and a uid outside the group got `PermissionError` on connect | until the owner applies the setup on a real host the socket keeps the daemon's group (reported in `cogbus list`) | the same owner-run setup | `cogbus-gap-uid` |
| 4 | After `hello`, a silent connection is kept forever | leaked connections and queues | ping on silence, close without pong; per-connection subscription and per-instance connection caps | `cogbus-gap-idle` |
| 5 | No publish rate limit; nothing stops a misbehaving cog (B3 rate-limits refusal logging and disconnects after repeated refusals) | a cog can flood readers within its declared topics | per-topic budget from `[[outputs]].rate`; broker asks the workload runtime to stop the instance, governed and chained | `cogbus-gap-rate-stop` |

Gaps 2 and 3 also block the plan's P3 device-owner cogs, whose per-device allowlists
need per-cog uids. After `cogbus-gap-uid` P3 can rely on per-cog uids only where the
machine service is privileged and set up; on a user daemon a device-owner cog shares the
daemon's uid, so a per-device allowlist is not enforceable there and must refuse or warn
(`require_cog_uid` is the lever).

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
| B5 | blob store by hash, TTL and quota | a frame handle resolves locally and expires on TTL (done: §13) |
| B6 | mesh topics as ADR-120 exposures, credits, raw-data refusal | a PG cog subscribes to a Ventuno reading topic; a raw IQ subscription from PG is refused unless exposed and requested |
| S1 | service calls (section 12): `serve`, `call`, `reply`, deadlines, governance | two callers share one provider; an unknown topic, an undeclared caller, a cross-project caller and a late reply are refused; governance sees `cogbus.serve` and `cogbus.call` |

B1–B4 are the plan's P2. B5–B6 are P6. S1 is the primitive the plan's P4 (`npu-infer`) needs.

## Decisions on the open questions (2026-10-25)

The owner took the recommended option for each:

1. **Stream rings**: evaluate iceoryx2 first in B2; write our own `memfd` ring only if it does not fit.
2. **Reading encoding on the socket**: length-prefixed JSON, matching the mesh today.
3. **Audit depth**: governance decisions and the activity log for declare, subscribe and
   refuse; no chain event per subscription.
4. **Defaults**: 4 MiB per stream ring and 2 GiB of blobs per node.
