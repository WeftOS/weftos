# Display sessions over the certified-leaf channel

- **Status**: design note for ADR-117 D1 → D2 (2026-10-10). D1 shipped the
  client crate `weftos-display-client` with the simulator and desktop-window
  backends; the real-leaf backend is a typed stub that returns
  `DisplayError::NotYet`. This note fixes how a session rides the existing
  certified-leaf channel so D2 can build it without changing the client API.
- **Reads with**: `docs/adr/adr-117-display-endpoints-objects-and-bindings.md`,
  `docs/plans/adr-118-cos-contract.md` §1, `docs/leaf-push-protocol.md` §2–§3,
  `docs/design/vector-leaf-display.md` §4.

## 1. What D1 built

```text
crates/weftos-display-client/
  src/session.rs      DisplaySession: open / apply / poll_event / size / capabilities / close
  src/target.rs       DisplayTarget::{LeafSim, Window, Leaf}
  src/event.rs        ObjectEvent::{Tap, Drag, Sensor}; ptt.down / ptt.up names
  src/limits.rs       SessionLimits (4,096 nodes, ops/s, image bytes), ledger, rate window
  src/grant.rs        DisplayGrant, GrantGate (D2 seam), Arbitration record
  src/gesture.rs      raw InputEnvelope → Tap / Drag (node captured at pointer-down)
  src/backend/mod.rs  Backend trait: the leaf as the session sees it
  src/backend/sim.rs  LeafSim handle + in-process leaf on weftos-leaf-sim (headless)
  src/backend/window.rs  [feature window] SDL window: mouse → touch, space → ptt
  src/backend/leaf.rs    [feature leaf] SessionFrame carrier types, open() = NotYet
```

The seam is `Backend`: the session bounds and ledgers every batch, hands an
envelope to the backend, and turns what comes back into object events. The
leaf side owns the `SceneStore`, the renderer and the hit-test, in the sim
exactly as on the pad. A real-leaf backend therefore only has to carry
`SceneEnvelope`s one way and `InputEnvelope`s plus sensor events the other.

## 2. The channel as it stands

From `docs/leaf-push-protocol.md` and `crates/clawft-kernel/src/mesh_leaf.rs`:

- Host → leaf on topic `mesh.leaf.<pk>.push`; leaf → host on
  `mesh.leaf.<pk>.input`. Payloads are CBOR inside a `SignedPublish` frame
  signed by the leaf certificate (leaf → host) or the parent (host → leaf);
  the parent acks with a signed `PublishAck`.
- Transport is a length-prefixed TCP stream (leaf port 9600, UDP discovery
  9599), with a durable offline journal on the leaf that replays signed
  frames after reconnect (`mesh.rs::LeafLink::replay`).
- The pad already decodes a `SceneEnvelope` on `.push`
  (`mesh.rs::apply_push` tries `decode_scene_envelope`, then the raster
  `LeafPush`), and already emits `InputEnvelope`s from the GT911 thread on
  `.input` (`mesh.rs::input_envelope`), with the hit-test done in
  `LeafScene` before publishing.

So the bytes a session needs already flow; what is missing is *who* they
belong to.

## 3. Session framing

One new topic pair keeps session traffic apart from the un-sessioned
`weaver leaf scene …` path, which stays as the operator's direct line:

| Direction | Topic | Body |
|---|---|---|
| host → leaf | `mesh.leaf.<pk>.display` | `SessionFrame::{Open, Ops, Close}` |
| leaf → host | `mesh.leaf.<pk>.input` | `SessionFrame::{Opened, Refused, Input, Sensor, Ended}` |

`SessionFrame` is the enum in `src/backend/leaf.rs`, CBOR-encoded with
`weftos_leaf_scene::codec::encode`. It wraps the existing `SceneEnvelope`
and `InputEnvelope` unchanged, so `WIRE_VERSION` does not move. Rules:

- **Open is idempotent.** `Open { subject, display, proof }` from a subject
  that already holds a session returns the same `Opened { session, … }`.
  Reconnects therefore never leak sessions.
- **Opened carries the leaf's limits.** The client takes the smaller of its
  `SessionLimits` and the advertised ones. The pad advertises 4,096 nodes,
  its real op budget (to be measured; the renderer runs at 31 Hz), and an
  image budget sized from free PSRAM.
