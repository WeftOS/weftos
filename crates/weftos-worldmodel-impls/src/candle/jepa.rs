//! JEPA self-supervised training of the ViT encoder with the AdaLN `pred_φ` (plan W5, CPU).
//!
//! ```text
//! z_t    = f_θ(x_t)                       online ViT, differentiable (train mode)
//! z*_tp1 = stopgrad f_ξ(x_{t+1})          EMA target ViT, same architecture, no grad
//! ẑ_tp1  = pred_φ(z_t, a_t)               AdaLN predictor (adaLN-zero init: copy-last)
//! L      = MSE(ẑ_tp1, z*_tp1) + λ · ( R(z_t) + η · S(z_t) + v · H(z_t) + c · K(z_t) )
//! ξ      ← τ ξ + (1 − τ) θ                after every optimiser step
//! ```
//!
//! AdamW updates θ and φ (every tensor except `lewm.config` and the AdaLN `norm_stats.*`,
//! which stay at identity: the isotropy term keeps latents near zero mean, unit variance).
//!
//! # Isotropy penalty `R` (SIGReg-style)
//!
//! With `μ` the batch mean, `C` the batch covariance (`n − 1` denominator) of the online
//! latents, `σ_i = √(C_ii + ε)` (`ε =` [`ISO_STD_EPS`]) and `d = 192`:
//!
//! ```text
//! R = ( ‖μ‖² + Σ_i (σ_i − 1)² ) / d  +  γ · Σ_{i≠j} C_ij² / d
//! ```
//!
//! **Which variant.** The default is the **diagonal** part (`γ =`
//! [`JepaTrainConfig::offdiag_weight`] `= 0`). Its minimum (`μ = 0`, every `C_ii = 1`) is
//! where [`crate::sigreg::WelfordSigRegMonitor`] reports health 1: the monitor's energy is
//! `(‖μ‖² + Σ (C_ii − 1)²) / d`. The penalty uses the standard deviation, not the variance
//! (as VICReg does), because the variance form has a vanishing gradient at collapse:
//! `∂(C_ii − 1)²/∂θ ∝ σ_i`. Measured on the tiny test config: with the variance form, AdamW
//! at lr 1e-2 drives the batch to `C ≈ 0` within 20 steps and stays there for 240 steps
//! (penalty pinned at 1.000, all 192 coordinates under the harness activity floor). With the
//! std form the same run leaves the plateau after ~60 steps.
//!
//! **Sliced term `S`** (default `η =` [`JepaTrainConfig::slice_weight`] `= 1`). `R` alone is
//! satisfied by a **rank-one** latent `z = s·v` with `|v_i| = 1`: every coordinate has unit
//! variance, all are copies of one feature. Measured on the default ViT-tiny long run
//! (SYNTHETIC, 288 steps, `η = 0`): `R` fell from 1.39 to 0.005 while the train-split
//! participation ratio fell from 3.27 to 1.00. `S` is the LeJEPA SIGReg idea in its
//! cheapest form: draw `K =` [`JepaTrainConfig::slice_projections`] random unit directions
//! `u_k` per step and penalise `mean_k (σ_k − 1)²`, `σ_k = √(Var(z·u_k) + ε)`. An isotropic
//! batch has unit variance along every direction; a rank-one batch does not.
//! [`SliceTest::EppsPulley`] swaps it for the LeJEPA characteristic-function test
//! ([`epps_pulley_penalty`]).
//!
//! **VICReg terms** `H = mean_i max(0, 1 − σ_i)` and `K = Σ_{i≠j} M_ij² / d`
//! ([`vicreg_terms`], weights [`JepaTrainConfig::vic_var_weight`] and
//! [`JepaTrainConfig::vic_cov_weight`]). With [`JepaTrainConfig::vic_correlation`] (default)
//! `M` is the batch **correlation** matrix, otherwise `C`. `γ > 0` and `K` on `C` both fail
//! when the batch is smaller than `d`: `C` then has rank `< d`, so the off-diagonal mass can
//! only fall by pulling the diagonal under 1, which the monitor scores as unhealthy. The
//! correlation form is scale-free, so it pushes rank up without touching the variance.
//!
//! # Dimensional collapse (lane J bake-off, SYNTHETIC)
//!
//! Default ViT-tiny, lane G's long-run fixture, 288 steps, seed 21. With [`JepaTrainConfig::lane_g`]
//! the latents go rank one **inside the first epoch** (participation ratio 1.00, top eigenvalue
//! 157 vs 0.03 next), and the surviving direction is constant within each session (between-
//! session variance fraction 1.000). The cause is not the prediction term: the untrained
//! encoder's top principal direction is already 98 % session-explained at tiny total variance
//! (mean-pooling over patches with a 0.02-std position embedding hides blob position, while
//! distractor presence and the day background move the pooled vector), and the variance
//! penalty alone (prediction weight 0) also ends at ratio 1.00. Removing those frame-visible
//! session markers from the fixture stops the session collapse (fraction 0.03) but leaves
//! rank ≈ 2. One change at a time (batch 64 with `γ = 1`, Epps-Pulley with `K = 128`, AdaLN
//! hidden 64, cosine `τ` 0.9 → 0.996, two-step targets, VICReg on `C`) none reaches a held-out
//! ratio above 2. The default is lane G plus batch 32, `τ = 0.99`, `H` (weight 1) and `K` on
//! the correlation matrix (weight 0.04): train / held-out ratio 12.6 / 10.6, 0 collapsed
//! coordinates, held-out health 0.90, AdaLN held-out error 178 vs copy-last 350 and linear
//! 204. Training seeds 22 and 23 give held-out ratio 10.0 / 9.8, health 0.87 / 0.84, and
//! beat both baselines. The gate still vetoes on probe accuracy and temporal straightening.
//! The report's participation ratio `(tr C)² / ‖C‖_F²` on train and held-out latents is the
//! dimensional-collapse check; [`JepaTrainConfig::diagnostics`] traces it per epoch.
//!
//! # Collapse checks
//!
//! After training, [`train_jepa`] encodes every transition with the trained online encoder
//! (`z_t = f_θ(x_t)`, `z_tp1 = f_θ(x_tp1)`), runs [`crate::eval_harness::evaluate_by_day`]
//! with the trained AdaLN as the model, and runs a fresh
//! [`crate::sigreg::WelfordSigRegMonitor`] over the held-out latents. The harness predicts
//! `f_θ(x_tp1)` while training targets `f_ξ(x_tp1)`, so the report also runs the harness on
//! `(f_θ(x_t), a, f_ξ(x_tp1))` with the JEPA `pred_φ` ([`JepaTrainReport::eval_target`]). With
//! the default `τ = 0.99`, `τ^288 ≈ 0.06` and both pairings beat both baselines.

