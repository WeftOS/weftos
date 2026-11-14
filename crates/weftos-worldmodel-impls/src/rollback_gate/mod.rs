//! Four-condition AND rollback gate — production metrics (WEFT-530).
//!
//! Computes the four streaming-merge promotion metrics online and combines
//! them with a strict AND:
//!
//! | # | Metric | Score |
//! |---|--------|-------|
//! | 1 | Cluster SIGReg health | from [`WelfordSigRegMonitor`] / host |
//! | 2 | Held-out probe accuracy | fraction of held-out pairs where the model beats copy-last |
//! | 3 | VoE surprise differentiation | real vs null residual gap |
//! | 4 | Temporal straightening | chord / path over a latent window |
//!
//! Any single condition below its threshold blocks checkpoint promotion.
//!
//! # Condition 2: the probe (changed 2026-11-14, owner decision, plan item W4)
//!
//! The probe is computed online from the [`ProbePair`] window the gate keeps. Each pair now
//! carries the model's prediction `ẑ`, so the default [`GateProbeKind::BeatsCopyLast`] can ask
//! whether the prediction carries information beyond `ẑ = z_t`. The legacy sign-agreement score
//! (`Δz` against the mean residual) never looked at the prediction and scored a perfect model
//! 0.54 on mean-reverting dynamics, so it vetoed every synthetic model. It stays reachable with
//! [`FourConditionRollbackGate::with_probe_kind`]. The floor `GATE_HELD_OUT_PROBE_MIN` (0.70) is
//! unchanged. The other three conditions and their thresholds are unchanged. See [`probe`] for
//! the argument and the failure case.

pub mod probe;

pub use probe::{beats_copy_last, sign_agreement, GateProbeKind};

extern crate alloc;

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use weftos_worldmodel_core::{
    surprise_voe, GateCondition, GateVerdict, Latent, RollbackGate,
    RollbackGateMetrics, LATENT_DIM,
};

/// Default held-out pair buffer capacity.
pub const DEFAULT_PROBE_CAPACITY: usize = 64;

/// Default trajectory window for temporal straightening.
pub const DEFAULT_TRAJECTORY_CAPACITY: usize = 16;

/// Minimum samples before probe / straightening leave the cold-start
/// perfect-score regime (avoids false vetoes at boot).
pub const DEFAULT_MIN_SAMPLES: u64 = 4;

/// One held-out transition used by the probe.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProbePair {
    /// Latent at step `t`.
    pub z_t: Latent,
    /// Model prediction `ẑ = pred_φ(z_t, a_t)` for step `t+1`.
    pub z_hat: Latent,
    /// Observed latent at step `t+1`.
    pub z_tp1: Latent,
}

/// Online four-condition gate for streaming-merge checkpoints.
///
/// Feed frames via [`FourConditionRollbackGate::observe_transition`]. Hosts
/// that already run a Welford SIGReg monitor should call
/// [`FourConditionRollbackGate::set_sigreg_health`] (or pass health into
/// `observe_transition`) so condition 1 tracks the cluster monitor.
#[derive(Debug, Clone)]
pub struct FourConditionRollbackGate {
    /// Latest cluster SIGReg health score `[0, 1]`.
    sigreg_health: f32,
    /// Held-out transition buffer (ring).
    probe_buf: VecDeque<ProbePair>,
    /// Capacity of the probe buffer.
    probe_capacity: usize,
    /// Score feeding condition 2.
    probe_kind: GateProbeKind,
    /// Recent latent trajectory for straightening.
    trajectory: VecDeque<Latent>,
    /// Capacity of the trajectory window.
    trajectory_capacity: usize,
    /// Running mean of real VoE surprise.
    mean_real_surprise: f32,
    /// Running mean of null VoE surprise (`ẑ = z_t`, identity baseline).
    mean_null_surprise: f32,
    /// Transition count (for Welford-style means and warm-up).
    n_transitions: u64,
    /// Minimum transitions before probe/VoE/straighten can leave 1.0.
    pub min_samples: u64,
    /// Total AND-gate veto events (promotion blocked).
    pub veto_count: u64,
    /// Total promotion-allowed evaluations.
    pub promote_count: u64,
    /// Last evaluated verdict.
    pub last_verdict: Option<GateVerdict>,
}

