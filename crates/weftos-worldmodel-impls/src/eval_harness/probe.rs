//! Linear baseline fit and the held-out probe options.

use super::Transition;
use weftos_worldmodel_core::LATENT_DIM;

/// Which held-out probe accuracy is fed to the rollback gate's condition 2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProbeKind {
    /// The gate's default score: fraction of held-out pairs where the model beats copy-last.
    #[default]
    BeatsCopyLast,
    /// The legacy gate score: sign agreement of `Δz` with the mean residual direction.
    SignAgreement,
    /// Per-dimension linear probe `ẑ_i = α_i z_i + β_i` (fit on train); accuracy is the fraction
    /// of dimensions whose held-out error is below the copy-last error.
    LinearLsq,
}

/// Per-dimension least squares `z_{t+1} ≈ α z_t + β`.
pub(super) fn fit_linear(train: &[Transition]) -> ([f32; LATENT_DIM], [f32; LATENT_DIM]) {
    let n = train.len() as f32;
    let (mut alpha, mut beta) = ([0.0f32; LATENT_DIM], [0.0f32; LATENT_DIM]);
    for d in 0..LATENT_DIM {
        let (mx, my) =
            train.iter().fold((0.0, 0.0), |(a, b), t| (a + t.z_t[d] / n, b + t.z_tp1[d] / n));
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

/// Fraction of dimensions where the linear probe's mean squared error on `held` is below
/// copy-last's. A dimension copy-last already predicts exactly counts as a pass.
pub(super) fn linear_probe_accuracy(
    held: &[Transition],
    alpha: &[f32; LATENT_DIM],
    beta: &[f32; LATENT_DIM],
) -> f32 {
    if held.is_empty() {
        return 1.0;
    }
    let mut wins = 0usize;
    for d in 0..LATENT_DIM {
        let (mut e_probe, mut e_copy) = (0.0f32, 0.0f32);
        for t in held {
            let p = alpha[d] * t.z_t[d] + beta[d] - t.z_tp1[d];
            let c = t.z_t[d] - t.z_tp1[d];
            e_probe += p * p;
            e_copy += c * c;
        }
        if e_probe < e_copy || e_copy <= 1e-12 {
            wins += 1;
        }
    }
    wins as f32 / LATENT_DIM as f32
}
