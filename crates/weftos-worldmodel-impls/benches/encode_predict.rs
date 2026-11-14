//! Encode / predict timing on CPU (plan W5 "runs at 10 Hz on the Mac").
//!
//! Harness-free bench (no criterion dependency): `std::time::Instant` over 200 synthetic
//! 64 × 64 RGB frames, one call per frame after a short warm-up. Prints mean µs per call and
//! calls per second.
//!
//! ```text
//! cargo bench -p weftos-worldmodel-impls --features candle --bench encode_predict
//! ```

use std::hint::black_box;
use std::time::Instant;

use weftos_worldmodel_impls::{
    AdaLnPredictor, ActionLinearModel, Action, CandleVitEncoder, Encoder, HashEncoder, Latent,
    LinearPredPhi, Predictor, VitTinyConfig,
};

const FRAMES: usize = 200;
const WARMUP: usize = 10;

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

fn time(name: &str, mut f: impl FnMut(usize)) {
    for i in 0..WARMUP {
        f(i);
    }
    let t0 = Instant::now();
    for i in 0..FRAMES {
        f(i);
    }
    let us = t0.elapsed().as_secs_f64() * 1e6 / FRAMES as f64;
    println!("| {name:<36} | {us:>12.1} | {:>12.0} |", 1e6 / us);
}

fn main() {
    let cfg = VitTinyConfig::default();
    let data = frames(&cfg);
    let vit = CandleVitEncoder::new_random(1).expect("vit");
    let adaln = AdaLnPredictor::new_random(1).expect("adaln");
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
        adaln.config.hidden_dim, adaln.config.depth);
    println!("| {:<36} | {:>12} | {:>12} |", "call", "mean µs", "calls/s");
    println!("|{:-<38}|{:-<14}|{:-<14}|", "", "", "");
    time("HashEncoder::encode", |i| {
        black_box(HashEncoder.encode(black_box(&data[i % FRAMES])).unwrap());
    });
    time("LinearPredPhi::predict (untrained)", |i| {
        black_box(lin_untrained.predict(black_box(&latents[i % FRAMES]), &action).unwrap());
    });
    time("LinearPredPhi::predict (learned A,b)", |i| {
        black_box(lin_learned.predict(black_box(&latents[i % FRAMES]), &action).unwrap());
    });
    time("CandleVitEncoder::encode (random)", |i| {
        black_box(vit.encode(black_box(&data[i % FRAMES])).unwrap());
    });
    time("AdaLnPredictor::predict (random)", |i| {
        black_box(adaln.predict(black_box(&latents[i % FRAMES]), &action).unwrap());
    });
    time("ViT encode + AdaLN predict (1 step)", |i| {
        let z = vit.encode(&data[i % FRAMES]).unwrap();
        black_box(adaln.predict(&z, &action).unwrap());
    });
}
