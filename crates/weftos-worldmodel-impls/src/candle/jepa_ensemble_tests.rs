//! Ensemble regression for the default ViT-tiny (lane S). All data SYNTHETIC.
//!
//! One 288-step run's θ margin is chaotic: under one-ulp init perturbations CPU passes the
//! 0.9 margin on 9 of 13 runs and Metal on 31 of 39 (lane P). The ensemble scores the
//! config instead of one draw: five training seeds, the same rule on CPU and Metal.
//!
//! | check | rule |
//! |---|---|
//! | θ ratio `model_err / min(copy_last, linear)` | median ≤ [`MARGIN`] (the harness margin) |
//! | ξ ratio (target pairing) | median ≤ [`MARGIN`] |
//! | held-out participation ratio | ≥ [`MIN_PR`] on every run |
//! | collapsed coordinates | 0 on every run |
//! | held-out SIGReg health | median ≥ [`MIN_HEALTH`] |
//! | gate probe (θ pairing) | passes on a strict majority of runs |
//!
//! Median of 5 ≤ 0.9 means at least 3 runs pass. With lane P's per-run pass rates that is a
//! 0.94 (Metal) / 0.83 (CPU) pass probability, against about 1e-7 for a model no better than
//! the linear baseline (ratio ≈ 1.0, sd 0.035). The derivation is in the lane S note.
//! `LEWM_ENSEMBLE_N=k` (1 to 5) runs the first k seeds; the default is 5 on both devices.

use super::bakeoff_tests::fixture;
use super::*;
use crate::eval_harness::EvalReport;

/// Training seeds of the ensemble, fixed before the first run (no jitter).
pub(crate) const ENSEMBLE_SEEDS: [u64; 5] = [21, 22, 23, 24, 25];
/// Per-run and median margin: the eval harness's `beat_margin`.
pub(crate) const MARGIN: f64 = 0.9;
/// Held-out participation-ratio floor, every run.
pub(crate) const MIN_PR: f64 = 8.0;
/// Held-out SIGReg health floor, on the median.
pub(crate) const MIN_HEALTH: f64 = 0.85;

/// What one run contributes to the ensemble.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct RunStats {
    pub seed: u64,
    pub theta_ratio: f64,
    pub xi_ratio: f64,
    pub held_pr: f64,
    pub collapsed: usize,
    pub health: f64,
    pub probe_ok: bool,
}

/// `model_err / min(copy_last, linear)`; `beats_baselines` is exactly `ratio ≤ beat_margin`.
pub(crate) fn baseline_ratio(e: &EvalReport) -> f64 {
    e.model_err as f64 / e.baselines.copy_last.min(e.baselines.linear) as f64
}

impl RunStats {
    pub(crate) fn from_report(seed: u64, r: &JepaTrainReport) -> Self {
        RunStats {
            seed,
            theta_ratio: baseline_ratio(&r.eval),
            xi_ratio: baseline_ratio(&r.eval_target),
            held_pr: r.held_out_participation_ratio as f64,
            collapsed: r.eval.collapsed_coords.len(),
            health: r.held_out_sigreg_health as f64,
            probe_ok: r.eval.gate.probe_ok,
        }
    }
}

/// Median; NaN sorts last (`total_cmp`), an empty slice gives NaN. Even length: mean of the two middle values.
pub(crate) fn median(v: &[f64]) -> f64 {
    let mut s = v.to_vec();
    s.sort_by(f64::total_cmp);
    match s.len() {
        0 => f64::NAN,
        n if n % 2 == 1 => s[n / 2],
        n => (s[n / 2 - 1] + s[n / 2]) / 2.0,
    }
}

/// The ensemble's summary statistics and every rule it broke.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Verdict {
    pub theta_median: f64,
    pub xi_median: f64,
    pub health_median: f64,
    pub probe_passes: usize,
    pub failures: Vec<String>,
}

impl Verdict {
    pub(crate) fn passed(&self) -> bool {
        self.failures.is_empty()
    }
}

/// `x` breaks an upper bound `bar` (NaN breaks it).
fn fails_max(x: f64, bar: f64) -> bool {
    x.is_nan() || x > bar
}

