# android-glasses-node

P0 glasses agent `com.weftos.glasses` for the Mentra Live (Android 11, API 30, MT6761, no
display). Design: `docs/plans/phone-parent-node.md` (sections 3 and 4, gates in section 9).
It is the glasses half of the phone parent-node link: mic up, speaker down, indicator cues
and LED, all over one TCP connection to the phone app (`apps/android-phone-node`).

It merges what `firmware/mentra-mic` (capture) and `mentra-audio-bridge` (playback) did
separately. The microphone foreground service is started from the adb shell, the way
`mentramic` proved works with the display off; nothing needs root, a system partition or
a privileged app. The stock `com.mentra.asg_client` is never touched.

## Layout

- `crates/weftos-node-core`: the protocol core. `GlassesNode` (`glasses.rs`) is this app's
  protocol layer: stream reassembly, command routing, the `indicator` machine, status/event/
  audio frame building, pairing hello. `ffi_glasses.rs` is its UniFFI facade (`GlassesCore`).
- `app/src/main/java/com/weftos/glasses/`
  - `GlassesService`: foreground service (`microphone|connectedDevice`), exported only behind
    `android.permission.DUMP` so only the adb shell can start it. Extra actions drive it (below).
  - `Agent`: wires mic, gate, link, playback, LED and the core together.
  - `MicCapture`, `EnergyGate`, `AudioChunker`: `AudioRecord` (VOICE_RECOGNITION, MIC fallback),
    20 ms frames, energy gate with calibrated floor, 400 ms pre-roll, 800 ms hangover.
  - `Playback`: `AudioTrack` streaming (PCM16 16 kHz mono), flush, idle close, BES I2S lines.
  - `Cues`, `assets/cues/*.wav`: the five bundled cues; `scripts/gen_cues.py` renders them.
  - `LedWriter`: BES serial lines, write only, claim/release, `unavailable` on failure.
  - `ParentLink`: TCP client with capped backoff; `Environment`: battery, Wi-Fi, gateway lookup.
  - `StatusBoard`: the status snapshot (log line and `status.json`).

## Build

```bash
scripts/build.sh test weftos-node-core   # Rust unit + both loopback tests (host)
scripts/build.sh glasses                 # cue check, bindings, arm64 .so (cargo-ndk), JVM tests, debug APK
scripts/build.sh glasses --dry-run       # show the steps and anything missing
python3 apps/android-glasses-node/scripts/gen_cues.py   # re-render cues after editing tones
```

`scripts/build.sh glasses` installs nothing and uses no adb. Needs JDK 17, Android SDK
platform 35, an NDK, `cargo-ndk`, the `aarch64-linux-android` Rust target, Gradle and
python3. Generated files (`jniLibs/`, `generated-uniffi/`) are gitignored. The APK is
`apps/android-glasses-node/app/build/outputs/apk/debug/app-debug.apk`.

## Install, grant, start (for whoever owns the glasses)

Pick the glasses with `-s` as usual (`G` below is the adb serial; no address is baked in).

```bash
APK=apps/android-glasses-node/app/build/outputs/apk/debug/app-debug.apk
adb -s $G install -r $APK
adb -s $G shell pm grant com.weftos.glasses android.permission.RECORD_AUDIO
adb -s $G shell appops set com.weftos.glasses RECORD_AUDIO allow            # only if the mic is denied
adb -s $G shell dumpsys deviceidle whitelist +com.weftos.glasses           # battery allow-list
adb -s $G shell am start -n com.weftos.glasses/.LauncherActivity           # optional: one foreground launch

# start the agent (shell-started foreground service, as mentramic)
adb -s $G shell am start-foreground-service -n com.weftos.glasses/.GlassesService \
  --es parent_host <phone address on the hotspot>
```

`parent_host` is optional: with none, the agent connects to the Wi-Fi default gateway, which
on the phone's hotspot is the phone. Extras are persisted, so later starts need none. Other
settings (all strings): `parent_port`, `capture_on_start`, `allow_unpaired`, `voice_volume`,
`cue_volume` (relative to voice, default 0.4), `cues`, `led`, `led_order` (`a` =
`color,on,off,cycles,brightness`, `b` = `color,brightness,on,off,cycles`), `audio_usage`
(`notification` default, `media`, `assistant`), `serial_path`, `i2s_control`, `half_duplex`,
`wake_lock`, `gate_open_rms`, `gate_close_rms`, `gate_hangover_ms`, `gate_preroll_ms`.

Drive and inspect it with further `start-foreground-service` calls (same component, add `-a`):

