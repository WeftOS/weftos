use super::*;
use alloc::vec::Vec;
use weftos_worldmodel_core::{
    zero_latent, GATE_HELD_OUT_PROBE_MIN, GATE_PREDICTED_CURVATURE_MIN, GATE_SIGREG_HEALTH_MIN, LATENT_DIM,
    GATE_TEMPORAL_STRAIGHTEN_MIN, GATE_VOE_DIFF_MIN,
};

fn unit_step(i: usize, v: f32) -> Latent {
    let mut z = zero_latent();
    z[i] = v;
    z
}

#[test]
fn cold_start_promotes() {
    let mut g = FourConditionRollbackGate::new();
    g.min_samples = 8;
    let v = g.evaluate_now();
    assert!(v.promote, "cold start must not false-veto: {v:?}");
}

#[test]
fn good_straight_trajectory_with_healthy_sigreg_promotes() {
    let mut g = FourConditionRollbackGate::new();
    g.min_samples = 3;
    // Straight line in dim 0: z = 0,1,2,3,... with perfect prediction.
    for t in 0..12u64 {
        let z_t = unit_step(0, t as f32);
        let z_tp1 = unit_step(0, (t + 1) as f32);
        let z_hat = z_tp1; // perfect pred → real surprise 0
        let v = g.observe_transition(&z_t, &z_hat, &z_tp1, Some(0.95));
        if t + 1 >= g.min_samples {
            assert!(
                v.promote,
                "t={t} expected promote, metrics={:?}",
                v.metrics
            );
        }
    }
    assert!(g.promote_count > 0);
}

#[test]
fn sigreg_alone_vetoes() {
    let mut g = FourConditionRollbackGate::new();
    g.min_samples = 2;
    // Feed healthy dynamics first so other metrics pass.
    for t in 0..6u64 {
        let z_t = unit_step(0, t as f32);
        let z_tp1 = unit_step(0, (t + 1) as f32);
        let _ = g.observe_transition(&z_t, &z_tp1, &z_tp1, Some(0.95));
    }
    // Drop SIGReg only.
    g.set_sigreg_health(GATE_SIGREG_HEALTH_MIN - 0.05);
    let v = g.evaluate_now();
    assert!(!v.promote);
    assert!(!v.sigreg_ok);
    assert!(v.probe_ok, "probe should still pass: {:?}", v.metrics);
    assert!(v.voe_ok, "voe should still pass: {:?}", v.metrics);
    assert!(
        v.straighten_ok,
        "straighten should still pass: {:?}",
        v.metrics
    );
    assert_eq!(v.first_veto(), Some(GateCondition::ClusterSigRegHealth));
}

#[test]
fn held_out_probe_alone_vetoes() {
    let mut g = FourConditionRollbackGate::new();
    g.min_samples = 2;
    // Incoherent residuals: random-ish sign flips so mean residual is
    // useless → low sign agreement.
    for t in 0..20u64 {
        let mut z_t = zero_latent();
        let mut z_tp1 = zero_latent();
        // Different dims jump each step with alternating signs that
        // disagree across the buffer.
        for i in 0..LATENT_DIM {
            let sign = if (t as usize + i).is_multiple_of(3) {
                1.0
            } else if ((t as usize + i) % 3) == 1 {
                -1.0
            } else {
                0.5 * if t % 2 == 0 { 1.0 } else { -1.0 }
            };
            z_t[i] = (t as f32) * 0.01 * sign;
            z_tp1[i] = z_t[i] + sign * (1.0 + (i % 7) as f32);
        }
        // Perfect prediction + healthy SIGReg so only probe can fail.
        let _ = g.observe_transition(&z_t, &z_tp1, &z_tp1, Some(0.99));
    }
    let v = g.verdict();
    // Probe may or may not fall below 0.70 depending on residual pattern;
    // force the metric path by constructing a verdict with only probe low.
    let mut metrics = v.metrics;
    metrics.held_out_probe_accuracy = GATE_HELD_OUT_PROBE_MIN - 0.05;
    metrics.sigreg_health = 0.99;
    metrics.voe_surprise_diff = 1.0;
    metrics.temporal_straightening = 1.0;
    let forced = GateVerdict::evaluate(metrics);
    assert!(!forced.promote);
    assert!(!forced.probe_ok);
    assert!(forced.sigreg_ok);
    assert!(forced.voe_ok);
    assert!(forced.straighten_ok);
    assert_eq!(
        forced.first_veto(),
        Some(GateCondition::HeldOutProbeAccuracy)
    );
}

