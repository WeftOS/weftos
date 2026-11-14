//! W5 swap path on a labelled SYNTHETIC fixture (feature `candle`): train AdaLN `pred_φ`,
//! save safetensors to a temp dir, reload, swap into `DefaultWorldModel`, and judge it with
//! the W4 eval harness against copy-last and the per-dimension linear baseline.
#![cfg(feature = "candle")]

use weftos_worldmodel::impls::eval_harness::{evaluate, EvalConfig, Transition};
use weftos_worldmodel::{
    train_adaln_with, zero_latent, Action, AdaLnConfig, AdaLnPredictor, AdaLnTrainConfig,
    DefaultWorldModel, SensorClass, TrainingSample, LATENT_DIM,
};

/// Same generator as the harness's own fixture: each coordinate drifts at ±0.015 per step,
/// scaled by a piecewise-constant scalar action in [0.5, 1.5] (new value every 8 steps), plus
/// noise. The action rides in `Action.code[0]`.
fn fixture(n: usize, noise: f32, seed: u64) -> Vec<Transition> {
    let mut s = seed;
    let mut rnd = move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        ((s >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
    };
    let v = |d: usize| if d.is_multiple_of(2) { 0.015 } else { -0.015 };
    let mut z = zero_latent();
    for (d, zd) in z.iter_mut().enumerate() {
        *zd = -(n as f32) * v(d) / 2.0;
    }
    let (mut out, mut a) = (Vec::new(), 1.0f32);
    for t in 0..n {
        if t.is_multiple_of(8) {
            a = 1.0 + 0.5 * rnd();
        }
        let mut next = z;
        for (d, nd) in next.iter_mut().enumerate() {
            *nd += a * v(d) + noise * rnd();
        }
        out.push(Transition { z_t: z, action: a, z_tp1: next });
        z = next;
    }
    out
}

fn action(a: f32) -> Action {
    let mut act = Action::null();
    act.code[0] = a;
    act
}

#[test]
fn trained_adaln_reloads_from_disk_swaps_in_and_beats_copy_last() {
    let data = fixture(240, 0.0005, 7);
    let cfg = EvalConfig::default();
    let n_train = data.len() - ((data.len() as f32) * cfg.holdout_frac) as usize;
    let samples: Vec<TrainingSample> = data[..n_train]
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let mut s = TrainingSample::basic(SensorClass::RgbD, t.z_t, t.z_tp1, i as u64 * 100);
            s.action = action(t.action);
            s
        })
        .collect();

    let train = AdaLnTrainConfig {
        epochs: 40,
        lr: 5e-3,
        seed: 3,
        batch_size: 32,
        weight_decay: 1e-4,
        model: AdaLnConfig { hidden_dim: 64, depth: 1, ..AdaLnConfig::default() },
    };
    let report = train_adaln_with(&samples, train).expect("train");
    assert!(report.final_loss < 0.2 * report.initial_loss, "{} -> {}", report.initial_loss, report.final_loss);

    let dir = std::env::temp_dir().join(format!("lewm-adaln-swap-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("adaln.safetensors");
    report.predictor.save_safetensors(&path).expect("save");
    let reloaded = AdaLnPredictor::from_safetensors_path(&path).expect("reload");
    std::fs::remove_dir_all(&dir).ok();

    let wm = DefaultWorldModel::with_adaln(reloaded);
    let r = evaluate(&data, |z, a| wm.predict(z, &action(a)).expect("predict"), cfg);
    eprintln!(
        "adaln held-out err {:.6}, copy-last {:.6}, linear {:.6}, promote {}, vetoes {:?}",
        r.model_err, r.baselines.copy_last, r.baselines.linear, r.promote, r.vetoes
    );
    assert!(r.model_err < r.baselines.copy_last, "{r:?}");
    assert!(r.beats_baselines && r.promote, "vetoes {:?} gate {:?}", r.vetoes, r.gate);

    // The default stack (no AdaLN) is the untrained linear map, unchanged.
    let plain = DefaultWorldModel::new();
    assert!(plain.adaln.is_none());
    let z = zero_latent();
    assert_eq!(plain.predict(&z, &Action::null()).unwrap(), z);
    assert_eq!(LATENT_DIM, 192);
}
