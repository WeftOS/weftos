# Voice Guide

This guide covers the voice pipeline in clawft: speech-to-text (STT) via the
browser, cloud text-to-speech (TTS) through server-side proxying, and the
talk mode overlay for hands-free continuous conversation.

---

## 1. Overview

Voice features enable natural speech interaction with your weft agent. The
pipeline splits work between the browser and the server:

- **Speech-to-text (STT)** runs entirely in the browser via the Web Speech
  API. No server-side transcription service is required.
- **Text-to-speech (TTS)** providers (WEFT-238):
  - **`local`** (default) — offline-first; matches the local Piper model
    default and `VoicePersonality.provider`. Not synthesized by the cloud
    HTTP proxy.
  - **`browser`** — UI-only Web Speech API (`speechSynthesis` in
    `clawft-ui`); not a server-side synthesizer.
  - **`openai`** / **`elevenlabs`** — cloud neural voices, proxied through
    `/api/voice/tts` so API keys stay server-side.
- **Talk mode** provides a full-screen overlay with waveform visualization,
  continuous listening, and tap-to-interrupt, enabling hands-free
  conversation with the agent.

---

## 2. Enabling Voice

Voice requires two configuration flags:

```json
{
  "voice": {
    "enabled": true
  },
  "gateway": {
    "api_enabled": true
  }
}
```

| Field | Purpose |
|-------|---------|
| `voice.enabled` | Activates the voice pipeline. Default: `false`. |
| `gateway.api_enabled` | Starts the HTTP/WebSocket server that the web dashboard connects to. Voice runs through the dashboard, so this must be `true`. |

The voice pipeline is browser-side for STT and server-side for cloud TTS
proxying. When both flags are enabled, the web dashboard exposes the voice
panel and talk mode controls.

### 2.1 Transport mode (ADR-074 / WEFT-689)

Talk-Mode and agent conductor demos can use **xAI Grok Voice** as the interim
primary cloud speech-to-speech path, with the local ADR-061 stack as offline
fallback.

```toml
[voice]
enabled = true
# local | xai_s2s | xai_hybrid
# Product default: xai_s2s (fails open to local without key / on health fail)
mode = "xai_s2s"

[voice.xai]
# empty → inherit XAI_API_KEY / providers.xai
api_key_env = "XAI_API_KEY"
realtime_model = "grok-voice-latest"
endpoint = "wss://api.x.ai/v1/realtime"
```

| Field | Default | Meaning |
|-------|---------|---------|
| `voice.mode` | `xai_s2s` | Transport: `local` always local; `xai_s2s` full Realtime S2S; `xai_hybrid` hybrid STT/TTS |
| `voice.xai.api_key_env` | `XAI_API_KEY` | Env var name holding the key (key never stored in config / never logged) |
| `voice.xai.realtime_model` | `grok-voice-latest` | Realtime model alias |
| `voice.xai.endpoint` | `wss://api.x.ai/v1/realtime` | WebSocket base URL |

**Selection rule** (`resolve_voice_transport` in `clawft-types`):

1. `mode = local` → local only.
2. Else if no API key / health check fails → **fail open to local** with a
   single user-visible notice (not silent forever).
3. Else use the xAI path.
4. **Runtime** (connect/session error) → fail open to local via
   `resolve_voice_transport_runtime_fallback` /
   `connect_hello_or_fallback` (never leave Talk-Mode without a path).

**Hybrid composition** (`mode = xai_hybrid`, when key + healthy):

| Leg | Backend |
|-----|---------|
| STT | Local (ADR-061) |
| Agent | Local Hermes / configured LLM |
| TTS | xAI |

When hybrid fails open (no key / unhealthy / runtime), **all three legs** become local.

**Health probe (V2 stub):** `probe_xai_voice_health` / `XaiHealthProbeMode` in
`clawft-channels` (`voice-xai`). Default is a pure stub (no network) so CI stays
deterministic; inject `Stub { healthy: false }` to force failover in tests.
Wire a real pre-dial probe later without changing the selection rule.

**Metrics (V2):** `VoiceTransportMetrics` in `clawft-types` — atomic counters for
transport selection (`select_local` / `select_xai_s2s` / `select_xai_hybrid`),
fallback reasons (`fallback_no_key` / `fallback_unhealthy` / `fallback_runtime`),
connect ok/fail, disconnects, and TTFA samples. Use
`resolve_voice_transport_with_metrics` or `select_voice_transport(..., Some(&metrics))`.

