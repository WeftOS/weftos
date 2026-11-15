//! Trajectory divergence of the online encoder `θ` between backends (lane P,
//! `--features metal`). Several trainers start from the same seed and step in lockstep on
//! identical batches; after each step the relative L2 distance `‖θ_a − θ_b‖ / ‖θ_a‖` of chosen
//! pairs is printed (grep `TRACE|`). Pairs: CPU vs Metal; CPU vs CPU with a last-bits init
//! jitter (and the same on Metal), the chaos probe; and CPU vs Metal under each reduction
//! mode. If the backend pair grows at the same rate as the jitter pair and saturates at the
//! same level, the backend difference is one more tiny perturbation that the run amplifies,
//! not a bias. Data is SYNTHETIC.

use super::drift_tests::{host, named_vars};
use super::jepa::{FrameTransition, JepaTrainConfig, JepaTrainer};
use super::numerics::{JepaNumerics, ReduceMode};
use super::synthetic::{generate, SyntheticConfig};
use super::{AdaLnConfig, ComputeDevice, SplitMix64, VitTinyConfig};
use weftos_worldmodel_core::LATENT_DIM;

/// Relative init jitter of the chaos probe: about one f32 ulp.
const JITTER: f64 = 1.2e-7;

fn theta(tr: &JepaTrainer) -> Vec<f32> {
    let net = tr.encoder().net.expect("trainer encoder has weights");
    named_vars(&net.varmap).iter().flat_map(|(_, v)| host(v.as_tensor())).collect()
}

fn rel_l2(a: &[f32], b: &[f32]) -> f64 {
    let num: f64 = a.iter().zip(b).map(|(x, y)| (*x as f64 - *y as f64).powi(2)).sum();
    let den: f64 = a.iter().map(|x| (*x as f64).powi(2)).sum();
    (num / den.max(1e-30)).sqrt()
}

/// Named variants of `base`: device, reduction mode, jitter.
fn variants(base: JepaTrainConfig) -> Vec<(&'static str, JepaTrainConfig)> {
    let with = |device, reductions, init_jitter| JepaTrainConfig { device, numerics: JepaNumerics { reductions, init_jitter, jitter_seed: 1 }, ..base };
    vec![
        ("cpu", with(ComputeDevice::Cpu, ReduceMode::Native, 0.0)),
        ("metal", with(ComputeDevice::Metal, ReduceMode::Native, 0.0)),
        ("cpu+jitter", with(ComputeDevice::Cpu, ReduceMode::Native, JITTER)),
        ("metal+jitter", with(ComputeDevice::Metal, ReduceMode::Native, JITTER)),
        ("metal host-red", with(ComputeDevice::Metal, ReduceMode::Host, 0.0)),
        ("cpu pairwise", with(ComputeDevice::Cpu, ReduceMode::Pairwise, 0.0)),
        ("metal pairwise", with(ComputeDevice::Metal, ReduceMode::Pairwise, 0.0)),
    ]
}

const PAIRS: [(usize, usize); 5] = [(0, 1), (0, 2), (1, 3), (0, 4), (5, 6)];

/// Step every variant `steps` times in lockstep; return the per-step distances of [`PAIRS`]
/// and print them at a few checkpoints.
fn trace(tag: &str, base: JepaTrainConfig, data: &[FrameTransition], steps: usize) -> Vec<[f64; 5]> {
    let vs = variants(base);
    let mut trainers: Vec<JepaTrainer> = vs.iter().map(|(_, c)| JepaTrainer::new(*c).unwrap()).collect();
    let (mut order, mut rng) = ((0..data.len()).collect::<Vec<usize>>(), SplitMix64::new(base.seed ^ 0x0E9A_5EED));
    let (mut out, mut cursor) = (Vec::with_capacity(steps), data.len());
    let names: Vec<String> = PAIRS.iter().map(|(a, b)| format!("{} vs {}", vs[*a].0, vs[*b].0)).collect();
    eprintln!("TRACE|{tag}|step|{}", names.join("|"));
    let d0: Vec<f64> = { let th: Vec<Vec<f32>> = trainers.iter().map(theta).collect(); PAIRS.iter().map(|(a, b)| rel_l2(&th[*a], &th[*b])).collect() };
    eprintln!("TRACE|{tag}|init|{}", d0.iter().map(|d| format!("{d:.2e}")).collect::<Vec<_>>().join("|"));
    for s in 1..=steps {
        if cursor + base.batch_size > data.len() {
            for i in (1..order.len()).rev() {
                order.swap(i, (rng.next_u64() % (i as u64 + 1)) as usize);
            }
            cursor = 0;
        }
        let batch: Vec<&FrameTransition> = order[cursor..cursor + base.batch_size].iter().map(|&i| &data[i]).collect();
        cursor += base.batch_size;
        let losses: Vec<f32> = trainers.iter_mut().map(|t| t.step(&batch).unwrap().total).collect();
        assert!(losses.iter().all(|l| l.is_finite()), "step {s}: {losses:?}");
        let th: Vec<Vec<f32>> = trainers.iter().map(theta).collect();
        let d: [f64; 5] = std::array::from_fn(|i| rel_l2(&th[PAIRS[i].0], &th[PAIRS[i].1]));
        if s <= 5 || s % 5 == 0 {
            eprintln!(
                "TRACE|{tag}|{s:4}|{}|loss cpu {:.5} metal {:.5}",
                d.iter().map(|x| format!("{x:.2e}")).collect::<Vec<_>>().join("|"),
                losses[0],
                losses[1]
            );
        }
        out.push(d);
    }
    out
}

