//! ViT-tiny sensor encoder: RGB frame bytes → 192-d latent (plan W5).
//!
//! # Observation byte layout
//!
//! One frame is `3 · H · W` bytes (`H = W = image_size`), **HWC interleaved RGB u8**: row-major
//! pixels, each pixel `R, G, B`. Pixels are scaled to `[-1, 1]` (`x / 127.5 − 1`). Any other
//! length is refused with [`WorldModelError::InvalidObservation`] (the checked entry point
//! [`CandleVitEncoder::encode_checked`] says what length it wanted).
//!
//! # Graph
//!
//! ```text
//! patches (B, N, 3·p·p) ──patch_embed.proj──▶ (B, N, E) + pos_embed (N, E)
//!   └─ depth × [ x += attn(norm1(x)) ; x += mlp(norm2(x)) ]   (pre-norm, MHSA, GELU MLP ×4)
//!   └─ norm ─ mean over N ─ head ──▶ (B, latent_dim = 192)
//! ```
//!
//! The patch embedding is unfold + linear, numerically a stride-`p` conv whose kernel is
//! flattened in `(c, ky, kx)` order: a PyTorch `Conv2d(3, E, p, p)` weight `(E, 3, p, p)`
//! reshaped to `(E, 3·p·p)` loads as `patch_embed.proj.weight` unchanged.

use candle_core::{DType, Device, Module, Tensor, D};
use candle_nn::{linear, Init, Linear, VarBuilder, VarMap};
use weftos_worldmodel_core::{Encoder, Latent, WorldModelError, WorldModelResult, LATENT_DIM};

use super::{
    config_tensor, invalid, layer_norm_affine, seeded_reinit, CandleModelError, CandleResult,
    ARCH_VIT_TINY, CONFIG_LEN, CONFIG_TENSOR,
};

/// MLP hidden width = `MLP_RATIO · embed_dim`.
pub const VIT_MLP_RATIO: usize = 4;
/// Std of the random `pos_embed` init (timm's 0.02).
pub const VIT_POS_EMBED_STD: f64 = 0.02;
/// LayerNorm epsilon (timm ViT default).
pub const VIT_LN_EPS: f64 = 1e-6;

/// ViT-tiny hyper-parameters aligned with the SIGReg 192-d contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VitTinyConfig {
    /// Input image side length (square RGB).
    pub image_size: usize,
    /// Patch size (image_size must be divisible by patch_size).
    pub patch_size: usize,
    /// Transformer hidden width.
    pub embed_dim: usize,
    /// Number of transformer blocks.
    pub depth: usize,
    /// Attention heads.
    pub num_heads: usize,
    /// Output latent width (must equal [`LATENT_DIM`] for v1).
    pub latent_dim: usize,
}

impl Default for VitTinyConfig {
    fn default() -> Self {
        Self { image_size: 64, patch_size: 8, embed_dim: 192, depth: 4, num_heads: 3, latent_dim: LATENT_DIM }
    }
}

impl VitTinyConfig {
    /// Validate geometric constraints (no weights required).
    pub fn validate(&self) -> WorldModelResult<()> {
        if self.latent_dim != LATENT_DIM {
            return Err(WorldModelError::dim_mismatch(self.latent_dim as u16));
        }
        if self.patch_size == 0 || self.image_size == 0 || !self.image_size.is_multiple_of(self.patch_size) {
            return Err(WorldModelError::Other("invalid vit patch geometry"));
        }
        if self.num_heads == 0 || self.embed_dim == 0 || !self.embed_dim.is_multiple_of(self.num_heads) {
            return Err(WorldModelError::Other("invalid vit head geometry"));
        }
        Ok(())
    }

    /// Bytes per frame: `3 · image_size²`.
    pub fn frame_len(&self) -> usize {
        3 * self.image_size * self.image_size
    }

    /// Patches per frame.
    pub fn num_patches(&self) -> usize {
        (self.image_size / self.patch_size).pow(2)
    }

    pub(crate) fn params(&self) -> [usize; 6] {
        [self.image_size, self.patch_size, self.embed_dim, self.depth, self.num_heads, self.latent_dim]
    }

    pub(crate) fn from_params(p: [usize; 6]) -> CandleResult<Self> {
        let cfg = Self { image_size: p[0], patch_size: p[1], embed_dim: p[2], depth: p[3], num_heads: p[4], latent_dim: p[5] };
        cfg.validate().map_err(|e| CandleModelError::Invalid(format!("vit config {p:?}: {e}")))?;
        Ok(cfg)
    }
}

