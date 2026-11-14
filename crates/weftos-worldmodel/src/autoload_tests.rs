//! Registry auto-load of the learned `pred_φ` through the facade (lane H).
//!
//! Every fixture is labelled SYNTHETIC: `z_{t+1} = z_t + A*·a + b* + noise`.

use super::*;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> f32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        ((self.0 >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
    }
}

/// True dynamics `(A*, b*)` for a seed.
fn truth(seed: u64) -> (alloc::vec::Vec<f32>, Latent) {
    let mut rng = Rng(seed);
    let a = (0..LATENT_DIM * ACTION_CODE_DIM).map(|_| 0.05 * rng.next()).collect();
    let mut b = zero_latent();
    for v in b.iter_mut() {
        *v = 0.01 * rng.next();
    }
    (a, b)
}

fn step(a: &[f32], b: &Latent, z: &Latent, act: &Action) -> Latent {
    let mut out = *z;
    for (d, o) in out.iter_mut().enumerate() {
        let row = &a[d * ACTION_CODE_DIM..(d + 1) * ACTION_CODE_DIM];
        *o += b[d] + row.iter().zip(act.code.iter()).map(|(w, x)| w * x).sum::<f32>();
    }
    out
}

/// `n` independent transitions of the dynamics seeded `dyn_seed` (data seeded `data_seed`).
fn samples(class: SensorClass, n: usize, dyn_seed: u64, data_seed: u64) -> alloc::vec::Vec<TrainingSample> {
    let (a, b) = truth(dyn_seed);
    let mut rng = Rng(data_seed);
    (0..n)
        .map(|t| {
            let mut z = zero_latent();
            z.iter_mut().for_each(|v| *v = rng.next());
            let mut act = Action::null();
            act.code.iter_mut().for_each(|v| *v = rng.next());
            let mut next = step(&a, &b, &z, &act);
            next.iter_mut().for_each(|v| *v += 0.001 * rng.next());
            let mut s = TrainingSample::basic(class, z, next, t as u64 * 100);
            s.action = act;
            s
        })
        .collect()
}

/// Mean `‖ẑ − z'‖²` of `predict` over `held`.
fn held_out_err(held: &[TrainingSample], predict: impl Fn(&Latent, &Action) -> Latent) -> f32 {
    let total: f32 = held
        .iter()
        .map(|s| {
            let p = predict(&s.z_t, &s.action);
            p.iter().zip(s.z_tp1.iter()).map(|(a, b)| (a - b) * (a - b)).sum::<f32>()
        })
        .sum();
    total / held.len() as f32
}

fn bits(z: &Latent) -> alloc::vec::Vec<u32> {
    z.iter().map(|v| v.to_bits()).collect()
}

fn train_and_commit(wm: &mut DefaultWorldModel, class: SensorClass, data: &[TrainingSample], tick: u64) {
    for s in data {
        wm.offline_edge.push_sample(s).unwrap();
    }
    wm.offline_edge_cycle(class, SmallModelKind::Encode, tick).expect("cycle");
    wm.commit_models_at_tick(tick).expect("commit");
}

fn probe() -> (Latent, Action) {
    let mut rng = Rng(99);
    let mut z = zero_latent();
    z.iter_mut().for_each(|v| *v = rng.next());
    let mut a = Action::null();
    a.code.iter_mut().for_each(|v| *v = rng.next());
    (z, a)
}

#[test]
fn offline_cycle_commit_autoloads_and_beats_the_untrained_predictor() {
    let data = samples(SensorClass::Imu, 300, 11, 5);
    let (train, held) = data.split_at(200);
    let mut wm = DefaultWorldModel::new();
    let untrained = held_out_err(held, |z, a| wm.predict_for(SensorClass::Imu, z, a).unwrap());
    assert!(!wm.has_learned_predictor(SensorClass::Imu));

    train_and_commit(&mut wm, SensorClass::Imu, train, 4);
    assert!(wm.has_learned_predictor(SensorClass::Imu));
    let learned = held_out_err(held, |z, a| wm.predict_for(SensorClass::Imu, z, a).unwrap());
    assert!(learned < 0.01 * untrained, "held-out {untrained} -> {learned}");
}

#[test]
fn staged_but_uncommitted_checkpoint_changes_nothing() {
    let data = samples(SensorClass::Imu, 100, 11, 5);
    let mut wm = DefaultWorldModel::new();
    for s in &data {
        wm.offline_edge.push_sample(s).unwrap();
    }
    wm.offline_edge_cycle(SensorClass::Imu, SmallModelKind::Encode, 9).unwrap();
    wm.commit_models_at_tick(3).unwrap(); // deferred
    assert!(!wm.has_learned_predictor(SensorClass::Imu));
    wm.commit_models_at_tick(9).unwrap();
    assert!(wm.has_learned_predictor(SensorClass::Imu));
}

