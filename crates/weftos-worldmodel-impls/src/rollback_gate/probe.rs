//! Held-out probe scores for condition 2 of the four-condition gate.
//!
//! Both scores read the same window of [`ProbePair`]s the gate already keeps. They are pure
//! functions of that window, so the eval harness can report them next to the gate's own number.

use weftos_worldmodel_core::{zero_latent, LATENT_DIM};

use super::ProbePair;

/// Squared error under which a prediction counts as exact.
const EXACT: f32 = 1e-12;

/// Which score feeds the gate's held-out probe condition.
///
/// # Why `BeatsCopyLast` is the default (owner decision 2026-11-14)
///
/// The legacy [`GateProbeKind::SignAgreement`] score asks whether `Δz = z_{t+1} - z_t` points the
/// same way as the window's *mean* residual, dimension by dimension. It never looks at the model's
/// prediction, and it only rewards drifting data. A mean-reverting process `z' = 0.5 z` has a
/// mean residual near zero, so the sign of `Δz` is a coin flip: a perfect model scored 0.54
/// (lane C's table, 2026-11-14) and every synthetic model was vetoed on this one condition even
/// when it beat both matched baselines.
///
/// [`GateProbeKind::BeatsCopyLast`] asks the question the gate exists to answer: does the model's
/// prediction carry information beyond the trivial predictor `ẑ = z_t`? Its score is the fraction
/// of held-out pairs with `‖ẑ − z_{t+1}‖² < ‖z_t − z_{t+1}‖²`.
///
/// - A perfect model on mean-reverting data scores 1.0, and on drifting data 1.0.
/// - A copy-last model ties on every pair, never strictly wins, and scores 0.0.
/// - A model that is wrong more often than copy-last scores under 0.5.
///
/// The score is per transition, not per dimension. It needs no fitted baseline, so it stays
/// online and bounded by the ring. A per-dimension variant (lane C's `LinearLsq`) compares a
/// *fitted* probe, not the deployed model, so it stays in the eval harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GateProbeKind {
    /// Legacy: sign agreement of `Δz` with the mean residual direction. Ignores the prediction.
    SignAgreement,
    /// Fraction of held-out pairs where the model beats copy-last.
    #[default]
    BeatsCopyLast,
}

impl GateProbeKind {
    /// Score a window. Returns 1.0 (no veto) when the window carries no signal.
    pub fn score<'a, I>(self, pairs: I) -> f32
    where
        I: Iterator<Item = &'a ProbePair> + Clone,
    {
        match self {
            Self::SignAgreement => sign_agreement(pairs),
            Self::BeatsCopyLast => beats_copy_last(pairs),
        }
    }
}

#[inline]
fn sq_dist(a: &[f32; LATENT_DIM], b: &[f32; LATENT_DIM]) -> f32 {
    let mut acc = 0.0f32;
    for i in 0..LATENT_DIM {
        let d = a[i] - b[i];
        acc += d * d;
    }
    acc
}

/// Fraction of pairs where `‖ẑ − z_{t+1}‖² < ‖z_t − z_{t+1}‖²`.
///
/// A pair copy-last already predicts exactly counts as a pass only when the model is exact too.
pub fn beats_copy_last<'a, I>(pairs: I) -> f32
where
    I: Iterator<Item = &'a ProbePair>,
{
    let (mut wins, mut total) = (0u64, 0u64);
    for p in pairs {
        let e_model = sq_dist(&p.z_hat, &p.z_tp1);
        let e_copy = sq_dist(&p.z_t, &p.z_tp1);
        total += 1;
        if e_model < e_copy || (e_copy <= EXACT && e_model <= EXACT) {
            wins += 1;
        }
    }
    if total == 0 {
        return 1.0;
    }
    (wins as f32 / total as f32).clamp(0.0, 1.0)
}

/// Legacy score: fraction of (pair, dimension) cells whose `Δz` agrees in sign with the window's
/// mean residual in that dimension. Near-zero cells carry no signal and are skipped.
pub fn sign_agreement<'a, I>(pairs: I) -> f32
where
    I: Iterator<Item = &'a ProbePair> + Clone,
{
    let mut mean_dz = zero_latent();
    let mut n = 0.0f32;
    for p in pairs.clone() {
        n += 1.0;
        for (i, m) in mean_dz.iter_mut().enumerate() {
            *m += p.z_tp1[i] - p.z_t[i];
        }
    }
    if n < 1.0 {
        return 1.0;
    }
    for m in mean_dz.iter_mut() {
        *m /= n;
    }
    let (mut correct, mut total) = (0u64, 0u64);
    for p in pairs {
        for (i, &mean_i) in mean_dz.iter().enumerate() {
            let dz = p.z_tp1[i] - p.z_t[i];
            if dz.abs() < 1e-6 && mean_i.abs() < 1e-6 {
                continue;
            }
            total += 1;
            if dz.signum() == mean_i.signum() {
                correct += 1;
            }
        }
    }
    if total == 0 {
        return 1.0;
    }
    (correct as f32 / total as f32).clamp(0.0, 1.0)
}
