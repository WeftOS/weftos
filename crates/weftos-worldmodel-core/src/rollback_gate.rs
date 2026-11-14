//! Four-condition AND rollback / promotion gate (WEFT-530 / ADR-055 training).
//!
//! Online streaming-merge world-model checkpoints must satisfy **all four**
//! conditions before promotion; any single failure vetoes the checkpoint and
//! triggers rollback to the last good segment:
//!
//! 1. **Cluster SIGReg health** — merged manifold health ≥ threshold
//! 2. **Held-out probing accuracy** — linear probe on held-out latents
//! 3. **VoE surprise differentiation** — real transitions surprise less than null
//! 4. **Temporal-straightening score** — trajectory curvature is low enough
//!
//! See `.planning/symposiums/lewm-worldmodel/diagram.md` (training layer) and
//! research stream T29 / D32.

use crate::sigreg::SIGREG_HEALTH_ROLLBACK_THRESHOLD;

/// Minimum cluster SIGReg health for promotion (same as edge auto-rollback).
pub const GATE_SIGREG_HEALTH_MIN: f32 = SIGREG_HEALTH_ROLLBACK_THRESHOLD;

/// Minimum held-out probe accuracy in `[0, 1]`.
///
/// Since 2026-11-14 the score is the fraction of held-out transitions where the model beats
/// copy-last (`weftos-worldmodel-impls::GateProbeKind::BeatsCopyLast`). The floor is unchanged:
/// a model must beat the trivial predictor on at least 70 % of held-out pairs. On the synthetic
/// fixtures a good model scores 1.0 and copy-last scores 0.0, so 0.70 separates them with wide
/// margin and no tuning was needed.
pub const GATE_HELD_OUT_PROBE_MIN: f32 = 0.70;

/// Minimum VoE surprise differentiation score in `[0, 1]`.
///
/// Higher means real-transition surprise is reliably lower than null /
/// shuffled baselines (model explains variance of experience).
pub const GATE_VOE_DIFF_MIN: f32 = 0.10;

/// Minimum temporal-straightening score in `[0, 1]` (`chord / path`).
pub const GATE_TEMPORAL_STRAIGHTEN_MIN: f32 = 0.50;

/// Named gate conditions (for veto reporting / ExoChain logs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum GateCondition {
    /// Cluster SIGReg health (merged Welford partial sums).
    ClusterSigRegHealth,
    /// Held-out probing accuracy.
    HeldOutProbeAccuracy,
    /// VoE surprise differentiation (real vs null).
    VoeSurpriseDifferentiation,
    /// Temporal-straightening score.
    TemporalStraightening,
}

impl GateCondition {
    /// Stable wire / log name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ClusterSigRegHealth => "cluster_sigreg_health",
            Self::HeldOutProbeAccuracy => "held_out_probe_accuracy",
            Self::VoeSurpriseDifferentiation => "voe_surprise_differentiation",
            Self::TemporalStraightening => "temporal_straightening",
        }
    }

    /// All four conditions in evaluation order.
    pub const ALL: [GateCondition; 4] = [
        Self::ClusterSigRegHealth,
        Self::HeldOutProbeAccuracy,
        Self::VoeSurpriseDifferentiation,
        Self::TemporalStraightening,
    ];
}

/// Snapshot of the four scalar metrics used by the AND gate.
///
/// All scores are in `[0, 1]` where higher is better (promotion-friendly).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RollbackGateMetrics {
    /// Cluster SIGReg health (merged partial sums), `[0, 1]`.
    pub sigreg_health: f32,
    /// Held-out linear-probe accuracy, `[0, 1]`.
    pub held_out_probe_accuracy: f32,
    /// VoE surprise differentiation, `[0, 1]`.
    pub voe_surprise_diff: f32,
    /// Temporal-straightening score (`chord / path`), `[0, 1]`.
    pub temporal_straightening: f32,
}

impl RollbackGateMetrics {
    /// Perfect / promotion-friendly metrics (all 1.0).
    pub const fn perfect() -> Self {
        Self {
            sigreg_health: 1.0,
            held_out_probe_accuracy: 1.0,
            voe_surprise_diff: 1.0,
            temporal_straightening: 1.0,
        }
    }

