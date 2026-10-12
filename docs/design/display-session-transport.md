# Display sessions over the certified-leaf channel

- **Status**: built and tested without hardware (2026-10-12); not yet run on
  the pad. Covers ADR-117 D1 (sessions, scene ops, object events) and the
  transport half of D2. Grants are a seam, not ADR-108 chains yet (section 7).
- **Reads with**: `docs/adr/adr-117-display-endpoints-objects-and-bindings.md`,
  `docs/plans/adr-118-cos-contract.md` section 1, `docs/leaf-push-protocol.md`,
  `docs/guides/edgepad-console-hil.md`.

## 1. The pieces

```text
app (cos-console-host)           daemon                             pad (or emulator)
DisplaySession                   display.* RPC (Unix socket)        DisplayService
  DisplayTarget::Leaf  ───────▶   DisplayEndpoint  ───push topic──▶   (scene, hit-test, render)
  backend/leaf.rs      ◀───────   event queues     ◀──input topic──   touch, ptt button
```

| Piece | Where |
|---|---|
| Session frames, base64, mesh envelopes, the leaf-side state machine `DisplayService` | `crates/weftos-leaf-scene/src/session/` (`no_std`, shared by firmware and emulator) |
| Client backend (JSON-lines RPC to the daemon) | `crates/weftos-display-client/src/backend/leaf.rs` |
| Daemon endpoint: sessions, chunking, event queues, grant policy, keepalive | `crates/clawft-weave/src/display_endpoint/` |
| `display.*` RPC and chain records | `crates/clawft-weave/src/display_rpc.rs` |
| Pad firmware glue | `crates/clawft-edge-pad-idf/src/session.rs`, `mesh.rs` |
| Host-side pad emulator (`WireLeaf`, `leaf-emulator`) | `crates/weftos-display-client/src/wire_leaf/` (feature `wire-leaf`) |

The app never speaks to the pad. It holds a Unix-socket connection to the
daemon, which owns the certified-leaf channel (signed frames, replay floor,
ACKs, journal). That keeps grants checkable in one place and lets the console
run on one machine while the pad sits on the LAN.

## 2. The channel

Host to leaf rides `mesh.leaf.<id>.push` (the one topic a certified leaf may
subscribe to); leaf to host rides `mesh.leaf.<id>.input` (a certified leaf may
publish only `.input` and `.announce`). No kernel change was needed. The
carrier is the JSON the pad already parses:

```json
{"type":"display_session","cbor_b64":"<CBOR SessionFrame>","target_pubkey":"<leaf id>"}
```

Leaf to host, that JSON text is the `message` of a signed `ipc.publish`
envelope, journaled in NVS and replayed oldest-first until the daemon's signed
ACK. Un-sessioned traffic (`weaver leaf scene`, raw `leaf_input` touch) is
unchanged.

## 3. Frames (`SessionFrame`)

| Frame | Direction | Meaning |
|---|---|---|
| `Open { subject, display, proof }` | host to leaf | take the screen; idempotent per subject |
| `Opened { session, info, max_nodes, max_ops_per_sec, image_budget_bytes }` | leaf to host | size, render mask, touch and ptt, the leaf's bounds |
| `Refused { reason }` | leaf to host | `display busy: held by <subject>`, `no such display` |
| `Ops { session, seq, more, envelope }` | host to leaf | scene ops; `more` marks a non-final fragment; empty ops are the keepalive |
| `Close { session }` | host to leaf | release the screen |
| `Input { session, envelope }` | leaf to host | pointer event, hit-tested on the pad (`node_id` set) |
| `Sensor { session, name, pressed, value }` | leaf to host | `ptt.down` / `ptt.up` |
| `Rejected { session, seq, reason }` | leaf to host | a batch was refused whole; the session continues |
| `Ended { session, reason }` | leaf to host | the session is over (idle, link lost, unknown) |

## 4. Rules the pad enforces (`DisplayService`)

- One holder at a time. A second subject is refused with the holder's name.
- **Node ownership.** The service tracks the nodes the session created. `Clear`
  and `Replace` act only on those, never on the firmware's. Ids from
  `0xF0_0000` up (boot screen, raster shim, the future system strip) and other
  displays' ids are refused. `SetLayerBlend` is refused (it would reach the
  firmware's layers). Tweens must name owned nodes.