**Feature flag:** compile the Realtime client with
`clawft-channels` feature `voice-xai` (implies `voice`).

| Phase | Scope |
|-------|--------|
| **V0** (WEFT-689) | Connect + `session.update` hello |
| **V1** (WEFT-690) | Tool bridge: `spawn_agent` / `focus_pane` / `summarize` → `WindowIntent`; optional governed `shell_intent` (not registered by default) |
| **V2** (WEFT-691) | Hybrid composition; health probe stub; metrics; graceful local fallback on connect failure |

**AEC requirement (cloud S2S):** keep native AEC (`clawft-voice-aec`) **in front of the mic** before uplink to Realtime — otherwise the model hears itself. V1 documents this (`AEC_IN_FRONT_OF_MIC_NOTE`); live capture wiring may still be passthrough until device/AEC features are on.

```bash
# Unit tests (no network) — prefer scripts/build.sh in this repo
scripts/build.sh test -p clawft-types voice_
# or focused:
cargo test -p clawft-types voice_
cargo test -p clawft-types voice_metrics
cargo test -p clawft-channels --features voice-xai xai_realtime
cargo test -p clawft-channels --features voice-xai xai_tool_bridge
cargo test -p clawft-channels --features voice-xai xai_health

# Manual live hello (requires network + key)
XAI_API_KEY=... cargo test -p clawft-channels --features voice-xai \
  xai_realtime_live_hello -- --ignored --nocapture
```

Privacy: in `xai_s2s` / hybrid modes audio leaves the machine. Enterprise
profiles may force `mode = "local"`. Never log API keys or raw audio.

---

## 3. TTS Provider Configuration

The `voice.tts` section selects which text-to-speech engine produces audio.
Only one provider is active at a time.

| Provider | Default? | Where it runs | Cloud proxy `/api/voice/tts` |
|----------|----------|---------------|------------------------------|
| `local` | **yes** | Offline / native talk pipeline | **400** clear error (not cloud) |
| `local-stub` | no | Alias for local / stub engines | **400** (same as local) |
| `browser` | no | Browser Web Speech (`clawft-ui`) | **400** — use Web Speech directly |
| `openai` | no | Server → OpenAI | audio/mpeg when key present |
| `elevenlabs` | no | Server → ElevenLabs | audio/mpeg when key present |

**WEFT-238 decision:** the product default is **`local`**, not `browser`.
The old default pointed at a server path that never implemented Web Speech
dispatch. Browser synthesis is **client-side only** and already wired in
`clawft-ui` `speak()` when the provider is `local`, `local-stub`, or
`browser` (cloud proxy is only called for `openai` / `elevenlabs`).

### 3.1 Local (Default)

Offline-first provider. Aligns with the default Piper model name
(`vits-piper-en_US-amy-medium`) and with `VoicePersonality.provider =
"local"`. No cloud API key is required.

```json
{
  "voice": {
    "tts": {
      "provider": "local",
      "model": "vits-piper-en_US-amy-medium"
    }
  }
}
```

In the web dashboard, `local` falls through to the browser Web Speech API
for playback (zero-config UX). Native talk mode uses the local ADR-061 /
`clawft-voice-tts` path when models are installed. The HTTP cloud proxy
does **not** synthesize `local` — it returns HTTP 400 with guidance to use
native talk or switch to a cloud provider.

Alias: `"local-stub"` is accepted as local (stub / empty-audio engines).

### 3.1.1 Native dual-layer fast tier (WEFT-613)

Native Talk-Mode uses a dual-layer TTS (ADR-061): **fast** ack + **slow**
expressive answer (Orpheus over Ollama → SNAC).

| Fast engine | Status (0.8) | Notes |
|-------------|--------------|--------|
| **Kokoro** (ONNX preset) | **Default** | Works when model staged under `.weftos/models/kokoro/` |
| **Chatterbox** (cloned voice) | Scaffold only | Bundle probe + selection; **no** native inference yet |

```toml
[voice.tts]
provider = "local"
# auto | kokoro | chatterbox  (env: WEFTOS_FAST_TTS)
fast_engine = "auto"
```

- `auto` (default, david-profile intent): use Chatterbox when
  `ChatterboxTts::is_runtime_available()`; otherwise Kokoro.
- In 0.8, clone inference is gated off (`CHATTERBOX_INFERENCE_IMPLEMENTED =
  false`), so `auto` always falls through to Kokoro — accepted **timbre split**
  vs Orpheus until 0.9.x.