#[derive(Debug, Clone)]
struct Block {
    norm1: (Tensor, Tensor),
    qkv: Linear,
    proj: Linear,
    norm2: (Tensor, Tensor),
    fc1: Linear,
    fc2: Linear,
}

fn norm_params(vb: &VarBuilder, dim: usize) -> candle_core::Result<(Tensor, Tensor)> {
    Ok((vb.get_with_hints(dim, "weight", Init::Const(1.0))?, vb.get_with_hints(dim, "bias", Init::Const(0.0))?))
}

impl Block {
    fn new(vb: VarBuilder, e: usize) -> candle_core::Result<Self> {
        Ok(Self {
            norm1: norm_params(&vb.pp("norm1"), e)?,
            qkv: linear(e, 3 * e, vb.pp("attn").pp("qkv"))?,
            proj: linear(e, e, vb.pp("attn").pp("proj"))?,
            norm2: norm_params(&vb.pp("norm2"), e)?,
            fc1: linear(e, VIT_MLP_RATIO * e, vb.pp("mlp").pp("fc1"))?,
            fc2: linear(VIT_MLP_RATIO * e, e, vb.pp("mlp").pp("fc2"))?,
        })
    }

    fn forward(&self, x: &Tensor, heads: usize, train: bool) -> candle_core::Result<Tensor> {
        let (b, n, e) = x.dims3()?;
        let hd = e / heads;
        let h = norm(x, &self.norm1, train)?;
        let qkv = self.qkv.forward(&h)?.reshape((b, n, 3, heads, hd))?.permute((2, 0, 3, 1, 4))?;
        let q = qkv.get(0)?.contiguous()?;
        let k = qkv.get(1)?.t()?.contiguous()?;
        let v = qkv.get(2)?.contiguous()?;
        let att = (q.matmul(&k)? * (1.0 / (hd as f64).sqrt()))?;
        let att = if train { candle_nn::ops::softmax(&att, D::Minus1)? } else { candle_nn::ops::softmax_last_dim(&att)? };
        let out = att.matmul(&v)?.transpose(1, 2)?.contiguous()?.reshape((b, n, e))?;
        let x = (x + self.proj.forward(&out)?)?;
        let h = norm(&x, &self.norm2, train)?;
        let m = self.fc2.forward(&self.fc1.forward(&h)?.gelu_erf()?)?;
        x + m
    }
}

/// Fused LayerNorm for inference, composed (differentiable) for training.
fn norm(x: &Tensor, (w, b): &(Tensor, Tensor), train: bool) -> candle_core::Result<Tensor> {
    if train || !x.is_contiguous() {
        layer_norm_affine(x, w, b, VIT_LN_EPS)
    } else {
        candle_nn::ops::layer_norm(x, w, b, VIT_LN_EPS as f32)
    }
}

/// Built ViT graph over a [`VarMap`].
#[derive(Clone)]
pub(crate) struct VitNet {
    pub(crate) varmap: VarMap,
    patch: Linear,
    pub(crate) pos: Tensor,
    blocks: Vec<Block>,
    norm: (Tensor, Tensor),
    head: Linear,
}

impl std::fmt::Debug for VitNet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VitNet").field("blocks", &self.blocks.len()).finish_non_exhaustive()
    }
}

impl VitNet {
    pub(crate) fn build(cfg: &VitTinyConfig, seed: u64) -> CandleResult<Self> {
        cfg.validate().map_err(|e| CandleModelError::Invalid(format!("vit config: {e}")))?;
        let varmap = VarMap::new();
        let vb = VarBuilder::from_varmap(&varmap, DType::F32, &Device::Cpu);
        let (e, p) = (cfg.embed_dim, cfg.patch_size);
        let cfg_var = vb.get_with_hints(CONFIG_LEN, CONFIG_TENSOR, Init::Const(0.0))?;
        let patch = linear(3 * p * p, e, vb.pp("patch_embed").pp("proj"))?;
        let pos = vb.get_with_hints((cfg.num_patches(), e), "pos_embed", Init::Const(0.0))?;
        let blocks = (0..cfg.depth)
            .map(|i| Block::new(vb.pp("blocks").pp(i), e))
            .collect::<candle_core::Result<Vec<_>>>()?;
        let norm = norm_params(&vb.pp("norm"), e)?;
        let head = linear(e, cfg.latent_dim, vb.pp("head"))?;
        seeded_reinit(&varmap, seed, |_| false)?;
        set_var(&varmap, CONFIG_TENSOR, &config_tensor(ARCH_VIT_TINY, cfg.params())?)?;
        drop(cfg_var);
        Ok(Self { varmap, patch, pos, blocks, norm, head })
    }

