//! Metal GPU tests (`--features metal`, macOS with a GPU). Every test builds the same model
//! on CPU and on Metal and compares the results. Data is SYNTHETIC.

use candle_core::Device;
use weftos_worldmodel_core::{Action, Latent, Predictor, LATENT_DIM};

use super::jepa::{train_jepa, FrameTransition, JepaTrainConfig, JepaTrainer};
use super::synthetic::{generate, SyntheticConfig};
use super::{
    candle_default_device, metal_supported, AdaLnConfig, AdaLnPredictor, CandleVitEncoder, ComputeDevice, VitTinyConfig,
};

fn metal() -> Device {
    Device::new_metal(0).expect("the `metal` feature needs a Metal GPU (macOS)")
}

fn small_vit() -> VitTinyConfig {
    VitTinyConfig { image_size: 32, patch_size: 8, embed_dim: 32, depth: 2, num_heads: 2, latent_dim: LATENT_DIM }
}

fn small_adaln() -> AdaLnConfig {
    AdaLnConfig { hidden_dim: 32, depth: 2, ..AdaLnConfig::default() }
}

fn frame(cfg: &VitTinyConfig, seed: u8) -> Vec<u8> {
    (0..cfg.frame_len()).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)).collect()
}

fn max_diff(a: &Latent, b: &Latent) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| (x - y).abs()).fold(0.0, f32::max)
}

fn tiny_data() -> Vec<FrameTransition> {
    generate(&SyntheticConfig { image_size: 32, days: 3, sessions_per_day: 2, steps_per_session: 16, blob_radius: 4.0, action_step_px: 3.0, noise: 6, seed: 11, ..SyntheticConfig::default() })
}

fn tiny_cfg(device: ComputeDevice) -> JepaTrainConfig {
    JepaTrainConfig {
        epochs: 4,
        lr: 1e-2,
        batch_size: 16,
        seed: 3,
        ema_tau: 0.8,
        encoder: small_vit(),
        predictor: small_adaln(),
        device,
        ..JepaTrainConfig::lane_g()
    }
}

#[test]
fn device_selection_reports_metal_and_never_downgrades_silently() {
    assert!(metal_supported());
    assert!(candle_default_device().is_metal());
    assert!(ComputeDevice::Metal.resolve().unwrap().is_metal());
    assert!(ComputeDevice::Auto.resolve().unwrap().is_metal());
    assert!(ComputeDevice::Cpu.resolve().unwrap().is_cpu());
}

#[test]
fn vit_inference_matches_cpu() {
    let cfg = small_vit();
    let cpu = CandleVitEncoder::new_random_on(cfg, 5, &Device::Cpu).unwrap();
    let gpu = CandleVitEncoder::new_random_on(cfg, 5, &metal()).unwrap();
    assert!(gpu.device().unwrap().is_metal());
    let frames: Vec<Vec<u8>> = (0..5).map(|i| frame(&cfg, i * 9)).collect();
    let refs: Vec<&[u8]> = frames.iter().map(Vec::as_slice).collect();
    let (a, b) = (cpu.encode_batch(&refs).unwrap(), gpu.encode_batch(&refs).unwrap());
    let worst = a.iter().zip(b.iter()).map(|(x, y)| max_diff(x, y)).fold(0.0, f32::max);
    assert!(worst < 2e-3, "cpu vs metal encode differ by {worst}");
}

