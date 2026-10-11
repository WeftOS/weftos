//! Native-Rust TTS node-renderers for WeftOS — **no Python** (ADR-062 Phase 4).
//!
//! ADR-062 D3 maps a voice turn to two self-branches: a **Speculative** node
//! rendered immediately by a *fast* engine (latency cover), superseded by a
//! **Committed** node rendered by an expressive *slow* engine. This crate hosts
//! the two concrete engines behind the generic
//! [`clawft_channels::voice::tts::TtsEngine`] seam, plus the SNAC audio-codec
//! decoder the slow engine streams through. It is the TTS sibling of the STT /
//! speaker / endpoint models in `clawft-voice-onnx` and follows the same idiom:
//! a feature flag (`onnx`) gates `ort`, the token/frame math is pure Rust and
//! unit-tested, and inference degrades gracefully when weights or the Ollama
//! runtime are absent so default builds and unit tests need no model files.
//!
//! # Engines
//!
//! - [`OrpheusTts`] — **slow** layer (`TtsTier::Slow`). Drives Orpheus token
//!   generation over the **Ollama** HTTP streaming API
//!   (`POST /api/generate`, `raw`+`stream`), parses the `<custom_token_N>`
//!   stream, and decodes it to gap-free PCM through a [`SnacDecode`]. Performs
//!   paralinguistic tags (`<laugh>`) literally — text is never scrubbed.
//! - [`KokoroTts`] — **fast** layer (`TtsTier::Fast`) **0.8 default**. A native
//!   ONNX TTS for the Speculative ack (<1 s TTFA). Generic preset voice
//!   (timbre may shift vs Orpheus).
//! - [`ChatterboxTts`] — **fast** layer scaffold for the voicelab **cloned**
//!   voice (david / `am_onyx`). Bundle discovery + readiness gate only;
//!   native inference is **not** implemented in 0.8 (WEFT-613 → 0.9.x).
//! - [`fast_tier`] — selects clone path when runtime-ready, else Kokoro
//!   (`WEFTOS_FAST_TTS` / `voice.tts.fast_engine`).
//!
//! # SNAC
//!
//! [`SnacDecode`] is the seam the slow engine renders through; [`SnacOnnxDecoder`]
//! is the concrete SNAC-24kHz `ort` decoder. The Orpheus→SNAC token math
//! ([`parse_custom_token`], [`SnacDecode::decode_frames`]'s frame regroup) is
//! pure Rust and unit-tested without any model.
//!
//! # Feature flags
//!
//! - `pocket-tts` — Pocket TTS via Gradium `ptts` ([`PocketTts`], trial, off by default).
//! - `onnx` — pull in `ort` for real SNAC + Kokoro inference. Without it the
//!   token/frame front-end still compiles and is unit-tested; the decoders
//!   return a clear [`clawft_channels::voice::types::VoiceError`]. Chatterbox
//!   inference (when landed) will use the same gate.

pub mod chatterbox;
pub mod fast_tier;
pub mod kokoro;
pub mod orpheus;
pub mod pocket_tts;
pub mod pocket_tts_bench;
#[cfg(feature = "pocket-tts")]
mod pocket_tts_ptts;
pub mod snac;

pub use chatterbox::{
    CHATTERBOX_INFERENCE_IMPLEMENTED, CHATTERBOX_SAMPLE_RATE, ChatterboxTts, DEFAULT_CLONE_VOICE_ID,
};
pub use fast_tier::{
    FastEnginePreference, FastTierSelection, NativeFastEngineKind, build_native_fast_engine,
    build_native_fast_engine_with, resolve_fast_engine, resolve_fast_engine_with,
    select_native_fast_tier,
};
pub use kokoro::KokoroTts;
pub use pocket_tts::{PocketTts, PocketTtsConfig};
pub use orpheus::{OrpheusTts, parse_custom_token};
pub use snac::{SnacDecode, SnacOnnxDecoder};
