//! [`train_jepa`]: the JEPA training run, the collapse checks and the eval-harness report.
//! See [`crate::candle::jepa`] for the loss.

use weftos_worldmodel_core::{Latent, Predictor};

use super::jepa::{FrameTransition, JepaLoss, JepaTrainConfig, JepaTrainer};
use super::jepa_diag::{diagnose, relative_gap, EpochDiagnostics, LatentDiagnostics};
use super::{invalid, AdaLnPredictor, CandleResult, CandleVitEncoder, SplitMix64};
use crate::candle::isotropy::participation_ratio;
use crate::eval_harness::{evaluate_by_day, EvalConfig, EvalReport, Transition};
use crate::sigreg::WelfordSigRegMonitor;

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
    /// Per-epoch collapse trace; empty unless [`JepaTrainConfig::diagnostics`].
    pub epoch_diagnostics: Vec<EpochDiagnostics>,
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
    /// Diagnostics of the final train-split `f_θ(x_t)` latents.
    pub train_diagnostics: LatentDiagnostics,
    /// Diagnostics of the final held-out `f_θ(x_t)` latents.
    pub held_out_diagnostics: LatentDiagnostics,
    /// Eval harness on `(f_θ(x_t), a, f_θ(x_{t+1}))`, held out by day (the deployed pairing:
    /// one encoder for both ends).
    pub eval: EvalReport,
    /// Eval harness on `(f_θ(x_t), a, f_ξ(x_{t+1}))`: the pairing training optimises. Its
    /// SIGReg and collapse checks see a mix of online and target latents.
    pub eval_target: EvalReport,
}

/// Encode `frames` in chunks of 32 with the inference graph.
pub fn encode_all(encoder: &CandleVitEncoder, frames: &[&[u8]]) -> CandleResult<Vec<Latent>> {
    let mut out = Vec::with_capacity(frames.len());
    for chunk in frames.chunks(32) {
        out.extend(encoder.encode_batch(chunk)?);
    }
    Ok(out)
}

/// Same-session successor of each transition: `data[i + 1]` when it shares day and session
/// and starts on the frame `data[i]` ends on.
fn successors(data: &[FrameTransition]) -> Vec<Option<usize>> {
    (0..data.len())
        .map(|i| {
            let (a, b) = (&data[i], data.get(i + 1)?);
            (a.day == b.day && a.session == b.session && a.frame_tp1 == b.frame_t).then_some(i + 1)
        })
        .collect()
}

fn epoch_diagnostics(tr: &JepaTrainer, train: &[&FrameTransition], step_mean: &JepaLoss) -> CandleResult<EpochDiagnostics> {
    let frames: Vec<&[u8]> = train.iter().map(|t| t.frame_t.as_slice()).collect();
    let tags: Vec<(u16, u16)> = train.iter().map(|t| (t.day, t.session)).collect();
    let on = encode_all(&tr.encoder(), &frames)?;
    let tg = encode_all(&tr.target_encoder(), &frames)?;
    Ok(EpochDiagnostics {
        online: diagnose(&on, &tags),
        target_participation_ratio: participation_ratio(&tg),
        target_gap: relative_gap(&tg, &on),
        prediction_share: if step_mean.total > 0.0 { step_mean.prediction / step_mean.total } else { f32::NAN },
    })
}

fn harness(data: &[FrameTransition], z_t: &[Latent], z_tp1: &[Latent], predictor: &AdaLnPredictor, cfg: &JepaTrainConfig, n_held_days: usize) -> EvalReport {
    let transitions: Vec<Transition> = data
        .iter()
        .zip(z_t.iter().zip(z_tp1.iter()))
        .map(|(t, (a, b))| Transition { z_t: *a, action: t.action_scalar, z_tp1: *b })
        .collect();
    let tags: Vec<u16> = data.iter().map(|t| t.day).collect();
    let to_code = cfg.action_from_scalar;
    evaluate_by_day(&transitions, &tags, |z, a| predictor.predict(z, &to_code(a)).unwrap_or(*z), EvalConfig { holdout_days: n_held_days, ..cfg.eval })
}

