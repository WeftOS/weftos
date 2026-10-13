# android-phone-node

P0 scaffold of the WeftOS phone parent-node app. Design: `docs/plans/phone-parent-node.md`
(section 9, P0). Reference device: the owner's Galaxy S24 Ultra only (Q2).
Sibling of `apps/android-splat-capture`; same Gradle/Compose setup, separate app.

## Layout

- `crates/weftos-node-core`: the sans-IO Rust core (identity, `glasses.v1` envelopes and
  audio frame header, indicator state machine, `led.set`, leaf-parent pairing, fake
  transport) with a UniFFI facade (`NodeCore`).
- `app/`: Kotlin/Compose shell. `NodeRuntime` owns the one `NodeCore`; `LinkService` is the
  `connectedDevice` foreground service stub; `MainActivity` shows node id, pairing state and
  indicator state.

The node id comes only from the Rust core (`hex(SHA-256(pubkey)[..16])`, the same file and
derivation as `clawft-android-edge`). If the native library is missing the UI shows an
error; there is no Kotlin fallback identity.

## Build

```bash
scripts/build.sh test weftos-node-core   # Rust unit + loopback tests (host)
scripts/build.sh android                 # bindings, arm64-v8a .so via cargo-ndk, debug APK
scripts/build.sh android --dry-run       # show the steps and what is missing
```

`scripts/build.sh android` installs nothing. It generates the Kotlin bindings from a host
build, then lists anything missing. It needs: Android SDK platform 35 and an NDK (side by
side) under `~/Library/Android/sdk`, `cargo install cargo-ndk --locked`,
`rustup target add aarch64-linux-android`, JDK 17, and Gradle 8.9 (no wrapper jar is
checked in yet). Generated files (`jniLibs/`, `generated-uniffi/`) are gitignored.

## What works (P0)

- Rust core and its tests: identity, CBOR envelopes, audio frame encode/decode, indicator
  machine (including the 20 s thinking timeout to `done_error`), BES LED line rendering,
  pairing happy path and wrong-peer refusal over a fake transport, host loopback.
- UniFFI surface for identity, pairing state and the indicator machine.
- App skeleton: UI, foreground service lifecycle, JNI load of the core.

## What is stubbed

- No glasses link: no TCP listener, no BLE, no Noise channel (pairing messages are plain
  CBOR on the pairing lane in the tests; Noise XX over BLE is P1).
- No audio relay, no mesh gateway or upstream stream, no cue playback, no LED serial writes
  (the core only renders the lines; the glasses agent writes them).
- Seed stored as a 0600 file (Keystore wrapping is P1). Wi-Fi credential hand-off, CDM
  association, dashboard enrollment and revocation are P1.
- No STT, TTS, VAD or AEC on the phone, by design. X1 (on-phone STT experiment) is not started.

## P0 checklist

| Gate | Test (design section 9) | Status | Needs |
|---|---|---|---|
| G0 | Glasses mic after foreground launch, 30 min idle | not started | glasses agent APK, glasses (Grok owns the unit) |
| G1 | Glasses speaker streaming without adb | not started | glasses agent APK, glasses |
| G2 | Coexistence with the stock client (BLE, mic, camera) | not started | glasses agent APK, glasses |
| G-LED | RGB LED via BES serial: claim, `cs_ledon`, `cs_ledoff`, release | stubbed | core renders the lines (tested); glasses agent writes them; eyes-on check |
| G3 | AEC feasibility | not started | glasses agent, glasses |
| A1 | End-to-end loop, hotspot, Mac off, p50 at most 4 s over 20 trials | not started | phone link, workflow stub, glasses |
| A2 | 30 min soak | not started | A1 |
| A3 | Screen-off 10 min then a turn | not started | A1, battery allow-list on the S24 Ultra |
| A4 | Hotspot off 30 s, reconnect under 15 s | not started | phone link, glasses |
| A5 | Wrong peer refused and logged | stubbed | refusal done in core (pairing tests, loopback); logging and the real transport are P1 |
| A6 | Traffic shows only phone, glasses and the mesh gateway | not started | A1 |
| X1 | On-phone STT (sherpa-onnx Parakeet) on the S24 Ultra | not started | model delivery plan, speech.v1 provider seam |
| B1 | Bandwidth per utterance and reply | not started | A1 |
| B2 | Uplink loss mid-utterance | not started | upload client, workflow stub |

Also done in P0 and not a gate: indicator machine (design 3.7) and `led.set` message,
audio frame header, reliable-lane envelopes, pairing state machine with SAS and leaf
certificate. Built and installed on a device: no (no phone is connected; the APK has not
been built because the NDK, JDK and Gradle are not installed on the build host).
