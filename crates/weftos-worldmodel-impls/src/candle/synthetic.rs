//! SYNTHETIC moving-blob frames for the JEPA tests and the bench (`feature = "candle"`).
//!
//! **Fixture only.** Nothing trained on these frames says anything about real sensors; every
//! number produced from them is labelled SYNTHETIC in reports.
//!
//! # Scene
//!
//! Each frame is `S × S` HWC RGB u8 (the [`crate::candle::vit`] layout). A per-day background
//! grey level (`40 + 3·day mod 12`) gives held-out days a small domain shift; it is kept small
//! because a large one becomes the dominant latent factor (see `candle::jepa`). On top of it:
//!
//! - **blob 0** (red disc) is the *controlled* object. Per step it moves
//!   `action · action_step_px` pixels horizontally and a fixed per-session drift vertically.
//! - **blob 1** (green disc, present in half the sessions) is a *distractor* with its own
//!   constant per-session velocity, independent of the action.
//!
//! Positions wrap around the frame (torus), so blobs never leave the scene. Each pixel gets
//! uniform noise in `[-noise, noise]` after rendering.
//!
//! # Actions
//!
//! The scalar action is one of five levels `{-1, -0.5, 0, 0.5, 1}`, drawn per step.
//! [`action_code`] maps it to the 32-d [`Action`] code the AdaLN predictor consumes
//! (`code[0] = a`, a one-hot of the level in `code[1..6]`). The scalar is what
//! [`crate::eval_harness::Transition::action`] carries.
//!
//! Everything is a pure function of [`SyntheticConfig::seed`].

use weftos_worldmodel_core::Action;

use super::jepa::FrameTransition;
use super::SplitMix64;

/// Discrete action levels.
pub const ACTION_LEVELS: [f32; 5] = [-1.0, -0.5, 0.0, 0.5, 1.0];

/// Generator settings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SyntheticConfig {
    /// Frame side length in pixels.
    pub image_size: usize,
    /// Number of distinct days (`day` tags `0..days`).
    pub days: u16,
    /// Sessions recorded per day.
    pub sessions_per_day: u16,
    /// Transitions per session (consecutive frames).
    pub steps_per_session: usize,
    /// Blob radius in pixels.
    pub blob_radius: f32,
    /// Horizontal pixels moved per unit of action.
    pub action_step_px: f32,
    /// Per-pixel uniform noise amplitude.
    pub noise: u8,
    /// Seed for every random choice.
    pub seed: u64,
}

impl Default for SyntheticConfig {
    fn default() -> Self {
        Self {
            image_size: 64,
            days: 4,
            sessions_per_day: 2,
            steps_per_session: 32,
            blob_radius: 7.0,
            action_step_px: 6.0,
            noise: 8,
            seed: 0,
        }
    }
}

/// 32-d action code for a scalar action level (`code[0] = a`, one-hot of the nearest level
/// in `code[1..6]`). Out-of-range scalars clamp to the nearest level.
pub fn action_code(a: f32) -> Action {
    let mut act = Action::null();
    act.code[0] = a;
    act.code[1 + level_index(a)] = 1.0;
    act
}

fn level_index(a: f32) -> usize {
    ((a.clamp(-1.0, 1.0) + 1.0) * 2.0).round() as usize
}

#[derive(Debug, Clone, Copy)]
struct Blob {
    x: f32,
    y: f32,
    color: [u8; 3],
}

struct Scene {
    size: usize,
    background: u8,
    radius: f32,
    blobs: Vec<Blob>,
}

impl Scene {
    fn render(&self, rng: &mut SplitMix64, noise: u8) -> Vec<u8> {
        let s = self.size;
        let mut out = vec![self.background; 3 * s * s];
        let r2 = self.radius * self.radius;
        for b in &self.blobs {
            for py in 0..s {
                let dy = torus_delta(py as f32 + 0.5, b.y, s as f32);
                if dy * dy > r2 {
                    continue;
                }
                for px in 0..s {
                    let dx = torus_delta(px as f32 + 0.5, b.x, s as f32);
                    if dx * dx + dy * dy <= r2 {
                        out[3 * (py * s + px)..3 * (py * s + px) + 3].copy_from_slice(&b.color);
                    }
                }
            }
        }
        if noise > 0 {
            let span = 2 * noise as u64 + 1;
            for v in out.iter_mut() {
                let n = (rng.next_u64() % span) as i16 - noise as i16;
                *v = (*v as i16 + n).clamp(0, 255) as u8;
            }
        }
        out
    }
}

/// Signed shortest distance from `b` to `a` on a ring of length `len`.
fn torus_delta(a: f32, b: f32, len: f32) -> f32 {
    let d = (a - b).rem_euclid(len);
    if d > len / 2.0 {
        d - len
    } else {
        d
    }
}

fn wrap(v: f32, len: f32) -> f32 {
    v.rem_euclid(len)
}

