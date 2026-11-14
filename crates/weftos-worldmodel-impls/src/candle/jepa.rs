//! JEPA self-supervised training of the ViT encoder with the AdaLN `pred_φ` (plan W5, CPU).
//!
//! ```text
//! z_t    = f_θ(x_t)                       online ViT, differentiable (train mode)
//! z*_tp1 = stopgrad f_ξ(x_{t+1})          EMA target ViT, same architecture, no grad
//! ẑ_tp1  = pred_φ(z_t, a_t)               AdaLN predictor (adaLN-zero init: copy-last)
//! L      = MSE(ẑ_tp1, z*_tp1) + λ · ( R(z_t) + η · S(z_t) )
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
//! where [`WelfordSigRegMonitor`] reports health 1: the monitor's energy is
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
//! batch has unit variance along every direction; a rank-one batch does not. **Open issue:**
//! on the default ViT-tiny the sliced term does not prevent rank-one latents under the full
//! JEPA loss (960 steps, `η = 1`, and 288 steps at `λ = 10`: participation ratio 1.00, sliced
//! term parked at ≈ 0.4, the rank-one value `2 − 2·E|g|`), although optimising the encoder on
//! `R + S` alone raises the ratio (2.5 → 4.5 in 300 steps on the tiny config). The tiny
//! test config does not collapse (ratio ≈ 2).
//!
//! `γ > 0` adds VICReg-style off-diagonal decorrelation instead. Only use it when the batch is
//! larger than `d`: below that `C` has rank `< d`, so the off-diagonal term can only fall by
//! pulling the diagonal under 1, which the monitor scores as unhealthy. The default batch is
//! 16. The report's participation ratio `(tr C)² / ‖C‖_F²` on train and held-out latents is
//! the dimensional-collapse check.
//!
//! # Collapse checks
//!
//! After training, [`train_jepa`] encodes every transition with the trained online encoder
//! (`z_t = f_θ(x_t)`, `z_tp1 = f_θ(x_tp1)`), runs [`evaluate_by_day`] with the trained AdaLN
//! as the model, and runs a fresh [`WelfordSigRegMonitor`] over the held-out latents. The
//! harness predicts `f_θ(x_tp1)` while training targets `f_ξ(x_tp1)`; the two agree only once
//! the target has caught up (`τ^steps` small), so short runs should use a smaller `τ`.

use candle_core::{Tensor, Var};
use candle_nn::{AdamW, Optimizer, ParamsAdamW, VarMap};
use weftos_worldmodel_core::{Action, Latent, Predictor, LATENT_DIM};

use super::adaln::{stack_rows, AdaLnConfig, AdaNet, NORM_STATS};
use super::vit::{VitNet, VitTinyConfig};
use super::{invalid, synthetic, AdaLnPredictor, CandleModelError, CandleResult, CandleVitEncoder, SplitMix64, CONFIG_TENSOR};
pub(crate) use super::isotropy::random_directions;
pub use super::isotropy::{isotropy_penalty, participation_ratio, sliced_penalty, ISO_STD_EPS};
use crate::eval_harness::{evaluate_by_day, EvalConfig, EvalReport, Transition};
use crate::sigreg::WelfordSigRegMonitor;

/// One observed step on frame bytes (any source; [`synthetic::generate`] makes test ones).
#[derive(Debug, Clone, PartialEq)]
pub struct FrameTransition {
    /// Frame at `t` (HWC RGB u8, the encoder's layout).
    pub frame_t: Vec<u8>,
    /// Action code fed to `pred_φ`.
    pub action: Action,
    /// Scalar action for the eval harness ([`Transition::action`]).
    pub action_scalar: f32,
    /// Frame at `t + 1`.
    pub frame_tp1: Vec<u8>,
    /// Day tag (the held-out split is by day).
    pub day: u16,
    /// Session tag within the day.
    pub session: u16,
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
    /// Weight `λ` of the isotropy penalty.
    pub iso_weight: f64,
    /// Weight `γ` of the off-diagonal covariance term (0 = diagonal variant).
    pub offdiag_weight: f64,
    /// Weight `η` of the sliced term (random-direction std penalty, see the module docs).
    pub slice_weight: f64,
    /// Random unit directions `K` drawn per step for the sliced term.
    pub slice_projections: usize,
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
}