- **Ops carry a per-session `seq`.** The leaf applies in order and drops
  `seq <= last_seen`; journal replay after a reconnect is then harmless.
- **Close clears.** The leaf removes every node the session owns and
  answers nothing; a later `Ops` for that session gets `Ended`.
- **Input and Sensor name the session** so two queued apps never see each
  other's touches. `Sensor` is how `ptt.down` / `ptt.up` travel; this avoids
  adding `InputEvent::Button` to the scene wire format for now.
- **Ended** is the leaf's side of teardown: revoked grant, pre-empted by
  arbitration (D2), or reboot. The client's `poll_event` returns `None`
  and `is_closed()` turns true, the same as the sim's `disconnect()`.

### Node ownership on the leaf

Node ids are app-chosen (the console uses 1–4095). The leaf keeps a
`NodeId → SessionId` map next to the `SceneStore`, filled on `Insert` /
`Update` / `Replace` and emptied on `Remove` / `Clear` / `Close`. An op
that touches a node owned by another session is refused with `Ended
{ reason: "node owned by another session" }` rather than applied. The
firmware-owned system strip (D2) uses the reserved `0xFE_xxxx` range the
boot screen already uses, outside any app's reach.

## 4. The leaf backend in the client

`DisplayTarget::Leaf(LeafEndpoint { name, leaf_id, display })` opens by:

1. resolving `name` through ADR-114 to the leaf's parent (the mesh node the
   pad is enrolled under) and asking it for a session carrier — the parent
   already holds the signed channel and the journal, so the app does not
   speak to the pad directly;
2. sending `Open` with the grant's `proof` and waiting for `Opened` or
   `Refused` (→ `DisplayError::Denied`);
3. thereafter `apply` → `Ops`, `poll` → the parent's queue of `Input` /
   `Sensor` / `Ended` for that session, `close` → `Close`.

The carrier is a small request/stream API on the parent (the kernel mesh
service), not a new socket from every app to the pad. That is what keeps
ADR-108 grants checkable in one place and lets PG host the command center
while the pad sits on the LAN.

## 5. The pad side (`clawft-edge-pad-idf`)

Not sketched in code in D1: the crate needs the ESP-IDF toolchain to even
type-check, `mesh.rs` is already at 460 lines and should be split before it
grows, and flashing is out of scope for this lane. What the `display`
service is, so the firmware lane can size it:

- a `display_service` module owning: the session table (one holder plus a
  short queue), the `NodeId → SessionId` map, per-session op and node
  counters, and the `seq` watermark;
- `apply_push` learns the `.display` topic and routes `SessionFrame`s to
  it; the un-sessioned `SceneEnvelope` path keeps working for `weaver leaf
  scene`;
- the push-to-talk button: a GPIO with debounce on the touch thread,
  emitted as `Sensor { name: "ptt.down" | "ptt.up", pressed }` through the
  same journal path as touch;
- `LeafServices` (leaf-push-protocol §5) gains a `display` entry with size,
  `CapabilityMask`, media formats and `input: touch + ptt`, which is what
  `DisplaySession::capabilities()` returns for a real leaf.

Memory: the session table and ownership map are a few KiB; the real cost
is bitmaps, which is why the image budget is advertised per leaf.

## 6. Desktop window backend notes

- The window is `embedded-graphics-simulator`'s SDL2 window, scale 1 by
  default (`WindowConfig::scale` to zoom). Mouse left button is pointer 0;
  the space bar is the push-to-talk button (key repeat is ignored).
- SDL needs its window and event pump on the main thread on macOS: open the
  session and call `poll_event` from `main`.
- The `window` feature enables `sdl2/use-pkgconfig`, so a Homebrew or apt
  SDL2 is found without `LIBRARY_PATH` tricks.

## 7. Open questions

1. Does the carrier live in `clawft-kernel`'s mesh leaf service or in
   `clawft-weave` next to `weaver leaf`? Proposed: kernel, because that is
   where the signed channel and grants already are.
2. Should `Ops` be acked per `seq` to give the console a delivered/not
   signal, or is the channel's `PublishAck` enough? Proposed: the ack is
   enough for D2; a per-session ack can come with bindings (D3).
3. Image budget on the pad: fixed at boot from free PSRAM, or re-advertised
   as sessions come and go?
