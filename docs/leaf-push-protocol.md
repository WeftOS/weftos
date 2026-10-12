# Leaf-push protocol

The wire protocol for **kernel → leaf** push operations: how the WeftOS
kernel (and the services running on it) drive display layers, audio, and
control on constrained "leaf" devices — a Tidbyt, an ESP32 touch panel,
and (by design) future formats like smart glasses, watches, phones, and
apps.

Shared schema crate: [`crates/weftos-leaf-types`](../crates/weftos-leaf-types/src/lib.rs)
(`no_std + alloc`, CBOR via `ciborium`, compiles for `xtensa-esp32-*`).
Both sides — kernel publisher and leaf firmware — depend on it so the
payload formats can't drift.

## 1. Roles

- **Leaf** — a constrained device with one or more *sinks* (a display,
  an audio output, compute). It announces what it can do, then receives
  push payloads and renders/plays them. It is a peer mesh node, not a
  dependent peripheral: it degrades gracefully when the mesh is
  unreachable (renders last-known state, keeps sensing).
- **Kernel / service** — the publisher side. A service holding state
  worth showing (`cluster`, `kernel.ps`, an agent, …) produces
  `LeafPush` payloads aimed at a leaf and publishes them to that leaf's
  push topic.

## 2. Topics

Per leaf, keyed by its Ed25519 pubkey (hex):

| Topic | Direction | Purpose |
|---|---|---|
| `mesh.leaf.<pubkey_hex>.push` | kernel → leaf | `LeafPush` payloads |
| `mesh.leaf.<pubkey_hex>.announce` | leaf → kernel | `LeafServices` capability advertisement |

Helpers: `push_topic(pubkey_hex)` / `announce_topic(pubkey_hex)` in
`weftos-leaf-types`.

## 3. Transport

- **Mesh transport** — plaintext TCP to the daemon's `[kernel.mesh]`
  `listen_addr` (default `0.0.0.0:9489`) when `noise = false`. Noise-
  encrypted once leaf peers are provisioned with their own keys; for
  bring-up the config ships `noise = false` so leaf firmware can be
  brought up without the handshake first.
- **Framing** — `[4-byte big-endian length][payload]`.
- **Payload** (`noise = false`) — a UTF-8 JSON `MeshIpcEnvelope`:
  `{ source_node, dest_node, message, hop_count, envelope_id }`.
- A leaf **subscribes** by sending a `MeshIpcEnvelope` whose `message`
  targets `Topic("mesh.subscribe")` carrying the push topic it wants.
  The daemon then routes matching publishes to that connection.
- The `LeafPush` itself is **CBOR**, carried inside the envelope. The
  `weaver leaf push` path base64-wraps the CBOR in a small JSON message
  (`{ "type": "leaf_push", "cbor_b64": "...", "target_pubkey": "..." }`)
  and publishes via `ipc.publish` to the push topic; the topic router
  bridges that to the leaf's mesh-transport subscription.

### Governance / ExoChain (WEFT-150)

Leaf push is **not** a side door. The host path is:

1. `weaver leaf push|scene` encodes CBOR (`weftos-leaf-types` /
   `weftos-leaf-scene`) and calls daemon RPC `ipc.publish`.
2. **RPC capability** — `ipc.publish` requires `Capability::Write`
   (anonymous clients cannot publish). The local UDS `DaemonClient`
   auto-attaches an `admin` token, so operator CLI keeps working.
3. **Optional actor signature** — when `actor_id` is set, Ed25519
   signature over `publish_payload(...)` is verified; wrong signatures
   are rejected. Missing `actor_id` is still accepted for bring-up
   (warn-logged).
4. **A2A routing gate** — delivery goes through `A2ARouter::send_checked`
   (PID 0 / kernel as sender). Topic publishes call
   `CapabilityChecker::check_ipc_topic`; `IpcScope::Restricted` (browser
   default) cannot publish `mesh.leaf.*` (not a public topic).
5. **ExoChain** — with the `exochain` feature (default for `weaver`):
   - `send_checked` appends `source=ipc` / `kind=ipc.send` (generic
     IPC audit).
   - On success the daemon also appends `kind=ipc.publish` with
     structured payload: `topic`, `subscribers`, `actor_id`,
     `leaf_push` (true when `is_push_topic`), `wire_type`
     (`leaf_push` / `scene_push` when present), `message_len`.

