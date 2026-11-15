//! Geometry audit of a learned action basis against held-out residuals (lane H).
//!
//! [`crate::training::ActionLinearModel`] predicts `ẑ = z + A·a + b`. The columns of
//! `A` (192 × 32, row-major) span the subspace of latent change that the action code
//! can explain. This audit asks how much of what was actually observed lies in it.

use alloc::vec::Vec;

use weftos_worldmodel_core::LATENT_DIM;

use crate::action_encoder::ACTION_CODE_DIM;
use crate::training::ActionLinearModel;

use super::geometry::{
    basis_is_full_rank, orthonormality_error, pseudoinverse_roundtrip_error_batch,
};
use super::Transition;

/// Geometry of the learned basis against held-out residuals.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GeometryAudit {
    /// `‖AᵀA − I‖_F`. Zero when the columns of `A` are orthonormal. The ridge fit does not
    /// ask for orthonormal columns, so a nonzero value is normal and only says how far the
    /// basis is from an orthonormal one. It is not a pass or fail threshold.
    pub orthonormality_error: f32,
    /// Mean over held-out transitions of `‖A A⁺ r − r‖ / ‖r‖` with `r = z_{t+1} − z_t` and
    /// `A⁺ = (AᵀA)⁻¹Aᵀ`. It is the fraction of the observed residual's norm that lies
    /// outside the span of the action basis: `0` when every residual is exactly
    /// `A·a` for some `a`, `1` when residuals are orthogonal to the span. Residuals that
    /// are exactly zero are skipped. The bias `b` is outside the span of `A`, so a
    /// constant drift raises this number.
    pub pseudoinverse_roundtrip_error_mean: f32,
}

/// Why no audit could be produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeometryAuditError {
    /// `A` has the wrong shape or is rank deficient (for example two identical columns, or
    /// an untrained all-zero `A`), so `A⁺` does not exist.
    SingularBasis,
    /// No held-out transition has a nonzero residual.
    NoResiduals,
}

/// Audit `model`'s action basis against the residuals `z_{t+1} − z_t` of `held`.
pub fn geometry_audit(
    model: &ActionLinearModel,
    held: &[Transition],
) -> Result<GeometryAudit, GeometryAuditError> {
    let (rows, cols) = (LATENT_DIM, ACTION_CODE_DIM);
    let ortho = orthonormality_error(&model.a, rows, cols).ok_or(GeometryAuditError::SingularBasis)?;
    if !basis_is_full_rank(&model.a, rows, cols) {
        return Err(GeometryAuditError::SingularBasis);
    }
    let mut residuals: Vec<f32> = Vec::with_capacity(held.len() * rows);
    for t in held {
        residuals.extend(t.z_tp1.iter().zip(t.z_t.iter()).map(|(n, p)| n - p));
    }
    let mean = pseudoinverse_roundtrip_error_batch(&model.a, rows, cols, &residuals)
        .ok_or(GeometryAuditError::NoResiduals)?;
    Ok(GeometryAudit { orthonormality_error: ortho, pseudoinverse_roundtrip_error_mean: mean })
}

#[cfg(test)]
mod tests {
    extern crate std;
    use std::eprintln;

    use super::*;
    use crate::eval_harness::{evaluate, evaluate_with_basis, EvalConfig};
    use weftos_worldmodel_core::zero_latent;

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> f32 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            ((self.0 >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
        }
    }

    /// A random learned-looking basis (`b = 0`), SYNTHETIC.
    fn basis(seed: u64) -> ActionLinearModel {
        let mut rng = Rng(seed);
        let mut m = ActionLinearModel::zero();
        m.a.iter_mut().for_each(|v| *v = 0.05 * rng.next());
        m
    }

    /// Orthonormal basis of `A`'s column space (modified Gram-Schmidt in f64), columns first.
    fn column_space(m: &ActionLinearModel) -> Vec<Vec<f64>> {
        let mut q: Vec<Vec<f64>> = Vec::new();
        for c in 0..ACTION_CODE_DIM {
            let mut v: Vec<f64> = (0..LATENT_DIM).map(|r| m.a[r * ACTION_CODE_DIM + c] as f64).collect();
            for u in &q {
                let d: f64 = v.iter().zip(u).map(|(a, b)| a * b).sum();
                v.iter_mut().zip(u).for_each(|(a, b)| *a -= d * b);
            }
            let n = v.iter().map(|a| a * a).sum::<f64>().sqrt();
            v.iter_mut().for_each(|a| *a /= n);
            q.push(v);
        }
        q
    }

    /// `n` held-out transitions with residual `A·a + sigma·n⊥`, where `n⊥` is a unit noise
    /// vector orthogonal to the span of `A` (so `sigma` is the out-of-span norm).
    fn transitions(m: &ActionLinearModel, n: usize, sigma: f32, seed: u64) -> Vec<Transition> {
        let q = column_space(m);
        let mut rng = Rng(seed);
        (0..n)
            .map(|_| {
                let mut z = zero_latent();
                z.iter_mut().for_each(|v| *v = rng.next());
                let mut a = [0.0f32; ACTION_CODE_DIM];
                a.iter_mut().for_each(|v| *v = rng.next());
                let mut noise: Vec<f64> = (0..LATENT_DIM).map(|_| rng.next() as f64).collect();
                for u in &q {
                    let d: f64 = noise.iter().zip(u).map(|(x, y)| x * y).sum();
                    noise.iter_mut().zip(u).for_each(|(x, y)| *x -= d * y);
                }
                let norm = noise.iter().map(|x| x * x).sum::<f64>().sqrt();
                let mut next = z;
                for d in 0..LATENT_DIM {
                    let row = &m.a[d * ACTION_CODE_DIM..(d + 1) * ACTION_CODE_DIM];
                    let drift: f32 = row.iter().zip(a.iter()).map(|(w, x)| w * x).sum();
                    next[d] += drift + sigma * (noise[d] / norm) as f32;
                }
                Transition { z_t: z, action: 0.0, z_tp1: next }
            })
            .collect()
    }

