//! The avatar (nodes 100–199): a wireframe head of lines and rings in
//! the visual panel, with four animated states and a static amber
//! `Blocked` pose. Animation is driven by `tick(now_ms)`, which emits
//! scene tweens (stroke colour, position) and node updates; the v1
//! store snaps each tween on its next tick, v1.1 will interpolate the
//! same ops.

use std::collections::BTreeMap;

use weftos_leaf_scene::{Node, Rgba, SceneOp};

use crate::prim::{arc, circle_fill, circle_stroke, hidden, line, recolor, text, tween_pos, tween_stroke};
use crate::status::AgentState;
use crate::text::{center, truncate};
use crate::theme::{AMBER, AMBER_DIM, DIM, FAINT, GREEN, SMALL, VISUAL};

pub const HEAD_CX: i32 = 150;
pub const HEAD_CY: i32 = 200;
pub const HEAD_R: i32 = 60;
const MOUTH_Y: i32 = 230;
const BAR_XS: [i32; 5] = [134, 142, 150, 158, 166];
const ORBIT_R: f32 = 80.0;

pub const ID_HEAD: u32 = 100;
const ID_BROW_RING: u32 = 101; // 16 segments (8 near, 8 far)
const ID_LAT1: u32 = 117; // 12
const ID_LAT2: u32 = 129; // 8
const ID_PUPILS: [u32; 2] = [139, 140];
const ID_LIDS: [u32; 2] = [141, 142];
const ID_NOSE: [u32; 2] = [143, 144];
pub const ID_MOUTH: u32 = 145;
pub const ID_BARS: [u32; 5] = [146, 147, 148, 149, 150];
const ID_NECK: [u32; 2] = [151, 152];
const ID_SHOULDERS: [u32; 2] = [153, 154];
pub const ID_RINGS: [u32; 2] = [157, 158];
pub const ID_BRACKETS: [u32; 6] = [159, 160, 161, 162, 163, 164];
pub const ID_SCAN: u32 = 165;
pub const ID_DOTS: [u32; 3] = [166, 167, 168];
pub const ID_CAPTION: u32 = 169;
pub const ID_REASON: u32 = 170;
/// Upper and lower eyelid arcs, 4 segments each; `ID_EYES` names the
/// upper arcs' first segments.
const ID_EYE_UPPER: [u32; 2] = [171, 175];
const ID_EYE_LOWER: [u32; 2] = [179, 183];
pub const ID_EYES: [u32; 2] = ID_EYE_UPPER;
const EYE_SEGS: u32 = 4;

const BLINK_EVERY_MS: u32 = 3400;
const BLINK_MS: u32 = 140;
const BREATH_MS: u32 = 4000;
const PULSE_MS: u32 = 1200;
const SCAN_MS: u32 = 1800;
const ORBIT_MS: u32 = 2400;
const BAR_STEP_MS: u32 = 80;

#[derive(Debug, Clone)]
pub struct Avatar {
    state: AgentState,
    nodes: BTreeMap<u32, Node>,
    last_blink: u32,
    blink_until: Option<u32>,
    last_tick: u32,
    bar_step: u32,
    dot_pos: [(i32, i32); 3],
}

impl Default for Avatar {
    fn default() -> Self {
        Self::new()
    }
}

impl Avatar {
    pub fn new() -> Self {
        let mut a = Self {
            state: AgentState::Idle,
            nodes: BTreeMap::new(),
            last_blink: 0,
            blink_until: None,
            last_tick: 0,
            bar_step: 0,
            dot_pos: [(0, 0); 3],
        };
        a.nodes = a.build_nodes();
        a
    }

    pub fn state(&self) -> &AgentState {
        &self.state
    }

    pub fn node(&self, id: u32) -> Option<&Node> {
        self.nodes.get(&id)
    }

    /// Every avatar node, for a first draw or after a `Clear`.
    pub fn all_ops(&self) -> Vec<SceneOp> {
        self.nodes.values().cloned().map(SceneOp::Insert).collect()
    }

