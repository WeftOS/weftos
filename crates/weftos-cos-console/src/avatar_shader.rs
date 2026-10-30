//! Phosphor-green port of the twigl geekest shader at
//! `https://twigl.app/?ol=true&ss=-P3XImU-dNzeIaNBQR_I` (mode 7).
//! The published source is the one-liner whose hue is `0.59`. Here that
//! hue is folded into a 256-entry green ramp, and the march parameters
//! move with the agent state: pace, reach, spin, inner pulse, and how
//! hard the light sits on the ramp (need, emotion, expectation).
//!
//! The pad reads mesh frames of at most 8192 bytes, so the field is
//! painted as short RGB565 tiles rather than one bitmap.

use crate::status::AgentState;

/// Compute resolution. The panel shows each sample as a 2×2 block.
pub const COMPUTE_W: i32 = 48;
/// Compute height, in samples.
pub const COMPUTE_H: i32 = 72;
/// Panel pixels per compute sample along each axis.
pub const SCALE: i32 = 2;
/// Field width in panel pixels.
pub const FIELD_W: i32 = COMPUTE_W * SCALE;
/// Field height in panel pixels.
pub const FIELD_H: i32 = COMPUTE_H * SCALE;
/// Rows per mesh tile. One `SceneOp` must encode under the pad's
/// chunk budget (`MAX_PUSH_CBOR` minus the Ops wrapper), so a strip
/// stays at 96×8 RGB565.
pub const TILE_H: i32 = 8;
/// Number of `TILE_H`-row strips that make up one field.
pub const TILES: usize = (FIELD_H / TILE_H) as usize;
/// Size of one full field in bytes: RGB565, 2 bytes per pixel.
pub const FIELD_BYTES: usize = (FIELD_W * FIELD_H * 2) as usize;

/// Where the field sits in the visual panel (the caption stays below it).
/// Panel column where the field starts.
pub const FIELD_X: i32 = 102;
/// Panel row where the field starts.
pub const FIELD_Y: i32 = 120;

const STOCK_STEPS: u32 = 70;

/// Parameters that stand in for the constants in the twigl source.
#[derive(Clone, Copy, Debug)]
pub struct Mood {
    /// Outer march. The source uses 70; the pad uses fewer so a frame
    /// finishes inside one host tick. The count itself is a mood.
    pub steps: u32,
    /// Divisor on the accumulated light. The source uses 40.
    pub gain: f32,
    /// `e * s - thresh` gate. The source uses 0.15.
    pub thresh: f32,
    /// Multiplier on the clock.
    pub time_scale: f32,
    /// Angular speed (`t * spin` on the polar angle). The source uses 0.3.
    pub spin: f32,
    /// Inner-loop phase speed. The source uses 0.9.
    pub pulse: f32,
    /// Ray step. The source uses 0.3.
    pub march: f32,
    /// Domain scale inside the log-polar map. The source uses 1.3.
    pub zoom: f32,
    /// Field of view (`FC / r * frame`). The source uses 0.4.
    pub frame: f32,
    /// Horizontal aim. The source uses 0.2.
    pub drift: f32,
    /// Vertical aim. The source uses -0.8.
    pub lift: f32,
    /// Inner loop stops doubling `s` here. The source runs to 1000.
    pub inner_cap: f32,
    /// `acc / (acc + knee)`. Smaller knee burns brighter.
    pub knee: f32,
}

/// Mood for an agent state. Idle is the closest to the published
/// constants. Listening opens the view and speeds the spin. Thinking
/// tightens the step and busies the interior. Speaking is the brightest
/// and the fastest. Blocked keeps the same green family, dim and slow.
pub fn mood_for(state: &AgentState) -> Mood {
    let stock = Mood {
        steps: 28,
        gain: 40.0,
        thresh: 0.15,
        time_scale: 0.35,
        spin: 0.30,
        pulse: 0.90,
        march: 0.30,
        zoom: 1.3,
        frame: 0.40,
        drift: 0.20,
        lift: -0.80,
        inner_cap: 128.0,
        knee: 0.70,
    };
    match state {
        AgentState::Idle => stock,
        AgentState::Listening => Mood {
            time_scale: 0.7,
            spin: 0.55,
            pulse: 1.25,
            march: 0.34,
            frame: 0.48,
            drift: 0.12,
            lift: -0.95,
            knee: 0.42,
            ..stock
        },
        AgentState::Thinking => Mood {
            steps: 32,
            time_scale: 1.05,
            spin: 0.16,
            pulse: 1.7,
            march: 0.22,
            zoom: 1.5,
            frame: 0.36,
            drift: 0.28,
            lift: -0.55,
            inner_cap: 160.0,
            knee: 0.50,
            ..stock
        },
        AgentState::Speaking => Mood {
            time_scale: 1.35,
            spin: 0.62,
            pulse: 1.3,
            march: 0.40,
            lift: -1.05,
            drift: 0.05,
            knee: 0.05,
            ..stock
        },
        // Same march depth as idle so the fractal keeps its shape.
        // A short inner cap saturates every pixel to one green.
        AgentState::Blocked(_) => Mood {
            time_scale: 0.14,
            spin: 0.08,
            pulse: 0.35,
            march: 0.26,
            drift: 0.34,
            lift: -0.62,
            knee: 2.20,
            ..stock
        },
    }
}

