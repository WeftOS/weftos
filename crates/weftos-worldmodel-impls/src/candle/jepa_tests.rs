//! Tests for the JEPA loop (all data SYNTHETIC, from [`crate::candle::synthetic`]).

use super::*;
use crate::candle::synthetic::{generate, SyntheticConfig};
use candle_core::Device;
use weftos_worldmodel_core::{Latent, Predictor};

/// Small encoder so debug-mode tests stay fast.
fn tiny_encoder() -> VitTinyConfig {
    VitTinyConfig { image_size: 32, patch_size: 8, embed_dim: 32, depth: 1, num_heads: 2, latent_dim: LATENT_DIM }
}

/// Lane G's settings on the tiny encoder. The lane J default (VICReg hinge + correlation
/// decorrelation) is tuned for the default ViT-tiny; on this tiny encoder at lr 1e-2 it drives
/// every coordinate under the activity floor, so these tests keep the lane G loss.
fn tiny_cfg() -> JepaTrainConfig {
    JepaTrainConfig {
        epochs: 40,
        lr: 1e-2,
        batch_size: 16,
        seed: 3,
        ema_tau: 0.8,
        encoder: tiny_encoder(),
        predictor: AdaLnConfig { hidden_dim: 32, depth: 1, ..AdaLnConfig::default() },
        ..JepaTrainConfig::lane_g()
    }
}

fn tiny_data() -> Vec<FrameTransition> {
    generate(&SyntheticConfig { image_size: 32, days: 3, sessions_per_day: 2, steps_per_session: 16, blob_radius: 4.0, action_step_px: 3.0, noise: 6, seed: 11, ..SyntheticConfig::default() })
}

fn scalar(t: &Tensor) -> f64 {
    t.to_scalar::<f32>().unwrap() as f64
}

#[test]
fn isotropy_penalty_matches_a_brute_force_reference() {
    let (b, d) = (5usize, 3usize);
    let vals: Vec<f32> = (0..b * d).map(|i| ((i * 7 % 11) as f32 - 5.0) * 0.4).collect();
    let z = Tensor::from_vec(vals.clone(), (b, d), &Device::Cpu).unwrap();
    let row = |i: usize| &vals[i * d..(i + 1) * d];
    let mu: Vec<f64> = (0..d).map(|j| (0..b).map(|i| row(i)[j] as f64).sum::<f64>() / b as f64).collect();
    let cov = |j: usize, k: usize| (0..b).map(|i| (row(i)[j] as f64 - mu[j]) * (row(i)[k] as f64 - mu[k])).sum::<f64>() / (b as f64 - 1.0);
    let mean_term: f64 = mu.iter().map(|m| m * m).sum();
    let diag: f64 = (0..d).map(|j| ((cov(j, j) + ISO_STD_EPS).sqrt() - 1.0).powi(2)).sum();
    let off: f64 = (0..d).flat_map(|j| (0..d).map(move |k| (j, k))).filter(|(j, k)| j != k).map(|(j, k)| cov(j, k).powi(2)).sum();
    let got_diag = scalar(&isotropy_penalty(&z, 0.0).unwrap());
    assert!((got_diag - (mean_term + diag) / d as f64).abs() < 1e-4, "{got_diag}");
    // gamma = 1 adds the squared off-diagonal covariances.
    let got_full = scalar(&isotropy_penalty(&z, 1.0).unwrap());
    assert!((got_full - (mean_term + diag + off) / d as f64).abs() < 1e-4, "{got_full}");
    // A collapsed batch (all rows equal) scores |mu|^2/d + (sqrt(eps) - 1)^2.
    let flat = Tensor::from_vec(vec![0.5f32; 4 * d], (4, d), &Device::Cpu).unwrap();
    let want = 0.25 + (ISO_STD_EPS.sqrt() - 1.0).powi(2);
    assert!((scalar(&isotropy_penalty(&flat, 0.0).unwrap()) - want).abs() < 1e-5);
}