`weftos-leaf-types` itself is schema-only (`no_std` CBOR types + topic
helpers). Governance and chain witnessing live on the kernel / weave
side of `ipc.publish`, not in the leaf firmware crate.

### Binding the certified-leaf listeners separately

Certified (WLF1) leaves connect to a dedicated TCP listener at the main
listener's port + 2, with UDP discovery at port + 1, both on the main
listener's IP. On a machine whose mesh stays on `127.0.0.1:9489`, a leaf on
the LAN can never reach them. `[kernel.mesh] leaf_listen_addr` moves just
those two sockets. The main listener, its Noise setting and the admission
policy do not change, and leaves are admitted by the same certificate and
signature checks wherever the sockets bind.

```toml
# A Mac whose mesh stays on loopback while an ESP32 display on the LAN
# enrols as a leaf. 192.0.2.10 is this Mac's LAN address.
[kernel.mesh]
enabled = true
listen_addr = "127.0.0.1:9489"      # main mesh: loopback only
leaf_listen_addr = "192.0.2.10"     # leaf TCP 192.0.2.10:9491, discovery udp 192.0.2.10:9490
# leaf_listen_addr = "192.0.2.10:9600"   # explicit: TCP 9600, discovery udp 9599
```

A wildcard (`0.0.0.0`, `::`) is refused at boot unless
`leaf_listen_any = true` is also set. `weaver kernel status` prints
`Leaf bind:  tcp <addr> / discovery udp <addr>` once the listeners are up.
Discovery advertises the leaf bind IP (for a wildcard, the interface that
answers the asking leaf). This key applies to the in-daemon mesh
(`[kernel.mesh]`); the machine mesh service reads its own `mesh.toml`.

### Display sessions (ADR-117)

An app can hold a leaf's screen as a session instead of pushing bare scene
envelopes. The frames (`SessionFrame` in `weftos-leaf-scene::session`) ride
the same two topics: host to leaf on `mesh.leaf.<pk>.push`, leaf to host on
`mesh.leaf.<pk>.input`. The carrier is the JSON the leaf already uses,
`{"type":"display_session","cbor_b64":"<CBOR SessionFrame>","target_pubkey":"<id>"}`;
the leaf's replies and its touch and button events go out as signed
`ipc.publish` frames through the same durable journal. The daemon side is the
`display.*` RPC (`crates/clawft-weave/src/display_rpc.rs`). Wire details and
the pad's rules: `docs/design/display-session-transport.md`.

## 4. Payloads — `LeafPush`

`#[non_exhaustive]` enum — new variants are additive, old leaves ignore
unknown variants.

| Variant | Payload |
|---|---|
| `DisplayText` | `z: LayerSlot`, `text` (soft cap 64), `x`, `y`, `color: [u8;3]`, `clear_first` |
| `DisplayImage` | `z: LayerSlot`, `rgb` (RGB888 row-major), `effect`, `alpha` |
| `DisplayClear` | `z: LayerSlot` |
| `DisplayBrightness` | `on_us: u32` (PWM on-time) |
| `LayerEffect` | `z: LayerSlot`, `effect: LayerEffectKind` |
| `Audio` | `AudioDrop` — `Chord` / `Scuttle` / `Pcm` |

### Display layers — `LayerSlot`

Z-ordered compositing slots: `Bg` < `Widget` < `Text` < `Alert`. A leaf
composites its layers bottom-up. Effects (`LayerEffectKind::Static {
keep_ratio }`) apply per layer.

## 5. Capability advertisement — `LeafServices`

A leaf announces, on its `.announce` topic:

```
LeafServices {
  node_pubkey: [u8; 32],
  hostname: String,
  firmware_version: String,
  audio_sink:   Option<AudioSinkCap>,    // sample_rate, channels, bit_depth, max_voices
  display_sink: Option<DisplaySinkCap>,  // width, height, pixel_format, layers, blend_modes
  compute:      Option<ComputeCap>,      // cpu_mhz, free_heap_bytes, eml_core
}
```

