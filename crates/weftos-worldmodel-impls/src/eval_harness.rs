//! Evaluation harness for the rollback gate (plan W4, `docs/plans/world-model-data-and-training.md`).
//!
//! The four-condition gate alone cannot tell a model that predicts well from one that merely
//! does not break its own statistics. This harness puts a held-out split and matched baselines
//! in front of it and adds the two jepa-anything checks:
//!
//! - **Matched baselines** on held-out transitions: copy-last (`ẑ = z_t`) and a per-dimension
//!   linear `pred_φ` (`ẑ_i = α_i z_t,i + β_i`, closed-form least squares on the train split).
//!   The model must beat the better of the two by [`EvalConfig::beat_margin`].
//! - **Activity floor** (collapse alarm): every latent coordinate keeps a minimum standard
//!   deviation over the held-out window, checked per coordinate so one live coordinate cannot
//!   hide a collapsed one.
//! - **Geometry audit** ([`orthonormality_error`]): `‖PᵀP − I‖_F` of a learned projection.
//! - **Gate**: the held-out transitions are fed through [`FourConditionRollbackGate`] with the
//!   SIGReg health from a [`WelfordSigRegMonitor`] run over all latents (score after the last latent).
//!
//! The final verdict promotes only when the gate, the baseline comparison and the activity floor
//! all agree. The split is contiguous (last [`EvalConfig::holdout_frac`] held out), a stand-in
//! for the held-out-by-day split of the real dataset (W2), which does not exist yet.

use alloc::vec::Vec;

use weftos_worldmodel_core::{
    surprise_voe, zero_latent, GateCondition, GateVerdict, Latent, LATENT_DIM,
};

use crate::rollback_gate::FourConditionRollbackGate;
use crate::sigreg::WelfordSigRegMonitor;

/// One observed step: latent, the action taken (scalar code), next latent.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Transition {
    /// Latent at `t`.
    pub z_t: Latent,
    /// Action taken at `t` (scalar code; `0.0` when action-free).
    pub action: f32,
    /// Observed latent at `t+1`.
    pub z_tp1: Latent,
}

/// Harness settings.
#[derive(Debug, Clone, Copy)]
pub struct EvalConfig {
    /// Fraction of transitions (from the end) held out.
    pub holdout_frac: f32,
    /// Per-coordinate standard-deviation floor (jepa-anything `factor_min_std`).
    pub activity_floor_std: f32,
    /// Model error must be at most `beat_margin` times the better baseline's error.
    pub beat_margin: f32,
}

impl Default for EvalConfig {
    fn default() -> Self {
        Self { holdout_frac: 0.3, activity_floor_std: 0.1, beat_margin: 0.9 }
    }
}

/// Why the harness said no.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvalVeto {
    /// A rollback-gate condition failed.
    Gate(GateCondition),
    /// The model did not beat the better matched baseline on held-out data.
    DoesNotBeatBaseline,
    /// At least one coordinate fell under the activity floor.
    ActivityCollapse,
    /// Too few transitions to split and judge.
    TooFewTransitions,
}

/// Mean held-out `‖ẑ − z‖²` of the two matched baselines.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BaselineErrors {
    /// `ẑ = z_t`.
    pub copy_last: f32,
    /// Per-dimension linear fit on the train split.
    pub linear: f32,
}

/// Harness result.
#[derive(Debug, Clone, PartialEq)]
pub struct EvalReport {
    /// Train transitions.
    pub n_train: usize,
    /// Held-out transitions.
    pub n_held_out: usize,
    /// Baseline errors on held-out data.
    pub baselines: BaselineErrors,
    /// Mean held-out `‖ẑ − z‖²` of the model.
    pub model_err: f32,
    /// Model error is at most `beat_margin` times the better baseline.
    pub beats_baselines: bool,
    /// Coordinates under the activity floor.
    pub collapsed_coords: Vec<usize>,
    /// SIGReg health after ingesting all latents.
    pub sigreg_health: f32,
    /// Four-condition gate verdict on the held-out transitions.
    pub gate: GateVerdict,
    /// Everything agrees to promote.
    pub promote: bool,
    /// Reasons for refusing, empty when `promote`.
    pub vetoes: Vec<EvalVeto>,
}