use candle_core::{Device, Tensor, Var};
use candle_nn::{AdamW, Optimizer, ParamsAdamW, VarMap};
use weftos_worldmodel_core::{Action, LATENT_DIM};

use super::adaln::{stack_rows, AdaLnConfig, AdaNet, NORM_STATS};
use super::vit::{set_var, VitNet, VitTinyConfig, VIT_POS_EMBED_STD};
use super::{assign, invalid, synthetic, AdaLnPredictor, CandleModelError, CandleResult, CandleVitEncoder, ComputeDevice, SplitMix64, CONFIG_TENSOR};
pub(crate) use super::isotropy::random_directions;
pub use super::isotropy::{epps_pulley_penalty, isotropy_penalty, participation_ratio, sliced_penalty, vicreg_terms, EP_BETA, ISO_STD_EPS};
pub use super::jepa_diag::{EpochDiagnostics, LatentDiagnostics};
pub use super::jepa_report::{encode_all, train_jepa, JepaTrainReport};
use crate::eval_harness::EvalConfig;

/// One observed step on frame bytes (any source; [`synthetic::generate`] makes test ones).
#[derive(Debug, Clone, PartialEq)]
pub struct FrameTransition {
    /// Frame at `t` (HWC RGB u8, the encoder's layout).
    pub frame_t: Vec<u8>,
    /// Action code fed to `pred_φ`.
    pub action: Action,
    /// Scalar action for the eval harness ([`crate::eval_harness::Transition::action`]).
    pub action_scalar: f32,
    /// Frame at `t + 1`.
    pub frame_tp1: Vec<u8>,
    /// Day tag (the held-out split is by day).
    pub day: u16,
    /// Session tag within the day.
    pub session: u16,
}

