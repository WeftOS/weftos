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
//!   deviation over the held-out window, checked per coordinate.
//! - **Geometry audit** ([`orthonormality_error`], [`pseudoinverse_roundtrip_error`]). With a learned
//!   action basis, [`evaluate_with_basis`] also fills [`EvalReport::geometry`] ([`geometry_audit`]).
//! - **Gate**: the held-out transitions are fed through [`FourConditionRollbackGate`] with the
//!   SIGReg health from a [`WelfordSigRegMonitor`] run over all latents. Condition 2 uses the
//!   gate's own probe, which since 2026-11-14 is [`GateProbeKind::BeatsCopyLast`] (see the
//!   `rollback_gate` module doc). [`EvalConfig::probe`] can select the legacy
//!   [`ProbeKind::SignAgreement`] or the fitted [`ProbeKind::LinearLsq`] instead.
//!
//! Two splits: [`evaluate`] holds out the contiguous tail; [`evaluate_by_day`] holds out the
//! last [`EvalConfig::holdout_days`] distinct days, so a day never straddles the split.
//! The verdict promotes only when the gate, the baselines and the activity floor all agree.

mod basis_audit;
mod geometry;
mod probe;
mod split;
#[cfg(test)]
mod tests;

use alloc::vec::Vec;

use weftos_worldmodel_core::{
    surprise_voe, zero_latent, GateCondition, GateVerdict, Latent, RollbackGateMetrics, LATENT_DIM,
};

use crate::rollback_gate::{
    beats_copy_last, sign_agreement, FourConditionRollbackGate, GateProbeKind, ProbePair,
};
use crate::sigreg::WelfordSigRegMonitor;
use crate::training::ActionLinearModel;

pub use geometry::{
    orthonormality_error, pseudoinverse_roundtrip_error, pseudoinverse_roundtrip_error_batch,
    MAX_BASIS_COLS,
};
pub use basis_audit::{geometry_audit, GeometryAudit, GeometryAuditError};
pub use probe::ProbeKind;

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
    /// Fraction of transitions (from the end) held out by the contiguous split.
    pub holdout_frac: f32,
    /// Number of distinct days (the latest) held out by [`evaluate_by_day`]. Default 1.
    pub holdout_days: usize,
    /// Per-coordinate standard-deviation floor (jepa-anything `factor_min_std`).
    pub activity_floor_std: f32,
    /// Model error must be at most `beat_margin` times the better baseline's error.
    pub beat_margin: f32,
    /// Probe whose accuracy feeds the gate's condition 2. Default is the gate's own score.
    pub probe: ProbeKind,
}

impl Default for EvalConfig {
    fn default() -> Self {
        Self {
            holdout_frac: 0.3,
            holdout_days: 1,
            activity_floor_std: 0.1,
            beat_margin: 0.9,
            probe: ProbeKind::BeatsCopyLast,
        }
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
    /// Too few transitions to split and judge (or `days` and `transitions` differ in length).
    TooFewTransitions,
    /// By-day split needs at least one train day and `holdout_days` held-out days.
    TooFewDays,
}

/// Mean held-out `‖ẑ − z‖²` of the two matched baselines.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BaselineErrors {
    /// `ẑ = z_t`.
    pub copy_last: f32,
    /// Per-dimension linear fit on the train split.
    pub linear: f32,
}

/// Held-out accuracy of both probes, whichever one fed the gate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProbeAccuracies {
    /// The legacy sign-agreement score against the mean residual.
    pub sign_agreement: f32,
    /// Fraction of held-out pairs where the model beats copy-last (the gate's default probe).
    pub beats_copy_last: f32,
    /// Fraction of dimensions where the per-dimension linear probe beats copy-last.
    pub linear_lsq: f32,
}