- **Bounds.** Node cap, bitmap budget, ops per second. A batch that breaks one
  is refused whole (`Rejected`), nothing partly applied.
- **Replay.** `seq <= last seen` is dropped, so journal replays are harmless.
- **Fragments.** The daemon splits a batch into frames of at most 4,608 bytes
  of CBOR (the pad reads 8 KiB, and base64 plus the JSON wrapper cost a third
  more). Damage is kept until the last fragment, so a big scene renders once.
- **Idle.** A host silent for 45 s loses the screen (`Ended`). The daemon sends
  an empty `Ops` every 10 s per live session, and closes a session whose app
  has not called for 60 s.
- On open the pad removes its boot text; when a session ends it redraws it.
- With no session, touch goes out as raw `leaf_input` as before, and button
  edges are dropped (they belong to nobody).

## 5. Daemon RPC (`display.*`)

| Method | Capability | Notes |
|---|---|---|
| `display.open {leaf_id, subject, resource?, proof_hex?, display?}` | Write | subscribes to the leaf's `.input`, sends `Open`, waits 5 s for the answer; errors say unreachable / refused / denied |
| `display.ops {session, cbor_b64}` | Write | CBOR `SceneEnvelope`; one op too big for a frame is refused, never dropped |
| `display.poll {session, max?}` | Write | `{events, rejected, ended, dropped}`; the queue holds 256 events and drops the oldest |
| `display.close {session}` | Write | idempotent |
| `display.revoke {subject}` | Admin | the subject's next call is refused and the pad clears its objects |
| `display.status {leaf_id}` | Read | `{connected, session}`: is there a mesh route to the leaf, and who holds its screen (`weaver leaf status --leaf-id <id>`) |

Outside a project (the user daemon's `read_only` policy) the five verbs above
are user-level Admin acts in `scope_gate` (`display.status` is a plain read); the client sends the same implicit `admin` scope as
the CLI's daemon client, which the daemon honours only from a same-uid Unix peer.

A push to a leaf with no route fails loudly (`leaf <id> is not connected`)
instead of vanishing. Open, close and revoke are appended to the chain
(`display.open`, `display.close`, `display.revoke`); no scene data is.

## 6. Verified without hardware

`scripts/build.sh leaf-display` runs, among others:

- `weftos-leaf-scene/tests/session.rs`: frames, carrier, base64, every rule in
  section 4;
- `clawft-weave/tests/display_endpoint.rs`: the endpoint against the real
  `DisplayService` behind a fake sink (fragments, revoke, keepalive, reaping,
  refusals, the bounded queue);
- `clawft-weave/tests/display_e2e.rs`: the real daemon dispatch over its Unix
  socket, the real certified-leaf listener and signed discovery, an emulated
  pad (`WireLeaf`: signed journal, ACKs, the same `DisplayService`), the real
  display client and the console host. Scene ops land and draw; a tap and a
  drag come back as `Tap` and `Drag` on the right node; `ptt.down` /
  `ptt.up` light and clear the console's status dot; a second app is refused;
  a revoked app is cut off and its objects cleared; an unconnected leaf is an
  error.

## 7. Not done

- **Grants.** `GrantPolicy` is the seam; today it admits any subject the RPC
  layer authorised (`Write`) and honours `display.revoke`. The `proof` bytes
  reach the leaf but nothing verifies them. ADR-108 per-resource grants
  (chains, pairing) are D2.
- **Arbitration.** One holder; a second app is refused, not queued. The
  firmware-owned system strip and swipe-to-switch are D2.
- **Touch rate on the pad.** Every pointer move becomes a signed frame
  journaled to NVS (4 KiB bound), so a fast drag backpressures; coalescing
  moves is open. The emulator has no such bound.
- **Hardware.** The firmware builds (`scripts/build.sh edgepad-fw`); it has not
  been flashed or run on the pad. The emulator is a faithful model of the
  wire, not of timing or of the GT911.
- **Daemon restart.** A restarted daemon forgets its sessions; the pad ends
  the old one when the host goes quiet (45 s) and the app reopens.
