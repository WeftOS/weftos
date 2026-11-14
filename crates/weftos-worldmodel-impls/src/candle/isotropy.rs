//! Isotropy penalties for the JEPA loop and the participation-ratio collapse diagnostic.
//! See [`crate::candle::jepa`] for the definitions and why each term exists.

use candle_core::Tensor;
use weftos_worldmodel_core::{Latent, LATENT_DIM};

use super::{CandleResult, SplitMix64};

/// `(d, k)` matrix of `k` random unit directions (Box-Muller normals from SplitMix64, each
/// column normalised).
pub(crate) fn random_directions(rng: &mut SplitMix64, d: usize, k: usize) -> CandleResult<Tensor> {
    let mut v = vec![0.0f32; d * k];
    for x in v.iter_mut() {
        let u1 = ((rng.next_u64() >> 11) as f64 + 1.0) / (1u64 << 53) as f64;
        let u2 = (rng.next_u64() >> 11) as f64 / (1u64 << 53) as f64;
        *x = ((-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()) as f32;
    }
    for c in 0..k {
        let n = (0..d).map(|r| (v[r * k + c] as f64).powi(2)).sum::<f64>().sqrt().max(1e-12);
        for r in 0..d {
            v[r * k + c] = (v[r * k + c] as f64 / n) as f32;
        }
    }
    Ok(Tensor::from_vec(v, (d, k), &candle_core::Device::Cpu)?)
}

/// Sliced isotropy term: `mean_k (σ_k − 1)²` with `σ_k = √(Var(z u_k) + ε)` over the columns
/// `u_k` of `dirs` (`(d, k)`, unit columns). `B ≥ 2`. For `z ~ N(0, I)` every direction has
/// unit variance; a rank-deficient batch scores low variance on most directions.
pub fn sliced_penalty(z: &Tensor, dirs: &Tensor) -> candle_core::Result<Tensor> {
    let b = z.dims2()?.0;
    let zc = z.broadcast_sub(&z.mean(0)?)?;
    let var = (zc.matmul(dirs)?.sqr()?.sum(0)? / (b as f64 - 1.0))?;
    ((var + ISO_STD_EPS)?.sqrt()? - 1.0)?.sqr()?.mean_all()
}

/// Width `β` of the Gaussian weight `w(t) = N(t; 0, β²)` in [`epps_pulley_penalty`].
pub const EP_BETA: f64 = 1.0;

/// Sliced Epps-Pulley test (LeJEPA SIGReg, characteristic-function form). For each column
/// `u_k` of `dirs` (`(d, k)`, unit columns) the projections `x_n = z_n · u_k` (not centred:
/// the target is `N(0, 1)`, mean included) are scored by
///
/// ```text
/// D_k = ∫ |φ̂(t) − e^{−t²/2}|² w(t) dt,   φ̂(t) = (1/B) Σ_n e^{i t x_n},   w = N(0, β²)
///     = (1/B²) Σ_{m,n} e^{−β²(x_m − x_n)²/2}
///       − (2/B) (1+β²)^{−½} Σ_n e^{−β² x_n² / (2(1+β²))}
///       + (1+2β²)^{−½}
/// ```
///
/// and the penalty is `mean_k D_k` (`β =` [`EP_BETA`]). `D_k ≥ 0`, and it is 0 only when the
/// empirical characteristic function matches the standard normal's under `w`, so unlike the
/// std form it also sees skew, heavy tails and clustered (multi-modal) projections. The
/// `m = n` terms add a constant `1/B` with no gradient. `B ≥ 2`.
pub fn epps_pulley_penalty(z: &Tensor, dirs: &Tensor) -> candle_core::Result<Tensor> {
    let b2 = EP_BETA * EP_BETA;
    let x = z.matmul(dirs)?; // (B, K)
    let diff = x.unsqueeze(1)?.broadcast_sub(&x.unsqueeze(0)?)?; // (B, B, K)
    let pair = (diff.sqr()? * (-0.5 * b2))?.exp()?.mean(0)?.mean(0)?; // (K)
    let single = ((x.sqr()? * (-b2 / (2.0 * (1.0 + b2))))?.exp()?.mean(0)? * (2.0 / (1.0 + b2).sqrt()))?;
    ((pair - single)? + 1.0 / (1.0 + 2.0 * b2).sqrt())?.mean_all()
}

/// VICReg variance and covariance terms of a `(B, d)` batch:
/// `var = mean_i max(0, 1 − σ_i)` with `σ_i = √(C_ii + ε)`, and
/// `cov = Σ_{i≠j} M_ij² / d` (`n − 1` covariance). `M = C` (VICReg) or, with `correlation`,
/// `M_ij = C_ij / (σ_i σ_j)`: scale-free, so with `B ≤ d` it pushes rank up without pulling
/// the diagonal under 1. `B ≥ 2`.
pub fn vicreg_terms(z: &Tensor, correlation: bool) -> candle_core::Result<(Tensor, Tensor)> {
    let (b, d) = z.dims2()?;
    let zc = z.broadcast_sub(&z.mean(0)?)?;
    let var = (zc.sqr()?.sum(0)? / (b as f64 - 1.0))?;
    let std = (&var + ISO_STD_EPS)?.sqrt()?;
    let hinge = (1.0 - &std)?.relu()?.mean_all()?;
    // The correlation path uses its own tiny ε: with ISO_STD_EPS, shrinking every coordinate
    // under ε would also shrink `ρ` and so lower the term (measured: it drives z to 0).
    let zm = if correlation { zc.broadcast_div(&(&var + CORR_EPS)?.sqrt()?)? } else { zc };
    let c = (zm.t()?.matmul(&zm)? / (b as f64 - 1.0))?;
    let diag = (zm.sqr()?.sum(0)? / (b as f64 - 1.0))?;
    let off = ((c.sqr()?.sum_all()? - diag.sqr()?.sum_all()?)? / d as f64)?;
    Ok((hinge, off))
}

/// `ε` of the correlation normalisation in [`vicreg_terms`].
pub const CORR_EPS: f64 = 1e-8;

/// `ε` inside `σ_i = √(C_ii + ε)` of the isotropy penalty.
pub const ISO_STD_EPS: f64 = 1e-4;

/// Isotropy penalty `R` of a `(B, d)` batch (see the module docs). `B ≥ 2`.
pub fn isotropy_penalty(z: &Tensor, offdiag_weight: f64) -> candle_core::Result<Tensor> {
    let (b, d) = z.dims2()?;
    let mu = z.mean(0)?;
    let zc = z.broadcast_sub(&mu)?;
    let var = (zc.sqr()?.sum(0)? / (b as f64 - 1.0))?;
    let std = (&var + ISO_STD_EPS)?.sqrt()?;
    let energy = (mu.sqr()?.sum_all()? + (std - 1.0)?.sqr()?.sum_all()?)?;
    let mut pen = (energy / d as f64)?;
    if offdiag_weight > 0.0 {
        let c = (zc.t()?.matmul(&zc)? / (b as f64 - 1.0))?;
        let off = (c.sqr()?.sum_all()? - var.sqr()?.sum_all()?)?;
        pen = (pen + (off * (offdiag_weight / d as f64))?)?;
    }
    Ok(pen)
}

/// `(tr C)² / ‖C‖_F²` of the sample covariance of `zs` (f64 accumulation).
pub fn participation_ratio(zs: &[Latent]) -> f32 {
    let n = zs.len();
    if n < 2 {
        return f32::NAN;
    }
    let mut mean = [0.0f64; LATENT_DIM];
    for z in zs {
        for (m, v) in mean.iter_mut().zip(z.iter()) {
            *m += *v as f64 / n as f64;
        }
    }
    let mut c = vec![0.0f64; LATENT_DIM * LATENT_DIM];
    for z in zs {
        let zc: Vec<f64> = z.iter().zip(mean.iter()).map(|(v, m)| *v as f64 - m).collect();
        for i in 0..LATENT_DIM {
            for j in i..LATENT_DIM {
                c[i * LATENT_DIM + j] += zc[i] * zc[j];
            }
        }
    }
    let (mut tr, mut fro) = (0.0f64, 0.0f64);
    for i in 0..LATENT_DIM {
        tr += c[i * LATENT_DIM + i];
        for j in i..LATENT_DIM {
            let v = c[i * LATENT_DIM + j];
            fro += if i == j { v * v } else { 2.0 * v * v };
        }
    }
    if fro == 0.0 {
        return 0.0;
    }
    (tr * tr / fro) as f32
}
