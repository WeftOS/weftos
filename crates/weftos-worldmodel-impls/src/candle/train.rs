//! AdamW training of the AdaLN `pred_φ` on [`TrainingSample`]s (plan W5, CPU, synthetic only).
//!
//! Loss: importance-weighted MSE between the predicted and observed residual, both in the
//! normalised space `(z_{t+1} − z_t − delta_mean) / delta_std`. The per-dimension
//! `z_*` / `delta_*` statistics come from the training set, are written into the predictor's
//! `norm_stats.*` tensors and stay fixed (they are not optimised). Minibatches are drawn by a
//! seeded shuffle, so a run is reproducible for a given `seed`.
//!
//! The ViT encoder is **not** trained here. The JEPA path (encoder on frame bytes, EMA target
//! encoder, SIGReg-style isotropy penalty, loss through this predictor) is the next step; the
//! encoder graph already has a differentiable mode for it (`VitNet::forward(.., train = true)`).

use candle_core::{Device, Tensor, Var, D};
use candle_nn::{AdamW, Optimizer, ParamsAdamW};
use weftos_worldmodel_core::{TrainingSample, LATENT_DIM};

use super::adaln::{stack_rows, AdaLnConfig, AdaNet, NORM_STATS};
use super::{invalid, AdaLnPredictor, CandleModelError, CandleResult, SplitMix64, CONFIG_TENSOR};
use crate::training::ImportanceReplayBuffer;

/// Training settings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AdaLnTrainConfig {
    /// Passes over the training set.
    pub epochs: usize,
    /// AdamW learning rate.
    pub lr: f64,
    /// Seed for init and minibatch shuffling.
    pub seed: u64,
    /// Minibatch size.
    pub batch_size: usize,
    /// AdamW decoupled weight decay.
    pub weight_decay: f64,
    /// Architecture.
    pub model: AdaLnConfig,
}

impl Default for AdaLnTrainConfig {
    fn default() -> Self {
        Self { epochs: 20, lr: 1e-3, seed: 0, batch_size: 32, weight_decay: 0.0, model: AdaLnConfig::default() }
    }
}

/// Result of a training run.
#[derive(Debug, Clone)]
pub struct AdaLnTrainReport {
    /// The trained predictor.
    pub predictor: AdaLnPredictor,
    /// Its safetensors checkpoint.
    pub safetensors: Vec<u8>,
    /// Weighted normalised-residual MSE over the training set before the first step.
    pub initial_loss: f32,
    /// Same, after the last step.
    pub final_loss: f32,
    /// Optimiser steps taken.
    pub steps: usize,
}

/// Train the default-architecture AdaLN predictor.
pub fn train_adaln(samples: &[TrainingSample], epochs: usize, lr: f64, seed: u64) -> CandleResult<AdaLnTrainReport> {
    train_adaln_with(samples, AdaLnTrainConfig { epochs, lr, seed, ..AdaLnTrainConfig::default() })
}

/// Per-dimension mean and std (std falls back to 1 for a constant dimension).
fn mean_std(rows: &[[f32; LATENT_DIM]]) -> ([f32; LATENT_DIM], [f32; LATENT_DIM]) {
    let n = rows.len() as f64;
    let (mut mean, mut std) = ([0.0f32; LATENT_DIM], [1.0f32; LATENT_DIM]);
    for d in 0..LATENT_DIM {
        let m = rows.iter().map(|r| r[d] as f64).sum::<f64>() / n;
        let v = rows.iter().map(|r| (r[d] as f64 - m).powi(2)).sum::<f64>() / n;
        mean[d] = m as f32;
        if v.sqrt() > 1e-8 {
            std[d] = v.sqrt() as f32;
        }
    }
    (mean, std)
}

struct Data {
    z: Tensor,
    a: Tensor,
    target: Tensor,
    w: Tensor,
}

fn weighted_loss(net: &AdaNet, z: &Tensor, a: &Tensor, target: &Tensor, w: &Tensor) -> candle_core::Result<Tensor> {
    let err = (net.forward_delta_norm(z, a)? - target)?.sqr()?.mean_keepdim(D::Minus1)?;
    err.broadcast_mul(w)?.mean_all()
}