#[test]
fn streaming_promote_commit_autoloads_and_beats_the_untrained_predictor() {
    let data = samples(SensorClass::RgbD, 340, 21, 8);
    let (stream, held) = data.split_at(300);
    let mut wm = DefaultWorldModel::new();
    let untrained = held_out_err(held, |z, a| wm.predict_for(SensorClass::RgbD, z, a).unwrap());
    wm.streaming_merge.gate.min_samples = 2;
    wm.streaming_merge.batch_size = 8;

    let mut promoted = 0usize;
    for (i, s) in stream.iter().enumerate() {
        wm.streaming_merge.push_sample(s).unwrap();
        if i < 1 {
            continue; // the trainer needs two replay samples
        }
        let r = wm.streaming_merge_step(SensorClass::RgbD, SmallModelKind::Encode).unwrap();
        promoted += usize::from(r.promoted);
        wm.commit_models_at_tick(i as u64 + 2).unwrap();
    }
    assert!(promoted > 0, "the AND gate never promoted");
    assert!(wm.has_learned_predictor(SensorClass::RgbD));
    let learned = held_out_err(held, |z, a| wm.predict_for(SensorClass::RgbD, z, a).unwrap());
    assert!(learned < 0.1 * untrained, "held-out {untrained} -> {learned} ({promoted} promoted)");
}

#[test]
fn rollback_restores_the_previous_weights_bit_for_bit() {
    let (z, a) = probe();
    let mut wm = DefaultWorldModel::new();
    let default_out = wm.predict_for(SensorClass::Imu, &z, &a).unwrap();

    train_and_commit(&mut wm, SensorClass::Imu, &samples(SensorClass::Imu, 120, 11, 5), 1);
    let v1 = wm.predict_for(SensorClass::Imu, &z, &a).unwrap();
    assert_ne!(bits(&v1), bits(&default_out));

    // A second cycle trains on different dynamics, replacing v1.
    let mut wm2 = wm.clone();
    wm2.offline_edge = OfflineEdgePipeline::new();
    train_and_commit(&mut wm2, SensorClass::Imu, &samples(SensorClass::Imu, 120, 77, 6), 2);
    let v2 = wm2.predict_for(SensorClass::Imu, &z, &a).unwrap();
    assert_ne!(bits(&v2), bits(&v1));

    let outcome = wm2.rollback_model(SensorClass::Imu, SmallModelKind::Encode).unwrap();
    assert!(matches!(outcome, HotSwapOutcome::RolledBack { .. }));
    let back = wm2.predict_for(SensorClass::Imu, &z, &a).unwrap();
    assert_eq!(bits(&back), bits(&v1), "rollback must serve v1's weights exactly");
    assert_eq!(bits(&wm2.predict(&z, &a).unwrap()), bits(&v1));

    // With no last-good checkpoint, rollback drops the class to the default predictor.
    let outcome = wm.rollback_model(SensorClass::Imu, SmallModelKind::Encode).unwrap();
    assert!(matches!(outcome, HotSwapOutcome::RolledBack { .. }));
    assert!(!wm.has_learned_predictor(SensorClass::Imu));
    assert_eq!(bits(&wm.predict_for(SensorClass::Imu, &z, &a).unwrap()), bits(&default_out));
}

#[test]
fn gate_veto_on_the_registry_rolls_the_predictor_back_after_refresh() {
    let (z, a) = probe();
    let mut wm = DefaultWorldModel::new();
    train_and_commit(&mut wm, SensorClass::Imu, &samples(SensorClass::Imu, 120, 11, 5), 1);
    let v1 = wm.predict_for(SensorClass::Imu, &z, &a).unwrap();
    wm.offline_edge = OfflineEdgePipeline::new();
    train_and_commit(&mut wm, SensorClass::Imu, &samples(SensorClass::Imu, 120, 77, 6), 2);

    let veto = GateVerdict::evaluate(RollbackGateMetrics {
        sigreg_health: 0.1,
        held_out_probe_accuracy: 1.0,
        voe_surprise_diff: 1.0,
        temporal_straightening: 1.0,
    });
    wm.sensor_models
        .apply_gate_verdict(SensorClass::Imu, SmallModelKind::Encode, veto)
        .unwrap();
    wm.refresh_predictors(); // direct registry edits need an explicit resync
    assert_eq!(bits(&wm.predict_for(SensorClass::Imu, &z, &a).unwrap()), bits(&v1));
}

#[test]
fn a_committed_class_does_not_change_other_classes() {
    let (z, a) = probe();
    let mut wm = DefaultWorldModel::new();
    let default_out = wm.predict_for(SensorClass::Lidar, &z, &a).unwrap();
    train_and_commit(&mut wm, SensorClass::Imu, &samples(SensorClass::Imu, 120, 11, 5), 1);
    assert_eq!(bits(&wm.predict_for(SensorClass::Lidar, &z, &a).unwrap()), bits(&default_out));
    assert_ne!(bits(&wm.predict_for(SensorClass::Imu, &z, &a).unwrap()), bits(&default_out));

    let imu = wm.predict_for(SensorClass::Imu, &z, &a).unwrap();
    wm.offline_edge = OfflineEdgePipeline::new();
    train_and_commit(&mut wm, SensorClass::Lidar, &samples(SensorClass::Lidar, 120, 77, 6), 2);
    assert_eq!(bits(&wm.predict_for(SensorClass::Imu, &z, &a).unwrap()), bits(&imu));
    assert_ne!(bits(&wm.predict_for(SensorClass::Lidar, &z, &a).unwrap()), bits(&default_out));
}