- Optional staging (does not enable audio by itself):
  `.weftos/models/chatterbox/{chatterbox.onnx,reference.wav}` or
  `WEFTOS_CHATTERBOX_DIR`.

See `docs/plans/wave-ws10-WEFT-613-result.md`.

### 3.2 Browser (UI Web Speech opt-in)

Uses the Web Speech API built into the browser. No API key is needed.
Opt in only when you want to force the client path explicitly.

```json
{
  "voice": {
    "tts": {
      "provider": "browser"
    }
  }
}
```

Quality is functional but robotic. Works in browsers that implement
`SpeechSynthesis` (Chromium, Safari; Firefox support varies). There is
**no** server-side browser dispatch — `POST /api/voice/tts` returns 400
if this provider is selected.

### 3.3 OpenAI TTS

High-quality neural voices from OpenAI.

```json
{
  "voice": {
    "tts": {
      "provider": "openai",
      "model": "tts-1",
      "voice": "alloy",
      "speed": 1.0
    }
  }
}
```

**Models:**

| Model | Description |
|-------|-------------|
| `tts-1` | Fast, lower latency, cheaper. Good for real-time conversation. |
| `tts-1-hd` | Higher quality audio. Better for pre-recorded or polished output. |

**Voices:**

| Voice | Character |
|-------|-----------|
| `alloy` | Neutral, balanced |
| `echo` | Warm, conversational |
| `fable` | Expressive, storytelling |
| `onyx` | Deep, authoritative |
| `nova` | Friendly, upbeat |
| `shimmer` | Soft, gentle |

**Speed:** Accepts values from `0.25` to `4.0`. Default is `1.0`.

**API key:** Set `providers.openai.apiKey` in the config file, or set the
`OPENAI_API_KEY` environment variable. The config value takes precedence
when both are present.

### 3.4 ElevenLabs TTS

Highest quality and most natural sounding voices.

```json
{
  "voice": {
    "tts": {
      "provider": "elevenlabs",
      "model": "eleven_multilingual_v2",
      "voice": "Rachel"
    }
  }
}
```

**Models:**

| Model | Description |
|-------|-------------|
| `eleven_multilingual_v2` | Best quality, supports 29 languages. |
| `eleven_turbo_v2_5` | Low latency, optimized for real-time use. |
| `eleven_monolingual_v1` | English-only, legacy model. |

**Voices:** Use any voice ID from your ElevenLabs account, or one of the
preset names:

| Preset Name | Character |
|-------------|-----------|
| `Rachel` | Calm, narration |
| `Domi` | Strong, assertive |
| `Bella` | Soft, warm |
| `Antoni` | Well-rounded, male |
| `Josh` | Deep, young male |
| `Adam` | Clear, middle-aged male |
| `Sam` | Raspy, male |

Custom voice IDs from the ElevenLabs voice library or voice cloning are
also supported. Pass the voice ID string directly in the `voice` field.

**API key:** Set `providers.elevenlabs.apiKey` in the config file, or set
the `ELEVENLABS_API_KEY` environment variable. The config value takes
precedence when both are present.

---

## 4. Environment Variables

| Variable | Provider | Description |
|----------|----------|-------------|
| `OPENAI_API_KEY` | OpenAI TTS | Falls back to this when `providers.openai.apiKey` is empty. |
| `ELEVENLABS_API_KEY` | ElevenLabs | Falls back to this when `providers.elevenlabs.apiKey` is empty. |

The config value always takes precedence over the environment variable.
When neither is set and a cloud provider is selected, TTS requests will
fail with an authentication error.

---

## 5. Full Configuration Example

A complete `config.json` snippet with voice, provider keys, and gateway:

```json
{
  "voice": {
    "enabled": true,
    "tts": {
      "provider": "openai",
      "model": "tts-1",
      "voice": "nova",
      "speed": 1.0
    }
  },

  "providers": {
    "openai": {
      "apiKey": "sk-..."
    },
    "elevenlabs": {
      "apiKey": "xi-..."
    }
  },

  "gateway": {
    "host": "127.0.0.1",
    "port": 18790,
    "api_enabled": true
  }
}
```

To switch providers, set `voice.tts.provider` to `"openai"`,
`"elevenlabs"`, `"browser"`, or keep the default `"local"`. The
provider-specific fields (`model`, `voice`, `speed`) are only read for
the active provider.

---

## 6. Talk Mode

Talk mode is a full-screen overlay activated from the voice panel in the
web dashboard. It provides hands-free, continuous conversation with the
agent.

### States

The overlay cycles through four states:

| State | Indicator | Behavior |
|-------|-----------|----------|
| **Idle** | Pulsing circle | Waiting for the user to begin speaking. |
| **Listening** | Waveform animation | Capturing speech via Web Speech API. |
| **Processing** | Spinner | Transcript sent to agent, awaiting response. |
| **Speaking** | Waveform playback | TTS audio playing through speakers. |

### Interaction

- **Start listening:** Tap the center icon or begin speaking (continuous
  recognition auto-activates after the agent finishes speaking).
- **Interrupt:** Tap the center icon during the speaking state to stop
  playback and return to the listening state.
- **Exit:** Tap the close button or press Escape to leave talk mode.

Speech recognition runs continuously between responses. After the agent
finishes speaking, the microphone re-engages automatically so the user can
respond without tapping anything.

---

## 7. Voice Mode Prompt

When a message arrives from the voice channel (`chat_id="voice"`), a
system prompt is injected that instructs the LLM to respond in natural
conversational language:

- No Markdown formatting (no headers, bold, lists, or code blocks).
- No URLs or links in the response text.
- Uses contractions and casual phrasing.
- Keeps answers brief and to the point.

The response text is also stripped of any remaining Markdown artifacts
before it is sent to the TTS engine. This prevents the TTS from reading
out formatting characters like asterisks or hash marks.

If voice responses are unexpectedly verbose or formatted, verify that the
inbound message has `chat_id` set to `"voice"` so the voice system prompt
is applied.

---

## 8. Architecture

The voice pipeline has six stages:

```
Browser                              Server
------                              ------

1. Microphone
   |
   v
2. Web Speech API (STT)
   |  transcript text
   v
3. POST /api/sessions/voice/messages -----> Agent pipeline
                                             (6-stage processing)
                                                  |
                                                  v
4. WebSocket broadcast <---------------------- Response text
   |
   v
5. TTS rendering
   |  POST /api/voice/tts (cloud)
   |  -- or --
   |  SpeechSynthesis API (browser)
   |
   v
6. Web Audio API playback
```

**Step by step:**

1. The browser captures speech from the microphone.
2. The Web Speech API transcribes speech to text in real time (STT). This
   runs entirely client-side with no server round-trip.
3. The transcript is sent to the backend via `POST /api/sessions/voice/messages`.
   The backend tags the message with `chat_id="voice"` so the voice system
   prompt is applied.
4. The agent processes the message through the standard 6-stage pipeline
   (context assembly, routing, LLM call, tool execution, response
   formatting, delivery). The response is broadcast to the talk mode
   overlay via WebSocket.
5. The response text is rendered to audio. For cloud providers, the
   dashboard sends the text to `POST /api/voice/tts`, which proxies the
   request to OpenAI or ElevenLabs and returns an audio buffer. For the
   browser provider, the `SpeechSynthesis` API generates audio locally.
6. The audio is played back through the Web Audio API, and the overlay
   transitions to the speaking state with waveform visualization.

---

## 9. Troubleshooting

### "Speech recognition not supported"

The Web Speech API is not available in this browser. Use a Chromium-based
browser (Chrome, Edge, Brave) or Safari. Firefox does not support the Web
Speech API for speech recognition.

### "No API key configured"

A cloud TTS provider is selected but no API key was found. Set the
environment variable (`OPENAI_API_KEY` or `ELEVENLABS_API_KEY`) or add the
key to the `providers` section of your config file.

### TTS sounds robotic

Default `local` / `browser` playback in the dashboard uses the operating
system's built-in speech synthesis (Web Speech), which varies in quality.
Switch to `"openai"` or `"elevenlabs"` for neural-quality voices:

```json
{
  "voice": {
    "tts": {
      "provider": "openai",
      "model": "tts-1",
      "voice": "nova"
    }
  }
}
```

### Cannot interrupt speech

Tap the center icon during the speaking state. The overlay must be in the
speaking state (waveform playback animation) for interrupt to take effect.
If the overlay is in the processing state (spinner), the audio has not
started yet and there is nothing to interrupt.

### Voice responses are too verbose

The voice mode system prompt instructs the LLM to keep responses concise.
If responses are unexpectedly long or formatted with Markdown, verify that:

1. The inbound message has `chat_id` set to `"voice"`.
2. The voice system prompt is not being overridden by a `SOUL.md` or
   `IDENTITY.md` file that conflicts with the conversational instructions.

### No audio playback

