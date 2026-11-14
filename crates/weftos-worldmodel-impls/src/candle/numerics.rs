//! Numerics knobs for the JEPA trainer (lane P, Metal vs CPU drift). All off by default.
//!
//! [`ReduceMode`] picks how the reductions that feed the loss are computed: the encoder's
//! mean-pool over patches, the prediction MSE and the regulariser's batch statistics (mean,
//! variance, the correlation Gram matrix and the sliced projections).
//!
//! | mode | what | why |
//! |---|---|---|
//! | [`ReduceMode::Native`] | candle's own `mean` / `sum` / `matmul` on the model's device | default, unchanged numbers |
//! | [`ReduceMode::Host`] | the same ops, but on CPU (tensors moved with `to_device`, gradients flow back) | Metal keeps the encoder matmuls, CPU does the reductions |
//! | [`ReduceMode::Pairwise`] | fixed binary-tree sums built from elementwise adds (zero-padded to a power of two) | the reduction order no longer depends on the backend's kernel |
//!
//! [`JepaNumerics::init_jitter`] multiplies every online and target encoder weight by
//! `1 + j·u`, `u ∈ [-1, 1)` from [`JepaNumerics::jitter_seed`], after the seeded init. With `j`
//! near f32 epsilon it is a last-bits perturbation: the chaos probe that tells backend bias
//! from sensitivity of the training run itself. The fast-math toggle is not here: candle
//! reads `CANDLE_METAL_ENABLE_FAST_MATH` once, when a Metal device compiles its shaders.

use candle_core::{Device, Tensor};
use candle_nn::VarMap;

use super::{CandleModelError, CandleResult, SplitMix64, CONFIG_TENSOR};

/// How the loss-side reductions are computed (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReduceMode {
    /// candle's kernels on the model's device (the default).
    #[default]
    Native,
    /// candle's kernels on CPU; the rest of the graph stays on the model's device.
    Host,
    /// Fixed-order pairwise trees of elementwise adds, on the model's device.
    Pairwise,
}

/// Numerics settings of a [`crate::candle::JepaTrainConfig`]. `Default` changes nothing.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct JepaNumerics {
    /// Reduction mode for mean-pool, prediction MSE and the regulariser.
    pub reductions: ReduceMode,
    /// Relative init perturbation `j` of both encoders (0 = off).
    pub init_jitter: f64,
    /// Seed of the jitter draws.
    pub jitter_seed: u64,
}

/// Sum over `dim` (removed) as a fixed binary tree of elementwise adds.
pub(crate) fn pairwise_sum(x: &Tensor, dim: usize) -> candle_core::Result<Tensor> {
    let n = x.dim(dim)?;
    let p = n.next_power_of_two();
    let mut x = if p == n {
        x.clone()
    } else {
        let mut dims = x.dims().to_vec();
        dims[dim] = p - n;
        Tensor::cat(&[x, &Tensor::zeros(dims, x.dtype(), x.device())?], dim)?
    };
    let mut m = p;
    while m > 1 {
        m /= 2;
        x = (x.narrow(dim, 0, m)? + x.narrow(dim, m, m)?)?;
    }
    x.squeeze(dim)
}

/// Reductions in one [`ReduceMode`] (`Host` behaves as `Native`: the caller moves tensors).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Red(pub(crate) ReduceMode);

impl Red {
    fn pairwise(self) -> bool {
        self.0 == ReduceMode::Pairwise
    }

    /// `x.sum(dim)`.
    pub(crate) fn sum(self, x: &Tensor, dim: usize) -> candle_core::Result<Tensor> {
        if self.pairwise() { pairwise_sum(x, dim) } else { x.sum(dim) }
    }

    /// `x.mean(dim)`.
    pub(crate) fn mean(self, x: &Tensor, dim: usize) -> candle_core::Result<Tensor> {
        if self.pairwise() { pairwise_sum(x, dim)? / x.dim(dim)? as f64 } else { x.mean(dim) }
    }

    /// `x.sum_all()`.
    pub(crate) fn sum_all(self, x: &Tensor) -> candle_core::Result<Tensor> {
        if self.pairwise() { pairwise_sum(&x.flatten_all()?, 0) } else { x.sum_all() }
    }

    /// `x.mean_all()`.
    pub(crate) fn mean_all(self, x: &Tensor) -> candle_core::Result<Tensor> {
        if self.pairwise() { self.sum_all(x)? / x.elem_count() as f64 } else { x.mean_all() }
    }

    /// `a.matmul(b)` for `(B, d) × (d, K)`; pairwise over `d` as an outer product.
    pub(crate) fn matmul(self, a: &Tensor, b: &Tensor) -> candle_core::Result<Tensor> {
        if self.pairwise() { pairwise_sum(&a.unsqueeze(2)?.broadcast_mul(&b.unsqueeze(0)?)?, 1) } else { a.matmul(b) }
    }

