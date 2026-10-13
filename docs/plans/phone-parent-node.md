# WeftOS phone app as the glasses' parent node

Status: Proposed design, 2026-10-13. Research and design only; nothing here is built. Owner: the WeftOS owner. Companion plans: `docs/plans/cos-cloud-migration.md` (M4, "every surface is a node"), `docs/plans/cos-dashboard-bridge.md` (relay and control files), `docs/leaf-push-protocol.md` and `docs/plans/adr-103-leaf-implementation.md` (leaf and certified leaf), ADR-114 (`weftos://` names), ADR-120 (exposures).

**Owner direction (2026-10-13), verbatim:**

- "we do not want to use mentra cloud for audio and transcriptions and video, we want the app on the phone to control the node inside the glasses. I was thinking we may even be able to redo the wifi firmware eventually and do CSI from them"
- "Let's look at this as an android app, but we may end up having to do it on apple as well."

**Evidence tags.** **[V]** verified against a primary source this session (URL in section 12). **[I]** inferred from memory, from a secondary source, or by reasoning; treat as a hypothesis until a spike confirms it. **[U]** unknown; named so it gets answered. **[D]** a design decision proposed here. **[R]** read from the repo or the glasses workspace.

---

## 1. Summary and decisions

1. The phone runs a **WeftOS node** (a Rust core in a native Android shell now, an iOS shell later). It is the **parent** of the glasses node. The glasses run a small agent that holds a leaf identity, streams the microphone to the phone, plays what the phone sends, and takes control commands.
2. **Nothing goes through Mentra cloud.** The glasses keep their stock firmware and the stock Mentra client app only as the hardware owner (launcher, camera HAL, LED, speaker MCU). We never talk to Mentra servers and never need the Mentra phone app.
3. The **glasses dial the phone**, over the phone's hotspot or a shared Wi-Fi. BLE is the doorbell, the provisioning channel and the low-rate fallback; it is not the audio path.
4. Speech: **ElevenLabs by default** (STT and TTS through single-use tokens the phone node receives from the cloud CoS), an **on-device recognizer as the offline fallback**. Audio between phone and ElevenLabs never touches Vercel; audio between glasses and phone never leaves the local link.
5. Cross-platform: **one Rust core with UniFFI bindings and thin native shells** (Kotlin/Compose now, Swift/SwiftUI later). Kotlin Multiplatform, Flutter and React Native are compared in section 6; none removes the hard, OS-specific work.
6. Biggest risks, in order: (a) the glasses cannot hold the microphone or the speaker path from an ordinary app without a foreground launch or privileges ([U], section 3.4); (b) iOS cannot keep a listening process alive in the background the way Android can ([V] section 7); (c) the audio route to the glasses speaker depends on a serial command to a co-processor that the current AudioBridge issues ([R], section 3.5).
7. **P0 is a spike** that proves the loop without the Mac, on Android, in two to three weeks. It also answers the three unknowns above, because they decide whether P1 is a plan or a rewrite.

What changes for existing pieces: the Mac adb loops (`mentra-cos-listen`, `mentra-cos-speak`) become a development fallback; `mentramic` and AudioBridge merge into one glasses agent; the `mentra-leaf` bring-up process (Noise XX, signed admission hello, status envelope [R]) becomes the glasses agent's link layer; the `/phone` page's Mentra tab reads node status instead of Mac-written status files.

---

## 2. Architecture

### 2.1 The picture

```
                     cloud CoS (Vercel, AI Gateway, Supabase)           ElevenLabs
                        ^   cos.v2 commands/events (HTTPS, SSE,           ^   single-use-token
                        |   Realtime, long-poll)                          |   WebSocket (TTS, STT)
                        |                                                 |
                  +-----+-------------------------------------------------+-----+
                  |                   PHONE NODE  (weftos://<mesh>/nodes/<phone>)  |
                  |  Rust core: identity, Noise, leaf-parent, cos client, speech   |
                  |  glue, VAD/AEC, cache.   Native shell: service, BLE, sockets   |
                  +-----+---------------------+---------------------+-------------+
                        | local Wi-Fi link     | BLE (doorbell,      | mesh, when
                        | (TCP, Noise)         |  provisioning,      | reachable
                        v                      |  fallback)          v
          +-------------------------+          |             PG mesh hub / other nodes
          |  GLASSES NODE (child)   |<---------+
          |  agent APK: mic, speaker|
          |  LED, camera, status    |
          +-------------------------+
```

- **Phone node.** A node with its own keypair, enrolled to the user in the dashboard, named `weftos://<mesh>/nodes/<phone id>` (ADR-114). It is credentialed to the user and speaks the `cos.v2` surface protocol of the cloud plan (plan section 4): announce `status` and capabilities, subscribe to commands, post results and committed utterance text. It is a surface in its own right (display, touch, location, its own mic) and the parent of the glasses.
- **Glasses node.** A child node. It has its own Ed25519 key and a certificate issued by the parent chain (section 4.3). It announces what it can do (`surface.mic`, `surface.speaker`, `surface.led`, optionally `surface.camera`) to its parent only; the parent republishes them upward as exposures. The cloud CoS addresses "speak on node N" and never learns that the phone is a hop [D].
- **Why the glasses dial the phone.** The phone is the access point when it shares its hotspot, so its address on that link is the default gateway the glasses already know; no discovery is needed. On a shared Wi-Fi the glasses use signed discovery (the certified-leaf pattern: a nonce-bound, short-lived UDP advertisement, `docs/plans/adr-103-leaf-implementation.md`) or mDNS. The direction also fits iOS: accepting inbound TCP connections and receiving UDP unicast need no Local Network permission, while outgoing connections to local addresses do **[V]** (Apple TN3179, section 12). Dialing in lets an iPhone app avoid the prompt for the main path.
- **Transport ladder** for phone to glasses: (1) Wi-Fi TCP over the hotspot or shared network; (2) the same over a Tailscale-style overlay if both are on it [I]; (3) BLE for control and a reduced-rate audio fallback (section 4.4). If the user's router isolates clients and there is no hotspot, only BLE remains; that case is called out in the P1 acceptance tests.
- **Phone to cloud**: the transports of plan 4.4 in order (Supabase Realtime, SSE, long-poll). Uplink is always HTTPS with a short-lived node token. The phone is the only component that holds a route to the cloud; the glasses hold no cloud credential at all [D].
- **Phone to mesh**: when the phone sees a mesh member (home or lab Wi-Fi, or an overlay), its core dials the certified-leaf listener of the mesh hub (the `leaf_listen_addr` list already supports "LAN and tailnet at once" [R]). Off-network it is cloud-only. Mesh membership adds placement (a stronger local STT or a local model) but is never required.

### 2.2 What the phone is not

It is not a relay for Mentra's protocol, not a Mentra SDK app, and does not run MentraOS cloud apps. It does not require the Mentra account. The glasses' stock client keeps owning pairing with Mentra's own app; we neither use nor disturb it ([U]: whether a second BLE client can coexist, section 3.2).

### 2.3 Phone as a surface and a host