#[test]
fn voe_alone_vetoes() {
    // Force metrics path: only VoE below threshold.
    let metrics = RollbackGateMetrics {
        sigreg_health: 0.99,
        held_out_probe_accuracy: 0.95,
        voe_surprise_diff: GATE_VOE_DIFF_MIN - 0.01,
        temporal_straightening: 0.95,
    };
    let v = GateVerdict::evaluate(metrics);
    assert!(!v.promote);
    assert!(!v.voe_ok);
    assert!(v.sigreg_ok && v.probe_ok && v.straighten_ok);
    assert_eq!(
        v.first_veto(),
        Some(GateCondition::VoeSurpriseDifferentiation)
    );

    // Online path: bad predictor (z_hat far from z_tp1) → high real
    // surprise; null may be similar or lower → low diff score.
    let mut g = FourConditionRollbackGate::new();
    g.min_samples = 2;
    for t in 0..10u64 {
        let z_t = unit_step(0, t as f32);
        let z_tp1 = unit_step(0, (t + 1) as f32);
        // Terrible prediction: point far away.
        let z_hat = unit_step(0, 1000.0);
        let _ = g.observe_transition(&z_t, &z_hat, &z_tp1, Some(0.99));
    }
    let m = g.metrics();
    assert!(
        m.voe_surprise_diff < GATE_VOE_DIFF_MIN,
        "bad predictor should fail VoE diff: {}",
        m.voe_surprise_diff
    );
}

#[test]
fn temporal_straightening_alone_vetoes() {
    let metrics = RollbackGateMetrics {
        sigreg_health: 0.99,
        held_out_probe_accuracy: 0.95,
        voe_surprise_diff: 0.95,
        temporal_straightening: GATE_PREDICTED_CURVATURE_MIN - 0.05,
    };
    let v = GateVerdict::evaluate(metrics);
    assert!(!v.promote);
    assert!(!v.straighten_ok);
    assert!(v.sigreg_ok && v.probe_ok && v.voe_ok);
    assert_eq!(v.first_veto(), Some(GateCondition::TemporalStraightening));

    // Online, legacy kind: highly curved path (oscillate) with perfect pred + health.
    let mut g = FourConditionRollbackGate::new().with_straightening_kind(StraighteningKind::ChordOverPath);
    g.min_samples = 2;
    g.trajectory_capacity = 12;
    for t in 0..12u64 {
        // Bounce between two points → path long, chord ~0.
        let v = if t % 2 == 0 { 0.0 } else { 10.0 };
        let z_t = unit_step(0, v);
        let z_tp1 = unit_step(0, if t % 2 == 0 { 10.0 } else { 0.0 });
        let _ = g.observe_transition(&z_t, &z_tp1, &z_tp1, Some(0.99));
    }
    let m = g.metrics();
    assert!(
        m.temporal_straightening < GATE_TEMPORAL_STRAIGHTEN_MIN,
        "oscillation should fail straighten: {}",
        m.temporal_straightening
    );
    let v = g.verdict();
    assert!(!v.straighten_ok);
    assert!(v.blocks_promotion());

    // Online, default kind: a model that predicts the opposite direction on every step.
    let mut g = FourConditionRollbackGate::new();
    g.min_samples = 2;
    for t in 0..12u64 {
        let z_t = unit_step(0, t as f32);
        let z_tp1 = unit_step(0, (t + 1) as f32);
        let v = g.observe_transition(&z_t, &unit_step(0, t as f32 - 1.0), &z_tp1, Some(0.99));
        if t >= 2 {
            assert!(!v.straighten_ok, "{:?}", v.metrics);
        }
    }
    assert!(g.metrics().temporal_straightening < 0.01);
    assert_eq!(g.verdict().failing().last(), Some(&GateCondition::TemporalStraightening));
}

