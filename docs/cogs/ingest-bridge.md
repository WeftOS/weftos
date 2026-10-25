# Cog ingest bridge

Code: `crates/clawft-kernel/src/cog_ingest/`. Card: mesh-placement-10.
Decision: ADR-100, "Decision 5 resolved (2026-10-02)".

A placed cog posts feature vectors to `POST /api/v1/store/ingest` with its
`COGNITUM_COG_TOKEN`. The released cog binaries always post to
`127.0.0.1:80`; the bridge is what answers there (or what a container relay
forwards to). The bridge validates the request, forwards the batch to the
store owner as a signed mesh message, and answers the cog.

```
ESP32 --UDP--> cog (native or container) --HTTP + token--> bridge (node A)
                                                              |
                          signed MeshIpcEnvelope (Ed25519)    v
                                                   store owner (node A or B)
                                                   project store (or controller's)
```

## Where the vectors go

Ingested vectors belong to the project that placed the cog. When the cog is
placed the node registers an `InstanceBinding { instance_id, project_id,
controller_node }` together with the instance's token
(`TokenRegistry::register`), and revokes it at unload (`revoke`).

`StoreRouter` picks the owner from the binding:

| Binding | Store that takes the batch |
|---|---|
| `project_id = Some(p)` | the store owned by project `p`'s kernel; if `p` has no route the batch is **refused (502)**, never redirected to the controller |
| `project_id = None` | the placing controller's store (`controller_node`) |

The owner may be this node (`LocalForwarder`) or another (`MeshForwarder`).

## Through the cogbus (ADR-122 section 9, B4)

When the node's cogbus broker runs (`crates/clawft-weave/src/cogbus_ingest.rs`),
the bridge is a compatibility listener in front of it:

```
released cog --HTTP + token--> bridge --publish store.vector/<instance>--> broker
                                  |                                          |
                                  +--- same batch, awaited ---> store sink <-+-- cogs publishing
                                                                 |             store.vector
                                                                 v             themselves
                                                    router -> store owner (as above)
```

- Each accepted post is published as a `store.vector.v1` reading on
  `store.vector/<instance id>` by the broker-internal writer `@ingest-bridge`,
  in the topic bucket of the placing project, then handed to the broker's
  store sink. The HTTP request, auth, rate limits and answers are unchanged;
  the answer is the store's counts as before. The sink writes the batch the
  bridge parsed, so the floats stored are the ones the bridge read.
- The topic's source id is the **instance id** (or `i-<16 hex>` of it when
  the id is not a valid source id): the store keys vectors by instance, and two
  placements of the same cog in two projects would otherwise share one topic.
- The store sink is the broker's built-in subscriber to `store.vector/*`. It
  routes through the same router, forwarders and vector store dir, and writes
  to the store of the project that placed the publishing instance only.
- A cog run with `WEFTOS_COGBUS` that declares
  `[[outputs]] topic = "store.vector", schema = "store.vector.v1", class = "reading"`
  can publish its own readings; they go through the same sink and the same
  per-instance budgets, and need no HTTP port.

## Wiring into placement

The project id travels with the placement:

1. The caller names the project: `PlaceOrder.project_id` (the
   `workload.place` RPC takes `project`, a project id). `PlaceBody.project_id`
   carries it to the target, and `PlacementRecord` keeps it (`project_id`,
   absent when the placement had no project). The RPC refuses a project the
   identity records do not know or have revoked (`check_project`, on a
   daemon that has them) and a malformed id (`clawft_types::project::validate_id`).
2. **Who may place for a project.** The controller's signature covers the
   project id but does not prove the controller may use it, so the target
   host checks (`IngestHooks::authorize`, refusal code `unauthorized`):
   the requester is this node's own key, or is listed in that project
   route's `controllers` in `cog-ingest.json`, or is the node of the key
   bound to the project in the identity records (the project daemon's node
   key, ADR-103). Otherwise the placement is refused and no token is issued.
