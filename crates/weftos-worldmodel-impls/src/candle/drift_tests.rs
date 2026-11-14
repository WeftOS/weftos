//! Per-op Metal vs CPU drift (lane P, `--features metal`). Every op runs on identical host
//! inputs on both devices; where cheap, an f64 reference in plain Rust gives the exact value,
//! so the table also says which backend is closer to it. Data is SYNTHETIC. Bounds are loose
//! sanity checks; the numbers are the point (`-- --nocapture`, grep `DRIFT|`).

use candle_core::{Device, Module, Tensor, D};
use candle_nn::{AdamW, Linear, Optimizer, ParamsAdamW};
use weftos_worldmodel_core::LATENT_DIM;

use super::adaln::NORM_STATS;
use super::isotropy::{isotropy_penalty_in, random_directions, sliced_penalty_in, vicreg_terms_in, CORR_EPS, ISO_STD_EPS};
use super::numerics::{pool, Red, ReduceMode};
use super::synthetic::{generate, SyntheticConfig};
use super::vit::{patchify, VitNet, VitTinyConfig, VIT_LN_EPS};
use super::{layer_norm, AdaLnConfig, AdaLnPredictor, SplitMix64, CONFIG_TENSOR};

pub(crate) const SEEDS: [u64; 3] = [21, 22, 23];

pub(crate) fn metal() -> Device {
    Device::new_metal(0).expect("the `metal` feature needs a Metal GPU (macOS)")
}

pub(crate) fn host(t: &Tensor) -> Vec<f32> {
    t.to_device(&Device::Cpu).unwrap().flatten_all().unwrap().to_vec1().unwrap()
}

fn uniform(n: usize, seed: u64, scale: f32, offset: f32) -> Vec<f32> {
    let mut r = SplitMix64::new(seed);
    (0..n).map(|_| r.uniform() * scale + offset).collect()
}

fn t(v: &[f32], shape: &[usize], dev: &Device) -> Tensor {
    Tensor::from_slice(v, shape, dev).unwrap()
}

/// `(max |a − b|, that / max |a|)`.
pub(crate) fn drift(a: &[f32], b: &[f32]) -> (f64, f64) {
    let abs = a.iter().zip(b).map(|(x, y)| (*x as f64 - *y as f64).abs()).fold(0.0, f64::max);
    let scale = a.iter().map(|x| (*x as f64).abs()).fold(0.0, f64::max).max(1e-30);
    (abs, abs / scale)
}

fn exact_rel(a: &[f32], exact: &[f64]) -> f64 {
    let abs = a.iter().zip(exact).map(|(x, e)| (*x as f64 - e).abs()).fold(0.0, f64::max);
    abs / exact.iter().map(|e| e.abs()).fold(0.0, f64::max).max(1e-30)
}

/// One table row: CPU vs Metal, and each vs the f64 reference when there is one.
fn row(op: &str, seed: u64, cpu: &[f32], gpu: &[f32], exact: Option<&[f64]>) -> f64 {
    let (abs, rel) = drift(cpu, gpu);
    let ex = exact.map(|e| format!("cpu-exact {:.2e} | metal-exact {:.2e}", exact_rel(cpu, e), exact_rel(gpu, e))).unwrap_or_default();
    eprintln!("DRIFT|{op:<28}|seed {seed}|abs {abs:.2e}|rel {rel:.2e}|{ex}");
    assert!(cpu.iter().chain(gpu).all(|v| v.is_finite()), "{op}: non-finite");
    rel
}

fn both(f: impl Fn(&Device) -> Tensor) -> (Vec<f32>, Vec<f32>) {
    (host(&f(&Device::Cpu)), host(&f(&metal())))
}