The cloud plan already says a device that cannot reach the cloud is fronted by a host node that announces its capabilities (plan 4.1). The glasses have a real identity (unlike the pad, which the host fronts), but their only route out is the phone. So the phone is both: a surface (it can show a message, play audio on its own speaker, take push-to-talk on its own mic) and the **transport and policy point** for the glasses. The ADR-120 rules hold at both hops: nothing exposed by default, local switch wins (on the phone screen, and a hardware gesture on the glasses if the agent can read one, [U]), the dashboard may switch off but not on, every use logged without bodies.

---

## 3. The glasses side

### 3.1 What the glasses are

From the device profile [R]: a MediaTek MT6761 phone-class SoC (4x Cortex-A53, about 2 GB RAM, 24 GB storage), Android 11 (API 30), arm64, a build with `ro.debuggable=1` and permissive SELinux, Wi-Fi 5 at 5 GHz (390 Mbps link rate, which is a single spatial stream at 80 MHz [I]), a MediaTek combo radio for Bluetooth, no GPS, no cellular, A/B partitions with verified boot green. The profile's own README for `mentramic` mentions Android 14 behaviors, but the profile says API 30; confirm with `getprop ro.build.version.sdk` before relying on either ([U]). Everything below assumes API 30.

The device is a normal Android userspace on a debuggable build. That makes a **user-installed APK** the right deployment unit. It also makes tempting shortcuts available (`adb root`, running native binaries from `/data/local/tmp` as the shell user, which is how `mentra-leaf` runs today [R]). We do not build on them: they vanish with the adb session, a Mentra OTA may close them, and they would not be a product. **No root, no system partition, no privileged app, no custom ROM.** One USB-cable provisioning session with unprivileged `adb` commands (install, `pm grant`, `appops`, battery allow-list) is the ceiling for P0 and P1 [D].

### 3.2 One glasses agent APK

Merge `com.weftos.mentramic` (capture) and `com.mentra.audiobridge` (playback) into a single app, `com.weftos.glasses` [D]:

| Component | Job | Android type |
|---|---|---|
| `LinkService` | Holds the Noise session to the parent, the reconnect loop, the BLE doorbell; hosts the Rust leaf (the `mentra-leaf` code as a JNI library) | foreground service, `connectedDevice` |
| `CaptureService` | `AudioRecord`, pre-roll ring buffer, energy gate, frame and send | foreground service, `microphone` |
| `PlaybackService` | `AudioTrack` stream, jitter buffer, flush, volume, cue tones | foreground service, `mediaPlayback` (or none, [I]) |
| `CameraService` (P1) | stills and short clips on command | foreground service, `camera` |
| `ProvisionReceiver` | accepts the one-time adb-driven setup and grants | none |

Permissions: `RECORD_AUDIO`, `CAMERA` (P1), `FOREGROUND_SERVICE` plus the type-specific ones, `BLUETOOTH_*` as the API level requires, `POST_NOTIFICATIONS` (for the unavoidable foreground notification, invisible on a display-less device). All grantable by `pm grant` at provisioning [R: the existing README does this for `RECORD_AUDIO`].

**Coexistence with the stock client [U].** The stock `com.mentra.asg_client` is the launcher and owns the camera, LED and BLE link to Mentra's app. Three things must be measured: (1) can our `AudioRecord` capture while asg_client also holds the mic (Android 10+ allows concurrent capture with priority rules, but the first app may be preferred; the mentramic README already warns that "another process may hold the mic"); (2) can our BLE peripheral advertise at the same time as the stock GATT server; (3) can our camera service open the camera or must we ask asg_client through an exported intent. If concurrency fails, the fallback is to disable asg_client's mic use through an app-op for our package group only, never to uninstall it.

### 3.3 Provisioning and permissions, without privileges

1. **First time, USB cable, one session [D]**: `adb install` the agent; `pm grant` RECORD_AUDIO (and CAMERA); allow-list the app from battery optimization (`dumpsys deviceidle whitelist +<pkg>` [I]); launch it once through its translucent activity so the process starts from a foreground state (the existing mentramic trick for mic access [R]). After that adb is not required for operation.
2. **Wi-Fi credentials** are not typed on a display-less device. The phone sends the hotspot SSID and key over BLE inside the encrypted pairing channel (section 4.3), and the agent adds the network through the Wi-Fi APIs available to an app on API 30 (suggestions or the legacy `WifiManager` configuration calls) [I: confirm which works for a non-system app on this build].
3. **Updates**: the phone app can push an updated APK over a Wi-Fi adb session that the user starts from the phone, or the user plugs in USB. No auto-updater with install privilege exists without being a privileged app. This is an owner decision (Q7).

### 3.4 The microphone problem: the first risk to retire

Android restricts mic and camera access from services started in the background (while-in-use permissions). The rules in the current docs are for Android 12 to 14+ **[V]**: a foreground service needing a while-in-use permission cannot be created from the background on Android 14+ even if an exemption would otherwise allow it, and `microphone` services cannot start from `BOOT_COMPLETED`. The same documentation lists the exemptions that permit a background start, among them a Companion Device Manager association, the user disabling battery optimization for the app, `SYSTEM_ALERT_WINDOW` (Android 15 needs a visible overlay), and a start that follows a user interaction with a notification, widget or activity **[V]**. On the glasses (API 30) the rules are older; I believe the camera/mic while-in-use restriction began in Android 11 **[I]**, which would explain why the existing workflow launches the capture through the launcher activity "from the foreground" rather than by broadcast **[R]**.

A display-less device has no user to bring an activity forward. Candidate answers, to be tried in the P0 on-glasses test G0 (section 9) in this order: (1) start the process once from the provisioning launch, keep it alive as a sticky foreground service with the microphone type, and verify the mic still returns audio after hours and after a screen-off, reboot and Wi-Fi change; (2) if a start after reboot yields silence, add the boot path by launching the translucent activity from a `BOOT_COMPLETED` receiver where permitted, or from the phone's doorbell (an adb-free route is unlikely) [U]; (3) if neither works, the owner chooses between a one-time adb launch after each glasses reboot, or a privileged build (rejected by default). The gate: **G0 passes if the capture service still delivers non-silent audio 30 minutes after the last foreground launch, with the glasses idle and the app's process never visible.**

### 3.5 The speaker problem: the second risk

AudioBridge, as it exists on disk [R], does not run an HTTP server. It is a broadcast receiver driving `MediaPlayer` over a file queue (`PLAY`, `QUEUE`, `STOP`, `VOLUME` actions) on the notification stream, and it also opens a serial path to the audio co-processor and sends JSON "start I2S" and "stop I2S" commands around each playback (`mh_starti2s`, `mh_stopi2s`). The source I read contains no network listener, so the "HTTP service for a Rust daemon" described in the brief either lives elsewhere or has not been built; treat any HTTP playback path as new work **[U]**.

Two consequences. First, streaming playback needs `AudioTrack` (PCM in, low latency, flush on barge-in), not `MediaPlayer` on files; a streaming path has to keep the I2S path open for the length of an utterance and close it after silence. Second, whether an unprivileged app can open that serial node depends on file permissions and on SELinux being permissive on this build **[U]**; permissive SELinux is "unusual and helpful" per the device profile [R], and a stock-hardened Mentra release would break it. If the node is root-only, the agent cannot play audio at all without help from asg_client; the fallback is to ask asg_client to play through the path it already owns (an exported intent or its own audio API), or to use classic Bluetooth audio if the glasses present a sink, which is not known [U]. This is the second gate: **G1 passes if the agent plays a 10-second PCM stream through the speaker with no adb session attached, with under 150 ms from the first frame to sound and clean stop on `flush`.**