/// Train encoder + predictor on `data` with the latest `cfg.holdout_days` days held out, then
/// run the collapse checks and the eval harness (see the module docs of `jepa`).
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
    let train_idx: Vec<usize> = (0..data.len()).filter(|&i| data[i].day < first_held).collect();
    let train: Vec<&FrameTransition> = train_idx.iter().map(|&i| &data[i]).collect();
    let held: Vec<&FrameTransition> = data.iter().filter(|t| t.day >= first_held).collect();
    if train.len() < cfg.batch_size.max(2) || held.len() < 9 {
        return invalid(format!("jepa training: {} train / {} held-out transitions is too few", train.len(), held.len()));
    }
    let succ = successors(data);

    let mut trainer = JepaTrainer::new(cfg)?;
    let per_epoch = train.chunks(cfg.batch_size).filter(|c| c.len() >= 2).count();
    trainer.set_total_steps(per_epoch * cfg.epochs);
    let initial_loss = trainer.evaluate_loss(&train)?;
    let mut rng = SplitMix64::new(cfg.seed ^ 0x0E9A_5EED);
    let mut order: Vec<usize> = (0..train.len()).collect();
    let (mut epoch_losses, mut epoch_diag) = (Vec::with_capacity(cfg.epochs), Vec::new());
    for _ in 0..cfg.epochs {
        for i in (1..order.len()).rev() {
            order.swap(i, (rng.next_u64() % (i as u64 + 1)) as usize);
        }
        let (mut acc, mut k) = (JepaLoss::default(), 0.0f32);
        for chunk in order.chunks(cfg.batch_size).filter(|c| c.len() >= 2) {
            let batch: Vec<&FrameTransition> = chunk.iter().map(|&i| train[i]).collect();
            let next: Vec<Option<&FrameTransition>> = chunk.iter().map(|&i| succ[train_idx[i]].map(|j| &data[j])).collect();
            let l = trainer.step_multi(&batch, &next)?;
            acc.total += l.total;
            acc.prediction += l.prediction;
            acc.isotropy += l.isotropy;
            acc.prediction2 += l.prediction2;
            k += 1.0;
        }
        let mean = JepaLoss { total: acc.total / k, prediction: acc.prediction / k, isotropy: acc.isotropy / k, prediction2: acc.prediction2 / k };
        if cfg.diagnostics {
            epoch_diag.push(epoch_diagnostics(&trainer, &train, &mean)?);
        }
        epoch_losses.push(mean);
    }
    let final_loss = trainer.evaluate_loss(&train)?;
    let held_out_loss = trainer.evaluate_loss(&held)?;
    let (encoder, predictor, target) = (trainer.encoder(), trainer.predictor(), trainer.target_encoder());
    let steps = trainer.steps();
    drop(trainer);

    let z_t = encode_all(&encoder, &data.iter().map(|t| t.frame_t.as_slice()).collect::<Vec<_>>())?;
    let tp1: Vec<&[u8]> = data.iter().map(|t| t.frame_tp1.as_slice()).collect();
    let z_tp1 = encode_all(&encoder, &tp1)?;
    let z_tp1_target = encode_all(&target, &tp1)?;
    let split = |held: bool| -> (Vec<Latent>, Vec<(u16, u16)>) {
        data.iter().zip(z_t.iter()).filter(|(t, _)| (t.day >= first_held) == held).map(|(t, z)| (*z, (t.day, t.session))).unzip()
    };
    let (held_z, held_tags) = split(true);
    let (train_z, train_tags) = split(false);
    let mut monitor = WelfordSigRegMonitor::new(1);
    let mut held_out_sigreg_health = 1.0f32;
    for z in &held_z {
        // Constant clock: offline pass, keep the online rollback timer out of the statistics.
        if let Ok(h) = monitor.update_at(z, 0) {
            held_out_sigreg_health = h.score;
        }
    }
    let eval = harness(data, &z_t, &z_tp1, &predictor, &cfg, n_held_days);
    let eval_target = harness(data, &z_t, &z_tp1_target, &predictor, &cfg, n_held_days);
    let (train_diagnostics, held_out_diagnostics) = (diagnose(&train_z, &train_tags), diagnose(&held_z, &held_tags));

    Ok(JepaTrainReport {
        encoder_safetensors: encoder.to_safetensors_bytes()?,
        predictor_safetensors: predictor.to_safetensors_bytes()?,
        encoder,
        predictor,
        initial_loss,
        final_loss,
        held_out_loss,
        epoch_losses,
        epoch_diagnostics: epoch_diag,
        steps,
        n_train: train.len(),
        n_held_out: held.len(),
        held_out_days,
        held_out_sigreg_health,
        held_out_participation_ratio: held_out_diagnostics.participation_ratio,
        train_participation_ratio: train_diagnostics.participation_ratio,
        train_diagnostics,
        held_out_diagnostics,
        eval,
        eval_target,
    })
}
