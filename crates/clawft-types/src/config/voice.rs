//! Voice pipeline configuration types (W-VOICE workstream).
//!
//! Defines the full voice pipeline config: audio capture, STT, TTS,
//! voice activity detection, wake word, and cloud fallback.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::personality::{ResolvedTtsVoice, VoicePersonality};

/// Voice pipeline configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VoiceConfig {
    /// Enable voice features globally.
    #[serde(default)]
    pub enabled: bool,

    /// Voice transport mode (ADR-074).
    ///
    /// Wire values: `"local"`, `"xai_s2s"`, `"xai_hybrid"`. Product default
    /// is [`VoiceMode::XaiS2s`]; without a key or on health failure the
    /// selection rule fails open to local (see [`resolve_voice_transport`]).
    #[serde(default)]
    pub mode: VoiceMode,

    /// xAI Realtime / speech-to-speech settings (ADR-074).
    #[serde(default)]
    pub xai: VoiceXaiConfig,

    /// Audio capture settings.
    #[serde(default)]
    pub audio: AudioConfig,

    /// Raw-audio retention policy (SC-2 / WEFT-223).
    ///
    /// Default [`AudioRetention::None`]: process PCM in memory and
    /// discard (zeroize) after STT — never write `.wav`/`.pcm` to disk.
    #[serde(default, alias = "audioRetention")]
    pub audio_retention: AudioRetention,

    /// Speech-to-text settings.
    #[serde(default)]
    pub stt: SttConfig,

    /// Text-to-speech settings.
    #[serde(default)]
    pub tts: TtsConfig,

    /// Operator pin for the microphone node whose audio feeds whisper and
    /// the classifier (a node id, 32 hex chars). The `WHISPER_INPUT_NODE_ID`
    /// env var overrides it. When unset the daemon auto-selects only if
    /// exactly one registered node publishes `sensor/mic`, and refuses
    /// (with a warning naming the candidates) when there are several.
    #[serde(default, alias = "micNodeId", skip_serializing_if = "Option::is_none")]
    pub mic_node_id: Option<String>,

    /// Voice activity detection settings.
    #[serde(default)]
    pub vad: VadConfig,

    /// Wake word detection settings.
    #[serde(default)]
    pub wake: WakeConfig,

    /// Cloud fallback settings.
    #[serde(default, alias = "cloudFallback")]
    pub cloud_fallback: CloudFallbackConfig,

    /// Per-agent voice personality map.
    ///
    /// Keys are agent names/IDs, values are their voice personalities.
    /// Agents not in this map use the default personality.
    #[serde(default, alias = "personalities")]
    pub personalities: HashMap<String, VoicePersonality>,

    /// Voice transcript consumer configuration (WEFT-555 / M5-W).
    ///
    /// Controls the daemon-side bridge that subscribes to the substrate
    /// transcript topic published by `clawft-service-whisper`, then
    /// routes each transcript into either:
    ///
    /// - the configured agent's `agent.chat` conversation, or
    /// - the daemon's RPC dispatch, when the transcript starts with
    ///   the configured `command_prefix`.
    ///
    /// Disabled by default; voice features must be opted into.
    #[serde(default)]
    pub consumer: VoiceConsumerConfig,

    /// Destructive-action confirmation (SC-6 / WEFT-225).
    ///
    /// Anti-replay nonce + transcription-echo prompts for
    /// `VoiceCommand.confirm` (and other destructive voice actions).
    #[serde(default)]
    pub confirmation: VoiceConfirmationConfig,

    /// Voice rate limiting (SC-8 / WEFT-226).
    ///
    /// Token-bucket caps for commands and wake activations, plus a
    /// post-fail cooldown after consecutive low-confidence / failed
    /// command attempts.
    #[serde(default, alias = "rateLimit")]
    pub rate_limit: VoiceRateLimitConfig,
}

/// Voice transport mode — mouth/ears path (ADR-074).
///
/// | Mode | Behavior |
/// |------|----------|
/// | [`Local`](Self::Local) | ADR-061 pipeline only (offline / private) |
/// | [`XaiS2s`](Self::XaiS2s) | Full-duplex xAI Realtime speech-to-speech (interim primary when key + healthy) |
/// | [`XaiHybrid`](Self::XaiHybrid) | Local or xAI STT + agent + xAI TTS (transition / cost control) |
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VoiceMode {
    /// Always use the local ADR-061 pipeline.
    Local,
    /// xAI Realtime speech-to-speech (interim product default; fails open to local).
    #[default]
    XaiS2s,
    /// Hybrid: mix local STT/agent with cloud TTS (or similar).
    XaiHybrid,
}

impl VoiceMode {
    /// True when this mode would prefer an xAI path (subject to key + health).
    pub fn prefers_xai(self) -> bool {
        matches!(self, Self::XaiS2s | Self::XaiHybrid)
    }
}

/// xAI Realtime configuration under `[voice.xai]` (ADR-074 / WEFT-689).
///
/// Secrets are never stored here — only the **name** of the env var that holds
/// the API key. Never log the key or raw audio.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoiceXaiConfig {
    /// Environment variable that holds the xAI API key.
    /// Empty string means inherit the default [`DEFAULT_XAI_API_KEY_ENV`].
    #[serde(default = "default_xai_api_key_env", alias = "apiKeyEnv")]
    pub api_key_env: String,

    /// Realtime model alias (e.g. `grok-voice-latest`).
    #[serde(default = "default_xai_realtime_model", alias = "realtimeModel")]
    pub realtime_model: String,

    /// WebSocket endpoint for Realtime (OpenAI-Realtime-compatible shape).
    #[serde(default = "default_xai_endpoint")]
    pub endpoint: String,
}

/// Default env var for the xAI API key (matches LLM provider story).
pub const DEFAULT_XAI_API_KEY_ENV: &str = "XAI_API_KEY";

/// Default xAI Realtime model alias (tracks think-fast line).
pub const DEFAULT_XAI_REALTIME_MODEL: &str = "grok-voice-latest";

/// Default xAI Realtime WebSocket endpoint.
pub const DEFAULT_XAI_REALTIME_ENDPOINT: &str = "wss://api.x.ai/v1/realtime";

fn default_xai_api_key_env() -> String {
    DEFAULT_XAI_API_KEY_ENV.into()
}
fn default_xai_realtime_model() -> String {
    DEFAULT_XAI_REALTIME_MODEL.into()
}
fn default_xai_endpoint() -> String {
    DEFAULT_XAI_REALTIME_ENDPOINT.into()
}

impl Default for VoiceXaiConfig {
    fn default() -> Self {
        Self {
            api_key_env: default_xai_api_key_env(),
            realtime_model: default_xai_realtime_model(),
            endpoint: default_xai_endpoint(),
        }
    }
}

impl VoiceXaiConfig {
    /// Resolved env var name (falls back to [`DEFAULT_XAI_API_KEY_ENV`] when empty).
    pub fn api_key_env_name(&self) -> &str {
        if self.api_key_env.trim().is_empty() {
            DEFAULT_XAI_API_KEY_ENV
        } else {
            self.api_key_env.as_str()
        }
    }

    /// Build the Realtime WebSocket URL with `?model=…` query parameter.
    ///
    /// Does not include credentials. Safe to log.
    pub fn realtime_url(&self) -> String {
        let base = if self.endpoint.trim().is_empty() {
            DEFAULT_XAI_REALTIME_ENDPOINT
        } else {
            self.endpoint.trim()
        };
        let model = if self.realtime_model.trim().is_empty() {
            DEFAULT_XAI_REALTIME_MODEL
        } else {
            self.realtime_model.trim()
        };
        // Preserve any existing query string; append or set model.
        if base.contains('?') {
            if base.contains("model=") {
                base.to_string()
            } else {
                format!("{base}&model={model}")
            }
        } else {
            format!("{base}?model={model}")
        }
    }
}

/// Resolved transport after applying ADR-074 selection rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedVoiceTransport {
    /// Local ADR-061 pipeline.
    Local,
    /// xAI full speech-to-speech Realtime.
    XaiS2s,
    /// Hybrid cloud/local mix.
    XaiHybrid,
}

impl ResolvedVoiceTransport {
    /// True when the resolved path uses xAI cloud voice.
    pub fn is_xai(self) -> bool {
        matches!(self, Self::XaiS2s | Self::XaiHybrid)
    }
}