/// Run the harness. `model` is `pred_φ(z_t, action)`.
pub fn evaluate(
    transitions: &[Transition],
    mut model: impl FnMut(&Latent, f32) -> Latent,
    cfg: EvalConfig,
) -> EvalReport {
    let n_hold = ((transitions.len() as f32) * cfg.holdout_frac) as usize;
    let n_train = transitions.len().saturating_sub(n_hold);
    if n_train < 8 || n_hold < 8 {
        let gate = GateVerdict::evaluate(weftos_worldmodel_core::RollbackGateMetrics::perfect());
        return EvalReport {
            n_train,
            n_held_out: n_hold,
            baselines: BaselineErrors { copy_last: f32::NAN, linear: f32::NAN },
            model_err: f32::NAN,
            beats_baselines: false,
            collapsed_coords: Vec::new(),
            sigreg_health: f32::NAN,
            gate,
            promote: false,
            vetoes: alloc::vec![EvalVeto::TooFewTransitions],
        };
    }
    let (train, held) = transitions.split_at(n_train);
    let (alpha, beta) = fit_linear(train);

    let (mut e_copy, mut e_lin, mut e_model) = (0.0f32, 0.0f32, 0.0f32);
    let mut gate = FourConditionRollbackGate::new().with_probe_capacity(held.len());
    // SIGReg judges the embedding distribution over the whole dataset, not just the tail.
    let mut monitor = WelfordSigRegMonitor::new(1);
    let mut health = 1.0f32;
    // Offline pass: a constant clock keeps the monitor's online auto-rollback timer (which resets
    // the Welford state after sustained low health) from truncating the statistics.
    for t in transitions {
        if let Ok(h) = monitor.update_at(&t.z_tp1, 0) {
            health = h.score;
        }
    }
    for t in held {
        let mut lin = zero_latent();
        for d in 0..LATENT_DIM {
            lin[d] = alpha[d] * t.z_t[d] + beta[d];
        }
        let z_hat = model(&t.z_t, t.action);
        e_copy += surprise_voe(&t.z_t, &t.z_tp1);
        e_lin += surprise_voe(&lin, &t.z_tp1);
        e_model += surprise_voe(&z_hat, &t.z_tp1);
        gate.observe_transition(&t.z_t, &z_hat, &t.z_tp1, Some(health));
    }
    let n = held.len() as f32;
    let baselines = BaselineErrors { copy_last: e_copy / n, linear: e_lin / n };
    let model_err = e_model / n;
    let best = baselines.copy_last.min(baselines.linear);
    let beats_baselines = model_err <= cfg.beat_margin * best;

    let collapsed_coords = activity_collapse(held, cfg.activity_floor_std);
    let verdict = gate.evaluate_now();

    let mut vetoes: Vec<EvalVeto> = verdict.failing().into_iter().map(EvalVeto::Gate).collect();
    if !beats_baselines {
        vetoes.push(EvalVeto::DoesNotBeatBaseline);
    }
    if !collapsed_coords.is_empty() {
        vetoes.push(EvalVeto::ActivityCollapse);
    }
    EvalReport {
        n_train,
        n_held_out: held.len(),
        baselines,
        model_err,
        beats_baselines,
        collapsed_coords,
        sigreg_health: health,
        gate: verdict,
        promote: vetoes.is_empty(),
        vetoes,
    }
}

/// Per-dimension least squares `z_{t+1} ≈ α z_t + β` on the train split.
fn fit_linear(train: &[Transition]) -> ([f32; LATENT_DIM], [f32; LATENT_DIM]) {
    let n = train.len() as f32;
    let (mut alpha, mut beta) = ([0.0f32; LATENT_DIM], [0.0f32; LATENT_DIM]);
    for d in 0..LATENT_DIM {
        let (mx, my) = train.iter().fold((0.0, 0.0), |(a, b), t| (a + t.z_t[d] / n, b + t.z_tp1[d] / n));
        let (mut cov, mut var) = (0.0f32, 0.0f32);
        for t in train {
            cov += (t.z_t[d] - mx) * (t.z_tp1[d] - my);
            var += (t.z_t[d] - mx) * (t.z_t[d] - mx);
        }
        alpha[d] = if var > 1e-12 { cov / var } else { 0.0 };
        beta[d] = my - alpha[d] * mx;
    }
    (alpha, beta)
}

/// Coordinates whose standard deviation over the window is under `floor`.
pub fn activity_collapse(window: &[Transition], floor: f32) -> Vec<usize> {
    let n = window.len() as f32;
    let mut out = Vec::new();
    for d in 0..LATENT_DIM {
        let mean = window.iter().map(|t| t.z_tp1[d]).sum::<f32>() / n;
        let var = window.iter().map(|t| (t.z_tp1[d] - mean) * (t.z_tp1[d] - mean)).sum::<f32>() / n;
        if var < floor * floor {
            out.push(d);
        }
    }
    out
}

/// Geometry audit: `‖PᵀP − I‖_F` for a row-major `rows × cols` projection whose columns should
/// be orthonormal. Zero for an orthonormal basis. Returns `None` on a size mismatch.
pub fn orthonormality_error(p: &[f32], rows: usize, cols: usize) -> Option<f32> {
    if rows == 0 || cols == 0 || p.len() != rows * cols {
        return None;
    }
    let mut acc = 0.0f32;
    for i in 0..cols {
        for j in 0..cols {
            let dot: f32 = (0..rows).map(|r| p[r * cols + i] * p[r * cols + j]).sum();
            let d = dot - if i == j { 1.0 } else { 0.0 };
            acc += d * d;
        }
    }
    Some(libm_sqrt(acc))
}

fn libm_sqrt(x: f32) -> f32 {
    crate::math_no_std::sqrtf(x)
}

#[cfg(test)]
mod tests {
    use super::*;

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
    /// drift (a collapsed latent), `noise` is the per-step noise amplitude.
    fn fixture(n: usize, scale: f32, noise: f32, seed: u64) -> Vec<Transition> {
        let mut rng = Rng(seed);
        let v: Vec<f32> = (0..LATENT_DIM).map(|d| if d % 2 == 0 { 0.015 } else { -0.015 } * scale).collect();
        let mut z = zero_latent();
        for d in 0..LATENT_DIM {
            z[d] = -(n as f32) * v[d] / 2.0;
        }
        let (mut out, mut a) = (Vec::new(), 1.0f32);
        for t in 0..n {
            if t % 8 == 0 {
                a = 1.0 + 0.5 * rng.next();
            }
            let mut next = z;
            for d in 0..LATENT_DIM {
                next[d] += a * v[d] + noise * rng.next();
            }
            out.push(Transition { z_t: z, action: a, z_tp1: next });
            z = next;
        }
        out
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
        // Predicts the mean drift: as good as the linear baseline, not better.
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
    fn geometry_audit_separates_orthonormal_from_skewed_bases() {
        // 3x2 with orthonormal columns.
        let q = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];
        assert!(orthonormality_error(&q, 3, 2).unwrap() < 1e-6);
        let skew = [1.0, 1.0, 0.0, 1.0, 0.0, 0.0];
        assert!(orthonormality_error(&skew, 3, 2).unwrap() > 0.5);
        assert!(orthonormality_error(&q, 3, 3).is_none());
    }
}