/// Harness result.
#[derive(Debug, Clone, PartialEq)]
pub struct EvalReport {
    /// Train transitions.
    pub n_train: usize,
    /// Held-out transitions.
    pub n_held_out: usize,
    /// Distinct train days (`0` on the contiguous path).
    pub n_train_days: usize,
    /// Distinct held-out days (`0` on the contiguous path).
    pub n_held_out_days: usize,
    /// Held-out day ids, ascending (empty on the contiguous path).
    pub held_out_days: Vec<u16>,
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
    /// Both probes' held-out accuracy (NaN when refused for too little data).
    pub probes: ProbeAccuracies,
    /// Four-condition gate verdict on the held-out transitions.
    pub gate: GateVerdict,
    /// Everything agrees to promote.
    pub promote: bool,
    /// Reasons for refusing, empty when `promote`.
    pub vetoes: Vec<EvalVeto>,
    /// Audit of the learned action basis against held-out residuals. `None` on
    /// [`evaluate`] / [`evaluate_by_day`], and when the audit was refused (see
    /// [`Self::geometry_error`]). Informational: it never vetoes.
    pub geometry: Option<GeometryAudit>,
    /// Why [`Self::geometry`] is `None` although a basis was supplied.
    pub geometry_error: Option<GeometryAuditError>,
}

/// Day bookkeeping carried into the report.
#[derive(Default)]
struct DayInfo {
    n_train_days: usize,
    n_held_days: usize,
    held_days: Vec<u16>,
}

fn refused(n_train: usize, n_held: usize, info: DayInfo, veto: EvalVeto) -> EvalReport {
    EvalReport {
        n_train,
        n_held_out: n_held,
        n_train_days: info.n_train_days,
        n_held_out_days: info.n_held_days,
        held_out_days: info.held_days,
        baselines: BaselineErrors { copy_last: f32::NAN, linear: f32::NAN },
        model_err: f32::NAN,
        beats_baselines: false,
        collapsed_coords: Vec::new(),
        sigreg_health: f32::NAN,
        probes: ProbeAccuracies {
            sign_agreement: f32::NAN,
            beats_copy_last: f32::NAN,
            linear_lsq: f32::NAN,
        },
        gate: GateVerdict::evaluate(RollbackGateMetrics::perfect()),
        promote: false,
        vetoes: alloc::vec![veto],
        geometry: None,
        geometry_error: None,
    }
}

/// Run the harness with the contiguous split (last `holdout_frac` held out).
/// `model` is `pred_φ(z_t, action)`.
pub fn evaluate(
    transitions: &[Transition],
    model: impl FnMut(&Latent, f32) -> Latent,
    cfg: EvalConfig,
) -> EvalReport {
    let n_hold = ((transitions.len() as f32) * cfg.holdout_frac) as usize;
    let n_train = transitions.len().saturating_sub(n_hold);
    if n_train < 8 || n_hold < 8 {
        return refused(n_train, n_hold, DayInfo::default(), EvalVeto::TooFewTransitions);
    }
    let (train, held) = transitions.split_at(n_train);
    run(transitions, train, held, DayInfo::default(), model, cfg, None)
}

/// [`evaluate`] plus a [`geometry_audit`] of `basis` against the held-out residuals.
/// Every other field of the report is identical to [`evaluate`]'s.
pub fn evaluate_with_basis(
    transitions: &[Transition],
    model: impl FnMut(&Latent, f32) -> Latent,
    cfg: EvalConfig,
    basis: &ActionLinearModel,
) -> EvalReport {
    let n_hold = ((transitions.len() as f32) * cfg.holdout_frac) as usize;
    let n_train = transitions.len().saturating_sub(n_hold);
    if n_train < 8 || n_hold < 8 {
        return refused(n_train, n_hold, DayInfo::default(), EvalVeto::TooFewTransitions);
    }
    let (train, held) = transitions.split_at(n_train);
    run(transitions, train, held, DayInfo::default(), model, cfg, Some(basis))
}

/// Run the harness with a held-out-by-day split. `days[i]` tags `transitions[i]`; the last
/// `cfg.holdout_days` distinct day ids are held out, the rest train. Transitions from one
/// day never straddle the split.
pub fn evaluate_by_day(
    transitions: &[Transition],
    days: &[u16],
    model: impl FnMut(&Latent, f32) -> Latent,
    cfg: EvalConfig,
) -> EvalReport {
    let s = match split::by_day(transitions, days, cfg.holdout_days) {
        Ok(s) => s,
        Err(veto) => return refused(0, 0, DayInfo::default(), veto),
    };
    let info = DayInfo {
        n_train_days: s.n_train_days,
        n_held_days: s.n_held_days,
        held_days: s.held_days,
    };
    if s.train.len() < 8 || s.held.len() < 8 {
        return refused(s.train.len(), s.held.len(), info, EvalVeto::TooFewTransitions);
    }
    run(transitions, &s.train, &s.held, info, model, cfg, None)
}