impl Default for JepaTrainConfig {
    fn default() -> Self {
        Self {
            epochs: 10,
            lr: 1e-3,
            weight_decay: 0.01,
            batch_size: 16,
            seed: 0,
            ema_tau: 0.996,
            iso_weight: 1.0,
            offdiag_weight: 0.0,
            slice_weight: 1.0,
            slice_projections: 64,
            holdout_days: 1,
            encoder: VitTinyConfig::default(),
            predictor: AdaLnConfig::default(),
            action_from_scalar: synthetic::action_code,
            eval: EvalConfig::default(),
        }
    }
}

/// Loss terms (means over the transitions they were measured on).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct JepaLoss {
    /// `prediction + λ · isotropy`.
    pub total: f32,
    /// MSE between `pred_φ(f_θ(x_t), a)` and `f_ξ(x_{t+1})`.
    pub prediction: f32,
    /// Regulariser `R(f_θ(x_t)) + η · sliced(f_θ(x_t))` (std forms, see the module docs).
    pub isotropy: f32,
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
    /// Draws the per-step sliced directions.
    dir_rng: SplitMix64,
    /// Fixed directions for [`JepaTrainer::evaluate_loss`] (comparable across calls).
    eval_dirs: Tensor,
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
        if !(0.0..=1.0).contains(&cfg.ema_tau) || cfg.iso_weight < 0.0 || cfg.offdiag_weight < 0.0 || cfg.slice_weight < 0.0 || cfg.slice_projections == 0 {
            return invalid("jepa training: ema_tau must be in [0, 1], weights >= 0");
        }
        let online = VitNet::build(&cfg.encoder, cfg.seed)?;
        let target = VitNet::build(&cfg.encoder, cfg.seed)?;
        let pred = AdaLnPredictor::new_random_with(cfg.predictor, cfg.seed ^ 0xADA1_17E5)?
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
        let eval_dirs = random_directions(&mut SplitMix64::new(cfg.seed ^ 0xE7A1), LATENT_DIM, cfg.slice_projections)?;
        Ok(Self { cfg, online, target, pred, opt, ema_pairs, steps: 0, dir_rng, eval_dirs })
    }

    /// Optimiser steps taken so far.
    pub fn steps(&self) -> usize {
        self.steps
    }

    /// `λ · (R + η · sliced)` for a batch of online latents.
    fn iso_terms(&self, z: &Tensor, dirs: &Tensor) -> candle_core::Result<(Tensor, Tensor)> {
        let iso = isotropy_penalty(z, self.cfg.offdiag_weight)?;
        let sliced = sliced_penalty(z, dirs)?;
        let reg = ((&iso + (&sliced * self.cfg.slice_weight)?)? * self.cfg.iso_weight)?;
        Ok((reg, iso.add(&(sliced * self.cfg.slice_weight)?)?))
    }

    fn losses(&self, batch: &[&FrameTransition], train: bool, dirs: &Tensor) -> CandleResult<(Tensor, Tensor, Tensor)> {
        if batch.len() < 2 {
            return invalid(format!("jepa batch needs >= 2 transitions, got {}", batch.len()));
        }
        let enc = &self.cfg.encoder;
        let ft: Vec<&[u8]> = batch.iter().map(|t| t.frame_t.as_slice()).collect();
        let ftp1: Vec<&[u8]> = batch.iter().map(|t| t.frame_tp1.as_slice()).collect();
        let z = self.online.forward(enc, &ft, train)?;
        let target = self.target.forward(enc, &ftp1, false)?.detach();
        let a = stack_rows(batch.iter().map(|t| t.action.code), batch.len())?;
        let pred = (self.pred.forward(&z, &a)? - target)?.sqr()?.mean_all()?;
        let (reg, iso) = self.iso_terms(&z, dirs)?;
        let total = (&pred + reg)?;
        Ok((total, pred, iso))
    }

    /// One AdamW step on `batch` followed by the EMA update. Returns the pre-step loss.
    pub fn step(&mut self, batch: &[&FrameTransition]) -> CandleResult<JepaLoss> {
        let dirs = random_directions(&mut self.dir_rng, LATENT_DIM, self.cfg.slice_projections)?;
        let (total, pred, iso) = self.losses(batch, true, &dirs)?;
        self.opt.backward_step(&total)?;
        self.ema_update()?;
        self.steps += 1;
        Ok(JepaLoss { total: total.to_scalar()?, prediction: pred.to_scalar()?, isotropy: iso.to_scalar()? })
    }

    fn ema_update(&self) -> CandleResult<()> {
        let tau = self.cfg.ema_tau;
        for (xi, theta) in &self.ema_pairs {
            let mixed = ((xi.as_tensor().detach() * tau)? + (theta.as_tensor().detach() * (1.0 - tau))?)?;
            xi.set(&mixed)?;
        }
        Ok(())
    }

    /// Loss over all of `data`, no update, inference-mode ops. The prediction term is the
    /// mean over minibatches of `batch_size`; the isotropy term is computed **once over the
    /// whole set** (it is a batch statistic, and an unshuffled chunk from one session would
    /// score its own low spread rather than the encoder's).
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
            let a = stack_rows(chunk.iter().map(|t| t.action.code), chunk.len())?;
            let err: f32 = (self.pred.forward(&z, &a)? - target)?.sqr()?.mean_all()?.to_scalar()?;
            pred_sum += err * chunk.len() as f32;
            zs.push(z);
        }
        let prediction = pred_sum / data.len() as f32;
        let isotropy: f32 = self.iso_terms(&Tensor::cat(&zs, 0)?, &self.eval_dirs)?.1.to_scalar()?;
        Ok(JepaLoss { total: prediction + self.cfg.iso_weight as f32 * isotropy, prediction, isotropy })
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

