//! Condition 4 (temporal straightening, 2026-11-14): unit tests and the SYNTHETIC research
//! measurements behind the change (`--nocapture` prints the `STRAIGHT|` rows the lane note
//! quotes). No recorded data exists yet (plan W2); every number here is synthetic.

extern crate std;
use alloc::vec::Vec;
use std::eprintln;

use super::*;
use crate::math_no_std::acosf;
use weftos_worldmodel_core::{zero_latent, GATE_PREDICTED_CURVATURE_MIN, LATENT_DIM};

const PI: f32 = core::f32::consts::PI;

struct Rng(u64);
impl Rng {
    /// Uniform in [-1, 1).
    fn u(&mut self) -> f32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        ((self.0 >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
    }
    /// Approximately standard normal (sum of 12 uniforms on [0, 1)).
    fn n(&mut self) -> f32 {
        (0..12).map(|_| 0.5 * (self.u() + 1.0)).sum::<f32>() - 6.0
    }
}

fn pair(z_t: Latent, z_hat: Latent, z_tp1: Latent) -> ProbePair {
    ProbePair { z_t, z_hat, z_tp1 }
}

fn e(i: usize, v: f32) -> Latent {
    let mut z = zero_latent();
    z[i] = v;
    z
}

// ── unit tests ──────────────────────────────────────────────────────────────

#[test]
fn acos_matches_known_values_on_both_build_paths() {
    for (x, want) in [(1.0, 0.0), (0.5, PI / 3.0), (0.0, PI / 2.0), (-0.5, 2.0 * PI / 3.0), (-1.0, PI)] {
        assert!((acosf(x) - want).abs() < 2e-4, "acos({x}) = {} want {want}", acosf(x));
    }
    assert!((acosf(1.5) - 0.0).abs() < 1e-6 && (acosf(-3.0) - PI).abs() < 1e-5);
}

#[test]
fn predicted_curvature_reference_values() {
    let z = e(0, 1.0);
    let z1 = e(0, 2.0);
    let w = |hat: Latent| predicted_curvature([pair(z, hat, z1)].iter());
    assert!((w(z1) - 1.0).abs() < 1e-4, "exact direction");
    assert!((w(e(0, 5.0)) - 1.0).abs() < 1e-4, "right direction, wrong length still 1");
    assert!((w(z) - 0.5).abs() < 1e-6, "copy-last is chance");
    assert!(w(e(0, 0.0)).abs() < 1e-4, "backwards is 0");
    let mut ortho = z;
    ortho[1] = 1.0;
    assert!((w(ortho) - 0.5).abs() < 1e-4, "orthogonal is chance");
    // Still scene predicted still: skipped, so the window carries no signal.
    assert_eq!(predicted_curvature([pair(z, z, z)].iter()), 1.0);
    // Predicted motion that did not happen: chance.
    assert!((predicted_curvature([pair(z, z1, z)].iter()) - 0.5).abs() < 1e-6);
    assert_eq!(predicted_curvature(core::iter::empty()), 1.0);
}

#[test]
fn predicted_curvature_ignores_window_order_and_chord_over_path_does_not() {
    let mut rng = Rng(5);
    let mut ps: Vec<ProbePair> = (0..16)
        .map(|t| {
            let z = e(0, t as f32);
            let mut z1 = e(0, t as f32 + 1.0);
            z1[1] = 0.3 * rng.u();
            pair(z, e(0, t as f32 + 1.0), z1)
        })
        .collect();
    let (a, c) = (predicted_curvature(ps.iter()), chord_over_path(ps.iter().map(|p| &p.z_tp1)));
    ps.reverse();
    ps.swap(2, 9);
    assert!((predicted_curvature(ps.iter()) - a).abs() < 1e-5);
    assert!((chord_over_path(ps.iter().map(|p| &p.z_tp1)) - c).abs() > 0.05);
}

#[test]
fn chord_over_path_keeps_the_legacy_numbers() {
    let line: Vec<Latent> = (0..16).map(|t| e(0, t as f32)).collect();
    assert!((chord_over_path(line.iter()) - 1.0).abs() < 1e-6);
    let bounce: Vec<Latent> = (0..12).map(|t| e(0, if t % 2 == 0 { 0.0 } else { 10.0 })).collect();
    assert!((chord_over_path(bounce.iter()) - 10.0 / 110.0).abs() < 1e-6);
    let still = [e(3, 1.0); 5];
    assert_eq!(chord_over_path(still.iter()), 1.0);
    assert_eq!(chord_over_path([e(0, 1.0)].iter()), 1.0);
}

#[test]
fn kind_floors_are_the_core_constants() {
    assert_eq!(StraighteningKind::default(), StraighteningKind::PredictedCurvature);
    assert_eq!(StraighteningKind::PredictedCurvature.floor(), GATE_PREDICTED_CURVATURE_MIN);
    assert_eq!(
        StraighteningKind::ChordOverPath.floor(),
        weftos_worldmodel_core::GATE_TEMPORAL_STRAIGHTEN_MIN
    );
}

/// The floor's first argument, checked empirically: a predictor with no directional information
/// passes the 0.75 floor on at most ~3.8 % of 16-transition windows when the latent is
/// effectively one-dimensional, and on none in 192 dimensions.
#[test]
fn a_chance_predictor_rarely_reaches_the_floor_on_a_sixteen_step_window() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let windows = 4000;
    let (mut pass_1d, mut pass_hd, mut mean_1d) = (0u32, 0u32, 0.0f32);
    for _ in 0..windows {
        let ps1: Vec<ProbePair> = (0..16)
            .map(|_| pair(zero_latent(), e(0, rng.u()), e(0, rng.u())))
            .collect();
        let ps: Vec<ProbePair> = (0..16)
            .map(|_| {
                let (mut h, mut z1) = (zero_latent(), zero_latent());
                for d in 0..LATENT_DIM {
                    h[d] = rng.n();
                    z1[d] = rng.n();
                }
                pair(zero_latent(), h, z1)
            })
            .collect();
        let s1 = predicted_curvature(ps1.iter());
        mean_1d += s1 / windows as f32;
        pass_1d += (s1 >= GATE_PREDICTED_CURVATURE_MIN) as u32;
        pass_hd += (predicted_curvature(ps.iter()) >= GATE_PREDICTED_CURVATURE_MIN) as u32;
    }
    let f1 = pass_1d as f32 / windows as f32;
    eprintln!("STRAIGHT|chance|1-D mean {mean_1d:.3} pass {f1:.4} (exact 0.0384)|192-D pass {pass_hd}/{windows}");
    assert!((mean_1d - 0.5).abs() < 0.02);
    assert!(f1 < 0.055, "1-D chance pass rate {f1}");
    assert_eq!(pass_hd, 0);
}