impl Default for FourConditionRollbackGate {
    fn default() -> Self {
        Self::new()
    }
}

impl FourConditionRollbackGate {
    /// Construct with default capacities and perfect cold-start health.
    pub fn new() -> Self {
        Self {
            sigreg_health: 1.0,
            probe_buf: VecDeque::with_capacity(DEFAULT_PROBE_CAPACITY),
            probe_capacity: DEFAULT_PROBE_CAPACITY,
            probe_kind: GateProbeKind::default(),
            trajectory: VecDeque::with_capacity(DEFAULT_TRAJECTORY_CAPACITY),
            trajectory_capacity: DEFAULT_TRAJECTORY_CAPACITY,
            mean_real_surprise: 0.0,
            mean_null_surprise: 0.0,
            n_transitions: 0,
            min_samples: DEFAULT_MIN_SAMPLES,
            veto_count: 0,
            promote_count: 0,
            last_verdict: None,
        }
    }

    /// Override probe buffer capacity (held-out ring).
    pub fn with_probe_capacity(mut self, cap: usize) -> Self {
        self.probe_capacity = cap.max(2);
        self
    }

    /// Select the condition-2 score. `SignAgreement` restores the legacy behaviour.
    pub fn with_probe_kind(mut self, kind: GateProbeKind) -> Self {
        self.probe_kind = kind;
        self
    }

    /// The condition-2 score in use.
    pub fn probe_kind(&self) -> GateProbeKind {
        self.probe_kind
    }

    /// Override trajectory window capacity.
    pub fn with_trajectory_capacity(mut self, cap: usize) -> Self {
        self.trajectory_capacity = cap.max(2);
        self
    }

    /// Inject / refresh cluster SIGReg health (from Welford monitor).
    pub fn set_sigreg_health(&mut self, score: f32) {
        self.sigreg_health = score.clamp(0.0, 1.0);
    }

    /// Number of transitions ingested.
    pub fn transition_count(&self) -> u64 {
        self.n_transitions
    }

    /// Ingest one observed transition and return the updated AND verdict.
    ///
    /// - `z_t` / `z_tp1`: observed latents
    /// - `z_hat`: model prediction `pred_φ(z_t, a_t)`
    /// - `sigreg_health`: optional override for condition 1 (`None` keeps last)
    pub fn observe_transition(
        &mut self,
        z_t: &Latent,
        z_hat: &Latent,
        z_tp1: &Latent,
        sigreg_health: Option<f32>,
    ) -> GateVerdict {
        if let Some(h) = sigreg_health {
            self.set_sigreg_health(h);
        }

        // VoE: real vs null (identity) surprise means.
        let real = surprise_voe(z_hat, z_tp1);
        let null = surprise_voe(z_t, z_tp1); // identity baseline ẑ = z_t
        self.n_transitions = self.n_transitions.saturating_add(1);
        let n = self.n_transitions as f32;
        self.mean_real_surprise += (real - self.mean_real_surprise) / n;
        self.mean_null_surprise += (null - self.mean_null_surprise) / n;

        // Held-out probe ring.
        if self.probe_buf.len() >= self.probe_capacity {
            self.probe_buf.pop_front();
        }
        self.probe_buf.push_back(ProbePair {
            z_t: *z_t,
            z_hat: *z_hat,
            z_tp1: *z_tp1,
        });

        // Trajectory for temporal straightening.
        if self.trajectory.len() >= self.trajectory_capacity {
            self.trajectory.pop_front();
        }
        self.trajectory.push_back(*z_tp1);

        let verdict = self.verdict();
        self.last_verdict = Some(verdict);
        if verdict.promote {
            self.promote_count = self.promote_count.saturating_add(1);
        } else {
            self.veto_count = self.veto_count.saturating_add(1);
        }
        verdict
    }

    /// Evaluate without ingesting a new sample.
    pub fn evaluate_now(&mut self) -> GateVerdict {
        let verdict = self.verdict();
        self.last_verdict = Some(verdict);
        if verdict.promote {
            self.promote_count = self.promote_count.saturating_add(1);
        } else {
            self.veto_count = self.veto_count.saturating_add(1);
        }
        verdict
    }

