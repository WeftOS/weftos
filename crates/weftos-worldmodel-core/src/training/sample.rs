//! Training samples and versioned model checkpoints.

use super::{SensorClass, SmallModelKind, TrainingSurfaceKind};
use crate::latent::Latent;
use crate::types::Action;

/// One transition sample used by either training surface.
///
/// ML trainers may ignore some fields; the stub path uses residual
/// statistics (`z_tp1 - z_t`) weighted by [`TrainingSample::importance`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TrainingSample {
    /// Sensor class that produced the observation.
    pub sensor_class: SensorClass,
    /// Latent at step `t`.
    pub z_t: Latent,
    /// Observed latent at step `t+1`.
    pub z_tp1: Latent,
    /// Control / intervention applied between `t` and `t+1`.
    pub action: Action,
    /// VoE surprise for this transition (higher → more rare / informative).
    pub surprise: f32,
    /// Per-class importance weight for replay (must be `> 0` to be drawn).
    pub importance: f32,
    /// Observation timestamp (milliseconds).
    pub timestamp_ms: u64,
}

impl TrainingSample {
    /// Construct with default importance `1.0` and zero action.
    pub fn basic(
        sensor_class: SensorClass,
        z_t: Latent,
        z_tp1: Latent,
        timestamp_ms: u64,
    ) -> Self {
        Self {
            sensor_class,
            z_t,
            z_tp1,
            action: Action::null(),
            surprise: 0.0,
            importance: 1.0,
            timestamp_ms,
        }
    }

    /// Residual `z_tp1 - z_t` (used by stub trainers).
    pub fn residual(&self) -> Latent {
        let mut r = self.z_tp1;
        for (ri, zt) in r.iter_mut().zip(self.z_t.iter()) {
            *ri -= *zt;
        }
        r
    }
}

/// Versioned model checkpoint delivered via RVF or promoted in-service.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelCheckpoint {
    /// Sensor class this checkpoint targets.
    pub sensor_class: SensorClass,
    /// Which of the three small-model slots.
    pub kind: SmallModelKind,
    /// Monotonic version tag (ExoChain attestation).
    pub version_tag: u64,
    /// Training surface that produced the checkpoint.
    pub surface: TrainingSurfaceKind,
    /// Opaque weight blob (stub: packed f32 residual; later: real tensors).
    pub weights: alloc::vec::Vec<u8>,
    /// FNV-1a style digest of `weights` for integrity / equality.
    pub weight_digest: u64,
    /// Tick at which a hot-swap may commit (`None` = immediate / online).
    pub target_tick: Option<u64>,
}

impl ModelCheckpoint {
    /// Recompute [`Self::weight_digest`] from current weights.
    pub fn recompute_digest(&mut self) {
        self.weight_digest = fnv1a64(&self.weights);
    }

    /// Construct and digest in one step.
    pub fn new(
        sensor_class: SensorClass,
        kind: SmallModelKind,
        version_tag: u64,
        surface: TrainingSurfaceKind,
        weights: alloc::vec::Vec<u8>,
        target_tick: Option<u64>,
    ) -> Self {
        let weight_digest = fnv1a64(&weights);
        Self {
            sensor_class,
            kind,
            version_tag,
            surface,
            weights,
            weight_digest,
            target_tick,
        }
    }
}

/// FNV-1a 64-bit over bytes (no_std, no deps).
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x100_0000_01b3;
    let mut h = OFFSET;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(PRIME);
    }
    h
}
