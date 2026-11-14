extern crate std;
use alloc::vec::Vec;
use std::eprintln;

use super::*;
use weftos_worldmodel_core::zero_latent;

/// Deterministic noise in [-1, 1).
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> f32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        ((self.0 >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
    }
}

/// Labelled SYNTHETIC fixture (no recorded data exists yet, plan W2): each coordinate drifts
/// at a fixed signed rate `v_d` scaled by a piecewise-constant action `a` in [0.5, 1.5]
/// (new action every 8 steps), plus noise. A model that sees the action predicts it almost
/// exactly; copy-last and the linear baseline cannot know the action. `scale` shrinks the
/// drift (a collapsed latent), `noise` is the per-step noise amplitude. When `day_len` is
/// non-zero the latent also jumps by a per-day random offset (`day_jump` amplitude) at each
/// day boundary, and the returned day tags number the days from 0.
fn fixture_days(
    n: usize,
    scale: f32,
    noise: f32,
    seed: u64,
    day_len: usize,
    day_jump: f32,
) -> (Vec<Transition>, Vec<u16>) {
    let mut rng = Rng(seed);
    let v: Vec<f32> =
        (0..LATENT_DIM).map(|d| if d % 2 == 0 { 0.015 } else { -0.015 } * scale).collect();
    let mut z = zero_latent();
    for d in 0..LATENT_DIM {
        z[d] = -(n as f32) * v[d] / 2.0;
    }
    let (mut out, mut days, mut a) = (Vec::new(), Vec::new(), 1.0f32);
    for t in 0..n {
        if day_len > 0 && t > 0 && t % day_len == 0 {
            for zd in z.iter_mut() {
                *zd += day_jump * rng.next();
            }
        }
        if t % 8 == 0 {
            a = 1.0 + 0.5 * rng.next();
        }
        let mut next = z;
        for d in 0..LATENT_DIM {
            next[d] += a * v[d] + noise * rng.next();
        }
        out.push(Transition { z_t: z, action: a, z_tp1: next });
        days.push(t.checked_div(day_len).unwrap_or(0) as u16);
        z = next;
    }
    (out, days)
}

fn fixture(n: usize, scale: f32, noise: f32, seed: u64) -> Vec<Transition> {
    fixture_days(n, scale, noise, seed, 0, 0.0).0
}

/// 240 steps = 5 days x 48, each day starting from a per-day offset.
fn day_fixture() -> (Vec<Transition>, Vec<u16>) {
    fixture_days(240, 1.0, 0.0005, 7, 48, 0.3)
}

fn true_model(scale: f32) -> impl FnMut(&Latent, f32) -> Latent {
    move |z: &Latent, a: f32| {
        let mut o = *z;
        for (d, od) in o.iter_mut().enumerate() {
            *od += a * if d % 2 == 0 { 0.015 } else { -0.015 } * scale;
        }
        o
    }
}

#[test]
fn a_model_that_uses_the_action_beats_both_baselines_and_is_promoted() {
    let data = fixture(240, 1.0, 0.0005, 7);
    let r = evaluate(&data, true_model(1.0), EvalConfig::default());
    assert!(r.beats_baselines, "{r:?}");
    assert!(r.model_err < r.baselines.copy_last && r.model_err < r.baselines.linear);
    assert!(r.collapsed_coords.is_empty());
    assert!(r.promote, "vetoes {:?} health {} gate {:?}", r.vetoes, r.sigreg_health, r.gate);
    // The contiguous path leaves the day fields empty.
    assert_eq!((r.n_train_days, r.n_held_out_days), (0, 0));
    assert!(r.held_out_days.is_empty());
}

#[test]
fn a_model_no_better_than_copy_last_is_refused() {
    let data = fixture(240, 1.0, 0.0005, 7);
    let r = evaluate(&data, |z, _| *z, EvalConfig::default());
    assert!(!r.promote);
    assert!(r.vetoes.contains(&EvalVeto::DoesNotBeatBaseline));
    assert!(r.vetoes.contains(&EvalVeto::Gate(GateCondition::VoeSurpriseDifferentiation)));
}

#[test]
fn a_model_that_ignores_the_action_does_not_beat_the_linear_baseline() {
    let data = fixture(240, 1.0, 0.0005, 7);
    let mut mean = true_model(1.0);
    let r = evaluate(&data, move |z, _| mean(z, 1.0), EvalConfig::default());
    assert!(!r.beats_baselines && !r.promote, "{r:?}");
}