/// Which sliced test the `η` term uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SliceTest {
    /// `mean_k (σ_k − 1)²` over random directions ([`sliced_penalty`], lane G).
    Std,
    /// Sliced Epps-Pulley characteristic-function test against `N(0, 1)`
    /// ([`epps_pulley_penalty`], LeJEPA SIGReg).
    EppsPulley,
}

/// JEPA training settings.
#[derive(Debug, Clone, Copy)]
pub struct JepaTrainConfig {
    /// Passes over the training days.
    pub epochs: usize,
    /// AdamW learning rate.
    pub lr: f64,
    /// AdamW decoupled weight decay.
    pub weight_decay: f64,
    /// Minibatch size (at least 2: the isotropy term needs a covariance).
    pub batch_size: usize,
    /// Seed for init (online and target start identical) and shuffling.
    pub seed: u64,
    /// EMA rate `τ` of the target encoder.
    pub ema_tau: f64,
    /// Weight of the one-step prediction term (1; 0 trains on the regulariser alone, a
    /// diagnostic).
    pub prediction_weight: f64,
    /// Weight `λ` of the isotropy penalty.
    pub iso_weight: f64,
    /// Weight `γ` of the off-diagonal covariance term (0 = diagonal variant).
    pub offdiag_weight: f64,
    /// Weight `η` of the sliced term (random-direction std penalty, see the module docs).
    pub slice_weight: f64,
    /// Random unit directions `K` drawn per step for the sliced term.
    pub slice_projections: usize,
    /// Which sliced test the `η` term is.
    pub slice_test: SliceTest,
    /// EMA rate at step 0; `τ` follows a cosine from here to [`Self::ema_tau`] over the run.
    /// `None` keeps `τ = ema_tau` constant.
    pub ema_tau_start: Option<f64>,
    /// Weight of the two-step target `MSE(pred(pred(z_t, a_t), a_{t+1}), sg f_ξ(x_{t+2}))`
    /// on transitions whose successor in the same session is known (0 = off).
    pub multistep_weight: f64,
    /// Weight of the VICReg hinge `mean_i max(0, 1 − σ_i)` on the online latents.
    pub vic_var_weight: f64,
    /// Weight of the VICReg covariance `Σ_{i≠j} C_ij² / d` on the online latents.
    pub vic_cov_weight: f64,
    /// Use the correlation matrix instead of `C` in the VICReg covariance term.
    pub vic_correlation: bool,
    /// Std of the initial `pos_embed` of both encoders (the ViT's own init uses 0.02; the
    /// trainer rescales it when this differs). Position must reach the mean-pooled latent
    /// through it, see the module docs.
    pub pos_embed_std: f64,
    /// After the JEPA phase, fit a fresh AdaLN for this many epochs on the frozen encoder's
    /// own train pairs `(f_θ(x_t), a, f_θ(x_{t+1}))` with [`crate::candle::train_adaln_with`]
    /// (residual normalisation stats from those pairs). It becomes the report's deployed
    /// predictor; 0 keeps the JEPA `pred_φ`.
    pub predictor_refit_epochs: usize,
    /// Record [`EpochDiagnostics`] after every epoch (re-encodes the train split twice per
    /// epoch, so off by default).
    pub diagnostics: bool,
    /// Latest distinct days held out from training and judged by the harness.
    pub holdout_days: usize,
    /// Encoder architecture.
    pub encoder: VitTinyConfig,
    /// Predictor architecture.
    pub predictor: AdaLnConfig,
    /// Maps the harness's scalar action back to the predictor's action code.
    pub action_from_scalar: fn(f32) -> Action,
    /// Harness settings (`holdout_days` is overridden by the field above).
    pub eval: EvalConfig,
    /// Device the trainer runs on (default CPU; `Metal` needs the `metal` feature).
    pub device: ComputeDevice,
}