3. **Refuse what cannot deliver.** At place time the target also checks
   that a store owner is routed for the placement (a route for the project,
   or for the requesting controller when the placement has no project). If
   not, the placement is refused with `admission` ("project ... is not
   routed here"), so the controller tries the next candidate. A cog is
   never placed to have every post fail with 502.
4. At `place` / `load` on the target, `WorkloadHostService` (given
   `with_ingest(IngestHooks)`) issues the cog's token with its host
   contract, injects `COGNITUM_COG_TOKEN` and `COGNITUM_INGEST_URL`, and
   after the adapter names the instance registers
   `InstanceBinding { instance_id, project_id, controller_node = requester }`.
   Only `cog` workloads get a token. The place result, `status` rows and the
   `workload-host` advertisement carry `ingest: enabled | disabled | none`.
5. `stop` and `unload` revoke the token before the adapter acts, and a
   failed start rolls back and revokes. `start` registers it again. The
   bridge checks the token again immediately before it writes, so a stop or
   unload that finished while a request was in flight is not followed by a
   write.
6. **Bridge down, cogs degraded.** If the shared bridge could not bind, the
   daemon builds disabled hooks: cogs are still placed, but with no
   `COGNITUM_COG_TOKEN` and no `COGNITUM_INGEST_URL` in their environment
   (a cog never carries a credential for a port some other process may
   own), no token is registered, and the place result, status and
   advertisement say `ingest: disabled`, and so does the place output: the
   dispatch attempt carries `ingest: disabled` and the explanation adds
   "placed with ingest disabled". The project authorisation check (step 2)
   still runs in this state, so a degraded record never carries an
   unverified project id. The orders have no "require ingest" flag yet; a
   caller that needs ingest must read `ingest` in the result.
7. Native cogs get the shared loopback listener's URL. Container routes get
   their own token-scoped listener (when `bridge.container_bind` is set),
   passed to the adapter as the relay's `ingest_upstream`; that listener
   accepts only that instance's token (another instance's valid token is
   403) and is closed with the instance.

There is no package-revocation path to the host yet (card 13); when one
force-unloads an instance it goes through `unload`, which revokes the token.
Anything that re-creates an instance record without going through `place`
(a controller adopting an in-flight placement, a host restart re-adopting
running instances) must issue a new lease (`IngestHooks::lease`), or the
cog's token is unknown and its posts are refused.

## Daemon configuration

The daemon (`crates/clawft-weave/src/cog_ingest_serve.rs`) starts the bridge
when it builds its placement host, and the `cog-store` service when asked.
Everything is optional; `<runtime>/cog-ingest.json` overrides the defaults:

```json
{
  "bridge": { "bind": "127.0.0.1:80", "fallback_bind": "127.0.0.1:18080",
              "requests_per_sec": 20, "vectors_per_sec": 2048,
              "container_bind": "192.168.64.1" },
  "routes": [
    { "project": "<project id>", "owner": "local", "controllers": ["<node id>"] },
    { "project": "<project id>",
      "owner": { "node": "<node id>", "key": "<64 hex>", "addr": "host:9472", "noise": true } },
    { "controller": "<node id>", "owner": "local" }
  ],
  "store_owner": { "listen": "0.0.0.0:9472", "noise": true,
                   "forwarders": [ { "key": "<64 hex>", "projects": ["<id>"] },
                                   { "key": "<64 hex>", "projects": "*" } ],
                   "projects": ["<id>"], "fallback": false }
}
```

