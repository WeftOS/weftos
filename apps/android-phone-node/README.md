# android-phone-node

P0 WeftOS phone parent-node app (`ai.weftos.phonenode`). Design: `docs/plans/phone-parent-node.md`
(section 9, P0). Reference device: the owner's Galaxy S24 Ultra only (Q2). It is the phone half
of the glasses link: the glasses agent (`apps/android-glasses-node`) dials this app over the
phone's hotspot, the app relays each utterance to the cloud speech endpoint, and streams the
reply and the indicator states back. Sibling of `apps/android-splat-capture`.

## Layout

- `crates/weftos-node-core`: the sans-IO Rust core. `phone.rs` is `PhoneSession`, the phone
  side of one glasses link: stream framing, pairing and the owner's code comparison, utterance
  assembly (uplink PCM16 between the glasses' `listening` and `captured` reports), the relay
  turn (indicator commands, reply frames), timeouts. `ffi_phone.rs` is its UniFFI facade
  (`PhoneLink`). Identity, envelopes, the indicator machine and the leaf-parent pairing
  state machine are the same modules the glasses use.
- `app/`: Kotlin/Compose shell.
  - `LinkService`: `connectedDevice` foreground service; owns the notification only.
  - `LinkEngine`: TCP listener (default port 7440, one glasses peer, a new connection replaces
    the old), the POST to the speech endpoint, the clock. It performs what the core returns.
  - `Wav`, `SpeechResponse`, `SpeechClient`: pure JVM. 16 kHz mono PCM16 WAV writer, response
    to outcome mapping, the HTTP call (bearer token in one header, never logged).
  - `TokenStore`, `KeystoreTokenStore`: the node token sealed with an Android Keystore AES-GCM
    key in app-private preferences; the abstraction is platform-free and unit tested.
  - `Sse`, `CosGlance`, `CosGlanceClient`: pure JVM. The SSE parser, the chief-of-staff
    `card` event and `GET /v1/cos/glance` shapes, the answer body, and the client (event
    stream with reconnect backoff, glance on every connect, `art.png`, `POST /v1/cos/answer`,
    a 400 keeps the card up and refetches; free-text replies to optionless cards). HTTP is
    injected (`CosHttp`).
  - `CosLink`, `CosCardView`: the card link inside `LinkService` (no second service), the
    needs-you notification (tap opens the app; each option is an action), and the card or
    idle art on the home screen. The CoS URL and its endpoint token (`weftos-cos endpoint
    add`) are separate settings from the speech endpoint and node token; plain `http://` is
    accepted only to a `*.ts.net` name (`res/xml/network_security_config.xml`).
  - `Settings`, `HomeScreen`, `NodeRuntime`: settings (endpoint, port), the token entry
    (paste, clipboard or QR scan), the pairing code comparison and the status view.

The node id comes only from the Rust core (`hex(SHA-256(pubkey)[..16])`, the same file and
derivation as `clawft-android-edge`). If the native library is missing the UI shows an
error; there is no Kotlin fallback identity.

## Build

```bash
scripts/build.sh test weftos-node-core   # Rust unit + loopback tests (host), incl. phone_loopback
scripts/build.sh android                 # bindings, arm64-v8a .so via cargo-ndk, JVM tests, debug APK
scripts/build.sh android --dry-run       # show the steps and what is missing
```

`scripts/build.sh android` installs nothing and uses no adb. It needs: Android SDK platform 35
and an NDK (side by side) under `~/Library/Android/sdk`, `cargo install cargo-ndk --locked`,
`rustup target add aarch64-linux-android`, JDK 17, and Gradle (no wrapper jar is checked in).
Generated files (`jniLibs/`, `generated-uniffi/`) are gitignored. The debug APK is
`apps/android-phone-node/app/build/outputs/apk/debug/app-debug.apk`; its package is
`ai.weftos.phonenode.debug` (the debug build adds a suffix).

## First real run (S24 Ultra and the glasses)

For the owner and Grok. The token is issued from the dashboard by the lead; this app does not
mint one. Nothing below is baked in: addresses and serials are yours.

1. **Token.** The lead issues a node token from the dashboard and hands it over (a string, or a
   QR code that encodes only that string). Keep it out of chat logs and shell history.
2. **Install both APKs** (`P` is the phone's adb serial, `G` the glasses'):
   ```bash
   adb -s $P install -r apps/android-phone-node/app/build/outputs/apk/debug/app-debug.apk
   adb -s $G install -r apps/android-glasses-node/app/build/outputs/apk/debug/app-debug.apk
   adb -s $G shell pm grant com.weftos.glasses android.permission.RECORD_AUDIO
   ```
   The rest of the glasses grants (battery allow-list, the optional foreground launch) are in
   `apps/android-glasses-node/README.md`.
3. **Hotspot.** Turn on the phone's Mobile Hotspot and put the glasses on it (however you join
   Wi-Fi today; credential hand-off is P1). If the glasses will not join, try the hotspot's
   2.4 GHz band.
4. **Phone app.** Open Weft Phone Node, tap Start link (allow notifications). In Settings:
   leave the endpoint and port (7440) at their defaults unless told otherwise, paste the token
   (Save token), or tap Use clipboard, or Scan QR. The status view then shows `Node token: set`
   and `Link: listening on :7440`. The Phone addresses block lists the phone's own addresses; the
   hotspot interface is the one the glasses use as their gateway.
5. **Glasses app.** Start the agent. With no `parent_host` it connects to the Wi-Fi default
   gateway, which on the phone's hotspot is the phone. If it does not, pass the phone's hotspot
   address from step 4:
   ```bash
   adb -s $G shell am start-foreground-service -n com.weftos.glasses/.GlassesService \
     --es parent_host <phone hotspot address>
   ```