/// One of 256 greens as RGB565 (R in the top 5, G in the middle 6, B in
/// the bottom 5). Built once: each entry keeps G at least R and at least
/// B, index 0 is black, and all 256 words differ. A straight `g = i>>2`
/// plus a red/blue nudge collides at the dark end and walks out of green.
const SHADES: [u16; 256] = shade_table();

const fn shade_table() -> [u16; 256] {
    let mut out = [0u16; 256];
    let mut n = 0usize;
    let mut g = 0u16;
    while g < 64 {
        let room = if g > 31 { 31 } else { g };
        // Four probes inside the green square. Dark levels have fewer
        // distinct probes; the fill pass below tops the table up.
        let pairs = [(0u16, 0u16), (room, 0), (0, room), (room / 2, room / 2)];
        let mut s = 0usize;
        while s < 4 {
            let r = pairs[s].0;
            let b = pairs[s].1;
            let mut seen = false;
            let mut p = 0usize;
            while p < s {
                if pairs[p].0 == r && pairs[p].1 == b {
                    seen = true;
                }
                p += 1;
            }
            if !seen && n < 256 {
                out[n] = (r << 11) | (g << 5) | b;
                n += 1;
            }
            s += 1;
        }
        g += 1;
    }
    g = 63;
    while n < 256 {
        let mut r = 0u16;
        while r <= 31 && n < 256 {
            let mut b = 0u16;
            while b <= 31 && n < 256 {
                let word = (r << 11) | (g << 5) | b;
                let mut exists = false;
                let mut i = 0usize;
                while i < n {
                    if out[i] == word {
                        exists = true;
                        break;
                    }
                    i += 1;
                }
                if !exists {
                    out[n] = word;
                    n += 1;
                }
                b += 1;
            }
            r += 1;
        }
        g -= 1;
    }
    out
}

pub fn green_shade(index: u8) -> u16 {
    SHADES[index as usize]
}

/// Raster `mood` at `time_s` into a `FIELD_W × FIELD_H` RGB565 buffer,
/// little-endian, the order the pad's Raw565 blit expects.
pub fn render(mood: &Mood, time_s: f32, pixels: &mut [u8]) {
    assert_eq!(pixels.len(), FIELD_BYTES);
    let t = time_s * mood.time_scale;
    let steps = mood.steps.clamp(1, STOCK_STEPS);
    for cy in 0..COMPUTE_H {
        for cx in 0..COMPUTE_W {
            let lum = march(cx as f32, cy as f32, t, mood, steps);
            let index = (lum * 255.0).round().clamp(0.0, 255.0) as u8;
            let word = green_shade(index).to_le_bytes();
            for dy in 0..SCALE {
                for dx in 0..SCALE {
                    let x = cx * SCALE + dx;
                    let y = cy * SCALE + dy;
                    let i = ((y * FIELD_W + x) * 2) as usize;
                    pixels[i] = word[0];
                    pixels[i + 1] = word[1];
                }
            }
        }
    }
}