/// Generate `days · sessions_per_day · steps_per_session` transitions, session by session,
/// in day order. Each transition's `frame_tp1` is the next transition's `frame_t` within a
/// session (re-rendered with fresh noise).
pub fn generate(cfg: &SyntheticConfig) -> Vec<FrameTransition> {
    let mut rng = SplitMix64::new(cfg.seed ^ 0xB10B_5EED);
    let s = cfg.image_size as f32;
    let mut out = Vec::with_capacity(cfg.days as usize * cfg.sessions_per_day as usize * cfg.steps_per_session);
    for day in 0..cfg.days {
        let background = 40 + ((day as u32 * 3) % 12) as u8;
        for session in 0..cfg.sessions_per_day {
            let pos = |rng: &mut SplitMix64| (rng.uniform() * 0.5 + 0.5) * s;
            let mut blobs = vec![Blob { x: pos(&mut rng), y: pos(&mut rng), color: [220, 60, 60] }];
            let drift_y = rng.uniform() * 1.5;
            let distractor = rng.next_u64().is_multiple_of(2);
            let (vx, vy) = (rng.uniform() * 3.0, rng.uniform() * 3.0);
            if distractor {
                blobs.push(Blob { x: pos(&mut rng), y: pos(&mut rng), color: [60, 200, 80] });
            }
            let mut scene = Scene { size: cfg.image_size, background, radius: cfg.blob_radius, blobs };
            let mut frame = scene.render(&mut rng, cfg.noise);
            for _ in 0..cfg.steps_per_session {
                let a = ACTION_LEVELS[(rng.next_u64() % ACTION_LEVELS.len() as u64) as usize];
                let b0 = &mut scene.blobs[0];
                b0.x = wrap(b0.x + a * cfg.action_step_px, s);
                b0.y = wrap(b0.y + drift_y, s);
                if let Some(b1) = scene.blobs.get_mut(1) {
                    b1.x = wrap(b1.x + vx, s);
                    b1.y = wrap(b1.y + vy, s);
                }
                let next = scene.render(&mut rng, cfg.noise);
                out.push(FrameTransition {
                    frame_t: frame,
                    action: action_code(a),
                    action_scalar: a,
                    frame_tp1: next.clone(),
                    day,
                    session,
                });
                frame = next;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn small() -> SyntheticConfig {
        SyntheticConfig { image_size: 16, days: 2, sessions_per_day: 2, steps_per_session: 5, blob_radius: 3.0, action_step_px: 2.0, noise: 4, seed: 9 }
    }

    #[test]
    fn deterministic_shapes_and_tags() {
        let cfg = small();
        let a = generate(&cfg);
        assert_eq!(a.len(), 2 * 2 * 5);
        assert_eq!(a, generate(&cfg));
        assert_ne!(a, generate(&SyntheticConfig { seed: 10, ..cfg }));
        for t in &a {
            assert_eq!(t.frame_t.len(), 3 * 16 * 16);
            assert_eq!(t.frame_tp1.len(), 3 * 16 * 16);
            assert!(ACTION_LEVELS.contains(&t.action_scalar));
            assert_eq!(t.action, action_code(t.action_scalar));
        }
        assert_eq!(a.iter().map(|t| t.day).max(), Some(1));
        assert_eq!(a.iter().filter(|t| t.day == 0 && t.session == 1).count(), 5);
    }

    #[test]
    fn controlled_blob_moves_with_the_action() {
        let cfg = SyntheticConfig { noise: 0, sessions_per_day: 6, ..small() };
        // (red pixel count, red column centroid).
        let red = |f: &[u8]| {
            let (mut sum, mut n) = (0.0f32, 0usize);
            for (i, px) in f.chunks(3).enumerate() {
                if px == [220, 60, 60] {
                    sum += (i % 16) as f32;
                    n += 1;
                }
            }
            (n, sum / n.max(1) as f32)
        };
        let green = |f: &[u8]| f.chunks(3).any(|px| px == [60, 200, 80]);
        let mut checked = 0;
        for t in &generate(&cfg) {
            let ((_, x0), (_, x1)) = (red(&t.frame_t), red(&t.frame_tp1));
            // Skip sessions with the distractor (it can occlude) and discs split by the wrap.
            let split = |f: &[u8]| {
                let cols: Vec<usize> = f.chunks(3).enumerate().filter(|(_, px)| *px == [220, 60, 60]).map(|(i, _)| i % 16).collect();
                cols.iter().max().unwrap_or(&0) - cols.iter().min().unwrap_or(&0) > 7
            };
            if green(&t.frame_t) || green(&t.frame_tp1) || split(&t.frame_t) || split(&t.frame_tp1) {
                continue;
            }
            assert!((x1 - x0 - t.action_scalar * cfg.action_step_px).abs() < 0.75, "a {} dx {}", t.action_scalar, x1 - x0);
            checked += usize::from(t.action_scalar != 0.0);
        }
        assert!(checked >= 5, "only {checked} unoccluded moving transitions");
    }

    #[test]
    fn action_code_layout() {
        let c = action_code(0.5);
        assert_eq!(c.code[0], 0.5);
        assert_eq!(c.code[1..6], [0.0, 0.0, 0.0, 1.0, 0.0]);
        assert_eq!(action_code(-3.0).code[1], 1.0);
        assert!(c.code[6..].iter().all(|v| *v == 0.0));
    }
}