/// Why the selection rule failed open to local (ADR-074 / WEFT-691).
///
/// Distinct from [`VoiceTransportDecision::notice`]: reason is for metrics
/// and control flow; notice is the single user-visible string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoiceFallbackReason {
    /// Configured xAI mode but API key env was missing/empty.
    NoApiKey,
    /// Pre-dial health probe reported unhealthy (or stub injected fail).
    HealthFailed,
    /// Connect / mid-session failure; graceful re-resolve to local.
    RuntimeFailure,
}

/// One leg of a hybrid voice path (STT / agent / TTS).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HybridVoiceLeg {
    /// Local ADR-061 component.
    Local,
    /// xAI cloud component.
    Xai,
}

/// Composition for [`ResolvedVoiceTransport::XaiHybrid`] (or all-local after fallback).
///
/// ADR-074 hybrid: **local STT + local agent (Hermes) + xAI TTS** when healthy.
/// On any fail-open, every leg is [`HybridVoiceLeg::Local`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HybridVoiceComposition {
    pub stt: HybridVoiceLeg,
    pub agent: HybridVoiceLeg,
    pub tts: HybridVoiceLeg,
}

impl HybridVoiceComposition {
    /// Fully local pipeline (offline / fallback).
    pub const LOCAL_ALL: Self = Self {
        stt: HybridVoiceLeg::Local,
        agent: HybridVoiceLeg::Local,
        tts: HybridVoiceLeg::Local,
    };

    /// Default healthy hybrid: local STT + Hermes agent + xAI TTS.
    pub const HYBRID_DEFAULT: Self = Self {
        stt: HybridVoiceLeg::Local,
        agent: HybridVoiceLeg::Local,
        tts: HybridVoiceLeg::Xai,
    };

    /// True when any leg still uses xAI.
    pub fn uses_xai(self) -> bool {
        matches!(self.stt, HybridVoiceLeg::Xai)
            || matches!(self.agent, HybridVoiceLeg::Xai)
            || matches!(self.tts, HybridVoiceLeg::Xai)
    }
}

/// Outcome of [`resolve_voice_transport`] / [`VoiceConfig::resolve_transport`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceTransportDecision {
    /// Transport path that should be used.
    pub transport: ResolvedVoiceTransport,
    /// Single user-visible notice when failing open to local (never silent forever).
    pub notice: Option<&'static str>,
    /// Machine-readable fallback reason (metrics / control). `None` when not falling back.
    pub fallback_reason: Option<VoiceFallbackReason>,
    /// Hybrid leg composition. Meaningful for hybrid (and all-local after hybrid fallback).
    pub hybrid: HybridVoiceComposition,
}

impl VoiceTransportDecision {
    /// True when this decision is a fail-open (or runtime) fallback to local.
    pub fn is_fallback(&self) -> bool {
        self.fallback_reason.is_some()
    }

    /// Local-only decision with no notice (explicit `mode = local` or default local path).
    pub fn local_ok() -> Self {
        Self {
            transport: ResolvedVoiceTransport::Local,
            notice: None,
            fallback_reason: None,
            hybrid: HybridVoiceComposition::LOCAL_ALL,
        }
    }
}

/// User-visible notice when xAI was requested but key is missing.
pub const VOICE_NOTICE_XAI_NO_KEY: &str =
    "xAI voice requested but API key not found; using local voice pipeline.";

/// User-visible notice when xAI was requested but health check failed.
pub const VOICE_NOTICE_XAI_UNHEALTHY: &str =
    "xAI voice requested but endpoint health check failed; using local voice pipeline.";

/// User-visible notice when xAI was selected but failed at runtime (connect/session).
pub const VOICE_NOTICE_XAI_RUNTIME_FALLBACK: &str =
    "xAI voice session failed; falling back to local voice pipeline.";

/// Pure health-probe result (V2 stub — no I/O). Callers supply the boolean
/// from a real probe later; unit tests inject fail/success directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XaiHealthProbeResult {
    /// Whether the xAI voice endpoint is considered healthy.
    pub healthy: bool,
    /// Safe-to-log detail (never credentials or raw audio).
    pub detail: &'static str,
}

impl XaiHealthProbeResult {
    /// Inject a healthy result (default when probing is skipped).
    pub fn healthy_stub() -> Self {
        Self {
            healthy: true,
            detail: "stub: assumed healthy",
        }
    }

    /// Inject an unhealthy result (unit tests / forced fallback).
    pub fn unhealthy_stub() -> Self {
        Self {
            healthy: false,
            detail: "stub: injected unhealthy",
        }
    }

    /// Build from a precomputed flag (channels health probe maps into this).
    pub fn from_flag(healthy: bool) -> Self {
        if healthy {
            Self::healthy_stub()
        } else {
            Self::unhealthy_stub()
        }
    }
}

/// ADR-074 selection rule (pure; no I/O).
///
/// 1. `mode = local` → always local.
/// 2. Else if no API key **or** health fails → fail open to local with one notice.
/// 3. Else use the xAI path corresponding to `mode` (hybrid keeps local STT/agent).
///
/// `api_key_present` and `xai_healthy` are supplied by the caller so unit tests
/// can mock env / health without process-global state.
pub fn resolve_voice_transport(
    mode: VoiceMode,
    api_key_present: bool,
    xai_healthy: bool,
) -> VoiceTransportDecision {
    if mode == VoiceMode::Local {
        return VoiceTransportDecision::local_ok();
    }

    if !api_key_present {
        return VoiceTransportDecision {
            transport: ResolvedVoiceTransport::Local,
            notice: Some(VOICE_NOTICE_XAI_NO_KEY),
            fallback_reason: Some(VoiceFallbackReason::NoApiKey),
            hybrid: HybridVoiceComposition::LOCAL_ALL,
        };
    }

    if !xai_healthy {
        return VoiceTransportDecision {
            transport: ResolvedVoiceTransport::Local,
            notice: Some(VOICE_NOTICE_XAI_UNHEALTHY),
            fallback_reason: Some(VoiceFallbackReason::HealthFailed),
            hybrid: HybridVoiceComposition::LOCAL_ALL,
        };
    }

    match mode {
        VoiceMode::Local => VoiceTransportDecision::local_ok(),
        VoiceMode::XaiS2s => VoiceTransportDecision {
            transport: ResolvedVoiceTransport::XaiS2s,
            notice: None,
            fallback_reason: None,
            // S2S is full cloud; hybrid composition unused but all-xai-ish for TTS/STT.
            hybrid: HybridVoiceComposition {
                stt: HybridVoiceLeg::Xai,
                agent: HybridVoiceLeg::Xai,
                tts: HybridVoiceLeg::Xai,
            },
        },
        VoiceMode::XaiHybrid => VoiceTransportDecision {
            transport: ResolvedVoiceTransport::XaiHybrid,
            notice: None,
            fallback_reason: None,
            hybrid: HybridVoiceComposition::HYBRID_DEFAULT,
        },
    }
}

/// Same as [`resolve_voice_transport`] but accepts a structured health probe result.
pub fn resolve_voice_transport_with_health(
    mode: VoiceMode,
    api_key_present: bool,
    health: &XaiHealthProbeResult,
) -> VoiceTransportDecision {
    resolve_voice_transport(mode, api_key_present, health.healthy)
}

/// Graceful mid-session / connect failure fallback to local (WEFT-691).
///
/// Always returns local with [`VOICE_NOTICE_XAI_RUNTIME_FALLBACK`]. Safe to call
/// when `mode` was already local (still reports runtime reason for metrics).
pub fn resolve_voice_transport_runtime_fallback(mode: VoiceMode) -> VoiceTransportDecision {
    let _ = mode; // preferred mode is abandoned; local wins
    VoiceTransportDecision {
        transport: ResolvedVoiceTransport::Local,
        notice: Some(VOICE_NOTICE_XAI_RUNTIME_FALLBACK),
        fallback_reason: Some(VoiceFallbackReason::RuntimeFailure),
        hybrid: HybridVoiceComposition::LOCAL_ALL,
    }
}

