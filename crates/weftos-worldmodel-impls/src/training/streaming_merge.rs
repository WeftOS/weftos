//! Online streaming-merge trainer with AND-gate promotion (WEFT-531 surface 2).
//!
//! Importance-weighted replay → one normalised-LMS step of the action-linear
//! `pred_φ` (`ẑ = z + A·a + b`, see [`super::action_linear`]) per batch →
//! four-condition AND gate. Checkpoints are only marked `promoted` when the
//! gate allows.

extern crate alloc;

use alloc::vec::Vec;

use weftos_worldmodel_core::{
    Action, GateVerdict, Latent, ModelCheckpoint, RollbackGate, SensorClass, SmallModelKind,
    StreamingMergeTrainer, StreamingTrainResult, TrainingSample, TrainingSurfaceKind,
    WorldModelError, WorldModelResult,
};

use crate::rollback_gate::FourConditionRollbackGate;

use super::action_linear::ActionLinearModel;
use super::replay::{ImportanceReplayBuffer, DEFAULT_REPLAY_CAPACITY};

/// Promote only when the AND gate allowed the checkpoint.
pub fn require_promoted(result: StreamingTrainResult) -> WorldModelResult<ModelCheckpoint> {
    if result.promoted {
        Ok(result.checkpoint)
    } else {
        Err(WorldModelError::CheckpointRejected)
    }
}

/// Default batch size for one streaming train step.
pub const DEFAULT_BATCH_SIZE: usize = 8;

/// Minimum replay samples before a train step is allowed.
pub const DEFAULT_MIN_REPLAY: usize = 2;

/// Online streaming-merge world-model trainer.
#[derive(Debug, Clone)]
pub struct StreamingMergePipeline {
    /// Importance-weighted replay buffer.
    pub replay: ImportanceReplayBuffer,
    /// Four-condition AND gate (shared metrics with host service).
    pub gate: FourConditionRollbackGate,
    /// Running action-linear model (trained by SGD).
    model: ActionLinearModel,
    /// Next version tag.
    pub next_version: u64,
    /// Batch size per train step.
    pub batch_size: usize,
    /// Minimum samples in replay for a step.
    pub min_replay: usize,
    /// Normalised-LMS step size (`0 < lr < 2`; with a null action this is the
    /// EMA rate of the action-free residual).
    pub learning_rate: f32,
    /// Last gate verdict.
    last_gate: Option<GateVerdict>,
    /// Steps that produced a promoted checkpoint.
    pub promote_count: u64,
    /// Steps rejected by the AND gate.
    pub reject_count: u64,
    /// Total train steps attempted.
    pub step_count: u64,
}

impl Default for StreamingMergePipeline {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamingMergePipeline {
    /// Construct with defaults.
    pub fn new() -> Self {
        Self {
            replay: ImportanceReplayBuffer::new(DEFAULT_REPLAY_CAPACITY),
            gate: FourConditionRollbackGate::new(),
            model: ActionLinearModel::zero(),
            next_version: 1,
            batch_size: DEFAULT_BATCH_SIZE,
            min_replay: DEFAULT_MIN_REPLAY,
            learning_rate: 0.5,
            last_gate: None,
            promote_count: 0,
            reject_count: 0,
            step_count: 0,
        }
    }

    /// Replace / share the gate (host may inject a live gate).
    pub fn with_gate(mut self, gate: FourConditionRollbackGate) -> Self {
        self.gate = gate;
        self
    }

    /// Current action-free residual `b` (latent view).
    pub fn residual(&self) -> &Latent {
        &self.model.b
    }

    /// Current action-linear model.
    pub fn model(&self) -> &ActionLinearModel {
        &self.model
    }

    /// `ẑ_{t+1} = z_t + A·a + b` under the current model.
    pub fn predict(&self, z_t: &Latent, action: &Action) -> Latent {
        self.model.predict(z_t, action)
    }

    /// Feed a transition into the AND gate (host live path) so streaming
    /// promotion tracks SIGReg / VoE / probe / straighten.
    pub fn observe_transition_for_gate(
        &mut self,
        z_t: &Latent,
        z_hat: &Latent,
        z_tp1: &Latent,
        sigreg_health: Option<f32>,
    ) -> GateVerdict {
        let v = self
            .gate
            .observe_transition(z_t, z_hat, z_tp1, sigreg_health);
        self.last_gate = Some(v);
        v
    }

    /// Predict `ẑ_{t+1} = z_t + b` (action-free residual only).
    pub fn predict_residual(&self, z_t: &Latent) -> Latent {
        self.model.predict(z_t, &Action::null())
    }
}

impl StreamingMergeTrainer for StreamingMergePipeline {
    fn push_sample(&mut self, sample: &TrainingSample) -> WorldModelResult<()> {
        // Also feed the gate so online metrics track live data.
        let z_hat = self.predict(&sample.z_t, &sample.action);
        let _ = self.gate.observe_transition(
            &sample.z_t,
            &z_hat,
            &sample.z_tp1,
            None,
        );
        self.replay.push(*sample);
        Ok(())
    }

