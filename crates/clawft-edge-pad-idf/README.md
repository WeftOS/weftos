# clawft-edge-pad-idf

ESP-IDF-on-Rust port of `clawft-edge-pad`. WeftOS Inkpad Actor firmware
for the Elecrow CrowPanel DIS08070H (7" 800×480 RGB TFT + GT911 touch,
ESP32-S3-WROOM-1 N4R8).

## Why a second crate?

The bare-metal `clawft-edge-pad` crate spent eleven config iterations
hand-patching a raw `esp_hal::lcd_cam::dpi` driver against an open
upstream gap (esp-hal #5262 — DPI bounce-buffer support, unmerged).
The factory ESP-IDF reference firmware uses LovyanGFX, which programs
LCD_CAM registers directly; Espressif's official `esp_lcd_panel_rgb`
driver provides bounce buffers, frame sync, and hardware-erratum
compensation out of the box.

This crate moves the firmware onto `esp_lcd_panel_rgb` via
`esp-idf-hal` + `esp-idf-svc`. The sibling `clawft-edge-pad/` crate
stays in tree as the working spike + comparison baseline.

The toolchain pattern (Xtensa target via `xtensa-esp32s3-espidf`,
sdkconfig.defaults, embuild) mirrors `crates/clawft-edge-bench/`,
which is the proven IDF-on-Rust precedent in this repo.

## Crate layout

```
clawft-edge-pad-idf/
├── Cargo.toml                # own [workspace] table -- out-of-workspace
├── .cargo/config.toml        # xtensa-esp32s3-espidf target + runner
├── build.rs                  # embuild::espidf::sysenv::output()
├── rust-toolchain.toml       # channel = "esp"
├── sdkconfig.defaults        # SPIRAM Octal 80M, LCD_RGB_ISR_IRAM_SAFE, ...
└── src/
    ├── main.rs               # entry, boot order, task spawn
    ├── board.rs              # pin map + timings (port from edge-pad)
    ├── display.rs            # esp_lcd_panel_rgb wrapper + SceneSurface impl (2 FBs)
    ├── scene.rs              # SceneStore owner: boot screen, scene/raster pushes
    ├── selftest.rs           # boot display self-test + touch-target screen
    ├── drivers/
    │   ├── mod.rs
    │   ├── pca9557.rs        # blocking sync port
    │   └── gt911.rs          # blocking sync port
    ├── mesh.rs               # std::net mesh client
    ├── net.rs                # esp-idf-svc WiFi bringup
    ├── wifi_secrets.rs       # gitignored
    └── wifi_secrets.rs.example
```

## Building

This crate is **out-of-workspace** (its `Cargo.toml` opens with an
empty `[workspace]` table). Build it from its own directory, not from
the repo root.

Prerequisites (once-per-machine):
- `espup install` (provides the `esp` rustup toolchain).
- `cargo install espflash ldproxy`.

```sh
source ~/export-esp.sh
cd crates/clawft-edge-pad-idf
cargo build --release
```

First build will auto-download and compile ESP-IDF v5.3.x (~10-15 min,
~3 GB on disk under `target/`). Subsequent builds reuse the cached
IDF tree.

## Flashing

The release image is ~1.29 MiB. ESP-IDF's default single-app partition
table gives `factory` only 1 MiB, so the default table is **not
flashable** — the first hardware flash (2026-10-10) failed on exactly
this. `partitions.csv` at the crate root (nvs 24 KiB, phy 4 KiB,
factory 3 MiB, single-app / no OTA on the 4 MB module) is applied at
flash time; the app reads the table from flash at 0x8000, nothing is
compiled in. (`CONFIG_PARTITION_TABLE_CUSTOM_FILENAME` does not work
from this crate: esp-idf-sys resolves it relative to the generated
project under `target/`, not the crate root.)

With a USB-attached host toolchain (`cargo run` uses the runner in
`.cargo/config.toml`, which already passes the table):
```sh
cargo run --release
```

From a build container without USB access (how the Mac bring-up ran —
`espressif/idf-rust` image, device on the host), produce a merged image
in the container and write it with host `esptool`:
```sh
# in the container, crate dir
OUT=target/xtensa-esp32s3-espidf/release/build/esp-idf-sys-*/out/build
espflash save-image --chip esp32s3 --flash-size 4mb --merge \
  --bootloader $OUT/bootloader/bootloader.bin \
  --partition-table partitions.csv \
  target/xtensa-esp32s3-espidf/release/clawft-edge-pad-idf edgepad-merged.bin
# on the host (CrowPanel CH340 enumerates as /dev/cu.usbserial-*)
esptool --port /dev/cu.usbserial-XX --baud 460800 write-flash 0x0 edgepad-merged.bin
esptool --port /dev/cu.usbserial-XX read-flash 0 0x400000 backup.bin   # before the first write
```
The CH340 drops bytes under sustained transfer; if `write-flash` or
`read-flash` fails with "Invalid head of packet", retry at 230400 or
read in 256 KiB chunks. Serial console is 115200.

## Cross-reference to `clawft-edge-pad`

| edge-pad file              | edge-pad-idf file               | Notes |
|----------------------------|--------------------------------|-------|
| `Cargo.toml` (esp-hal)     | `Cargo.toml` (esp-idf-*)       | full re-spec |
| `.cargo/config.toml`       | `.cargo/config.toml`           | `-espidf` target |
| `rust-toolchain.toml`      | `rust-toolchain.toml`          | same `esp` channel |
| n/a                        | `sdkconfig.defaults`           | IDF-only |
| n/a                        | `build.rs`                     | embuild glue |
| `src/main.rs`              | `src/main.rs`                  | std main, FreeRTOS threads |
| `src/board.rs`             | `src/board.rs`                 | 1:1 transcript |
| `src/drivers/pca9557.rs`   | `src/drivers/pca9557.rs`       | async→blocking |
| `src/drivers/gt911.rs`     | `src/drivers/gt911.rs`         | async→blocking |
| `src/drivers/dpi_surface.rs` | `src/display.rs`             | replaced wholesale |
| `src/drivers/lcd_rgb.rs`   | (gone — superseded)            | day-2 broken path |
| `src/mesh.rs`              | `src/mesh.rs`                  | embassy-net→std::net |
| `src/net.rs`               | `src/net.rs`                   | esp-radio→esp-idf-svc |
| `src/wifi_secrets.rs.example` | `src/wifi_secrets.rs.example` | identical template |

## Display path (2026-10-10)

- Rendering is the vector-first leaf display: `weftos-leaf-scene`
  (`SceneStore`, wire envelopes, damage) + `weftos-leaf-renderer`
  (`render_damage` over the `SceneSurface` implemented by `display.rs`).
  `src/scene.rs` owns the store, draws the boot screen, applies
  `SceneEnvelope`s from `weaver leaf scene …`, and translates the older
  raster `LeafPush` (`weaver leaf push text|clear|image`) into scene
  nodes so that path still renders. The deprecated
  `weftos-leaf-display` compositor is no longer a dependency.
- Two PSRAM framebuffers; `present`/`end_frame` flip at the bounce-buffer
  frame boundary (`on_bounce_frame_finish`, IRAM callback). Partial
  repaints copy only the previous frame's damage rects front→back
  (age-2 damage). Measured on the panel: full-frame repaint 15.7 fps,
  partial updates 30.7 fps = the refresh rate (15 MHz pclk, 928×525
  clocks per frame ≈ 31 Hz).
- Boot self-test (`src/selftest.rs`, ~20 s, each phase logged): colour
  bars, full fields, 1-px border + grid, corner text, sweep, partial-
  update bench. While unprovisioned the panel shows a touch-target screen
  (five circles + drag bar, green on hit; events on serial).

## Push-to-talk and microphones (2026-10-11)

Decided wiring (V3.0 board; the amp's I²S links are the only I²S pins,
so SCK/WS are tapped at 0R pads). 3.3 V only — the INMP441 is not 5 V
tolerant.

| Signal | Board point | Notes |
|---|---|---|
| Push button | J10 pin 1 `RXD0` (IO44) ↔ J10 pin 4 `GND` | internal pull-up, active-low, 20 ms debounce → `ptt.down`/`ptt.up`; console becomes TX-only, flashing works unless held |
| Mic VDD | J7 pin 3 `3V3` (or J6 `3V3`) | both mics |
| Mic GND | J7 pin 4 `GND` | both mics |
| Mic SCK | pad of **R19** (IO42 side) | shared by both mics and the NS4168 BCLK |
| Mic WS | pad of **R12** (IO18 side) | shared; LRCLK of the amp |
| Mic SD | J7 pin 1 `IO38_D` | both mics on one SD line |
| Mic 1 L/R | GND | left slot |
| Mic 2 L/R | 3V3 | right slot |

Firmware: `cargo build --release --features ptt-mic` enables the I2S0
standard-mode RX (stereo 32-bit slots, 16 kHz; INMP441 = 24-bit
left-justified, `i32 >> 8`), capturing only while the button is held and
logging per-channel RMS/dBFS every 500 ms (`[ptt-mic] rms L=… R=…`).
The default build leaves IO18/IO42/IO38 idle so nothing drives unwired
pins. Streaming over the leaf channel is designed, not built (see the
module doc in `src/ptt.rs`).

## Status

- Flashed and camera-verified on the CrowPanel DIS08070H (see the
  frames under `/Users/mathewbeane/dev/edgepad-backups/frames/` on the
  bring-up Mac; serial logs alongside). GT911 answers at 0x5D on that
  unit.
- WiFi credentials: copy `wifi_secrets.rs.example` → `wifi_secrets.rs`
  and fill before building; the file is gitignored.
- Mesh: certified-leaf discovery (UDP broadcast :9490) + WLF1; the image
  needs `WEFTOS_LEAF_SEED_FILE` / `WEFTOS_LEAF_CERT_FILE` at build time
  (`weaver leaf provision`, `weaver mesh identity [--user] --out` for the
  pins). An unprovisioned image stays offline on the touch-target screen.