    /// Switch pose; returns the updates that re-dress every node.
    pub fn set_state(&mut self, state: AgentState) -> Vec<SceneOp> {
        self.state = state;
        self.blink_until = None;
        self.nodes = self.build_nodes();
        self.nodes.values().cloned().map(SceneOp::Update).collect()
    }

    /// `(main, dim, far)` strokes for the current pose.
    fn colors(&self) -> (Rgba, Rgba, Rgba) {
        if self.state.is_blocked() {
            (AMBER, AMBER_DIM, AMBER_DIM)
        } else {
            (GREEN, DIM, FAINT)
        }
    }

    fn build_nodes(&self) -> BTreeMap<u32, Node> {
        let (main, dim, far) = self.colors();
        let cx = HEAD_CX as f32;
        let listening = self.state == AgentState::Listening;
        let thinking = self.state == AgentState::Thinking;
        let speaking = self.state == AgentState::Speaking;
        let mut v: Vec<Node> = Vec::new();

        v.push(circle_stroke(ID_HEAD, HEAD_CX, HEAD_CY, HEAD_R, main));
        // Latitude rings of the cranium, each inside the head's chord
        // at its height; the near half is dim, the far half fainter.
        let rings = [
            (ID_BROW_RING, 168.0, 50.0, 10.0, 8u32),
            (ID_LAT1, 154.0, 38.0, 7.0, 6),
            (ID_LAT2, 145.0, 24.0, 4.0, 4),
        ];
        for (first, y, rx, ry, segs) in rings {
            v.extend(arc(first, cx, y, rx, ry, 0.0, 180.0, segs, dim));
            v.extend(arc(first + segs, cx, y, rx, ry, 180.0, 360.0, segs, far));
        }

        for (i, ex) in [126, 174].into_iter().enumerate() {
            let exf = ex as f32;
            v.extend(arc(ID_EYE_UPPER[i], exf, 190.0, 10.0, 6.0, 180.0, 360.0, EYE_SEGS, main));
            v.extend(arc(ID_EYE_LOWER[i], exf, 190.0, 10.0, 3.0, 20.0, 160.0, EYE_SEGS, dim));
            v.push(circle_fill(ID_PUPILS[i], ex, 189, 2, main));
            v.push(hidden(line(ID_LIDS[i], ex - 10, 190, ex + 10, 190, main)));
        }
        v.push(line(ID_NOSE[0], 150, 196, 143, 214, main));
        v.push(line(ID_NOSE[1], 143, 214, 157, 214, main));

        let mouth = line(ID_MOUTH, 134, MOUTH_Y, 166, MOUTH_Y, main);
        v.push(if speaking { hidden(mouth) } else { mouth });
        for (i, x) in BAR_XS.into_iter().enumerate() {
            let bar = line(ID_BARS[i], x, MOUTH_Y - 3, x, MOUTH_Y + 3, main);
            v.push(if speaking { bar } else { hidden(bar) });
        }

        v.push(line(ID_NECK[0], 138, 258, 134, 282, main));
        v.push(line(ID_NECK[1], 162, 258, 166, 282, main));
        v.push(line(ID_SHOULDERS[0], 134, 282, 44, 318, main));
        v.push(line(ID_SHOULDERS[1], 166, 282, 256, 318, main));

        for (i, r) in [74, 88].into_iter().enumerate() {
            let ring = circle_stroke(ID_RINGS[i], HEAD_CX, HEAD_CY, r, dim);
            v.push(if listening { ring } else { hidden(ring) });
        }
        let brackets = [
            line(ID_BRACKETS[0], 56, 176, 56, 224, main),
            line(ID_BRACKETS[1], 56, 176, 66, 176, main),
            line(ID_BRACKETS[2], 56, 224, 66, 224, main),
            line(ID_BRACKETS[3], 244, 176, 244, 224, main),
            line(ID_BRACKETS[4], 244, 176, 234, 176, main),
            line(ID_BRACKETS[5], 244, 224, 234, 224, main),
        ];
        v.extend(brackets.into_iter().map(|b| if listening { b } else { hidden(b) }));

        let scan = line(ID_SCAN, HEAD_CX - HEAD_R, HEAD_CY, HEAD_CX + HEAD_R, HEAD_CY, main);
        v.push(if thinking { scan } else { hidden(scan) });
        for (k, id) in ID_DOTS.into_iter().enumerate() {
            let (x, y) = orbit_point(0, k);
            let dot = circle_fill(id, x, y, 3, main);
            v.push(if thinking { dot } else { hidden(dot) });
        }

        let cols = SMALL.cols(VISUAL.w - 24);
        let caption = match &self.state {
            AgentState::Idle => "idle",
            AgentState::Listening => "listening",
            AgentState::Thinking => "thinking",
            AgentState::Speaking => "speaking",
            AgentState::Blocked(_) => "blocked",
        };
        let cap_color = if self.state.is_blocked() { AMBER } else { DIM };
        v.push(text(ID_CAPTION, &SMALL, VISUAL.x + 12, 352, center(&format!("- {caption} -"), cols), cap_color));
        let reason = match &self.state {
            AgentState::Blocked(r) => center(&truncate(r, cols), cols),
            _ => String::new(),
        };
        v.push(text(ID_REASON, &SMALL, VISUAL.x + 12, 368, reason, AMBER));

        v.into_iter().map(|n| (n.id.path_hash(), n)).collect()
    }