Check that the browser has permission to play audio. Some browsers require
a user interaction (click or tap) before allowing audio playback. Talk mode
handles this by requiring the user to tap the icon to start, which counts
as a user interaction.

### High latency on TTS

- Switch from `tts-1-hd` to `tts-1` (OpenAI) for faster generation.
- Switch from `eleven_multilingual_v2` to `eleven_turbo_v2_5` (ElevenLabs)
  for lower latency.
- Use the `"browser"` provider for zero-latency local synthesis at the
  cost of voice quality.

---

## Substrate Voice Pipeline (M5-W)

> Status: M5-W (0.7.0). Voice consumer ships disabled by default and is
> the foundation that unblocks the 5 P0 voice security controls
> (WEFT-207..211). Production deployments must wire those before
> turning voice routing on outside a dev shell.

The browser-side Web Speech pipeline above covers the WeftOS panel.
WeftOS also runs a **substrate-side voice pipeline** for headless and
sensor-driven deployments: ESP32-class mics push raw PCM, the
`clawft-service-whisper` substrate service transcribes it, and the
daemon's voice consumer routes the transcripts into either the
agent's chat conversation or the daemon's command surface.

### Pipeline

```text
   Sensor (ESP32 mic)            clawft-service-whisper           voice_router
   ┌────────────────────┐        ┌──────────────────────┐         ┌──────────────────┐
   │ pcm_chunk          │───────▶│ subscribe + window   │────────▶│ subscribe        │
   │ (substrate write)  │        │ POST /inference      │         │ + classify       │
   └────────────────────┘        │ publish transcript   │         │ + route          │
   substrate/<src>/                                                │                  │
   sensor/mic/pcm_chunk          substrate/_derived/transcript/    └────┬─────────┬───┘
                                 <src>/mic                              │         │
                                                                        ▼         ▼
                                                            agent.chat       daemon.dispatch
                                                            (concierge-bot)  (weft <verb> ...)
```

The consumer is intentionally decoupled from the STT backend. Per
ADR-053 (`docs/adr/adr-053-voice-stt-canonical-path.md`) the
substrate-side whisper service is the canonical STT path; swapping in
sherpa-onnx, cloud Whisper, or an offline model means implementing a
service that publishes to the same canonical transcript topic. No
code in the consumer changes.

### Configuration

The consumer reads its configuration from `~/.clawft/config.json`
under `voice.consumer`. Defaults are conservative: disabled, a stable
single-conversation id, and the mesh-canonical transcript topic for
the daemon's default ESP32 source node.

```json
{
  "voice": {
    "enabled": true,
    "consumer": {
      "enabled": true,
      "transcriptTopic": "substrate/_derived/transcript/<mic-node-id>/mic",
      "chatTargetAgent": "concierge-bot",
      "convId": "voice-default",
      "commandPrefix": "weft "
    }
  }
}
```

| Field | Default | Meaning |
| --- | --- | --- |
| `voice.consumer.enabled` | `false` | Master toggle. When false the daemon does not subscribe to the transcript topic. |
| `voice.consumer.transcriptTopic` | `""` (auto) | Substrate path to subscribe to. Empty follows the mic source node (see below); set an explicit path to pin it. Must match what your STT service publishes on. |
| `voice.consumer.chatTargetAgent` | `concierge-bot` | Agent identifier whose chat conversation receives non-command transcripts. |
| `voice.consumer.convId` | `voice-default` | Stable conversation id; per-conv mutex / sink / heartbeat anchor. |
| `voice.consumer.commandPrefix` | `"weft "` | Prefix marking a transcript as a verb. Empty disables command routing. |

#### Picking the transcript topic

The whisper service publishes at
`substrate/_derived/transcript/<source-node-id>/mic`. The source node
id is the substrate node that owns the microphone -- on the daemon
this is the `WHISPER_INPUT_NODE_ID` environment variable when set, and
otherwise the registered node that publishes `sensor/mic`. Node ids are
derived from each node's key (32 hex characters), so there is no default
id. The daemon starts whisper and classify once that node is found, and
logs a WARN naming `WHISPER_INPUT_NODE_ID` every minute while none is.
An empty `transcriptTopic` follows the same node; the consumer needs
whisper's output path, not the sensor's input.

If you are running multiple mic sources, run multiple consumer
instances -- each pinned to one transcript topic -- rather than
fanning out one consumer across topics. The consumer is single-topic
by design so the routing seam stays simple.

### Routing

For every transcript that survives validation:

1. **Command path.** If the transcript text starts with
   `commandPrefix` (case-insensitive -- whisper sometimes capitalizes
   the leading word), the prefix is stripped and the remainder is
   whitespace-split into `<method> <args...>`. The verb dispatches
   through the daemon's existing JSON-RPC `dispatch` function with
   params `{ "args": [...] }`, exactly the same surface the
   `weaver` CLI and the GUI panel use.
2. **Chat path.** Otherwise the transcript becomes a one-turn
   `agent.chat` call against `chatTargetAgent`. The daemon prepends a
   `system`-role message tagging the source so the agent loop's
   system prompt sees attribution:

   ```text
   [voice transcript -- source=voice topic=<substrate path> confidence=0.913]
   ```

   `confidence` is filled from whisper when its `response_format`
   carries a per-segment confidence; otherwise it is `n/a`.

Both paths share one invariant: a malformed or empty transcript
short-circuits without touching either handler. The consumer never
crashes the daemon over a bad transcript.

### Adding a new sensor

A new audio input must:

1. Sign as a registered substrate node (use the `node.register` RPC method
   on first boot; `weaver` has no node command).
2. Publish PCM chunks at `substrate/<your-node>/sensor/mic/pcm_chunk`
   with the wire shape documented on
   `clawft_service_whisper::SUBSTRATE_PCM_INPUT_PATH`:

   ```json
   { "data": "<base64 i16le>", "encoding": "base64", "format": "i16le",
     "sample_rate": 16000, "channels": 1, "samples": 8000,
     "start_ts_ms": 0 }
   ```

3. Be reachable from the substrate the daemon is connected to (mesh
   participant, or co-located on the same daemon).

The whisper service then picks it up automatically -- its input path
is configured per source node -- and publishes transcripts at the
canonical `_derived/transcript/<your-node>/mic`. Point your consumer
at the same path.

### Swapping STT backends

The substrate path is the contract. To run a non-whisper STT engine:

1. Implement an in-process service that subscribes to the same
   `pcm_chunk` topic.
2. Run the audio through your engine of choice (sherpa-onnx, cloud
   Whisper API, on-device wav2vec2, ...).
3. Publish transcripts to `substrate/_derived/transcript/<src>/mic`
   with the wire shape produced by
   `clawft_service_whisper::service::handle_inference_result`:

   ```json
   { "text": "...", "start_ms": 0, "end_ms": 2000,
     "confidence": null, "lang": "en", "seq": 0 }
   ```

4. Hold the `transcript` derived-write grant for your daemon node
   (the daemon issues its own grant at boot in
   `crates/clawft-weave/src/daemon.rs`; federated grants are a
   future phase).

Today there is no formal `SttBackend` trait -- the seam is the
substrate topic. A typed trait is on the roadmap for the multi-engine
phase; until then the topic shape is the load-bearing contract.

### Security: the 5 P0 controls

The consumer ships with **placeholder gating only**. Voice routing
must remain disabled in production until the following ship:

| Plane item | Control | Where it slots in |
| --- | --- | --- |
| WEFT-207 | Sensor enrollment -- gate transcripts on the source node's enrollment status before the consumer accepts them. | Inside `VoiceRouter::handle_line` before `route_command` / `dispatch_chat`. |
| WEFT-208 | Command authorization -- per-verb authz on the `weft <verb>` path. Replaces `permission_stub_allows`. | `voice_router::permission_stub_allows`. |
| WEFT-209 | Rate limit / flood protection -- token-bucket on the dispatch path so a stuck mic cannot DoS the agent loop. | Wraps `ChatHandler` and `CommandHandler`. |
| WEFT-210 | Audit log -- append every routed transcript to the substrate audit chain with source attribution. | After successful `dispatch_chat` / `dispatch_command`. |
| WEFT-211 | Privacy / redaction -- pre-dispatch redaction pass on the transcript text. | Inside `handle_line` before either route. |

Each control replaces a clearly-marked stub in
`crates/clawft-weave/src/voice_router.rs`. The consumer's
`enabled: false` default plus the daemon's "skip spawn when agent
service not wired" guard mean a misconfigured deployment cannot
accidentally surface voice without an operator opt-in.

### Operating the consumer

Daemon log lines worth knowing:

- `voice consumer: subscribed to transcript topic` -- boot succeeded.
- `voice consumer: disabled by config (voice.consumer.enabled=false)`
  -- config flag is off; this is the default.
- `voice consumer: requested but agent service not wired ...` --
  voice was enabled but the LLM-backed agent service did not come up
  at boot. Bring up the LLM service first.