#[test]
fn a_collapsed_latent_trips_the_activity_floor_even_with_a_perfect_model() {
    let data = fixture(240, 0.01, 0.0, 7);
    let r = evaluate(&data, true_model(0.01), EvalConfig::default());
    assert_eq!(r.collapsed_coords.len(), LATENT_DIM, "every coordinate is under the floor");
    assert!(!r.promote && r.vetoes.contains(&EvalVeto::ActivityCollapse));
}

#[test]
fn one_live_coordinate_cannot_hide_the_collapsed_ones() {
    let mut data = fixture(240, 0.01, 0.0, 7);
    for (i, t) in data.iter_mut().enumerate() {
        t.z_tp1[0] = (i as f32) * 0.05;
    }
    let held = &data[168..];
    let c = activity_collapse(held, 0.1);
    assert!(!c.contains(&0) && c.len() == LATENT_DIM - 1);
}

#[test]
fn too_little_data_is_refused_rather_than_judged() {
    let r = evaluate(&fixture(10, 1.0, 0.0, 1), true_model(1.0), EvalConfig::default());
    assert_eq!(r.vetoes, alloc::vec![EvalVeto::TooFewTransitions]);
}

#[test]
fn a_model_that_uses_the_action_is_promoted_under_the_by_day_split() {
    let (data, days) = day_fixture();
    let r = evaluate_by_day(&data, &days, true_model(1.0), EvalConfig::default());
    assert!(r.promote, "vetoes {:?} health {} gate {:?}", r.vetoes, r.sigreg_health, r.gate);
    assert_eq!((r.n_train_days, r.n_held_out_days), (4, 1));
    assert_eq!(r.held_out_days, alloc::vec![4]);
    assert_eq!((r.n_train, r.n_held_out), (192, 48));
    assert!(r.model_err < r.baselines.copy_last && r.model_err < r.baselines.linear);
}

#[test]
fn copy_last_is_refused_under_the_by_day_split() {
    let (data, days) = day_fixture();
    let r = evaluate_by_day(&data, &days, |z, _| *z, EvalConfig::default());
    assert!(!r.promote && r.vetoes.contains(&EvalVeto::DoesNotBeatBaseline), "{r:?}");
}

#[test]
fn holdout_days_widens_the_held_out_set() {
    let (data, days) = day_fixture();
    let cfg = EvalConfig { holdout_days: 2, ..EvalConfig::default() };
    let r = evaluate_by_day(&data, &days, true_model(1.0), cfg);
    assert_eq!(r.held_out_days, alloc::vec![3, 4]);
    assert_eq!((r.n_train_days, r.n_held_out_days, r.n_train, r.n_held_out), (3, 2, 144, 96));
}

#[test]
fn days_never_straddle_the_split_even_when_the_tags_are_shuffled() {
    let (data, mut days) = day_fixture();
    // Interleave two days' transitions in the input order.
    days.swap(3, 200);
    days.swap(10, 150);
    for holdout in 1..=3 {
        let s = split::by_day(&data, &days, holdout).unwrap();
        let mut train_days: Vec<u16> = Vec::new();
        let mut held_days: Vec<u16> = Vec::new();
        for (t, &d) in data.iter().zip(&days) {
            if s.held.contains(t) {
                held_days.push(d);
            } else {
                train_days.push(d);
            }
        }
        assert!(train_days.iter().all(|d| !held_days.contains(d)), "a day straddles");
        assert_eq!(s.train.len() + s.held.len(), data.len());
        assert_eq!(s.n_held_days, holdout);
        assert_eq!(s.held_days.len(), holdout);
    }
}

#[test]
fn one_day_of_data_is_refused_as_too_few_days() {
    let data = fixture(240, 1.0, 0.0005, 7);
    let one_day = alloc::vec![3u16; data.len()];
    let r = evaluate_by_day(&data, &one_day, true_model(1.0), EvalConfig::default());
    assert_eq!(r.vetoes, alloc::vec![EvalVeto::TooFewDays]);
    assert!(!r.promote);
    // holdout_days equal to the number of days leaves nothing to train on.
    let (d, days) = day_fixture();
    let cfg = EvalConfig { holdout_days: 5, ..EvalConfig::default() };
    assert_eq!(
        evaluate_by_day(&d, &days, true_model(1.0), cfg).vetoes,
        alloc::vec![EvalVeto::TooFewDays]
    );
}

