//! Temporal-straightening scores for condition 4 of the four-condition gate.
//!
//! Both scores read the window of [`ProbePair`]s the gate keeps for condition 4
//! ([`super::DEFAULT_TRAJECTORY_CAPACITY`] transitions). They are pure functions of that window,
//! so the eval harness can report both next to the gate's own number.

use weftos_worldmodel_core::{
    GATE_PREDICTED_CURVATURE_MIN, GATE_TEMPORAL_STRAIGHTEN_MIN, LATENT_DIM,
};

use super::ProbePair;
use crate::math_no_std::{acosf, sqrtf};

/// Squared displacement length under which a displacement has no direction.
const NO_DIRECTION: f32 = 1e-12;

/// Which score feeds the gate's temporal-straightening condition.
///
/// # Why `PredictedCurvature` is the default (owner decision 2026-11-14)
///
/// In the JEPA literature "temporal straightening" means that a good representation turns the
/// trajectory of a sequence into a straighter one, so that the next state can be predicted by
/// extrapolation (Hénaff, Goris and Simoncelli 2019, *Perceptual straightening of natural
/// videos*). The classic measure is **curvature**: the angle between consecutive displacements
/// `v_t = z_{t+1} − z_t` and `v_{t+1}`. That angle is exactly the angle between the realised
/// displacement and the one a *constant-velocity* predictor would have made.
///
/// The legacy [`StraighteningKind::ChordOverPath`] score is not that measure. It is
/// `‖z_last − z_first‖ / Σ‖z_{i+1} − z_i‖` over the last 16 *observed* latents, a global
/// property of the path, not a local curvature. It has three faults as a promotion condition:
///
/// - It never reads the prediction, so copy-last and a perfect model on the same data get the
///   same score. It judges the encoder and the world, not the checkpoint.
/// - In an action-conditioned scene the path is steered by the actions. With actions drawn per
///   step (the JEPA blob fixture) the true latent path is close to a random walk: on a replica
///   of that scene a perfect encoder averages 0.36 and clears 0.50 on 19 % of windows. A path
///   that reverses inside the window (pacing back and forth) scores near 0 however well it is
///   predicted.
/// - It concatenates whatever is in the window, so a session boundary, a day boundary or a
///   wrap-around jump in the window lowers it.
///
/// [`StraighteningKind::PredictedCurvature`] keeps the literature's per-step angle and replaces
/// the constant-velocity extrapolation with the model's own prediction. Per transition, with
/// `p = ẑ − z_t` (predicted displacement) and `r = z_{t+1} − z_t` (realised):
///
/// - `θ = arccos(p·r / (‖p‖‖r‖))` when both have a direction;
/// - `θ = π/2` (chance) when only one has a direction: copy-last predicts no motion, which
///   says nothing about where the state went, and a predicted motion that did not happen has
///   no realised direction to agree with;
/// - the transition is skipped when neither has a direction (a still scene, predicted still).
///
/// The score is `1 − mean θ / π` over the window: 1 when every prediction points the way the
/// state moved, 0.5 for chance, 0 when every prediction points backwards. When the model is a
/// constant-velocity extrapolator the score is the Hénaff curvature mapped to `[0, 1]`. The
/// floor `GATE_PREDICTED_CURVATURE_MIN` (0.75, a mean angle of at most 45°) is derived in the
/// core constant's doc comment.
///
/// - Copy-last scores 0.5 on any moving data and is refused.
/// - A latent that collapsed onto a nuisance feature leaves only noise in `r`, so the angle is
///   chance and it is refused.
/// - Turns and wrap-around count only on the steps where the model failed to predict them.
/// - Each transition is scored on its own, so session and day boundaries and the order of the
///   window do not matter.
///
/// Failure modes, stated plainly:
///
/// - **Scale-free.** A latent that shrank uniformly with its dynamics intact (all coordinates
///   under the activity floor) still scores 1.0 with an exact model. The SIGReg condition and
///   the harness activity floor catch that case, not this one.
/// - **Direction only.** A prediction with the right direction and a wrong length passes here;
///   condition 2 (beats copy-last) and condition 3 (VoE) judge the error size. Conversely a tiny
///   prediction in a nearly random direction passes condition 2 and fails here.
/// - **Unpredictable worlds.** When more than about half of the per-step displacement energy is
///   unpredictable from `(z_t, a_t)` (sensor noise, a person turning without an action channel,
///   a frame encoder that cannot see velocity), even the best model sits near the floor. That is
///   a property of the data and sensor rate, and W2 recordings must show what normal looks like.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StraighteningKind {
    /// Legacy: `chord / path` of the observed `z_{t+1}` over the window (floor 0.50).
    ChordOverPath,
    /// Mean angle between the predicted and the realised displacement, as `1 − θ̄/π` (floor 0.75).
    #[default]
    PredictedCurvature,
}

