//! Offline per-sensor-class edge-intelligence pipeline (WEFT-531 surface 1).
//!
//! Accumulates ExoChain / synthetic samples, fits the action-conditioned linear
//! `pred_φ` (`ẑ = z + A·a + b`, ridge least squares, see
//! [`super::action_linear`]) per sensor class, and emits an RVF-deliverable
//! checkpoint (`LWAL` blob) for tick-aligned hot-swap via
//! [`crate::training::SensorModelRegistry`].

extern crate alloc;

use alloc::vec::Vec;

use weftos_worldmodel_core::{
    zero_latent, Latent, ModelCheckpoint, OfflineEdgeTrainer, SensorClass, SmallModelKind,
    TrainingSample, TrainingSurfaceKind, WorldModelError, WorldModelResult, LATENT_DIM,
};

use super::action_linear::{ActionLinearModel, DEFAULT_RIDGE_LAMBDA};
use super::replay::{ImportanceReplayBuffer, DEFAULT_REPLAY_CAPACITY};

/// Minimum samples required for an offline train cycle.
pub const DEFAULT_MIN_OFFLINE_SAMPLES: usize = 2;

/// Offline edge-intelligence trainer (ridge least-squares action-linear `pred_φ`).
#[derive(Debug, Clone)]
pub struct OfflineEdgePipeline {
    /// Sample buffer (all classes; filtered at train time).
    pub buffer: ImportanceReplayBuffer,
    /// Next version tag to assign (monotonic).
    pub next_version: u64,
    /// Minimum samples per class for a cycle.
    pub min_samples: usize,
    /// Train cycles completed.
    pub cycles_completed: u64,
    /// Ridge penalty on the action matrix (per unit of sample weight).
    pub ridge_lambda: f32,
}

impl Default for OfflineEdgePipeline {
    fn default() -> Self {
        Self::new()
    }
}

impl OfflineEdgePipeline {
    /// Construct with default capacity / min samples.
    pub fn new() -> Self {
        Self {
            buffer: ImportanceReplayBuffer::new(DEFAULT_REPLAY_CAPACITY),
            next_version: 1,
            min_samples: DEFAULT_MIN_OFFLINE_SAMPLES,
            cycles_completed: 0,
            ridge_lambda: DEFAULT_RIDGE_LAMBDA,
        }
    }

    /// Fit the action-linear model on the buffered samples of `class`.
    pub fn fit_class(&self, class: SensorClass) -> Option<ActionLinearModel> {
        ActionLinearModel::fit_ridge(self.buffer.iter_class(class), self.ridge_lambda)
    }

    /// Pack a residual latent as little-endian f32 weight bytes (legacy pre-W5 blob,
    /// still accepted by [`ActionLinearModel::from_weights`]).
    pub fn pack_residual(residual: &Latent) -> Vec<u8> {
        let mut out = Vec::with_capacity(LATENT_DIM * 4);
        for &v in residual.iter() {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out
    }

    /// Action-free residual `b` of a checkpoint blob: the bias of an `LWAL` blob, or the
    /// legacy packed residual (zeros on short input).
    pub fn unpack_residual(weights: &[u8]) -> Latent {
        if ActionLinearModel::is_blob(weights) {
            return ActionLinearModel::from_weights(weights)
                .map(|m| m.b)
                .unwrap_or_else(|_| zero_latent());
        }
        let mut z = zero_latent();
        for (i, zi) in z.iter_mut().enumerate() {
            let start = i * 4;
            if start + 4 > weights.len() {
                break;
            }
            let mut buf = [0u8; 4];
            buf.copy_from_slice(&weights[start..start + 4]);
            *zi = f32::from_le_bytes(buf);
        }
        z
    }
}

impl OfflineEdgeTrainer for OfflineEdgePipeline {
    fn push_sample(&mut self, sample: &TrainingSample) -> WorldModelResult<()> {
        self.buffer.push(*sample);
        Ok(())
    }

