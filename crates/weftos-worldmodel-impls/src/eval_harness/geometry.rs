//! Geometry audit of a learned linear basis: orthonormality and the pseudoinverse round trip.

use alloc::vec;
use alloc::vec::Vec;

use crate::math_no_std::sqrtf;

/// Largest basis width the round-trip solve accepts.
pub const MAX_BASIS_COLS: usize = 192;

/// `‖PᵀP − I‖_F` for a row-major `rows × cols` projection whose columns should be
/// orthonormal. Zero for an orthonormal basis. Returns `None` on a size mismatch.
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
    Some(sqrtf(acc))
}

/// `(PᵀP)⁻¹`, row-major `cols × cols`, by Gauss-Jordan with partial pivoting.
/// `None` when the basis is rank deficient or the sizes are out of range.
fn gram_inverse(p: &[f32], rows: usize, cols: usize) -> Option<Vec<f32>> {
    if rows == 0 || cols == 0 || cols > MAX_BASIS_COLS || p.len() != rows * cols {
        return None;
    }
    let w = 2 * cols;
    let mut a = vec![0.0f32; cols * w];
    for i in 0..cols {
        for j in 0..cols {
            a[i * w + j] = (0..rows).map(|r| p[r * cols + i] * p[r * cols + j]).sum();
        }
        a[i * w + cols + i] = 1.0;
    }
    let scale = (0..cols).map(|i| a[i * w + i]).fold(0.0f32, f32::max);
    if scale <= 0.0 {
        return None;
    }
    let tol = scale * 1e-6;
    for c in 0..cols {
        let piv = (c..cols).max_by(|&x, &y| {
            a[x * w + c].abs().partial_cmp(&a[y * w + c].abs()).unwrap_or(core::cmp::Ordering::Equal)
        })?;
        if a[piv * w + c].abs() <= tol {
            return None;
        }
        if piv != c {
            for k in 0..w {
                a.swap(piv * w + k, c * w + k);
            }
        }
        let d = a[c * w + c];
        for k in 0..w {
            a[c * w + k] /= d;
        }
        for r in 0..cols {
            if r == c {
                continue;
            }
            let f = a[r * w + c];
            if f != 0.0 {
                for k in 0..w {
                    a[r * w + k] -= f * a[c * w + k];
                }
            }
        }
    }
    let mut inv = vec![0.0f32; cols * cols];
    for i in 0..cols {
        inv[i * cols..(i + 1) * cols].copy_from_slice(&a[i * w + cols..(i + 1) * w]);
    }
    Some(inv)
}

/// `‖P P⁺ z − z‖ / ‖z‖` with `P⁺ = (PᵀP)⁻¹Pᵀ`, given the Gram inverse. `None` for `z ≈ 0`.
fn roundtrip(p: &[f32], rows: usize, cols: usize, ginv: &[f32], z: &[f32]) -> Option<f32> {
    let zz: f32 = z.iter().map(|v| v * v).sum();
    if zz <= 1e-24 {
        return None;
    }
    let ptz: Vec<f32> = (0..cols).map(|c| (0..rows).map(|r| p[r * cols + c] * z[r]).sum()).collect();
    let coef: Vec<f32> =
        (0..cols).map(|i| (0..cols).map(|j| ginv[i * cols + j] * ptz[j]).sum()).collect();
    let mut resid = 0.0f32;
    for r in 0..rows {
        let rec: f32 = (0..cols).map(|c| p[r * cols + c] * coef[c]).sum();
        resid += (rec - z[r]) * (rec - z[r]);
    }
    Some(sqrtf(resid / zz))
}

/// Round-trip error of one latent through a row-major `rows × cols` basis. `~0` when `z` lies
/// in the span of the columns, `~1` when it is orthogonal to it. `None` on a size mismatch,
/// `cols > MAX_BASIS_COLS`, a singular basis, or a zero latent.
pub fn pseudoinverse_roundtrip_error(p: &[f32], rows: usize, cols: usize, z: &[f32]) -> Option<f32> {
    if z.len() != rows {
        return None;
    }
    let ginv = gram_inverse(p, rows, cols)?;
    roundtrip(p, rows, cols, &ginv, z)
}