impl Default for JepaTrainConfig {
    /// The lane J bake-off winner (see the module docs): [`Self::lane_g`] plus batch 32,
    /// `τ = 0.99`, and the VICReg hinge (weight 1) and correlation decorrelation (weight
    /// 0.04) on the online latents.
    fn default() -> Self {
        Self { batch_size: 32, ema_tau: 0.99, vic_var_weight: 1.0, vic_cov_weight: 0.04, vic_correlation: true, ..Self::lane_g() }
    }
}

impl JepaTrainConfig {
    /// The lane G settings (std sliced term, `K = 64`, constant `τ = 0.996`, no multi-step or
    /// VICReg terms), which collapse the default ViT-tiny to rank one. Kept for comparison.
    pub fn lane_g() -> Self {
        Self {
            epochs: 10,
            lr: 1e-3,
            weight_decay: 0.01,
            batch_size: 16,
            seed: 0,
            ema_tau: 0.996,
            prediction_weight: 1.0,
            iso_weight: 1.0,
            offdiag_weight: 0.0,
            slice_weight: 1.0,
            slice_projections: 64,
            slice_test: SliceTest::Std,
            ema_tau_start: None,
            multistep_weight: 0.0,
            vic_var_weight: 0.0,
            vic_cov_weight: 0.0,
            vic_correlation: false,
            pos_embed_std: VIT_POS_EMBED_STD,
            predictor_refit_epochs: 0,
            diagnostics: false,
            holdout_days: 1,
            encoder: VitTinyConfig::default(),
            predictor: AdaLnConfig::default(),
            action_from_scalar: synthetic::action_code,
            eval: EvalConfig::default(),
            device: ComputeDevice::Cpu,
        }
    }
}

/// Loss terms (means over the transitions they were measured on).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct JepaLoss {
    /// `prediction + w₂ · prediction2 + λ · isotropy`.
    pub total: f32,
    /// MSE between `pred_φ(f_θ(x_t), a)` and `f_ξ(x_{t+1})`.
    pub prediction: f32,
    /// Regulariser `R + η · sliced + VICReg terms` on `f_θ(x_t)`, before `λ`.
    pub isotropy: f32,
    /// Two-step prediction MSE (0 when the multi-step term is off or had no successors).
    pub prediction2: f32,
}

fn trainable(varmap: &VarMap) -> CandleResult<Vec<(String, Var)>> {
    let map = varmap.data().lock().map_err(|_| CandleModelError::Invalid("varmap lock poisoned".into()))?;
    let mut named: Vec<(String, Var)> = map
        .iter()
        .filter(|(k, _)| k.as_str() != CONFIG_TENSOR && !NORM_STATS.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    named.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(named)
}

/// Online encoder, EMA target encoder, AdaLN predictor and their AdamW state.
pub struct JepaTrainer {
    cfg: JepaTrainConfig,
    online: VitNet,
    target: VitNet,
    pred: AdaNet,
    opt: AdamW,
    /// `(target var, online var)`, same names, sorted.
    ema_pairs: Vec<(Var, Var)>,
    steps: usize,
    /// Planned optimiser steps (for the `τ` schedule; 0 = constant `ema_tau`).
    total_steps: usize,
    /// Draws the per-step sliced directions.
    dir_rng: SplitMix64,
    /// Fixed directions for [`JepaTrainer::evaluate_loss`] (comparable across calls).
    eval_dirs: Tensor,
    device: Device,
}

impl std::fmt::Debug for JepaTrainer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JepaTrainer").field("steps", &self.steps).finish_non_exhaustive()
    }
}

