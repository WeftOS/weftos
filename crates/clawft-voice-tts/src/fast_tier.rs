//! Native Talk-Mode **fast-tier** engine selection (WEFT-613).
//!
//! ADR-061 wants a **cloned-voice** fast ack (voicelab Chatterbox / `am_onyx`).
//! The native 0.8 stack ships **Kokoro** as the working preset fast engine and
//! scaffolds Chatterbox discovery + readiness. This module is the single place
//! that decides which backend is bound into [`DualLayerTts`](clawft_channels::voice::tts::DualLayerTts).
//!
//! # Preference sources (first match wins for explicit force)
//!
//! 1. Explicit [`FastEnginePreference`] argument (from config / profile).
//! 2. Env `WEFTOS_FAST_TTS` = `auto` | `kokoro` | `chatterbox` (alias `clone`).
//! 3. Default: **`auto`** — use clone path when
//!    [`ChatterboxTts::is_runtime_available`], else Kokoro.
//!
//! # 0.8 reality
//!
//! [`CHATTERBOX_INFERENCE_IMPLEMENTED`](crate::chatterbox::CHATTERBOX_INFERENCE_IMPLEMENTED)
//! is `false`, so `auto` always resolves to Kokoro. Staging a bundle logs intent
//! but does not break the audio path. Full clone inference is **0.9.x**.

use std::sync::Arc;

use clawft_channels::voice::tts::TtsEngine;
use tracing::{info, warn};

use crate::chatterbox::ChatterboxTts;
use crate::kokoro::KokoroTts;
use crate::pocket_tts::{PocketTts, PocketTtsConfig};

/// Env var for fast-tier preference (david / operator override).
pub const FAST_TTS_ENV: &str = "WEFTOS_FAST_TTS";

/// Which native fast engine to bind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeFastEngineKind {
    /// Cloned-voice path (Chatterbox scaffold / future sherpa-onnx VC).
    ClonedVoice,
    /// Preset Kokoro ONNX — accepted 0.8 default.
    KokoroPreset,
    /// Pocket TTS via `ptts` (WEFT-657 trial, `pocket-tts` feature).
    PocketTts,
}

/// Operator / profile preference for the native fast tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FastEnginePreference {
    /// Prefer clone when runtime-ready; otherwise Kokoro (david intent).
    #[default]
    Auto,
    /// Always Kokoro (ignore staged chatterbox).
    Kokoro,
    /// Prefer Chatterbox; fall back to Kokoro if not runtime-ready.
    Chatterbox,
    /// Pocket TTS trial; Kokoro fallback when the feature/model is absent.
    PocketTts,
}

impl FastEnginePreference {
    /// Parse wire / env / config tokens.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" | "" => Some(Self::Auto),
            "kokoro" | "preset" => Some(Self::Kokoro),
            "chatterbox" | "clone" | "cloned" | "cloned_voice" => Some(Self::Chatterbox),
            "pocket" | "pocket-tts" | "pocket_tts" | "ptts" => Some(Self::PocketTts),
            _ => None,
        }
    }

    /// Config / docs wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Kokoro => "kokoro",
            Self::Chatterbox => "chatterbox",
            Self::PocketTts => "pocket-tts",
        }
    }

    /// Resolve from `WEFTOS_FAST_TTS`, defaulting to [`Self::Auto`].
    pub fn from_env() -> Self {
        match std::env::var(FAST_TTS_ENV) {
            Ok(v) => Self::parse(&v).unwrap_or_else(|| {
                warn!(
                    value = %v,
                    "unknown {FAST_TTS_ENV}; expected auto|kokoro|chatterbox — using auto"
                );
                Self::Auto
            }),
            Err(_) => Self::Auto,
        }
    }
}

/// Outcome of selection (testable pure decision + human reason).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FastTierSelection {
    pub kind: NativeFastEngineKind,
    pub preference: FastEnginePreference,
    /// Why this backend was chosen (logs / diagnostics).
    pub reason: &'static str,
}