// ── research measurements (SYNTHETIC) ───────────────────────────────────────

/// Raw turning-angle curvature of a latent path (option E, not a gate kind): `1 − θ̄/π` over the
/// angles between consecutive displacements.
fn raw_curvature(path: &[Latent]) -> f32 {
    let (mut sum, mut n) = (0.0f32, 0u32);
    for w in path.windows(3) {
        let (mut a, mut b) = (zero_latent(), zero_latent());
        for d in 0..LATENT_DIM {
            a[d] = w[1][d] - w[0][d];
            b[d] = w[2][d] - w[1][d];
        }
        if let Some(th) = straighten::angle(&a, &b) {
            sum += th;
            n += 1;
        }
    }
    if n == 0 { 1.0 } else { 1.0 - sum / (n as f32 * PI) }
}

/// Mean and pass fraction of the three scores over every 16-transition window of `ps`.
fn window_stats(ps: &[ProbePair]) -> [(f32, f32); 3] {
    let mut acc = [(0.0f32, 0.0f32); 3];
    let wins: Vec<&[ProbePair]> = ps.windows(16).collect();
    for w in &wins {
        let path: Vec<Latent> = w.iter().map(|p| p.z_tp1).collect();
        let s = [chord_over_path(path.iter()), raw_curvature(&path), predicted_curvature(w.iter())];
        let floors = [0.50, GATE_PREDICTED_CURVATURE_MIN, GATE_PREDICTED_CURVATURE_MIN];
        for k in 0..3 {
            acc[k].0 += s[k] / wins.len() as f32;
            acc[k].1 += (s[k] >= floors[k]) as u32 as f32 / wins.len() as f32;
        }
    }
    acc
}

fn row(fixture: &str, model: &str, ps: &[ProbePair]) -> [(f32, f32); 3] {
    let s = window_stats(ps);
    eprintln!(
        "STRAIGHT|{fixture}|{model}|chord/path {:.3} pass {:.2}|raw curvature (E) {:.3} pass {:.2}|predicted curvature {:.3} pass {:.2}",
        s[0].0, s[0].1, s[1].0, s[1].1, s[2].0, s[2].1
    );
    s
}