    /// `(B, latent_dim)` latents for `B` frames.
    pub(crate) fn forward(&self, cfg: &VitTinyConfig, frames: &[&[u8]], train: bool) -> CandleResult<Tensor> {
        let patches = patchify(cfg, frames)?;
        let mut x = self.patch.forward(&patches)?.broadcast_add(&self.pos)?;
        for blk in &self.blocks {
            x = blk.forward(&x, cfg.num_heads, train)?;
        }
        let x = norm(&x, &self.norm, train)?.mean(1)?;
        Ok(self.head.forward(&x)?)
    }
}

pub(crate) fn set_var(varmap: &VarMap, name: &str, value: &Tensor) -> CandleResult<()> {
    let data = varmap.data().lock().map_err(|_| CandleModelError::Invalid("varmap lock poisoned".into()))?;
    match data.get(name) {
        Some(var) => Ok(var.set(value)?),
        None => invalid(format!("no variable named {name}")),
    }
}

/// Frame bytes → `(B, N, 3·p·p)` patches in `(c, ky, kx)` order.
fn patchify(cfg: &VitTinyConfig, frames: &[&[u8]]) -> CandleResult<Tensor> {
    let (s, p) = (cfg.image_size, cfg.patch_size);
    if frames.is_empty() {
        return invalid("vit encoder: no frames");
    }
    let mut data = Vec::with_capacity(frames.len() * cfg.frame_len());
    for (i, f) in frames.iter().enumerate() {
        if f.len() != cfg.frame_len() {
            return invalid(format!(
                "vit encoder frame {i}: expected {} bytes (3 x {s} x {s} RGB u8, HWC interleaved), got {}",
                cfg.frame_len(),
                f.len()
            ));
        }
        data.extend(f.iter().map(|&v| v as f32 / 127.5 - 1.0));
    }
    let (b, g) = (frames.len(), s / p);
    let t = Tensor::from_vec(data, vec![b, g, p, g, p, 3], &Device::Cpu)?;
    Ok(t.permute(vec![0, 1, 3, 5, 2, 4])?.contiguous()?.reshape((b, g * g, 3 * p * p))?)
}

fn rows_to_latents(t: &Tensor) -> CandleResult<Vec<Latent>> {
    let rows: Vec<Vec<f32>> = t.to_vec2()?;
    rows.into_iter()
        .map(|r| {
            r.try_into().map_err(|r: Vec<f32>| CandleModelError::Invalid(format!("latent width {} != {LATENT_DIM}", r.len())))
        })
        .collect()
}

/// Candle ViT-tiny encoder.
///
/// Unloaded (no weights) it returns [`WorldModelError::Unavailable`] so callers fall back to
/// local ECC (ADR-090). Loaded from a checkpoint (`crate::candle::io`) or randomly initialised
/// with [`CandleVitEncoder::new_random`] (counts as loaded), it runs the forward pass on CPU.
#[derive(Debug, Clone)]
pub struct CandleVitEncoder {
    /// Architecture hyper-parameters.
    pub config: VitTinyConfig,
    /// Where the weights came from, when loaded from a file.
    pub weights_path: Option<String>,
    pub(crate) net: Option<VitNet>,
}

impl CandleVitEncoder {
    /// Default ViT-tiny config and no weights.
    pub fn unloaded() -> Self {
        Self { config: VitTinyConfig::default(), weights_path: None, net: None }
    }

    /// Validated config, still unloaded.
    pub fn with_config(config: VitTinyConfig) -> WorldModelResult<Self> {
        config.validate()?;
        Ok(Self { config, weights_path: None, net: None })
    }

    /// Default config with deterministic random weights from `seed`.
    pub fn new_random(seed: u64) -> CandleResult<Self> {
        Self::new_random_with(VitTinyConfig::default(), seed)
    }

    /// `config` with deterministic random weights from `seed`.
    pub fn new_random_with(config: VitTinyConfig, seed: u64) -> CandleResult<Self> {
        Ok(Self { config, weights_path: None, net: Some(VitNet::build(&config, seed)?) })
    }

    /// Whether weights are present (loaded or random-initialised).
    pub fn is_loaded(&self) -> bool {
        self.net.is_some()
    }

    /// Encode one frame with a descriptive error.
    pub fn encode_checked(&self, frame: &[u8]) -> CandleResult<Latent> {
        Ok(self.encode_batch(&[frame])?.remove(0))
    }

    /// Encode a batch of frames in one forward pass.
    pub fn encode_batch(&self, frames: &[&[u8]]) -> CandleResult<Vec<Latent>> {
        let net = self.net.as_ref().ok_or_else(|| CandleModelError::Invalid("vit encoder has no weights".into()))?;
        rows_to_latents(&net.forward(&self.config, frames, false)?)
    }
}