/// Growth exponent per step of each pair's distance between `from` and `to` (log-slope).
fn growth(trace: &[[f64; 5]], from: usize, to: usize) -> [f64; 5] {
    std::array::from_fn(|i| (trace[to - 1][i].max(1e-30) / trace[from - 1][i].max(1e-30)).ln() / (to - from) as f64)
}

fn tiny_cfg() -> JepaTrainConfig {
    JepaTrainConfig {
        lr: 1e-2,
        batch_size: 16,
        seed: 3,
        ema_tau: 0.8,
        encoder: VitTinyConfig { image_size: 32, patch_size: 8, embed_dim: 32, depth: 2, num_heads: 2, latent_dim: LATENT_DIM },
        predictor: AdaLnConfig { hidden_dim: 32, depth: 2, ..AdaLnConfig::default() },
        ..JepaTrainConfig::lane_g()
    }
}

/// 50 steps of the tiny Metal-test config.
#[test]
fn tiny_trajectory_divergence_cpu_vs_metal_vs_jitter() {
    let data = generate(&SyntheticConfig { image_size: 32, days: 3, sessions_per_day: 2, steps_per_session: 16, blob_radius: 4.0, action_step_px: 3.0, noise: 6, seed: 11, ..SyntheticConfig::default() });
    let t = trace("tiny", tiny_cfg(), &data, 50);
    eprintln!("TRACE|tiny|growth/step 2..20|{:.3?}", growth(&t, 2, 20));
    assert!(t.iter().flatten().all(|d| d.is_finite() && *d < 10.0));
    // The default numerics are untouched: two CPU trainers with the same config stay identical.
    let (mut a, mut b) = (JepaTrainer::new(tiny_cfg()).unwrap(), JepaTrainer::new(tiny_cfg()).unwrap());
    let batch: Vec<&FrameTransition> = data.iter().take(16).collect();
    assert_eq!(a.step(&batch).unwrap(), b.step(&batch).unwrap());
    assert_eq!(theta(&a), theta(&b));
}

/// 50 steps of the default config on the default ViT-tiny (lane J fixture `std`, seed from
/// `LEWM_SEED`, default 21). Release only:
///
/// ```text
/// cargo test --release -p weftos-worldmodel-impls --features metal \
///     default_trajectory_divergence -- --ignored --nocapture
/// ```
#[test]
#[ignore = "release only, about a minute of CPU"]
fn default_trajectory_divergence_cpu_vs_metal_vs_jitter() {
    let seed = std::env::var("LEWM_SEED").ok().and_then(|v| v.parse().ok()).unwrap_or(21);
    let (data, _) = super::jepa::bakeoff_tests::fixture("std");
    let train: Vec<FrameTransition> = data.into_iter().filter(|t| t.day < 4).collect();
    let steps = std::env::var("LEWM_STEPS").ok().and_then(|v| v.parse().ok()).unwrap_or(50);
    let t = trace(&format!("default s{seed}"), JepaTrainConfig { seed, ..JepaTrainConfig::default() }, &train, steps);
    eprintln!("TRACE|default s{seed}|growth/step 2..{steps}|{:.3?}", growth(&t, 2, steps));
    assert!(t.iter().flatten().all(|d| d.is_finite()));
}