impl VoiceConfig {
    /// Apply ADR-074 selection using an env lookup for the configured key name.
    ///
    /// `xai_healthy` is a precomputed health flag (V2: wire via
    /// [`XaiHealthProbeResult`] / channels probe stub). Never logs secret values.
    pub fn resolve_transport(
        &self,
        env_lookup: impl FnOnce(&str) -> Option<String>,
        xai_healthy: bool,
    ) -> VoiceTransportDecision {
        let key_name = self.xai.api_key_env_name();
        let api_key_present = env_lookup(key_name)
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false);
        resolve_voice_transport(self.mode, api_key_present, xai_healthy)
    }

    /// Resolve using a structured health probe result (V2).
    pub fn resolve_transport_with_health(
        &self,
        env_lookup: impl FnOnce(&str) -> Option<String>,
        health: &XaiHealthProbeResult,
    ) -> VoiceTransportDecision {
        self.resolve_transport(env_lookup, health.healthy)
    }

    /// Convenience: read the configured env var via `std::env::var`.
    pub fn resolve_transport_from_env(&self, xai_healthy: bool) -> VoiceTransportDecision {
        self.resolve_transport(|name| std::env::var(name).ok(), xai_healthy)
    }

    /// Resolve per-agent TTS voice parameters from [`Self::personalities`]
    /// (WEFT-222).
    ///
    /// Lookup is by exact `agent_id` key. Agents not present in the map fall
    /// back to [`VoicePersonality::default`], with `voice_id` / `speed`
    /// overridden from global [`Self::tts`] when those fields are set
    /// (non-empty voice, non-default speed).
    pub fn resolve_tts_for_agent(&self, agent_id: &str) -> ResolvedTtsVoice {
        if let Some(p) = self.personalities.get(agent_id) {
            return ResolvedTtsVoice::from_personality(agent_id, p);
        }
        let mut resolved = ResolvedTtsVoice::default_for_agent(agent_id);
        if !self.tts.voice.trim().is_empty() {
            resolved.voice_id = self.tts.voice.clone();
        }
        // Global TtsConfig has no pitch; speed is always present.
        resolved.speed = self.tts.speed;
        if !self.tts.provider.trim().is_empty() {
            resolved.provider = self.tts.provider.clone();
        }
        resolved
    }

    /// Greeting string for `agent_id` if configured (trimmed, non-empty).
    ///
    /// Callers that own a session should consume this **once** at session
    /// start (see channels `PersonalityTtsDispatch::take_session_greeting`),
    /// not on every turn.
    pub fn session_greeting_for(&self, agent_id: &str) -> Option<String> {
        self.resolve_tts_for_agent(agent_id)
            .greeting()
            .map(|s| s.to_string())
    }

    /// Build spoken text for a turn: when `session_start` is true and the
    /// agent has a `greeting_prefix`, prefix the body (or return the
    /// greeting alone when `body` is empty).
    pub fn tts_text_for(&self, agent_id: &str, body: &str, session_start: bool) -> String {
        if session_start
            && let Some(g) = self.session_greeting_for(agent_id)
        {
            let body = body.trim();
            if body.is_empty() {
                return g;
            }
            return format!("{g} {body}");
        }
        body.to_string()
    }

    /// Validate every configured personality (speed/pitch/voice_id ranges).
    pub fn validate_personalities(&self) -> Result<(), String> {
        for (agent_id, p) in &self.personalities {
            p.validate()
                .map_err(|e| format!("personality for agent {agent_id:?}: {e}"))?;
        }
        Ok(())
    }
}

/// How long raw PCM may be retained after capture (SC-2 / WEFT-223).
///
/// | Variant | Memory | Disk |
/// |---------|--------|------|
/// | [`None`](Self::None) | zeroize after STT / drop | never |
/// | [`Session`](Self::Session) | hold until session end, then zeroize | never |
/// | [`Persist`](Self::Persist) | as session | may write under `~/.clawft/audio/` |
///
/// Default is **`none`** — privacy-preserving; operators must opt into
/// retention explicitly.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AudioRetention {
    /// Do not retain raw audio (default). Zeroize buffers on drop; never
    /// write PCM/WAV to disk.
    #[default]
    None,
    /// Keep raw audio in memory for the current voice session only.
    /// Deleted / zeroized on session exit. Still no disk write.
    Session,
    /// Persist raw audio under the workspace / home audio directory.
    /// Operator is responsible for cleanup and access control.
    Persist,
}

impl AudioRetention {
    /// True when raw samples may be written to disk (`.wav` / `.pcm`).
    pub fn allows_disk_write(self) -> bool {
        matches!(self, Self::Persist)
    }

    /// True when raw samples may be held past the immediate STT handoff
    /// (session buffer or disk-backed archive).
    pub fn allows_session_hold(self) -> bool {
        matches!(self, Self::Session | Self::Persist)
    }

    /// True when the policy is the default privacy-preserving mode.
    pub fn is_none(self) -> bool {
        matches!(self, Self::None)
    }
}

/// Voice transcript consumer configuration.
///
/// The consumer subscribes to a single substrate path emitted by
/// `clawft-service-whisper` (typically the mesh-canonical
/// `substrate/_derived/transcript/<source-node>/mic`) and bridges
/// transcripts into the daemon's agent / command surfaces.
///
/// Defaults match the daemon's ESP32-source whisper wiring at boot
/// (`WHISPER_INPUT_NODE_ID` env var, else the registered node that publishes
/// `sensor/mic`; there is no baked-in node id). The
/// `enabled` flag defaults to `false` — voice routing is opt-in until
/// the 5 P0 voice security controls (WEFT-207/208/209/210/211) ship.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoiceConsumerConfig {
    /// Master enable toggle. When `false` the daemon does not subscribe
    /// to the transcript topic at all; whisper still publishes
    /// transcripts but no consumer reads them.
    #[serde(default)]
    pub enabled: bool,

    /// Substrate path to subscribe to for transcripts. Defaults to the
    /// mesh-canonical path produced by `clawft-service-whisper` for the
    /// daemon's configured input source node.
    #[serde(default = "default_transcript_topic", alias = "transcriptTopic")]
    pub transcript_topic: String,

    /// Agent identifier whose conversation receives non-command
    /// transcripts. Defaults to the daemon's single concierge principal
    /// (`concierge-bot`); per-agent fan-out is a future phase.
    #[serde(default = "default_chat_target_agent", alias = "chatTargetAgent")]
    pub chat_target_agent: String,

    /// Conversation id used when synthesizing the agent.chat dispatch.
    /// Stable across transcripts so the agent loop's per-conv state
    /// (mutex, sink, heartbeat) accumulates one continuous voice
    /// session.
    #[serde(default = "default_voice_conv_id", alias = "convId")]
    pub conv_id: String,

    /// Prefix indicating a transcript should be parsed as a command and
    /// routed through the daemon's RPC dispatch instead of the agent
    /// conversation. The prefix (including any trailing whitespace) is
    /// stripped before parsing; the remainder is whitespace-split into
    /// `<method> <args...>`.
    ///
    /// Empty string disables command routing — every transcript falls
    /// through to chat.
    #[serde(default = "default_command_prefix", alias = "commandPrefix")]
    pub command_prefix: String,

    /// Per-voice-principal permission grid (WEFT-208 / SC-4).
    ///
    /// Determines which voice transcripts may dispatch chat queries
    /// vs. commands, gated by a 3-level scheme:
    ///
    /// - **Level 0** (read-only / unauthenticated voice): chat dispatch
    ///   allowed; command dispatch refused outright.
    /// - **Level 1** (user / authenticated voice): chat dispatch +
    ///   commands listed in [`safe_commands`] only.
    /// - **Level 2** (privileged): chat + any command, subject to the
    ///   standard governance gate at the kernel.
    ///
    /// Source of principal: the `actor_id` carried on the substrate
    /// publish line that delivered the transcript (set by
    /// `clawft-service-whisper` to the source sensor node id). Missing
    /// principal → Level 0.
    #[serde(default)]
    pub permissions: VoicePermissionConfig,
}