/// The twigl body. `q` starts at the origin and `q.zy--` drops y and z
/// by one before the march, matching the source's `for` initializer.
fn march(cx: f32, cy: f32, t: f32, mood: &Mood, steps: u32) -> f32 {
    let w = COMPUTE_W as f32;
    let h = COMPUTE_H as f32;
    // gl_FragCoord is pixel-centre, origin at the bottom.
    let fc_x = cx + 0.5;
    let fc_y = h - cy - 0.5;
    let d = [
        fc_x / w * mood.frame - mood.drift,
        fc_y / h * mood.frame - mood.lift,
        1.0,
    ];
    let mut q = [0.0_f32, -1.0, -1.0];
    let mut e = 0.0_f32;
    let mut ray = 0.0_f32;
    let mut acc = 0.0_f32;
    // `s` is read by the colour term before this step resets it, so it
    // carries the previous inner loop's doubled value (0 on the first step).
    let mut s = 0.0_f32;
    for _ in 0..steps {
        acc += (e * s - mood.thresh).min(1.0) / mood.gain;
        s = 4.0;
        q[0] += d[0] * e * ray * mood.march;
        q[1] += d[1] * e * ray * mood.march;
        q[2] += d[2] * e * ray * mood.march;
        let (px, py, pz) = (q[0], q[1], q[2]);
        ray = (px * mood.zoom).hypot(py * mood.zoom).hypot(pz * mood.zoom).max(1.0e-4);
        let mut p = [
            ray.ln(),
            (-pz / ray).clamp(-8.0, 8.0).exp2(),
            py.atan2(px) * s - t * mood.spin,
        ];
        p[1] -= 1.0;
        e = p[1];
        while s < mood.inner_cap {
            let dot = (p[0] * s).cos() * (p[0] * s + t * mood.pulse).sin()
                + (p[1] * s).cos() * (p[1] * s + t * mood.pulse).sin()
                + (p[2] * s).cos() * (p[0] * s + t * mood.pulse).sin();
            e += dot.sin() / s * 0.5;
            s += s;
        }
    }
    let acc = if acc.is_finite() { acc.max(0.0) } else { 0.0 };
    acc / (acc + mood.knee)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mean_green(px: &[u8]) -> f32 {
        let mut sum = 0u32;
        let n = (px.len() / 2) as u32;
        for chunk in px.chunks_exact(2) {
            let w = u16::from_le_bytes([chunk[0], chunk[1]]);
            sum += ((w >> 5) & 63) as u32;
        }
        sum as f32 / n as f32
    }

    #[test]
    fn palette_is_256_distinct_greens() {
        let mut seen = std::collections::BTreeSet::new();
        for i in 0..=255 {
            let w = green_shade(i);
            let r = w >> 11;
            let g = (w >> 5) & 63;
            let b = w & 31;
            assert!(g >= r && g >= b, "shade {i} leaves green: r={r} g={g} b={b}");
            assert!(seen.insert(w), "shade {i} repeats {w:#06x}");
        }
        assert_eq!(seen.len(), 256);
        assert_eq!(green_shade(0), 0, "the bottom of the ramp is black");
    }

    #[test]
    fn green_shade_ramps_up_from_a_black_index_zero() {
        let channels = |w: u16| (w >> 11, (w >> 5) & 63, w & 31);
        let (r0, g0, b0) = channels(green_shade(0));
        assert_eq!((r0, g0, b0), (0, 0, 0), "index 0 is the darkest shade");
        let mut prev_green = g0;
        for i in 1..SHADES.len() {
            let (r, g, b) = channels(green_shade(i as u8));
            assert!(g >= r && g >= b, "shade {i} leaves green: r={r} g={g} b={b}");
            assert!(g >= prev_green, "shade {i} green {g} dips below {prev_green}");
            assert!(r + g + b > 0, "shade {i} is as dark as index 0");
            prev_green = g;
        }
    }

    #[test]
    fn moods_move_the_published_constants() {
        let idle = mood_for(&AgentState::Idle);
        let listen = mood_for(&AgentState::Listening);
        let think = mood_for(&AgentState::Thinking);
        let speak = mood_for(&AgentState::Speaking);
        let blocked = mood_for(&AgentState::Blocked("wait".into()));
        assert!(idle.steps < STOCK_STEPS);
        assert!(speak.time_scale > idle.time_scale);
        assert!(listen.spin > idle.spin);
        assert!(think.pulse > idle.pulse);
        assert!(think.march < idle.march);
        assert!(blocked.time_scale < idle.time_scale);
        assert!(blocked.knee > speak.knee);
    }

    #[test]
    fn speaking_reads_brighter_than_idle_and_blocked_reads_dimmer() {
        let mut speak = vec![0u8; FIELD_BYTES];
        let mut idle = vec![0u8; FIELD_BYTES];
        let mut blocked = vec![0u8; FIELD_BYTES];
        render(&mood_for(&AgentState::Speaking), 3.2, &mut speak);
        render(&mood_for(&AgentState::Idle), 3.2, &mut idle);
        render(&mood_for(&AgentState::Blocked("wait".into())), 3.2, &mut blocked);
        let (s, i, b) = (mean_green(&speak), mean_green(&idle), mean_green(&blocked));
        assert!(s > i, "speaking {s:.2} should outshine idle {i:.2}");
        assert!(i > b, "idle {i:.2} should outshine blocked {b:.2}");
    }

    #[test]
    fn time_and_mood_change_the_field() {
        let idle = mood_for(&AgentState::Idle);
        let mut a = vec![0u8; FIELD_BYTES];
        let mut b = vec![0u8; FIELD_BYTES];
        render(&idle, 0.4, &mut a);
        render(&idle, 2.6, &mut b);
        assert_ne!(a, b, "the field is still");
        render(&mood_for(&AgentState::Thinking), 2.6, &mut a);
        assert_ne!(a, b, "thinking matches idle");
    }
}