impl JepaTrainer {
    /// Build both encoders from the same seed (so `ξ = θ` at step 0) and a fresh predictor.
    pub fn new(cfg: JepaTrainConfig) -> CandleResult<Self> {
        if cfg.batch_size < 2 || cfg.lr <= 0.0 {
            return invalid("jepa training: batch_size must be >= 2 and lr > 0");
        }
        let weights = [cfg.prediction_weight, cfg.iso_weight, cfg.offdiag_weight, cfg.slice_weight, cfg.multistep_weight, cfg.vic_var_weight, cfg.vic_cov_weight];
        if !(0.0..=1.0).contains(&cfg.ema_tau) || !(0.0..=1.0).contains(&cfg.ema_tau_start.unwrap_or(cfg.ema_tau)) || weights.iter().any(|w| *w < 0.0) || cfg.slice_projections == 0 {
            return invalid("jepa training: ema_tau must be in [0, 1], weights >= 0");
        }
        let device = cfg.device.resolve()?;
        let online = VitNet::build(&cfg.encoder, cfg.seed, &device)?;
        let target = VitNet::build(&cfg.encoder, cfg.seed, &device)?;
        if cfg.pos_embed_std <= 0.0 {
            return invalid("jepa training: pos_embed_std must be > 0");
        }
        if cfg.pos_embed_std != VIT_POS_EMBED_STD {
            for net in [&online, &target] {
                let pos = net.pos.affine(cfg.pos_embed_std / VIT_POS_EMBED_STD, 0.0)?;
                set_var(&net.varmap, "pos_embed", &pos)?;
            }
        }
        let pred = AdaLnPredictor::new_random_on(cfg.predictor, cfg.seed ^ 0xADA1_17E5, &device)?
            .net
            .ok_or_else(|| CandleModelError::Invalid("adaln build returned no net".into()))?;
        let on = trainable(&online.varmap)?;
        let ema_pairs = trainable(&target.varmap)?
            .into_iter()
            .zip(on.iter())
            .map(|((tn, tv), (on_name, ov))| {
                debug_assert_eq!(&tn, on_name);
                (tv, ov.clone())
            })
            .collect();
        let mut vars: Vec<Var> = on.into_iter().map(|(_, v)| v).collect();
        vars.extend(trainable(&pred.varmap)?.into_iter().map(|(_, v)| v));
        let opt = AdamW::new(vars, ParamsAdamW { lr: cfg.lr, weight_decay: cfg.weight_decay, ..ParamsAdamW::default() })?;
        let dir_rng = SplitMix64::new(cfg.seed ^ 0x51_1CED);
        let eval_dirs = random_directions(&mut SplitMix64::new(cfg.seed ^ 0xE7A1), LATENT_DIM, cfg.slice_projections, &device)?;
        Ok(Self { cfg, online, target, pred, opt, ema_pairs, steps: 0, total_steps: 0, dir_rng, eval_dirs, device })
    }

    /// Optimiser steps taken so far.
    pub fn steps(&self) -> usize {
        self.steps
    }

    /// Plan `n` optimiser steps for the cosine `τ` schedule.
    pub fn set_total_steps(&mut self, n: usize) {
        self.total_steps = n;
    }

    /// EMA rate used after the current step: cosine from `ema_tau_start` (step 0) to
    /// `ema_tau` (last planned step), `τ_s = τ_end − (τ_end − τ_0)·(1 + cos(π s / S)) / 2`.
    pub fn current_tau(&self) -> f64 {
        let t1 = self.cfg.ema_tau;
        let Some(t0) = self.cfg.ema_tau_start else { return t1 };
        if self.total_steps <= 1 {
            return t1;
        }
        let frac = (self.steps as f64 / (self.total_steps - 1) as f64).min(1.0);
        t1 - (t1 - t0) * (1.0 + (std::f64::consts::PI * frac).cos()) / 2.0
    }