`DisplaySinkCap` is the **head profile** — it tells the publisher side
the exact geometry and capability of this head so a producer can design
content that fits. A 64×32 Tidbyt, an 800×480 ESP32 panel, a round
watch face, monocular glasses, and a phone app are all just different
`DisplaySinkCap` values.

## 6. REQUIREMENT (not yet built) — the `LeafRenderer` producer trait

> **Status: design requirement, captured here, not yet implemented.**
> Decision recorded 2026-05-14. The wire format (§4–§5) and the manual
> `weaver leaf push` CLI exist today; the producer-side trait below does
> not. First firmware bring-up uses `weaver leaf push` + a host script
> (see §7) — the trait is the "do it right" follow-up.

The missing half of the leaf abstraction is the **producer contract**:
the thing that turns a service's state into `LeafPush` operations for a
given head.

It should be a **first-class trait**, defined once in the leaf
abstraction (`weftos-leaf-types`, `std`-feature-gated — only the
publisher side needs it; the `no_std` leaf side only ever *receives*
`LeafPush`), and **implemented by each displayable service**. Not a
separate central "designer" process, and not ad-hoc per-service code —
a contract:

```rust
// weftos-leaf-types, behind the `std` feature
pub trait LeafRenderer {
    /// Given a target head's announced capabilities, produce the push
    /// operations that represent this service's current state.
    fn render(&self, profile: &DisplaySinkCap) -> Vec<LeafPush>;
}
```

Why a trait, why per-service, why in the leaf abstraction:

- **Rendering knowledge belongs with the data.** The `cluster` service
  knows how to represent cluster state better than any central designer
  could. Each service owns its own `render` impl.
- **It's a contract, not a convention.** A uniform interface means the
  kernel (or any orchestration) can drive *any* displayable service to
  *any* head without special-casing.
- **Format-extensibility is structural.** `render` takes a
  `DisplaySinkCap`. A new head format — glasses, watch, phone, app — is
  a new profile value, not a new code path. Services do responsive
  design against the profile (`if profile.width < 128 { terse } else {
  full }`); a new head needs zero service changes.
- **It belongs in the leaf abstraction** because the leaf protocol is
  what defines "what a head is" (`DisplaySinkCap`). The renderer trait
  is the other half of that same contract.

### Open sub-question (deferred)

Does `render` emit raw `LeafPush` ops, or an intermediate **design**
(layout intent) compiled to push-ops per profile? The intermediate-IR
form lets the head do final placement — better across wildly different
geometries — but is more machinery. v1 ships `render(profile) ->
Vec<LeafPush>`; a design-IR can be added later without breaking the
trait's callers.

This is a cross-crate architectural decision (it shapes
`weftos-leaf-types`, every displayable service, and the leaf firmware
contract) and should graduate to its own ADR when implemented.

## 7. The `LeafSurface` primitive — the leaf-side display bus

> **Status: design, agreed 2026-05-14 (three-expert team analysis).
> Crate `weftos-leaf-display` not yet built.**

`LeafRenderer` (§6) is the *publisher* contract: service state →
`LeafPush`. `LeafSurface` is its *leaf-side mirror*: the contract
between the leaf firmware's **compositor** (the code that turns
received `LeafPush` ops into pixels) and the **physical display bus**
(esp-hal DPI, a HUB75 RGB matrix, an SPI panel, a host simulator).

The reason it must exist: the first CrowPanel bring-up welded the
firmware directly to esp-hal's DMA/DPI calls. That produced a
fragile, esp-hal-specific path that broke (see §8). The primitive
seals the hardware bus behind one trait so the compositor and the
`LeafPush` dispatch are written **once**, against the abstraction.

