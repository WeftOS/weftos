//! Plugin trait definitions for clawft.
//!
//! This crate defines the unified plugin architecture for the clawft AI
//! assistant framework. It provides trait definitions for tools, channels,
//! pipeline stages, skills, memory backends, and voice handlers -- the
//! six core extension points that all downstream feature work depends on.
//!
//! # Trait Overview
//!
//! | Trait | Purpose |
//! |-------|---------|
//! | [`Tool`] | Tool execution interface for agent capabilities |
//! | [`ChannelAdapter`] | Channel message handling for external platforms |
//! | [`PipelineStage`] | Processing stage in the agent pipeline |
//! | [`Skill`] | High-level agent capability with tools and instructions |
//! | [`MemoryBackend`] | Pluggable memory storage backend |
//! | [`VoiceHandler`] | Voice/audio processing — forward-compat placeholder, no impl in 0.7.x (Workstream G) |
//!
//! # Supporting Traits
//!
//! | Trait | Purpose |
//! |-------|---------|
//! | [`KeyValueStore`] | Key-value storage exposed to plugins via `ToolContext` |
//! | [`ToolContext`] | Execution context passed to tool/skill invocations |
//! | [`ChannelAdapterHost`] | Host services for channel adapters |
//!
//! # Plugin Manifest
//!
//! Plugins declare their capabilities, permissions, and resource limits
//! through a [`PluginManifest`], typically parsed from a JSON file
//! (`clawft.plugin.json`).
//!
//! # Feature Flags
//!
//! - `voice` -- Voice umbrella. Pulls in `voice-vad`, `voice-wake`,
//!   `voice-stt`, and `voice-tts` so `cargo build --features voice`
//!   compiles the full in-process **legacy scaffold** (WEFT-212).
//!   **Disposition (WEFT-671):** product Talk Mode / STT / TTS / AEC live in
//!   `clawft-channels::voice` + `clawft-voice-{talk,tts,onnx,aec}` and
//!   substrate `clawft-service-whisper` (ADR-053). Under `voice/`, only
//!   wake (`WakeDaemon` / `WakeWordConfig`) has a live CLI caller; the rest
//!   is deprecated scaffold. See `docs/plans/wave-0a-WEFT-671-decision.md`.
//! - `voice-vad` -- Voice Activity Detection (legacy Silero stub).
//! - `voice-stt` -- Speech-to-Text (legacy sherpa-rs stub).
//! - `voice-tts` -- Text-to-Speech (legacy sherpa-rs stub).
//! - `voice-wake` -- Wake-word detection (transitional CLI home; **stub
//!   backend**, WEFT-216). `process_frame` always returns false.
//! - `voice-wake-rustpotter` -- Experimental fail-closed reserve for a
//!   rustpotter path. Does **not** link rustpotter (3.0.2 / candle-core
//!   0.2.2 does not compile here). `try_with_rustpotter` returns
//!   `NotImplemented`. Concrete alternative: OpenWakeWord ONNX via
//!   `clawft-voice-onnx`. See `docs/plans/wave-0k-WEFT-216-result.md`.
//!
//! ## Crate Ecosystem
//!
//! WeftOS is built from these crates:
//!
//! | Crate | Role |
//! |-------|------|
//! | [`weftos`](https://crates.io/crates/weftos) | Product facade -- re-exports kernel, core, types |
//! | [`clawft-kernel`](https://crates.io/crates/clawft-kernel) | Kernel: processes, services, governance, mesh, ExoChain |
//! | [`clawft-core`](https://crates.io/crates/clawft-core) | Agent framework: pipeline, context, tools, skills |
//! | [`clawft-types`](https://crates.io/crates/clawft-types) | Shared type definitions |
//! | [`clawft-platform`](https://crates.io/crates/clawft-platform) | Platform abstraction (native/WASM/browser) |
//! | [`clawft-plugin`](https://crates.io/crates/clawft-plugin) | Plugin SDK for tools, channels, and extensions |
//! | [`clawft-llm`](https://crates.io/crates/clawft-llm) | LLM provider abstraction (11 providers + local) |
//! | [`exo-resource-tree`](https://crates.io/crates/exo-resource-tree) | Hierarchical resource namespace with Merkle integrity |
//!
//! Source: <https://github.com/WeftOS/weftos>

pub mod error;
pub mod manifest;
pub mod message;
pub mod metrics;
pub mod sandbox;
pub mod skill_grants;
pub mod traits;

#[cfg(feature = "voice")]
pub mod voice;

// Re-export core types at crate root for convenience.
pub use error::{PluginError, SkillLoadError, WasmHostError};
pub use manifest::{
    PermissionDiff, PluginCapability, PluginManifest, PluginPermissions, PluginResourceConfig,
    VoiceCapability, VoiceGrants, validate_voice_capability,
};
pub use message::MessagePayload;
pub use metrics::{
    EVENT_KIND_PLUGIN_WASM_INVOKE, PluginMetrics, PluginMetricsRegistry, default_metrics_dir,
    global_plugin_metrics, load_metrics, metrics_file_path, record_plugin_invocation,
    save_metrics,
};
pub use sandbox::{
    EnvPolicy, FilesystemPolicy, NetworkPolicy, ProcessPolicy, SandboxAuditEntry, SandboxPolicy,
    SandboxType,
};
pub use skill_grants::validate_allowed_tools;
pub use traits::{
    CancellationToken, ChannelAdapter, ChannelAdapterHost, KeyValueStore, MemoryBackend,
    PipelineStage, PipelineStageType, Skill, Tool, ToolContext, VoiceHandler,
};
