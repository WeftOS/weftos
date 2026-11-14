//! Candle ViT-tiny encoder + AdaLN `pred_φ` (`feature = "candle"`, plan W5).
//!
//! Built and tested on **candle-core / candle-nn 0.11.1**. CPU by default. With the `metal`
//! feature every model can also run on the Apple GPU: see [`ComputeDevice`],
//! [`candle_default_device`] and [`metal_supported`]. Default builds never pull candle.
//!
//! | Piece | Module | What it is |
//! |---|---|---|
//! | [`CandleVitEncoder`] | [`vit`] | Patch embed + learned position embedding + pre-norm MHSA/MLP blocks + mean-pool + head to 192 |
//! | [`AdaLnPredictor`] | [`adaln`] | Residual MLP blocks on `z` whose LayerNorm scale/shift/gate come from the 32-d action code (DiT adaLN-zero) |
//! | safetensors I/O | [`io`] | `from_safetensors_path` / `from_safetensors_bytes`, byte export, codec `0x03`, registry helper |
//! | training | [`train`] | AdamW loop for the AdaLN predictor over [`weftos_worldmodel_core::TrainingSample`]s |
//! | JEPA training | [`jepa`], [`jepa_report`], [`jepa_diag`] | Encoder + predictor self-supervised: EMA target ViT, SIGReg-style isotropy penalty, harness collapse checks |
//! | fixtures | [`synthetic`] | SYNTHETIC moving-blob frames with actions and day/session tags (tests and bench only) |
//!
//! # Checkpoint layout
//!
//! Every checkpoint is one safetensors file of `F32` tensors. It is self-describing: the
//! tensor [`CONFIG_TENSOR`] (`"lewm.config"`, shape `[8]`) holds
//! `[arch, format, p0 .. p5]` with `arch` = [`ARCH_VIT_TINY`] or [`ARCH_ADALN`], so a loader
//! rebuilds the architecture from the file alone. Tensor names follow timm's ViT naming
//! (`patch_embed.proj.weight`, `blocks.{i}.attn.qkv.weight`, ...) so a PyTorch export only has
//! to reshape the patch conv kernel `(E, 3, p, p)` to `(E, 3·p·p)` and add `lewm.config`.

use std::fmt;

use candle_core::{DType, Device, Tensor, D};
use candle_nn::VarMap;
use weftos_worldmodel_core::WorldModelError;

pub mod adaln;
pub mod io;
pub mod isotropy;
pub mod jepa;
pub mod jepa_diag;
pub mod jepa_report;
pub mod synthetic;
pub mod train;
pub mod vit;

#[cfg(all(test, feature = "metal"))]
mod metal_tests;

pub use adaln::{AdaLnConfig, AdaLnPredictor};
pub use io::{active_candle_model, to_safetensors_bytes, CandleModel};
pub use jepa::{train_jepa, EpochDiagnostics, FrameTransition, JepaLoss, JepaTrainConfig, JepaTrainReport, JepaTrainer, LatentDiagnostics, SliceTest};
pub use train::{train_adaln, train_adaln_with, AdaLnTrainConfig, AdaLnTrainReport};
pub use vit::{CandleVitEncoder, VitTinyConfig};

/// The CPU device every model in this module runs on unless told otherwise.
#[inline]
pub fn candle_cpu_device() -> Device {
    Device::Cpu
}

/// Whether this build can run candle on the Apple GPU (the `metal` feature is on).
///
/// `true` means [`ComputeDevice::Metal`] can be requested; it does not mean a GPU is present
/// at runtime (`Device::new_metal` can still fail, and [`candle_default_device`] then logs the
/// reason and falls back to CPU). Every op these models use has a Metal kernel in candle
/// 0.11.1, forward and backward (see the crate README), so there is no per-path fallback.
#[inline]
pub const fn metal_supported() -> bool {
    cfg!(feature = "metal")
}

/// The best device available: Metal GPU 0 with the `metal` feature, otherwise CPU.
///
/// When Metal was compiled in but cannot be opened, the reason goes to stderr and the result
/// is CPU. Precision is never reduced: every model is F32 on either device.
pub fn candle_default_device() -> Device {
    #[cfg(feature = "metal")]
    {
        match Device::new_metal(0) {
            Ok(d) => return d,
            Err(e) => eprintln!("weftos-worldmodel-impls: metal device unavailable ({e}); falling back to CPU"),
        }
    }
    Device::Cpu
}

