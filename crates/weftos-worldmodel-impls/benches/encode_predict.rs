//! Encode / predict timing on CPU and, with `--features metal`, on the Apple GPU (plan W5
//! "runs at 10 Hz on the Mac").
//!
//! Harness-free bench (no criterion dependency): `std::time::Instant` over 200 synthetic
//! 64 × 64 RGB frames, one call per frame after a short warm-up. Prints mean µs per call and
//! calls per second. The candle rows run once per device (the warm-up also compiles the Metal
//! shaders); each result is read back to the host, so GPU time is included. The last rows are one
//! JEPA training step (`JepaTrainer::step`, batch 16 and 32: online ViT forward + backward, EMA
//! target forward, AdaLN forward + backward, AdamW, EMA update) on SYNTHETIC frames, timed over
//! [`JEPA_STEPS`] steps after two warm-up steps.
//!
//! ```text
//! cargo bench -p weftos-worldmodel-impls --features candle --bench encode_predict
//! cargo bench -p weftos-worldmodel-impls --features metal --bench encode_predict
//! ```

use std::hint::black_box;
use std::time::Instant;

use weftos_worldmodel_impls::candle::synthetic::{generate, SyntheticConfig};
use weftos_worldmodel_impls::{
    AdaLnPredictor, ActionLinearModel, Action, CandleVitEncoder, Encoder, FrameTransition,
    HashEncoder, JepaTrainConfig, JepaTrainer, Latent, LinearPredPhi, Predictor, VitTinyConfig,
};
use weftos_worldmodel_impls::candle::{metal_supported, ComputeDevice};

const FRAMES: usize = 200;
const WARMUP: usize = 10;
/// Timed JEPA training steps (each one is a full batch-16 forward + backward).
const JEPA_STEPS: usize = 10;

fn frames(cfg: &VitTinyConfig) -> Vec<Vec<u8>> {
    let mut s = 0x2545_F491_4F6C_DD1Du64;
    (0..FRAMES)
        .map(|_| {
            (0..cfg.frame_len())
                .map(|_| {
                    s ^= s << 13;
                    s ^= s >> 7;
                    s ^= s << 17;
                    (s >> 56) as u8
                })
                .collect()
        })
        .collect()
}

fn time(name: &str, dev: &str, mut f: impl FnMut(usize)) {
    for i in 0..WARMUP {
        f(i);
    }
    let t0 = Instant::now();
    for i in 0..FRAMES {
        f(i);
    }
    let us = t0.elapsed().as_secs_f64() * 1e6 / FRAMES as f64;
    println!("| {name:<36} | {dev:<5} | {us:>12.1} | {:>12.0} |", 1e6 / us);
}

fn jepa_row(batch_size: usize, device: ComputeDevice, label: &str) {
    let jcfg = JepaTrainConfig { batch_size, device, ..JepaTrainConfig::default() };
    let jdata = generate(&SyntheticConfig { days: 1, sessions_per_day: 1, steps_per_session: batch_size, ..SyntheticConfig::default() });
    let batch: Vec<&FrameTransition> = jdata.iter().collect();
    let mut trainer = JepaTrainer::new(jcfg).expect("jepa trainer");
    for _ in 0..2 {
        black_box(trainer.step(&batch).unwrap());
    }
    let t0 = Instant::now();
    for _ in 0..JEPA_STEPS {
        black_box(trainer.step(black_box(&batch)).unwrap());
    }
    let us = t0.elapsed().as_secs_f64() * 1e6 / JEPA_STEPS as f64;
    let name = format!("JEPA train step (batch {batch_size})");
    println!("| {name:<36} | {label:<5} | {us:>12.1} | {:>12.2} |", 1e6 / us);
    println!("  JEPA step {label} batch {batch_size}: {} frames/s through the online encoder", (batch_size as f64 * 1e6 / us).round());
}

fn main() {
    let cfg = VitTinyConfig::default();
    let data = frames(&cfg);
    let adaln_cfg = AdaLnPredictor::new_random(1).expect("adaln").config;
    let lin_untrained = LinearPredPhi::default();
    let mut m = ActionLinearModel::zero();
    for (i, v) in m.a.iter_mut().enumerate() {
        *v = ((i % 7) as f32 - 3.0) * 0.01;
    }
    let lin_learned = LinearPredPhi::from_model(m);
    let latents: Vec<Latent> = data.iter().map(|f| HashEncoder.encode(f).unwrap()).collect();
    let mut action = Action::null();
    action.code[0] = 0.5;
    action.code[7] = -0.25;

    println!("frames: {FRAMES} x {} bytes (ViT-tiny {}px, patch {}, embed {}, depth {}, heads {}); AdaLN hidden {} depth {}",
        cfg.frame_len(), cfg.image_size, cfg.patch_size, cfg.embed_dim, cfg.depth, cfg.num_heads,
        adaln_cfg.hidden_dim, adaln_cfg.depth);
    println!("| {:<36} | {:<5} | {:>12} | {:>12} |", "call", "dev", "mean µs", "calls/s");
    println!("|{:-<38}|{:-<7}|{:-<14}|{:-<14}|", "", "", "", "");
    time("HashEncoder::encode", "cpu", |i| {
        black_box(HashEncoder.encode(black_box(&data[i % FRAMES])).unwrap());
    });
    time("LinearPredPhi::predict (untrained)", "cpu", |i| {
        black_box(lin_untrained.predict(black_box(&latents[i % FRAMES]), &action).unwrap());
    });
    time("LinearPredPhi::predict (learned A,b)", "cpu", |i| {
        black_box(lin_learned.predict(black_box(&latents[i % FRAMES]), &action).unwrap());
    });

    let mut devices = vec![("cpu", ComputeDevice::Cpu)];
    if metal_supported() {
        devices.push(("metal", ComputeDevice::Metal));
    }
    for (label, cd) in &devices {
        let dev = cd.resolve().expect("device");
        let vit = CandleVitEncoder::new_random_on(cfg, 1, &dev).expect("vit");
        let adaln = AdaLnPredictor::new_random_on(adaln_cfg, 1, &dev).expect("adaln");
        time("CandleVitEncoder::encode (random)", label, |i| {
            black_box(vit.encode(black_box(&data[i % FRAMES])).unwrap());
        });
        time("AdaLnPredictor::predict (random)", label, |i| {
            black_box(adaln.predict(black_box(&latents[i % FRAMES]), &action).unwrap());
        });
        time("ViT encode + AdaLN predict (1 step)", label, |i| {
            let z = vit.encode(&data[i % FRAMES]).unwrap();
            black_box(adaln.predict(&z, &action).unwrap());
        });
    }
    for (label, cd) in &devices {
        for batch in [16, 32] {
            jepa_row(batch, *cd, label);
        }
    }
}