impl StraighteningKind {
    /// Score a window. Returns 1.0 (no veto) when the window carries no signal.
    pub fn score<'a, I>(self, pairs: I) -> f32
    where
        I: Iterator<Item = &'a ProbePair> + Clone,
    {
        match self {
            Self::ChordOverPath => chord_over_path(pairs.map(|p| &p.z_tp1)),
            Self::PredictedCurvature => predicted_curvature(pairs),
        }
    }

    /// The floor this score is compared against.
    pub const fn floor(self) -> f32 {
        match self {
            Self::ChordOverPath => GATE_TEMPORAL_STRAIGHTEN_MIN,
            Self::PredictedCurvature => GATE_PREDICTED_CURVATURE_MIN,
        }
    }
}

#[inline]
fn diff(a: &[f32; LATENT_DIM], b: &[f32; LATENT_DIM]) -> [f32; LATENT_DIM] {
    let mut o = [0.0f32; LATENT_DIM];
    for i in 0..LATENT_DIM {
        o[i] = a[i] - b[i];
    }
    o
}

#[inline]
fn dot(a: &[f32; LATENT_DIM], b: &[f32; LATENT_DIM]) -> f32 {
    let mut acc = 0.0f32;
    for i in 0..LATENT_DIM {
        acc += a[i] * b[i];
    }
    acc
}

/// Angle in `[0, π]` between two vectors, `None` when neither has a direction and `π/2` when
/// only one has.
pub(crate) fn angle(a: &[f32; LATENT_DIM], b: &[f32; LATENT_DIM]) -> Option<f32> {
    let (aa, bb) = (dot(a, a), dot(b, b));
    match (aa > NO_DIRECTION, bb > NO_DIRECTION) {
        (false, false) => None,
        (true, true) => {
            let c = dot(a, b) / (sqrtf(aa) * sqrtf(bb));
            Some(acosf(c.clamp(-1.0, 1.0)))
        }
        _ => Some(core::f32::consts::FRAC_PI_2),
    }
}

/// `1 − mean θ / π`, `θ` the angle between `ẑ − z_t` and `z_{t+1} − z_t` (see
/// [`StraighteningKind::PredictedCurvature`]). 1.0 when no transition has a direction.
pub fn predicted_curvature<'a, I>(pairs: I) -> f32
where
    I: Iterator<Item = &'a ProbePair>,
{
    let (mut sum, mut n) = (0.0f32, 0u32);
    for p in pairs {
        if let Some(th) = angle(&diff(&p.z_hat, &p.z_t), &diff(&p.z_tp1, &p.z_t)) {
            sum += th;
            n += 1;
        }
    }
    if n == 0 {
        return 1.0;
    }
    (1.0 - sum / (n as f32 * core::f32::consts::PI)).clamp(0.0, 1.0)
}

/// Legacy score: `‖z_last − z_first‖ / Σ‖z_{i+1} − z_i‖` over a latent path. 1.0 for a path
/// shorter than two points or a stationary one.
pub fn chord_over_path<'a, I>(path: I) -> f32
where
    I: Iterator<Item = &'a [f32; LATENT_DIM]>,
{
    let (mut first, mut prev): (Option<&[f32; LATENT_DIM]>, Option<&[f32; LATENT_DIM]>) =
        (None, None);
    let mut len = 0.0f32;
    for z in path {
        if let Some(p) = prev {
            let d = diff(z, p);
            len += sqrtf(dot(&d, &d));
        } else {
            first = Some(z);
        }
        prev = Some(z);
    }
    let (Some(a), Some(b)) = (first, prev) else {
        return 1.0;
    };
    if len < 1e-9 {
        return 1.0;
    }
    let c = diff(b, a);
    (sqrtf(dot(&c, &c)) / len).clamp(0.0, 1.0)
}