/// [`evaluate_by_day`] plus a [`geometry_audit`] of `basis` against the held-out residuals.
pub fn evaluate_by_day_with_basis(
    transitions: &[Transition],
    days: &[u16],
    model: impl FnMut(&Latent, f32) -> Latent,
    cfg: EvalConfig,
    basis: &ActionLinearModel,
) -> EvalReport {
    let s = match split::by_day(transitions, days, cfg.holdout_days) {
        Ok(s) => s,
        Err(veto) => return refused(0, 0, DayInfo::default(), veto),
    };
    let info = DayInfo {
        n_train_days: s.n_train_days,
        n_held_days: s.n_held_days,
        held_days: s.held_days,
    };
    if s.train.len() < 8 || s.held.len() < 8 {
        return refused(s.train.len(), s.held.len(), info, EvalVeto::TooFewTransitions);
    }
    run(transitions, &s.train, &s.held, info, model, cfg, Some(basis))
}

fn run(
    all: &[Transition],
    train: &[Transition],
    held: &[Transition],
    info: DayInfo,
    mut model: impl FnMut(&Latent, f32) -> Latent,
    cfg: EvalConfig,
    basis: Option<&ActionLinearModel>,
) -> EvalReport {
    let (alpha, beta) = probe::fit_linear(train);

    let (mut e_copy, mut e_lin, mut e_model) = (0.0f32, 0.0f32, 0.0f32);
    let gate_probe = match cfg.probe {
        ProbeKind::SignAgreement => GateProbeKind::SignAgreement,
        ProbeKind::BeatsCopyLast | ProbeKind::LinearLsq => GateProbeKind::BeatsCopyLast,
    };
    let mut gate = FourConditionRollbackGate::new()
        .with_probe_capacity(held.len())
        .with_probe_kind(gate_probe);
    let mut pairs = Vec::with_capacity(held.len());
    // SIGReg judges the embedding distribution over the whole dataset, not just the held-out part.
    let mut monitor = WelfordSigRegMonitor::new(1);
    let mut health = 1.0f32;
    // Offline pass: a constant clock keeps the monitor's online auto-rollback timer (which resets
    // the Welford state after sustained low health) from truncating the statistics.
    for t in all {
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
        pairs.push(ProbePair { z_t: t.z_t, z_hat, z_tp1: t.z_tp1 });
    }
    let n = held.len() as f32;
    let baselines = BaselineErrors { copy_last: e_copy / n, linear: e_lin / n };
    let model_err = e_model / n;
    let best = baselines.copy_last.min(baselines.linear);
    let beats_baselines = model_err <= cfg.beat_margin * best;

    let collapsed_coords = activity_collapse(held, cfg.activity_floor_std);
    let gate_verdict = gate.evaluate_now();
    let probes = ProbeAccuracies {
        sign_agreement: sign_agreement(pairs.iter()),
        beats_copy_last: beats_copy_last(pairs.iter()),
        linear_lsq: probe::linear_probe_accuracy(held, &alpha, &beta),
    };
    let verdict = match cfg.probe {
        ProbeKind::SignAgreement | ProbeKind::BeatsCopyLast => gate_verdict,
        ProbeKind::LinearLsq => {
            let mut m = gate_verdict.metrics;
            m.held_out_probe_accuracy = probes.linear_lsq;
            GateVerdict::evaluate(m)
        }
    };

    let mut vetoes: Vec<EvalVeto> = verdict.failing().into_iter().map(EvalVeto::Gate).collect();
    if !beats_baselines {
        vetoes.push(EvalVeto::DoesNotBeatBaseline);
    }
    if !collapsed_coords.is_empty() {
        vetoes.push(EvalVeto::ActivityCollapse);
    }
    let (geometry, geometry_error) = match basis.map(|b| geometry_audit(b, held)) {
        Some(Ok(g)) => (Some(g), None),
        Some(Err(e)) => (None, Some(e)),
        None => (None, None),
    };
    EvalReport {
        n_train: train.len(),
        n_held_out: held.len(),
        n_train_days: info.n_train_days,
        n_held_out_days: info.n_held_days,
        held_out_days: info.held_days,
        baselines,
        model_err,
        beats_baselines,
        collapsed_coords,
        sigreg_health: health,
        probes,
        gate: verdict,
        promote: vetoes.is_empty(),
        vetoes,
        geometry,
        geometry_error,
    }
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