/// Random linear embedding `R^k → R^192` (unit-variance columns).
fn embedding(k: usize, rng: &mut Rng) -> Vec<Latent> {
    (0..k)
        .map(|_| {
            let mut c = zero_latent();
            for v in c.iter_mut() {
                *v = rng.n() / (LATENT_DIM as f32).sqrt();
            }
            c
        })
        .collect()
}

fn embed(w: &[Latent], s: &[f32], noise: f32, rng: &mut Rng) -> Latent {
    let mut z = zero_latent();
    for (col, &x) in w.iter().zip(s) {
        for d in 0..LATENT_DIM {
            z[d] += col[d] * x;
        }
    }
    if noise > 0.0 {
        for v in z.iter_mut() {
            *v += noise * rng.n() / (LATENT_DIM as f32).sqrt();
        }
    }
    z
}

/// Rank-one collapse: the latent is a per-session constant plus isotropic jitter. Returns
/// `(copy-last pairs, mean-predictor pairs)`.
fn collapsed(sessions: usize, steps: usize, jitter: f32, rng: &mut Rng) -> (Vec<ProbePair>, Vec<ProbePair>) {
    let u = embedding(1, rng);
    let (mut copy, mut mean) = (Vec::new(), Vec::new());
    for _ in 0..sessions {
        let c = embed(&u, &[3.0 * rng.u()], 0.0, rng);
        let mut z = embed(&u, &[0.0], jitter, rng);
        for d in 0..LATENT_DIM {
            z[d] += c[d];
        }
        for _ in 0..steps {
            let mut z1 = embed(&u, &[0.0], jitter, rng);
            for d in 0..LATENT_DIM {
                z1[d] += c[d];
            }
            copy.push(pair(z, z, z1));
            mean.push(pair(z, c, z1));
            z = z1;
        }
    }
    (copy, mean)
}

/// An idealised walk at 10 Hz: 1.2 m/s, straight for 3 s, then a 90° turn over 1 s, with a
/// U-turn (180° over 1.5 s) every fourth turn; position noise 2 cm. Encoder (a) sees position
/// and velocity, encoder (b) position only. Good model: constant-velocity extrapolation of the
/// state the latent encodes (for (b) the velocity comes from the previous frame, so the model
/// needs one step of history).
#[test]
fn research_walking_trajectory_at_ten_hertz() {
    let mut rng = Rng(0xD1B5_4A32_D192_ED03);
    let (dt, speed, n) = (0.1f32, 1.2f32, 600usize);
    let (mut x, mut y, mut h) = (0.0f32, 0.0f32, 0.0f32);
    let mut states = Vec::with_capacity(n + 2);
    let mut turn_left = 0usize;
    let mut turn_rate = 0.0f32;
    let mut k = 0usize;
    for t in 0..n + 2 {
        if turn_left == 0 && t % 40 == 30 {
            k += 1;
            let (angle, steps) = if k.is_multiple_of(4) { (PI, 15usize) } else { (PI / 2.0, 10usize) };
            turn_left = steps;
            turn_rate = angle / steps as f32 * if k.is_multiple_of(2) { 1.0 } else { -1.0 };
        }
        if turn_left > 0 {
            h += turn_rate;
            turn_left -= 1;
        }
        let (vx, vy) = (speed * h.cos(), speed * h.sin());
        x += vx * dt;
        y += vy * dt;
        states.push([x, y, vx, vy]);
    }
    let noise = 0.02f32;
    let wa = embedding(4, &mut rng);
    let wb = embedding(2, &mut rng);
    let za: Vec<Latent> = states.iter().map(|s| embed(&wa, s, noise, &mut rng)).collect();
    let zb: Vec<Latent> = states.iter().map(|s| embed(&wb, &s[..2], noise, &mut rng)).collect();
    let (mut good_a, mut copy_a, mut good_b, mut stateless_b) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for t in 1..n + 1 {
        let s = states[t];
        let cv = [s[0] + s[2] * dt, s[1] + s[3] * dt, s[2], s[3]];
        good_a.push(pair(za[t], embed(&wa, &cv, 0.0, &mut rng), za[t + 1]));
        copy_a.push(pair(za[t], za[t], za[t + 1]));
        let mut hat = zb[t];
        for d in 0..LATENT_DIM {
            hat[d] += zb[t][d] - zb[t - 1][d];
        }
        good_b.push(pair(zb[t], hat, zb[t + 1]));
        // A stateless model on a position-only latent cannot know the heading.
        stateless_b.push(pair(zb[t], embed(&wb, &[s[0] + speed * dt, s[1]], 0.0, &mut rng), zb[t + 1]));
    }
    let ga = row("walk 10 Hz pos+vel", "constant velocity", &good_a);
    let ca = row("walk 10 Hz pos+vel", "copy-last", &copy_a);
    let gb = row("walk 10 Hz pos only", "previous-step velocity", &good_b);
    let sb = row("walk 10 Hz pos only", "stateless, fixed heading", &stateless_b);
    let (cc, cm) = collapsed(10, 60, 0.05, &mut rng);
    let cc = row("walk collapsed", "copy-last", &cc);
    let cm = row("walk collapsed", "mean predictor", &cm);
    // The new score separates the good model from copy-last and from the collapsed latent with
    // copy-last; the legacy score cannot see the model at all.
    assert!(ga[2].0 > GATE_PREDICTED_CURVATURE_MIN && ga[2].1 >= 0.9, "{ga:?}");
    assert!(ca[2].1 == 0.0 && (ca[2].0 - 0.5).abs() < 1e-3);
    assert!(gb[2].0 > ca[2].0 && sb[2].0 < ga[2].0);
    assert!(cc[2].1 == 0.0);
    assert_eq!(ga[0], ca[0], "legacy score is blind to the model");
    // Collapsed latent, mean predictor: the angle concentrates at 45°, on the floor.
    assert!((cm[2].0 - 0.75).abs() < 0.03, "{cm:?}");
}