#[test]
fn adaln_inference_matches_cpu() {
    let cfg = small_adaln();
    let cpu = AdaLnPredictor::new_random_on(cfg, 6, &Device::Cpu).unwrap();
    let gpu = AdaLnPredictor::new_random_on(cfg, 6, &metal()).unwrap();
    // adaLN-zero predicts copy-last at init; give the heads non-zero weights so the test sees them.
    for p in [&cpu, &gpu] {
        let net = p.net.as_ref().unwrap();
        for (name, var) in net.varmap.data().lock().unwrap().iter() {
            if name.contains("adaln.") || name.starts_with("final.out.") {
                super::assign(var, &(var.as_tensor().ones_like().unwrap() * 0.05).unwrap()).unwrap();
            }
        }
    }
    let z: Vec<Latent> = (0..4).map(|i| std::array::from_fn(|k| ((k + i) as f32 * 0.07).sin())).collect();
    let acts: Vec<Action> = (0..4).map(|i| Action { code: std::array::from_fn(|k| ((k * (i + 1)) as f32 * 0.11).cos()) }).collect();
    let (a, b) = (cpu.predict_batch(&z, &acts).unwrap(), gpu.predict_batch(&z, &acts).unwrap());
    let worst = a.iter().zip(b.iter()).map(|(x, y)| max_diff(x, y)).fold(0.0, f32::max);
    assert!(worst < 2e-3, "cpu vs metal predict differ by {worst}");
}

#[test]
fn checkpoint_round_trips_onto_metal() {
    let cfg = small_vit();
    let gpu = CandleVitEncoder::new_random_on(cfg, 8, &metal()).unwrap();
    let blob = gpu.to_safetensors_bytes().unwrap();
    let cpu_back = CandleVitEncoder::from_safetensors_bytes(&blob).unwrap();
    let gpu_back = CandleVitEncoder::from_safetensors_bytes_on(&blob, &metal()).unwrap();
    assert!(cpu_back.device().unwrap().is_cpu() && gpu_back.device().unwrap().is_metal());
    assert_eq!(cpu_back.to_safetensors_bytes().unwrap(), blob);
    assert_eq!(gpu_back.to_safetensors_bytes().unwrap(), blob);
}

#[test]
fn jepa_steps_track_cpu() {
    let data = tiny_data();
    let batch: Vec<&FrameTransition> = data.iter().take(16).collect();
    let mut cpu = JepaTrainer::new(tiny_cfg(ComputeDevice::Cpu)).unwrap();
    let mut gpu = JepaTrainer::new(tiny_cfg(ComputeDevice::Metal)).unwrap();
    for step in 0..3 {
        let (a, b) = (cpu.step(&batch).unwrap(), gpu.step(&batch).unwrap());
        let rel = |x: f32, y: f32| (x - y).abs() / x.abs().max(1e-3);
        assert!(rel(a.total, b.total) < 0.05, "step {step}: total cpu {} vs metal {}", a.total, b.total);
        assert!(rel(a.prediction, b.prediction) < 0.05, "step {step}: prediction cpu {} vs metal {}", a.prediction, b.prediction);
    }
    assert!(gpu.encoder().device().unwrap().is_metal() && gpu.predictor().device().unwrap().is_metal());
}

#[test]
fn full_jepa_run_on_metal_lowers_the_loss_and_exports() {
    let data = tiny_data();
    let r = train_jepa(&data, JepaTrainConfig { epochs: 12, ..tiny_cfg(ComputeDevice::Metal) }).unwrap();
    assert!(r.final_loss.total < r.initial_loss.total, "loss {:?} -> {:?}", r.initial_loss, r.final_loss);
    assert!(r.held_out_participation_ratio > 1.0 && r.eval.model_err.is_finite());
    let back = CandleVitEncoder::from_safetensors_bytes(&r.encoder_safetensors).unwrap();
    assert_eq!(back.config, small_vit());
}

#[test]
fn adaln_training_runs_on_metal() {
    use super::train::{train_adaln_with, AdaLnTrainConfig};
    let samples = crate::training::action_linear::tests::action_fixture(400, weftos_worldmodel_core::SensorClass::RgbD, 0.001, 31).0;
    let cfg = AdaLnTrainConfig { epochs: 8, lr: 3e-3, seed: 1, batch_size: 32, model: small_adaln(), device: ComputeDevice::Metal, ..AdaLnTrainConfig::default() };
    let r = train_adaln_with(&samples, cfg).unwrap();
    assert!(r.final_loss < 0.5 * r.initial_loss, "loss {} -> {}", r.initial_loss, r.final_loss);
    assert!(r.predictor.device().unwrap().is_metal());
    let s = &samples[0];
    assert!(r.predictor.predict(&s.z_t, &s.action).is_ok());
}