    #[test]
    fn residuals_exactly_in_the_span_round_trip_to_zero() {
        let m = basis(3);
        let audit = geometry_audit(&m, &transitions(&m, 60, 0.0, 9)).unwrap();
        eprintln!("in-span: {audit:?}");
        assert!(audit.pseudoinverse_roundtrip_error_mean < 1e-3, "{audit:?}");
        // A random Gaussian-ish basis of this scale is far from orthonormal.
        assert!(audit.orthonormality_error > 1.0, "{audit:?}");
    }

    #[test]
    fn error_grows_with_noise_orthogonal_to_the_span() {
        let m = basis(3);
        let mut last = -1.0f32;
        for sigma in [0.0f32, 0.05, 0.2, 0.8] {
            let held = transitions(&m, 60, sigma, 9);
            let e = geometry_audit(&m, &held).unwrap().pseudoinverse_roundtrip_error_mean;
            eprintln!("sigma {sigma}: round-trip error {e:.5}");
            assert!(e > last, "sigma {sigma}: {e} !> {last}");
            last = e;
            if sigma > 0.0 {
                // The out-of-span share of the residual norm, from the signal's mean norm.
                let sig: f32 = held
                    .iter()
                    .map(|t| t.z_tp1.iter().zip(t.z_t.iter()).map(|(n, p)| (n - p) * (n - p)).sum::<f32>())
                    .sum::<f32>()
                    / held.len() as f32;
                let expect = sigma / sig.sqrt();
                assert!((e - expect).abs() < 0.1 * expect + 2e-3, "sigma {sigma}: {e} vs ~{expect}");
            }
        }
    }

    #[test]
    fn singular_basis_is_reported_not_a_panic() {
        let mut m = basis(3);
        for r in 0..LATENT_DIM {
            m.a[r * ACTION_CODE_DIM + 1] = m.a[r * ACTION_CODE_DIM]; // two identical columns
        }
        let held = transitions(&basis(3), 20, 0.0, 9);
        assert_eq!(geometry_audit(&m, &held), Err(GeometryAuditError::SingularBasis));
        assert_eq!(geometry_audit(&ActionLinearModel::zero(), &held), Err(GeometryAuditError::SingularBasis));

        let mut bad = basis(3);
        bad.a.pop();
        assert_eq!(geometry_audit(&bad, &held), Err(GeometryAuditError::SingularBasis));
        assert_eq!(geometry_audit(&basis(3), &[]), Err(GeometryAuditError::NoResiduals));
    }

    #[test]
    fn evaluate_with_basis_fills_geometry_and_leaves_the_rest_alone() {
        let m = basis(3);
        let data = transitions(&m, 80, 0.05, 4);
        let cfg = EvalConfig::default();
        let plain = evaluate(&data, |z, _| *z, cfg);
        assert!(plain.geometry.is_none() && plain.geometry_error.is_none());

        let with = evaluate_with_basis(&data, |z, _| *z, cfg, &m);
        let audit = with.geometry.expect("audit");
        let n_held = with.n_held_out;
        assert_eq!(audit, geometry_audit(&m, &data[data.len() - n_held..]).unwrap());
        let mut stripped = with.clone();
        stripped.geometry = None;
        assert_eq!(stripped, plain);

        let mut sing = basis(3);
        sing.a.iter_mut().for_each(|v| *v = 0.0);
        let r = evaluate_with_basis(&data, |z, _| *z, cfg, &sing);
        assert_eq!((r.geometry, r.geometry_error), (None, Some(GeometryAuditError::SingularBasis)));
    }

    /// The basis the ridge trainer actually produces, on dynamics with a constant drift `b*`
    /// outside the span: the number is small but not zero, and it is the bias that costs it.
    #[test]
    fn ridge_fitted_basis_on_synthetic_dynamics() {
        use crate::training::DEFAULT_RIDGE_LAMBDA;
        use weftos_worldmodel_core::{Action, SensorClass, TrainingSample};

        let truth = basis(21);
        let mut rng = Rng(5);
        let mut b = zero_latent();
        b.iter_mut().for_each(|v| *v = 0.01 * rng.next());
        let mut all = Vec::new();
        for t in 0..300u64 {
            let mut z = zero_latent();
            z.iter_mut().for_each(|v| *v = rng.next());
            let mut act = Action::null();
            act.code.iter_mut().for_each(|v| *v = rng.next());
            let mut next = ActionLinearModel { a: truth.a.clone(), b }.predict(&z, &act);
            next.iter_mut().for_each(|v| *v += 0.001 * rng.next());
            let mut s = TrainingSample::basic(SensorClass::Imu, z, next, t);
            s.action = act;
            all.push(s);
        }
        let (train, held) = all.split_at(200);
        let fit = ActionLinearModel::fit_ridge(train, DEFAULT_RIDGE_LAMBDA).expect("fit");
        let held_t: Vec<Transition> =
            held.iter().map(|s| Transition { z_t: s.z_t, action: 0.0, z_tp1: s.z_tp1 }).collect();
        let audit = geometry_audit(&fit, &held_t).unwrap();
        eprintln!("ridge-fitted basis: {audit:?}");
        assert!(audit.pseudoinverse_roundtrip_error_mean < 0.15, "{audit:?}");
    }
}
