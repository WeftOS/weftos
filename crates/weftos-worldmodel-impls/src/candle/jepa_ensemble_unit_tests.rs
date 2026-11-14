//! Debug-build tests of the ensemble statistics (synthetic ratio vectors) and of the
//! report-to-stats path on the tiny config. All data SYNTHETIC.

use super::*;
use crate::candle::synthetic::{generate, SyntheticConfig};

fn run(seed: u64, theta: f64) -> RunStats {
    RunStats { seed, theta_ratio: theta, xi_ratio: 0.8, held_pr: 10.0, collapsed: 0, health: 0.9, probe_ok: true }
}

fn runs(thetas: &[f64]) -> Vec<RunStats> {
    thetas.iter().enumerate().map(|(i, &t)| run(21 + i as u64, t)).collect()
}

#[test]
fn median_handles_odd_even_empty_and_nan() {
    assert_eq!(median(&[0.95, 0.82, 0.874, 0.93, 0.83]), 0.874);
    assert!((median(&[0.9, 0.8]) - 0.85).abs() < 1e-12);
    assert!((median(&[0.7, 0.9, 0.8, 1.0]) - 0.85).abs() < 1e-12);
    assert!(median(&[]).is_nan());
    // NaN sorts last: one NaN in five does not move the median, a NaN majority does.
    assert_eq!(median(&[f64::NAN, 0.8, 0.85, 0.83, 0.86]), 0.85);
    assert!(median(&[f64::NAN, f64::NAN, f64::NAN, 0.8, 0.8]).is_nan());
}

#[test]
fn margin_is_the_harness_margin() {
    // The harness margin is an f32 0.9; the ensemble compares f64 ratios against 0.9.
    assert!((MARGIN - JepaTrainConfig::default().eval.beat_margin as f64).abs() < 1e-6);
}

#[test]
fn median_rule_passes_when_three_of_five_clear_the_margin() {
    // Lane P's seed-21 neighbourhood: two misses, three passes.
    let v = verdict(&runs(&[0.947, 0.874, 0.935, 0.824, 0.822]));
    assert!(v.passed(), "{:?}", v.failures);
    assert_eq!(v.theta_median, 0.874);
    // Exactly at the margin passes (the harness uses `≤`).
    assert!(verdict(&runs(&[0.9, 0.9, 0.9, 1.2, 1.2])).passed());
}

#[test]
fn median_rule_fails_when_three_of_five_miss() {
    let v = verdict(&runs(&[0.947, 0.874, 0.935, 0.91, 0.822]));
    assert!(!v.passed());
    assert_eq!(v.theta_median, 0.91);
    assert_eq!(v.failures.len(), 1, "{:?}", v.failures);
    // The null model (no action advantage over linear) fails.
    assert!(!verdict(&runs(&[1.01, 0.98, 1.03, 0.99, 1.0])).passed());
    // NaN ratios fail the median.
    assert!(!verdict(&runs(&[f64::NAN, f64::NAN, f64::NAN, 0.8, 0.8])).passed());
    assert!(!verdict(&[]).passed());
}

#[test]
fn structural_rules_are_all_runs_except_health_which_is_the_median() {
    let good = runs(&[0.85; 5]);
    let mut low_pr = good.clone();
    low_pr[3].held_pr = 7.9;
    assert!(!verdict(&low_pr).passed());
    let mut collapsed = good.clone();
    collapsed[0].collapsed = 1;
    assert!(!verdict(&collapsed).passed());
    // Health: one run under the bar is fine (lane J CPU seed 23 at 0.835), a median under it is not.
    let mut one_low = good.clone();
    one_low[2].health = 0.835;
    assert!(verdict(&one_low).passed());
    let mut three_low = one_low.clone();
    three_low[0].health = 0.84;
    three_low[4].health = 0.849;
    let v = verdict(&three_low);
    assert!(!v.passed() && (v.health_median - 0.849).abs() < 1e-12, "{v:?}");
    // ξ median is checked too.
    let mut xi = good.clone();
    for r in xi.iter_mut().take(3) {
        r.xi_ratio = 0.95;
    }
    assert!(!verdict(&xi).passed());
}

#[test]
fn probe_needs_a_strict_majority() {
    let mut r = runs(&[0.85; 5]);
    for x in r.iter_mut().take(2) {
        x.probe_ok = false;
    }
    assert!(verdict(&r).passed());
    r[2].probe_ok = false;
    let v = verdict(&r);
    assert!(!v.passed() && v.probe_passes == 2);
    // Even count: 2 of 4 is not a majority.
    let mut four = runs(&[0.85; 4]);
    four[0].probe_ok = false;
    four[1].probe_ok = false;
    assert!(!verdict(&four).passed());
}

/// The tiny config (lane G loss on a small ViT): the stats extracted from a real report agree
/// with the harness, and the broken-model transforms do what they say.
#[test]
fn run_stats_from_a_tiny_report_match_the_harness() {
    let data = generate(&SyntheticConfig { image_size: 32, days: 3, sessions_per_day: 2, steps_per_session: 16, blob_radius: 4.0, action_step_px: 3.0, noise: 6, seed: 11, ..SyntheticConfig::default() });
    let tiny = |c: JepaTrainConfig| JepaTrainConfig {
        lr: 1e-2,
        batch_size: 16,
        ema_tau: 0.8,
        encoder: VitTinyConfig { image_size: 32, patch_size: 8, embed_dim: 32, depth: 1, num_heads: 2, latent_dim: LATENT_DIM },
        predictor: AdaLnConfig { hidden_dim: 32, depth: 1, ..AdaLnConfig::default() },
        ..c
    };
    let lane_g = |epochs| JepaTrainConfig { epochs, seed: 3, holdout_days: 1, ..JepaTrainConfig::lane_g() };
    let r = train_jepa(&data, tiny(lane_g(4))).unwrap();
    let s = RunStats::from_report(3, &r);
    assert_eq!(s.theta_ratio <= MARGIN, r.eval.beats_baselines, "{s:?}");
    assert_eq!(s.xi_ratio <= MARGIN, r.eval_target.beats_baselines, "{s:?}");
    assert_eq!((s.probe_ok, s.collapsed), (r.eval.gate.probe_ok, r.eval.collapsed_coords.len()));
    assert_eq!(s.health, r.held_out_sigreg_health as f64);
    assert!(s.theta_ratio.is_finite() && s.theta_ratio > 0.0);

    // 0 epochs: the adaLN-zero predictor is copy-last, so the θ ratio is at least 1.
    let r0 = train_jepa(&data, tiny(lane_g(0))).unwrap();
    assert_eq!(r0.steps, 0);
    let s0 = RunStats::from_report(3, &r0);
    assert!(s0.theta_ratio >= 1.0 - 1e-6, "{s0:?}");
    assert!(!verdict(&[s0; 5]).passed());

    // The action-blind transform zeroes codes and keeps everything else.
    let blind = action_blind(&data);
    assert!(blind.iter().all(|t| t.action.code.iter().all(|&c| c == 0.0)));
    assert!(blind.iter().zip(&data).all(|(b, d)| b.frame_t == d.frame_t && b.action_scalar == d.action_scalar && b.day == d.day));
    let c = variant_config(Variant::ActionBlind, ComputeDevice::Cpu, 21, 1, 192);
    assert!((c.action_from_scalar)(0.7).code.iter().all(|&x| x == 0.0));
    assert_eq!(variant_config(Variant::Untrained, ComputeDevice::Cpu, 21, 1, 192).epochs, 0);
    assert_eq!(variant_config(Variant::Default, ComputeDevice::Cpu, 21, 1, 192).epochs, 48);
}
