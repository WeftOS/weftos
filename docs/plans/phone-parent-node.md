# WeftOS phone app as the glasses' parent node

Status: Proposed design, revision 2, 2026-10-13. Research and design only; nothing here is built. Owner: the WeftOS owner. Companion plans: `docs/plans/cos-cloud-migration.md` (M4, "every surface is a node"), `docs/plans/cos-dashboard-bridge.md` (relay and control files), `docs/leaf-push-protocol.md` and `docs/plans/adr-103-leaf-implementation.md` (leaf and certified leaf), ADR-114 (`weftos://` names), ADR-120 (exposures).

**Owner direction (2026-10-13), verbatim:**

- "we do not want to use mentra cloud for audio and transcriptions and video, we want the app on the phone to control the node inside the glasses. I was thinking we may even be able to redo the wifi firmware eventually and do CSI from them"
- "Let's look at this as an android app, but we may end up having to do it on apple as well."

**Evidence tags.** **[V]** verified against a primary source this session (URL in section 12). **[I]** inferred from memory, from a secondary source, or by reasoning; treat as a hypothesis until a spike confirms it. **[U]** unknown; named so it gets answered. **[D]** a design decision proposed here. **[R]** read from the repo or the glasses workspace.

---

## 1. Summary and decisions

Revision 2 (2026-10-13): the owner corrected the speech design. **The phone app does no STT, TTS or other speech processing.** It moves audio and text between the glasses node and the mesh; STT, speech shaping and TTS run in a durable workflow on the weaver side, reached over the mesh. Sections 1, 2, 5, 7, 8, 9 and 12 reflect that. Section 6.4 (the existing `clawft-android-edge` crate) was added in the same revision. Revision 3 adds the LED capability and the `indicator` state machine (3.6, 3.7) and moves the CSI research to the glasses workspace (section 10).

1. The phone runs a **WeftOS node** (a Rust core in a native Android shell now, an iOS shell later). It is the **parent** of the glasses node. The glasses run a small agent that holds a leaf identity, streams the microphone to the phone, plays what the phone sends, and takes control commands.
2. **Nothing goes through Mentra cloud.** The glasses keep their stock firmware and the stock Mentra client app only as the hardware owner (launcher, camera HAL, LED, speaker path). We never talk to Mentra servers and never need the Mentra phone app.
3. The **glasses dial the phone**, over the phone's hotspot or a shared Wi-Fi. BLE is the doorbell, the provisioning channel and the low-rate fallback; it is not the audio path.
4. Speech: the phone is a **thin byte mover**. Glasses audio goes up the mesh to a **per-user durable speech workflow** (STT, the CoS turn, speech shaping, ElevenLabs TTS, all server side); rendered reply audio comes back down and the phone forwards it to the glasses speaker. Audio is Opus end to end so the cellular leg stays small (about 3 kB/s per direction). Echo cancellation and the cheap energy gate live on the glasses, because the glasses own both the microphone and the speaker.
5. Cross-platform: **one Rust core with UniFFI bindings and thin native shells** (Kotlin/Compose now, Swift/SwiftUI later). The repo already has a seed for the core, `crates/clawft-android-edge` (identity, pair store, UniFFI plumbing, 967 lines), which is a good start and not yet a node (section 6.4).
6. Biggest risks, in order: (a) the glasses cannot hold the microphone or the speaker path from an ordinary app without a foreground launch or privileges ([U], section 3.4); (b) **where the audio ingress lives**: the cloud plan assumed audio never passes through Vercel, and a durable workflow is not a low-latency audio pipe (section 5.2); (c) iOS cannot keep a process alive in the background the way Android can (section 7.2); (d) the audio route to the glasses speaker depends on a serial command to a co-processor (section 3.5).
7. **P0 is a spike** that proves the loop without the Mac, on Android, in two to three weeks, and answers the unknowns above, because they decide whether P1 is a plan or a rewrite.

---

## 2. Architecture

### 2.1 The picture

```
        durable speech workflow (per user, weaver side / cloud CoS)
        STT -> CoS turn -> speech shaping (speech.v1) -> ElevenLabs TTS
                       ^                         |
                       | mesh messages:          | mesh messages:
                       | utterance chunks        | reply audio chunks, text, events
                       | (Opus, ordered, resumable)
                  +----+-------------------------v----+
                  |       PHONE NODE (thin)           |   weftos://<mesh>/nodes/<phone>
                  |  Rust core: identity, Noise,      |
                  |  leaf-parent, mesh client, relay. |
                  |  Native shell: service, BLE, IO.  |
                  |  NO STT / TTS / VAD / AEC.        |
                  +----+---------------------+--------+
                       | local Wi-Fi link     | BLE (doorbell,
                       | (TCP, Noise)         |  provisioning, fallback)
                       v                      |
          +-------------------------+         |
          |  GLASSES NODE (child)   |<--------+
          |  agent APK: mic gate +  |
          |  Opus enc/dec + AEC,    |
          |  speaker, LED, camera   |
          +-------------------------+
```