/// Pure selection: given preference and whether clone inference is ready.
pub fn resolve_fast_engine(
    preference: FastEnginePreference,
    clone_runtime_ready: bool,
) -> FastTierSelection {
    resolve_fast_engine_with(preference, clone_runtime_ready, false)
}

/// As [`resolve_fast_engine`], also told whether Pocket TTS is runtime-ready
/// (feature compiled in and a model configured). `Auto` never picks Pocket TTS:
/// it is an explicit opt-in trial.
pub fn resolve_fast_engine_with(
    preference: FastEnginePreference,
    clone_runtime_ready: bool,
    pocket_ready: bool,
) -> FastTierSelection {
    match preference {
        FastEnginePreference::PocketTts if pocket_ready => FastTierSelection {
            kind: NativeFastEngineKind::PocketTts,
            preference,
            reason: "preference=pocket-tts and model ready",
        },
        FastEnginePreference::PocketTts => FastTierSelection {
            kind: NativeFastEngineKind::KokoroPreset,
            preference,
            reason: "preference=pocket-tts but feature/model not ready; Kokoro fallback",
        },
        FastEnginePreference::Kokoro => FastTierSelection {
            kind: NativeFastEngineKind::KokoroPreset,
            preference,
            reason: "preference=kokoro",
        },
        FastEnginePreference::Chatterbox if clone_runtime_ready => FastTierSelection {
            kind: NativeFastEngineKind::ClonedVoice,
            preference,
            reason: "preference=chatterbox and clone runtime ready",
        },
        FastEnginePreference::Chatterbox => FastTierSelection {
            kind: NativeFastEngineKind::KokoroPreset,
            preference,
            reason: "preference=chatterbox but clone not runtime-ready; Kokoro fallback (WEFT-613)",
        },
        FastEnginePreference::Auto if clone_runtime_ready => FastTierSelection {
            kind: NativeFastEngineKind::ClonedVoice,
            preference,
            reason: "auto: clone runtime ready (david clone path)",
        },
        FastEnginePreference::Auto => FastTierSelection {
            kind: NativeFastEngineKind::KokoroPreset,
            preference,
            reason: "auto: clone not runtime-ready; Kokoro preset (0.8 accepted default)",
        },
    }
}

/// Probe the default Chatterbox bundle and resolve preference from env.
pub fn select_native_fast_tier() -> FastTierSelection {
    select_native_fast_tier_with(FastEnginePreference::from_env(), ChatterboxTts::new())
}

/// Resolve with an explicit preference and Chatterbox probe instance.
pub fn select_native_fast_tier_with(
    preference: FastEnginePreference,
    chatterbox: ChatterboxTts,
) -> FastTierSelection {
    let sel = resolve_fast_engine_with(
        preference,
        chatterbox.is_runtime_available(),
        preference == FastEnginePreference::PocketTts && PocketTtsConfig::from_env().is_ready(),
    );
    if chatterbox.is_bundle_staged() && !chatterbox.is_runtime_available() {
        info!(
            target: "clawft_voice_tts::fast_tier",
            summary = %chatterbox.status_summary(),
            selected = ?sel.kind,
            "WEFT-613: chatterbox bundle staged; native clone inference deferred — {}",
            sel.reason
        );
    } else {
        info!(
            target: "clawft_voice_tts::fast_tier",
            selected = ?sel.kind,
            reason = sel.reason,
            "native fast-tier selection"
        );
    }
    sel
}

/// Build the concrete [`TtsEngine`] for the native dual-layer fast seat.
///
/// Always returns a Fast-tier engine. When the clone path is not ready,
/// returns [`KokoroTts`] (may itself be model-absent until weights are staged).
pub fn build_native_fast_engine() -> Arc<dyn TtsEngine> {
    build_native_fast_engine_with(FastEnginePreference::from_env())
}