    /// Metric value for a named condition.
    pub const fn value(self, cond: GateCondition) -> f32 {
        match cond {
            GateCondition::ClusterSigRegHealth => self.sigreg_health,
            GateCondition::HeldOutProbeAccuracy => self.held_out_probe_accuracy,
            GateCondition::VoeSurpriseDifferentiation => self.voe_surprise_diff,
            GateCondition::TemporalStraightening => self.temporal_straightening,
        }
    }

    /// Threshold for a named condition.
    pub const fn threshold(cond: GateCondition) -> f32 {
        match cond {
            GateCondition::ClusterSigRegHealth => GATE_SIGREG_HEALTH_MIN,
            GateCondition::HeldOutProbeAccuracy => GATE_HELD_OUT_PROBE_MIN,
            GateCondition::VoeSurpriseDifferentiation => GATE_VOE_DIFF_MIN,
            GateCondition::TemporalStraightening => GATE_TEMPORAL_STRAIGHTEN_MIN,
        }
    }

    /// Whether a single condition passes its threshold.
    pub fn passes(self, cond: GateCondition) -> bool {
        self.value(cond) >= Self::threshold(cond)
    }
}

/// AND-gate verdict: promote only when every condition passes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GateVerdict {
    /// Metrics that were evaluated.
    pub metrics: RollbackGateMetrics,
    /// Cluster SIGReg health ≥ threshold.
    pub sigreg_ok: bool,
    /// Held-out probe ≥ threshold.
    pub probe_ok: bool,
    /// VoE differentiation ≥ threshold.
    pub voe_ok: bool,
    /// Temporal straightening ≥ threshold.
    pub straighten_ok: bool,
    /// `true` only when **all four** conditions pass (AND).
    pub promote: bool,
}

impl GateVerdict {
    /// Evaluate the four-condition AND gate.
    ///
    /// Promotion is allowed iff every condition is at or above its threshold.
    /// Any single failure blocks promotion (rollback / hold last good segment).
    pub fn evaluate(metrics: RollbackGateMetrics) -> Self {
        let sigreg_ok = metrics.passes(GateCondition::ClusterSigRegHealth);
        let probe_ok = metrics.passes(GateCondition::HeldOutProbeAccuracy);
        let voe_ok = metrics.passes(GateCondition::VoeSurpriseDifferentiation);
        let straighten_ok = metrics.passes(GateCondition::TemporalStraightening);
        let promote = sigreg_ok && probe_ok && voe_ok && straighten_ok;
        Self {
            metrics,
            sigreg_ok,
            probe_ok,
            voe_ok,
            straighten_ok,
            promote,
        }
    }

    /// Whether promotion must be blocked (inverse of [`Self::promote`]).
    #[inline]
    pub const fn blocks_promotion(self) -> bool {
        !self.promote
    }

    /// Whether this verdict should trigger checkpoint rollback.
    #[inline]
    pub const fn should_rollback(self) -> bool {
        self.blocks_promotion()
    }

    /// First failing condition in evaluation order, if any.
    pub fn first_veto(self) -> Option<GateCondition> {
        if !self.sigreg_ok {
            Some(GateCondition::ClusterSigRegHealth)
        } else if !self.probe_ok {
            Some(GateCondition::HeldOutProbeAccuracy)
        } else if !self.voe_ok {
            Some(GateCondition::VoeSurpriseDifferentiation)
        } else if !self.straighten_ok {
            Some(GateCondition::TemporalStraightening)
        } else {
            None
        }
    }

    /// All failing conditions (stable order).
    pub fn failing(self) -> alloc::vec::Vec<GateCondition> {
        let mut out = alloc::vec::Vec::new();
        if !self.sigreg_ok {
            out.push(GateCondition::ClusterSigRegHealth);
        }
        if !self.probe_ok {
            out.push(GateCondition::HeldOutProbeAccuracy);
        }
        if !self.voe_ok {
            out.push(GateCondition::VoeSurpriseDifferentiation);
        }
        if !self.straighten_ok {
            out.push(GateCondition::TemporalStraightening);
        }
        out
    }