    /// `(λ · reg, reg)` with `reg = R + η · sliced + vic_var · hinge + vic_cov · cov`.
    fn iso_terms(&self, z: &Tensor, dirs: &Tensor) -> candle_core::Result<(Tensor, Tensor)> {
        let c = &self.cfg;
        let mut reg = isotropy_penalty(z, c.offdiag_weight)?;
        if c.slice_weight > 0.0 {
            let sliced = match c.slice_test {
                SliceTest::Std => sliced_penalty(z, dirs)?,
                SliceTest::EppsPulley => epps_pulley_penalty(z, dirs)?,
            };
            reg = (reg + (sliced * c.slice_weight)?)?;
        }
        if c.vic_var_weight > 0.0 || c.vic_cov_weight > 0.0 {
            let (hinge, cov) = vicreg_terms(z, c.vic_correlation)?;
            reg = ((reg + (hinge * c.vic_var_weight)?)? + (cov * c.vic_cov_weight)?)?;
        }
        Ok(((&reg * c.iso_weight)?, reg))
    }

    /// `(total, prediction, prediction2, reg)`. `next[i]` is `batch[i]`'s successor in the
    /// same session (empty slice = no multi-step term).
    pub(crate) fn losses(&self, batch: &[&FrameTransition], next: &[Option<&FrameTransition>], train: bool, dirs: &Tensor) -> CandleResult<(Tensor, Tensor, Tensor, Tensor)> {
        if batch.len() < 2 {
            return invalid(format!("jepa batch needs >= 2 transitions, got {}", batch.len()));
        }
        let enc = &self.cfg.encoder;
        let ft: Vec<&[u8]> = batch.iter().map(|t| t.frame_t.as_slice()).collect();
        let ftp1: Vec<&[u8]> = batch.iter().map(|t| t.frame_tp1.as_slice()).collect();
        let z = self.online.forward(enc, &ft, train)?;
        let target = self.target.forward(enc, &ftp1, false)?.detach();
        let a = stack_rows(batch.iter().map(|t| t.action.code), batch.len(), &self.device)?;
        let zhat = self.pred.forward(&z, &a)?;
        let pred = (&zhat - target)?.sqr()?.mean_all()?;
        let (reg_w, reg) = self.iso_terms(&z, dirs)?;
        let mut total = ((&pred * self.cfg.prediction_weight)? + reg_w)?;
        let mut pred2 = Tensor::new(0.0f32, &self.device)?;
        let idx: Vec<u32> = (0..batch.len()).filter(|&i| matches!(next.get(i), Some(Some(_)))).map(|i| i as u32).collect();
        if self.cfg.multistep_weight > 0.0 && !idx.is_empty() {
            let succ: Vec<&FrameTransition> = idx.iter().filter_map(|&i| next[i as usize]).collect();
            let sel = Tensor::from_vec(idx.clone(), idx.len(), &self.device)?;
            let a2 = stack_rows(succ.iter().map(|t| t.action.code), succ.len(), &self.device)?;
            let ftp2: Vec<&[u8]> = succ.iter().map(|t| t.frame_tp1.as_slice()).collect();
            let target2 = self.target.forward(enc, &ftp2, false)?.detach();
            let zhat2 = self.pred.forward(&zhat.index_select(&sel, 0)?, &a2)?;
            pred2 = (zhat2 - target2)?.sqr()?.mean_all()?;
            total = (total + (&pred2 * self.cfg.multistep_weight)?)?;
        }
        Ok((total, pred, pred2, reg))
    }

    /// One AdamW step on `batch` followed by the EMA update. Returns the pre-step loss.
    pub fn step(&mut self, batch: &[&FrameTransition]) -> CandleResult<JepaLoss> {
        self.step_multi(batch, &[])
    }