/// Result of [`train_jepa`]. All numbers are from whatever data was passed in.
#[derive(Debug, Clone)]
pub struct JepaTrainReport {
    /// Trained online encoder `f_θ`.
    pub encoder: CandleVitEncoder,
    /// Trained predictor `pred_φ`.
    pub predictor: AdaLnPredictor,
    /// Encoder checkpoint (arch 1).
    pub encoder_safetensors: Vec<u8>,
    /// Predictor checkpoint (arch 2).
    pub predictor_safetensors: Vec<u8>,
    /// Train-split loss before the first step ([`JepaTrainer::evaluate_loss`]).
    pub initial_loss: JepaLoss,
    /// Train-split loss after the last step.
    pub final_loss: JepaLoss,
    /// Held-out-day loss after the last step.
    pub held_out_loss: JepaLoss,
    /// Mean step loss per epoch (pre-step losses, train mode, isotropy per shuffled batch).
    pub epoch_losses: Vec<JepaLoss>,
    /// Optimiser steps.
    pub steps: usize,
    /// Train transitions.
    pub n_train: usize,
    /// Held-out transitions.
    pub n_held_out: usize,
    /// Held-out day ids, ascending.
    pub held_out_days: Vec<u16>,
    /// [`WelfordSigRegMonitor`] health after the held-out `f_θ(x_t)` latents.
    pub held_out_sigreg_health: f32,
    /// Participation ratio `(tr C)² / ‖C‖_F²` of the held-out latents (1 = rank-one collapse).
    pub held_out_participation_ratio: f32,
    /// Same, on the train-split latents.
    pub train_participation_ratio: f32,
    /// Eval harness on the encoded transitions, held out by day.
    pub eval: EvalReport,
}

/// Encode `frames` in chunks of 32 with the inference graph.
pub fn encode_all(encoder: &CandleVitEncoder, frames: &[&[u8]]) -> CandleResult<Vec<Latent>> {
    let mut out = Vec::with_capacity(frames.len());
    for chunk in frames.chunks(32) {
        out.extend(encoder.encode_batch(chunk)?);
    }
    Ok(out)
}