### 3.6 Control, status, events

The agent exposes these to the parent only (section 4.2 for wire format):

- **Control**: `capture.start{mode: ptt|vad|continuous, max_s}` and `capture.stop`; `play.begin{utt_id}`, `play.flush{utt_id}`, `volume.set`; `cue{kind}` (cached tones: heard, working, done, error); `led.set{pattern}` (only if an LED is controllable, the profile lists backlight and vibrator, not a status LED **[R]**, so this is [U] pending the asg_client intent surface); `camera.capture{still|clip, quality}` (P1); `probe{kind}`; `ping`.
- **Status** every 5 s and on change: battery, charging, Wi-Fi RSSI and link rate, link RTT to the parent, mic state and level, speaker state, agent and asg_client versions, thermal state, free storage.
- **Events**: `mic.gate{open|close, rms, voiced}`, `button{...}` and `wear{...}` only if asg_client broadcasts them [U], `link.up|down`, `playback.done{utt_id, spoken_through_seq}`.
- **Leaf announcement**: `LeafServices` with sources (mic, camera) and sinks (audio out, led). The glasses have no display, so there are no scenes: the "scene" is the cue vocabulary (tones and LED patterns), which the parent renders through the same command set. `weftos-leaf-scene` stays a pad and e-ink thing.

---

## 4. The glasses protocol (`glasses.v1`)

### 4.1 Layers

Reuse, do not invent:

- **Security and framing**: the Noise handshake the existing leaf already speaks (`Noise_XX_25519_ChaChaPoly_SHA256`, then a signed admission hello and a status envelope **[R]**), length-prefixed frames as in the leaf-push protocol (`[4-byte big-endian length][payload]` **[R]**), and the certified-leaf (WLF1) signed, sequence-numbered, durably acknowledged envelopes for anything that changes state or must not be lost **[R]**.
- **Two lanes on one connection** [D]: a **reliable lane** (CBOR, WLF1-style signed envelopes, ACKed): control, status, events, scene-like cues, commands with idempotency keys. A **realtime lane** (unacknowledged audio frames, sequence numbers and timestamps only, authenticated by the Noise session so no per-frame signature): mic uplink and speaker downlink. Mixing audio into the durable journal is wrong because a late frame is worse than a lost one.
- **Parent side**: the phone implements the receive half of the certified-leaf design (certificate check, scope, sequence floor, ACK). Today that half lives in the kernel's mesh code **[R]**, which is far too large for a phone. P1 extracts a small `weftos-leaf-parent` crate (certificate chain validation, replay floor, framing, ACK) that the kernel then also uses [D]. Whether it can be carved out cleanly is [U] until someone reads those modules.

### 4.2 Frames

Reliable lane (CBOR maps, `t` is the type):

```
cmd:   {t:"cmd", id, seq, ttl_ms, k:"capture.start", p:{mode:"vad", max_s:20}}
res:   {t:"res", id, ok, err?, p?}
stat:  {t:"stat", seq, battery, wifi:{rssi,rate}, mic:{state,level}, spk:{state}, ver}
evt:   {t:"evt", seq, k:"mic.gate", p:{open:true, rms:812, voiced:0.42}}
```

Realtime lane:

```
audio.up:   {stream:1, seq, ts_ms, codec:"pcm16"|"opus", payload}   glasses to phone
audio.down: {stream:2, utt, seq, ts_ms, codec, payload}             phone to glasses
```

Mic: 16 kHz mono (what `mentramic` captures now **[R]**), 20 ms frames, 640 bytes of PCM16 each, 256 kbit/s, trivial on Wi-Fi. **P0 ships PCM16**; Opus (about 24 kbit/s) arrives in P1 for BLE fallback and radio duty cycle. Speaker: 24 kHz or 16 kHz PCM16 (ElevenLabs output is converted by the phone), also 20 ms frames, 60 to 120 ms jitter buffer on the glasses, `flush` drops the buffer at once.