/// f64 Epps-Pulley statistic of one projection by trapezoid quadrature of the definition
/// `∫ |φ̂(t) − e^{−t²/2}|² N(t; 0, β²) dt` (independent of the closed form under test).
fn epps_pulley_quadrature(x: &[f64], beta: f64) -> f64 {
    let (n, lim, m) = (x.len() as f64, 12.0 * beta, 24_001usize);
    let h = 2.0 * lim / (m - 1) as f64;
    (0..m)
        .map(|i| {
            let t = -lim + i as f64 * h;
            let (re, im) = x.iter().fold((0.0, 0.0), |(r, s), v| (r + (t * v).cos() / n, s + (t * v).sin() / n));
            let w = (-t * t / (2.0 * beta * beta)).exp() / (2.0 * std::f64::consts::PI * beta * beta).sqrt();
            let f = ((re - (-t * t / 2.0).exp()).powi(2) + im * im) * w;
            if i == 0 || i == m - 1 { f * h / 2.0 } else { f * h }
        })
        .sum()
}

#[test]
fn epps_pulley_penalty_matches_quadrature_of_its_definition() {
    let (b, d, k) = (7usize, 4usize, 3usize);
    let vals: Vec<f32> = (0..b * d).map(|i| ((i * 5 % 13) as f32 - 6.0) * 0.3).collect();
    let z = Tensor::from_vec(vals.clone(), (b, d), &Device::Cpu).unwrap();
    let dirs = random_directions(&mut SplitMix64::new(8), d, k, &Device::Cpu).unwrap();
    let u: Vec<Vec<f32>> = dirs.to_vec2().unwrap();
    let want: f64 = (0..k)
        .map(|c| {
            let x: Vec<f64> = (0..b).map(|r| (0..d).map(|j| vals[r * d + j] as f64 * u[j][c] as f64).sum()).collect();
            epps_pulley_quadrature(&x, EP_BETA)
        })
        .sum::<f64>()
        / k as f64;
    let got = scalar(&epps_pulley_penalty(&z, &dirs).unwrap());
    assert!((got - want).abs() < 1e-5, "{got} vs {want}");
    // Collapsed at the origin: 1 − 2/√2 + 1/√3; standard-normal-like samples score far lower.
    let zero = Tensor::zeros((b, d), candle_core::DType::F32, &Device::Cpu).unwrap();
    let collapsed = 1.0 - 2.0 / 2f64.sqrt() + 1.0 / 3f64.sqrt();
    assert!((scalar(&epps_pulley_penalty(&zero, &dirs).unwrap()) - collapsed).abs() < 1e-5);
    let mut rng = SplitMix64::new(2);
    let mut u01 = || ((rng.uniform() as f64 + 1.0) / 2.0).max(1e-9);
    let gauss: Vec<f32> = (0..256 * d).map(|_| ((-2.0 * u01().ln()).sqrt() * (std::f64::consts::TAU * u01()).cos()) as f32).collect();
    let g = Tensor::from_vec(gauss, (256, d), &Device::Cpu).unwrap();
    assert!(scalar(&epps_pulley_penalty(&g, &dirs).unwrap()) < 0.01);
}

#[test]
fn vicreg_terms_match_a_brute_force_reference() {
    let (b, d) = (6usize, 4usize);
    let vals: Vec<f32> = (0..b * d).map(|i| ((i * 7 % 11) as f32 - 5.0) * 0.15).collect();
    let z = Tensor::from_vec(vals.clone(), (b, d), &Device::Cpu).unwrap();
    let row = |i: usize| &vals[i * d..(i + 1) * d];
    let mu: Vec<f64> = (0..d).map(|j| (0..b).map(|i| row(i)[j] as f64).sum::<f64>() / b as f64).collect();
    let cov = |j: usize, k: usize| (0..b).map(|i| (row(i)[j] as f64 - mu[j]) * (row(i)[k] as f64 - mu[k])).sum::<f64>() / (b as f64 - 1.0);
    let hinge: f64 = (0..d).map(|j| (1.0 - (cov(j, j) + ISO_STD_EPS).sqrt()).max(0.0)).sum::<f64>() / d as f64;
    let off: f64 = (0..d).flat_map(|j| (0..d).map(move |k| (j, k))).filter(|(j, k)| j != k).map(|(j, k)| cov(j, k).powi(2)).sum::<f64>() / d as f64;
    let (h, c) = vicreg_terms(&z, false).unwrap();
    assert!((scalar(&h) - hinge).abs() < 1e-5, "{} vs {hinge}", scalar(&h));
    assert!((scalar(&c) - off).abs() < 1e-5, "{} vs {off}", scalar(&c));
    let sd = |j: usize| (cov(j, j) + crate::candle::isotropy::CORR_EPS).sqrt();
    let rho = |j: usize, k: usize| cov(j, k) / (sd(j) * sd(k));
    let off_r: f64 = (0..d).flat_map(|j| (0..d).map(move |k| (j, k))).filter(|(j, k)| j != k).map(|(j, k)| rho(j, k).powi(2)).sum::<f64>() / d as f64;
    let (h2, c2) = vicreg_terms(&z, true).unwrap();
    assert!((scalar(&h2) - hinge).abs() < 1e-5);
    assert!((scalar(&c2) - off_r).abs() < 1e-4, "{} vs {off_r}", scalar(&c2));
    // Scale-free: doubling z leaves the correlation term unchanged.
    let (_, c3) = vicreg_terms(&(z * 2.0).unwrap(), true).unwrap();
    assert!((scalar(&c3) - scalar(&c2)).abs() < 1e-3 * scalar(&c2).max(1.0));
}