    fn train_cycle(
        &mut self,
        class: SensorClass,
        kind: SmallModelKind,
        target_tick: u64,
    ) -> WorldModelResult<ModelCheckpoint> {
        if self.buffer.count_class(class) < self.min_samples {
            return Err(WorldModelError::TrainingFailed);
        }
        let model = self.fit_class(class).ok_or(WorldModelError::TrainingFailed)?;
        let weights = model.to_weights();
        let version = self.next_version;
        self.next_version = self.next_version.saturating_add(1);
        self.cycles_completed = self.cycles_completed.saturating_add(1);
        Ok(ModelCheckpoint::new(
            class,
            kind,
            version,
            TrainingSurfaceKind::OfflineEdge,
            weights,
            Some(target_tick),
        ))
    }

    fn sample_count(&self) -> usize {
        self.buffer.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use weftos_worldmodel_core::OfflineEdgeTrainer;

    fn step_sample(class: SensorClass, t: f32) -> TrainingSample {
        let mut z0 = zero_latent();
        let mut z1 = zero_latent();
        z0[0] = t;
        z1[0] = t + 2.0; // residual 2.0 on dim 0
        TrainingSample::basic(class, z0, z1, (t as u64) * 10)
    }

    #[test]
    fn one_offline_cycle_emits_checkpoint() {
        let mut pipe = OfflineEdgePipeline::new();
        pipe.push_sample(&step_sample(SensorClass::Imu, 0.0))
            .unwrap();
        pipe.push_sample(&step_sample(SensorClass::Imu, 1.0))
            .unwrap();
        let cp = pipe
            .train_cycle(SensorClass::Imu, SmallModelKind::Encode, 10)
            .expect("cycle");
        assert_eq!(cp.surface, TrainingSurfaceKind::OfflineEdge);
        assert_eq!(cp.sensor_class, SensorClass::Imu);
        assert_eq!(cp.kind, SmallModelKind::Encode);
        assert_eq!(cp.target_tick, Some(10));
        assert_eq!(cp.version_tag, 1);
        let r = OfflineEdgePipeline::unpack_residual(&cp.weights);
        assert!((r[0] - 2.0).abs() < 1e-5, "residual={}", r[0]);
        assert_eq!(pipe.cycles_completed, 1);
        assert!(ActionLinearModel::is_blob(&cp.weights));
    }

    #[test]
    fn offline_cycle_learns_the_action_and_beats_the_residual_mean() {
        let (data, _) =
            crate::training::action_linear::tests::action_fixture(300, SensorClass::Imu, 0.001, 17);
        let (train, held) = data.split_at(200);
        let mut pipe = OfflineEdgePipeline::new();
        for s in train {
            pipe.push_sample(s).unwrap();
        }
        let cp = pipe.train_cycle(SensorClass::Imu, SmallModelKind::Encode, 1).unwrap();
        let trained = ActionLinearModel::from_weights(&cp.weights).unwrap();
        // The pre-W5 stub: mean residual over the train split, action ignored.
        let mut mean = zero_latent();
        for s in train {
            for (m, r) in mean.iter_mut().zip(s.residual().iter()) {
                *m += r / train.len() as f32;
            }
        }
        let residual_mean = ActionLinearModel::from_residual(mean);
        let (e_trained, e_mean) = (trained.loss(held), residual_mean.loss(held));
        assert!(e_trained < 0.01 * e_mean, "held-out {e_trained} vs residual-mean {e_mean}");
    }

    #[test]
    fn insufficient_samples_fail() {
        let mut pipe = OfflineEdgePipeline::new();
        pipe.push_sample(&step_sample(SensorClass::Audio, 0.0))
            .unwrap();
        assert!(matches!(
            pipe.train_cycle(SensorClass::Audio, SmallModelKind::Aggregate, 1),
            Err(WorldModelError::TrainingFailed)
        ));
    }
}