/// `x (rows, k) · w (n, k)ᵀ + b` in f64.
fn matmul_ref(x: &[f32], k: usize, w: &[f32], b: Option<&[f32]>) -> Vec<f64> {
    let (rows, n) = (x.len() / k, w.len() / k);
    let mut out = vec![0.0f64; rows * n];
    for r in 0..rows {
        for c in 0..n {
            let s: f64 = (0..k).map(|i| x[r * k + i] as f64 * w[c * k + i] as f64).sum();
            out[r * n + c] = s + b.map_or(0.0, |b| b[c] as f64);
        }
    }
    out
}

fn layer_norm_ref(x: &[f32], e: usize, eps: f64) -> Vec<f64> {
    x.chunks(e)
        .flat_map(|r| {
            let m = r.iter().map(|v| *v as f64).sum::<f64>() / e as f64;
            let v = r.iter().map(|x| (*x as f64 - m).powi(2)).sum::<f64>() / e as f64;
            r.iter().map(move |x| (*x as f64 - m) / (v + eps).sqrt())
        })
        .collect()
}

fn softmax_ref(x: &[f32], l: usize) -> Vec<f64> {
    x.chunks(l)
        .flat_map(|r| {
            let m = r.iter().fold(f64::MIN, |a, v| a.max(*v as f64));
            let s: f64 = r.iter().map(|v| (*v as f64 - m).exp()).sum();
            r.iter().map(move |v| (*v as f64 - m).exp() / s)
        })
        .collect()
}

/// `erf` by its Maclaurin series (inputs `|x| ≤ 3`; about 1e-13 absolute there).
fn erf_ref(x: f64) -> f64 {
    let (mut term, mut sum) = (x, x);
    for n in 1..80 {
        term *= -x * x / n as f64;
        sum += term / (2 * n + 1) as f64;
    }
    sum * 2.0 / std::f64::consts::PI.sqrt()
}

/// Default-config regulariser `R + S + H + 0.04·K` (correlation form) in f64.
fn reg_ref(z: &[f32], b: usize, dirs: &[f32], k: usize) -> f64 {
    let d = LATENT_DIM;
    let mu: Vec<f64> = (0..d).map(|i| (0..b).map(|r| z[r * d + i] as f64).sum::<f64>() / b as f64).collect();
    let zc: Vec<f64> = (0..b * d).map(|j| z[j] as f64 - mu[j % d]).collect();
    let var: Vec<f64> = (0..d).map(|i| (0..b).map(|r| zc[r * d + i].powi(2)).sum::<f64>() / (b as f64 - 1.0)).collect();
    let std: Vec<f64> = var.iter().map(|v| (v + ISO_STD_EPS).sqrt()).collect();
    let r = (mu.iter().map(|m| m * m).sum::<f64>() + std.iter().map(|s| (s - 1.0).powi(2)).sum::<f64>()) / d as f64;
    let s = (0..k)
        .map(|c| {
            let v = (0..b).map(|row| (0..d).map(|i| zc[row * d + i] * dirs[i * k + c] as f64).sum::<f64>().powi(2)).sum::<f64>() / (b as f64 - 1.0);
            ((v + ISO_STD_EPS).sqrt() - 1.0).powi(2)
        })
        .sum::<f64>()
        / k as f64;
    let h = std.iter().map(|s| (1.0 - s).max(0.0)).sum::<f64>() / d as f64;
    let zm: Vec<f64> = (0..b * d).map(|j| zc[j] / (var[j % d] + CORR_EPS).sqrt()).collect();
    let mut off = 0.0;
    for i in 0..d {
        for j in 0..d {
            if i != j {
                off += ((0..b).map(|row| zm[row * d + i] * zm[row * d + j]).sum::<f64>() / (b as f64 - 1.0)).powi(2);
            }
        }
    }
    r + s + h + 0.04 * off / d as f64
}

/// The same regulariser through the trainer's own functions.
fn reg(z: &Tensor, dirs: &Tensor, mode: ReduceMode) -> Tensor {
    let (hinge, cov) = vicreg_terms_in(z, true, mode).unwrap();
    let r = (isotropy_penalty_in(z, 0.0, mode).unwrap() + sliced_penalty_in(z, dirs, mode).unwrap()).unwrap();
    ((r + hinge).unwrap() + (cov * 0.04).unwrap()).unwrap()
}

