//! Collapse diagnostics for the JEPA loop (f64, host side): participation ratio, the top
//! eigenvalues of the latent covariance, and how much of the latent variance a grouping
//! (session, day) explains. Used by [`crate::candle::jepa::train_jepa`] for the final report
//! and, with [`crate::candle::jepa::JepaTrainConfig::diagnostics`], once per epoch.

use weftos_worldmodel_core::{Latent, LATENT_DIM};

/// Eigenvalues reported by [`LatentDiagnostics::top_eigenvalues`].
pub const TOP_EIGEN: usize = 5;

/// Shape of one set of latents.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct LatentDiagnostics {
    /// `(tr C)² / ‖C‖_F²` (1 = rank one).
    pub participation_ratio: f32,
    /// Largest eigenvalues of the sample covariance `C` (its singular values, `C` is PSD),
    /// descending.
    pub top_eigenvalues: [f32; TOP_EIGEN],
    /// Between-session over total variance, summed over coordinates (0..1).
    pub session_fraction: f32,
    /// Between-day over total variance, summed over coordinates (0..1).
    pub day_fraction: f32,
    /// Between-session over total variance along the top principal direction only.
    pub top_pc_session_fraction: f32,
    /// Between-day over total variance along the top principal direction only.
    pub top_pc_day_fraction: f32,
}

/// Per-epoch trace (only with `JepaTrainConfig::diagnostics`).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct EpochDiagnostics {
    /// Online latents `f_θ(x_t)` of the train split, measured after the epoch.
    pub online: LatentDiagnostics,
    /// Participation ratio of the EMA target's latents `f_ξ(x_t)` on the same frames.
    pub target_participation_ratio: f32,
    /// `Σ‖f_ξ(x) − f_θ(x)‖² / Σ‖f_θ(x) − mean‖²`: how far the target lags the online
    /// encoder. A detached, correctly updated target starts at 0 (ξ = θ), grows while θ moves
    /// faster than `1 − τ`, and would stay at 0 if the target tracked θ instantly.
    pub target_gap: f32,
    /// Mean step prediction loss over the mean step total (pre-step, train mode).
    pub prediction_share: f32,
}

/// Sample covariance (`n − 1`) and mean of `zs`, row-major `d × d`.
pub fn covariance(zs: &[Latent]) -> (Vec<f64>, [f64; LATENT_DIM]) {
    let n = zs.len();
    let mut mean = [0.0f64; LATENT_DIM];
    for z in zs {
        for (m, v) in mean.iter_mut().zip(z.iter()) {
            *m += *v as f64 / n as f64;
        }
    }
    let d = LATENT_DIM;
    let mut c = vec![0.0f64; d * d];
    for z in zs {
        let zc: Vec<f64> = z.iter().zip(mean.iter()).map(|(v, m)| *v as f64 - m).collect();
        for i in 0..d {
            let zi = zc[i];
            for j in i..d {
                c[i * d + j] += zi * zc[j];
            }
        }
    }
    let denom = (n as f64 - 1.0).max(1.0);
    for i in 0..d {
        for j in i..d {
            let v = c[i * d + j] / denom;
            c[i * d + j] = v;
            c[j * d + i] = v;
        }
    }
    (c, mean)
}

/// `(tr C)² / ‖C‖_F²` of a covariance matrix (0 for an all-zero `C`).
pub fn participation_ratio_of(c: &[f64], d: usize) -> f64 {
    let tr: f64 = (0..d).map(|i| c[i * d + i]).sum();
    let fro: f64 = c.iter().map(|v| v * v).sum();
    if fro == 0.0 {
        0.0
    } else {
        tr * tr / fro
    }
}

/// Top `k` eigenpairs of a symmetric PSD matrix by power iteration with deflation
/// (deterministic start vectors). Eigenvalues descending.
pub fn top_eigen(c: &[f64], d: usize, k: usize) -> Vec<(f64, Vec<f64>)> {
    let mut a = c.to_vec();
    let mut out = Vec::with_capacity(k);
    for e in 0..k {
        let mut v: Vec<f64> = (0..d).map(|i| 1.0 + ((i * 31 + e * 17) % 13) as f64 * 0.1).collect();
        let mut lambda = 0.0;
        for _ in 0..400 {
            let w: Vec<f64> = (0..d).map(|i| (0..d).map(|j| a[i * d + j] * v[j]).sum()).collect();
            let n = w.iter().map(|x| x * x).sum::<f64>().sqrt();
            if n == 0.0 {
                break;
            }
            v = w.into_iter().map(|x| x / n).collect();
            lambda = n;
        }
        for i in 0..d {
            for j in 0..d {
                a[i * d + j] -= lambda * v[i] * v[j];
            }
        }
        out.push((lambda, v));
    }
    out
}