/// Voice-router permission configuration (WEFT-208 / SC-4).
///
/// See [`VoiceConsumerConfig::permissions`] for the conceptual grid.
/// All three fields default to the strictest reading: every voice
/// principal is Level 0 (chat-only) until the operator grants higher
/// trust.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoicePermissionConfig {
    /// Permission level used for any voice principal not present in
    /// [`Self::principal_levels`]. Also used when the substrate publish
    /// line carries no `actor_id`.
    ///
    /// Valid values: 0, 1, 2. Out-of-range values are clamped to 0 by
    /// the router (treated as "unknown / read-only").
    #[serde(default = "default_voice_default_level", alias = "defaultLevel")]
    pub default_level: u8,

    /// Per-principal permission overrides. Key is the substrate
    /// `actor_id` (typically a sensor node id like `n-bfc4cd`), value is
    /// the level (0, 1, or 2).
    #[serde(default, alias = "principalLevels")]
    pub principal_levels: HashMap<String, u8>,

    /// Allowlist of command verbs that Level 1 principals may dispatch.
    /// Verbs are matched case-sensitively against the first whitespace-
    /// delimited token of the command body (the same `method` the
    /// router would otherwise hand to the kernel).
    ///
    /// Default: `["status", "list", "help"]` — the read-only verbs the
    /// audit's voice security spec calls out as universally safe.
    #[serde(default = "default_safe_commands", alias = "safeCommands")]
    pub safe_commands: Vec<String>,
}

fn default_voice_default_level() -> u8 {
    0
}

fn default_safe_commands() -> Vec<String> {
    vec!["status".into(), "list".into(), "help".into()]
}

impl Default for VoicePermissionConfig {
    fn default() -> Self {
        Self {
            default_level: default_voice_default_level(),
            principal_levels: HashMap::new(),
            safe_commands: default_safe_commands(),
        }
    }
}

/// Empty means "auto": the daemon subscribes to
/// `substrate/_derived/transcript/<mic source node>/mic`, resolving the
/// mic node at runtime. Set an explicit path to pin it.
fn default_transcript_topic() -> String {
    String::new()
}

fn default_chat_target_agent() -> String {
    "concierge-bot".into()
}

fn default_voice_conv_id() -> String {
    "voice-default".into()
}

fn default_command_prefix() -> String {
    "weft ".into()
}

impl Default for VoiceConsumerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            transcript_topic: default_transcript_topic(),
            chat_target_agent: default_chat_target_agent(),
            conv_id: default_voice_conv_id(),
            command_prefix: default_command_prefix(),
            permissions: VoicePermissionConfig::default(),
        }
    }
}

/// Audio capture/playback configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioConfig {
    /// Sample rate in Hz.
    #[serde(default = "default_sample_rate", alias = "sampleRate")]
    pub sample_rate: u32,

    /// Audio chunk size in samples.
    #[serde(default = "default_chunk_size", alias = "chunkSize")]
    pub chunk_size: u32,

    /// Number of audio channels (1 = mono).
    #[serde(default = "default_audio_channels")]
    pub channels: u16,

    /// Input device name (None = system default).
    #[serde(default, alias = "inputDevice")]
    pub input_device: Option<String>,

    /// Output device name (None = system default).
    #[serde(default, alias = "outputDevice")]
    pub output_device: Option<String>,
}

fn default_sample_rate() -> u32 {
    16000
}
fn default_chunk_size() -> u32 {
    512
}
fn default_audio_channels() -> u16 {
    1
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            sample_rate: default_sample_rate(),
            chunk_size: default_chunk_size(),
            channels: default_audio_channels(),
            input_device: None,
            output_device: None,
        }
    }
}

/// Speech-to-text configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SttConfig {
    /// Enable STT.
    #[serde(default = "super::default_true")]
    pub enabled: bool,

    /// STT model name or path.
    #[serde(default = "default_stt_model")]
    pub model: String,

    /// Language code (e.g. "en", "zh", "es"). Empty = auto-detect.
    #[serde(default)]
    pub language: String,
}

fn default_stt_model() -> String {
    "sherpa-onnx-streaming-zipformer-en-20M".into()
}

impl Default for SttConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            model: default_stt_model(),
            language: String::new(),
        }
    }
}

/// Text-to-speech configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TtsConfig {
    /// Enable TTS.
    #[serde(default = "super::default_true")]
    pub enabled: bool,

    /// TTS provider: "local" (default), "browser" (UI Web Speech), "openai", or "elevenlabs".
    #[serde(default = "default_tts_provider")]
    pub provider: String,

    /// TTS model name or path.
    #[serde(default = "default_tts_model")]
    pub model: String,

    /// TTS voice ID.
    #[serde(default)]
    pub voice: String,

    /// Speaking speed multiplier (1.0 = normal).
    #[serde(default = "default_speed")]
    pub speed: f32,

    /// Native Talk-Mode **fast-tier** engine preference (WEFT-613 / ADR-061).
    ///
    /// Wire values: `"auto"` (default), `"kokoro"`, `"chatterbox"` (alias
    /// `"clone"`). `auto` selects the cloned-voice path when native Chatterbox
    /// (or equivalent) inference is runtime-ready; otherwise Kokoro preset.
    /// In 0.8 clone inference is **not** implemented — `auto` always falls
    /// through to Kokoro. Env override: `WEFTOS_FAST_TTS`.
    #[serde(default = "default_fast_engine", alias = "fastEngine")]
    pub fast_engine: String,
}

/// local Piper model default. Cloud proxy (`openai` / `elevenlabs`) requires
/// keys; `"browser"` is a UI-only Web Speech opt-in (already wired in
/// `clawft-ui` `speak()` — not a server-side synthesizer).
pub const DEFAULT_TTS_PROVIDER: &str = "local";

/// Known TTS provider wire values for `voice.tts.provider`.
pub mod tts_providers {
    /// Offline / native path (default). Not served by `/api/voice/tts` cloud proxy.
    pub const LOCAL: &str = "local";
    /// Alias for local stub / empty-audio engines (accepted as local).
    pub const LOCAL_STUB: &str = "local-stub";
    /// Browser Web Speech API (client-side only; UI dispatches `speechSynthesis`).
    pub const BROWSER: &str = "browser";
    /// OpenAI cloud TTS (server proxy; needs API key).
    pub const OPENAI: &str = "openai";
    /// ElevenLabs cloud TTS (server proxy; needs API key).
    pub const ELEVENLABS: &str = "elevenlabs";
}

/// True when the provider is offline/local (including `local-stub`).
pub fn is_local_tts_provider(provider: &str) -> bool {
    matches!(
        provider,
        tts_providers::LOCAL | tts_providers::LOCAL_STUB
    )
}

/// True when the provider is the browser Web Speech opt-in.
pub fn is_browser_tts_provider(provider: &str) -> bool {
    provider == tts_providers::BROWSER
}

/// True when the provider is a cloud TTS that the HTTP proxy can synthesize.
pub fn is_cloud_tts_provider(provider: &str) -> bool {
    matches!(
        provider,
        tts_providers::OPENAI | tts_providers::ELEVENLABS
    )
}


fn default_tts_provider() -> String {
    DEFAULT_TTS_PROVIDER.into()
}
fn default_tts_model() -> String {
    "vits-piper-en_US-amy-medium".into()
}
fn default_speed() -> f32 {
    1.0
}

/// Default native fast-tier preference (david profile intent: auto → clone when ready).
pub const DEFAULT_TTS_FAST_ENGINE: &str = "auto";

fn default_fast_engine() -> String {
    DEFAULT_TTS_FAST_ENGINE.into()
}

impl Default for TtsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            provider: default_tts_provider(),
            model: default_tts_model(),
            voice: String::new(),
            speed: default_speed(),
            fast_engine: default_fast_engine(),
        }
    }
}

/// Voice activity detection configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VadConfig {
    /// VAD activation threshold (0.0-1.0).
    #[serde(default = "default_vad_threshold")]
    pub threshold: f32,

    /// Silence duration in ms before speech end (baseline for adaptive
    /// learning; see [`Self::adaptive`] / WEFT-230).
    #[serde(default = "default_silence_timeout_ms", alias = "silenceTimeoutMs")]
    pub silence_timeout_ms: u32,

    /// Minimum speech duration in ms to trigger processing.
    #[serde(default = "default_min_speech_ms", alias = "minSpeechMs")]
    pub min_speech_ms: u32,

    /// Per-session adaptive silence-timeout learning (WEFT-230).
    ///
    /// Starts from [`Self::silence_timeout_ms`] and learns toward the
    /// user's pause/speech patterns within the configured min/max.
    #[serde(default, alias = "adaptiveSilence")]
    pub adaptive: super::AdaptiveSilenceConfig,
}

fn default_vad_threshold() -> f32 {
    0.5
}
fn default_silence_timeout_ms() -> u32 {
    1500
}
fn default_min_speech_ms() -> u32 {
    250
}