    /// Reset online stats (after a successful rollback to last-good segment).
    pub fn reset_stats(&mut self) {
        self.probe_buf.clear();
        self.trajectory.clear();
        self.mean_real_surprise = 0.0;
        self.mean_null_surprise = 0.0;
        self.n_transitions = 0;
        self.last_verdict = None;
        // Keep sigreg_health and counters for audit continuity.
    }

    // ── metric computers ──────────────────────────────────────────────────

    fn compute_held_out_probe_accuracy(&self) -> f32 {
        if self.n_transitions < self.min_samples || self.probe_buf.len() < 2 {
            return 1.0; // cold start: do not veto
        }
        self.probe_kind.score(self.probe_buf.iter())
    }

    fn compute_voe_diff(&self) -> f32 {
        if self.n_transitions < self.min_samples {
            return 1.0;
        }
        // Differentiation: how much lower real surprise is vs null baseline.
        // score = clamp((null - real) / (null + eps), 0, 1)
        let denom = self.mean_null_surprise + 1e-6;
        let raw = (self.mean_null_surprise - self.mean_real_surprise) / denom;
        raw.clamp(0.0, 1.0)
    }

    fn compute_temporal_straightening(&self) -> f32 {
        if self.n_transitions < self.min_samples || self.trajectory.len() < 2 {
            return 1.0;
        }
        // score = chord / path  (1 = perfectly straight trajectory)
        let mut path = 0.0f32;
        let traj: Vec<&Latent> = self.trajectory.iter().collect();
        for w in traj.windows(2) {
            path += l2(w[0], w[1]);
        }
        let chord = l2(traj[0], traj[traj.len() - 1]);
        if path < 1e-9 {
            // Stationary trajectory: treat as perfectly straight.
            return 1.0;
        }
        (chord / path).clamp(0.0, 1.0)
    }

    fn snapshot_metrics(&self) -> RollbackGateMetrics {
        RollbackGateMetrics {
            sigreg_health: self.sigreg_health.clamp(0.0, 1.0),
            held_out_probe_accuracy: self.compute_held_out_probe_accuracy(),
            voe_surprise_diff: self.compute_voe_diff(),
            temporal_straightening: self.compute_temporal_straightening(),
        }
    }
}

impl RollbackGate for FourConditionRollbackGate {
    fn metrics(&self) -> RollbackGateMetrics {
        self.snapshot_metrics()
    }
}

/// JSON-friendly payload for ExoChain / metrics logging of gate evaluations.
#[derive(Debug, Clone, PartialEq)]
pub struct RollbackGateLog {
    /// Event kind string.
    pub kind: &'static str,
    /// Whether promotion is allowed.
    pub promote: bool,
    /// SIGReg health score.
    pub sigreg_health: f32,
    /// Held-out probe accuracy.
    pub held_out_probe_accuracy: f32,
    /// VoE surprise differentiation.
    pub voe_surprise_diff: f32,
    /// Temporal straightening score.
    pub temporal_straightening: f32,
    /// First failing condition name, if any.
    pub first_veto: Option<&'static str>,
    /// Transition count at evaluation.
    pub transition_count: u64,
}

impl FourConditionRollbackGate {
    /// Build a log record from the last verdict (or evaluate now).
    pub fn to_log(&self) -> RollbackGateLog {
        let v = self.last_verdict.unwrap_or_else(|| self.verdict());
        RollbackGateLog {
            kind: weftos_worldmodel_core::EVENT_KIND_ROLLBACK_GATE,
            promote: v.promote,
            sigreg_health: v.metrics.sigreg_health,
            held_out_probe_accuracy: v.metrics.held_out_probe_accuracy,
            voe_surprise_diff: v.metrics.voe_surprise_diff,
            temporal_straightening: v.metrics.temporal_straightening,
            first_veto: v.first_veto().map(GateCondition::as_str),
            transition_count: self.n_transitions,
        }
    }
}

#[inline]
fn l2(a: &Latent, b: &Latent) -> f32 {
    let mut acc = 0.0f32;
    for i in 0..LATENT_DIM {
        let d = a[i] - b[i];
        acc += d * d;
    }
    crate::math_no_std::sqrtf(acc)
}

#[cfg(test)]
mod tests;