/// Which device a model or a training run uses. `Copy`, so it can live in
/// [`JepaTrainConfig`]; [`ComputeDevice::resolve`] opens the actual [`Device`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ComputeDevice {
    /// CPU (the default; the only choice without the `metal` feature).
    #[default]
    Cpu,
    /// Apple GPU 0. An error from [`ComputeDevice::resolve`] without the `metal` feature or
    /// without a GPU: it never silently runs on CPU.
    Metal,
    /// [`candle_default_device`]: Metal when available, otherwise CPU.
    Auto,
}

impl ComputeDevice {
    /// Open the device.
    pub fn resolve(self) -> CandleResult<Device> {
        match self {
            Self::Cpu => Ok(Device::Cpu),
            Self::Auto => Ok(candle_default_device()),
            #[cfg(feature = "metal")]
            Self::Metal => Ok(Device::new_metal(0)?),
            #[cfg(not(feature = "metal"))]
            Self::Metal => invalid("ComputeDevice::Metal needs the `metal` feature"),
        }
    }
}

/// Name of the architecture/config tensor carried by every checkpoint.
pub const CONFIG_TENSOR: &str = "lewm.config";
/// Length of [`CONFIG_TENSOR`].
pub const CONFIG_LEN: usize = 8;
/// Checkpoint format version stored in `lewm.config[1]`.
pub const CHECKPOINT_FORMAT: f32 = 1.0;
/// `lewm.config[0]` for a ViT-tiny encoder.
pub const ARCH_VIT_TINY: f32 = 1.0;
/// `lewm.config[0]` for an AdaLN predictor.
pub const ARCH_ADALN: f32 = 2.0;

/// Errors from building, running, loading or training a candle model.
#[derive(Debug)]
pub enum CandleModelError {
    /// Error raised inside candle.
    Candle(candle_core::Error),
    /// Bad input or checkpoint (message says what and why).
    Invalid(String),
}