/// `x` breaks a lower bound `bar` (NaN breaks it).
fn fails_min(x: f64, bar: f64) -> bool {
    x.is_nan() || x < bar
}

/// Apply the ensemble rules (module docs). NaN fails every bound.
pub(crate) fn verdict(runs: &[RunStats]) -> Verdict {
    let col = |f: fn(&RunStats) -> f64| median(&runs.iter().map(f).collect::<Vec<_>>());
    let (theta_median, xi_median, health_median) = (col(|r| r.theta_ratio), col(|r| r.xi_ratio), col(|r| r.health));
    let probe_passes = runs.iter().filter(|r| r.probe_ok).count();
    let mut failures = Vec::new();
    if runs.is_empty() {
        failures.push("no runs".to_string());
    }
    if fails_max(theta_median, MARGIN) {
        failures.push(format!("median θ ratio {theta_median:.4} > {MARGIN}"));
    }
    if fails_max(xi_median, MARGIN) {
        failures.push(format!("median ξ ratio {xi_median:.4} > {MARGIN}"));
    }
    if fails_min(health_median, MIN_HEALTH) {
        failures.push(format!("median held-out health {health_median:.4} < {MIN_HEALTH}"));
    }
    for r in runs {
        if fails_min(r.held_pr, MIN_PR) {
            failures.push(format!("seed {}: held-out PR {:.2} < {MIN_PR}", r.seed, r.held_pr));
        }
        if r.collapsed != 0 {
            failures.push(format!("seed {}: {} collapsed coordinates", r.seed, r.collapsed));
        }
    }
    if probe_passes * 2 <= runs.len() {
        failures.push(format!("probe passed on {probe_passes} of {} runs (needs a strict majority)", runs.len()));
    }
    Verdict { theta_median, xi_median, health_median, probe_passes, failures }
}

/// Model variants: the default, and the deliberately broken models the ensemble must reject.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Variant {
    /// `JepaTrainConfig::default()`.
    Default,
    /// Action codes zeroed for training and evaluation: the predictor cannot use the action.
    ActionBlind,
    /// `JepaTrainConfig::lane_g()`, which collapses (lane J).
    LaneG,
    /// 0 epochs: untrained encoder, adaLN-zero predictor (copy-last).
    Untrained,
}

/// Zero every action code (the `ActionBlind` data transform).
pub(crate) fn action_blind(data: &[FrameTransition]) -> Vec<FrameTransition> {
    data.iter().map(|t| FrameTransition { action: Action::null(), ..t.clone() }).collect()
}

/// One change from the default for the lane U bake-off; `None` keeps the base config's value.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct Candidate {
    pub steps: Option<usize>,
    pub lr_final_ratio: Option<f64>,
    pub vic_var_weight: Option<f64>,
    pub vic_cov_weight: Option<f64>,
    pub pred_hidden: Option<usize>,
    pub pred_depth: Option<usize>,
    pub ema_tau: Option<f64>,
    pub batch_size: Option<usize>,
}

impl Candidate {
    /// From `LEWM_STEPS`, `LEWM_LR_FINAL`, `LEWM_VICVAR`, `LEWM_VICCOV`, `LEWM_PRED_HIDDEN`,
    /// `LEWM_PRED_DEPTH`, `LEWM_TAU` and `LEWM_BATCH` (unset = keep the default).
    pub(crate) fn from_env() -> Self {
        fn f<T: std::str::FromStr>(k: &str) -> Option<T> {
            std::env::var(k).ok().and_then(|v| v.parse().ok())
        }
        Candidate {
            steps: f("LEWM_STEPS"),
            lr_final_ratio: f("LEWM_LR_FINAL"),
            vic_var_weight: f("LEWM_VICVAR"),
            vic_cov_weight: f("LEWM_VICCOV"),
            pred_hidden: f("LEWM_PRED_HIDDEN"),
            pred_depth: f("LEWM_PRED_DEPTH"),
            ema_tau: f("LEWM_TAU"),
            batch_size: f("LEWM_BATCH"),
        }
    }