    fn train_step(
        &mut self,
        class: SensorClass,
        kind: SmallModelKind,
    ) -> WorldModelResult<StreamingTrainResult> {
        if self.replay.count_class(class) < self.min_replay {
            return Err(WorldModelError::TrainingFailed);
        }
        let batch = self.replay.sample_batch(self.batch_size, Some(class));
        if batch.is_empty() {
            return Err(WorldModelError::TrainingFailed);
        }
        let lr = self.learning_rate;
        self.model.sgd_step(&batch, lr);

        // Refresh VoE / probe metrics under the updated model using
        // **time-ordered** class samples (not the random batch). The default
        // straightening score is per transition and ignores order, but the
        // legacy chord / path kind would zigzag under random re-feeds and
        // false-veto healthy promotions.
        let ordered: Vec<TrainingSample> = self.replay.iter_class(class).copied().collect();
        // Soft reset trajectory-sensitive stats while keeping SIGReg health.
        let sigreg = self.gate.metrics().sigreg_health;
        self.gate.reset_stats();
        self.gate.set_sigreg_health(sigreg);
        for s in &ordered {
            let z_hat = self.predict(&s.z_t, &s.action);
            let v = self
                .gate
                .observe_transition(&s.z_t, &z_hat, &s.z_tp1, None);
            self.last_gate = Some(v);
        }

        let gate = self.gate.evaluate_now();
        self.last_gate = Some(gate);
        self.step_count = self.step_count.saturating_add(1);

        let weights = self.model.to_weights();
        let version = self.next_version;
        self.next_version = self.next_version.saturating_add(1);
        let checkpoint = ModelCheckpoint::new(
            class,
            kind,
            version,
            TrainingSurfaceKind::OnlineStreamingMerge,
            weights,
            None, // online: no deferred tick; host may still stage if desired
        );

        let promoted = gate.promote;
        if promoted {
            self.promote_count = self.promote_count.saturating_add(1);
        } else {
            self.reject_count = self.reject_count.saturating_add(1);
        }

        Ok(StreamingTrainResult {
            checkpoint,
            gate,
            promoted,
        })
    }

    fn last_gate(&self) -> Option<GateVerdict> {
        self.last_gate
    }

    fn replay_len(&self) -> usize {
        self.replay.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::action_linear::{tests::action_fixture, ACTION_LINEAR_BLOB_LEN};
    use weftos_worldmodel_core::{zero_latent, GateCondition, StreamingMergeTrainer};

    fn good_sample(t: u64) -> TrainingSample {
        // Straight residual + perfect match for residual predictor after train.
        let mut z0 = zero_latent();
        let mut z1 = zero_latent();
        z0[0] = t as f32;
        z1[0] = (t as f32) + 1.0;
        let mut s = TrainingSample::basic(SensorClass::RgbD, z0, z1, t * 1000);
        s.importance = 1.0;
        s.surprise = 0.1;
        s
    }

    #[test]
    fn one_streaming_cycle_promotes_when_gate_ok() {
        let mut pipe = StreamingMergePipeline::new();
        pipe.gate.min_samples = 2;
        for t in 0..8 {
            pipe.push_sample(&good_sample(t)).unwrap();
        }
        // Healthy SIGReg so only dynamics matter.
        pipe.gate.set_sigreg_health(0.95);
        let result = pipe
            .train_step(SensorClass::RgbD, SmallModelKind::Encode)
            .expect("step");
        assert_eq!(
            result.checkpoint.surface,
            TrainingSurfaceKind::OnlineStreamingMerge
        );
        // With consistent residual 1.0 and perfect pred after update, gate should promote.
        assert!(
            result.promoted,
            "expected promote, gate={:?}",
            result.gate
        );
        assert!(pipe.promote_count >= 1);
        assert_eq!(result.checkpoint.weights.len(), ACTION_LINEAR_BLOB_LEN);
    }

    #[test]
    fn streaming_sgd_learns_the_action_over_a_few_hundred_samples() {
        let (data, _) = action_fixture(360, SensorClass::RgbD, 0.001, 23);
        let (stream, held) = data.split_at(300);
        let mut pipe = StreamingMergePipeline::new();
        pipe.replay = ImportanceReplayBuffer::new(32);
        let initial = pipe.model().loss(held);
        for (t, s) in stream.iter().enumerate() {
            pipe.push_sample(s).unwrap();
            if t >= 2 {
                pipe.train_step(SensorClass::RgbD, SmallModelKind::Encode).unwrap();
            }
        }
        let fin = pipe.model().loss(held);
        assert!(fin < 0.1 * initial, "held-out loss {initial} -> {fin}");
        let cp = ActionLinearModel::from_weights(
            &pipe.train_step(SensorClass::RgbD, SmallModelKind::Encode).unwrap().checkpoint.weights,
        )
        .unwrap();
        assert!(cp.loss(held) < 0.1 * initial);
    }

    #[test]
    fn and_gate_veto_blocks_promotion() {
        let mut pipe = StreamingMergePipeline::new();
        pipe.gate.min_samples = 2;
        for t in 0..6 {
            pipe.push_sample(&good_sample(t)).unwrap();
        }
        // Force SIGReg-only veto.
        pipe.gate.set_sigreg_health(0.10);
        let result = pipe
            .train_step(SensorClass::RgbD, SmallModelKind::Aggregate)
            .expect("step");
        assert!(!result.promoted);
        assert_eq!(
            result.gate.first_veto(),
            Some(GateCondition::ClusterSigRegHealth)
        );
        assert!(matches!(
            require_promoted(result),
            Err(WorldModelError::CheckpointRejected)
        ));
        assert!(pipe.reject_count >= 1);
    }

    #[test]
    fn empty_replay_fails() {
        let mut pipe = StreamingMergePipeline::new();
        assert!(matches!(
            pipe.train_step(SensorClass::Imu, SmallModelKind::TransmitGate),
            Err(WorldModelError::TrainingFailed)
        ));
    }
}