- `voice consumer: chat dispatch failed` -- agent service surfaced an
  error mid-turn (rate limit, context overflow, ...). The
  subscription stays alive; the next transcript is processed.
- `voice consumer: command dispatched` / `command dispatch failed` --
  info / warn breadcrumbs for the verb path.

The consumer is **not** rate-limited today (WEFT-209). If you are
testing with a chatty mic, plan to terminate the daemon if the agent
loop falls behind -- there is no built-in backpressure between the
consumer and `AgentService::dispatch`.

### Testing

Two layers ship in the M5-W landing:

- Unit tests in `crates/clawft-weave/src/voice_router.rs` cover the
  decode + routing logic with stub handlers (no substrate, no agent
  service).
- An integration smoke test in
  `crates/clawft-weave/tests/voice_consumer_smoke.rs` boots a real
  `SubstrateService`, spawns the consumer against it, publishes
  synthetic transcripts, and asserts both routes within a 2-second
  deadline.

```bash
scripts/build.sh check
scripts/build.sh clippy
cargo test -p clawft-weave --tests voice_consumer_smoke
```

The full daemon path (with `DAEMON_AGENT` wired) is covered by the
`agent_chat_dispatch` integration test plus the unit tests in
`clawft-service-agent`. The smoke test deliberately stops short of
booting the LLM service -- that is integration-test territory once
the voice security controls land.

## Mic Privacy Indicator (WEFT-207 / SC-1)

The sensor-side counterpart to the substrate STT consumer is the
mic privacy indicator. Whenever a microphone capture stream opens
(or closes), the capture path emits the indicator on **two
surfaces** so subscribers can choose the one that matches their
trust model:

| Surface | Where | Payload | Consumer |
| --- | --- | --- | --- |
| Tracing event | `target = "voice.privacy.indicator"` | structured `tracing` fields (`state`, `device`, `sample_rate`, `channels`, `ts_unix_micros`, `topic`) | chain layer, syslog, `tracing-subscriber` filter |
| Substrate topic | `weftos.voice.indicator.v1` | JSON `IndicatorPayload` | future GUI (`clawft-gui-egui`), web UI, third-party tray apps |

Both surfaces fire from a single seam --
`crates/clawft-plugin/src/voice/privacy_indicator.rs::emit_indicator`
-- so they cannot drift. The capture handle (`AudioCapture`) wires
its `start` / `stop` (and `Drop` failsafe) through that seam, which
means the indicator already covers the in-tree stub *and* the
0.8.x in-process `cpal::Stream::new` path that lands later.

### Payload schema (v1)

```json
{
  "state":          "capturing" | "idle",
  "device":         "USB Mic" | null,        // null = system default
  "sample_rate":    16000,                    // Hz the capture spec asked for
  "channels":       1,                        // channel count
  "ts_unix_micros": 1714435200000000          // wall-clock μs since epoch
}
```

The `state` strings are stable; treat them as wire contract.

### Subscribing from a UI

```rust
// Pseudo-code for a future GUI consumer.
let mut sub = substrate.subscribe(
    clawft_plugin::voice::privacy_indicator::INDICATOR_TOPIC,
    /* caller_id = */ "gui-mic-indicator",
)?;
while let Some(line) = sub.next().await {
    let payload: IndicatorPayload = serde_json::from_slice(&line.value)?;
    match payload.state.as_str() {
        "capturing" => render_red_dot(payload.device.as_deref()),
        "idle"      => clear_red_dot(),
        _           => {} // forward-compat
    }
}
```

The CLI / TUI today renders the indicator by subscribing to the
tracing target via the existing chain-event layer; once the GUI
ships it consumes the substrate topic instead, with zero changes
on the capture side.

### Wiring a substrate publisher

`clawft-plugin` is intentionally substrate-free, so the capture
layer publishes indicator events through the
`IndicatorPublisher` trait. The default constructor wires
`NoopIndicatorPublisher` (tracing-only); the daemon installs a
substrate-aware publisher at boot:

```rust
let pub_ = Arc::new(SubstrateIndicatorPublisher::new(
    substrate.clone(),
    /* topic = */ INDICATOR_TOPIC,
));
let cap = AudioCapture::new_with_publisher(spec.into(), pub_);
```

Tests use `InMemoryIndicatorPublisher` to assert on the exact
payload sequence without booting substrate.

### Audit invariants

- A `start` always emits exactly one `capturing` event; a
  duplicate `start` is a no-op.
- A `stop` (or `Drop` while active) always emits exactly one
  `idle` event; a duplicate `stop` is a no-op.