impl Default for VadConfig {
    fn default() -> Self {
        Self {
            threshold: default_vad_threshold(),
            silence_timeout_ms: default_silence_timeout_ms(),
            min_speech_ms: default_min_speech_ms(),
            adaptive: super::AdaptiveSilenceConfig::default(),
        }
    }
}

impl VadConfig {
    /// Build a per-session [`super::AdaptiveSilenceTimeout`] from this
    /// config (baseline = [`Self::silence_timeout_ms`]).
    pub fn silence_estimator(&self) -> super::AdaptiveSilenceTimeout {
        super::AdaptiveSilenceTimeout::new(self.silence_timeout_ms, self.adaptive.clone())
    }
}

/// Wake word detection configuration.
///
/// # WEFT-240 — sensitivity vs detector threshold
///
/// Product config uses **`sensitivity`** (higher = more sensitive = easier
/// to trigger). The transitional wake detector
/// (`WakeWordConfig.threshold` in `clawft-plugin`) uses a **score threshold**
/// (higher = harder to trigger). Mapping:
///
/// ```text
/// detector.threshold = (1.0 - sensitivity).clamp(0.0, 1.0)
/// sensitivity        = (1.0 - detector.threshold).clamp(0.0, 1.0)
/// ```
///
/// `threshold` is accepted as a **serde alias** of `sensitivity` for
/// historical configs that already used the detector name; when present it
/// is interpreted as **product sensitivity** (identity), not inverted, so
/// old `sensitivity` keys keep their meaning. Prefer `sensitivity` in new
/// product TOML/JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WakeConfig {
    /// Enable wake word detection.
    #[serde(default)]
    pub enabled: bool,

    /// Wake word phrase (e.g. "hey weft").
    #[serde(default = "default_wake_phrase")]
    pub phrase: String,

    /// Detection sensitivity (0.0–1.0). Higher = more sensitive.
    ///
    /// Serde alias `threshold` is accepted for historical keys (same scale
    /// as this field — **not** the detector score threshold). Use
    /// [`WakeConfig::to_detector_threshold`] for the inverted detector knob.
    #[serde(
        default = "default_wake_sensitivity",
        alias = "threshold"
    )]
    pub sensitivity: f32,

    /// Custom wake word model path (`String` for JSON/TOML friendliness).
    ///
    /// Convert with [`WakeConfig::model_path_buf`] for `PathBuf` consumers
    /// (e.g. `WakeWordConfig.model_path`).
    #[serde(default, alias = "modelPath")]
    pub model_path: Option<String>,
}

fn default_wake_phrase() -> String {
    "hey weft".into()
}
fn default_wake_sensitivity() -> f32 {
    0.5
}

impl Default for WakeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            phrase: default_wake_phrase(),
            sensitivity: default_wake_sensitivity(),
            model_path: None,
        }
    }
}

impl WakeConfig {
    /// Detector score threshold corresponding to this product sensitivity
    /// (WEFT-240): `threshold = 1.0 - sensitivity`.
    pub fn to_detector_threshold(&self) -> f32 {
        (1.0 - self.sensitivity).clamp(0.0, 1.0)
    }

    /// Build product sensitivity from a detector score threshold.
    pub fn sensitivity_from_detector_threshold(threshold: f32) -> f32 {
        (1.0 - threshold).clamp(0.0, 1.0)
    }

    /// `model_path` as [`std::path::PathBuf`], if set and non-empty.
    pub fn model_path_buf(&self) -> Option<std::path::PathBuf> {
        self.model_path
            .as_ref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(std::path::PathBuf::from)
    }
}

/// Known cloud STT providers for [`CloudFallbackConfig::stt_provider`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CloudSttProviderKind {
    /// OpenAI Whisper API (`whisper`, `openai`, `openai-whisper`, `openai_whisper`).
    OpenAiWhisper,
}

impl CloudSttProviderKind {
    /// Canonical config string for this provider.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenAiWhisper => "whisper",
        }
    }

    /// Human-readable label for SC-3 transparency logs (no secrets).
    pub fn display_name(self) -> &'static str {
        match self {
            Self::OpenAiWhisper => "OpenAI Whisper API",
        }
    }
}

/// Known cloud TTS providers for [`CloudFallbackConfig::tts_provider`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CloudTtsProviderKind {
    /// OpenAI TTS API (`openai`, `openai-tts`, `openai_tts`).
    OpenAi,
    /// ElevenLabs TTS (`elevenlabs`, `11labs`).
    ElevenLabs,
}

impl CloudTtsProviderKind {
    /// Canonical config string for this provider.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::ElevenLabs => "elevenlabs",
        }
    }

    /// Human-readable label for SC-3 transparency logs (no secrets).
    pub fn display_name(self) -> &'static str {
        match self {
            Self::OpenAi => "OpenAI TTS API",
            Self::ElevenLabs => "ElevenLabs API",
        }
    }
}

/// Error from [`CloudFallbackConfig`] provider routing (WEFT-239).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloudProviderRouteError {
    /// Which field failed (`stt_provider` or `tts_provider`).
    pub field: &'static str,
    /// The raw config string that was not recognized.
    pub value: String,
}

impl std::fmt::Display for CloudProviderRouteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "unknown cloud {} value {:?}; expected one of: {}",
            self.field,
            self.value,
            match self.field {
                "stt_provider" => "whisper | openai | openai-whisper",
                "tts_provider" => "openai | openai-tts | elevenlabs",
                _ => "see docs",
            }
        )
    }
}

impl std::error::Error for CloudProviderRouteError {}

/// Parse a config-string STT provider (WEFT-239).
///
/// Empty string returns `None` (no cloud STT selected).
pub fn parse_cloud_stt_provider(
    raw: &str,
) -> Result<Option<CloudSttProviderKind>, CloudProviderRouteError> {
    let key = raw.trim().to_ascii_lowercase();
    if key.is_empty() {
        return Ok(None);
    }
    match key.as_str() {
        "whisper" | "openai" | "openai-whisper" | "openai_whisper" | "openai.whisper" => {
            Ok(Some(CloudSttProviderKind::OpenAiWhisper))
        }
        _ => Err(CloudProviderRouteError {
            field: "stt_provider",
            value: raw.to_string(),
        }),
    }
}

/// Parse a config-string TTS provider (WEFT-239).
///
/// Empty string returns `None` (no cloud TTS selected).
pub fn parse_cloud_tts_provider(
    raw: &str,
) -> Result<Option<CloudTtsProviderKind>, CloudProviderRouteError> {
    let key = raw.trim().to_ascii_lowercase();
    if key.is_empty() {
        return Ok(None);
    }
    match key.as_str() {
        "openai" | "openai-tts" | "openai_tts" | "openai.tts" => {
            Ok(Some(CloudTtsProviderKind::OpenAi))
        }
        "elevenlabs" | "11labs" | "eleven_labs" | "eleven-labs" => {
            Ok(Some(CloudTtsProviderKind::ElevenLabs))
        }
        _ => Err(CloudProviderRouteError {
            field: "tts_provider",
            value: raw.to_string(),
        }),
    }
}

/// Cloud STT/TTS fallback configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CloudFallbackConfig {
    /// Enable cloud fallback when local models fail.
    #[serde(default)]
    pub enabled: bool,

    /// Cloud STT provider string (e.g. `"whisper"` for OpenAI Whisper API).
    ///
    /// Resolved via [`CloudFallbackConfig::resolve_stt_provider`] (WEFT-239).
    #[serde(default, alias = "sttProvider")]
    pub stt_provider: String,

    /// Cloud TTS provider string (`"openai"` or `"elevenlabs"`).
    ///
    /// Resolved via [`CloudFallbackConfig::resolve_tts_provider`] (WEFT-239).
    #[serde(default, alias = "ttsProvider")]
    pub tts_provider: String,
}

impl CloudFallbackConfig {
    /// Map `stt_provider` to a concrete provider kind.
    ///
    /// - Empty + `enabled == false` → `Ok(None)`
    /// - Empty + `enabled == true` → default [`CloudSttProviderKind::OpenAiWhisper`]
    /// - Non-empty unknown → `Err` (fail at startup)
    pub fn resolve_stt_provider(
        &self,
    ) -> Result<Option<CloudSttProviderKind>, CloudProviderRouteError> {
        match parse_cloud_stt_provider(&self.stt_provider)? {
            Some(kind) => Ok(Some(kind)),
            None if self.enabled => Ok(Some(CloudSttProviderKind::OpenAiWhisper)),
            None => Ok(None),
        }
    }