#[test]
fn predict_follows_the_last_committed_class_else_the_default() {
    let (z, a) = probe();
    let mut wm = DefaultWorldModel::new();
    let default_out = wm.predict(&z, &a).unwrap();
    train_and_commit(&mut wm, SensorClass::Imu, &samples(SensorClass::Imu, 120, 11, 5), 1);
    assert_eq!(bits(&wm.predict(&z, &a).unwrap()), bits(&wm.predict_for(SensorClass::Imu, &z, &a).unwrap()));
    wm.offline_edge = OfflineEdgePipeline::new();
    train_and_commit(&mut wm, SensorClass::Lidar, &samples(SensorClass::Lidar, 120, 77, 6), 2);
    assert_eq!(bits(&wm.predict(&z, &a).unwrap()), bits(&wm.predict_for(SensorClass::Lidar, &z, &a).unwrap()));
    assert_ne!(bits(&wm.predict(&z, &a).unwrap()), bits(&default_out));
}

#[test]
fn unloadable_encode_checkpoint_leaves_the_class_on_the_default() {
    let (z, a) = probe();
    let mut wm = DefaultWorldModel::new();
    let default_out = wm.predict_for(SensorClass::Audio, &z, &a).unwrap();
    let junk = ModelCheckpoint::new(
        SensorClass::Audio,
        SmallModelKind::Encode,
        1,
        TrainingSurfaceKind::OfflineEdge,
        alloc::vec![1, 2, 3],
        Some(1),
    );
    wm.sensor_models.stage(junk).unwrap();
    wm.commit_models_at_tick(1).unwrap();
    assert!(!wm.has_learned_predictor(SensorClass::Audio));
    assert!(!wm.refresh_class_predictor(SensorClass::Audio));
    assert_eq!(bits(&wm.predict_for(SensorClass::Audio, &z, &a).unwrap()), bits(&default_out));
}

#[test]
fn non_encode_checkpoints_do_not_touch_pred_phi() {
    let mut wm = DefaultWorldModel::new();
    let data = samples(SensorClass::Imu, 60, 11, 5);
    for s in &data {
        wm.offline_edge.push_sample(s).unwrap();
    }
    wm.offline_edge_cycle(SensorClass::Imu, SmallModelKind::TransmitGate, 1).unwrap();
    wm.commit_models_at_tick(1).unwrap();
    assert!(!wm.has_learned_predictor(SensorClass::Imu));
}

#[cfg(feature = "candle")]
#[test]
fn committed_safetensors_adaln_checkpoint_autoloads() {
    use weftos_worldmodel_impls::AdaLnConfig;

    let data = samples(SensorClass::RgbD, 64, 11, 5);
    let cfg = AdaLnTrainConfig {
        epochs: 3,
        model: AdaLnConfig { hidden_dim: 16, depth: 1, ..AdaLnConfig::default() },
        ..AdaLnTrainConfig::default()
    };
    let report = train_adaln_with(&data, cfg).expect("train");
    let cp = report
        .predictor
        .to_model_checkpoint(SensorClass::RgbD, 1, TrainingSurfaceKind::OfflineEdge, Some(1))
        .expect("checkpoint");
    assert_eq!(codec_for_weights(&cp.weights), LEWM_MODEL_SEGMENT_CODEC_SAFETENSORS);

    let (z, a) = probe();
    let mut wm = DefaultWorldModel::new();
    let default_out = wm.predict_for(SensorClass::Imu, &z, &a).unwrap();
    wm.sensor_models.stage(cp).unwrap();
    wm.commit_models_at_tick(1).unwrap();
    assert!(wm.has_learned_predictor(SensorClass::RgbD));

    let want = report.predictor.predict(&z, &a).unwrap();
    let got = wm.predict_for(SensorClass::RgbD, &z, &a).unwrap();
    let max = want.iter().zip(got.iter()).map(|(p, q)| (p - q).abs()).fold(0.0f32, f32::max);
    assert!(max < 1e-5, "reloaded AdaLN differs by {max}");
    assert_eq!(bits(&wm.predict(&z, &a).unwrap()), bits(&got), "last committed class");
    assert_eq!(bits(&wm.predict_for(SensorClass::Imu, &z, &a).unwrap()), bits(&default_out));

    // Rolling back with no last-good drops the class to the default.
    wm.rollback_model(SensorClass::RgbD, SmallModelKind::Encode).unwrap();
    assert!(!wm.has_learned_predictor(SensorClass::RgbD));
}