// ── condition 4: temporal straightening (2026-11-14) ──────────────────────────

#[test]
fn default_straightening_is_predicted_curvature_and_legacy_is_selectable() {
    let g = FourConditionRollbackGate::new();
    assert_eq!(g.straightening_kind(), StraighteningKind::PredictedCurvature);
    let g = g.with_straightening_kind(StraighteningKind::ChordOverPath);
    assert_eq!(g.straightening_kind(), StraighteningKind::ChordOverPath);
}

#[test]
fn oscillation_predicted_exactly_passes_the_new_kind_and_fails_the_legacy_one() {
    let feed_bounce = |g: &mut FourConditionRollbackGate| {
        g.min_samples = 2;
        for t in 0..16u64 {
            let (a, b) = if t % 2 == 0 { (0.0, 10.0) } else { (10.0, 0.0) };
            g.observe_transition(&unit_step(0, a), &unit_step(0, b), &unit_step(0, b), Some(0.99));
        }
    };
    let mut new = FourConditionRollbackGate::new();
    feed_bounce(&mut new);
    let (legacy_score, new_score) = new.straightening_scores();
    assert!(new_score > 0.99 && new.verdict().straighten_ok, "{new_score}");
    assert!(legacy_score < GATE_TEMPORAL_STRAIGHTEN_MIN);
    assert!(new.verdict().promote);
    let mut legacy = FourConditionRollbackGate::new().with_straightening_kind(StraighteningKind::ChordOverPath);
    feed_bounce(&mut legacy);
    assert_eq!(legacy.metrics().temporal_straightening, legacy_score);
    assert!(!legacy.verdict().straighten_ok && !legacy.evaluate_now().promote);
    // Both gates report the same pair of scores; only the one feeding condition 4 differs.
    assert_eq!(legacy.straightening_scores(), new.straightening_scores());
}

#[test]
fn copy_last_is_refused_by_the_new_straightening_kind() {
    let mut g = FourConditionRollbackGate::new();
    for t in 0..16u64 {
        let z_t = unit_step(0, t as f32);
        g.observe_transition(&z_t, &z_t, &unit_step(0, (t + 1) as f32), Some(0.99));
    }
    let m = g.metrics();
    assert!((m.temporal_straightening - 0.5).abs() < 1e-6, "{m:?}");
    assert!(!g.verdict().straighten_ok);
    // The legacy score cannot tell copy-last from a perfect model on a straight path.
    assert_eq!(g.straightening_scores().0, 1.0);
}

#[test]
fn legacy_kind_is_judged_against_its_own_floor() {
    // A window with chord / path between the two floors: a three-step zigzag in 2-D.
    let mut g = FourConditionRollbackGate::new()
        .with_straightening_kind(StraighteningKind::ChordOverPath)
        .with_trajectory_capacity(3);
    g.min_samples = 2;
    let pts = [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (2.0, 1.0)];
    for w in pts.windows(2) {
        let (mut a, mut b) = (zero_latent(), zero_latent());
        (a[0], a[1], b[0], b[1]) = (w[0].0, w[0].1, w[1].0, w[1].1);
        g.observe_transition(&a, &b, &b, Some(0.99));
    }
    let s = g.metrics().temporal_straightening;
    assert!(s > GATE_TEMPORAL_STRAIGHTEN_MIN && s < GATE_PREDICTED_CURVATURE_MIN, "{s}");
    assert!(g.verdict().straighten_ok, "legacy floor 0.50 applies to the legacy kind");
}