impl Encoder for CandleVitEncoder {
    fn encode(&self, observation: &[u8]) -> WorldModelResult<Latent> {
        self.config.validate()?;
        if !self.is_loaded() {
            return Err(WorldModelError::Unavailable);
        }
        self.encode_checked(observation).map_err(|e| match e {
            CandleModelError::Invalid(_) => WorldModelError::InvalidObservation,
            CandleModelError::Candle(_) => WorldModelError::EncodeFailed,
        })
    }

    fn latent_dim(&self) -> usize {
        self.config.latent_dim
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Small config so debug-mode tests stay fast.
    pub(crate) fn small() -> VitTinyConfig {
        VitTinyConfig { image_size: 16, patch_size: 8, embed_dim: 24, depth: 1, num_heads: 3, latent_dim: LATENT_DIM }
    }

    pub(crate) fn frame(cfg: &VitTinyConfig, seed: u8) -> Vec<u8> {
        (0..cfg.frame_len()).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)).collect()
    }

    #[test]
    fn unloaded_encoder_unavailable() {
        let enc = CandleVitEncoder::unloaded();
        assert!(!enc.is_loaded());
        assert!(matches!(enc.encode(b"rgb"), Err(WorldModelError::Unavailable)));
        assert_eq!(enc.latent_dim(), LATENT_DIM);
    }

    #[test]
    fn bad_config_rejected() {
        let mut cfg = VitTinyConfig { latent_dim: 64, ..VitTinyConfig::default() };
        assert!(matches!(cfg.validate(), Err(WorldModelError::LatentDimMismatch { got: 64, .. })));
        cfg = VitTinyConfig { patch_size: 7, ..VitTinyConfig::default() };
        assert!(cfg.validate().is_err() && CandleVitEncoder::new_random_with(cfg, 1).is_err());
    }

    #[test]
    fn default_vit_tiny_encodes_a_64px_frame() {
        let enc = CandleVitEncoder::new_random(7).unwrap();
        assert!(enc.is_loaded());
        let f = frame(&enc.config, 3);
        assert_eq!(f.len(), 12_288);
        let z = enc.encode(&f).unwrap();
        assert!(z.iter().all(|v| v.is_finite()) && z.iter().any(|v| v.abs() > 1e-4));
    }

    #[test]
    fn random_init_is_deterministic_per_seed_and_input_sensitive() {
        let cfg = small();
        let (a, b) = (CandleVitEncoder::new_random_with(cfg, 1).unwrap(), CandleVitEncoder::new_random_with(cfg, 1).unwrap());
        let c = CandleVitEncoder::new_random_with(cfg, 2).unwrap();
        let (f1, f2) = (frame(&cfg, 1), frame(&cfg, 90));
        assert_eq!(a.encode(&f1).unwrap(), b.encode(&f1).unwrap());
        assert_ne!(a.encode(&f1).unwrap(), c.encode(&f1).unwrap());
        assert_ne!(a.encode(&f1).unwrap(), a.encode(&f2).unwrap());
        let batch = a.encode_batch(&[&f1, &f2]).unwrap();
        for (x, y) in batch[1].iter().zip(a.encode(&f2).unwrap().iter()) {
            assert!((x - y).abs() < 1e-5);
        }
    }

    #[test]
    fn wrong_frame_length_is_a_clear_error() {
        let enc = CandleVitEncoder::new_random_with(small(), 1).unwrap();
        assert!(matches!(enc.encode(&[0u8; 10]), Err(WorldModelError::InvalidObservation)));
        let msg = enc.encode_checked(&[0u8; 10]).unwrap_err().to_string();
        assert!(msg.contains("expected 768 bytes") && msg.contains("got 10"), "{msg}");
    }

    #[test]
    fn patchify_matches_a_stride_p_conv_layout() {
        // 2x2 image, patch 1: each patch is one pixel's (R, G, B).
        let cfg = VitTinyConfig { image_size: 2, patch_size: 1, embed_dim: 3, depth: 0, num_heads: 1, latent_dim: LATENT_DIM };
        let f: Vec<u8> = (0..12).map(|v| v as u8).collect();
        let p: Vec<Vec<f32>> = patchify(&cfg, &[&f]).unwrap().squeeze(0).unwrap().to_vec2().unwrap();
        assert_eq!(p.len(), 4);
        assert!((p[1][0] - (3.0 / 127.5 - 1.0)).abs() < 1e-6, "patch 1 starts at pixel (0,1) R");
    }
}