    /// Pass/fail flag for a named condition.
    pub const fn ok(self, cond: GateCondition) -> bool {
        match cond {
            GateCondition::ClusterSigRegHealth => self.sigreg_ok,
            GateCondition::HeldOutProbeAccuracy => self.probe_ok,
            GateCondition::VoeSurpriseDifferentiation => self.voe_ok,
            GateCondition::TemporalStraightening => self.straighten_ok,
        }
    }
}

/// Host-facing gate trait: supply metrics, receive AND verdict.
///
/// Production path lives in `weftos-worldmodel-impls::FourConditionRollbackGate`.
pub trait RollbackGate {
    /// Current metrics without side effects.
    fn metrics(&self) -> RollbackGateMetrics;

    /// Evaluate AND gate on current metrics.
    fn verdict(&self) -> GateVerdict {
        GateVerdict::evaluate(self.metrics())
    }

    /// Whether a streaming-merge checkpoint may be promoted.
    fn may_promote(&self) -> bool {
        self.verdict().promote
    }
}

/// ExoChain / metrics event kind for four-condition gate evaluations.
pub const EVENT_KIND_ROLLBACK_GATE: &str = "lewm.rollback_gate";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn perfect_metrics_promote() {
        let v = GateVerdict::evaluate(RollbackGateMetrics::perfect());
        assert!(v.promote);
        assert!(!v.blocks_promotion());
        assert!(v.first_veto().is_none());
        assert!(v.failing().is_empty());
    }

    #[test]
    fn any_single_failure_blocks_promotion() {
        // SIGReg veto
        let mut m = RollbackGateMetrics::perfect();
        m.sigreg_health = 0.84;
        let v = GateVerdict::evaluate(m);
        assert!(!v.promote);
        assert!(!v.sigreg_ok);
        assert_eq!(v.first_veto(), Some(GateCondition::ClusterSigRegHealth));

        // Probe veto
        m = RollbackGateMetrics::perfect();
        m.held_out_probe_accuracy = 0.69;
        let v = GateVerdict::evaluate(m);
        assert!(!v.promote);
        assert!(!v.probe_ok);
        assert_eq!(v.first_veto(), Some(GateCondition::HeldOutProbeAccuracy));

        // VoE veto
        m = RollbackGateMetrics::perfect();
        m.voe_surprise_diff = 0.09;
        let v = GateVerdict::evaluate(m);
        assert!(!v.promote);
        assert!(!v.voe_ok);
        assert_eq!(
            v.first_veto(),
            Some(GateCondition::VoeSurpriseDifferentiation)
        );

        // Straightening veto
        m = RollbackGateMetrics::perfect();
        m.temporal_straightening = 0.49;
        let v = GateVerdict::evaluate(m);
        assert!(!v.promote);
        assert!(!v.straighten_ok);
        assert_eq!(v.first_veto(), Some(GateCondition::TemporalStraightening));
    }

    #[test]
    fn thresholds_match_research_constants() {
        assert!((GATE_SIGREG_HEALTH_MIN - 0.85).abs() < f32::EPSILON);
        assert!((GATE_HELD_OUT_PROBE_MIN - 0.70).abs() < f32::EPSILON);
        assert!((GATE_VOE_DIFF_MIN - 0.10).abs() < f32::EPSILON);
        assert!((GATE_TEMPORAL_STRAIGHTEN_MIN - 0.50).abs() < f32::EPSILON);
        assert_eq!(EVENT_KIND_ROLLBACK_GATE, "lewm.rollback_gate");
        assert_eq!(GateCondition::ALL.len(), 4);
    }

    #[test]
    fn boundary_values_pass() {
        let m = RollbackGateMetrics {
            sigreg_health: GATE_SIGREG_HEALTH_MIN,
            held_out_probe_accuracy: GATE_HELD_OUT_PROBE_MIN,
            voe_surprise_diff: GATE_VOE_DIFF_MIN,
            temporal_straightening: GATE_TEMPORAL_STRAIGHTEN_MIN,
        };
        assert!(GateVerdict::evaluate(m).promote);
    }
}