    /// Map `tts_provider` to a concrete provider kind.
    ///
    /// - Empty + `enabled == false` → `Ok(None)`
    /// - Empty + `enabled == true` → default [`CloudTtsProviderKind::OpenAi`]
    /// - Non-empty unknown → `Err` (fail at startup)
    pub fn resolve_tts_provider(
        &self,
    ) -> Result<Option<CloudTtsProviderKind>, CloudProviderRouteError> {
        match parse_cloud_tts_provider(&self.tts_provider)? {
            Some(kind) => Ok(Some(kind)),
            None if self.enabled => Ok(Some(CloudTtsProviderKind::OpenAi)),
            None => Ok(None),
        }
    }

    /// Validate both provider strings. Call at config load / startup.
    pub fn validate_providers(&self) -> Result<(), CloudProviderRouteError> {
        self.resolve_stt_provider()?;
        self.resolve_tts_provider()?;
        Ok(())
    }
}

/// Voice confirmation gate configuration (SC-6 / WEFT-225).
///
/// Controls the anti-replay nonce and transcription-echo pattern used
/// when a voice command (or other destructive action) requires spoken
/// confirmation before execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoiceConfirmationConfig {
    /// Seconds to wait for a spoken confirmation before cancelling.
    /// Spec default: 10.
    #[serde(default = "default_confirmation_timeout_seconds", alias = "timeoutSeconds")]
    pub timeout_seconds: u32,

    /// Include a one-shot nonce word in the confirm prompt
    /// ("Say 'confirm delta' to proceed."). Default true.
    #[serde(default = "default_true", alias = "antiReplayNonce")]
    pub anti_replay_nonce: bool,

    /// Echo the heard transcription before asking for confirmation
    /// ("I heard: '…'. Is that correct?"). Default true.
    #[serde(default = "default_true", alias = "transcriptionEcho")]
    pub transcription_echo: bool,
}

fn default_confirmation_timeout_seconds() -> u32 {
    10
}

fn default_true() -> bool {
    true
}

impl Default for VoiceConfirmationConfig {
    fn default() -> Self {
        Self {
            timeout_seconds: default_confirmation_timeout_seconds(),
            anti_replay_nonce: true,
            transcription_echo: true,
        }
    }
}

/// Voice rate-limit configuration (SC-8 / WEFT-226).
///
/// Token-bucket caps and the post-fail cooldown that protects against
/// ambient-noise flooding and adversarial rapid-fire wake/command loops.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoiceRateLimitConfig {
    /// Max voice commands accepted per minute (token bucket capacity +
    /// refill rate). Spec default: 10.
    #[serde(
        default = "default_commands_per_minute",
        alias = "commandsPerMinute"
    )]
    pub commands_per_minute: u32,

    /// Max wake-word activations accepted per minute. Spec default: 5.
    #[serde(
        default = "default_wake_activations_per_minute",
        alias = "wakeActivationsPerMinute"
    )]
    pub wake_activations_per_minute: u32,

    /// Consecutive failed command attempts that trip the post-fail
    /// cooldown. Spec default: 3.
    #[serde(default = "default_fail_threshold", alias = "failThreshold")]
    pub fail_threshold: u32,

    /// Cooldown duration (seconds) after the fail threshold is reached.
    /// Spec default: 30.
    #[serde(
        default = "default_post_fail_cooldown_seconds",
        alias = "postFailCooldownSeconds"
    )]
    pub post_fail_cooldown_seconds: u32,
}

fn default_commands_per_minute() -> u32 {
    10
}
fn default_wake_activations_per_minute() -> u32 {
    5
}
fn default_fail_threshold() -> u32 {
    3
}
fn default_post_fail_cooldown_seconds() -> u32 {
    30
}