#[test]
fn target_branch_gets_no_gradient_and_tau_follows_the_cosine_schedule() {
    let data = tiny_data();
    let batch: Vec<&FrameTransition> = data.iter().take(8).collect();
    let next: Vec<Option<&FrameTransition>> = (0..8).map(|i| Some(&data[i + 1])).collect();
    let cfg = JepaTrainConfig { multistep_weight: 1.0, vic_var_weight: 1.0, vic_cov_weight: 0.1, slice_test: SliceTest::EppsPulley, ..tiny_cfg() };
    let tr = JepaTrainer::new(cfg).unwrap();
    let dirs = random_directions(&mut SplitMix64::new(1), LATENT_DIM, 16, &Device::Cpu).unwrap();
    let (total, _, pred2, _) = tr.losses(&batch, &next, true, &dirs).unwrap();
    assert!(scalar(&pred2) > 0.0, "two-step term active");
    let grads = total.backward().unwrap();
    let online_hit = trainable(&tr.online.varmap).unwrap().iter().filter(|(_, v)| grads.get(v.as_tensor()).is_some()).count();
    assert!(online_hit > 0, "online encoder receives gradient");
    for (name, v) in trainable(&tr.target.varmap).unwrap() {
        assert!(grads.get(v.as_tensor()).is_none(), "target var {name} has a gradient");
    }
    // Cosine tau: start at ema_tau_start, end at ema_tau, monotone in between.
    let mut tr = JepaTrainer::new(JepaTrainConfig { ema_tau_start: Some(0.5), ema_tau: 0.9, ..tiny_cfg() }).unwrap();
    tr.set_total_steps(5);
    let mut taus = vec![tr.current_tau()];
    for _ in 0..4 {
        tr.step(&batch).unwrap();
        taus.push(tr.current_tau());
    }
    assert!((taus[0] - 0.5).abs() < 1e-12 && (taus[4] - 0.9).abs() < 1e-12 && (taus[2] - 0.7).abs() < 1e-12, "{taus:?}");
    assert!(taus.windows(2).all(|w| w[1] > w[0]));
}

#[test]
fn participation_ratio_sees_rank_one_collapse() {
    let mut rng = SplitMix64::new(5);
    let spread: Vec<Latent> = (0..400).map(|_| core::array::from_fn(|_| rng.uniform())).collect();
    let line: Vec<Latent> = (0..400).map(|_| {
        let s = rng.uniform();
        core::array::from_fn(|i| s * (1.0 + i as f32 * 0.01))
    }).collect();
    let (pr_spread, pr_line) = (participation_ratio(&spread), participation_ratio(&line));
    assert!(pr_spread > 100.0, "{pr_spread}");
    assert!((pr_line - 1.0).abs() < 1e-3, "{pr_line}");
}

fn vars_of(net: &VitNet) -> Vec<Vec<f32>> {
    trainable(&net.varmap).unwrap().into_iter().map(|(_, v)| v.as_tensor().flatten_all().unwrap().to_vec1().unwrap()).collect()
}

#[test]
fn ema_target_follows_the_online_encoder_at_rate_tau() {
    let data = tiny_data();
    let batch: Vec<&FrameTransition> = data.iter().take(8).collect();
    for tau in [0.0, 0.5, 1.0] {
        let mut tr = JepaTrainer::new(JepaTrainConfig { ema_tau: tau, ..tiny_cfg() }).unwrap();
        let before = vars_of(&tr.target);
        assert_eq!(before, vars_of(&tr.online), "online and target start identical");
        tr.step(&batch).unwrap();
        let (on, tg) = (vars_of(&tr.online), vars_of(&tr.target));
        assert_ne!(on, before, "the online encoder moved");
        for ((t, o), b) in tg.iter().zip(on.iter()).zip(before.iter()) {
            for ((t, o), b) in t.iter().zip(o.iter()).zip(b.iter()) {
                let want = tau as f32 * b + (1.0 - tau as f32) * o;
                assert!((t - want).abs() < 1e-6, "tau {tau}: {t} vs {want}");
            }
        }
    }
    assert_eq!(JepaTrainer::new(tiny_cfg()).unwrap().steps(), 0);
}