impl fmt::Display for CandleModelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Candle(e) => write!(f, "candle: {e}"),
            Self::Invalid(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for CandleModelError {}

impl From<candle_core::Error> for CandleModelError {
    fn from(e: candle_core::Error) -> Self {
        Self::Candle(e)
    }
}

impl From<CandleModelError> for WorldModelError {
    fn from(e: CandleModelError) -> Self {
        match e {
            CandleModelError::Candle(_) => WorldModelError::Other("candle model runtime error"),
            CandleModelError::Invalid(_) => WorldModelError::SegmentCodec,
        }
    }
}

/// Result alias for this module.
pub type CandleResult<T> = Result<T, CandleModelError>;

pub(crate) fn invalid<T>(msg: impl Into<String>) -> CandleResult<T> {
    Err(CandleModelError::Invalid(msg.into()))
}

/// SplitMix64: deterministic init without seeding candle's CPU RNG (candle 0.11 refuses
/// `Device::Cpu.set_seed`).
#[derive(Debug, Clone)]
pub(crate) struct SplitMix64(u64);

impl SplitMix64 {
    pub(crate) fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub(crate) fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `[-1, 1)`.
    pub(crate) fn uniform(&mut self) -> f32 {
        ((self.next_u64() >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
    }
}

/// Re-initialise every variable of `varmap` deterministically from `seed`, in name order.
///
/// `zero_init(name)` marks adaLN-zero tensors. Otherwise: `*.bias` → 0; rank-1 `*.weight`
/// (norm gains) and `*_std` → 1; other rank-1 → 0; `pos_embed` → U(±0.02·√3) (std 0.02);
/// rank-2 → U(±1/√fan_in). [`CONFIG_TENSOR`] is left alone.
pub(crate) fn seeded_reinit(
    varmap: &VarMap,
    seed: u64,
    zero_init: impl Fn(&str) -> bool,
) -> CandleResult<()> {
    let data = varmap
        .data()
        .lock()
        .map_err(|_| CandleModelError::Invalid("varmap lock poisoned".into()))?;
    let mut names: Vec<&String> = data.keys().collect();
    names.sort();
    let mut rng = SplitMix64::new(seed);
    for name in names {
        if name == CONFIG_TENSOR {
            continue;
        }
        let var = &data[name];
        let dims = var.dims().to_vec();
        let n: usize = dims.iter().product();
        let vals: Vec<f32> = if zero_init(name) || name.ends_with(".bias") {
            vec![0.0; n]
        } else if dims.len() == 1 {
            let one = name.ends_with(".weight") || name.ends_with("_std");
            vec![if one { 1.0 } else { 0.0 }; n]
        } else if name == "pos_embed" {
            (0..n).map(|_| 0.02 * 3f32.sqrt() * rng.uniform()).collect()
        } else {
            let bound = 1.0 / (dims[dims.len() - 1] as f32).sqrt();
            (0..n).map(|_| bound * rng.uniform()).collect()
        };
        var.set(&Tensor::from_vec(vals, dims.as_slice(), var.device())?)?;
    }
    Ok(())
}

/// Read and check [`CONFIG_TENSOR`].
pub(crate) fn read_config(t: &Tensor, arch: f32) -> CandleResult<[usize; 6]> {
    let v: Vec<f32> = t.to_dtype(DType::F32)?.flatten_all()?.to_vec1()?;
    if v.len() != CONFIG_LEN {
        return invalid(format!("{CONFIG_TENSOR} must have {CONFIG_LEN} entries, got {}", v.len()));
    }
    let what = |a: f32| match a as u32 {
        1 => "vit_tiny encoder",
        2 => "adaln predictor",
        _ => "unknown architecture",
    };
    if v[0] != arch {
        return invalid(format!(
            "checkpoint is a {} (arch {}), expected a {}",
            what(v[0]),
            v[0],
            what(arch)
        ));
    }
    if v[1] != CHECKPOINT_FORMAT {
        return invalid(format!("unsupported checkpoint format {} (expected {CHECKPOINT_FORMAT})", v[1]));
    }
    let mut out = [0usize; 6];
    for (o, x) in out.iter_mut().zip(v[2..].iter()) {
        if *x < 0.0 || x.fract() != 0.0 {
            return invalid(format!("{CONFIG_TENSOR} entry {x} is not a non-negative integer"));
        }
        *o = *x as usize;
    }
    Ok(out)
}

/// Build the [`CONFIG_TENSOR`] value.
pub(crate) fn config_tensor(arch: f32, params: [usize; 6]) -> CandleResult<Tensor> {
    let mut v = vec![arch, CHECKPOINT_FORMAT];
    v.extend(params.iter().map(|&p| p as f32));
    Ok(Tensor::from_vec(v, CONFIG_LEN, &Device::Cpu)?)
}

/// Write `value` into `var`, moving it to the variable's device first.
pub(crate) fn assign(var: &candle_core::Var, value: &Tensor) -> candle_core::Result<()> {
    var.set(&value.to_device(var.device())?)
}

/// Differentiable LayerNorm over the last dim without affine (candle's fused
/// `ops::layer_norm` and `ops::softmax_last_dim` have no backward pass in 0.11).
pub(crate) fn layer_norm(x: &Tensor, eps: f64) -> candle_core::Result<Tensor> {
    let mean = x.mean_keepdim(D::Minus1)?;
    let xc = x.broadcast_sub(&mean)?;
    let var = xc.sqr()?.mean_keepdim(D::Minus1)?;
    xc.broadcast_div(&(var + eps)?.sqrt()?)
}

/// LayerNorm with gain `w` and bias `b`.
pub(crate) fn layer_norm_affine(
    x: &Tensor,
    w: &Tensor,
    b: &Tensor,
    eps: f64,
) -> candle_core::Result<Tensor> {
    layer_norm(x, eps)?.broadcast_mul(w)?.broadcast_add(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_tensor_roundtrip_and_arch_check() {
        let t = config_tensor(ARCH_ADALN, [192, 32, 64, 1, 0, 0]).unwrap();
        assert_eq!(read_config(&t, ARCH_ADALN).unwrap(), [192, 32, 64, 1, 0, 0]);
        let err = read_config(&t, ARCH_VIT_TINY).unwrap_err().to_string();
        assert!(err.contains("adaln predictor") && err.contains("expected a vit_tiny"), "{err}");
        let _ = candle_cpu_device();
    }

    #[test]
    fn composed_layer_norm_matches_candle_fused_layer_norm() {
        let vals: Vec<f32> = (0..12).map(|v| v as f32 * 0.37 - 1.0).collect();
        let x = Tensor::from_vec(vals, (3, 4), &Device::Cpu).unwrap();
        let w = Tensor::ones(4, DType::F32, &Device::Cpu).unwrap();
        let b = Tensor::zeros(4, DType::F32, &Device::Cpu).unwrap();
        let ours: Vec<f32> = layer_norm_affine(&x, &w, &b, 1e-5).unwrap().flatten_all().unwrap().to_vec1().unwrap();
        let fused: Vec<f32> =
            candle_nn::ops::layer_norm(&x, &w, &b, 1e-5).unwrap().flatten_all().unwrap().to_vec1().unwrap();
        for (a, b) in ours.iter().zip(fused.iter()) {
            assert!((a - b).abs() < 1e-4, "{a} vs {b}");
        }
    }
}
