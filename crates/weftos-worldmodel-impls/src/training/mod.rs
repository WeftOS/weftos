//! LeWM training surfaces + RVF-hosted small models (WEFT-531 / WEFT-532).
//!
//! # Surfaces
//!
//! | Surface | Type | Role |
//! |---------|------|------|
//! | Offline edge | [`OfflineEdgePipeline`] | Per-sensor-class ridge fit of `ẑ = z + A·a + b` → RVF segment → tick hot-swap |
//! | Online streaming-merge | [`StreamingMergePipeline`] | Importance-weighted replay + normalised-LMS SGD + AND-gated promotion |
//!
//! # Small models (WEFT-532)
//!
//! [`SensorModelRegistry`] holds transmit-gate / aggregate / encode slots per
//! [`SensorClass`], stages RVF-delivered checkpoints, commits at tick
//! boundaries, and rolls back to last-good on AND-gate veto.
//!
//! Both surfaces train the same action-conditioned linear `pred_φ`
//! ([`ActionLinearModel`], pure Rust, `no_std`): closed-form ridge least
//! squares offline, one SGD step per batch online. The neural ViT-tiny /
//! AdaLN path lives under `crate::candle` (feature `candle`).

pub(crate) mod action_linear;
mod offline_edge;
mod replay;
mod rvf_segment;
mod sensor_models;
mod streaming_merge;

pub use action_linear::{
    ActionLinearModel, ACTION_LINEAR_BLOB_LEN, ACTION_LINEAR_FORMAT_VERSION,
    ACTION_LINEAR_HEADER_LEN, ACTION_LINEAR_MAGIC, DEFAULT_RIDGE_LAMBDA,
};
pub use offline_edge::OfflineEdgePipeline;
pub use replay::{ImportanceReplayBuffer, DEFAULT_REPLAY_CAPACITY};
pub use rvf_segment::{
    codec_for_weights, decode_model_segment, encode_model_segment, is_safetensors_blob, model_segment_from_wire, model_segment_to_wire,
    LewmModelSegment, SEGMENT_HEADER_LEN,
};
pub use sensor_models::{SensorModelRegistry, SlotState, DEFAULT_BOOT_VERSION};
pub use streaming_merge::{require_promoted, StreamingMergePipeline};
