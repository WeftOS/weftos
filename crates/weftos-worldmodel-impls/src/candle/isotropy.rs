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