    /// [`Self::step`] with each transition's same-session successor (for the multi-step term).
    pub fn step_multi(&mut self, batch: &[&FrameTransition], next: &[Option<&FrameTransition>]) -> CandleResult<JepaLoss> {
        let dirs = random_directions(&mut self.dir_rng, LATENT_DIM, self.cfg.slice_projections, &self.device)?;
        let (total, pred, pred2, iso) = self.losses(batch, next, true, &dirs)?;
        self.opt.backward_step(&total)?;
        self.ema_update(self.current_tau())?;
        self.steps += 1;
        Ok(JepaLoss { total: total.to_scalar()?, prediction: pred.to_scalar()?, isotropy: iso.to_scalar()?, prediction2: pred2.to_scalar()? })
    }

    fn ema_update(&self, tau: f64) -> CandleResult<()> {
        for (xi, theta) in &self.ema_pairs {
            let mixed = ((xi.as_tensor().detach() * tau)? + (theta.as_tensor().detach() * (1.0 - tau))?)?;
            assign(xi, &mixed)?;
        }
        Ok(())
    }

    /// Loss over all of `data`, no update, inference-mode ops, one-step term only. The
    /// prediction term is the mean over minibatches of `batch_size`; the regulariser is
    /// computed **once over the whole set** (it is a batch statistic, and an unshuffled chunk
    /// from one session would score its own low spread rather than the encoder's).
    pub fn evaluate_loss(&self, data: &[&FrameTransition]) -> CandleResult<JepaLoss> {
        if data.len() < 2 {
            return invalid(format!("jepa evaluate_loss needs >= 2 transitions, got {}", data.len()));
        }
        let enc = &self.cfg.encoder;
        let (mut pred_sum, mut zs) = (0.0f32, Vec::new());
        for chunk in data.chunks(self.cfg.batch_size) {
            let ft: Vec<&[u8]> = chunk.iter().map(|t| t.frame_t.as_slice()).collect();
            let ftp1: Vec<&[u8]> = chunk.iter().map(|t| t.frame_tp1.as_slice()).collect();
            let z = self.online.forward(enc, &ft, false)?;
            let target = self.target.forward(enc, &ftp1, false)?;
            let a = stack_rows(chunk.iter().map(|t| t.action.code), chunk.len(), &self.device)?;
            let err: f32 = (self.pred.forward(&z, &a)? - target)?.sqr()?.mean_all()?.to_scalar()?;
            pred_sum += err * chunk.len() as f32;
            zs.push(z);
        }
        let prediction = pred_sum / data.len() as f32;
        let isotropy: f32 = self.iso_terms(&Tensor::cat(&zs, 0)?, &self.eval_dirs)?.1.to_scalar()?;
        Ok(JepaLoss { total: prediction + self.cfg.iso_weight as f32 * isotropy, prediction, isotropy, prediction2: 0.0 })
    }

    /// The settings this trainer was built with.
    pub fn config(&self) -> &JepaTrainConfig {
        &self.cfg
    }

    /// The online encoder `f_θ` (shares weights with the trainer).
    pub fn encoder(&self) -> CandleVitEncoder {
        CandleVitEncoder { config: self.cfg.encoder, weights_path: None, net: Some(self.online.clone()) }
    }

    /// The EMA target encoder `f_ξ` (shares weights with the trainer).
    pub fn target_encoder(&self) -> CandleVitEncoder {
        CandleVitEncoder { config: self.cfg.encoder, weights_path: None, net: Some(self.target.clone()) }
    }

    /// The predictor `pred_φ` (shares weights with the trainer).
    pub fn predictor(&self) -> AdaLnPredictor {
        AdaLnPredictor { weights_path: None, hidden_dim: self.cfg.predictor.hidden_dim, config: self.cfg.predictor, net: Some(self.pred.clone()) }
    }
}

#[cfg(test)]
#[path = "jepa_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "jepa_bakeoff_tests.rs"]
mod bakeoff_tests;