pub(crate) fn frames(seed: u64, n: usize) -> Vec<super::jepa::FrameTransition> {
    let mut v = generate(&SyntheticConfig { days: 1, sessions_per_day: 1, steps_per_session: n, seed, ..SyntheticConfig::default() });
    v.truncate(n);
    v
}

#[test]
fn per_op_forward_drift_cpu_vs_metal() {
    let cfg = VitTinyConfig::default();
    let (e, p) = (cfg.embed_dim, 3 * cfg.patch_size * cfg.patch_size);
    for seed in SEEDS {
        let data = frames(seed, 32);
        let refs: Vec<&[u8]> = data.iter().map(|t| t.frame_t.as_slice()).collect();
        let patches = host(&patchify(&cfg, &refs, &Device::Cpu).unwrap());
        let (w, b) = (uniform(e * p, seed, 1.0 / (p as f32).sqrt(), 0.0), uniform(e, seed ^ 1, 0.02, 0.0));
        let (c, g) = both(|d| Linear::new(t(&w, &[e, p], d), Some(t(&b, &[e], d))).forward(&patchify(&cfg, &refs, d).unwrap()).unwrap());
        assert!(row("patch embed (unfold+matmul)", seed, &c, &g, Some(&matmul_ref(&patches, p, &w, Some(&b)))) < 1e-3);

        let x = uniform(2048 * e, seed ^ 2, 2.0, 0.3);
        let ln = layer_norm_ref(&x, e, VIT_LN_EPS);
        let (c, g) = both(|d| layer_norm(&t(&x, &[2048, e], d), VIT_LN_EPS).unwrap());
        assert!(row("LayerNorm (composed)", seed, &c, &g, Some(&ln)) < 1e-3);
        let (ones, zeros) = (vec![1.0f32; e], vec![0.0f32; e]);
        let (c, g) = both(|d| candle_nn::ops::layer_norm(&t(&x, &[2048, e], d), &t(&ones, &[e], d), &t(&zeros, &[e], d), VIT_LN_EPS as f32).unwrap());
        row("LayerNorm (fused, inference)", seed, &c, &g, Some(&ln));

        let shape = [32, 3, 64, 64];
        let logits = uniform(32 * 3 * 64 * 64, seed ^ 3, 4.0, 0.0);
        let sm = softmax_ref(&logits, 64);
        let (c, g) = both(|d| candle_nn::ops::softmax(&t(&logits, &shape, d), D::Minus1).unwrap());
        assert!(row("softmax (composed)", seed, &c, &g, Some(&sm)) < 1e-3);
        let (c, g) = both(|d| candle_nn::ops::softmax_last_dim(&t(&logits, &shape, d)).unwrap());
        row("softmax (fused, inference)", seed, &c, &g, Some(&sm));

        let x = uniform(2048 * 4 * e, seed ^ 4, 4.0, 0.0);
        let gelu: Vec<f64> = x.iter().map(|v| *v as f64 * (1.0 + erf_ref(*v as f64 / 2f64.sqrt())) / 2.0).collect();
        let (c, g) = both(|d| t(&x, &[2048, 4 * e], d).gelu_erf().unwrap());
        assert!(row("GELU (erf)", seed, &c, &g, Some(&gelu)) < 1e-3);

        let (q, k) = (uniform(32 * 3 * 64 * 64, seed ^ 5, 1.0, 0.0), uniform(32 * 3 * 64 * 64, seed ^ 6, 1.0, 0.0));
        let att: Vec<f64> = (0..96).flat_map(|h| matmul_ref(&q[h * 4096..(h + 1) * 4096], 64, &k[h * 4096..(h + 1) * 4096], None)).collect();
        let (c, g) = both(|d| t(&q, &shape, d).matmul(&t(&k, &shape, d).t().unwrap().contiguous().unwrap()).unwrap());
        assert!(row("attention q·kᵀ (k = 64)", seed, &c, &g, Some(&att)) < 1e-3);
        let (h, w2) = (uniform(512 * 4 * e, seed ^ 7, 1.0, 0.0), uniform(e * 4 * e, seed ^ 8, 1.0 / ((4 * e) as f32).sqrt(), 0.0));
        let (c, g) = both(|d| t(&h, &[512, 4 * e], d).matmul(&t(&w2, &[e, 4 * e], d).t().unwrap()).unwrap());
        assert!(row("MLP fc2 matmul (k = 768)", seed, &c, &g, Some(&matmul_ref(&h, 4 * e, &w2, None))) < 1e-3);

        let x = uniform(32 * 64 * e, seed ^ 9, 1.5, 0.2);
        let pooled: Vec<f64> = (0..32 * e).map(|j| (0..64).map(|n| x[(j / e) * 64 * e + n * e + j % e] as f64).sum::<f64>() / 64.0).collect();
        for (name, mode) in [("mean-pool (native)", ReduceMode::Native), ("mean-pool (pairwise)", ReduceMode::Pairwise)] {
            let (c, g) = both(|d| pool(&t(&x, &[32, 64, e], d), mode).unwrap());
            assert!(row(name, seed, &c, &g, Some(&pooled)) < 1e-3);
        }

        let z = uniform(32 * LATENT_DIM, seed ^ 10, 1.7, 0.1);
        let dirs = host(&random_directions(&mut SplitMix64::new(seed), LATENT_DIM, 64, &Device::Cpu).unwrap());
        let exact = [reg_ref(&z, 32, &dirs, 64)];
        for (name, mode) in [("regulariser (native)", ReduceMode::Native), ("regulariser (pairwise)", ReduceMode::Pairwise)] {
            let (c, g) = both(|d| reg(&t(&z, &[32, LATENT_DIM], d), &t(&dirs, &[LATENT_DIM, 64], d), mode));
            assert!(row(name, seed, &c, &g, Some(&exact)) < 1e-3);
        }
        let (c, g) = both(|d| Red(ReduceMode::Pairwise).mean_all(&t(&z, &[32, LATENT_DIM], d).sqr().unwrap()).unwrap());
        row("MSE-style mean_all (pairwise)", seed, &c, &g, None);
    }
}