/// A replica of the JEPA blob scene in state space (64 px torus, blob 0 moved `6·a` px per step
/// with `a` drawn per step from five levels, per-session vertical drift in ±1.5 px, a distractor
/// in half the sessions at ±3 px per axis). A perfect encoder that embeds positions linearly,
/// so wrap-around is a jump. Sessions are concatenated like the harness's held-out stream.
#[test]
fn research_blob_scene_with_wrap_around() {
    let mut rng = Rng(0xA076_1D64_78BD_642F);
    let w = embedding(4, &mut rng);
    let levels = [-1.0f32, -0.5, 0.0, 0.5, 1.0];
    let wrap = |v: f32| v.rem_euclid(64.0);
    let (mut oracle, mut action_only, mut copy) = (Vec::new(), Vec::new(), Vec::new());
    for _ in 0..30 {
        let drift = 1.5 * rng.u();
        let dist = rng.u() > 0.0;
        let (dvx, dvy) = (3.0 * rng.u(), 3.0 * rng.u());
        let mut s = [32.0 + 32.0 * rng.u(), 32.0 + 32.0 * rng.u(), 32.0 + 32.0 * rng.u(), 32.0 + 32.0 * rng.u()];
        let state = |s: &[f32; 4]| if dist { *s } else { [s[0], s[1], 0.0, 0.0] };
        let mut z = embed(&w, &state(&s), 0.5, &mut rng);
        for _ in 0..32 {
            let a = levels[((rng.u() + 1.0) * 2.5) as usize % 5];
            let next = [wrap(s[0] + 6.0 * a), wrap(s[1] + drift), wrap(s[2] + dvx), wrap(s[3] + dvy)];
            let z1 = embed(&w, &state(&next), 0.5, &mut rng);
            oracle.push(pair(z, embed(&w, &state(&next), 0.0, &mut rng), z1));
            let ao = [wrap(s[0] + 6.0 * a), s[1], s[2], s[3]];
            action_only.push(pair(z, embed(&w, &state(&ao), 0.0, &mut rng), z1));
            copy.push(pair(z, z, z1));
            s = next;
            z = z1;
        }
    }
    let o = row("blob replica", "oracle (action, drift, distractor)", &oracle);
    let a = row("blob replica", "action only", &action_only);
    let c = row("blob replica", "copy-last", &copy);
    let (cc, cm) = collapsed(30, 32, 0.5, &mut rng);
    let cc = row("blob replica collapsed", "copy-last", &cc);
    row("blob replica collapsed", "mean predictor", &cm);
    // A perfect encoder of this scene fails the legacy floor on most windows: actions are drawn
    // per step, so the path is a random walk, and wraps add jumps.
    assert!(o[0].1 < 0.5, "legacy pass fraction for a perfect encoder {:?}", o[0]);
    assert!(o[2].0 > 0.9 && o[2].1 > 0.95, "{o:?}");
    assert!(a[2].0 > c[2].0 && c[2].1 == 0.0 && cc[2].1 == 0.0);
}