#[test]
fn bad_settings_and_too_few_days_are_refused() {
    assert!(JepaTrainer::new(JepaTrainConfig { batch_size: 1, ..tiny_cfg() }).is_err());
    assert!(JepaTrainer::new(JepaTrainConfig { ema_tau: 1.5, ..tiny_cfg() }).is_err());
    let one_day: Vec<FrameTransition> = tiny_data().into_iter().filter(|t| t.day == 0).collect();
    let err = train_jepa(&one_day, tiny_cfg()).unwrap_err().to_string();
    assert!(err.contains("distinct days"), "{err}");
    let tr = JepaTrainer::new(tiny_cfg()).unwrap();
    assert!(tr.evaluate_loss(&[]).is_err());
}

fn print_report(tag: &str, r: &JepaTrainReport) {
    eprintln!(
        "[{tag}] SYNTHETIC: {} train / {} held-out (days {:?}), {} steps; loss total {:.4} -> {:.4} (pred {:.4} -> {:.4}, iso {:.4} -> {:.4}); held-out {:.4}",
        r.n_train, r.n_held_out, r.held_out_days, r.steps, r.initial_loss.total, r.final_loss.total,
        r.initial_loss.prediction, r.final_loss.prediction, r.initial_loss.isotropy, r.final_loss.isotropy, r.held_out_loss.total
    );
    for (i, l) in r.epoch_losses.iter().enumerate() {
        eprintln!("[{tag}] epoch {i}: total {:.4} pred {:.4} iso {:.4}", l.total, l.prediction, l.isotropy);
    }
    let e = &r.eval;
    eprintln!(
        "[{tag}] held-out sigreg {:.4}, participation ratio held-out {:.2} train {:.2}; harness: model {:.5} copy-last {:.5} linear {:.5} beats {} collapsed {} sigreg {:.4} promote {} vetoes {:?}",
        r.held_out_sigreg_health, r.held_out_participation_ratio, r.train_participation_ratio, e.model_err, e.baselines.copy_last, e.baselines.linear,
        e.beats_baselines, e.collapsed_coords.len(), e.sigreg_health, e.promote, e.vetoes
    );
}