/// Train encoder + predictor on `data` with the latest `cfg.holdout_days` days held out, then
/// run the collapse checks and the eval harness (see the module docs).
pub fn train_jepa(data: &[FrameTransition], cfg: JepaTrainConfig) -> CandleResult<JepaTrainReport> {
    let mut days: Vec<u16> = data.iter().map(|t| t.day).collect();
    days.sort_unstable();
    days.dedup();
    let n_held_days = cfg.holdout_days.max(1);
    if days.len() < n_held_days + 1 {
        return invalid(format!("jepa training needs > {n_held_days} distinct days, got {}", days.len()));
    }
    let held_out_days = days[days.len() - n_held_days..].to_vec();
    let first_held = held_out_days[0];
    let (train, held): (Vec<&FrameTransition>, Vec<&FrameTransition>) = data.iter().partition(|t| t.day < first_held);
    if train.len() < cfg.batch_size.max(2) || held.len() < 9 {
        return invalid(format!("jepa training: {} train / {} held-out transitions is too few", train.len(), held.len()));
    }

    let mut trainer = JepaTrainer::new(cfg)?;
    let initial_loss = trainer.evaluate_loss(&train)?;
    let mut rng = SplitMix64::new(cfg.seed ^ 0x0E9A_5EED);
    let mut order: Vec<usize> = (0..train.len()).collect();
    let mut epoch_losses = Vec::with_capacity(cfg.epochs);
    for _ in 0..cfg.epochs {
        for i in (1..order.len()).rev() {
            order.swap(i, (rng.next_u64() % (i as u64 + 1)) as usize);
        }
        let (mut acc, mut k) = (JepaLoss::default(), 0.0f32);
        for chunk in order.chunks(cfg.batch_size).filter(|c| c.len() >= 2) {
            let batch: Vec<&FrameTransition> = chunk.iter().map(|&i| train[i]).collect();
            let l = trainer.step(&batch)?;
            acc.total += l.total;
            acc.prediction += l.prediction;
            acc.isotropy += l.isotropy;
            k += 1.0;
        }
        epoch_losses.push(JepaLoss { total: acc.total / k, prediction: acc.prediction / k, isotropy: acc.isotropy / k });
    }
    let final_loss = trainer.evaluate_loss(&train)?;
    let held_out_loss = trainer.evaluate_loss(&held)?;
    let (encoder, predictor) = (trainer.encoder(), trainer.predictor());
    let steps = trainer.steps();
    drop(trainer);

    let z_t = encode_all(&encoder, &data.iter().map(|t| t.frame_t.as_slice()).collect::<Vec<_>>())?;
    let z_tp1 = encode_all(&encoder, &data.iter().map(|t| t.frame_tp1.as_slice()).collect::<Vec<_>>())?;
    let (held_z, train_z): (Vec<(&FrameTransition, &Latent)>, Vec<_>) = data.iter().zip(z_t.iter()).partition(|(t, _)| t.day >= first_held);
    let held_z: Vec<Latent> = held_z.into_iter().map(|(_, z)| *z).collect();
    let train_z: Vec<Latent> = train_z.into_iter().map(|(_, z)| *z).collect();
    let mut monitor = WelfordSigRegMonitor::new(1);
    let mut held_out_sigreg_health = 1.0f32;
    for z in &held_z {
        // Constant clock: offline pass, keep the online rollback timer out of the statistics.
        if let Ok(h) = monitor.update_at(z, 0) {
            held_out_sigreg_health = h.score;
        }
    }
    let transitions: Vec<Transition> = data
        .iter()
        .zip(z_t.iter().zip(z_tp1.iter()))
        .map(|(t, (a, b))| Transition { z_t: *a, action: t.action_scalar, z_tp1: *b })
        .collect();
    let tags: Vec<u16> = data.iter().map(|t| t.day).collect();
    let to_code = cfg.action_from_scalar;
    let eval = evaluate_by_day(
        &transitions,
        &tags,
        |z, a| predictor.predict(z, &to_code(a)).unwrap_or(*z),
        EvalConfig { holdout_days: n_held_days, ..cfg.eval },
    );

    Ok(JepaTrainReport {
        encoder_safetensors: encoder.to_safetensors_bytes()?,
        predictor_safetensors: predictor.to_safetensors_bytes()?,
        encoder,
        predictor,
        initial_loss,
        final_loss,
        held_out_loss,
        epoch_losses,
        steps,
        n_train: train.len(),
        n_held_out: held.len(),
        held_out_days,
        held_out_sigreg_health,
        held_out_participation_ratio: participation_ratio(&held_z),
        train_participation_ratio: participation_ratio(&train_z),
        eval,
    })
}

#[cfg(test)]
#[path = "jepa_tests.rs"]
mod tests;