| Knob | Default | Meaning |
|---|---|---|
| `bridge.bind` | `127.0.0.1:80` | Shared listener; must be loopback. Released cogs post to this address. If it cannot bind (taken, or unprivileged on Linux) the daemon logs it and places cogs degraded (`ingest: disabled`, see above). The user daemon and each project daemon all default to this port and only the first to bind wins; give the others a distinct loopback port in their own runtime dir's `cog-ingest.json` (cogs that honour `COGNITUM_INGEST_URL` follow it; a released cog that posts to the fixed port 80 can be served by one daemon per host). |
| `bridge.bind` = `"off"` | | No HTTP listener (cogs are placed `ingest: disabled`, with no `COGNITUM_COG_TOKEN`). Cogs publishing `store.vector` on the cogbus still reach the store. |
| `bridge.fallback_bind` | none | Loopback listener used when `bridge.bind` cannot be bound (for example an unprivileged daemon and port 80). Native cogs then get this address as `COGNITUM_INGEST_URL`; released cogs that post to the fixed port 80 are not served by this daemon. Not allowed with `bind: "off"`. |
| `bridge.requests_per_sec`, `vectors_per_sec` | 20, 2048 | Per-instance budgets. |
| `bridge.container_bind` | none | One gateway address for token-scoped container listeners (the engine or VM gateway); unspecified (`0.0.0.0`) and multicast addresses are refused. Unset: containers get no scoped listener. |
| `routes` | one route: project-less placements by this node's key go to this node's store | A `project` route sends that project's batches to its owner and may list `controllers` (node ids besides this node that may place for it; default none); a `controller` route takes project-less batches placed by that node. `owner` is `"local"` or a remote node pinned by `key`. A project or controller with no route is refused at place time. |
| `store_owner` | off | Serve `cog-store` on `listen` (Noise XX by default). The service serves **only** `projects` (and the project-less store when `fallback` is true); it does not serve the stores of this node's own local routes. Each forwarder key must state its scope: a list of project ids, or `"*"` for any project and project-less batches. There is no default. |

Vector ids are namespaced per instance inside a store (`(instance, id)`):
one instance cannot overwrite or dedup against another's vectors in a shared
project store.

Stores on an owner node are in-memory HNSW indexes created on first use
(`VectorDirectory`), backed by a durable log so a daemon restart does not
lose them. Every accepted batch is appended and synced to
`<runtime dir>/cog-ingest-vectors/<project id>.vec` (`_controller.vec` for the
project-less store) before the ingest is acknowledged, one capped file per
project (64 MiB, mode 0600). On the project's first use after a restart the
index and its dedup state are rebuilt from the log; a torn tail left by a
crash is dropped. A batch that would pass the cap is refused as store full and
writes nothing; vectors skipped as duplicates do not count against the cap.
The index is updated only after the batch is on disk. A crash mid-append
leaves a torn tail that is dropped on open. A checksum failure mid-file drops
the frame and everything after it, logs a warning with the byte count, and
keeps the dropped bytes in `<name>.vec.corrupt.<unix ts>`. A file that is not a
log at all (wrong magic or over the cap) is moved aside as
`<name>.vec.unreadable.<ts>` and the store starts empty; a transient I/O error
only refuses the call and is retried. The log directory is mode 0700. The log only grows (no
compaction), so repeated upserts of one id use space until the cap. Wiring
stores to a project kernel's own durable store is a follow-up.

The bridge and owner both log, and the host chains the placement as before;
ingest requests themselves are counted (`BridgeStats`), not chained.

## Request contract

`POST /api/v1/store/ingest`, `Content-Length` body, JSON:

```json
{"vectors": [[1, [0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8]]], "dedup": true}
```

| Rule | Limit | Status |
|---|---|---|
| Body size | 64 KiB (checked from `Content-Length` before reading) | 413 |
| Vectors per batch | 1 to 256 | 400 / 413 |
| Vector | `[id, [8 numbers]]`, id a non-negative integer | 400 |
| Values | finite f32 (NaN, infinity, overflow rejected) | 400 |
| Keys | only `vectors` and `dedup` | 400 |
| Token | `Authorization: Bearer <token>` or `X-API-Key: <token>` | 401 unknown / revoked, 403 other instance's token on an instance-scoped listener |
| Rate | per instance, default 20 requests and 2048 vectors per second | 429 with `Retry-After` |
| Transport | one request per connection, no chunked bodies, 8 KiB head, 5 s read timeout, 64 connections per listener | 400 / 408 |

