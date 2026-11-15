//! Training surfaces and RVF-hosted small-model contracts (WEFT-531 / WEFT-532).
//!
//! ADR diagram / research stream contracts:
//!
//! - **Two training surfaces** (T30 / D33):
//!   1. **Offline edge intelligence** — per-sensor-class, RVF-delivered,
//!      hot-swapped at tick alignment.
//!   2. **Online streaming-merge** — in-service trainer with per-class
//!      importance-weighted replay, gated by the four-condition AND rollback
//!      gate ([`crate::rollback_gate`]).
//! - **Three small models per sensor class** (T31 / D37):
//!   transmit-gate / aggregate / encode, RVF-hosted, tick-aligned hot-swap,
//!   auto-rollback on gate veto.
//!
//! Full neural training (ViT-tiny / AdaLN) remains residual; these traits
//! define the host surface so impls can ship stub trainers + segment I/O
//! and later swap in real optimisers.

mod sample;
mod sensor_class;
mod small_model;
mod traits;

pub use sample::{fnv1a64, ModelCheckpoint, TrainingSample};
pub use sensor_class::SensorClass;
pub use small_model::{
    SmallModelKind, LEWM_MODEL_SEGMENT_CODEC_JSON, LEWM_MODEL_SEGMENT_CODEC_RAW,
    LEWM_MODEL_SEGMENT_CODEC_SAFETENSORS, LEWM_MODEL_SEGMENT_DOMAIN_END,
    LEWM_MODEL_SEGMENT_DOMAIN_START,
};
pub use traits::{
    HotSwapOutcome, ModelHotSwap, OfflineEdgeTrainer, StreamingMergeTrainer, StreamingTrainResult,
    TrainingSurfaceKind, EVENT_KIND_MODEL_HOT_SWAP, EVENT_KIND_TRAINING_CYCLE,
};

#[cfg(test)]
mod tests;