    /// Advance the animation to `now_ms`. Returns the ops for this
    /// frame (empty when nothing moved).
    pub fn tick(&mut self, now_ms: u32) -> Vec<SceneOp> {
        let dt = now_ms.saturating_sub(self.last_tick).clamp(1, 1000);
        self.last_tick = now_ms;
        let mut ops = Vec::new();
        match self.state.clone() {
            AgentState::Idle => {
                self.breathe(now_ms, dt, &mut ops);
                self.blink(now_ms, &mut ops);
            }
            AgentState::Listening => self.pulse_rings(now_ms, dt, &mut ops),
            AgentState::Thinking => {
                self.scan(now_ms, &mut ops);
                self.orbit(now_ms, dt, &mut ops);
            }
            AgentState::Speaking => self.mouth_bars(now_ms, &mut ops),
            AgentState::Blocked(_) => {}
        }
        ops
    }

    fn set_stroke(&mut self, id: u32, to: Rgba, dt: u32, ops: &mut Vec<SceneOp>) {
        let Some(n) = self.nodes.get_mut(&id) else { return };
        let from = n.style.stroke.unwrap_or(to);
        if from == to {
            return;
        }
        recolor(n, to);
        ops.push(tween_stroke(id, from, to, dt));
    }

    fn set_visible(&mut self, id: u32, visible: bool, ops: &mut Vec<SceneOp>) {
        if let Some(n) = self.nodes.get_mut(&id) {
            if n.style.visible != visible {
                n.style.visible = visible;
                ops.push(SceneOp::Update(n.clone()));
            }
        }
    }

    /// Slow breathing: the head outline eases between bright and a
    /// half-dim green over four seconds.
    fn breathe(&mut self, now_ms: u32, dt: u32, ops: &mut Vec<SceneOp>) {
        let s = half_sine(now_ms % BREATH_MS, BREATH_MS);
        let to = GREEN.lerp_q8(&DIM, (s * 150.0) as u16);
        self.set_stroke(ID_HEAD, to, dt, ops);
    }

    fn blink(&mut self, now_ms: u32, ops: &mut Vec<SceneOp>) {
        if let Some(until) = self.blink_until {
            if now_ms >= until {
                self.blink_until = None;
                self.set_eyes_open(true, ops);
            }
            return;
        }
        if now_ms.saturating_sub(self.last_blink) >= BLINK_EVERY_MS {
            self.last_blink = now_ms;
            self.blink_until = Some(now_ms + BLINK_MS);
            self.set_eyes_open(false, ops);
        }
    }

    fn set_eyes_open(&mut self, open: bool, ops: &mut Vec<SceneOp>) {
        for i in 0..2 {
            for s in 0..EYE_SEGS {
                self.set_visible(ID_EYE_UPPER[i] + s, open, ops);
                self.set_visible(ID_EYE_LOWER[i] + s, open, ops);
            }
            self.set_visible(ID_PUPILS[i], open, ops);
            self.set_visible(ID_LIDS[i], !open, ops);
        }
    }