Nothing is read from the body of an unauthenticated request. Owner-side
failure detail is logged, not returned. Connections that have not
authenticated are capped (64 at once per listener). A further connection
waits, first come first served, up to the 2 s pre-auth deadline for a slot,
and is dropped beyond that; every connection must present an authenticated
head within 2 s. **Residual limit:** the listener is loopback, so a process
on the same host that keeps 64 connections open and renews them every 2 s
still delays every cog by up to that long and can starve it. The token cannot
be checked before the head arrives; the defence is that only local processes
can do this.

`COGNITUM_COG_TOKEN` sits in the cog's environment, so any process of the
same uid can read it (`/proc/<pid>/environ`, `ps eww`). The token only
authorises writes to its own instance's vectors, and is revoked at stop and
unload; run cogs under their own unprivileged user to keep other processes
out.

`dedup: true` skips a vector when the store already holds the same id or a
bit-identical value. `dedup: false` upserts by id. The store applies it, so
dedup holds across bridges and nodes.

## Forwarding to a remote owner

`MeshForwarder` sends a `ForwardRequest` signed by the bridge node's key
(domain `weftos.cog_ingest.forward.v1`) in a `MeshIpcEnvelope` addressed to
`ServiceMethod { cog-store, store.ingest }`. The owner's `StoreOwnerService`
checks size, signature, structure, that `requester` is the signing key's
node, addressee, time window (30 s skew, 5 min maximum lifetime), the
`ForwardPolicy` (which keys may forward, optionally per project), nonce
freshness, then writes. The answer is signed by the owner and bound to the
request nonce; the forwarder pins the owner's key and rejects any other
signer. A replayed request is refused, never written twice.

The Noise XX link between bridge and owner gives confidentiality against
passive observers only: the handshake keys are ephemeral and not pinned to
node identities, so it authenticates no one. Integrity and authorisation come
from the signed request and the signed, key-pinned response. A `noise: false`
link (a warning is logged) travels in clear.

Transport is the same as `workload.ctl`: in-process streams for tests
(`OwnerConnector::register_local`), mesh TCP with optional Noise XX
otherwise (`serve_listener`).

## stdout is evidence, never data

Whatever a cog prints is captured by the runtime adapter as `RunEvidence`
for logs and the chain. It is never parsed into a store write and never
used as control input. The only path from a cog to a store is this bridge.

## Network policy per runtime (egress enforcement is deferred)

The contract (ADR-100 section 4) is: sensor feed in, bridge out, nothing
else. **Card 10's clause "a cog cannot reach anything else" is not met in
code; it is deferred, per runtime, as below.** Until then the adapters tell
the gate `egress` and placing a cog needs a permit.

| Runtime | Enforced now | Deferred |
|---|---|---|
| native (Linux) | The bridge binds loopback only. The token is per instance and checked on every request. The cog runs unprivileged under rlimits. | Blocking the cog's other egress with landlock, seccomp and nftables rules. A native cog can open other sockets today. |
| native (macOS) | As above. | A macOS sandbox profile (`sandbox-exec` / App Sandbox) denying other network access. Follow-up. |
| container (docker, podman, apple) | The in-container relay carries the cog's `127.0.0.1:80` to a token-scoped listener (`BridgeScope::Token`); another instance's token on it is 403; `network=none` is refused when an upstream is set. | The adapter verifying the container's network. |
| Seed (`remote.api`) | Not this bridge: the Seed ingests into its own store. | |

**Container egress is operator network configuration.** The recipe:

1. Create a network with no route out: `docker network create --internal
   cogs` (podman: `podman network create --internal cogs`; Apple
   `container`: a host-only network via `container network create`).
2. Set the runtime config `network` to that network.
3. Set `bridge.container_bind` to the gateway address of that network (the
   address the container reaches the host at). The daemon then binds one
   listener per container there, scoped to the container's token.
4. Do not publish other host ports into that network.

Steps 1 to 3 give a container exactly two peers: the relay's scoped listener
and whatever else the operator attached to that network. The adapter cannot
check this, which is why the gate sees `egress` for every network except
`none`.

## Optional UDP forwarder