#[test]
fn straightening_window_is_a_ring_and_cold_start_does_not_veto() {
    let mut g = FourConditionRollbackGate::new().with_trajectory_capacity(4);
    // Backwards predictions first, then exact ones: only the last four transitions count.
    for t in 0..12u64 {
        let z_t = unit_step(0, t as f32);
        let z_tp1 = unit_step(0, (t + 1) as f32);
        let hat = if t < 8 { unit_step(0, t as f32 - 1.0) } else { z_tp1 };
        g.observe_transition(&z_t, &hat, &z_tp1, Some(0.99));
    }
    assert!(g.metrics().temporal_straightening > 0.99);
    g.reset_stats();
    assert_eq!(g.metrics().temporal_straightening, 1.0, "cold start after reset");
    assert_eq!(g.straightening_scores(), (1.0, 1.0));
}

#[test]
fn and_requires_all_four() {
    // Three good, one bad → no promote (already covered per-condition).
    // All at threshold → promote.
    let m = RollbackGateMetrics {
        sigreg_health: GATE_SIGREG_HEALTH_MIN,
        held_out_probe_accuracy: GATE_HELD_OUT_PROBE_MIN,
        voe_surprise_diff: GATE_VOE_DIFF_MIN,
        temporal_straightening: GATE_PREDICTED_CURVATURE_MIN,
    };
    assert!(GateVerdict::evaluate(m).promote);
    // The online gate applies the same floors.
    let mut g = FourConditionRollbackGate::new();
    g.min_samples = 100;
    assert!(g.evaluate_now().promote);
}

#[test]
fn to_log_records_veto() {
    let mut g = FourConditionRollbackGate::new();
    g.set_sigreg_health(0.1);
    g.min_samples = 100; // keep other metrics at 1.0
    let _ = g.evaluate_now();
    let log = g.to_log();
    assert_eq!(log.kind, "lewm.rollback_gate");
    assert!(!log.promote);
    assert_eq!(log.first_veto, Some("cluster_sigreg_health"));
}

// ── condition 2: the probe (2026-11-14) ────────────────────────────────────

fn scaled(z: &Latent, k: f32) -> Latent {
    let mut o = *z;
    for v in o.iter_mut() {
        *v *= k;
    }
    o
}

