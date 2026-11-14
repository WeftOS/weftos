//! AdaLN-modulated action-conditioned predictor `pred_φ(z_t, a_t) → ẑ_{t+1}` (plan W5).
//!
//! DiT-style adaLN-zero over a residual MLP stack on the latent:
//!
//! ```text
//! zn = (z − z_mean) / z_std                       h = in_proj(zn)             (B, H)
//! c  = cond.fc2(silu(cond.fc1(a)))                 (32 → H → H)
//! per block i:  shift, scale, gate = blocks.i.adaln(silu(c))
//!               h += gate ⊙ mlp(LN(h) ⊙ (1 + scale) + shift)      (mlp: H → 4H → H, GELU)
//! final:        shift, scale = final.adaln(silu(c))
//!               δn = final.out(LN(h) ⊙ (1 + scale) + shift)       (H → 192)
//! ẑ  = z + δn ⊙ delta_std + delta_mean
//! ```
//!
//! Every `*.adaln.*` and `final.out.*` tensor starts at zero, so a freshly initialised
//! predictor is `ẑ = z + delta_mean`: copy-last for [`AdaLnPredictor::new_random`] (stats
//! 0 / 1), the residual-mean model once [`crate::candle::train`] has set the stats. The
//! `norm_stats.*` tensors are fixed during training and travel in the checkpoint.

use candle_core::{DType, Device, Module, Tensor};
use candle_nn::{linear, Init, Linear, VarBuilder, VarMap};
use weftos_worldmodel_core::{Action, Latent, Predictor, WorldModelError, WorldModelResult, LATENT_DIM};

use super::vit::set_var;
use super::{
    config_tensor, invalid, layer_norm, seeded_reinit, CandleModelError, CandleResult, ARCH_ADALN,
    CONFIG_LEN, CONFIG_TENSOR,
};
use crate::action_encoder::ACTION_CODE_DIM;

/// LayerNorm epsilon inside the AdaLN blocks.
pub const ADALN_LN_EPS: f64 = 1e-6;
/// MLP hidden width = `ADALN_MLP_RATIO · hidden_dim`.
pub const ADALN_MLP_RATIO: usize = 4;
/// Names of the fixed normalisation tensors (excluded from the optimiser).
pub const NORM_STATS: [&str; 4] =
    ["norm_stats.z_mean", "norm_stats.z_std", "norm_stats.delta_mean", "norm_stats.delta_std"];

/// AdaLN predictor hyper-parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdaLnConfig {
    /// Latent width (must equal [`LATENT_DIM`]).
    pub latent_dim: usize,
    /// Action code width (must equal [`ACTION_CODE_DIM`]).
    pub action_dim: usize,
    /// Residual stream width.
    pub hidden_dim: usize,
    /// Number of AdaLN blocks.
    pub depth: usize,
}

impl Default for AdaLnConfig {
    fn default() -> Self {
        Self { latent_dim: LATENT_DIM, action_dim: ACTION_CODE_DIM, hidden_dim: 256, depth: 2 }
    }
}

impl AdaLnConfig {
    /// Check the widths against the v1 contract.
    pub fn validate(&self) -> CandleResult<()> {
        if self.latent_dim != LATENT_DIM {
            return invalid(format!("adaln latent_dim {} != {LATENT_DIM}", self.latent_dim));
        }
        if self.action_dim != ACTION_CODE_DIM {
            return invalid(format!("adaln action_dim {} != {ACTION_CODE_DIM}", self.action_dim));
        }
        if self.hidden_dim == 0 {
            return invalid("adaln hidden_dim must be > 0");
        }
        Ok(())
    }

    pub(crate) fn params(&self) -> [usize; 6] {
        [self.latent_dim, self.action_dim, self.hidden_dim, self.depth, 0, 0]
    }

    pub(crate) fn from_params(p: [usize; 6]) -> CandleResult<Self> {
        let cfg = Self { latent_dim: p[0], action_dim: p[1], hidden_dim: p[2], depth: p[3] };
        cfg.validate()?;
        Ok(cfg)
    }
}

#[derive(Debug, Clone)]
struct AdaBlock {
    adaln: Linear,
    fc1: Linear,
    fc2: Linear,
}

/// `LN(h) ⊙ (1 + scale) + shift`.
fn modulate(h: &Tensor, shift: &Tensor, scale: &Tensor) -> candle_core::Result<Tensor> {
    layer_norm(h, ADALN_LN_EPS)?.broadcast_mul(&scale.affine(1.0, 1.0)?)?.broadcast_add(shift)
}

/// Built AdaLN graph over a [`VarMap`].
#[derive(Clone)]
pub(crate) struct AdaNet {
    pub(crate) varmap: VarMap,
    pub(crate) device: Device,
    in_proj: Linear,
    cond_fc1: Linear,
    cond_fc2: Linear,
    blocks: Vec<AdaBlock>,
    final_adaln: Linear,
    final_out: Linear,
    stats: [Tensor; 4],
}

impl std::fmt::Debug for AdaNet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdaNet").field("blocks", &self.blocks.len()).finish_non_exhaustive()
    }
}