`UdpForwarder` relays the ESP32 feed from the node's LAN to a container's
feed port, datagram for datagram: at most 512 bytes, empty datagrams and
datagrams from any source other than the configured sensor address are
dropped and counted. Nothing is parsed.

## Wiring a node

```rust
let registry = Arc::new(TokenRegistry::new());
let router = StaticRouter::new()
    .with_project(project_id, owner_forwarder)       // MeshForwarder or LocalForwarder
    .with_controller(controller_node, controller_forwarder);
let bridge = IngestBridge::new(registry.clone(), Arc::new(router),
                               RateBudget::default(), BridgeConfig::default());
let contract = HostContract::default_feed();          // fresh token
registry.register(InstanceBinding::new(instance_id, Some(project_id), controller_node), &contract)?;
let listener = bridge.bind("127.0.0.1:80".parse()?, BridgeScope::Any).await?;
// ... place the cog with `contract`; on unload: registry.revoke(instance_id)
```

The owner node runs a `StoreOwnerService` over a `StoreDirectory` (its
project stores and the controller fallback) with a `KeyPolicy` naming the
bridge keys it accepts, and serves it with `serve_listener`. `IngestStore`
is implemented by `MemoryIngestStore` and, with the `ecc` feature, by
`VectorBackendStore` over the kernel's HNSW / DiskANN backends.

## Acceptance runs use a replayed feed

Acceptance uses a replayed ESP32 feed (recorded or synthetic packets:
ADR-069 magic `0xC5110003`, 48 bytes, 8 little-endian f32 at offset 16).
`scripts/cogs/harness.py` produces the reference feed. A live ESP32 feed is
optional: point the sensor at the node (or the UDP forwarder) and the path
is the same. Document the live run next to its chain export when one is
made.

## Tests

`cargo test -p clawft-kernel --lib cog_ingest workload_ctl::tests_ingest`,
`cargo test -p clawft-weave --lib cog_ingest cogbus` and
`cargo test -p weftos-cogbus`:

- through the cogbus: a released-cog post answered byte for byte as by the
  plain bridge (accepted, 401, 400, 429) while published on the broker and
  stored for the right project; two projects' posts reach only their own
  store; a broker-native cog's `store.vector` reaches its project's store
  and a reading naming another project, a revoked instance or a bad body is
  refused; with the primary bind taken the fallback serves HTTP and the
  broker path still stores; with `bind: "off"` the broker path still stores;

- a stub cog placed through the real placement path (controller, signed
  `workload.ctl`, fetch-before-load, native adapter) posts with its injected
  token and URL, its vectors land in the placing project's store, and stop,
  start and unload revoke and reissue the token (further posts 401);
- a project-less placement lands in the controller's store; a malformed
  project id is refused by the host; stdout claiming vectors writes nothing;
- daemon config defaults and validation; an unbindable bridge disables
  ingest without failing the daemon; a bridge on one daemon delivers to the
  store-owner daemon over real TCP with Noise;
- who may place for a project (own key, listed controller, bound project
  key; a stranger and an unlisted project are refused), a placement with no
  store route refused at place time, and the degraded no-token placement
  when the bridge is down;
- token-scoped container listeners that close with their lease, a token
  revoked mid-request writing nothing, the anonymous-connection cap, and
  per-instance vector-id namespacing;
- the owner service serving only what `store_owner` lists, forwarder scope
  required, container-bind validation, and the RPC refusing unregistered
  projects;

- request validation (shape, dimensions, non-finite, batch and body caps);
- fake feed, cog stub, bridge, store: vectors land and dedup holds;
- unknown, revoked and other-instance tokens, malformed and oversize
  requests, per-instance rate and vector budgets, bind discipline;
- project routing: project store, controller fallback, no route refused;
- two nodes in process: a cog's bridge on node A, vectors queried on node
  B's store; forged, replayed, expired, mis-addressed, unauthorised and
  tampered forwards refused; a wrong owner key rejected;
- UDP forwarder; HNSW-backed store.