/// Like [`build_native_fast_engine`] with an explicit preference (config hook).
pub fn build_native_fast_engine_with(preference: FastEnginePreference) -> Arc<dyn TtsEngine> {
    let chatterbox = ChatterboxTts::new();
    let sel = select_native_fast_tier_with(preference, chatterbox.clone());
    match sel.kind {
        NativeFastEngineKind::ClonedVoice => Arc::new(chatterbox),
        NativeFastEngineKind::KokoroPreset => Arc::new(KokoroTts::new()),
        NativeFastEngineKind::PocketTts => match PocketTts::load(&PocketTtsConfig::from_env()) {
            Ok(engine) => Arc::new(engine),
            Err(e) => {
                warn!(error = %e, "pocket-tts load failed; falling back to Kokoro");
                Arc::new(KokoroTts::new())
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clawft_channels::voice::tts::TtsTier;

    #[test]
    fn parse_preference_tokens() {
        assert_eq!(
            FastEnginePreference::parse("auto"),
            Some(FastEnginePreference::Auto)
        );
        assert_eq!(
            FastEnginePreference::parse("KOKORO"),
            Some(FastEnginePreference::Kokoro)
        );
        assert_eq!(
            FastEnginePreference::parse("chatterbox"),
            Some(FastEnginePreference::Chatterbox)
        );
        assert_eq!(
            FastEnginePreference::parse("clone"),
            Some(FastEnginePreference::Chatterbox)
        );
        assert_eq!(FastEnginePreference::parse("nope"), None);
    }

    #[test]
    fn pocket_tts_is_explicit_opt_in() {
        use FastEnginePreference as P;
        assert_eq!(P::parse("pocket-tts"), Some(P::PocketTts));
        assert_eq!(P::parse("ptts"), Some(P::PocketTts));
        let ready = resolve_fast_engine_with(P::PocketTts, false, true);
        assert_eq!(ready.kind, NativeFastEngineKind::PocketTts);
        let not_ready = resolve_fast_engine_with(P::PocketTts, true, false);
        assert_eq!(not_ready.kind, NativeFastEngineKind::KokoroPreset);
        // Auto never selects it, even when ready.
        let auto = resolve_fast_engine_with(P::Auto, false, true);
        assert_eq!(auto.kind, NativeFastEngineKind::KokoroPreset);
    }

    #[test]
    fn auto_prefers_clone_when_ready() {
        let s = resolve_fast_engine(FastEnginePreference::Auto, true);
        assert_eq!(s.kind, NativeFastEngineKind::ClonedVoice);
    }

    #[test]
    fn auto_falls_back_to_kokoro_when_not_ready() {
        let s = resolve_fast_engine(FastEnginePreference::Auto, false);
        assert_eq!(s.kind, NativeFastEngineKind::KokoroPreset);
        assert!(s.reason.contains("Kokoro"));
    }

    #[test]
    fn force_kokoro_ignores_ready_clone() {
        let s = resolve_fast_engine(FastEnginePreference::Kokoro, true);
        assert_eq!(s.kind, NativeFastEngineKind::KokoroPreset);
    }

    #[test]
    fn force_chatterbox_falls_back_when_not_ready() {
        let s = resolve_fast_engine(FastEnginePreference::Chatterbox, false);
        assert_eq!(s.kind, NativeFastEngineKind::KokoroPreset);
        assert!(s.reason.contains("WEFT-613") || s.reason.contains("fallback"));
    }

    #[test]
    fn default_build_is_fast_tier_kokoro_in_0_8() {
        // Inference gate is off → even Auto + staged bundle → Kokoro.
        let engine = build_native_fast_engine_with(FastEnginePreference::Auto);
        assert_eq!(engine.tier(), TtsTier::Fast);
        let sel = resolve_fast_engine(FastEnginePreference::Auto, false);
        assert_eq!(sel.kind, NativeFastEngineKind::KokoroPreset);
    }

    #[test]
    fn selection_with_unstaged_probe() {
        let probe = ChatterboxTts::with_dir("/nonexistent/weft613-fast-tier");
        let sel = select_native_fast_tier_with(FastEnginePreference::Auto, probe);
        assert_eq!(sel.kind, NativeFastEngineKind::KokoroPreset);
    }
}