```rust
// weftos-leaf-display, no_std
pub trait LeafSurface {
    /// Back buffer the compositor draws into — an
    /// `embedded_graphics::DrawTarget<Color = Rgb888>`.
    type Frame<'a>: DrawTarget<Color = Rgb888, Error = Self::Error>
    where Self: 'a;
    type Error: core::fmt::Debug;

    /// Self-description. Single source of truth for
    /// `LeafServices.display_sink` and every `LeafRenderer`.
    fn capability(&self) -> DisplaySinkCap;

    /// Borrow the back buffer; the compositor renders the full
    /// composited layer stack into it each frame.
    fn frame(&mut self) -> Self::Frame<'_>;

    /// Present the back buffer. Swap / DMA-kick / cache-flush / blit
    /// semantics are ENTIRELY the implementation's problem. Returns
    /// when it is safe to draw the next frame.
    fn present(&mut self) -> Result<(), Self::Error>;

    /// Optional hardware brightness; default = graceful Unsupported,
    /// compositor falls back to pixel-level dimming.
    fn set_brightness(&mut self, _on_us: u32) -> Result<(), Self::Error> {
        Err(Self::Error::unsupported())
    }
}
```

What the primitive **promises** (every impl): an accurate
`DisplaySinkCap`, an `Rgb888` `DrawTarget` of `width×height`, and a
`present()` barrier that's safe to call in a loop. What it
**leaves to the impl**: buffer location (PSRAM / SRAM / host `Vec`),
double-buffering, DMA, cache coherency, blit. It deliberately does
*not* promise tearing-free, a fixed framerate, or partial updates —
those are quality-of-implementation.

The **layer compositor is a generic free function** in
`weftos-leaf-display`: it takes `&[Layer]` (the `LayerSlot` stack)
and any `LeafSurface`, folds `Bg → Alert` into `surface.frame()`,
calls `present()`. Z-ordering is solved once, generically.

### Implementations slot under it

| Impl | `capability()` | `frame()` | `present()` |
|---|---|---|---|
| esp-hal DPI (`clawft-edge-pad`) | const 800×480 | PSRAM framebuffer view | the §8 circular-chain + cache dance |
| HUB75 RGB matrix (Tidbyt) | 64×32 | small SRAM buffer | bit-bang / I²S the scan |
| SPI panel (watch) | e.g. 240×240 | line buffer | chunked SPI DMA; `set_brightness` = backlight PWM |
| host simulator (`std`, dev/CI) | configurable | `Vec<Rgb888>` | push to a window / dump PNG |

The host simulator is what makes the `LeafPush` render path
**testable with zero hardware** — build and verify the compositor
against it before the esp-hal impl exists.

### Crate placement

New `crates/weftos-leaf-display/` — `#![no_std]`, `alloc` always.
Depends on `weftos-leaf-types` (`DisplaySinkCap`, `LeafPush`,
`LayerSlot`) + `embedded-graphics-core`. **Not** folded into
`weftos-leaf-types` — that stays a pure, hardware-free wire-schema
crate. The `std` feature gates only the host simulator. Hardware
impls (esp-hal DPI) live in their *own* crates and depend on
`weftos-leaf-display` — the abstraction crate never imports esp-hal.

`DisplaySinkCap` is the shared pivot: `LeafSurface::capability()`
*produces* it (firmware builds `LeafServices.display_sink` straight
from it — no hand-written announce), `LeafRenderer::render()`
*consumes* it.

## 8. Implementing `LeafSurface` for a scanned RGB panel

The CrowPanel esp-hal `LeafSurface::present()` must port a proven
pattern. Root cause of the first failed attempt + the references
(three-expert team, 2026-05-14):

**Why the naive attempt failed.** esp-hal 1.0's `DmaTxBuf` over a
linear PSRAM `Vec` *compiles and the DPI scans* — but it is **not a
frame-coherent path**:

1. `DmaTxBuf` is **one-shot, non-circular** — its descriptor chain
   ends in a single `suc_eof`; the GDMA stops there while the DPI
   peripheral free-runs its raster from its own timing registers →
   FIFO underrun → everything past the consumed bytes is black. The
   day-2 `dma_loop_buffer!` "worked" *only* because `DmaLoopBuf` is
   **circular** (and DRAM-only, ~4 KB — useless for a 768 KB frame).
2. Cache: `rom_Cache_WriteBack_Addr` flushes *resident dirty cache
   lines* — it is not a RAM→PSRAM copy. A 768 KB CPU fill through a
   32 KB write-back dcache leaves most of the framebuffer never
   written to PSRAM; only the resident tail survives.