    /// Apply to `c`. On [`Variant::LaneG`] the knobs that define lane J's delta from lane G
    /// (batch, τ, hinge and decorrelation weights) keep lane G's values.
    pub(crate) fn apply(&self, c: &mut JepaTrainConfig, variant: Variant) {
        c.lr_final_ratio = self.lr_final_ratio.unwrap_or(c.lr_final_ratio);
        c.predictor.hidden_dim = self.pred_hidden.unwrap_or(c.predictor.hidden_dim);
        c.predictor.depth = self.pred_depth.unwrap_or(c.predictor.depth);
        if variant != Variant::LaneG {
            c.vic_var_weight = self.vic_var_weight.unwrap_or(c.vic_var_weight);
            c.vic_cov_weight = self.vic_cov_weight.unwrap_or(c.vic_cov_weight);
            c.ema_tau = self.ema_tau.unwrap_or(c.ema_tau);
            c.batch_size = self.batch_size.unwrap_or(c.batch_size);
        }
    }
}

/// The run's config: `variant` with `cand` applied on `device`, training seed `seed`, 288
/// steps (or `cand.steps`) of the `std` fixture.
pub(crate) fn variant_config(variant: Variant, cand: &Candidate, device: ComputeDevice, seed: u64, holdout_days: usize, n_train: usize) -> JepaTrainConfig {
    let mut base = if variant == Variant::LaneG { JepaTrainConfig::lane_g() } else { JepaTrainConfig::default() };
    cand.apply(&mut base, variant);
    let steps = cand.steps.unwrap_or(288);
    let epochs = if variant == Variant::Untrained { 0 } else { steps.div_ceil(n_train.div_ceil(base.batch_size)) };
    let mut c = JepaTrainConfig { seed, holdout_days, device, epochs, ..base };
    if variant == Variant::ActionBlind {
        c.action_from_scalar = |_| Action::null();
    }
    c
}

/// Run count from `LEWM_ENSEMBLE_N` (clamped to 1..=5, default 5).
fn ensemble_n() -> usize {
    std::env::var("LEWM_ENSEMBLE_N").ok().and_then(|v| v.parse().ok()).unwrap_or(ENSEMBLE_SEEDS.len()).clamp(1, ENSEMBLE_SEEDS.len())
}

/// Train `variant` on the first `n` ensemble seeds and print one `ENS|` line per run.
pub(crate) fn ensemble_runs(device: ComputeDevice, n: usize, variant: Variant, cand: &Candidate) -> Vec<RunStats> {
    let (data, holdout) = fixture("std");
    let data = if variant == Variant::ActionBlind { action_blind(&data) } else { data };
    let n_days = data.iter().map(|t| t.day).max().unwrap_or(0) as usize + 1;
    let n_train = data.iter().filter(|t| (t.day as usize) < n_days - holdout).count();
    ENSEMBLE_SEEDS[..n]
        .iter()
        .map(|&seed| {
            let t0 = std::time::Instant::now();
            let r = train_jepa(&data, variant_config(variant, cand, device, seed, holdout, n_train)).unwrap();
            let s = RunStats::from_report(seed, &r);
            eprintln!(
                "ENS|{device:?}|{variant:?}|seed {seed}|theta {:.1}/{:.1}/{:.1} ratio {:.4}|xi ratio {:.4}|PR {:.2}|collapsed {}|health {:.3}|probe {} ({:.3})|{:.1}s",
                r.eval.model_err, r.eval.baselines.copy_last, r.eval.baselines.linear, s.theta_ratio, s.xi_ratio, s.held_pr, s.collapsed, s.health,
                s.probe_ok, r.eval.gate.metrics.held_out_probe_accuracy, t0.elapsed().as_secs_f64()
            );
            s
        })
        .collect()
}

fn ensemble_verdict(device: ComputeDevice, n: usize, variant: Variant) -> Verdict {
    ensemble_verdict_with(device, n, variant, &Candidate::default())
}

fn ensemble_verdict_with(device: ComputeDevice, n: usize, variant: Variant, cand: &Candidate) -> Verdict {
    let v = verdict(&ensemble_runs(device, n, variant, cand));
    eprintln!(
        "ENSEMBLE|{device:?}|{variant:?}|n {n}|median theta {:.4}|median xi {:.4}|median health {:.3}|probe {}/{n}|passed {}|{:?}",
        v.theta_median, v.xi_median, v.health_median, v.probe_passes, v.passed(), v.failures
    );
    v
}