impl AdaNet {
    pub(crate) fn build(cfg: &AdaLnConfig, seed: u64, device: &Device) -> CandleResult<Self> {
        cfg.validate()?;
        let varmap = VarMap::new();
        let vb = VarBuilder::from_varmap(&varmap, DType::F32, device);
        let (l, a, h) = (cfg.latent_dim, cfg.action_dim, cfg.hidden_dim);
        vb.get_with_hints(CONFIG_LEN, CONFIG_TENSOR, Init::Const(0.0))?;
        let blocks = (0..cfg.depth)
            .map(|i| {
                let vb = vb.pp("blocks").pp(i);
                Ok(AdaBlock {
                    adaln: linear(h, 3 * h, vb.pp("adaln"))?,
                    fc1: linear(h, ADALN_MLP_RATIO * h, vb.pp("mlp").pp("fc1"))?,
                    fc2: linear(ADALN_MLP_RATIO * h, h, vb.pp("mlp").pp("fc2"))?,
                })
            })
            .collect::<candle_core::Result<Vec<_>>>()?;
        let stat = |name: &str| vb.get_with_hints(l, name, Init::Const(0.0));
        let net = Self {
            in_proj: linear(l, h, vb.pp("in_proj"))?,
            cond_fc1: linear(a, h, vb.pp("cond").pp("fc1"))?,
            cond_fc2: linear(h, h, vb.pp("cond").pp("fc2"))?,
            blocks,
            final_adaln: linear(h, 2 * h, vb.pp("final").pp("adaln"))?,
            final_out: linear(h, l, vb.pp("final").pp("out"))?,
            stats: [stat(NORM_STATS[0])?, stat(NORM_STATS[1])?, stat(NORM_STATS[2])?, stat(NORM_STATS[3])?],
            varmap,
            device: device.clone(),
        };
        seeded_reinit(&net.varmap, seed, |n| n.contains("adaln.") || n.starts_with("final.out."))?;
        set_var(&net.varmap, CONFIG_TENSOR, &config_tensor(ARCH_ADALN, cfg.params())?)?;
        Ok(net)
    }

    /// Set the fixed normalisation stats (`z_mean`, `z_std`, `delta_mean`, `delta_std`).
    pub(crate) fn set_stats(&self, stats: [&[f32]; 4]) -> CandleResult<()> {
        for (name, v) in NORM_STATS.iter().zip(stats) {
            set_var(&self.varmap, name, &Tensor::from_slice(v, v.len(), &Device::Cpu)?)?;
        }
        Ok(())
    }

    /// Normalised residual `δn` for `z (B, L)`, `a (B, A)`.
    pub(crate) fn forward_delta_norm(&self, z: &Tensor, a: &Tensor) -> candle_core::Result<Tensor> {
        let [z_mean, z_std, ..] = &self.stats;
        let zn = z.broadcast_sub(z_mean)?.broadcast_div(z_std)?;
        let mut h = self.in_proj.forward(&zn)?;
        let c = self.cond_fc2.forward(&self.cond_fc1.forward(a)?.silu()?)?.silu()?;
        for blk in &self.blocks {
            let m = blk.adaln.forward(&c)?.chunk(3, 1)?;
            let x = modulate(&h, &m[0], &m[1])?;
            let y = blk.fc2.forward(&blk.fc1.forward(&x)?.gelu_erf()?)?;
            h = (h + m[2].broadcast_mul(&y)?)?;
        }
        let m = self.final_adaln.forward(&c)?.chunk(2, 1)?;
        self.final_out.forward(&modulate(&h, &m[0], &m[1])?)
    }

    /// `ẑ = z + δn ⊙ delta_std + delta_mean`.
    pub(crate) fn forward(&self, z: &Tensor, a: &Tensor) -> candle_core::Result<Tensor> {
        let [_, _, d_mean, d_std] = &self.stats;
        z + self.forward_delta_norm(z, a)?.broadcast_mul(d_std)?.broadcast_add(d_mean)?
    }
}

/// Stack latents / actions into `(B, dim)` tensors.
pub(crate) fn stack_rows<const N: usize>(rows: impl Iterator<Item = [f32; N]>, b: usize, device: &Device) -> CandleResult<Tensor> {
    let flat: Vec<f32> = rows.flat_map(|r| r.into_iter()).collect();
    Ok(Tensor::from_vec(flat, (b, N), device)?)
}

/// AdaLN `pred_φ`. Unloaded it returns [`WorldModelError::Unavailable`].
#[derive(Debug, Clone, Default)]
pub struct AdaLnPredictor {
    /// Where the weights came from, when loaded from a file.
    pub weights_path: Option<String>,
    /// Residual stream width (mirrors `config.hidden_dim`).
    pub hidden_dim: usize,
    /// Architecture hyper-parameters.
    pub config: AdaLnConfig,
    pub(crate) net: Option<AdaNet>,
}

impl AdaLnPredictor {
    /// Default config, no weights.
    pub fn unloaded() -> Self {
        let config = AdaLnConfig::default();
        Self { weights_path: None, hidden_dim: config.hidden_dim, config, net: None }
    }