**The proven pattern** (ESP-IDF `esp_lcd_panel_rgb` "Mode A";
LovyanGFX `Bus_RGB`, which drives this exact CrowPanel):

1. Framebuffer in PSRAM, **aligned to the GDMA external-memory /
   cache-line constraint**, width padded to a 4-byte multiple.
2. A **hand-built circular descriptor chain** spanning the whole
   framebuffer (~190 descriptors at ≤4032-byte chunks; the **last
   descriptor's `next` points back to the first**). The GDMA
   auto-rescans every VSYNC with no CPU intervention. esp-hal 1.0
   has **no buffer type** that is PSRAM-backed *and* large *and*
   looping — so this is a custom `DmaTxBuffer` impl, porting Mode A
   into Rust.
3. **Explicit dcache writeback of the framebuffer** after
   compositing, before the GDMA reads it (ESP-IDF's `esp_cache_msync`
   pattern; the ROM `rom_Cache_WriteBack_Addr` is the esp-hal-side
   primitive).
4. **Fallback — bounce buffers ("Mode B")**: two internal-SRAM
   buffers ping-pong (PSRAM→SRAM copy→LCD). This sidesteps the cache
   problem entirely (the GDMA only ever reads coherent SRAM) and is
   the recommended path at higher pixel clocks. The CrowPanel's
   ~16 MHz clock makes direct Mode A acceptable if (2) and (3) are
   correct; Mode B is the robust escape hatch.

All of the above lives **inside one `LeafSurface::present()` impl**.
Nothing above the trait sees a descriptor, a cache line, or a DMA
channel.

References: ESP-IDF `components/esp_lcd/rgb/esp_lcd_panel_rgb.c`
(`lcd_rgb_panel_alloc_frame_buffers`, `lcd_rgb_panel_init_trans_link`,
the `bounce_buffer` path); LovyanGFX
`src/lgfx/v1/platforms/esp32s3/Bus_RGB.cpp` (circular `_dmadesc`
loop, `_dmadesc_restart`, VSYNC_END ISR); esp-hal
`esp-hal/src/lcd_cam/lcd/dpi.rs` + `esp-hal/src/dma/buffers/`.

## 9. Current status

| Piece | Status |
|---|---|
| `LeafPush` / `LeafServices` wire schema (`weftos-leaf-types`) | ✅ exists |
| `weaver leaf push` CLI (manual push: text/clear/brightness/effect/audio) | ✅ exists |
| Topic routing (`ipc.publish` → `mesh.leaf.*.push` → mesh subscriber) | ✅ exists |
| Governance + ExoChain on leaf-push path (WEFT-150) | ✅ Write gate + topic scope + `ipc.publish` chain event |
| `scripts/leaf-push-ps.sh` — `kernel.ps` → leaf-push host script | ✅ exists |
| `LeafSurface` primitive + compositor + host simulator (`weftos-leaf-display`) | ⛔ not built — next step |
| esp-hal `LeafSurface` impl — circular PSRAM chain + cache (§8) | ⛔ not built |
| Leaf firmware: WiFi + mesh subscribe + `LeafPush` render via `LeafSurface` | ⛔ not built — `crates/clawft-edge-pad` |
| Automatic per-service publishers / `LeafRenderer` producer trait (§6) | ⛔ requirement only |

### Bring-up path

1. **`weftos-leaf-display`** — the `LeafSurface` trait + the generic
   layer compositor + the `std` host-simulator impl. Pure software,
   testable on the host.
2. **`LeafPush` render path** — the compositor mapping `LeafPush` →
   `LayerSlot` layers → `LeafSurface`. Verified on the simulator
   before any hardware impl exists.
3. **esp-hal `LeafSurface` impl** (§8) — the circular PSRAM chain +
   cache writeback, sealed in `present()`.
4. **Leaf firmware** (`crates/clawft-edge-pad`) — WiFi, mesh
   subscribe to `mesh.leaf.<pubkey>.push`, CBOR-decode `LeafPush`,
   feed the compositor + the esp-hal `LeafSurface`.
5. **Host push** — `scripts/leaf-push-ps.sh` already pushes
   `kernel.ps` rows; proves the channel end-to-end before the
   `LeafRenderer` trait (§6) exists.
