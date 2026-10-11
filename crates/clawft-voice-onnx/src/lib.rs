//! Native-Rust ONNX voice models for WeftOS.
//!
//! This crate hosts the cross-platform, backend-agnostic voice models that run
//! locally via [`ort`](https://docs.rs/ort) (ONNX Runtime), keeping
//! `clawft-channels` free of model/runtime weight. It is the audio sibling of
//! the text embedders in `clawft-kernel` (`embedding_qwen3` / `embedding_onnx`)
//! and follows the same idiom: a feature flag (`onnx`) gates `ort`, the front-end
//! ("input prep") is pure Rust, and inference degrades gracefully when weights
//! are absent so default builds and unit tests need no model files.
//!
//! # Models
//!
//! - [`EcapaEmbedder`] — ECAPA-TDNN speaker embedding (192-d d-vectors),
//!   satisfying [`clawft_channels::voice::speaker::SpeakerEmbedder`]. Front-end:
//!   [`Fbank`] (SpeechBrain-parity 80-dim log-mel).
//! - [`SmartTurnEndpoint`] — smart-turn-v3 semantic endpointer (Whisper-Tiny
//!   encoder + linear head), satisfying
//!   [`clawft_channels::voice::turn::EndpointModel`]. Front-end: [`WhisperMel`]
//!   (Slaney log-mel). Falls back to the no-model heuristic without weights.
//! - [`ParakeetStt`] — parakeet-tdt-0.6b speech-to-text (NeMo FastConformer +
//!   TDT transducer greedy decode), satisfying
//!   [`clawft_channels::voice::stt::SttBackend`]. Front-end: [`NemoMel`]
//!   (NeMo 128-dim log-mel).
//! - [`SileroVoiceness`] — Silero VAD v5 neural voice-activity detector,
//!   satisfying [`clawft_channels::voice::voiceness::Voiceness`]. Stateful
//!   512-sample windows with recurrent h/c state; falls back to
//!   [`SpectralVoiceness`](clawft_channels::voice::voiceness::SpectralVoiceness)
//!   when weights are absent. Select via [`preferred_voiceness`].
//!
//! # Feature flags
//!
//! - `onnx` — pull in `ort` for real inference. Without it the front-end still
//!   compiles and is unit-tested; embedders return a clear "model unavailable"
//!   [`clawft_channels::voice::types::VoiceError`].

pub mod ecapa;
pub mod fbank;
pub mod nemo_mel;
pub mod parakeet;
#[cfg(feature = "onnx")]
mod parakeet_tdt;
pub mod silero_vad;
pub mod smart_turn;
pub mod turn_compare;
pub mod turn_compare_report;
pub mod wav_input;
pub mod whisper_mel;

pub use ecapa::{EMBED_DIM, EcapaEmbedder};
pub use fbank::{Fbank, N_MELS, SAMPLE_RATE};
pub use nemo_mel::NemoMel;
pub use parakeet::ParakeetStt;
pub use silero_vad::{
    CATALOG_ID as SILERO_CATALOG_ID, MODEL_ENV as SILERO_MODEL_ENV, MODEL_FILE as SILERO_MODEL_FILE,
    SileroVoiceness, default_model_path as silero_default_model_path, home_model_dir as silero_home_model_dir,
    preferred_voiceness, stage_model_file as stage_silero_model_file,
};
pub use smart_turn::SmartTurnEndpoint;
pub use whisper_mel::WhisperMel;