/// Trainable `(name, var)` of a varmap, sorted.
pub(crate) fn named_vars(varmap: &candle_nn::VarMap) -> Vec<(String, candle_core::Var)> {
    let data = varmap.data().lock().unwrap();
    let mut v: Vec<(String, candle_core::Var)> =
        data.iter().filter(|(k, _)| k.as_str() != CONFIG_TENSOR && !NORM_STATS.contains(&k.as_str())).map(|(k, v)| (k.clone(), v.clone())).collect();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

struct Pass {
    outputs: [Vec<f32>; 3],
    grads: Vec<(String, Vec<f32>)>,
    deltas: Vec<f32>,
}

/// Default ViT + AdaLN on `dev`: inference and train-mode encodes, AdaLN forward with
/// non-zero heads, one backward pass of `MSE + regulariser`, one AdamW step.
fn model_pass(seed: u64, dev: &Device) -> Pass {
    let cfg = VitTinyConfig::default();
    let vit = VitNet::build(&cfg, seed, dev).unwrap();
    let pred = AdaLnPredictor::new_random_on(AdaLnConfig::default(), seed ^ 0xADA1_17E5, dev).unwrap().net.unwrap();
    for (name, var) in named_vars(&pred.varmap) {
        if name.contains("adaln.") || name.starts_with("final.out.") {
            let n = var.elem_count();
            super::assign(&var, &Tensor::from_vec(uniform(n, seed ^ n as u64, 0.05, 0.0), var.dims(), &Device::Cpu).unwrap()).unwrap();
        }
    }
    let data = frames(seed, 8);
    let (ft, ftp1): (Vec<&[u8]>, Vec<&[u8]>) = data.iter().map(|t| (t.frame_t.as_slice(), t.frame_tp1.as_slice())).unzip();
    let a = super::adaln::stack_rows(data.iter().map(|t| t.action.code), 8, dev).unwrap();
    let inference = host(&vit.forward(&cfg, &ft, false).unwrap());
    let z = vit.forward(&cfg, &ft, true).unwrap();
    let target = vit.forward(&cfg, &ftp1, false).unwrap().detach();
    let zhat = pred.forward(&z, &a).unwrap();
    let dirs = random_directions(&mut SplitMix64::new(seed), LATENT_DIM, 64, dev).unwrap();
    let loss = ((&zhat - target).unwrap().sqr().unwrap().mean_all().unwrap() + reg(&z, &dirs, ReduceMode::Native)).unwrap();
    let grads = loss.backward().unwrap();
    let vars: Vec<(String, candle_core::Var)> = named_vars(&vit.varmap).into_iter().chain(named_vars(&pred.varmap)).collect();
    let g = vars.iter().filter_map(|(n, v)| grads.get(v.as_tensor()).map(|g| (n.clone(), host(g)))).collect();
    let before: Vec<f32> = vars.iter().flat_map(|(_, v)| host(v.as_tensor())).collect();
    let mut opt = AdamW::new(vars.iter().map(|(_, v)| v.clone()).collect(), ParamsAdamW { lr: 1e-3, weight_decay: 0.01, ..ParamsAdamW::default() }).unwrap();
    opt.step(&grads).unwrap();
    let after: Vec<f32> = vars.iter().flat_map(|(_, v)| host(v.as_tensor())).collect();
    Pass { outputs: [inference, host(&z), host(&zhat)], grads: g, deltas: after.iter().zip(&before).map(|(a, b)| a - b).collect() }
}

#[test]
fn model_forward_backward_and_adamw_drift_cpu_vs_metal() {
    for seed in SEEDS {
        let (c, g) = (model_pass(seed, &Device::Cpu), model_pass(seed, &metal()));
        for (i, name) in ["full ViT forward (inference)", "full ViT forward (train)", "AdaLN forward"].iter().enumerate() {
            assert!(row(name, seed, &c.outputs[i], &g.outputs[i], None) < 1e-3);
        }
        let (mut worst, mut num, mut den) = ((0.0f64, String::new()), 0.0f64, 0.0f64);
        for ((name, a), (_, b)) in c.grads.iter().zip(&g.grads) {
            let (_, rel) = drift(a, b);
            if rel > worst.0 && a.iter().any(|v| *v != 0.0) {
                worst = (rel, name.clone());
            }
            num += a.iter().zip(b).map(|(x, y)| (*x as f64 - *y as f64).powi(2)).sum::<f64>();
            den += a.iter().map(|x| (*x as f64).powi(2)).sum::<f64>();
        }
        let l2 = (num / den.max(1e-30)).sqrt();
        eprintln!("DRIFT|backward grads (L2, all)       |seed {seed}|rel {l2:.2e}|worst var {} max-rel {:.2e}", worst.1, worst.0);
        assert!(l2 < 1e-2, "gradient L2 drift {l2}");
        let lr = 1e-3f64;
        let (abs, _) = drift(&c.deltas, &g.deltas);
        let flipped = c.deltas.iter().zip(&g.deltas).filter(|(a, b)| (**a as f64 - **b as f64).abs() > 0.5 * lr).count();
        eprintln!(
            "DRIFT|AdamW step (update)           |seed {seed}|max |Δupdate| / lr {:.2e}|elements off by > lr/2: {flipped} of {} ({:.3} %)",
            abs / lr,
            c.deltas.len(),
            100.0 * flipped as f64 / c.deltas.len() as f64
        );
        assert!(c.deltas.iter().chain(&g.deltas).all(|v| v.is_finite()));
    }
}