- A capture that is never started emits zero indicator events.

These are enforced by the unit tests in
`crates/clawft-plugin/src/voice/capture.rs` and
`crates/clawft-plugin/src/voice/privacy_indicator.rs`. Treat
them as acceptance criteria for any future capture backend.

## Cloud Fallback Transparency (WEFT-224 / SC-3)

When local STT or TTS fails (or STT confidence is below the chain
threshold) and a cloud provider is configured, the fallback chain
dispatches to the cloud. **Before any request leaves the machine**, a
WARN line is emitted so operators can see that audio (or a synthesis
request) is leaving the host:

```text
Cloud fallback active: sending audio to OpenAI Whisper API
```

| Field | Meaning |
| --- | --- |
| Tracing target | `voice.cloud_fallback` |
| Level | **WARN** (always, on every dispatch) |
| `event` | `voice.cloud_fallback` |
| `provider` | Human-readable provider label (e.g. `OpenAI Whisper API`, `OpenAI TTS API`) |
| `reason` | `local_error` or `low_confidence` |
| `modality` | `stt` or `tts` |

**Privacy invariants**

- The log line never includes raw PCM / audio bytes, transcripts,
  synthesis text, or API keys.
- Cloud fallback still requires an explicit key / config path (e.g.
  `OPENAI_API_KEY` for the tools chain); SC-3 only adds transparency
  when that path is already armed.

**Where it lives**

| Stack | Module |
| --- | --- |
| Live tools chain (WEFT-214) | `clawft-tools::voice_backend` — `FallbackStt` / `FallbackTts` |
| Plugin scaffold | `clawft-plugin::voice::fallback` — `SttFallbackChain` / `TtsFallbackChain` |

Filter logs with `RUST_LOG=voice.cloud_fallback=warn` (or include that
target in a broader filter).

## Wake word service

The `voice install-service` subcommand of `weft` installs a background
wake-word listener (the `voice wake` subcommand) that starts with your
session. These subcommands exist only in a `weft` built with the `voice`
Cargo feature; the default build has no `voice` command. The CLI auto-detects the platform service manager:

| Platform | Manager | Artifact / mechanism |
|----------|---------|----------------------|
| Linux | `systemd` | User unit from `scripts/clawft-wake.service` |
| macOS | `launchd` | `~/Library/LaunchAgents/com.clawft.wake.plist` |
| Windows | **`schtasks`** | Task Scheduler task **`ClawftWake`** (ONLOGON) |

Override detection with the `--manager` option (`systemd`, `launchd` or `schtasks`).

### Windows (WEFT-220) — final supported route

Windows does **not** install a Windows Service (SCM). The supported path is
a **per-user Task Scheduler logon task** so the process runs in the
interactive session and can use the microphone.

**Preferred (automated):**

```text
voice install-service
voice install-service --manager schtasks
```

Both are subcommands of `weft` (feature `voice`).

**Optional companion script** (same schtasks semantics; useful for imaging
or when the CLI is not yet on PATH):

```powershell
powershell -ExecutionPolicy Bypass -File scripts/install-clawft-wake-schtasks.ps1
# optional:
#   -WeftPath 'C:\Program Files\weft\weft.exe'
#   -Uninstall
```

**Manual schtasks** (if automation is unavailable):

```text
schtasks /Create /TN "ClawftWake" /TR "C:\path\to\weft.exe voice wake" /SC ONLOGON /RL LIMITED /F
```

Quote the exe path when it contains spaces:

```text
schtasks /Create /TN "ClawftWake" /TR "\"C:\Program Files\weft\weft.exe\" voice wake" /SC ONLOGON /RL LIMITED /F
```

**GUI:** Task Scheduler → Create Task → name `ClawftWake` → trigger At log on
→ action Start a program (`weft.exe`, arguments `voice wake`).

**Manage:**

```text
schtasks /Run    /TN "ClawftWake"
schtasks /End    /TN "ClawftWake"
schtasks /Query  /TN "ClawftWake" /V /FO LIST
schtasks /Delete /TN "ClawftWake" /F
```

This is the **final** Windows install-service design (not a temporary
placeholder). A future Windows Service backend is out of scope unless a
separate ticket reopens the SCM path with interactive/mic constraints
addressed.

---

## Further Reading

- [Configuration Guide](configuration.md) -- Full config file reference.
- [Providers Guide](providers.md) -- Provider routing and API key management.
- [Channels Guide](channels.md) -- Channel plugin architecture.