- **Phone node.** A node with its own keypair, enrolled to the user in the dashboard, named `weftos://<mesh>/nodes/<phone id>` (ADR-114). It is credentialed to the user. Its jobs: parent of the glasses (certificates, link, policy), and relay between the glasses link and the mesh. It does no speech processing; it may show a message or play a notification sound as a surface, but the speech path never decodes audio.
- **Glasses node.** A child node with its own Ed25519 key and a certificate issued by the parent chain (section 4.3). It announces `surface.mic`, `surface.speaker`, `surface.led` and optionally `surface.camera` to its parent, which republishes them upward as exposures. The cloud CoS addresses "speak on node N" and never learns that the phone is a hop [D].
- **Speech workflow.** The owner's design: the phone "uses the mesh to send it off to durable workflow". A durable workflow is a per-user run (the cloud plan's Workflow runner, or a speech child workflow of the per-user CoS) that owns STT, the CoS turn, speech shaping and TTS. It is named as a mesh service (`speech.v1` exposure on the user's mesh), so *placement* decides where it runs: the cloud by default, a LAN node with a GPU when the phone can reach one, a local fallback. The phone does not know or care which (section 5).
- **Why the glasses dial the phone.** On the phone's hotspot the phone is the default gateway the glasses already know; no discovery is needed. On a shared Wi-Fi the glasses use signed discovery (the certified-leaf pattern) or mDNS. The direction also fits iOS: accepting inbound TCP and receiving UDP unicast need no Local Network permission, while outgoing connections to local addresses do **[V]** (Apple TN3179).
- **Transport ladder** for phone to glasses: (1) Wi-Fi TCP over the hotspot or shared network; (2) the same over an overlay network if both are on it [I]; (3) BLE for control and a reduced audio fallback (section 4.4). Under client isolation with no hotspot, only BLE remains.
- **Phone to mesh/cloud**: control and text use the cloud plan's transports in order (Supabase Realtime, SSE, long-poll). **Audio uses a dedicated streaming leg**, because Vercel functions cannot take inbound WebSockets and the plan capped payloads at 4.5 MB (section 5.2). The glasses hold no cloud credential [D].
- **Phone to the rest of the mesh**: when the phone sees a mesh member (home or lab Wi-Fi, or an overlay), its core dials that hub's certified-leaf listener (the `leaf_listen_addr` list supports several addresses at once **[R]**). Off-network it is cloud-only.

### 2.2 What the phone is not

Not a relay for Mentra's protocol, not a MentraOS app, and no Mentra account is needed. The glasses' stock client keeps owning pairing with Mentra's own app; we do not use or disturb it ([U]: whether a second BLE client can coexist, section 3.2).

### 2.3 Phone as a surface and a host

The phone is both a surface (display, touch, push-to-talk) and the transport and policy point for the glasses, which have their own identity but no route out except the phone. ADR-120 holds at both hops: nothing exposed by default, the local switch wins (phone screen, and a glasses gesture if readable, [U]), the dashboard may switch off but not on, every use logged without bodies.

---

## 3. The glasses side

### 3.1 What the glasses are

From the device profile [R]: a MediaTek MT6761 phone-class SoC (4x Cortex-A53, about 2 GB RAM, 24 GB storage), Android 11 (API 30), arm64, a build with `ro.debuggable=1` and permissive SELinux, Wi-Fi 5 at 5 GHz (390 Mbps link rate, which is a single spatial stream at 80 MHz [I]), a MediaTek combo radio for Bluetooth, no GPS, no cellular, A/B partitions with verified boot green. The `mentramic` README speaks of Android 14 behaviors, but the device properties read in Grok's probe confirm **API 30 / Android 11 [R]**; the README wording is wrong or refers to its own target SDK. Everything below assumes API 30.

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

The serial node it opens is the link to the BES2700 co-processor beside the CONNSYS **[R: the glasses' UART notes]**. Two consequences. First, streaming playback needs `AudioTrack` (PCM in, low latency, flush on barge-in), not `MediaPlayer` on files; a streaming path has to keep the I2S path open for the length of an utterance and close it after silence. Second, the serial path itself. The glasses' own protocol notes **[R]** document it: `/dev/ttyS1`, 460800 baud, 8N1, no flow control, mode `crwxrwxrwx` (world accessible, no root needed), carrying one JSON object per line, `{"C":"<code>","V":1,"B":"<data>"}` (the "K900" protocol), between the MT6761 and the BES2700. The stock `asg_client` opens it (`SerialManager.openSerial`) and runs a reader thread on it. An unprivileged app can therefore open the port to **write**; the Mac scripts already do this from `adb shell` while asg_client may also be writing ("one quoted line"). **Reading** is the problem: two readers on one tty split the bytes, which would corrupt asg_client's input. The agent is therefore write-only on the port [D] and learns the BES events that arrive on it (`hs_ntfy` button clicks, `sr_vad` hardware voice activity, `hm_batv` battery) only if asg_client forwards them as broadcasts **[U]**. Writes must be whole lines and serialized with a lock; interleaving with asg_client's own writes is the residual risk [U]. If the port turns out to be unusable, the fallback is to ask asg_client to play through the path it already owns (an exported intent or its own audio API), or classic Bluetooth audio if the glasses present a sink, which is not known [U]. This is the second gate: **G1 passes if the agent plays a 10-second Opus stream (decoded on the glasses) through the speaker with no adb session attached, with under 150 ms from the first frame to sound and clean stop on `flush`.**

### 3.6 Control, status, events

The agent exposes these to the parent only (section 4.2 for wire format):

- **Control**: `capture.start{mode: ptt|vad|continuous, max_s}` and `capture.stop`; `play.begin{utt_id}`, `play.flush{utt_id}`, `volume.set`; `cue{kind}` (cached tones: heard, working, done, error); `indicator.set{state}` (section 3.7), `led.set{state}` (below) and `cue_volume.set`; `camera.capture{still|clip, quality}` (P1); `probe{kind}`; `ping`.
- **Status** every 5 s and on change: battery, charging, Wi-Fi RSSI and link rate, link RTT to the parent, mic state and level, speaker state, agent and asg_client versions, thermal state, free storage.
- **Events**: `mic.gate{open|close, rms, voiced}`, `button{...}` and `wear{...}` only if asg_client broadcasts them [U], `link.up|down`, `playback.done{utt_id, spoken_through_seq}`.
- **LED (supported capability, `surface.led`).** The owner confirmed an RGB LED on the inside of the frame. It is driven through the BES co-processor control path, not an Android LED class device **[R: BES2700 UART notes, `mentra-cos-speak`, and Grok's session log]**. The sequence over the serial path of section 3.5: (1) claim authority: `{"C":"android_control_led","V":1,"B":"1"}`; (2) set: `{"C":"cs_ledon","V":1,"B":"<color>,<on_ms>,<off_ms>,<cycles>,<brightness>"}` (colors per the notes: red 0, green 1, blue 2, orange 3, white 4; brightness 0 to 255; Grok's example `1,3100,100,1,180` is green, 3100 ms on, 100 ms off, one cycle, brightness 180); (3) `{"C":"cs_ledoff","V":1,"B":""}`; (4) release authority with `B":"0"`. Authority exists so the BES firmware's own LED animations do not fight ours, so the agent claims it only while a state is shown and releases it when idle [D]. A separate MTK-side recording LED (GPIO, vendor library) belongs to the camera path; **the agent never touches it** and never suppresses a recording indicator. The field order is disputed between the glasses' own notes: the UART protocol doc says `color,on_ms,off_ms,cycles,brightness` (example `1,500,500,3,200` is green, 500/500 ms, 3 cycles, brightness 200), while the testing notes say `color,brightness,on_ms,off_ms,cycles`; the protocol doc is primary and gate G-LED confirms it by eye. Whether writes work while `asg_client` holds the port is unconfirmed (a world-writable node worked from the shell under permissive SELinux, but no LED change was observed during a CoS reply) **[U]**; no BES reply is read. Colors are unconfirmed by eye **[U]**. The states are in the `indicator` machine of section 3.7; the colors there are proposals pending an eyes-on check. A steady state is a long `on_ms` renewed before it expires, since a cycle count is the only repetition control seen. The LED is also the **mic-open indicator**: it shows `listening` whenever the gate is open (section 11). If the serial write fails, the agent falls back to a cue tone and reports `led:"unavailable"` in status, never silently dropping the indicator. Transport dependency: the LED shares the BES serial path that asg_client currently holds (section 3.5); G1 and G2 therefore also cover LED writes.
- **Leaf announcement**: `LeafServices` with sources (mic, camera) and sinks (audio out, led). The glasses have no display, so there are no scenes: the "scene" is the cue vocabulary (tones and LED patterns), which the parent renders through the same command set. `weftos-leaf-scene` stays a pad and e-ink thing.

### 3.7 Indicators: the `indicator` state machine

Owner requirement: small, understandable, non-intrusive sounds that show listening started, listening ended, ongoing thinking, and done with success or error, paired with the LED for the same states.

```
idle -> listening -> captured -> thinking -> done_ok | done_error -> idle
```

| State | Cue (sound) | LED | Entered by |
|---|---|---|---|
| `listening` | soft rising two-note chirp (E5 to A5) | steady blue | glasses, when the gate opens or on push-to-talk |
| `captured` | soft falling two-note chirp (A5 to E5) | blue fading out | glasses, when the gate closes |
| `thinking` | a very quiet slow pulse loop (one low tick about every 1.8 s) | slow breathing amber | glasses, automatically after `captured` |
| `done_ok` | one gentle chime | green flash | phone or workflow: `indicator.set{done_ok}` |
| `done_error` | soft low double tone | red flash | phone or workflow, or the glasses on timeout or link loss |

- **Cues play locally.** They are short pre-rendered assets (Opus or PCM) inside the glasses APK, so there is zero network latency. The phone and the CoS workflow send **state transitions only, never cue audio**. Reliable lane: `indicator.set{state, turn_id, seq}` (stale `seq` ignored, idempotent); the glasses report their own transitions as `evt{indicator.state}` so the workflow and ledger know what was shown.
- **Local ownership of the fast edges.** `listening` and `captured` come from the glasses' own gate, so the first chirp never waits for a round trip. The glasses move to `thinking` on their own and hold it until a `done_*` arrives or the reply audio starts (the loop is then ducked within about 40 ms and stopped, so it never overlaps speech).
- **Timeouts.** `thinking` becomes `done_error` after 20 s by default; the workflow can extend it with `indicator.hold{ms}` (max 60 s) for a known long tool turn. Link loss while `thinking` goes to `done_error` at once, locally, so a dead link still ends in an audible and visible error.
- **Volume.** A separate `cue_volume`, default lower than `voice_volume` (default 0.4 of it), adjustable and mutable; the LED still runs when cues are muted. `voice_volume` already exists in the control-file contract **[R: `weftos-cos` bridge]**. Cues pass through the same playback path as speech, so the AEC reference includes them.
- **Reuse.** The WeftOS cue work is `crates/clawft-channels/src/voice/cue.rs` (WEFT-664 replaced spoken acks with tones): `CueKind::Ack` is the rising E5 to A5 blip (70 ms and 90 ms tones, amplitude 0.15 and 0.12) and `CueKind::Working` is one low C5 tick (80 ms, 0.08), generated at 16 kHz with raised-cosine ramps, peak at or below -16 dBFS, each at most 250 ms **[R]**. `listening` is the `Ack` tone; `thinking` repeats `Working` at lower amplitude; `captured`, `done_ok` and `done_error` are new in the same style (the mirror of `Ack`; one longer-decay tone; two low tones). The assets are rendered by the same generator so the cues share one voice, and exported into the APK at build time. Exact pitches are proposals until a listening test.
- **Accessibility.** The five sounds differ by **pitch contour and rhythm**, not only pitch: rising pair, falling pair, a repeating single tick, one rounded tone, a low descending pair. The LED repeats the state redundantly by pattern (steady, fade, breathing, flash) as well as color, so neither a hearing difference nor color blindness removes the signal.
- **`done_ok` after speech.** Default: the chime plays when the turn completes, after any spoken reply ends. If that proves redundant, `cue_done_after_speech=false` keeps the LED flash and drops the chime (Q22).

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
audio.up:   {stream:1, utt, seq, ts_ms, codec:"opus", payload}   glasses to phone
audio.down: {stream:2, utt, seq, ts_ms, codec:"opus", payload}   phone to glasses
```

**Opus from P0, encoded on the glasses.** With the phone doing no processing, the codec on the glasses link is also the codec on the cellular leg. Raw 16 kHz PCM16 is 256 kbit/s, about 115 MB per hour of open gate, which is not acceptable on a metered hotspot uplink; Opus at about 24 kbit/s is 3 kB/s and 10.8 MB per hour. The glasses encode (the platform `MediaCodec` Opus encoder exists from API 29 **[I]**, or libopus in the Rust agent) and decode reply audio the same way. The phone forwards frames unchanged, which also makes the iOS relay trivial. Frames are 20 ms; the phone batches about 100 ms into one upstream message to cut per-message overhead. PCM16 remains useful as a debug codec on the local link only.

**Gating and echo.** The glasses run the cheap parts: an energy floor with calibration, a pre-roll ring buffer (about 400 ms, so word starts are never clipped), a watchdog, and **acoustic echo cancellation**, because the glasses are the only place that has both the microphone signal and the exact speaker reference at a known delay (Android's `AcousticEchoCanceler` effect, if the audio HAL honors it **[U]**, or the WeftOS AEC crate in the agent). The spectral voiceness gate and endpointing move to the workflow, so some non-speech audio reaches it. A hard cap (max open-gate seconds per minute, adjustable) limits cost and bandwidth. While no gate is open the glasses send only status.

### 4.3 Pairing, keys, no shared bearer

Requirements: no static shared secret, a lost phone or glasses can be revoked, and a bystander cannot pair. [D]:

1. The glasses agent generates an Ed25519 identity and an X25519 key at first run and advertises over BLE a short, rotating pairing beacon. The phone app, in its pairing screen, scans for it.
2. Phone and glasses run a Noise XX handshake over a BLE channel. Both compute a **short authentication string** from the handshake hash. The **glasses speak it** through the speaker ("pairing code four seven two"), and the user confirms it on the phone. This works on a display-less device precisely because there is a speaker; it defeats a man in the middle without a QR code, a cable or a PIN to type. A fallback is the same string flashed as an LED pattern, or the USB provisioning session reading the glasses key out through adb, which is already physical.
3. The phone signs a **leaf certificate** for the glasses key: scope (parent tenant), capabilities (mic, speaker, led, camera), issuer chain up to the user's identity, short expiry (days) with renewal on every successful connection. This is the certified-leaf shape (certificate, sequence floor, signed ACK **[R]**) with a phone as the immediate issuer instead of the daemon.
4. Over the now-encrypted BLE channel the phone passes the Wi-Fi credentials (section 3.3) and the glasses store the parent's public key. From then on the Wi-Fi link starts with a Noise IK-style handshake against that pinned key.
5. **Revoke**: the user removes the glasses in the dashboard or the phone app; the phone stops renewing, drops the key from its registry, and tells the cloud, so even a compromised glasses agent cannot rejoin. The glasses forget their parent on a factory reset of the agent.
6. **Phone's own identity**: an Ed25519 key enrolled in the dashboard by the request and approval flow of plan 4.3, exchanged for short-lived node tokens by signing a server challenge. Hardware-backed storage: Android Keystore and the iOS Secure Enclave primarily support P-256, not Ed25519 **[I]**, so the practical pattern is to wrap the Ed25519 seed with a hardware-backed AES key and keep the plaintext seed in memory only.  The phone holds no speech-vendor credential at all (section 5).

What flows nowhere: no Mentra account, no Mentra token, no bearer on the glasses, no long-lived secret on a device that a user can lose in a taxi.

### 4.4 BLE: what it is for and what it cannot do

BLE has three jobs: discovery and pairing (section 4.3); the **doorbell** (a wake notification, essential on iOS, section 7); and a **fallback control and reduced-rate audio path** when there is no usable Wi-Fi. Typical BLE 1M throughput of a few hundred kbit/s [I] would carry 24 kbit/s Opus in each direction on an L2CAP connection-oriented channel, with noticeably worse latency; that is a degraded mode ("it still hears me, answers are slower"), not the design center. Our BLE radio is the Android CONNSYS one; the stock phone link runs on a separate co-processor (the BES2700), so a conflict with the stock client is less likely than first assumed **[I]**, but unproven [U].

---

## 5. Speech: the phone relays, the workflow speaks

### 5.1 Division of labor

| Step | Where | Notes |
|---|---|---|
| Capture, energy gate, pre-roll, Opus encode, AEC | Glasses agent | Cheap DSP; owns mic and speaker |
| Link security, buffering, relay, retry, bandwidth policy | Phone node | No decode, no recognition, no synthesis |
| Endpointing, STT (ElevenLabs realtime by default), CoS turn | Durable speech workflow | The plan's STT facts apply server side (`scribe_v2_realtime`, about 150 ms to commit, per the plan **[V in plan]**) |
| Speech shaping (`speech.v1`: lead, offers, clause chunks, cues) | Same workflow, via the conversation MCP | Unchanged from plan section 5.4 |
| TTS (ElevenLabs, single key held server side) | Same workflow | The platform key never leaves the server; no per-device tokens are needed for speech anymore |
| Playout, jitter buffer, flush on barge-in | Glasses agent | Phone forwards `play.flush` and `spoken_through_seq` |

This simplifies the earlier design: no single-use token to the phone, no ElevenLabs socket on the phone, no on-device recognizer, no TTS cache or AEC on the phone. It also removes the main iOS risk of keeping a vendor WebSocket alive in the background (section 7.2). The cost: every utterance and every reply crosses the phone's cellular link, and the ElevenLabs concurrency limits (4 to 40 by plan, per the cloud plan) now apply to the whole user base at the workflow, not per device.

### 5.2 The mesh leg: what the phone sends and where it lands

Owner wording: the phone "sends the captured audio, or a reference to it, as a mesh message" to the workflow and "gets the rendered reply audio back over the mesh". Three concrete shapes, one per phase:

| Mode | How | Fit |
|---|---|---|
| **A. Utterance upload** | After the gate closes, the phone uploads the whole Opus clip (a 5 s utterance is about 15 kB, 30 s about 90 kB, far below any payload limit) as a few chunked, sequence-numbered mesh messages, or as a blob with a short-lived upload URL and a mesh message carrying the reference. The workflow starts when the last chunk lands | Simplest; P0. Adds the whole utterance duration to latency, because STT starts after the upload |
| **B. Chunked live upload** | The phone forwards 100 ms batches as they arrive, ordered, resumable, over a persistent stream (HTTP/2 or WebSocket to a streaming ingress). `utterance.open` is sent when the gate opens so the workflow is armed before the speech ends; the ingress feeds ElevenLabs realtime STT as chunks arrive | P1 target. Removes the utterance duration from the critical path |
| **C. Streamed reply** | Reply audio returns as ordered Opus chunks (SSE or Realtime), the first as soon as TTS yields audio | Always; P0 may start with whole-reply download |

**The ingress problem (the real design question).** The cloud plan stated "audio never passes through Vercel" for two reasons: the 4.5 MB request cap and no inbound WebSockets on functions. The owner's design now routes audio through the weaver side. A durable workflow is also not a low-latency pipe: it orchestrates steps, with queue dispatch time that the cloud plan lists as unverified. So the audio data plane and the workflow control plane must be separated [D]:

1. **Control plane (durable workflow)**: owns the turn: arm on `utterance.open`, wait for the end-of-speech event, run the CoS turn, shape, synthesize, hand reply chunks to the output, record the ledger entry, resume after a crash. The hot path (STT stream, model call, first TTS chunk) should run inside **one streaming step** so step-boundary latency is not paid per stage.
2. **Data plane (streaming ingress/egress)**: a small service that accepts the ordered chunk stream and forwards it into the workflow's STT step, and carries reply chunks back. Candidates, none verified: (i) many small POSTs (about ten per second, 300 B to 2 kB each) to a function route that appends to the workflow's hook, which works on functions but costs an invocation per chunk (about $0.02 per user-hour at the plan's invocation price, **[I]**) and adds per-request latency; (ii) Supabase Realtime or Storage as the chunk carrier; (iii) a small always-on streaming service next to the mesh hub, reachable over HTTPS/WebSocket, with the workflow subscribing to it. Recommendation: **P0 on mode A with chunked POSTs; P1 builds mode B on (iii) if (i) misses the latency budget**, decided by the P0 measurements.
3. **Mesh naming**: the workflow is `weftos://<mesh>/services/speech`, an ADR-120 exposure with `audience` = the user's own nodes. The phone addresses messages to that name. How a Rust-mesh message becomes an HTTPS call to a workflow is a **mesh-to-cloud gateway** that the cloud plan describes only for commands (`cos_node_commands`), not for streams. Whether the mesh itself (TCP plus Noise to a listener) is the transport, or the mesh envelope rides inside HTTPS, is [U] and an owner decision (Q16). Default: the mesh envelope rides inside HTTPS/WebSocket to a gateway, because a phone on cellular cannot assume a reachable mesh listener.

**Phone-side trade-offs the owner asked about:**

- **Upload size and bandwidth**: Opus at about 24 kbit/s is 3 kB/s up and about the same down; a day with 20 minutes of open gate is about 3.6 MB up. PCM would be 20 to 40 times more. Protocol overhead of chunked POSTs at ten per second is more than the payload (about 10 kB/s with headers over HTTP/2 **[I]**), so batch to 200 to 300 ms where latency allows, or use a persistent stream.
- **Radio and battery**: continuous streaming keeps the cellular radio in a high-power state for the length of a turn and the tail after it. Streaming only while the gate is open, with a pre-armed connection, is the lever. Wi-Fi hotspot use means the phone's data plan carries the audio.
- **Loss and ordering**: the phone buffers up to a bounded window (default 10 s of Opus, 30 kB) while the uplink is down; beyond that it drops with an `audio.gap` marker the workflow can speak about ("I lost you for a second"). Chunks carry `utt`, `seq`, `ts_ms`; the workflow reassembles and ignores duplicates (idempotent by `(utt, seq)`).
- **Resumability**: an interrupted utterance can resume by sequence number; a reply stream resumes from the last acknowledged `seq`.
- **Privacy**: audio now passes the server side. It is processed in memory and never stored by default; the ledger records durations, not content (section 11).
- **Offline**: with no uplink, the phone cannot do speech at all. The glasses play a cached "offline" cue (pushed once). When the phone can reach a LAN mesh member that exposes `speech.v1` (placement picks it), the same messages go there; that is the offline-capable path, and it needs no code change on the phone. An on-device recognizer or platform TTS on the phone is deliberately not built (Q6).

### 5.3 Speech and barge-in

The workflow produces `speech.v1` (plan 5.4) and renders it to Opus chunks; the phone forwards `speech.start/chunk/cancel` effects as `play.begin`, audio frames and `play.flush`. Barge-in has three parts, none on the phone: the glasses' AEC keeps the speaker out of the microphone path; the glasses' gate opening during playback sends `evt{mic.gate}` and starts `audio.up` immediately; the workflow decides the floor and issues `speech.cancel` with `spoken_through_seq` from the glasses' last playout report. Until AEC is proven on this hardware (G3), barge-in is by the push-to-talk gesture or by ducking the speaker while the gate is open.

### 5.4 Latency budget (glasses to glasses, no tool call)

The plan targets 0.9 s p50 / 1.5 s p90 from end of speech to first sound for the browser node. Here the cellular round trip appears twice, ElevenLabs is in a data center near the workflow, and the workflow adds step boundaries.

| Stage | p50 (ms) | p90 (ms) | Note |
|---|---|---|---|
| Glasses capture frame + Wi-Fi to phone | 25 | 60 | 20 ms frame |
| Phone relay (no decode) | 5 | 20 | forward only |
| Last chunk, phone to ingress (cellular, mode B) | 50 | 150 | earlier chunks overlap speech; mode A adds the whole utterance |
| Ingress to the STT step | 20 | 80 | **[U]** depends on the ingress choice |
| End-of-speech commit (STT turn prediction) | 150 | 300 | vendor figure from the plan |
| Cue tone from the glasses (parallel, not additive) | 40 | 100 | triggered by the glasses' own gate close; heard by about 200 ms |
| CoS first spoken clause (fast model, no tool) | 450 | 900 | plan's target class |
| TTS first audio (turbo/flash) | 100 | 250 | vendor "about 100 ms" |
| Workflow step boundaries on the hot path | 100 | 400 | **[U]** queue dispatch time unverified; target 0 with a single streaming step |
| First reply chunk, ingress to phone (cellular) | 60 | 160 | |
| Phone relay + Wi-Fi to glasses | 25 | 60 | |
| Jitter buffer + driver + playout | 100 | 160 | on the glasses |
| **Total** | **about 1,085** | **about 2,520** | mode B, hot path in many steps |
| Total with a single streaming step | about 985 | about 2,120 | the design target |

That is about 0.1 to 0.2 s worse than the browser node at p50. Mode A (utterance upload) adds the utterance length (typically 1 to 5 s) and is a spike-only budget. The P0 spike measures every row; the table is a target, not a result. With a tool turn, the plan's targets apply (cue tone 0.15 s, first chunk after a tool 1.8 s / 3 s, barge-in silence 120 / 250 ms).

---

## 6. Cross-platform strategy

Workload: a long-lived background service; a local TCP listener and client; BLE central (and maybe peripheral); audio I/O only on the glasses side (the phone moves Opus bytes and does no speech work); Noise, Ed25519, CBOR and certificate logic; a persistent chunked stream and HTTPS/SSE to the mesh gateway; a small UI (pair, status, switches, settings), with rich screens already existing as the web dashboard. The hard, OS-specific parts are the background execution model, BLE and permissions. No cross-platform UI toolkit removes those.

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

`weftos-node-core` (identity, enrollment, token exchange, mesh/`cos.v2` client state machine, command routing), `weftos-leaf-parent` (certificate chain, replay floor, framing, ACK), `weftos-glasses-proto` (`glasses.v1` frames and Opus relay), `weftos-relay` (chunking, resume, bounded buffers, bandwidth policy), `weftos-mobile-ffi` (the UniFFI surface). There is no speech crate on the phone. Targets: `aarch64-linux-android`, `aarch64-apple-ios`, `aarch64-apple-ios-sim`, host. The glasses agent reuses the same crates (arm64 Android) for its leaf end, plus Opus and the AEC. All builds through `scripts/build.sh` lanes; the script has no Android lane today (a search found none), so P0 adds one [D]. Binary size and avoiding the full kernel are [U] until the extraction is attempted.

### 6.4 The existing `clawft-android-edge` crate: is it the seed?

I read the crate, its guide (`docs/guides/android-splat-capture-edge.md`), ADR-077 and the companion app `apps/android-splat-capture` **[R]**.

**What it provides** (967 lines, six modules, `lib` + `cdylib` + `staticlib`, UniFFI 0.29 proc-macro surface behind a default `uniffi` feature, no dependency on the kernel):

- `identity`: a persisted Ed25519 node identity (`node_identity.json`, seed and public key as hex, mode 0600), `node_id = hex(SHA-256(pubkey)[..16])`, kept equal to the kernel's `node_id_from_pubkey` (ADR-103 D11), and `edge_sign` for signing with the node key. Seed handling uses `zeroize`.
- `pair_store`: a host URL plus a capability token, parsed from a QR or URL, saved to `host_pair.json`.
- `upload`: a blocking `reqwest` multipart upload of a capture ZIP to `splatd`, job status polling, and a health check.
- A UniFFI free-function facade (`edge_*`) for Kotlin.

**What the app side has**: a Gradle/Kotlin Compose project (compile SDK 35, min SDK 29, JDK 17) with a pairing screen, navigation, a `network_security_config`, session storage and a WebView screen. The Rust crate is **not yet packaged in it**: there is no NDK or `cargo-ndk` step in the Gradle file, the bindgen steps in the guide are commented examples, and the app falls back to a Kotlin `EdgeCore` whose node id is derived differently (an HMAC key handle) from the Rust one. `scripts/build.sh` has no Android lane. ADR-077's A4 step (QUIC/Noise session to the host) is marked later. I did not build it for Android this session [U: it may need target and linker setup].

**Assessment**

| Question | Answer |
|---|---|
| Right seed for the **Rust core**? | **Yes, as a seed, not as it stands.** It already settles the hard-to-reverse choices: Ed25519, the node-id derivation shared with the kernel, UniFFI 0.29 over a crate that builds as `cdylib` and `staticlib` (iOS-usable), no kernel dependency. It lacks everything that makes a node: Noise sessions, certificates, a persistent connection, async and streaming I/O, a mesh or cloud client, BLE, chunked relay. |
| Right seed for the **Android shell**? | **Partly.** The Gradle project, Compose navigation, pairing flow and WebView pattern are reusable as templates, but it is a camera and IMU capture app. Make a sibling app (`apps/android-phone-node`) for the phone node; do not grow the splat app into it. Keep the splat app as a second consumer of the same crate. |
| What to change in the crate | (1) Move `identity` and `pair_store` into `weftos-node-core` and have `clawft-android-edge` depend on it, so the splat app and the phone node share one identity and the Kotlin fallback can be deleted; (2) replace the blocking client with async, streaming and WebSocket/SSE; (3) swap the plain JSON seed for a Keystore-wrapped seed in P1 (the 0600 file is fine for the spike); (4) rename the UniFFI surface (it is not Android specific, and iOS will use it) or leave `clawft-android-edge` as the splat-specific facade; (5) keep the capability-token pair store for `splatd`, but do **not** reuse it for the cloud or glasses paths, which use signed challenges and short-lived tokens (no standing bearer, section 4.3). |
| Build targets and NDK setup it gives us | Only the crate types and the written (unexecuted) `cargo ndk -t arm64-v8a ... jniLibs` recipe. P0 must add the Rust Android targets, an NDK path, a `scripts/build.sh android` lane, and the Gradle step that copies the `.so` and generated bindings. |
| Does it carry node pieces? | Identity and signing only. Mesh membership, leaf-parent, relay and the glasses protocol are all new. |

Net: adopt it as the starting commit of `weftos-node-core`, not as the app.

---

## 7. Platform constraints that bite

### 7.1 Android (phone)

Verified against the current Android documentation:

- **Foreground service types** [V]: since Android 14 every foreground service declares a type in the manifest and in `startForeground()`. `microphone` needs `FOREGROUND_SERVICE_MICROPHONE` and `RECORD_AUDIO`, is subject to while-in-use rules, and cannot start from `BOOT_COMPLETED`. `connectedDevice` needs `FOREGROUND_SERVICE_CONNECTED_DEVICE` plus at least one of the Bluetooth, Wi-Fi-state, NFC, IR or UWB permissions (or a USB permission callback); its documented use is "Bluetooth, NFC, IR, USB, or network connections to external devices". `dataSync` is for uploads and downloads; on Android 15 it has a roughly six-hour timeout (with `Service.onTimeout`) and cannot start from boot **[V]** (Android 15 beta blog). Audio relay to the cloud looks like a data sync, but a six-hour cap and a boot restriction make it the wrong type for an always-on link: **do not use `dataSync` for the link**. Foreground service types must also be declared in the Play Console for apps targeting Android 14+ **[V]**.
- **Plan** [D]: the link-and-relay service is `connectedDevice` (it holds the Wi-Fi listener, the BLE link and the upstream stream). Because the phone does no speech, it never needs the `microphone` type except for an optional push-to-talk on the phone's own mic, a short service started from a visible activity or notification action. The persistent notification is honest ("Glasses connected, listening").
- **Background starts** [V]: apps targeting Android 12+ cannot start foreground services from the background except under listed exemptions; on Android 14+ services needing a while-in-use permission cannot be created from the background at all. A **Companion Device Manager** association is a listed exemption, and `CompanionDeviceService` with `REQUEST_COMPANION_START_FOREGROUND_SERVICES_FROM_BACKGROUND` is the documented way to wake an app when an associated device appears **[V]**. We associate the glasses through CDM at pairing, so a glasses reboot or return into range restarts the link service without opening the app. Disabling battery optimization is also an exemption **[V]**, requested at onboarding with an explanation.
- **Doze and battery** [I]: a running foreground service keeps network and CPU for its app; hold a partial wake lock only while a turn is active and a Wi-Fi lock while the link is up. Because the phone only relays Opus, CPU and thermals are light; the radio is the cost. OEM task killers are the real enemy; onboarding links to per-OEM instructions, and the app records unexpected restarts in diagnostics.
- **Hotspot** [I]: the user starts the hotspot in system settings; apps cannot configure the main hotspot, only a local-only hotspot with a random SSID. The app listens on all interfaces and reports which one the glasses came in on. Whether Doze or a vendor hotspot manager drops the hotspot with the screen off differs by device and is a P0 test. The glasses' audio uses the phone's data plan.
- **Permissions UX and Play policy**: nearby-devices and notification permissions, prominent disclosure, and Play's sensitive-permission declarations [I]. The glasses record bystanders; section 11 covers the indicator and consent stance.

### 7.2 iOS (phone)

Verified where marked:

- **Local Network privacy** [V] (TN3179): iOS 14+; the first local-network operation prompts; user control in Settings; no MDM override; no API to query the state (use `NWConnection`'s `waiting` state with unsatisfied reason `localNetworkDenied`, or Bonjour policy-denied errors). **Listening for and accepting incoming TCP and receiving UDP unicast do not require the permission; outgoing TCP, UDP unicast send, multicast, broadcast and all Bonjour operations do.** Multicast and broadcast also need the multicast entitlement; Bonjour browsing needs `NSBonjourServices`. A background app with an undetermined state is denied silently and the denial is not recorded; the prompt appears at the next foreground operation. So the glasses dial in (no prompt on the main path), the pairing screen performs one local-network operation in the foreground to prompt at a good moment, and the app handles `waiting` states.
- **Personal Hotspot** [V]: it must be enabled manually in Settings; an app cannot start or configure it (Apple DTS, "No"). Station-to-station forwarding works today but Apple describes it as behavior, not a contract **[V]**. A forum answer I could not verify says IPv4 client-to-client traffic changed in iOS 18 and IPv6 link-local is the dependable path **[U]**; the P2 spike tests a real iPhone hotspot with a real glasses unit and keeps an IPv6 link-local option.
- **Background execution** [V for the mechanism, I for the strategy]: `UIBackgroundModes` `audio` keeps an app running only while it is playing or recording audio; the iPhone in this design does neither (the audio is bytes, not sound), so `audio` mode does not legitimately apply. `bluetooth-central` allows discovery, connection and delegate callbacks in the background, with limited scanning and about ten seconds of processing per event, and state preservation and restoration after termination. A held TCP socket does not keep an app alive; App Review limits background services to their intended purposes (2.5.4) and requires consent and an indication when recording (2.5.14) **[V]**.
- **What the thin design changes**: the earlier plan needed the iPhone to hold an ElevenLabs socket for the whole turn. Now the iPhone holds only the upstream and downstream byte streams to the mesh gateway and the local glasses link, a smaller and more defensible workload. But the keep-alive problem is the same and sharper: the relay must run for the whole turn (seconds to tens of seconds), upstream and downstream, with the app otherwise suspended. The only legitimate wake path is the **BLE doorbell**: the glasses write a characteristic ("utterance starting"), iOS wakes the app via `bluetooth-central`, the app opens a background task for the turn, tells the glasses over BLE that it is ready, and the glasses dial the TCP session. Whether wake-to-ready fits the budget (add 200 to 600 ms) and whether iOS honors the pattern after hours of suspension are P2's central questions. `URLSession` background transfers are discretionary and unsuitable for live turns [I]. Do **not** use a silent audio loop, a fake VoIP registration or a location mode to stay awake; these are the standard reasons for rejection under 2.5.4 [I].
- **App Store risk, honestly** [I]: an always-listening wearable companion raises privacy review (2.5.14, data collection disclosures); BLE and Wi-Fi accessories are common; the background strategy is legitimate (Bluetooth accessory communication) but must be explained in review notes. TestFlight and ad-hoc remain available while the owner decides on public release (Q9).

---|---|---|
| App suspended or killed overnight | OEM task killer; FGS stops; CDM presence relaunches | Suspended; BLE restoration relaunches |
| Glasses cannot reach the phone | Hotspot off; AP client isolation | Hotspot off; IPv4 client-to-client change |
| First connection silently blocked | Not applicable | Local Network denied or undetermined in background |
| Mic returns silence on glasses | G0 failure (section 3.4) | Same (glasses side) |
| Reply cut off | Doze network; jitter buffer underrun | Background task expired mid-turn |

---

## 8. Integration

### 8.1 Cloud CoS and `weftos-weaver`

- The phone node enrolls as a surface (plan 4.3): keypair, dashboard approval, short-lived node tokens. Commands from the cloud arrive over the plan's transports and are translated: `speak{utt_id,...}` is no longer a text-to-speech request to the phone but a **workflow-rendered audio stream** the phone relays as `audio.down` and `play.*`; `show{scene|glance}` becomes a notification or phone screen (the glasses have no display); `capture{mode,max_s}` becomes `capture.start`; `stop` becomes `play.flush` and `capture.stop`; `probe{kind}` becomes a bounded `probe` on the glasses; `status{}` returns merged phone plus glasses status. The glasses are addressable as their own node so `devices.status{node}` can answer "is the glasses mic working?" from the agent's real mic state and last-gate time **[R]**.
- **Speech is a workflow, not a surface feature**: a captured utterance starts or feeds a per-user speech workflow run (section 5), and the CoS turn runs inside it. The plan's separate steps (`cos_turn_state`, ledger, lease) apply; the lease already serializes two surfaces speaking at once.
- **Transport split**: control and text by Realtime/SSE/long-poll; audio by the dedicated streaming leg (section 5.2); glasses audio on the local link. The legacy bridge (`/api/nodes/cos/sync`, `pending?wait=25`) stays valid during the transition so P0 does not wait for the new command channel.
- **Relay versus direct**: the phone always talks to the mesh/cloud gateway (relay model) and to the glasses directly. The cloud never reaches the glasses by itself, and the glasses never open an internet connection of their own [D].
- **Mesh**: the phone node announces itself on the mesh when a path exists and receives mesh traffic addressed to `weftos://<mesh>/nodes/<phone>`. Phone-only users get the dashboard-minted workspace mesh authority (plan Q3) until a real mesh pairs.
- **Parent relation in names**: the glasses node record carries `parent=<phone node id>` in its facts. Whether ADR-114 needs a nested form is [U] and goes to the ADR owner.

### 8.2 The `/phone` dashboard page

Three roles, no conflict [D]:

1. **Desktop**: the existing page stays, but its Mentra tab reads the **glasses node's status rows** instead of the Mac-written `listen.status.json` and `speak.status.json` (`docs/plans/cos-dashboard-bridge.md`). The tab's push-to-talk and listening toggle become commands to the glasses node (`capture.start`, listening on/off) through the same `cos_node_commands` the plan defines, instead of `cos_requests` with `kind` handled by the Mac bridge.
2. **In the app**: the page loads in a WebView with the app's signed-in session for rich screens (history, settings, catalog), and a small JavaScript bridge exposes native features the page lacks: foreground push-to-talk from the phone's own mic, link status, and the local switches. The page remains the single place where UI features land first.
3. **Phone-first users**: the app's home screen is native and minimal (connection state, a mic toggle, the last turn), so the always-on part never depends on a web page loading.

### 8.3 The phone's own sensors as node capabilities

Offered as ADR-120 exposures, default off, with a phone-screen switch and battery-aware sampling [D]: coarse location, activity and steps, barometer, ambient light, battery, Wi-Fi and Bluetooth scan RSSI, camera still, microphone (push-to-talk). Announced as `sensor.phone.*` and `surface.*`. No CSI on a phone: neither platform exposes it through public APIs as far as I know **[I]**.

---

## 9. Phased build plan

### P0: spike (Android, 2 to 3 weeks)

**Goal**: with the Mac off and the lab network absent, say a sentence to the glasses and hear the CoS answer in them, using only the phone as a relay.

Scope:

- (a) **Glasses agent APK v0**: `AudioRecord`, energy gate with pre-roll, Opus encode (`MediaCodec`), socket to the phone; socket to Opus decode to `AudioTrack`; provisioned over USB.
- (b) **Phone app v0** (Kotlin Compose shell in a new `apps/android-phone-node`, Rust core seeded from `clawft-android-edge`, section 6.4): one `connectedDevice` foreground service, a TCP listener, a chunked upload client and a reply stream client. **No STT, TTS, VAD, AEC or ElevenLabs code or key on the phone.**
- (c) **Speech workflow stub** (server side, a dependency owned by the cloud-plan lane unless the owner assigns it here): accepts the chunked Opus utterance (mode A), runs ElevenLabs STT, posts the text through the existing CoS relay path (`cos_requests` and the sync route), waits for the reply text, runs ElevenLabs TTS to Opus, and returns it as ordered chunks (mode C). It runs as a durable workflow run if Vercel Workflow is ready, otherwise as a function pair; either way the phone's contract is the same mesh messages.
- (d) **Pairing** by the speak-the-code flow over BLE if G2 holds, else a USB-provisioned key exchange.
- (e) **Measurement harness**: timestamps at every row of the section 5.4 table, logged on the phone and the workflow.

Gates and acceptance tests:

| # | Test | Pass |
|---|---|---|
| G0 | Mic after foreground launch, glasses idle | Non-silent audio 30 min after last foreground launch, process never visible; also record the result after a Wi-Fi change and a glasses reboot (a fail is a result) |
| G1 | Speaker streaming without adb | A 10 s Opus stream decodes and plays; first frame to sound under 150 ms; `flush` stops within 100 ms |
| G2 | Coexistence | Our BLE advertise/connect works while the stock client is present; concurrent mic does not silence either; camera question answered |
| G-LED | RGB LED over the serial path | While `asg_client` runs, the claim, `cs_ledon`, `cs_ledoff`, release sequence changes the LED as expected by eye; field order confirmed; failure means tone-only cues and `led:"unavailable"` |
| G3 | AEC feasibility (measured, may fail) | Reply played at normal volume leaves under a set residual in the mic path with the platform echo canceller or the agent's AEC |
| A1 | End-to-end loop | Glasses on the phone hotspot, Mac powered down, say "what is on my board" and hear the answer; p50 end-of-speech to first sound at most 4 s over 20 trials (spike budget for mode A; the 1.0 s target is P1) |
| A2 | Soak | 30 min of intermittent speech, no drop, no crash, no memory growth |
| A3 | Screen-off | Phone locked 10 min, then a turn works, with battery optimization allow-listed |
| A4 | Reconnect | Toggle the hotspot off for 30 s; the link re-forms in under 15 s without touching the app |
| A5 | Wrong peer | A second device or wrong key is refused and logged |
| A6 | No Mentra, no speech vendor on the phone | Traffic inspection shows the phone and glasses talk only to each other and to the mesh gateway; none to Mentra, none to ElevenLabs |
| X1 | On-phone STT (experiment, owner 2026-10-13) | sherpa-onnx Parakeet (the same model family the Mac CoS uses) runs in the app as a `speech.v1` STT provider on the S24 Ultra: transcript of a 5 s utterance in under 700 ms, WER no worse than the cloud path on the owner's 20 recorded test phrases, under 5 % battery per hour of open-gate use, and placement can switch between phone and cloud STT per turn. Informs whether offline and low-latency STT moves to the phone. |
| B1 | Bandwidth | A 5 s utterance and a 10 s reply each cost at most 25 kB over the cellular leg including headers; a day's budget extrapolated |
| B2 | Uplink loss | Drop the phone's uplink for 5 s mid-utterance: the stream resumes by sequence or the workflow receives an `audio.gap` and answers sensibly; no crash, no duplicate speech |

Risks: G0/G1/G2 failing (ask asg_client for help, or an owner decision on privileges); hotspot vendor quirks; the ingress choice (section 5.2) missing the budget; workflow dispatch latency higher than assumed. **Owner decisions**: Q2 (reference phone), Q7 (updates), Q16 (mesh gateway shape), Q17 (who builds the workflow stub).

### P1: production Android (6 to 10 weeks after P0 passes)

Scope: the Rust core extracted (`weftos-node-core`, `weftos-leaf-parent`, `weftos-glasses-proto`, `weftos-relay`) with UniFFI, built from the `clawft-android-edge` seed; an Android build lane in `scripts/build.sh`; glasses agent as a signed release APK with a recovery path; the WLF1 certificate flow, enrollment in the dashboard, revocation; mode B live chunk upload with `utterance.open` pre-arming and the chosen streaming ingress; streamed reply (mode C) with `speech.v1` including offers, cancel and barge-in; the glasses' AEC; BLE fallback; CDM presence; bandwidth policy and bounded buffers; mesh placement so a LAN `speech.v1` service can answer when reachable; honest notifications; diagnostics; the dashboard WebView with the JS bridge; camera stills and `probe`; Play internal then closed testing.

Acceptance: the plan's M3/M4 criteria for a phone and a glasses surface; first-sound p50 at most 1.2 s and p90 at most 2.5 s without a tool, on the hotspot, over cellular uplink; a ten-hour overnight soak with no manual restart (the link service survives at least one forced kill via CDM relaunch); revocation takes effect on the next connection and the glasses cannot rejoin; switching the glasses mic exposure off locally refuses the next `capture`; a log of every use without bodies; two users on two phones see only their own devices and workflows; no audio stored by default; the phone APK still contains no STT/TTS code.

Risks: glasses OTA changes the environment; Play review of a foreground listener; the `weftos-leaf-parent` extraction is larger than expected; glasses battery; the workflow's concurrency and ElevenLabs limits at scale. **Owner decisions**: Q3 (Play vs sideload), Q4 (camera), Q8 (iOS timing), Q16, Q18 (cost caps).

### P2: iOS (4 to 8 weeks, after P1 stabilizes)

Scope: Swift/SwiftUI shell over the same UniFFI core; Core Bluetooth central with state restoration; the doorbell protocol on the glasses side; listener-based local networking; Local Network prompt at pairing; TestFlight; a hotspot test matrix (iPhone as the AP, and shared Wi-Fi); an IPv6 link-local path; App Review notes.

Acceptance: a full turn with the iPhone locked and the app suspended, woken by the doorbell, with the added wake-to-ready time measured and within an agreed bound (target at most 600 ms p50); the Local Network denied state is detected and explained; killing the app from the switcher and relaunching via BLE restoration works (or the failure is documented and the feature is marked foreground-only); a TestFlight cohort of at least three people for two weeks.

Risks: iOS may not allow the always-available behavior at all (decision: foreground-only or "wake by gesture"); hotspot IPv4/IPv6 changes; App Review. **Owner decisions**: Q8, Q9, Q10.

### P3: CSI research (parallel, research only)

Plan, tiers and acceptance live in `~/dev/mentra/docs/MentraGlasses_RuView.md` (sections 8 and 11). Here: read-only module and firmware analysis first, T0 with the phone as traffic source, a T4 bench with one ESP32-C5 receiver, and ICAP (T1) only on a sacrificial unit over USB with the owner's approval. Owner decisions: Q11, Q12, Q19.

---

## 10. CSI from the glasses' Wi-Fi

Moved by the owner's direction (2026-10-13). The CSI research lives in `~/dev/mentra/docs/MentraGlasses_RuView.md` (measured radio facts, the ICAP findings, tooling by chip family, tiers T0 to T5 with ICAP as the leading on-device candidate, firmware risk, the `adb root` path, the ESP32-C5 RuView receiver alternative, sources and next read-only steps; see its section 11). Short version: no CSI symbol exists in the glasses' Wi-Fi driver, raw I/Q capture (ICAP) is compiled in but time-shares the radio with networking and needs `adb root`, so it is a research tool only. This design uses it in P3 and nowhere else.
---

## 11. Privacy, safety and policy

- The glasses record other people. The design keeps an indicator and a switch on every capture path: the cue tone when the gate opens or listening starts, the RGB LED `listening` state whenever the gate is open (with the tone as the fallback), and a persistent phone notification. Nothing is stored by default: audio is processed in memory; transcripts go to the user's own CoS data; the TTS cache is stock phrases only.
- Local control wins (ADR-120): the phone UI switch refuses the next command; a hardware gesture on the glasses if the agent can read one [U].
- No bearer on the glasses, no long-lived secret on the phone beyond the wrapped seed, short-lived node tokens; the speech vendor key stays on the server.
- Third-party processing: the speech workflow sends audio to ElevenLabs when the cloud path is active. Audio is processed in memory and not stored by default; the ledger records durations, not content. The settings show the active path (cloud or a LAN speech service chosen by placement). A strictly local mode is possible only when a LAN `speech.v1` service is reachable.
- No hostnames, tailnet addresses, tokens or client names belong in this repository's docs; the implementation keeps them in secrets and per-user config.

---

## 12. Open questions, with defaults

| # | Question | Default |
|---|---|---|
| Q1 | Does the P0 spike need any speech credential on the phone? | No. Never on the phone, at any phase |
| Q2 | Which Android phone and OS version is the reference device? | **Decided 2026-10-13 (owner):** the owner's daily Samsung Galaxy S24 Ultra only. No second test device. |
| Q3 | Distribute through Play (internal then closed testing) or sideload only? | Play internal testing in P1; public decision later |
| Q4 | Is the camera in scope for P1 (stills on command)? | Stills yes, clips and streaming later |
| Q5 | Cached stock phrases: server side per user, pushed to the glasses? | Yes; the phone caches nothing |
| Q6 | Should the phone ever do speech on-device (offline mode)? | **Revised 2026-10-13 (owner): worth testing.** An on-phone STT service (sherpa-onnx/Parakeet on the S24 Ultra) hosted by the app as a `speech.v1` STT provider that placement can choose. The workflow still decides; the phone still does no TTS or shaping. Tested in P0 as experiment X1. |
| Q7 | How are glasses agent updates delivered? | USB or a phone-started Wi-Fi adb session the user approves each time; no standing adb |
| Q8 | When does iOS start? | After P1 acceptance; run a one-week iOS feasibility spike (BLE doorbell and hotspot only) during P1 |
| Q9 | Public App Store release, or TestFlight only? | TestFlight until the background behavior is proven |
| Q10 | If iOS cannot be always-on, is "foreground or wake-by-gesture" acceptable? | Yes, documented as a platform limit |
| Q11 | May we buy a second glasses unit for firmware risk? | **Decided 2026-10-13 (owner):** there is only one glasses unit, so no firmware work on it. Firmware-risk items wait until a second unit exists. |
| Q12 | CSI priority against P1/P2? | **Decided 2026-10-13 (owner):** CSI is delegated to the RuView work for later (see ~/dev/mentra/docs/MentraGlasses_RuView.md). Not part of P0–P2. |
| Q13 | Where does the leaf-parent code live? | A new small crate, adopted by the kernel |
| Q14 | Parent relation in ADR-114 names or in node facts? | Node facts for now; ADR owner decides |
| Q15 | Does the agent accept a hardware gesture for mute? | Yes if the stock client exposes the event; otherwise no |
| Q16 | How does a mesh message reach the speech workflow from a phone on cellular? | The mesh envelope rides inside HTTPS/WebSocket to a gateway; the rest of the mesh is an optional extra path |
| Q17 | Who builds the P0 speech workflow stub? | The cloud-plan (M2/M3) lane, with the P0 team consuming it; if it is not ready, a function pair for the spike |
| Q18 | Per-user caps on open-gate seconds and speech spend? | Yes, enforced in the workflow ledger and by the glasses' gate cap |
| Q19 | T4 receiver placement and count? | One ESP32-C5 near the usual AP or phone position for the bench |
| Q20 | The cloud plan said audio never passes through Vercel; does the owner accept an audio ingress outside Vercel (or a revised statement)? | **Decided 2026-10-13 (owner):** yes, a small streaming audio ingress outside Vercel, with the durable workflow as control plane. Exception: anything that must go through the AI Gateway goes through Vercel, i.e. our API on Vercel. Long term we take control of the whole speech path (self-hosted); ElevenLabs is fine for now. |
| Q21 | May the LED be the mic-open indicator, with the semantics in section 3.7 after an eyes-on check? | Yes |
| Q22 | Does the `done_ok` chime play after a spoken reply? | Yes by default; `cue_done_after_speech=false` drops it |
| Q23 | Thinking timeout before `done_error`? | 20 s, extendable to 60 s by the workflow |

## 13. Sources

Verified this session (primary):
- Android foreground service types and requirements: https://developer.android.com/develop/background-work/services/fgs/service-types
- Android restrictions on starting foreground services from the background, including the while-in-use rules and exemptions: https://developer.android.com/develop/background-work/services/fgs/restrictions-bg-start
- Android BLE in the background, `CompanionDeviceManager` presence and `CompanionDeviceService` permissions: https://developer.android.com/develop/connectivity/bluetooth/ble/background
- Android 15 `dataSync` and `mediaProcessing` roughly six-hour timeout (beta announcement): https://android-developers.googleblog.com/2024/05/the-second-beta-of-android-15.html
- Apple TN3179, Understanding local network privacy: https://developer.apple.com/documentation/technotes/tn3179-understanding-local-network-privacy
- Apple Core Bluetooth background processing (bluetooth-central, scanning limits, state restoration): https://developer.apple.com/library/archive/documentation/NetworkingInternetWeb/Conceptual/CoreBluetooth_concepts/CoreBluetoothBackgroundProcessingForIOSApps/PerformingTasksWhileYourAppIsInTheBackground.html
- Apple App Review Guidelines (2.5.1, 2.5.4, 2.5.14): https://developer.apple.com/app-store/review/guidelines/
- Apple Developer Forums, Personal Hotspot cannot be enabled by an app: https://developer.apple.com/forums/thread/802003
- Apple Developer Forums, Personal Hotspot STA-to-STA forwarding is not a contract: https://developer.apple.com/forums/thread/699414
- UniFFI (Kotlin, Swift and Python full support): https://mozilla.github.io/uniffi-rs/latest/

Secondary only ([I]):
- iOS `audio` background mode keeps an app running only while recording or playing (Apple forum posts surfaced by search): https://developer.apple.com/forums/thread/106415

Repo and workspace material read: `docs/plans/cos-cloud-migration.md` (sections 4, 5, 8), `docs/plans/cos-dashboard-bridge.md`, `docs/leaf-push-protocol.md`, `docs/plans/adr-103-leaf-implementation.md`, `docs/adr/adr-120-mesh-resources-and-exposures.md`, `firmware/mentra-mic/` (README and manifest), the AudioBridge source and the Mentra Live device profile in the glasses workspace, and the `mentra-leaf` bring-up source.

Also read for revisions 2 and 3: `crates/clawft-android-edge/` (all six modules), `docs/guides/android-splat-capture-edge.md`, `docs/adr/adr-077-android-splat-capture-edge-node.md`, `apps/android-splat-capture/` (Gradle files, `EdgeCore.kt`), and the glasses probe results relayed by the lead.