#[test]
fn mismatched_day_tags_and_tiny_days_are_refused() {
    let (data, days) = day_fixture();
    let r = evaluate_by_day(&data, &days[..10], true_model(1.0), EvalConfig::default());
    assert_eq!(r.vetoes, alloc::vec![EvalVeto::TooFewTransitions]);
    // Two days of 5 transitions each: enough days, too few transitions.
    let tiny: Vec<u16> = (0..10).map(|i| (i / 5) as u16).collect();
    let r = evaluate_by_day(&data[..10], &tiny, true_model(1.0), EvalConfig::default());
    assert_eq!(r.vetoes, alloc::vec![EvalVeto::TooFewTransitions]);
    assert_eq!(r.n_held_out_days, 1);
}

/// SYNTHETIC mean-reverting fixture: `z' = 0.5 z`, with `z` uniform on [-1, 1) per
/// coordinate. The mean residual `Δz = -0.5 z` is about zero, yet each dimension is
/// perfectly predictable from `z`.
fn mean_reverting(n: usize) -> Vec<Transition> {
    let mut rng = Rng(99);
    (0..n)
        .map(|_| {
            let (mut z, mut next) = (zero_latent(), zero_latent());
            for d in 0..LATENT_DIM {
                z[d] = rng.next();
                next[d] = 0.5 * z[d] + 0.002 * rng.next();
            }
            Transition { z_t: z, action: 0.0, z_tp1: next }
        })
        .collect()
}

#[test]
fn the_two_probes_differ_when_the_mean_residual_is_zero_but_dynamics_are_predictable() {
    let data = mean_reverting(240);
    let model = |z: &Latent, _: f32| {
        let mut o = *z;
        for v in o.iter_mut() {
            *v *= 0.5;
        }
        o
    };
    let sign = evaluate(&data, model, EvalConfig::default());
    assert!(sign.probes.linear_lsq > 0.99, "{:?}", sign.probes);
    assert!(sign.probes.sign_agreement < 0.7, "{:?}", sign.probes);
    // Default keeps the sign-agreement behaviour, so the gate vetoes on the probe.
    assert!(sign.gate.metrics.held_out_probe_accuracy < 0.7);
    assert!(!sign.gate.probe_ok);
    // Selecting the linear probe feeds its accuracy into the gate.
    let cfg = EvalConfig { probe: ProbeKind::LinearLsq, ..EvalConfig::default() };
    let lin = evaluate(&data, model, cfg);
    assert_eq!(lin.gate.metrics.held_out_probe_accuracy, lin.probes.linear_lsq);
    assert!(lin.gate.probe_ok);
    // Both runs saw identical probe numbers.
    assert_eq!(sign.probes, lin.probes);
}

#[test]
fn probe_accuracies_on_the_synthetic_fixtures() {
    let data = fixture(240, 1.0, 0.0005, 7);
    let r = evaluate(&data, true_model(1.0), EvalConfig::default());
    eprintln!("contiguous fixture probes: {:?}", r.probes);
    let (d, days) = day_fixture();
    let r = evaluate_by_day(&d, &days, true_model(1.0), EvalConfig::default());
    eprintln!("by-day fixture probes: {:?}", r.probes);
    let r = evaluate(&mean_reverting(240), |z, _| *z, EvalConfig::default());
    eprintln!("mean-reverting fixture probes: {:?}", r.probes);
    assert!(r.probes.linear_lsq.is_finite() && r.probes.sign_agreement.is_finite());
}

#[test]
fn the_offline_path_keeps_the_constant_clock_sigreg_health() {
    use crate::sigreg::WelfordSigRegMonitor;
    let (data, days) = day_fixture();
    let mut monitor = WelfordSigRegMonitor::new(1);
    let mut direct = 1.0f32;
    for t in &data {
        if let Ok(h) = monitor.update_at(&t.z_tp1, 0) {
            direct = h.score;
        }
    }
    let contiguous = evaluate(&data, true_model(1.0), EvalConfig::default());
    let by_day = evaluate_by_day(&data, &days, true_model(1.0), EvalConfig::default());
    assert_eq!(contiguous.sigreg_health, direct);
    assert_eq!(by_day.sigreg_health, direct);
    // Health is computed over all latents, not only the held-out part.
    assert_eq!(by_day.gate.metrics.sigreg_health, direct.clamp(0.0, 1.0));
}