    /// Listening: the two rings pulse in anti-phase.
    fn pulse_rings(&mut self, now_ms: u32, dt: u32, ops: &mut Vec<SceneOp>) {
        let t = now_ms % PULSE_MS;
        let s0 = half_sine(t, PULSE_MS);
        let s1 = half_sine((t + PULSE_MS / 2) % PULSE_MS, PULSE_MS);
        let c0 = DIM.lerp_q8(&GREEN, (s0 * 256.0) as u16);
        let c1 = DIM.lerp_q8(&GREEN, (s1 * 256.0) as u16);
        self.set_stroke(ID_RINGS[0], c0, dt, ops);
        self.set_stroke(ID_RINGS[1], c1, dt, ops);
    }

    /// Thinking: a chord sweeps down and up the head.
    fn scan(&mut self, now_ms: u32, ops: &mut Vec<SceneOp>) {
        let t = (now_ms % SCAN_MS) as f32 / SCAN_MS as f32;
        let frac = if t < 0.5 { 2.0 * t } else { 2.0 - 2.0 * t };
        let dy = -56.0 + 112.0 * frac;
        let half = ((HEAD_R * HEAD_R) as f32 - dy * dy).max(0.0).sqrt() as i32;
        let y = HEAD_CY + dy as i32;
        let n = line(ID_SCAN, HEAD_CX - half, y, HEAD_CX + half, y, GREEN);
        if self.nodes.get(&ID_SCAN) != Some(&n) {
            self.nodes.insert(ID_SCAN, n.clone());
            ops.push(SceneOp::Update(n));
        }
    }

    /// Thinking: three dots orbit the head.
    fn orbit(&mut self, now_ms: u32, dt: u32, ops: &mut Vec<SceneOp>) {
        for (k, id) in ID_DOTS.into_iter().enumerate() {
            let to = orbit_point(now_ms, k);
            let from = self.dot_pos[k];
            if from == to {
                continue;
            }
            self.dot_pos[k] = to;
            if let Some(n) = self.nodes.get_mut(&id) {
                n.transform.x = weftos_leaf_scene::px(to.0);
                n.transform.y = weftos_leaf_scene::px(to.1);
            }
            ops.push(tween_pos(id, from, to, dt));
        }
    }

    /// Speaking: five mouth bars take new heights every 80 ms.
    fn mouth_bars(&mut self, now_ms: u32, ops: &mut Vec<SceneOp>) {
        let step = now_ms / BAR_STEP_MS;
        if step == self.bar_step {
            return;
        }
        self.bar_step = step;
        let (main, _, _) = self.colors();
        for (i, x) in BAR_XS.into_iter().enumerate() {
            let h = 2 + (lcg(step.wrapping_mul(5).wrapping_add(i as u32)) % 14) as i32;
            let n = line(ID_BARS[i], x, MOUTH_Y - h, x, MOUTH_Y + h, main);
            self.nodes.insert(ID_BARS[i], n.clone());
            ops.push(SceneOp::Update(n));
        }
    }
}

/// 0 → 1 → 0 over one period, smooth at both ends.
fn half_sine(t: u32, period: u32) -> f32 {
    let x = t as f32 / period as f32;
    0.5 - 0.5 * (x * core::f32::consts::TAU).cos()
}

fn orbit_point(now_ms: u32, k: usize) -> (i32, i32) {
    let a = (now_ms % ORBIT_MS) as f32 / ORBIT_MS as f32 * core::f32::consts::TAU
        + k as f32 * core::f32::consts::TAU / 3.0;
    (
        (HEAD_CX as f32 + ORBIT_R * a.cos()).round() as i32,
        (HEAD_CY as f32 + ORBIT_R * a.sin()).round() as i32,
    )
}

/// Small deterministic hash for the mouth bars (no rand dependency).
fn lcg(x: u32) -> u32 {
    let mut v = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
    v ^= v >> 13;
    v.wrapping_mul(0x5bd1_e995) >> 8
}