/// The release regression on `device`: every ensemble rule must hold (module docs).
pub(crate) fn ensemble_regression(device: ComputeDevice, n: usize) {
    let v = ensemble_verdict(device, n, Variant::Default);
    assert!(v.passed(), "ensemble failed: {:?}", v.failures);
}

/// Device for the broken-model checks: `LEWM_DEVICE=metal` (with the feature) or CPU.
fn broken_device() -> ComputeDevice {
    match std::env::var("LEWM_DEVICE").as_deref() {
        Ok("metal") => ComputeDevice::Metal,
        _ => ComputeDevice::Cpu,
    }
}

/// Release-only regression for the default config on the default ViT-tiny: the five-seed
/// ensemble must pass on CPU (5 × ~75 s release; `LEWM_ENSEMBLE_N` shrinks it).
///
/// ```text
/// cargo test --release -p weftos-worldmodel-impls --features candle \
///     jepa::ensemble_tests::default_vit_tiny_does_not_collapse -- --ignored --exact --nocapture
/// ```
#[test]
#[ignore = "release only, ~6 min of CPU for 5 runs"]
fn default_vit_tiny_does_not_collapse() {
    ensemble_regression(ComputeDevice::Cpu, ensemble_n());
}

/// The same ensemble rule on the Apple GPU (`--features metal`, 5 × ~20 s release).
#[cfg(feature = "metal")]
#[test]
#[ignore = "release only, ~2 min of GPU for 5 runs"]
fn default_vit_tiny_does_not_collapse_on_metal() {
    ensemble_regression(ComputeDevice::Metal, ensemble_n());
}

/// The ensemble must reject a predictor that never sees the action.
#[test]
#[ignore = "release only; LEWM_DEVICE=metal for the GPU"]
fn ensemble_rejects_action_blind_model() {
    assert!(!ensemble_verdict(broken_device(), ensemble_n(), Variant::ActionBlind).passed());
}

/// The ensemble must reject lane G's collapsing config.
#[test]
#[ignore = "release only; LEWM_DEVICE=metal for the GPU"]
fn ensemble_rejects_lane_g_collapse() {
    assert!(!ensemble_verdict(broken_device(), ensemble_n(), Variant::LaneG).passed());
}

/// The ensemble must reject an untrained model (0 epochs).
#[test]
#[ignore = "release only; LEWM_DEVICE=metal for the GPU"]
fn ensemble_rejects_untrained_model() {
    assert!(!ensemble_verdict(broken_device(), ensemble_n(), Variant::Untrained).passed());
}

/// Lane U bake-off runner: the ensemble for `LEWM_VARIANT` (`default` | `action_blind` |
/// `lane_g` | `untrained`) with [`Candidate::from_env`] applied, on `LEWM_DEVICE`. Prints
/// the `ENSEMBLE|` verdict line and asserts nothing.
///
/// ```text
/// LEWM_DEVICE=metal LEWM_STEPS=576 cargo test --release -p weftos-worldmodel-impls \
///     --features metal candle::jepa::ensemble_tests::ensemble_candidate -- --ignored --exact --nocapture
/// ```
#[test]
#[ignore = "release only; lane U bake-off runner, configured by LEWM_* env vars"]
fn ensemble_candidate() {
    let variant = match std::env::var("LEWM_VARIANT").as_deref() {
        Ok("action_blind") => Variant::ActionBlind,
        Ok("lane_g") => Variant::LaneG,
        Ok("untrained") => Variant::Untrained,
        _ => Variant::Default,
    };
    let cand = Candidate::from_env();
    eprintln!("CAND|{}|{variant:?}|{cand:?}", std::env::var("LEWM_TAG").unwrap_or_default());
    ensemble_verdict_with(broken_device(), ensemble_n(), variant, &cand);
}

#[cfg(test)]
#[path = "jepa_ensemble_unit_tests.rs"]
mod unit;