#[test]
fn tiny_jepa_run_lowers_the_loss_keeps_latents_alive_and_reloads_identically() {
    let data = tiny_data();
    let r = train_jepa(&data, JepaTrainConfig { diagnostics: true, ..tiny_cfg() }).unwrap();
    print_report("tiny", &r);
    assert_eq!(r.epoch_diagnostics.len(), tiny_cfg().epochs);
    let d0 = r.epoch_diagnostics[0];
    assert!(d0.target_gap > 0.0 && d0.target_gap.is_finite(), "target lags theta: {d0:?}");
    assert!((0.0..=1.0).contains(&d0.online.session_fraction) && d0.prediction_share > 0.0 && d0.prediction_share < 1.0);
    assert!(r.eval_target.model_err.is_finite() && r.held_out_diagnostics.participation_ratio == r.held_out_participation_ratio);
    assert_eq!((r.n_train, r.n_held_out, r.held_out_days.clone()), (64, 32, vec![2]));
    let cfg = tiny_cfg();
    assert_eq!(r.steps, cfg.epochs * 4);
    assert_eq!(r.epoch_losses.len(), cfg.epochs);
    assert!(r.final_loss.total < r.initial_loss.total, "loss {:?} -> {:?}", r.initial_loss, r.final_loss);
    assert!(r.final_loss.isotropy < r.initial_loss.isotropy);
    assert!(r.held_out_sigreg_health.is_finite() && r.held_out_participation_ratio > 1.5);
    assert!(r.eval.collapsed_coords.is_empty(), "collapsed coords {:?}", r.eval.collapsed_coords);
    assert_eq!(r.eval.n_held_out_days, 1);

    // Export both checkpoints to files and bytes, reload, compare on held-out frames.
    let dir = std::env::temp_dir().join(format!("lewm-jepa-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (ep, pp) = (dir.join("encoder.safetensors"), dir.join("predictor.safetensors"));
    r.encoder.save_safetensors(&ep).unwrap();
    r.predictor.save_safetensors(&pp).unwrap();
    let enc_file = CandleVitEncoder::from_safetensors_path(&ep).unwrap();
    let pred_file = AdaLnPredictor::from_safetensors_path(&pp).unwrap();
    let enc_bytes = CandleVitEncoder::from_safetensors_bytes(&r.encoder_safetensors).unwrap();
    let pred_bytes = AdaLnPredictor::from_safetensors_bytes(&r.predictor_safetensors).unwrap();
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(enc_file.config, tiny_encoder());
    let held: Vec<&FrameTransition> = data.iter().filter(|t| t.day == 2).collect();
    let frames: Vec<&[u8]> = held.iter().map(|t| t.frame_t.as_slice()).collect();
    let z0 = encode_all(&r.encoder, &frames).unwrap();
    for other in [&enc_file, &enc_bytes] {
        for (a, b) in z0.iter().zip(encode_all(other, &frames).unwrap().iter()) {
            for (x, y) in a.iter().zip(b.iter()) {
                assert!((x - y).abs() <= 1e-5, "latent {x} vs {y}");
            }
        }
    }
    for (t, z) in held.iter().zip(z0.iter()) {
        let p0 = r.predictor.predict(z, &t.action).unwrap();
        for other in [&pred_file, &pred_bytes] {
            for (x, y) in p0.iter().zip(other.predict(z, &t.action).unwrap().iter()) {
                assert!((x - y).abs() <= 1e-5, "prediction {x} vs {y}");
            }
        }
    }
}

/// Longer run for manual use: default ViT-tiny (64 px, depth 4) on more synthetic days.
/// Writes `encoder.safetensors` + `predictor.safetensors` into `$LEWM_OUT` when set.
///
/// ```text
/// LEWM_OUT=/path/to/dir cargo test --release -p weftos-worldmodel-impls --features candle \
///     jepa_long_run -- --ignored --nocapture
/// ```
#[test]
#[ignore = "minutes of CPU; run by hand with LEWM_OUT set"]
fn jepa_long_run_writes_checkpoints() {
    let data = generate(&SyntheticConfig { days: 5, sessions_per_day: 3, steps_per_session: 32, seed: 21, ..SyntheticConfig::default() });
    let epochs = std::env::var("LEWM_EPOCHS").ok().and_then(|v| v.parse().ok()).unwrap_or(12);
    let env = |k: &str, d: f64| std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
    let cfg = JepaTrainConfig {
        epochs,
        lr: env("LEWM_LR", 1e-3),
        iso_weight: env("LEWM_ISO", 1.0),
        ema_tau: env("LEWM_TAU", 0.99),
        slice_weight: env("LEWM_SLICE", 1.0),
        offdiag_weight: env("LEWM_GAMMA", 0.0),
        seed: 21,
        ..JepaTrainConfig::default()
    };
    eprintln!("[long] iso {} epochs {} lr {} tau {} slice {} gamma {}", cfg.iso_weight, cfg.epochs, cfg.lr, cfg.ema_tau, cfg.slice_weight, cfg.offdiag_weight);
    let t0 = std::time::Instant::now();
    let r = train_jepa(&data, cfg).unwrap();
    eprintln!("[long] {:.1} s wall", t0.elapsed().as_secs_f64());
    print_report("long", &r);
    if std::env::var("LEWM_SKIP_UNTRAINED").is_err() {
        let untrained = train_jepa(&data, JepaTrainConfig { epochs: 0, ..cfg }).unwrap();
        print_report("long-untrained", &untrained);
    }
    assert!(r.final_loss.total < r.initial_loss.total);
    if let Ok(out) = std::env::var("LEWM_OUT") {
        let out = std::path::PathBuf::from(out);
        std::fs::create_dir_all(&out).unwrap();
        r.encoder.save_safetensors(out.join("encoder.safetensors")).unwrap();
        r.predictor.save_safetensors(out.join("predictor.safetensors")).unwrap();
        let back = CandleVitEncoder::from_safetensors_path(out.join("encoder.safetensors")).unwrap();
        let f = data[0].frame_t.as_slice();
        for (x, y) in back.encode_checked(f).unwrap().iter().zip(r.encoder.encode_checked(f).unwrap().iter()) {
            assert!((x - y).abs() <= 1e-5);
        }
        eprintln!("[long] wrote {}", out.display());
    }
}