    /// Default config with deterministic random weights (adaLN-zero: predicts copy-last).
    pub fn new_random(seed: u64) -> CandleResult<Self> {
        Self::new_random_with(AdaLnConfig::default(), seed)
    }

    /// `config` with deterministic random weights from `seed`.
    pub fn new_random_with(config: AdaLnConfig, seed: u64) -> CandleResult<Self> {
        Self::new_random_on(config, seed, &Device::Cpu)
    }

    /// [`Self::new_random_with`] on `device` (CPU or Metal).
    pub fn new_random_on(config: AdaLnConfig, seed: u64, device: &Device) -> CandleResult<Self> {
        let net = AdaNet::build(&config, seed, device)?;
        net.set_stats([&[0.0; LATENT_DIM], &[1.0; LATENT_DIM], &[0.0; LATENT_DIM], &[1.0; LATENT_DIM]])?;
        Ok(Self { weights_path: None, hidden_dim: config.hidden_dim, config, net: Some(net) })
    }

    /// Whether weights are present (loaded, trained or random-initialised).
    pub fn is_loaded(&self) -> bool {
        self.net.is_some()
    }

    /// The device the weights live on (`None` when unloaded).
    pub fn device(&self) -> Option<&Device> {
        self.net.as_ref().map(|n| &n.device)
    }

    /// Predict a batch in one forward pass.
    pub fn predict_batch(&self, z: &[Latent], actions: &[Action]) -> CandleResult<Vec<Latent>> {
        let net = self.net.as_ref().ok_or_else(|| CandleModelError::Invalid("adaln predictor has no weights".into()))?;
        if z.len() != actions.len() || z.is_empty() {
            return invalid(format!("adaln batch: {} latents vs {} actions", z.len(), actions.len()));
        }
        let zt = stack_rows(z.iter().copied(), z.len(), &net.device)?;
        let at = stack_rows(actions.iter().map(|a| a.code), z.len(), &net.device)?;
        let rows: Vec<Vec<f32>> = net.forward(&zt, &at)?.to_vec2()?;
        Ok(rows.into_iter().map(|r| r.try_into().expect("latent width fixed by config")).collect())
    }
}

impl Predictor for AdaLnPredictor {
    fn predict(&self, z_t: &Latent, action: &Action) -> WorldModelResult<Latent> {
        if !self.is_loaded() {
            return Err(WorldModelError::Unavailable);
        }
        self.predict_batch(&[*z_t], &[*action])
            .map(|mut v| v.remove(0))
            .map_err(|_| WorldModelError::PredictFailed)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use weftos_worldmodel_core::zero_latent;

    pub(crate) fn small() -> AdaLnConfig {
        AdaLnConfig { hidden_dim: 32, depth: 1, ..AdaLnConfig::default() }
    }

    #[test]
    fn unloaded_adaln_unavailable() {
        let pred = AdaLnPredictor::unloaded();
        assert!(!pred.is_loaded());
        assert!(matches!(pred.predict(&zero_latent(), &Action::null()), Err(WorldModelError::Unavailable)));
    }

    #[test]
    fn adaln_zero_init_predicts_copy_last() {
        let pred = AdaLnPredictor::new_random(3).unwrap();
        assert!(pred.is_loaded());
        let mut z = zero_latent();
        z[5] = 1.25;
        let mut a = Action::null();
        a.code[0] = 0.7;
        let out = pred.predict(&z, &a).unwrap();
        for (o, zi) in out.iter().zip(z.iter()) {
            assert!((o - zi).abs() < 1e-6);
        }
    }

    #[test]
    fn non_zero_head_makes_the_prediction_depend_on_the_action() {
        let pred = AdaLnPredictor::new_random_with(small(), 4).unwrap();
        let net = pred.net.as_ref().unwrap();
        // Lift adaLN-zero on the final layer so the action path reaches the output.
        for name in ["final.out.weight", "final.adaln.weight"] {
            let var = net.varmap.data().lock().unwrap()[name].clone();
            var.set(&(var.as_tensor().ones_like().unwrap() * 0.05).unwrap()).unwrap();
        }
        let z = zero_latent();
        let (mut a1, mut a2) = (Action::null(), Action::null());
        a1.code[0] = 1.0;
        a2.code[0] = -1.0;
        let batch = pred.predict_batch(&[z, z], &[a1, a2]).unwrap();
        assert_ne!(batch[0], batch[1]);
        for (x, y) in pred.predict(&z, &a1).unwrap().iter().zip(batch[0].iter()) {
            assert!((x - y).abs() < 1e-6, "single vs batched forward: {x} vs {y}");
        }
        assert!(pred.predict_batch(&[z], &[]).is_err());
    }

    #[test]
    fn config_widths_are_checked() {
        assert!(AdaLnConfig { action_dim: 16, ..AdaLnConfig::default() }.validate().is_err());
        assert!(AdaLnPredictor::new_random_with(AdaLnConfig { latent_dim: 8, ..small() }, 1).is_err());
    }
}