/// Train an AdaLN predictor with explicit settings.
pub fn train_adaln_with(samples: &[TrainingSample], cfg: AdaLnTrainConfig) -> CandleResult<AdaLnTrainReport> {
    let used: Vec<&TrainingSample> = samples.iter().filter(|s| ImportanceReplayBuffer::weight(s) > 0.0).collect();
    if used.len() < 2 {
        return invalid(format!("adaln training needs at least 2 weighted samples, got {}", used.len()));
    }
    if cfg.batch_size == 0 || cfg.lr <= 0.0 {
        return invalid("adaln training: batch_size and lr must be > 0");
    }
    let n = used.len();
    let zs: Vec<[f32; LATENT_DIM]> = used.iter().map(|s| s.z_t).collect();
    let deltas: Vec<[f32; LATENT_DIM]> = used.iter().map(|s| s.residual()).collect();
    let (z_mean, z_std) = mean_std(&zs);
    let (d_mean, d_std) = mean_std(&deltas);

    let net = AdaNet::build(&cfg.model, cfg.seed)?;
    net.set_stats([&z_mean, &z_std, &d_mean, &d_std])?;

    let targets = deltas.iter().map(|d| {
        let mut t = [0.0f32; LATENT_DIM];
        for k in 0..LATENT_DIM {
            t[k] = (d[k] - d_mean[k]) / d_std[k];
        }
        t
    });
    let w_raw: Vec<f32> = used.iter().map(|s| ImportanceReplayBuffer::weight(s)).collect();
    let w_mean = w_raw.iter().sum::<f32>() / n as f32;
    let data = Data {
        z: stack_rows(zs.iter().copied(), n)?,
        a: stack_rows(used.iter().map(|s| s.action.code), n)?,
        target: stack_rows(targets, n)?,
        w: Tensor::from_vec(w_raw.iter().map(|w| w / w_mean).collect::<Vec<_>>(), (n, 1), &Device::Cpu)?,
    };

    let vars: Vec<Var> = {
        let map = net.varmap.data().lock().map_err(|_| CandleModelError::Invalid("varmap lock poisoned".into()))?;
        let mut named: Vec<(&String, &Var)> =
            map.iter().filter(|(k, _)| k.as_str() != CONFIG_TENSOR && !NORM_STATS.contains(&k.as_str())).collect();
        named.sort_by(|a, b| a.0.cmp(b.0));
        named.into_iter().map(|(_, v)| v.clone()).collect()
    };
    let mut opt = AdamW::new(vars, ParamsAdamW { lr: cfg.lr, weight_decay: cfg.weight_decay, ..ParamsAdamW::default() })?;

    let full = |net: &AdaNet| -> CandleResult<f32> {
        Ok(weighted_loss(net, &data.z, &data.a, &data.target, &data.w)?.to_scalar::<f32>()?)
    };
    let initial_loss = full(&net)?;
    let mut rng = SplitMix64::new(cfg.seed ^ 0x5EED);
    let mut order: Vec<u32> = (0..n as u32).collect();
    let mut steps = 0usize;
    for _ in 0..cfg.epochs {
        for i in (1..n).rev() {
            order.swap(i, (rng.next_u64() % (i as u64 + 1)) as usize);
        }
        for chunk in order.chunks(cfg.batch_size) {
            let idx = Tensor::from_slice(chunk, chunk.len(), &Device::Cpu)?;
            let pick = |t: &Tensor| t.index_select(&idx, 0);
            let loss = weighted_loss(&net, &pick(&data.z)?, &pick(&data.a)?, &pick(&data.target)?, &pick(&data.w)?)?;
            opt.backward_step(&loss)?;
            steps += 1;
        }
    }
    let final_loss = full(&net)?;
    let predictor = AdaLnPredictor { weights_path: None, hidden_dim: cfg.model.hidden_dim, config: cfg.model, net: Some(net) };
    let safetensors = predictor.to_safetensors_bytes()?;
    Ok(AdaLnTrainReport { predictor, safetensors, initial_loss, final_loss, steps })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::candle::adaln;
    use crate::training::ActionLinearModel;
    use weftos_worldmodel_core::{Predictor, SensorClass};

    fn fixture() -> Vec<TrainingSample> {
        // Same generator as the default-path fixture: z' = z + A*·a + b* + noise.
        crate::training::action_linear::tests::action_fixture(800, SensorClass::RgbD, 0.001, 31).0
    }

    #[test]
    fn adamw_reduces_the_loss_and_the_blob_reloads_identically() {
        let data = fixture();
        let cfg = AdaLnTrainConfig { epochs: 15, lr: 3e-3, seed: 1, batch_size: 32, model: adaln::tests::small(), ..AdaLnTrainConfig::default() };
        let r = train_adaln_with(&data[..640], cfg).unwrap();
        assert_eq!(r.steps, 15 * 20);
        assert!(r.final_loss < 0.3 * r.initial_loss, "loss {} -> {}", r.initial_loss, r.final_loss);

        // Held out: beats the residual-mean model (what adaLN-zero init predicts).
        let held = &data[640..];
        let mse = |f: &dyn Fn(&TrainingSample) -> [f32; LATENT_DIM]| {
            held.iter().map(|s| f(s).iter().zip(s.z_tp1.iter()).map(|(p, y)| (p - y).powi(2)).sum::<f32>()).sum::<f32>() / held.len() as f32
        };
        let e_adaln = mse(&|s| r.predictor.predict(&s.z_t, &s.action).unwrap());
        let mut mean = [0.0f32; LATENT_DIM];
        for s in &data[..640] {
            for (m, d) in mean.iter_mut().zip(s.residual().iter()) {
                *m += d / 640.0;
            }
        }
        let rm = ActionLinearModel::from_residual(mean);
        let e_mean = mse(&|s| rm.predict(&s.z_t, &s.action));
        eprintln!("train loss {} -> {}; held-out adaln {e_adaln} vs residual-mean {e_mean}", r.initial_loss, r.final_loss);
        assert!(e_adaln < 0.5 * e_mean, "held-out adaln {e_adaln} vs residual-mean {e_mean}");

        let back = AdaLnPredictor::from_safetensors_bytes(&r.safetensors).unwrap();
        let s = &held[0];
        assert_eq!(back.predict(&s.z_t, &s.action).unwrap(), r.predictor.predict(&s.z_t, &s.action).unwrap());
    }

    #[test]
    fn too_few_samples_or_bad_settings_are_refused() {
        let data = fixture();
        assert!(train_adaln(&data[..1], 1, 1e-3, 0).is_err());
        let cfg = AdaLnTrainConfig { batch_size: 0, model: adaln::tests::small(), ..AdaLnTrainConfig::default() };
        assert!(train_adaln_with(&data, cfg).is_err());
    }
}