**Gating split.** The glasses run only the cheap part: an energy floor with calibration and a pre-roll ring buffer (about 400 ms) so the start of a word is never clipped, and a watchdog. The spectral voiceness gate, endpointing and everything else run on the phone (the two-stage VAD design from the project's own notes). While no gate is open the glasses send nothing but status, which is what keeps the radio and battery cheap [D].

### 4.3 Pairing, keys, no shared bearer

Requirements: no static shared secret, a lost phone or glasses can be revoked, and a bystander cannot pair. [D]:

1. The glasses agent generates an Ed25519 identity and an X25519 key at first run and advertises over BLE a short, rotating pairing beacon. The phone app, in its pairing screen, scans for it.
2. Phone and glasses run a Noise XX handshake over a BLE channel. Both compute a **short authentication string** from the handshake hash. The **glasses speak it** through the speaker ("pairing code four seven two"), and the user confirms it on the phone. This works on a display-less device precisely because there is a speaker; it defeats a man in the middle without a QR code, a cable or a PIN to type. A fallback is the same string flashed as an LED pattern, or the USB provisioning session reading the glasses key out through adb, which is already physical.
3. The phone signs a **leaf certificate** for the glasses key: scope (parent tenant), capabilities (mic, speaker, led, camera), issuer chain up to the user's identity, short expiry (days) with renewal on every successful connection. This is the certified-leaf shape (certificate, sequence floor, signed ACK **[R]**) with a phone as the immediate issuer instead of the daemon.
4. Over the now-encrypted BLE channel the phone passes the Wi-Fi credentials (section 3.3) and the glasses store the parent's public key. From then on the Wi-Fi link starts with a Noise IK-style handshake against that pinned key.
5. **Revoke**: the user removes the glasses in the dashboard or the phone app; the phone stops renewing, drops the key from its registry, and tells the cloud, so even a compromised glasses agent cannot rejoin. The glasses forget their parent on a factory reset of the agent.
6. **Phone's own identity**: an Ed25519 key enrolled in the dashboard by the request and approval flow of plan 4.3, exchanged for short-lived node tokens by signing a server challenge. Hardware-backed storage: Android Keystore and the iOS Secure Enclave primarily support P-256, not Ed25519 **[I]**, so the practical pattern is to wrap the Ed25519 seed with a hardware-backed AES key and keep the plaintext seed in memory only. Single-use ElevenLabs tokens never persist.

What flows nowhere: no Mentra account, no Mentra token, no bearer on the glasses, no long-lived secret on a device that a user can lose in a taxi.

### 4.4 BLE: what it is for and what it cannot do

BLE has three jobs: discovery and pairing (section 4.3); the **doorbell** (a wake notification, essential on iOS, section 7); and a **fallback control and reduced-rate audio path** when there is no usable Wi-Fi. Typical BLE 1M throughput of a few hundred kbit/s [I] would carry 24 kbit/s Opus in each direction on an L2CAP connection-oriented channel, with noticeably worse latency; that is a degraded mode ("it still hears me, answers are slower"), not the design center. A BLE connection also needs the glasses to keep their BLE peripheral role free of the stock client [U].

---

## 5. Speech

### 5.1 STT options

| Option | Where | Latency | Notes |
|---|---|---|---|
| ElevenLabs realtime STT (`scribe_v2_realtime`) | Cloud, direct from the phone | about 150 ms to commit per vendor figure in the plan **[V in plan]** | Default. Single-use token minted by the cloud CoS; only the committed text goes to the CoS |
| Android on-device `SpeechRecognizer` (`createOnDeviceSpeechRecognizer`, API 31+; check `isOnDeviceRecognitionAvailable()`; custom audio can be supplied through `EXTRA_AUDIO_SOURCE`) | Phone | device-dependent | **[V]** API facts. Free, offline fallback. Whether it accepts a live stream rather than a file descriptor of a finished clip is [U]; plan on feeding it per-utterance after the gate closes |
| Local model in the Rust core (whisper-class, or Parakeet via ONNX, both already used elsewhere in WeftOS) | Phone, or a mesh GPU node | 0.1 to 1 s, size dependent | Second fallback and the privacy mode; cost is app size, battery and thermals [I]. A mesh GPU node (placement layer) can take it when reachable |
| Apple `SFSpeechRecognizer` on-device | iPhone | device-dependent | iOS equivalent of the Android fallback [I] |

Decision [D]: the phone node uses an `SttBackend` trait (ADR-053) with four implementations and a policy: cloud when online and the user allows it, otherwise on-device. The choice and its reason are shown in `status`, so the CoS can say why it heard worse.

### 5.2 TTS and caching

ElevenLabs TTS is the default (plan section 5: WebSocket with streamed text in and audio chunks out, token single-use). The phone node receives `speech.chunk` events, forwards text to the TTS socket and relays the audio frames to the glasses as `audio.down`. `speech.cancel` closes the socket and sends `play.flush` with the `spoken_through_seq` back to the CoS (plan 5.4). Model choice follows the plan (`eleven_v4_turbo` or flash for conversation). Voice and prosody hints are advisory and mapped by the phone's engine adapter.

**Cache** [D]: key = hash of (voice, model, settings, normalized text). Only short, stock phrases are cached by default (cue words, "I didn't catch that", "one moment", pairing codes, error lines); a phrase becomes eligible after its second use. Full replies are not cached unless the user turns the setting on, because the hit rate is low and a transcript of what the assistant said is personal data. Encrypted at rest, LRU, 50 MB cap. Cached stock audio is pre-pushed to the glasses so cues play with zero network, which is what a 150 ms cue tone needs.

**Offline TTS**: the platform engine (Android `TextToSpeech`, iOS `AVSpeechSynthesizer`) for a short offline notice only; WeftOS local engines (Kokoro, Orpheus) are mesh-placed fallbacks, not phone-resident [I].

### 5.3 The CoS "its voice" stream on the phone

The phone consumes `speech.v1` exactly as the plan defines it: `speech.start`, `speech.chunk` (with advisory prosody and `interruptible`), `speech.offer`, `speech.cancel`, `speech.cue` (plan 5.4). It also posts upward `VoiceAnalysis` records (pure DSP, no model; consent-gated) and the barge-in point. The phone does not shape speech; it only plays the spoken plan. **Barge-in** needs echo handling because the glasses' mic and speaker are inches apart. The phone knows exactly what it sent to the speaker and when, so it can run the WeftOS AEC with that reference and a measured delay, and the glasses report playout timestamps to keep the delay estimate honest **[D, I]**. Until AEC is proven, barge-in is by a push-to-talk gesture or by pausing the speaker while the gate is open.

### 5.4 Latency budget (glasses to glasses, no tool call)

The plan targets 0.9 s p50 / 1.5 s p90 from end of speech to first sound for the browser node. The glasses add two local hops and a jitter buffer. Proposed budget:

| Stage | p50 (ms) | p90 (ms) | Note |
|---|---|---|---|
| Capture frame + radio to phone | 25 | 60 | 20 ms frame, Wi-Fi unicast |
| End-of-speech commit (STT turn prediction) | 150 | 300 | vendor figure from the plan, not measured here |
| Cue tone triggered locally | 40 | 100 | parallel, not additive; heard cue by about 200 ms |
| Phone uplink of text to CoS | 80 | 250 | cellular, kept-alive connection |
| CoS: first spoken clause (fast model, no tool) | 450 | 900 | the plan's own target class |
| Downlink of chunk to phone | 60 | 150 | Realtime push |
| TTS first audio (turbo/flash) | 100 | 250 | vendor "about 100 ms" figure |
| Phone to glasses + jitter buffer + playout | 120 | 220 | 20 ms frame, 80 ms buffer, 20 ms driver |
| **Total** | **about 985** | **about 2,000** | |

That is roughly 0.1 s worse than the browser node at p50 and exceeds the plan's p90 by 0.5 s; the levers are a shorter jitter buffer once the link is measured, a speculative first chunk, and the cue tone masking the wait. The P0 spike measures every row; the table is a target, not a result. With a tool turn, the plan's targets apply (cue tone 0.15 s, first chunk after a tool 1.8 s / 3 s, barge-in silence 120 / 250 ms).

---

## 6. Cross-platform strategy

### 6.1 What the workload actually needs

A long-lived background service; a local TCP listener and client; BLE central (and maybe peripheral); low-latency audio I/O only on the glasses side (the phone itself mostly moves bytes); Noise, Ed25519, CBOR and certificate logic; a WebSocket to ElevenLabs; HTTPS and SSE to the cloud; a small UI (pair, status, switches, settings), with rich screens already existing as the web dashboard. The hard, OS-specific parts are the background execution model, BLE and permissions. No cross-platform UI toolkit removes those.

### 6.2 Options

| | Shared Rust core + UniFFI + native shells | Kotlin Multiplatform | Flutter | React Native |
|---|---|---|---|---|
| Reuses existing WeftOS code (Noise, leaf types, certified-leaf, CBOR, VAD/AEC, speech traits) | Yes, directly; same crates compile to the browser node | No; would rewrite or wrap the Rust in a second FFI layer anyway | Via flutter_rust_bridge, an extra layer | Via custom native modules, an extra layer |
| Background service, BLE, sockets, audio | Native in the shell, written once per OS (about 15 to 25 percent of the code) | Native per OS (`expect`/`actual`) | Plugins (foreground-task, BLE plugins); each ultimately native; background isolate quirks [I] | Native modules; JS thread suspended in background; background tasks are the weakest [I] |
| UI sharing | None (two small native UIs) | Compose Multiplatform can share UI, maturing on iOS [I] | Strong | Good |
| Bindings maturity | UniFFI: full support for Kotlin and Swift, async and callback interfaces documented **[V]** | n/a | n/a | n/a |
| Fit to "needs to survive Doze and iOS suspension" | Best control of lifecycle | Same as native | Poorer visibility into lifecycle | Poorest |
| iOS App Review exposure | Same as any app | Same | Same | Same |
| Cost to have iOS later | A Swift shell (2 to 4 weeks once the core exists) | Moderate | Low UI cost, high platform-code cost | Same as Flutter |

**Recommendation [D]**: the shared Rust core with UniFFI bindings and thin native shells. The UI is small and mostly web; the logic and crypto are large and already Rust; the lifecycle is the risk and belongs in native code the team can see. The core is **sans-IO** (state machines that take bytes, timers and platform events and return actions) so each shell supplies sockets, BLE and timers in whatever way its OS prefers. That matters on iOS, where Apple recommends Network framework and `waitsForConnectivity` for connectivity changes and local-network prompts **[V]**, and on Android where the shell owns the service lifecycle. The core also gives a free desktop test harness and keeps the browser node (`clawft-wasm`) and the phone node on one protocol crate set.

Do not reuse the browser node inside a WebView as the phone node: a WebView's JavaScript is suspended in the background on both platforms [I], so it cannot be the always-on parent. The WebView is for rich screens (section 8.2).

### 6.3 Core layout (indicative)

`weftos-node-core` (identity, enrollment, token exchange, `cos.v2` client state machine, command routing), `weftos-leaf-parent` (certificate chain, replay floor, framing, ACK), `weftos-glasses-proto` (`glasses.v1` frames), `weftos-speech-glue` (STT/TTS traits, gate, AEC hookup, cache), `weftos-mobile-ffi` (the UniFFI surface). Targets: `aarch64-linux-android`, `aarch64-apple-ios`, `aarch64-apple-ios-sim`, host. The glasses agent reuses the same crates (arm64 Android) for its leaf end. All builds through `scripts/build.sh` lanes; the script gains an Android lane and an iOS lane when the work starts [D]. Binary size and the avoidance of the full kernel are [U] until the extraction is attempted.

---

## 7. Platform constraints that bite

### 7.1 Android (phone)

Verified against the current Android documentation:

- **Foreground service types** [V]: since Android 14 every foreground service declares a type in the manifest and in `startForeground()`. `microphone` needs `FOREGROUND_SERVICE_MICROPHONE` and `RECORD_AUDIO`, is subject to while-in-use rules, and cannot start from `BOOT_COMPLETED`. `connectedDevice` needs `FOREGROUND_SERVICE_CONNECTED_DEVICE` plus at least one of the Bluetooth, Wi-Fi-state, NFC, IR or UWB permissions (or a USB permission callback); the documentation names "Bluetooth, NFC, IR, USB, or network connections to external devices" as its use. `dataSync` is for uploads and downloads; on Android 15 it has a roughly six-hour timeout (with `Service.onTimeout`) and cannot start from boot **[V]** (Android 15 beta blog), so **do not use `dataSync` for the link**. Foreground service types must also be declared in the Play Console for apps targeting Android 14+ **[V]**.
- **Plan** [D]: the link service is `connectedDevice` (it holds the Wi-Fi listener and the BLE link); the phone's own microphone for push-to-talk is a separate short `microphone` service started from a visible activity or notification action; `mediaPlayback` is needed only if the phone plays audio itself. The service posts an honest persistent notification ("Glasses connected, listening").
- **Background starts** [V]: apps targeting Android 12+ cannot start foreground services from the background except under listed exemptions; on Android 14+ services needing a while-in-use permission cannot be created from the background at all. A **Companion Device Manager** association is a listed exemption, and `CompanionDeviceService` with `REQUEST_COMPANION_START_FOREGROUND_SERVICES_FROM_BACKGROUND` is the documented way to wake an app when an associated device appears **[V]**. We therefore associate the glasses through CDM at pairing, so a glasses reboot or return into range can restart the link service without the user opening the app. Disabling battery optimization is also an exemption **[V]** and is requested at onboarding with an explanation.
- **Doze and battery optimization** [I]: a running foreground service keeps network and CPU for its app; keep a partial wake lock only while a turn is active, and a Wi-Fi lock while the link is up. OEM task killers are the real enemy (aggressive vendors kill foreground services); the onboarding screen links to per-OEM instructions and the app detects an unexpected service restart and records it in the diagnostics.
- **Hotspot** [I]: the user starts the hotspot in system settings; apps cannot configure the main hotspot, only a local-only hotspot with a random SSID. Our app listens on all interfaces and reports which one the glasses came in on. Whether Doze or the vendor's hotspot manager drops the hotspot with the screen off differs by device and is a P0 test.
- **Permissions UX and Play policy**: microphone and nearby-devices permissions with prominent disclosure; Play's user-data and sensitive-permission declarations for an "always listening" feature [I]. The glasses record bystanders; the privacy section (11) covers the indicator and consent stance.

### 7.2 iOS (phone)

Verified where marked:

- **Local Network privacy** [V] (TN3179): iOS 14+; the first local-network operation prompts; user control in Settings; no MDM override; no API to query the state (use `NWConnection`'s `waiting` state with unsatisfied reason `localNetworkDenied`, or Bonjour policy-denied errors). **Listening for and accepting incoming TCP and receiving UDP unicast do not require the permission; outgoing TCP, UDP unicast send, multicast, broadcast and all Bonjour operations do.** Multicast and broadcast also need the multicast entitlement, and Bonjour browsing needs `NSBonjourServices` entries. If the app is in the background with the state undetermined, the system denies the operation silently and does not record the denial; the prompt then appears at the next foreground operation. So the design [D]: the glasses dial in (no prompt on the main path), the pairing screen deliberately performs one local-network operation in the foreground to prompt at a good moment, and the app handles `waiting` states.
- **Personal Hotspot** [V]: it must be enabled manually in Settings; an app cannot start or configure it (Apple DTS, "No"). STA-to-STA forwarding currently works but Apple describes it as behavior, not an API contract that they might change **[V]**. A forum answer I could not verify this session says IPv4 client-to-client traffic changed in iOS 18 and IPv6 link-local is the dependable path **[U]**; the P2 spike must test an iPhone hotspot with a real glasses unit and keep an IPv6 link-local option. Hotspot idle shutdown and the "maximize compatibility" (2.4 GHz) toggle are known behaviors I have not verified **[I]** and affect the glasses' 5 GHz link.
- **Background execution** [V for the mechanism, I for the strategy]: `UIBackgroundModes` with `audio` keeps an app running only while it is actually playing or recording audio; when that stops the system suspends the app. `bluetooth-central` allows discovery, connection and delegate callbacks in the background, with limited scanning (service UUIDs required, duplicates coalesced, slower intervals) and about ten seconds of processing per event, plus state preservation and restoration after termination. A held TCP socket does not keep an app alive, and App Review limits background services to their intended purposes (guideline 2.5.4) and requires explicit consent and a clear visible or audible indication when recording (2.5.14) **[V]**.
- **Consequence for this app** [D, I]: the iPhone does not record or play audio itself in the normal flow; it moves bytes between glasses, ElevenLabs and the CoS. Nothing in that flow is a legitimate reason for a long-lived background process other than the BLE link. So on iOS the **BLE connection is the keep-alive and the doorbell**: the glasses write a characteristic ("wake, utterance starting"), iOS wakes the app via `bluetooth-central`, the app starts a background task for the duration of the turn, tells the glasses over BLE that it is ready, and the glasses dial the TCP/Wi-Fi session. Whether the wake-to-ready time fits the latency budget (add 200 to 600 ms) and whether iOS honors the pattern after hours of suspension are P2's central questions. Do **not** use a silent audio loop, a fake VoIP or a location mode to stay awake: those are the standard reasons an app is rejected under 2.5.4 [I] and break when Apple tightens enforcement.
- **App Store risk, honestly** [I]: an always-listening wearable companion that streams a microphone raises privacy review (2.5.14, data collection disclosures); BLE and Wi-Fi accessories are common and acceptable; the background strategy above is legitimate (Bluetooth accessory communication) but must be explained in review notes. TestFlight and the enterprise or ad-hoc route remain available while the owner decides on public release (Q9).

### 7.3 What goes wrong in each case

| Failure | Android | iOS |
|---|---|---|
| App suspended or killed overnight | OEM task killer; FGS stops; CDM presence relaunches | Suspended; BLE restoration relaunches |
| Glasses cannot reach the phone | Hotspot off; AP client isolation | Hotspot off; IPv4 client-to-client change |
| First connection silently blocked | Not applicable | Local Network denied or undetermined in background |
| Mic returns silence on glasses | G0 failure (section 3.4) | Same (glasses side) |
| Reply cut off | Doze network; jitter buffer underrun | Background task expired mid-turn |

---

## 8. Integration

### 8.1 Cloud CoS and `weftos-weaver`

- The phone node enrolls as a surface (plan 4.3): keypair, dashboard approval, short-lived node tokens. Commands from the cloud arrive over the plan's transports and are translated: `speak{utt_id,...}` to `audio.down` frames; `show{scene|glance}` to a notification or screen on the phone (the glasses have no display); `capture{mode,max_s}` to `capture.start`; `stop` to `play.flush` and `capture.stop`; `probe{kind}` to a bounded `probe` on the glasses; `status{}` returns the merged phone plus glasses status. The glasses are addressable as their own node so `devices.status{node}` can answer "is the glasses mic working?" from the agent's real mic state and last-gate time, which is what the owner asked for in the capability notes [R].
- **Which transport carries what**: control and text by Realtime/SSE/long-poll; ElevenLabs audio by direct WebSocket; glasses audio by the local link. Nothing audio-sized passes the Vercel limit (plan 5.3). The legacy bridge (`/api/nodes/cos/sync`, `pending?wait=25`) stays valid during the transition: in P0 the phone can speak the long-poll floor so the spike does not wait for the new command channel.
- **Relay versus direct**: the phone node always talks to the cloud (relay model) and to the glasses directly. The cloud never reaches the glasses by itself, and the glasses never open an internet connection of their own [D].
- **Mesh**: the phone node announces itself on the mesh when a path exists and receives mesh traffic addressed to `weftos://<mesh>/nodes/<phone>`. Phone-only users get the dashboard-minted workspace mesh authority (plan Q3) until a real mesh pairs.
- **Parent relation in names**: the glasses node record carries `parent=<phone node id>` in its facts. Whether ADR-114 needs a nested form for a node whose parent is a node (it so far names meshes, nodes, projects and exposures) is [U] and goes to the ADR owner.

### 8.2 The `/phone` dashboard page

Three roles, no conflict [D]:

1. **Desktop**: the existing page stays, but its Mentra tab reads the **glasses node's status rows** instead of the Mac-written `listen.status.json` and `speak.status.json` (`docs/plans/cos-dashboard-bridge.md`). The tab's push-to-talk and listening toggle become commands to the glasses node (`capture.start`, listening on/off) through the same `cos_node_commands` the plan defines, instead of `cos_requests` with `kind` handled by the Mac bridge.
2. **In the app**: the page loads in a WebView with the app's signed-in session for rich screens (history, settings, catalog), and a small JavaScript bridge exposes native features the page lacks: foreground push-to-talk from the phone's own mic, link status, and the local switches. The page remains the single place where UI features land first.
3. **Phone-first users**: the app's home screen is native and minimal (connection state, a mic toggle, the last turn), so the always-on part never depends on a web page loading.

### 8.3 The phone's own sensors as node capabilities

Offer, each as an ADR-120 exposure, default off, user switch on the phone, battery-aware sampling [D]: coarse location, activity and step count, barometer, ambient light, battery, Wi-Fi scan RSSI, Bluetooth scan, camera still, microphone (push-to-talk). They are announced as `sensor.phone.*` and `surface.*` capabilities so the CoS and the RuView/world-model work can use them in the same ways as any node's. No CSI on a phone: neither Android nor iOS exposes it through public APIs as far as I know **[I]**.

---

## 9. Phased build plan

### P0: spike (Android, 2 to 3 weeks)

**Goal**: with the Mac off and the lab network absent, say a sentence to the glasses and hear the CoS answer in them, using only the phone.

Scope: (a) glasses agent APK v0 (capture to socket; socket to `AudioTrack`; PCM16; no Opus; no camera; no LED); provisioned over USB. (b) Phone app v0 (Kotlin; the Rust core only where it already exists: Noise and framing from `mentra-leaf`); one foreground service; a TCP listener; ElevenLabs STT via a single-use token, with a dev-only stored key allowed if the token route is not ready, labeled spike-only; text to the CoS by the existing long-poll relay; ElevenLabs TTS (fixed voice) streamed to the glasses; cue tones cached on the glasses. (c) Pairing by the speak-the-code flow over BLE if BLE coexistence (G2) holds, else by USB-provisioned key exchange. (d) Measurement harness: timestamps at every stage of the section 5.4 table, logged on the phone.

Gates and acceptance tests:

| # | Test | Pass |
|---|---|---|
| G0 | Mic after foreground launch, glasses idle | Non-silent audio 30 min after last foreground launch, process never visible; record the result after a Wi-Fi change and after a glasses reboot (even a fail is a result) |
| G1 | Speaker streaming without adb | 10 s PCM stream plays; first frame to sound under 150 ms; `flush` stops within 100 ms |
| G2 | Coexistence | Our BLE advertise/connect works while the stock client is present; concurrent mic does not silence either; camera question answered |
| A1 | End-to-end loop | Glasses on the phone hotspot, Mac powered down, say "what is on my board" and hear the answer; p50 end-of-speech to first sound at most 2.5 s over 20 trials (the 1.0 s target is P1) |
| A2 | Soak | 30 min of intermittent speech, no drop, no crash, no growth in memory |
| A3 | Screen-off | Phone locked for 10 min, then a turn works, with battery optimization allow-listed |
| A4 | Reconnect | Toggle hotspot off for 30 s and on; link re-forms in under 15 s without touching the app |
| A5 | Wrong peer | A second device attempting to connect, or the wrong key, is refused and logged |
| A6 | No Mentra | Block Mentra domains at the router or by inspecting traffic; the loop still works and the phone/glasses emit no traffic to them from our app |

Risks: G0/G1/G2 failing (the plan changes: ask asg_client for help, or an owner decision on privileges). Hotspot vendor quirks. Token minting not ready (use the dev key). **Owner decisions**: Q1 (spike-only key), Q2 (which Android phone), Q7 (updates).

### P1: production Android (6 to 10 weeks after P0 passes)

Scope: Rust core extracted (`weftos-leaf-parent`, `weftos-glasses-proto`, `weftos-node-core`) with UniFFI; glasses agent as a signed release APK and a recovery path; WLF1 certificate flow, enrollment in the dashboard, revocation; command channel (`cos_node_commands` with Realtime/SSE/long-poll); `speech.v1` including offers, cancel and barge-in with AEC; Opus; STT fallback chain; TTS cache; BLE fallback mode; CDM presence; honest notifications; diagnostics screen; the dashboard WebView with the JS bridge; camera stills and `probe`; `cos_ledger` accounting for speech; Play internal then closed testing.

Acceptance: the plan's M3/M4 criteria for a phone and a glasses surface; first-sound p50 at most 1.2 s and p90 at most 2.2 s without a tool, on the hotspot; ten-hour overnight soak with no manual restart (the link service survives at least one forced kill via CDM relaunch); revocation takes effect on the next connection and the glasses cannot rejoin; switching the glasses mic exposure off locally refuses the next `capture`; a log of every use without bodies; two users on two phones see only their own devices; no audio stored by default.

Risks: glasses OTA changes the environment; Play review of a foreground microphone listener; the `weftos-leaf-parent` extraction is larger than expected; battery on the glasses. **Owner decisions**: Q3 (Play listing vs sideload), Q4 (camera in scope), Q5 (cache policy), Q8 (iOS timing).

### P2: iOS (4 to 8 weeks, after P1 stabilizes)

Scope: Swift/SwiftUI shell over the same UniFFI core; Core Bluetooth central with state restoration; the doorbell protocol on the glasses side; listener-based local networking; Local Network prompt at pairing; TestFlight; a hotspot test matrix (iPhone as the AP, and shared Wi-Fi); an IPv6 link-local path; App Review notes.

Acceptance: a full turn with the iPhone locked and the app suspended, woken by the doorbell, with the added wake-to-ready time measured and within an agreed bound (target at most 600 ms p50); the Local Network denied state is detected and explained; killing the app from the switcher and relaunching via BLE restoration works (or the failure is documented and the feature is marked foreground-only); a TestFlight cohort of at least three people for two weeks.

Risks: iOS may not allow the always-available behavior at all (decision: foreground-only or "wake by gesture"); hotspot IPv4/IPv6 changes; App Review. **Owner decisions**: Q8, Q9, Q10.

### P3: CSI research (parallel, research only)

See section 10. Acceptance: a written feasibility memo with a go/no-go per tier, the chip identified, the driver and firmware sources located or declared unavailable, and a bench capture on a sacrificial unit if the owner agrees. Owner decisions: Q11 (a second glasses unit to risk), Q12 (RuView priority).

---

## 10. CSI from the glasses' Wi-Fi: feasibility research

### 10.1 What we know and what we do not

- Wi-Fi 5 (802.11ac) at 5 GHz, 390 Mbps link rate, driver named `wmtWifi`, Bluetooth on a MediaTek combo radio called "Cervino" in the profile, SoC MT6761 **[R]**. 390 Mbps at 80 MHz is the single-stream rate, so one receive chain [I]. One chain means no angle of arrival; CSI is still a per-subcarrier amplitude and phase stream usable for presence, motion and breathing.
- **Chip ID: [CHIP-ID: pending, being gathered separately by Grok].** Method to confirm it [D]: `adb shell` reads of the kernel log lines from the WMT/combo driver (chip ID and firmware load lines), the module and firmware file names under the vendor partition (the Wi-Fi RAM-code file names normally embed the chip number), `lsmod`/module paths for the Wi-Fi driver, `getprop` for `ro.vendor.connsys.*`, `cat /sys/class/net/wlan0/...`, and a read of the `connsys`-related kernel config from `/proc/config.gz` if present. A candidate by pairing (not verified here) is the MT6631 combo (Wi-Fi 5 1x1, Bluetooth, FM, GPS) that commonly sits next to an MT6761-class SoC **[I]**; the public sources I searched confirmed that MT6631 exists as a Wi-Fi, Bluetooth, GPS and FM part but did not confirm the pairing **[V partial]**. Do not act on the candidate until the ID is read from the device.

### 10.2 CSI tooling by chip family

| Family | Public tooling | State |
|---|---|---|
| Intel 5300 / AX200 / AX210 | Linux 802.11n CSI Tool; PicoScenes | Mature on PCIe cards [I] |
| Atheros / Qualcomm (AR9xxx, some QCA9xxx/ath10k) | Atheros CSI Tool; PicoScenes | Mature on cards and some routers [I] |
| Broadcom/Cypress (BCM43455c0, BCM4339 and relatives) | Nexmon CSI (firmware patching) | Works on specific chips and specific firmware builds, including some phones [I] |
| Espressif ESP32 family | `esp-csi` in ESP-IDF | Official, in this project's hands (the C5 port in `RuView`) [R/I] |
| MediaTek mt76 (Filogic, MT7915 and similar router/PCIe chips) | A reverse-engineered netlink vendor interface and patches for mt76 on OpenWrt, exposing I/Q plus metadata (source MAC, RSSI, SNR, sequence) | Real, community-maintained; tested on a Filogic router; not a mobile combo chip **[V]** |
| MediaTek mobile combo (MT663x with `wmtWifi`/gen4m driver) | None found | **[U]** This is our case. The out-of-tree Android Wi-Fi driver for Connac-generation parts has a CSI-related build option on some generations **[I, from memory]**; whether this chip generation and this device's driver have it, and whether the firmware supports it, is the first thing to read in the vendor kernel source drop |

### 10.3 Paths and their risk

| Tier | What | Needs | Risk |
|---|---|---|---|
| **T0: no firmware change** | RSSI/RTT-style sensing: link RSSI and beacon-level statistics at 10 Hz from the agent; a phone transmitting a controlled packet stream to the glasses to raise the sample rate | The agent only | Zero risk; coarse. A useful baseline and a way to measure what the signal carries |
| **T1: driver/kernel** | Build the vendor kernel module with CSI enabled if the driver supports it, flash it | Kernel source for this exact build (or a reconstruction), a bootloader unlock path (the profile notes MediaTek BROM/preloader tooling for dumps **[R]**), a boot/vendor image re-sign or verified boot off | Medium to high: voids the stock state, A/B and verified boot ("green" now **[R]**), OTA breaks, brick risk with BROM recovery as the safety net. Needs a sacrificial unit |
| **T2: chip firmware** | Patch or replace the combo chip's RAM code (Nexmon-style) | The firmware blob, an architecture for the chip's embedded core, a way to load it, signature behavior | High and unknown: not ruled out; the firmware may be signed or encrypted; weeks to months of reverse engineering [I] |
| **T3: alternative hardware** | A glasses variant that carries an ESP32-class radio, or an add-on sensor | Hardware work | Out of scope here; the existing ESP32-C5 CSI path is the reference |

"Redo the wifi firmware" in the owner's words most plausibly means T1 first, T2 only if T1's driver lacks the hooks. Both break Mentra OTA continuity: an updated Mentra build would overwrite or conflict with a custom image [I]. A replaced image must still keep the Wi-Fi stack usable for the ordinary path (the phone link), or the glasses lose their parent link.

### 10.4 What it would give RuView

- A **wearable CSI source**. Worn: the receiver moves with the head, so CSI reflects the wearer's own motion and the environment around their head; useful for head-motion and coarse proximity, not for a fixed room map. At rest (on a desk or charging case): a fixed single-chain, 80 MHz receiver, usable as one more sensing node beside the ESP32-C5 nodes.
- The **phone is the natural transmitter**: it is the AP in hotspot mode and can address controlled UDP packets to the glasses at 50 to 100 Hz, giving a known cadence without extra hardware [D]. Power saving on the glasses' Wi-Fi (DTIM) will reduce the usable frame rate unless the radio is kept awake, which costs battery [I].
- **Limits**: one chain, phase offsets from carrier and sampling frequency drift that the pipeline must sanitize, AGC jumps, and the frame cadence being set by the AP's own traffic. These are the same issues the ESP32 CSI pipeline already faces.
- **Privacy and law**: sensing people near the wearer without their consent is a different act from recording the wearer's own voice; this needs the same consent and indicator stance as the camera and microphone, and an owner decision on whether it is ever on outside the owner's own spaces.

### 10.5 What P3 does

Read the chip ID; locate the vendor kernel and driver source (for this exact Mentra build, or declare it unavailable); read the driver for CSI hooks and the firmware for sections; build a T0 capture in P1 so we know what the signal carries before touching firmware; write the memo; bench T1 on a sacrificial unit only with the owner's approval.

---

## 11. Privacy, safety and policy

- The glasses record other people. The design keeps an indicator and a switch on every capture path: the cue tone when the gate opens or listening starts, the LED if one is controllable, and a persistent phone notification. Nothing is stored by default: audio is processed in memory; transcripts go to the user's own CoS data; the TTS cache is stock phrases only.
- Local control wins (ADR-120): the phone UI switch refuses the next command; a hardware gesture on the glasses if the agent can read one [U].
- No bearer on the glasses, no long-lived secret on the phone beyond the wrapped seed, single-use speech tokens, short-lived node tokens.
- Third-party processing: ElevenLabs sees audio when the cloud STT/TTS path is used. The settings show the active path; a "local only" mode uses the on-device recognizer and platform TTS with degraded quality.
- No hostnames, tailnet addresses, tokens or client names belong in this repository's docs; the implementation keeps them in secrets and per-user config.

---

## 12. Open questions, with defaults

| # | Question | Default |
|---|---|---|
| Q1 | May the spike store a dev ElevenLabs key on the phone, labeled spike-only, if the token route is not ready? | Yes, for P0 only; never in P1 |
| Q2 | Which Android phone and OS version is the reference device? | The owner's daily phone, plus one older mid-range device for OEM battery behavior |
| Q3 | Distribute through Play (internal then closed testing) or sideload only? | Play internal testing in P1; public decision later |
| Q4 | Is the camera in scope for P1 (stills on command)? | Stills yes, clips and streaming later |
| Q5 | TTS cache: stock phrases only, or full replies opt-in? | Stock phrases only; opt-in for full replies |
| Q6 | Does the phone run STT on-device when online? | No; cloud when allowed, on-device when offline or "local only" |
| Q7 | How are glasses agent updates delivered? | USB or a phone-started Wi-Fi adb session the user approves each time; no standing adb |
| Q8 | When does iOS start? | After P1 acceptance; start a one-week iOS feasibility spike (doorbell and hotspot only) during P1 |
| Q9 | Public App Store release, or TestFlight only? | TestFlight until the background behavior is proven |
| Q10 | If iOS cannot be always-on, is "foreground or wake-by-gesture" acceptable? | Yes, documented as a platform limit |
| Q11 | May we buy a second glasses unit for firmware risk? | Yes; no firmware work on the daily unit |
| Q12 | CSI priority against P1/P2? | Research only (T0 capture in P1); T1 after P1 |
| Q13 | Where does the leaf-parent code live: the kernel crates or a new small crate? | A new small crate, adopted by the kernel |
| Q14 | Is the parent relation expressed in ADR-114 names or in node facts? | Node facts for now; ADR owner decides |
| Q15 | Does the glasses agent accept being managed by a hardware button gesture (mute)? | Yes if the stock client exposes the event; otherwise no |

---

## 13. Sources

Verified this session (primary):

- Android foreground service types and requirements: https://developer.android.com/develop/background-work/services/fgs/service-types
- Android restrictions on starting foreground services from the background, including the while-in-use rules and exemptions: https://developer.android.com/develop/background-work/services/fgs/restrictions-bg-start
- Android BLE in the background, `CompanionDeviceManager` presence and `CompanionDeviceService` permissions: https://developer.android.com/develop/connectivity/bluetooth/ble/background
- Android `SpeechRecognizer` (on-device recognizer, API 31, `EXTRA_AUDIO_SOURCE`): https://developer.android.com/reference/android/speech/SpeechRecognizer
- Android 15 `dataSync` and `mediaProcessing` roughly six-hour timeout (beta announcement): https://android-developers.googleblog.com/2024/05/the-second-beta-of-android-15.html
- Apple TN3179, Understanding local network privacy: https://developer.apple.com/documentation/technotes/tn3179-understanding-local-network-privacy
- Apple Core Bluetooth background processing (bluetooth-central, scanning limits, state restoration): https://developer.apple.com/library/archive/documentation/NetworkingInternetWeb/Conceptual/CoreBluetooth_concepts/CoreBluetoothBackgroundProcessingForIOSApps/PerformingTasksWhileYourAppIsInTheBackground.html
- Apple App Review Guidelines (2.5.1, 2.5.4, 2.5.14): https://developer.apple.com/app-store/review/guidelines/
- Apple Developer Forums, Personal Hotspot cannot be enabled by an app: https://developer.apple.com/forums/thread/802003
- Apple Developer Forums, Personal Hotspot STA-to-STA forwarding is not a contract: https://developer.apple.com/forums/thread/699414
- UniFFI (Kotlin, Swift and Python full support): https://mozilla.github.io/uniffi-rs/latest/

Verified only through secondary summaries (treat as [I]):

- iOS `audio` background mode keeps an app running only while recording or playing (Apple forum posts surfaced by search): https://developer.apple.com/forums/thread/106415
- MediaTek mt76 CSI extraction on OpenWrt: https://forum.openwrt.org/t/ekstra-csi-full-metadata-csi-extraction-for-mt76-on-openwrt/247100
- MT6631 as a Wi-Fi, Bluetooth, GPS and FM part: https://www.mediatek.com/press-room/mediatek-announces-worlds-smallest-4-in-1-combo-chip-wi-fi-bluetooth-gps-fm-solution (the page concerns an older combo chip; MT6631 appeared in search snippets only)

Repo and workspace material read: `docs/plans/cos-cloud-migration.md` (sections 4, 5, 8), `docs/plans/cos-dashboard-bridge.md`, `docs/leaf-push-protocol.md`, `docs/plans/adr-103-leaf-implementation.md`, `docs/adr/adr-120-mesh-resources-and-exposures.md`, `firmware/mentra-mic/` (README and manifest), the AudioBridge source and the Mentra Live device profile in the glasses workspace, and the `mentra-leaf` bring-up source.