/// Mean round-trip error over many latents stored flat (`zs.len()` a multiple of `rows`).
/// The Gram inverse is computed once. Zero latents are skipped; `None` if none remain.
pub fn pseudoinverse_roundtrip_error_batch(
    p: &[f32],
    rows: usize,
    cols: usize,
    zs: &[f32],
) -> Option<f32> {
    if rows == 0 || !zs.len().is_multiple_of(rows) {
        return None;
    }
    let ginv = gram_inverse(p, rows, cols)?;
    let (mut sum, mut n) = (0.0f32, 0usize);
    for z in zs.chunks_exact(rows) {
        if let Some(e) = roundtrip(p, rows, cols, &ginv, z) {
            sum += e;
            n += 1;
        }
    }
    (n > 0).then(|| sum / n as f32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geometry_audit_separates_orthonormal_from_skewed_bases() {
        let q = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];
        assert!(orthonormality_error(&q, 3, 2).unwrap() < 1e-6);
        let skew = [1.0, 1.0, 0.0, 1.0, 0.0, 0.0];
        assert!(orthonormality_error(&skew, 3, 2).unwrap() > 0.5);
        assert!(orthonormality_error(&q, 3, 3).is_none());
    }

    const Q: [f32; 6] = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];

    #[test]
    fn orthonormal_basis_and_z_in_span_round_trips_to_zero() {
        let e = pseudoinverse_roundtrip_error(&Q, 3, 2, &[0.3, -2.0, 0.0]).unwrap();
        assert!(e < 1e-6, "{e}");
    }

    #[test]
    fn z_orthogonal_to_the_span_has_error_one() {
        let e = pseudoinverse_roundtrip_error(&Q, 3, 2, &[0.0, 0.0, 5.0]).unwrap();
        assert!((e - 1.0).abs() < 1e-4, "{e}");
    }

    #[test]
    fn skewed_basis_still_inverts() {
        // Columns (1,0,0) and (1,1,0): not orthonormal, still spans the xy plane.
        let skew = [1.0, 1.0, 0.0, 1.0, 0.0, 0.0];
        assert!(orthonormality_error(&skew, 3, 2).unwrap() > 0.5);
        let e = pseudoinverse_roundtrip_error(&skew, 3, 2, &[0.7, -1.3, 0.0]).unwrap();
        assert!(e < 1e-5, "{e}");
        let off = pseudoinverse_roundtrip_error(&skew, 3, 2, &[0.0, 0.0, 1.0]).unwrap();
        assert!((off - 1.0).abs() < 1e-4, "{off}");
        // Partial projection: half in span, half out.
        let half = pseudoinverse_roundtrip_error(&skew, 3, 2, &[1.0, 1.0, 1.0]).unwrap();
        assert!((half - 1.0 / 3.0f32.sqrt()).abs() < 1e-4, "{half}");
    }

    #[test]
    fn singular_basis_and_bad_shapes_return_none() {
        // Two identical columns.
        let sing = [1.0, 1.0, 0.0, 0.0, 0.0, 0.0];
        assert!(pseudoinverse_roundtrip_error(&sing, 3, 2, &[1.0, 0.0, 0.0]).is_none());
        assert!(pseudoinverse_roundtrip_error(&Q, 3, 2, &[1.0, 0.0]).is_none());
        assert!(pseudoinverse_roundtrip_error(&Q, 3, 3, &[1.0, 0.0, 0.0]).is_none());
        assert!(pseudoinverse_roundtrip_error(&Q, 3, 2, &[0.0; 3]).is_none());
        let wide = vec![0.0f32; 4 * (MAX_BASIS_COLS + 1)];
        assert!(pseudoinverse_roundtrip_error(&wide, 4, MAX_BASIS_COLS + 1, &[1.0; 4]).is_none());
    }

    #[test]
    fn batch_is_the_mean_of_the_single_errors() {
        let zs = [0.3, -2.0, 0.0, 0.0, 0.0, 5.0, 0.0, 0.0, 0.0];
        let m = pseudoinverse_roundtrip_error_batch(&Q, 3, 2, &zs).unwrap();
        // errors: ~0, 1, skipped zero latent -> mean 0.5
        assert!((m - 0.5).abs() < 1e-4, "{m}");
        assert!(pseudoinverse_roundtrip_error_batch(&Q, 3, 2, &[0.0; 6]).is_none());
        assert!(pseudoinverse_roundtrip_error_batch(&Q, 3, 2, &[1.0; 4]).is_none());
    }
}