| Action | Effect |
|---|---|
| `-a com.weftos.glasses.STATUS` | write a fresh `status.json` and a log line |
| `-a com.weftos.glasses.DEMO` | walk listening, captured, thinking, done_ok, then an error turn locally (no phone needed): cues and LED |
| `-a com.weftos.glasses.CUE --es name listening` | play one cue (`listening`, `captured`, `thinking`, `done`, `error`) |
| `-a com.weftos.glasses.LED --ei color 1 --ei on_ms 1000 --ei off_ms 100 --ei cycles 1 --ei brightness 160 [--es led_order b]` | claim, then `cs_ledon` (G-LED by eye); colours 0 red, 1 green, 2 blue, 3 orange, 4 white |
| `-a com.weftos.glasses.LED_OFF` | `cs_ledoff` then release authority |
| `-a com.weftos.glasses.STOP` | stop the agent |

Status (the "endpoint"):

```bash
adb -s $G logcat -s WeftGlasses                                           # one line every 5 s
adb -s $G shell cat /sdcard/Android/data/com.weftos.glasses/files/status.json
```

`status.json` carries `link`, `indicator`, `mic` (state), `mic_rms`, `mic_peak`,
`mic_silent_ms` (time since a non-zero sample; large means the mic is returning silence),
`gate_open`, `speaking`, `led` (`ok` or `unavailable`) with `led_error` and `led_last_line`,
`frames_up`/`frames_down`, `reconnects`, `first_frame_to_write_ms`, `pairing` and `sas`.

## P0 gate checklist (glasses side)

Run in this order. Record results, including failures, in `docs/plans/phone-parent-node.md`
section 9; a fail is a result.

- [ ] **G0 mic after foreground launch.** Start as above, display off, app never visible.
  `status.json` shows `mic: recording` and `mic_silent_ms` stays near 0 while someone talks.
  Re-read it 30 minutes after the last foreground launch. Also record the result after a
  Wi-Fi change and after a glasses reboot (restart by shell again; boot start is P1).
- [ ] **G1 speaker without adb.** With the phone app sending reply frames (or via `CUE` and
  `DEMO` for a first check), the speaker plays; `first_frame_to_write_ms` under 150 and a
  `play.flush` stops within 100 ms. No adb session is needed once started. Opus decode is P1:
  the P0 reply stream is PCM16.
- [ ] **G2 coexistence.** The stock client still works while the agent runs: mic is not
  silenced either way (compare `mic_rms` with and without asg_client activity), and note
  whether asg_client is affected. BLE advertise and the camera question are P1.
- [ ] **G-LED.** `LED --ei color 1 ...` while asg_client runs: the green LED lights as
  commanded. Try `--es led_order b` if the timing looks wrong; confirm colours by eye
  (the blue/orange/green/red proposals come from the indicator machine). If the write fails,
  `status.json` shows `led: unavailable` and cues keep working (tone-only).
- [ ] **A1 (glasses half).** On the phone's hotspot, say a sentence: `frames_up` grows, the
  phone's reply frames arrive (`frames_down`), `listening`/`captured`/`thinking`/`done_ok`
  cues and LED follow, and `thinking` ends when the reply starts.
- [ ] **A4 reconnect.** Toggle the hotspot off for 30 s: `link` returns to `up` within 15 s,
  `reconnects` increments, no touch on the app.
- [ ] **A5 wrong peer (P1).** The pairing lane is wired, the secure channel is not.

## What is stubbed in P0

- **No Noise channel.** Pairing messages are plain CBOR on the pairing lane and the TCP link is
  unauthenticated; `allow_unpaired` (default true) streams audio before pairing completes.
  Lab use on the phone's own hotspot only. The six-digit code is in `status.json` (`sas`) and
  logcat, not spoken (no TTS on the glasses). The paired state and certificate are not persisted.
- **Discovery** is a config value, else the Wi-Fi default gateway. No mDNS, no BLE doorbell.
- **Codec is PCM16** on the local link (`Codec::Pcm16`); Opus encode/decode is P1.
- **No AEC.** `half_duplex` (default on) keeps the mic gate shut while the speaker plays and
  for 300 ms after; barge-in and G3 are P1. No VAD beyond the energy gate.
- **Boot start and the permanent start path** are P1. After a reboot, start by shell again.
- **No camera, no BLE, no Wi-Fi credential hand-off.**
- **Late failures** of an accepted command are reported through status (`led`), not a second
  `res`; `res{ok:true}` means accepted and routed.
- **LED field order** is a setting until G-LED confirms it; the default is the UART protocol
  doc's order. The serial port is written one whole line per open/close and never read.