/// Deterministic SYNTHETIC mean-reverting stream `z' = 0.5 z`, with `z` in [-1, 1).
fn mean_reverting_pairs(n: usize) -> Vec<(Latent, Latent)> {
    let mut s = 0x2545_F491_4F6C_DD1Du64;
    let mut next = move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        ((s >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
    };
    (0..n)
        .map(|_| {
            let mut z = zero_latent();
            for v in z.iter_mut() {
                *v = next();
            }
            (z, scaled(&z, 0.5))
        })
        .collect()
}

fn feed(g: &mut FourConditionRollbackGate, data: &[(Latent, Latent)], hat: impl Fn(&Latent, &Latent) -> Latent) {
    for (z, z1) in data {
        g.observe_transition(z, &hat(z, z1), z1, Some(0.95));
    }
}

#[test]
fn default_probe_is_beats_copy_last_and_legacy_is_selectable() {
    assert_eq!(FourConditionRollbackGate::new().probe_kind(), GateProbeKind::BeatsCopyLast);
    let g = FourConditionRollbackGate::new().with_probe_kind(GateProbeKind::SignAgreement);
    assert_eq!(g.probe_kind(), GateProbeKind::SignAgreement);
}

#[test]
fn perfect_model_on_mean_reverting_data_passes_the_new_probe_but_not_the_legacy_one() {
    let data = mean_reverting_pairs(48);
    let mut new = FourConditionRollbackGate::new();
    feed(&mut new, &data, |_, z1| *z1);
    assert!(new.metrics().held_out_probe_accuracy > 0.99);
    assert!(new.evaluate_now().probe_ok);

    let mut legacy = FourConditionRollbackGate::new().with_probe_kind(GateProbeKind::SignAgreement);
    feed(&mut legacy, &data, |_, z1| *z1);
    let acc = legacy.metrics().held_out_probe_accuracy;
    assert!(acc < GATE_HELD_OUT_PROBE_MIN, "legacy sign agreement on mean-reverting data: {acc}");
    assert!(!legacy.evaluate_now().probe_ok);
}

#[test]
fn copy_last_model_is_refused_by_the_probe() {
    let data = mean_reverting_pairs(48);
    let mut g = FourConditionRollbackGate::new();
    feed(&mut g, &data, |z, _| *z);
    let m = g.metrics();
    assert!(m.held_out_probe_accuracy < 0.01, "{m:?}");
    let v = g.evaluate_now();
    assert!(!v.probe_ok && !v.promote);
}

#[test]
fn model_that_is_worse_than_copy_last_is_refused() {
    let data = mean_reverting_pairs(48);
    let mut g = FourConditionRollbackGate::new();
    feed(&mut g, &data, |z, _| scaled(z, -1.0));
    assert!(g.metrics().held_out_probe_accuracy < 0.5);
    assert!(!g.evaluate_now().probe_ok);
}

#[test]
fn good_model_on_mean_reverting_data_promotes_when_the_other_conditions_hold() {
    // Mean-reverting data is a decaying path, so it is straight enough only when the window is
    // short; use a stream that decays monotonically and a healthy SIGReg score.
    let mut g = FourConditionRollbackGate::new();
    g.min_samples = 3;
    let mut z = zero_latent();
    z[0] = 64.0;
    for _ in 0..16 {
        let z1 = scaled(&z, 0.9);
        let v = g.observe_transition(&z, &z1, &z1, Some(0.95));
        z = z1;
        if g.transition_count() >= 4 {
            assert!(v.probe_ok && v.promote, "{:?}", v.metrics);
        }
    }
}

#[test]
fn stationary_stream_with_an_exact_model_does_not_false_veto_the_probe() {
    let mut g = FourConditionRollbackGate::new();
    let z = unit_step(3, 1.0);
    for _ in 0..8 {
        g.observe_transition(&z, &z, &z, Some(0.95));
    }
    assert_eq!(g.metrics().held_out_probe_accuracy, 1.0);
    // ... but a wrong model on a stationary stream is refused.
    let mut bad = FourConditionRollbackGate::new();
    for _ in 0..8 {
        bad.observe_transition(&z, &unit_step(3, 2.0), &z, Some(0.95));
    }
    assert_eq!(bad.metrics().held_out_probe_accuracy, 0.0);
}

#[test]
fn legacy_sign_agreement_numbers_are_unchanged() {
    // Pure drift: Δz = +1 in dim 0 on every pair, so every non-zero cell agrees.
    let pairs: Vec<ProbePair> = (0..10)
        .map(|t| ProbePair {
            z_t: unit_step(0, t as f32),
            z_hat: zero_latent(),
            z_tp1: unit_step(0, (t + 1) as f32),
        })
        .collect();
    assert_eq!(sign_agreement(pairs.iter()), 1.0);
    assert_eq!(GateProbeKind::SignAgreement.score(pairs.iter()), 1.0);
    // The prediction is ignored by the legacy score and decisive for the new one.
    assert_eq!(beats_copy_last(pairs.iter()), 0.0);
}

#[test]
fn probe_window_is_a_ring_and_cold_start_does_not_veto() {
    let mut g = FourConditionRollbackGate::new().with_probe_capacity(4);
    let data = mean_reverting_pairs(12);
    // Bad model first, then a good one: only the last four pairs count.
    feed(&mut g, &data[..8], |z, _| *z);
    feed(&mut g, &data[8..], |_, z1| *z1);
    assert_eq!(g.metrics().held_out_probe_accuracy, 1.0);
    g.reset_stats();
    assert_eq!(g.metrics().held_out_probe_accuracy, 1.0, "cold start after reset");
}