    /// `a.t().matmul(a)` for `(B, d)`; pairwise over `B` as an outer product.
    pub(crate) fn gram(self, a: &Tensor) -> candle_core::Result<Tensor> {
        if self.pairwise() { pairwise_sum(&a.unsqueeze(2)?.broadcast_mul(&a.unsqueeze(1)?)?, 0) } else { a.t()?.matmul(a) }
    }
}

/// Mean over the patch axis `1` of `(B, N, E)` in `mode` (`Host` pools on CPU).
pub(crate) fn pool(x: &Tensor, mode: ReduceMode) -> candle_core::Result<Tensor> {
    match mode {
        ReduceMode::Native => x.mean(1),
        ReduceMode::Host if !x.device().is_cpu() => x.to_device(&Device::Cpu)?.mean(1)?.to_device(x.device()),
        ReduceMode::Host => x.mean(1),
        ReduceMode::Pairwise => Red(mode).mean(x, 1),
    }
}

/// Multiply every variable of `varmap` (except [`CONFIG_TENSOR`]) by `1 + j·u`, `u` uniform
/// in `[-1, 1)` from `seed`, in name order, so two maps with the same names get the same draws.
pub(crate) fn jitter_varmap(varmap: &VarMap, j: f64, seed: u64) -> CandleResult<()> {
    let data = varmap.data().lock().map_err(|_| CandleModelError::Invalid("varmap lock poisoned".into()))?;
    let mut names: Vec<&String> = data.keys().filter(|k| k.as_str() != CONFIG_TENSOR).collect();
    names.sort();
    let mut rng = SplitMix64::new(seed);
    for name in names {
        let var = &data[name];
        let n = var.elem_count();
        let f: Vec<f32> = (0..n).map(|_| (1.0 + j * rng.uniform() as f64) as f32).collect();
        let f = Tensor::from_vec(f, var.dims(), var.device())?;
        var.set(&(var.as_tensor() * f)?)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use candle_core::{DType, Device};

    #[test]
    fn pairwise_matches_native_reductions() {
        let v: Vec<f32> = (0..5 * 7 * 3).map(|i| ((i * 37 % 11) as f32 - 5.0) * 0.25).collect();
        let x = Tensor::from_vec(v, (5, 7, 3), &Device::Cpu).unwrap();
        let close = |a: &Tensor, b: &Tensor| {
            let d = (a - b).unwrap().abs().unwrap().flatten_all().unwrap().max(0).unwrap().to_scalar::<f32>().unwrap();
            assert!(d < 1e-5, "differ by {d}");
        };
        for dim in 0..3 {
            close(&Red(ReduceMode::Pairwise).sum(&x, dim).unwrap(), &x.sum(dim).unwrap());
            close(&pool(&x, ReduceMode::Pairwise).unwrap(), &x.mean(1).unwrap());
        }
        close(&Red(ReduceMode::Pairwise).mean_all(&x).unwrap(), &x.mean_all().unwrap());
        let a = x.narrow(2, 0, 1).unwrap().squeeze(2).unwrap(); // (5, 7)
        let b = Tensor::ones((7, 4), DType::F32, &Device::Cpu).unwrap();
        close(&Red(ReduceMode::Pairwise).matmul(&a, &b).unwrap(), &a.matmul(&b).unwrap());
        close(&Red(ReduceMode::Pairwise).gram(&a).unwrap(), &a.t().unwrap().matmul(&a).unwrap());
    }

    #[test]
    fn pairwise_sum_has_a_gradient() {
        let v = candle_core::Var::from_vec(vec![1.0f32, 2.0, 3.0], 3, &Device::Cpu).unwrap();
        let s = (pairwise_sum(v.as_tensor(), 0).unwrap() * 2.0).unwrap();
        let g = s.backward().unwrap();
        assert_eq!(g.get(&v).unwrap().to_vec1::<f32>().unwrap(), vec![2.0; 3]);
    }

    #[test]
    fn jitter_is_relative_and_deterministic() {
        let m = VarMap::new();
        let vb = candle_nn::VarBuilder::from_varmap(&m, DType::F32, &Device::Cpu);
        let w = vb.get_with_hints(4, "w", candle_nn::Init::Const(2.0)).unwrap();
        jitter_varmap(&m, 1e-3, 9).unwrap();
        let after: Vec<f32> = w.to_vec1().unwrap();
        assert!(after.iter().all(|x| (x - 2.0).abs() <= 2.0 * 1e-3 + 1e-6) && after.iter().any(|x| *x != 2.0));
    }
}