6. **Pair.** The phone shows `Glasses: connected` and a six-digit Code. The glasses show the
   same code in `status.json` (`sas`) and logcat (`adb -s $G logcat -s WeftGlasses`). If they
   match, tap Codes match; if not, tap They differ. The phone then says `paired`. The phone
   remembers the confirmed glasses key, so a restarted glasses agent pairs again without a code.
7. **Speak.** Say a sentence near the glasses. Expected: listening cue, captured cue, the
   thinking tick, the spoken reply, the done chime. The phone shows the transcript, reply text,
   request round trip, server timings and the glasses indicator. Evidence for A1: p50 of the
   phone's `Request round trip` plus `first_frame_to_write_ms` from the glasses over 20 turns;
   `adb -s $P logcat -s WeftPhone` logs one line per turn (sizes, outcome, timing; never audio
   or the token).
8. **Reconnect (A4).** Toggle the hotspot off for 30 s. The glasses reconnect by themselves.
   The phone accepts a reconnect from the address that paired (P0 stands in for the Noise
   channel). If the glasses got a new address, or the phone service was restarted, restart the
   glasses agent (`-a com.weftos.glasses.STOP`, then start again): it re-sends hello and the
   confirmed key pairs without a code.

Failure display: the phone shows `speech endpoint: <code>` for a failed turn (`auth` is a bad
or revoked token, `budget` an exhausted cap, `timeout`, `network`, `no_token`); the glasses
play the error chime and flash red. A turn with no speech returns the glasses quietly to idle.

## What works (P0, built and tested on the host, not run on a device)

- Rust core: identity, CBOR envelopes, audio frame encode/decode, indicator machine (20 s
  thinking timeout), BES LED line rendering, pairing (code comparison, wrong-peer refusal),
  and `PhoneSession`. `crates/weftos-node-core/tests/phone_loopback.rs` drives a real
  `GlassesNode` against `PhoneSession` and a fake HTTP speech endpoint: pairing, utterance
  assembly, the POST (headers and WAV checked), reply frames back, and the indicator order
  `listening, captured, thinking, done_ok`; also error and budget (`done_error`), the 20 s
  timeout, a late verdict ignored, no speech (quiet idle, no chime), unpaired traffic ignored,
  resumed and trusted peers, short and over-long utterances.
- Kotlin JVM unit tests: WAV writer, response mapping, HTTP client against a loopback server,
  sealed token store (the token is not in the blob at rest, never in a description), settings
  checks.
- The app: listener inside the foreground service, the relay, the settings and status screen.

## What is stubbed

- **No Noise channel.** Pairing messages are plain CBOR on the pairing lane and the TCP link is
  unauthenticated. A reconnect from the address that paired is accepted without a new proof
  (the glasses keep their pairing in memory only). Lab use on the phone's own hotspot. Noise XX
  and a persisted proof are P1.
- **No BLE**, no Wi-Fi credential hand-off, no CDM association, no mesh gateway or upstream
  stream: the utterance is one POST (the endpoint takes at most 15 s, 500 KB).
- **Codec is PCM16** both ways on the local link; Opus is P1. The reply audio is sent as one
  burst of 20 ms frames (no flow control); `play.flush` and barge-in are not used yet.
- **`done_ok` waits for the glasses' `playback.done`**, with a fallback of the reply's duration
  plus 3 s if it never arrives.
- **No HTTP retry.** A failed POST ends the turn in `done_error`.
- **Token QR format** is the raw token string. Token issuing and revocation are the dashboard's
  job (the lead issues it); a revoked token shows as `auth`.
- **No wake lock or battery allow-list handling** on the phone (A3). Seed stored as a 0600 file
  (Keystore wrapping of the identity is P1). Dashboard enrollment is P1.
- No STT, TTS, VAD or AEC on the phone, by design. X1 (on-phone STT experiment) is not started.

## P0 checklist

| Gate | Test (design section 9) | Status | Needs |
|---|---|---|---|
| G0 | Glasses mic after foreground launch, 30 min idle | not started | glasses agent APK, glasses (Grok owns the unit) |
| G1 | Glasses speaker streaming without adb | not started | glasses agent APK, glasses |
| G2 | Coexistence with the stock client (BLE, mic, camera) | not started | glasses agent APK, glasses |
| G-LED | RGB LED via BES serial: claim, `cs_ledon`, `cs_ledoff`, release | stubbed | core renders the lines (tested); glasses agent writes them; eyes-on check |
| G3 | AEC feasibility | not started | glasses agent, glasses |
| A1 | End-to-end loop, hotspot, Mac off, p50 at most 4 s over 20 trials | built, not run | token from the dashboard, both APKs on the devices, glasses |
| A2 | 30 min soak | not started | A1 |
| A3 | Screen-off 10 min then a turn | not started | A1, battery allow-list on the S24 Ultra |
| A4 | Hotspot off 30 s, reconnect under 15 s | built, not run | both APKs, glasses |
| A5 | Wrong peer refused and logged | stubbed | refusal done in core (pairing tests, loopback) and unpaired traffic is ignored; the real transport (Noise) is P1 |
| A6 | Traffic shows only phone, glasses and the speech endpoint | not started | A1 |
| X1 | On-phone STT (sherpa-onnx Parakeet) on the S24 Ultra | not started | model delivery plan, speech.v1 provider seam |
| B1 | Bandwidth per utterance and reply | not started | A1 |
| B2 | Uplink loss mid-utterance | not started | upload client, workflow stub |

Also done in P0 and not a gate: indicator machine (design 3.7) and `led.set` message,
audio frame header, reliable-lane envelopes, pairing state machine with SAS and leaf
certificate, the phone link session. Built on the host: both APKs. Installed on a device: no.