/// Between-group sum of squares over total sum of squares of scalar or vector samples,
/// given per-sample group keys. `rows[n]` is sample `n` (any width).
pub fn between_fraction(rows: &[Vec<f64>], keys: &[u32]) -> f64 {
    let n = rows.len();
    if n < 2 {
        return f64::NAN;
    }
    let w = rows[0].len();
    let mut mean = vec![0.0f64; w];
    for r in rows {
        for (m, v) in mean.iter_mut().zip(r) {
            *m += v / n as f64;
        }
    }
    let total: f64 = rows.iter().map(|r| r.iter().zip(&mean).map(|(v, m)| (v - m).powi(2)).sum::<f64>()).sum();
    let mut groups: std::collections::BTreeMap<u32, (usize, Vec<f64>)> = std::collections::BTreeMap::new();
    for (r, k) in rows.iter().zip(keys) {
        let g = groups.entry(*k).or_insert_with(|| (0, vec![0.0; w]));
        g.0 += 1;
        for (s, v) in g.1.iter_mut().zip(r) {
            *s += v;
        }
    }
    let between: f64 = groups
        .values()
        .map(|(cnt, sum)| *cnt as f64 * sum.iter().zip(&mean).map(|(s, m)| (s / *cnt as f64 - m).powi(2)).sum::<f64>())
        .sum();
    if total == 0.0 {
        0.0
    } else {
        between / total
    }
}

/// Full diagnostics of `zs` with per-sample `(day, session)` tags.
pub fn diagnose(zs: &[Latent], tags: &[(u16, u16)]) -> LatentDiagnostics {
    if zs.len() < 2 {
        return LatentDiagnostics { participation_ratio: f32::NAN, ..LatentDiagnostics::default() };
    }
    let d = LATENT_DIM;
    let (c, mean) = covariance(zs);
    let eig = top_eigen(&c, d, TOP_EIGEN);
    let mut top_eigenvalues = [0.0f32; TOP_EIGEN];
    for (t, (l, _)) in top_eigenvalues.iter_mut().zip(&eig) {
        *t = *l as f32;
    }
    let session_keys: Vec<u32> = tags.iter().map(|(d, s)| (*d as u32) << 16 | *s as u32).collect();
    let day_keys: Vec<u32> = tags.iter().map(|(d, _)| *d as u32).collect();
    let rows: Vec<Vec<f64>> = zs.iter().map(|z| z.iter().map(|v| *v as f64).collect()).collect();
    let pc = &eig[0].1;
    let proj: Vec<Vec<f64>> = zs.iter().map(|z| vec![z.iter().zip(pc).zip(&mean).map(|((v, p), m)| (*v as f64 - m) * p).sum()]).collect();
    LatentDiagnostics {
        participation_ratio: participation_ratio_of(&c, d) as f32,
        top_eigenvalues,
        session_fraction: between_fraction(&rows, &session_keys) as f32,
        day_fraction: between_fraction(&rows, &day_keys) as f32,
        top_pc_session_fraction: between_fraction(&proj, &session_keys) as f32,
        top_pc_day_fraction: between_fraction(&proj, &day_keys) as f32,
    }
}

/// `Σ‖a_n − b_n‖² / Σ‖b_n − mean(b)‖²`.
pub fn relative_gap(a: &[Latent], b: &[Latent]) -> f32 {
    let (_, mean) = covariance(b);
    let num: f64 = a.iter().zip(b).map(|(x, y)| x.iter().zip(y).map(|(p, q)| (*p as f64 - *q as f64).powi(2)).sum::<f64>()).sum();
    let den: f64 = b.iter().map(|y| y.iter().zip(&mean).map(|(q, m)| (*q as f64 - m).powi(2)).sum::<f64>()).sum();
    if den == 0.0 {
        f32::INFINITY
    } else {
        (num / den) as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::candle::SplitMix64;

    #[test]
    fn eigenvalues_and_group_fractions_on_a_planted_structure() {
        // z = 3·g_session·e0 + 1·n·e1 (+ tiny noise): one session direction, one noise direction.
        let mut rng = SplitMix64::new(4);
        let mut zs = Vec::new();
        let mut tags = Vec::new();
        for s in 0..6u16 {
            let level = s as f32 - 2.5;
            for _ in 0..40 {
                let mut z = [0.0f32; LATENT_DIM];
                z[0] = 3.0 * level / 1.7;
                z[1] = rng.uniform() * 2.0 - 1.0;
                z[2] = (rng.uniform() - 0.5) * 1e-3;
                zs.push(z);
                tags.push((s / 3, s));
            }
        }
        let dg = diagnose(&zs, &tags);
        let (c, _) = covariance(&zs);
        let var0 = c[0];
        assert!((dg.top_eigenvalues[0] as f64 - var0).abs() < 1e-2 * var0, "{:?} vs {var0}", dg.top_eigenvalues);
        assert!((dg.top_eigenvalues[1] as f64 - c[LATENT_DIM + 1]).abs() < 2e-2, "{:?}", dg.top_eigenvalues);
        assert!(dg.top_eigenvalues[2] < 1e-5);
        assert!(dg.top_pc_session_fraction > 0.999, "{dg:?}");
        assert!(dg.session_fraction > 0.8 && dg.session_fraction < 0.99, "{dg:?}");
        assert!(dg.day_fraction < dg.session_fraction);
        assert!(dg.participation_ratio > 1.0 && dg.participation_ratio < 2.0);
    }
}