impl Default for VoiceRateLimitConfig {
    fn default() -> Self {
        Self {
            commands_per_minute: default_commands_per_minute(),
            wake_activations_per_minute: default_wake_activations_per_minute(),
            fail_threshold: default_fail_threshold(),
            post_fail_cooldown_seconds: default_post_fail_cooldown_seconds(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voice_mode_prefers_xai_only_for_xai_variants() {
        // Exhaustive match (no wildcard): a new variant fails to compile here.
        for mode in [VoiceMode::Local, VoiceMode::XaiS2s, VoiceMode::XaiHybrid] {
            let expected = match mode {
                VoiceMode::Local => false,
                VoiceMode::XaiS2s | VoiceMode::XaiHybrid => true,
            };
            assert_eq!(mode.prefers_xai(), expected, "{mode:?}");
        }
    }

    #[test]
    fn resolved_voice_transport_is_xai_only_for_xai_variants() {
        // Exhaustive match (no wildcard): a new variant fails to compile here.
        for transport in [
            ResolvedVoiceTransport::Local,
            ResolvedVoiceTransport::XaiS2s,
            ResolvedVoiceTransport::XaiHybrid,
        ] {
            let expected = match transport {
                ResolvedVoiceTransport::Local => false,
                ResolvedVoiceTransport::XaiS2s | ResolvedVoiceTransport::XaiHybrid => true,
            };
            assert_eq!(transport.is_xai(), expected, "{transport:?}");
        }
    }

    #[test]
    fn voice_mode_serde_snake_case() {
        assert_eq!(
            serde_json::to_string(&VoiceMode::Local).unwrap(),
            "\"local\""
        );
        assert_eq!(
            serde_json::to_string(&VoiceMode::XaiS2s).unwrap(),
            "\"xai_s2s\""
        );
        assert_eq!(
            serde_json::to_string(&VoiceMode::XaiHybrid).unwrap(),
            "\"xai_hybrid\""
        );
        let m: VoiceMode = serde_json::from_str("\"xai_s2s\"").unwrap();
        assert_eq!(m, VoiceMode::XaiS2s);
    }

    #[test]
    fn voice_xai_config_defaults_and_url() {
        let xai = VoiceXaiConfig::default();
        assert_eq!(xai.api_key_env, DEFAULT_XAI_API_KEY_ENV);
        assert_eq!(xai.realtime_model, DEFAULT_XAI_REALTIME_MODEL);
        assert_eq!(xai.endpoint, DEFAULT_XAI_REALTIME_ENDPOINT);
        assert_eq!(
            xai.realtime_url(),
            "wss://api.x.ai/v1/realtime?model=grok-voice-latest"
        );

        let custom = VoiceXaiConfig {
            api_key_env: String::new(),
            realtime_model: "grok-voice-think-fast-2.0".into(),
            endpoint: "wss://api.x.ai/v1/realtime".into(),
        };
        assert_eq!(custom.api_key_env_name(), DEFAULT_XAI_API_KEY_ENV);
        assert_eq!(
            custom.realtime_url(),
            "wss://api.x.ai/v1/realtime?model=grok-voice-think-fast-2.0"
        );
    }

    #[test]
    fn voice_confirmation_and_rate_limit_defaults() {
        let cfg: VoiceConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(cfg.confirmation.timeout_seconds, 10);
        assert!(cfg.confirmation.anti_replay_nonce);
        assert!(cfg.confirmation.transcription_echo);
        assert_eq!(cfg.rate_limit.commands_per_minute, 10);
        assert_eq!(cfg.rate_limit.wake_activations_per_minute, 5);
        assert_eq!(cfg.rate_limit.fail_threshold, 3);
        assert_eq!(cfg.rate_limit.post_fail_cooldown_seconds, 30);

        let custom: VoiceConfig = serde_json::from_str(
            r#"{
                "confirmation": {"timeoutSeconds": 15, "antiReplayNonce": false},
                "rateLimit": {"commandsPerMinute": 20, "wakeActivationsPerMinute": 2}
            }"#,
        )
        .unwrap();
        assert_eq!(custom.confirmation.timeout_seconds, 15);
        assert!(!custom.confirmation.anti_replay_nonce);
        assert!(custom.confirmation.transcription_echo); // default
        assert_eq!(custom.rate_limit.commands_per_minute, 20);
        assert_eq!(custom.rate_limit.wake_activations_per_minute, 2);
    }

    #[test]
    fn voice_config_parses_mode_and_xai() {
        let json = r#"{
            "enabled": true,
            "mode": "local",
            "xai": {
                "api_key_env": "MY_XAI_KEY",
                "realtime_model": "grok-voice-latest",
                "endpoint": "wss://api.x.ai/v1/realtime"
            }
        }"#;
        let cfg: VoiceConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.mode, VoiceMode::Local);
        assert_eq!(cfg.xai.api_key_env, "MY_XAI_KEY");
        assert_eq!(cfg.xai.realtime_model, "grok-voice-latest");

        // camelCase aliases
        let camel = r#"{
            "mode": "xai_hybrid",
            "xai": { "apiKeyEnv": "XAI_API_KEY", "realtimeModel": "grok-voice-latest" }
        }"#;
        let cfg2: VoiceConfig = serde_json::from_str(camel).unwrap();
        assert_eq!(cfg2.mode, VoiceMode::XaiHybrid);
        assert_eq!(cfg2.xai.api_key_env, "XAI_API_KEY");
    }

    #[test]
    fn selection_mode_local_always_local() {
        let d = resolve_voice_transport(VoiceMode::Local, true, true);
        assert_eq!(d.transport, ResolvedVoiceTransport::Local);
        assert!(d.notice.is_none());
        assert!(d.fallback_reason.is_none());
        assert!(!d.is_fallback());
        assert_eq!(d.hybrid, HybridVoiceComposition::LOCAL_ALL);

        // Even without key / unhealthy, local stays local with no notice.
        let d2 = resolve_voice_transport(VoiceMode::Local, false, false);
        assert_eq!(d2.transport, ResolvedVoiceTransport::Local);
        assert!(d2.notice.is_none());
        assert!(d2.fallback_reason.is_none());
    }

    #[test]
    fn selection_fail_open_no_key() {
        let d = resolve_voice_transport(VoiceMode::XaiS2s, false, true);
        assert_eq!(d.transport, ResolvedVoiceTransport::Local);
        assert_eq!(d.notice, Some(VOICE_NOTICE_XAI_NO_KEY));
        assert_eq!(d.fallback_reason, Some(VoiceFallbackReason::NoApiKey));
        assert!(d.is_fallback());

        let d2 = resolve_voice_transport(VoiceMode::XaiHybrid, false, true);
        assert_eq!(d2.transport, ResolvedVoiceTransport::Local);
        assert_eq!(d2.notice, Some(VOICE_NOTICE_XAI_NO_KEY));
        assert_eq!(d2.fallback_reason, Some(VoiceFallbackReason::NoApiKey));
        assert_eq!(d2.hybrid, HybridVoiceComposition::LOCAL_ALL);
    }

    #[test]
    fn selection_fail_open_unhealthy() {
        let d = resolve_voice_transport(VoiceMode::XaiS2s, true, false);
        assert_eq!(d.transport, ResolvedVoiceTransport::Local);
        assert_eq!(d.notice, Some(VOICE_NOTICE_XAI_UNHEALTHY));
        assert_eq!(d.fallback_reason, Some(VoiceFallbackReason::HealthFailed));
    }

    #[test]
    fn hybrid_falls_back_local_when_xai_unhealthy() {
        // WEFT-691: hybrid mode must fail open to local STT+agent+TTS.
        let d = resolve_voice_transport(VoiceMode::XaiHybrid, true, false);
        assert_eq!(d.transport, ResolvedVoiceTransport::Local);
        assert_eq!(d.notice, Some(VOICE_NOTICE_XAI_UNHEALTHY));
        assert_eq!(d.fallback_reason, Some(VoiceFallbackReason::HealthFailed));
        assert_eq!(d.hybrid, HybridVoiceComposition::LOCAL_ALL);
        assert!(!d.hybrid.uses_xai());
    }

    #[test]
    fn selection_xai_path_when_key_and_healthy() {
        let d = resolve_voice_transport(VoiceMode::XaiS2s, true, true);
        assert_eq!(d.transport, ResolvedVoiceTransport::XaiS2s);
        assert!(d.notice.is_none());
        assert!(d.fallback_reason.is_none());
        assert!(d.hybrid.uses_xai());

        let d2 = resolve_voice_transport(VoiceMode::XaiHybrid, true, true);
        assert_eq!(d2.transport, ResolvedVoiceTransport::XaiHybrid);
        assert!(d2.notice.is_none());
        assert_eq!(d2.hybrid, HybridVoiceComposition::HYBRID_DEFAULT);
        assert_eq!(d2.hybrid.stt, HybridVoiceLeg::Local);
        assert_eq!(d2.hybrid.agent, HybridVoiceLeg::Local);
        assert_eq!(d2.hybrid.tts, HybridVoiceLeg::Xai);
        assert!(d2.hybrid.uses_xai());
    }

    #[test]
    fn resolve_with_health_probe_stub() {
        let healthy = XaiHealthProbeResult::healthy_stub();
        let d = resolve_voice_transport_with_health(VoiceMode::XaiHybrid, true, &healthy);
        assert_eq!(d.transport, ResolvedVoiceTransport::XaiHybrid);

        let sick = XaiHealthProbeResult::unhealthy_stub();
        let d2 = resolve_voice_transport_with_health(VoiceMode::XaiHybrid, true, &sick);
        assert_eq!(d2.transport, ResolvedVoiceTransport::Local);
        assert_eq!(d2.fallback_reason, Some(VoiceFallbackReason::HealthFailed));
    }

    #[test]
    fn runtime_fallback_always_local_with_notice() {
        for mode in [VoiceMode::Local, VoiceMode::XaiS2s, VoiceMode::XaiHybrid] {
            let d = resolve_voice_transport_runtime_fallback(mode);
            assert_eq!(d.transport, ResolvedVoiceTransport::Local);
            assert_eq!(d.notice, Some(VOICE_NOTICE_XAI_RUNTIME_FALLBACK));
            assert_eq!(d.fallback_reason, Some(VoiceFallbackReason::RuntimeFailure));
            assert_eq!(d.hybrid, HybridVoiceComposition::LOCAL_ALL);
            assert!(d.is_fallback());
        }
    }

    #[test]
    fn resolve_transport_uses_env_lookup_mock() {
        let cfg = VoiceConfig {
            mode: VoiceMode::XaiS2s,
            xai: VoiceXaiConfig::default(),
            ..Default::default()
        };

        let no_key = cfg.resolve_transport(|_| None, true);
        assert_eq!(no_key.transport, ResolvedVoiceTransport::Local);
        assert_eq!(no_key.notice, Some(VOICE_NOTICE_XAI_NO_KEY));

        let empty_key = cfg.resolve_transport(|_| Some(String::new()), true);
        assert_eq!(empty_key.transport, ResolvedVoiceTransport::Local);

        let present = cfg.resolve_transport(|_| Some("xai-test-key".into()), true);
        assert_eq!(present.transport, ResolvedVoiceTransport::XaiS2s);
        assert!(present.notice.is_none());

        // Custom env name
        let mut custom = cfg.clone();
        custom.xai.api_key_env = "CUSTOM_XAI".into();
        let mut saw = String::new();
        let d = custom.resolve_transport(
            |name| {
                saw = name.to_string();
                Some("k".into())
            },
            true,
        );
        assert_eq!(saw, "CUSTOM_XAI");
        assert_eq!(d.transport, ResolvedVoiceTransport::XaiS2s);

        let health = XaiHealthProbeResult::from_flag(false);
        let sick = cfg.resolve_transport_with_health(|_| Some("k".into()), &health);
        assert_eq!(sick.fallback_reason, Some(VoiceFallbackReason::HealthFailed));
    }

    #[test]
    fn voice_config_default_mode_is_xai_s2s() {
        let cfg = VoiceConfig::default();
        assert_eq!(cfg.mode, VoiceMode::XaiS2s);
        assert_eq!(cfg.xai.api_key_env, DEFAULT_XAI_API_KEY_ENV);
    }

    /// WEFT-222: two configured personalities resolve distinct voice_id/speed/pitch.
    #[test]
    fn tts_fast_engine_defaults_to_auto() {
        let cfg = TtsConfig::default();
        assert_eq!(cfg.fast_engine, DEFAULT_TTS_FAST_ENGINE);
        let json = r#"{"enabled":true}"#;
        let parsed: TtsConfig = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.fast_engine, "auto");
        let camel = r#"{"fastEngine":"chatterbox"}"#;
        let c: TtsConfig = serde_json::from_str(camel).unwrap();
        assert_eq!(c.fast_engine, "chatterbox");
    }

    #[test]
    fn resolve_tts_for_agent_two_personalities() {
        let mut cfg = VoiceConfig::default();
        cfg.personalities.insert(
            "agent-alpha".into(),
            VoicePersonality {
                voice_id: "nova".into(),
                provider: "openai".into(),
                speed: 1.1,
                pitch: 0.2,
                greeting_prefix: Some("This is Alpha.".into()),
                language: "en".into(),
            },
        );
        cfg.personalities.insert(
            "agent-beta".into(),
            VoicePersonality {
                voice_id: "onyx".into(),
                provider: "openai".into(),
                speed: 0.9,
                pitch: -0.3,
                greeting_prefix: Some("Beta online.".into()),
                language: "en-GB".into(),
            },
        );

        let alpha = cfg.resolve_tts_for_agent("agent-alpha");
        let beta = cfg.resolve_tts_for_agent("agent-beta");
        assert!(alpha.matched_agent && beta.matched_agent);
        assert_eq!(alpha.voice_id, "nova");
        assert_eq!(beta.voice_id, "onyx");
        assert!((alpha.speed - 1.1).abs() < f32::EPSILON);
        assert!((beta.speed - 0.9).abs() < f32::EPSILON);
        assert!((alpha.pitch - 0.2).abs() < f32::EPSILON);
        assert!((beta.pitch - (-0.3)).abs() < f32::EPSILON);
        assert_ne!(alpha.voice_id, beta.voice_id);
        assert_ne!(alpha.pitch, beta.pitch);

        assert_eq!(
            cfg.session_greeting_for("agent-alpha").as_deref(),
            Some("This is Alpha.")
        );
        assert_eq!(
            cfg.session_greeting_for("agent-beta").as_deref(),
            Some("Beta online.")
        );

        // Session-start text prefixes greeting; later turns do not.
        assert_eq!(
            cfg.tts_text_for("agent-alpha", "Ready.", true),
            "This is Alpha. Ready."
        );
        assert_eq!(cfg.tts_text_for("agent-alpha", "Ready.", false), "Ready.");
        assert_eq!(
            cfg.tts_text_for("agent-beta", "", true),
            "Beta online."
        );

        // Unknown agent: no map match, no greeting; global tts speed applied.
        cfg.tts.voice = "alloy".into();
        cfg.tts.speed = 1.25;
        let unknown = cfg.resolve_tts_for_agent("agent-gamma");
        assert!(!unknown.matched_agent);
        assert_eq!(unknown.voice_id, "alloy");
        assert!((unknown.speed - 1.25).abs() < f32::EPSILON);
        assert!(cfg.session_greeting_for("agent-gamma").is_none());

        assert!(cfg.validate_personalities().is_ok());
    }

    #[test]
    fn validate_personalities_rejects_bad_entry() {
        let mut cfg = VoiceConfig::default();
        cfg.personalities.insert(
            "bad".into(),
            VoicePersonality {
                speed: 9.0,
                ..Default::default()
            },
        );
        let err = cfg.validate_personalities().unwrap_err();
        assert!(err.contains("bad"));
        assert!(err.contains("speed"));
    }

    // ── WEFT-239: CloudFallbackConfig provider router ─────────────────────

    #[test]
    fn cloud_stt_provider_aliases() {
        for alias in ["whisper", "openai", "openai-whisper", "OpenAI_Whisper"] {
            assert_eq!(
                parse_cloud_stt_provider(alias).unwrap(),
                Some(CloudSttProviderKind::OpenAiWhisper)
            );
        }
        assert_eq!(parse_cloud_stt_provider("").unwrap(), None);
        assert_eq!(parse_cloud_stt_provider("  ").unwrap(), None);
        let err = parse_cloud_stt_provider("azure-speech").unwrap_err();
        assert_eq!(err.field, "stt_provider");
        assert!(err.to_string().contains("azure-speech"));
    }

    #[test]
    fn cloud_tts_provider_aliases() {
        assert_eq!(
            parse_cloud_tts_provider("openai").unwrap(),
            Some(CloudTtsProviderKind::OpenAi)
        );
        assert_eq!(
            parse_cloud_tts_provider("openai-tts").unwrap(),
            Some(CloudTtsProviderKind::OpenAi)
        );
        assert_eq!(
            parse_cloud_tts_provider("elevenlabs").unwrap(),
            Some(CloudTtsProviderKind::ElevenLabs)
        );
        assert_eq!(
            parse_cloud_tts_provider("11labs").unwrap(),
            Some(CloudTtsProviderKind::ElevenLabs)
        );
        let err = parse_cloud_tts_provider("polly").unwrap_err();
        assert_eq!(err.field, "tts_provider");
    }

    #[test]
    fn cloud_fallback_resolve_defaults_when_enabled() {
        let cfg = CloudFallbackConfig {
            enabled: true,
            stt_provider: String::new(),
            tts_provider: String::new(),
        };
        assert_eq!(
            cfg.resolve_stt_provider().unwrap(),
            Some(CloudSttProviderKind::OpenAiWhisper)
        );
        assert_eq!(
            cfg.resolve_tts_provider().unwrap(),
            Some(CloudTtsProviderKind::OpenAi)
        );
        assert!(cfg.validate_providers().is_ok());
    }

    #[test]
    fn cloud_fallback_resolve_disabled_empty_is_none() {
        let cfg = CloudFallbackConfig::default();
        assert!(!cfg.enabled);
        assert_eq!(cfg.resolve_stt_provider().unwrap(), None);
        assert_eq!(cfg.resolve_tts_provider().unwrap(), None);
    }

    #[test]
    fn cloud_fallback_unknown_provider_errors_at_startup() {
        let cfg = CloudFallbackConfig {
            enabled: true,
            stt_provider: "not-a-provider".into(),
            tts_provider: "openai".into(),
        };
        assert!(cfg.validate_providers().is_err());
    }

    // ── WEFT-240: sensitivity ↔ detector threshold ────────────────────────

    #[test]
    fn wake_sensitivity_to_detector_threshold_inverts() {
        let cfg = WakeConfig {
            sensitivity: 0.8,
            ..Default::default()
        };
        assert!((cfg.to_detector_threshold() - 0.2).abs() < f32::EPSILON);
        assert!(
            (WakeConfig::sensitivity_from_detector_threshold(0.2) - 0.8).abs() < f32::EPSILON
        );
        let mid = WakeConfig::default();
        assert!((mid.to_detector_threshold() - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn wake_config_threshold_alias_deserializes_as_sensitivity() {
        let json = r#"{"enabled": true, "threshold": 0.7}"#;
        let cfg: WakeConfig = serde_json::from_str(json).unwrap();
        assert!((cfg.sensitivity - 0.7).abs() < f32::EPSILON);
    }

    #[test]
    fn wake_config_model_path_buf_bridge() {
        let empty = WakeConfig::default();
        assert!(empty.model_path_buf().is_none());
        let cfg = WakeConfig {
            model_path: Some("models/voice/wake/hey-weft.rpw".into()),
            ..Default::default()
        };
        assert_eq!(
            cfg.model_path_buf().unwrap(),
            std::path::PathBuf::from("models/voice/wake/hey-weft.rpw")
        );
    }

    #[test]
    fn tts_provider_predicates_partition_known_values() {
        use tts_providers::*;
        // (value, local, browser, cloud)
        let cases = [
            (LOCAL, true, false, false),
            (LOCAL_STUB, true, false, false),
            (BROWSER, false, true, false),
            (OPENAI, false, false, true),
            (ELEVENLABS, false, false, true),
            ("nope", false, false, false),
            ("", false, false, false),
            ("LOCAL", false, false, false),
        ];
        for (value, local, browser, cloud) in cases {
            assert_eq!(is_local_tts_provider(value), local, "local: {value:?}");
            assert_eq!(is_browser_tts_provider(value), browser, "browser: {value:?}");
            assert_eq!(is_cloud_tts_provider(value), cloud, "cloud: {value:?}");
            let hits = [local, browser, cloud].iter().filter(|b| **b).count();
            assert!(hits <= 1, "predicate sets overlap for {value:?}");
        }
    }
}
