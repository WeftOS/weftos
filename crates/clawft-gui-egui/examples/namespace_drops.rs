//! Namespace drops — sessions as a forest of stalks.
//!
//! Spike for the Trajectory Atlas mesh path (`examples/atlas_stress.rs`).
//! The picture draws a [`feed::World`]. Parsers in this file and in
//! `parse_history` each return a [`feed::Contribution`]. Another project's
//! parser can be pushed into the same merge without the mesh knowing where
//! the stalks came from.
//!
//! Each project is one cell of a square. The square steps up, 1×1 then 2×2
//! then 3×3, as another project arrives. Inside a cell the stalks fill their
//! own grid as that project grows. A seat says how an agent wants to sit:
//! tint, stem, instrument. A registrar can drop the same claim in
//! `namespace-drops-seats.json`. Standing stalks keep the history ink.
//!
//! The voice is off until you turn it on. Litany opens a breath with one
//! "who". Census speaks the phrases and does not announce them. Spoken lines
//! wait in order: each one is generated, played, then a short pause, then
//! the next. Reasoning, tool output, and command lines stay out.
//!
//! ```text
//! cargo run -p clawft-gui-egui --example namespace_drops
//! ```
//!
//! Inputs come from the environment; with none set the example reads the
//! current directory as one project and shows no sessions.
//!
//! - `NAMESPACE_DROPS_REPOS`: `name=path,name=path` repositories to read.
//! - `NAMESPACE_DROPS_BOARD`: optional JSON board export.
//! - `NAMESPACE_DROPS_SESSIONS`: colon list of Claude session transcripts.
//! - `NAMESPACE_DROPS_CODEX_DIR`: optional directory of Codex session files.
//! - `NAMESPACE_DROPS_LLM_DIR`: optional local TTS tool directory.

// The synth only has an audio sink on macOS; elsewhere its voices are
// compiled but not driven. The DSP and layout code takes many scalar
// arguments and indexes parallel tables by design.
#![cfg_attr(not(target_os = "macos"), allow(dead_code, non_snake_case))]
#![allow(clippy::too_many_arguments, clippy::needless_range_loop)]

use std::collections::VecDeque;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, Mesh, Pos2, Rect, Shape, Vec2};
use serde_json::Value;

#[path = "namespace_drops/activity.rs"]
mod activity;
#[path = "namespace_drops/codegraph.rs"]
mod codegraph;
#[path = "namespace_drops/feed.rs"]
mod feed;
#[path = "namespace_drops/parse_history.rs"]
mod parse_history;
#[path = "namespace_drops/sound.rs"]
mod sound;
#[path = "namespace_drops/stream.rs"]
mod stream;

const REPLAY: f32 = feed::REPLAY_SECS;
const TAIL: u64 = 1_500_000;
const LOD_FAR: usize = 7;
const LOD_NEAR: usize = 18;
/// Looking up. The orbit stops here, one step short of flipping the picture.
const PITCH_MIN: f32 = -1.45;
/// Straight down. The orbit stops here, one step short of flipping the picture.
const PITCH_MAX: f32 = 1.55;
/// Radians of pitch per pixel of drag.
const ORBIT_GAIN: f32 = 0.016;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Voice {
    Quiet,
    Litany,
    Census,
}

#[derive(Clone, Copy)]
struct Kind(u8);

impl Kind {
    const READ: Self = Self(0);
    const EDIT: Self = Self(1);
    const BASH: Self = Self(2);
    const AGENT: Self = Self(3);
    const OTHER: Self = Self(4);
    const FAIL: Self = Self(5);
    const CI: Self = Self(6);
    const DEPLOY: Self = Self(7);

    fn color(self) -> Color32 {
        match self.0 {
            0 => Color32::from_rgb(186, 214, 236),
            1 => Color32::from_rgb(232, 186, 96),
            2 => Color32::from_rgb(140, 196, 150),
            3 => Color32::from_rgb(198, 170, 224),
            5 => Color32::from_rgb(214, 96, 88),
            6 => Color32::from_rgb(120, 196, 214),
            7 => Color32::from_rgb(232, 150, 86),
            _ => Color32::from_rgb(180, 180, 176),
        }
    }

    fn label(self) -> &'static str {
        match self.0 {
            0 => "read",
            1 => "edit",
            2 => "command",
            3 => "agent",
            5 => "failed",
            6 => "ci",
            7 => "deploy",
            _ => "marked",
        }
    }
}

struct Node {
    /// Index into [`App::domains`].
    domain: u16,
    /// A branch or a card. Live session stalks are false.
    standing: bool,
    ns: String,
    pos: Pos2,
    hits: u16,
    /// Sum of record sizes. Ground-dot radius comes from this. The record
    /// itself is not kept.
    mass: u32,
    tint: Kind,
    /// History color. Live stalks leave this black and use kind or hue.
    ink: [u8; 3],
    /// Live tint from the agent's seat.
    hue: [u8; 3],
    /// Who touched it. Empty on a history stalk that predates seats.
    agent: String,
    /// Stage, and a short title only when this row is failing.
    label: String,
    /// Heat at `hot_ms`. Zero leaves the radius alone.
    heat: f32,
    hot_ms: u64,
}

struct Bead {
    kind: Kind,
    mag: f32,
    at: f32,
}

struct DropEv {
    node: u32,
    /// 0..1 along the replay.
    at: f32,
    kind: Kind,
    mag: f32,
}

/// One clause of assistant prose, placed on the replay clock.
struct Said {
    at: f32,
    clause: String,
}

struct Heard {
    ts: String,
    clause: String,
}

/// Silence after one line finishes playing, before the next queued line.
const SPEECH_GAP: Duration = Duration::from_millis(450);

/// Push one spoken line. Empty lines are refused. A later line waits behind
/// the ones already queued.
fn enqueue_line(queue: &mut VecDeque<String>, line: &str) -> bool {
    let line = line.trim();
    if line.is_empty() {
        return false;
    }
    queue.push_back(line.to_string());
    true
}

struct Speaker {
    /// Lines in the order they were asked. The mouth finishes one, pauses,
    /// then takes the next.
    pending: Arc<Mutex<VecDeque<String>>>,
    wake: Arc<Condvar>,
}

impl Speaker {
    fn spawn(speaking: Arc<AtomicBool>) -> Self {
        let pending = Arc::new(Mutex::new(VecDeque::<String>::new()));
        let wake = Arc::new(Condvar::new());
        let pending_t = Arc::clone(&pending);
        let wake_t = Arc::clone(&wake);
        std::thread::spawn(move || {
            let mut guard = pending_t.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                while guard.is_empty() {
                    guard = wake_t.wait(guard).unwrap_or_else(|e| e.into_inner());
                }
                let Some(line) = guard.pop_front() else {
                    continue;
                };
                drop(guard);
                // Brushes thin while this line is synthesized and played.
                // They come back for the pause between lines.
                speaking.store(true, Ordering::Relaxed);
                if !mlx_say(&line) && !supertonic_say(&line) {
                    let _ = Command::new("say")
                        .args(["-v", "Reed (English (US))", "-r", "150", &line])
                        .status();
                }
                speaking.store(false, Ordering::Relaxed);
                std::thread::sleep(SPEECH_GAP);
                guard = pending_t.lock().unwrap_or_else(|e| e.into_inner());
            }
        });
        Self { pending, wake }
    }

    fn say(&self, line: String) -> bool {
        let mut guard = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        let queued = enqueue_line(&mut guard, &line);
        if queued {
            self.wake.notify_one();
        }
        queued
    }
}

struct App {
    nodes: Vec<Node>,
    drops: Vec<DropEv>,
    beads: Vec<Vec<Bead>>,
    domains: Vec<feed::Domain>,
    domain_n: Vec<usize>,
    side: u32,
    zoom: f32,
    pan: Vec2,
    look: Vec2,
    yaw: f32,
    pitch: f32,
    /// Extra eye height above the orbit. The gaze still passes through `look`.
    lift: f32,
    flat: bool,
    by_kind: bool,
    show_synth: bool,
    /// Hide the branch field and leave the files the agents are touching.
    files: bool,
    show_graph: bool,
    graph: codegraph::Graph,
    ask: String,
    answer: String,
    /// Cleaned line spoken if the local reader does not come back.
    fallback: String,
    /// A local reader may replace `answer` and nudge the band.
    polish: Option<std::sync::mpsc::Receiver<stream::Reading>>,
    follow: Option<String>,
    /// 0 field, 1 soundtrack.
    tab: u8,
    voice: Voice,
    soundtrack: bool,
    master: f32,
    vols: [f32; 9],
    sound_on: [bool; 9],
    inst: [u8; 8],
    brush: f32,
    bpm: f32,
    release_approach: f32,
    release_landed: bool,
    failing: Vec<String>,
    sources: Vec<&'static str>,
    bed: sound::Player,
    speaker: Speaker,
    said: Vec<Said>,
    pending: Vec<String>,
    last_line: String,
    /// First frame asks macOS to treat this as a normal app. A binary
    /// started under an agent otherwise has no Dock icon and cannot be
    /// cmd-tabbed.
    shown: bool,
    since_spoke: Instant,
    prev_now: f32,
    t0: Instant,
    hover: Option<u32>,
    note: String,
    paint_ms: f32,
    drawn: usize,
    beads_drawn: usize,
    ground_px: Vec<Pos2>,
    ground_r: Vec<f32>,
    order: Vec<(f32, u32)>,
}

fn main() -> eframe::Result {
    let world = load();
    let mut graph = codegraph::pull(&spike_dir());
    codegraph::place(&mut graph, &world.domains, world.side);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1480.0, 900.0])
            .with_position(egui::pos2(120.0, 80.0))
            .with_active(true)
            .with_visible(true)
            .with_title("namespace drops"),
        ..Default::default()
    };
    eframe::run_native(
        "namespace-drops",
        options,
        Box::new(move |_cc| Ok(Box::new(App::new(world, graph)))),
    )
}

impl App {
    fn new(world: feed::World, graph: codegraph::Graph) -> Self {
        let now_ms = activity::now_ms();
        let heat = world
            .placed
            .iter()
            .map(|p| activity::live_heat(p.heat, p.hot_ms, now_ms))
            .fold(0.0f32, f32::max);
        let mut nodes: Vec<Node> = world
            .placed
            .iter()
            .map(|p| Node {
                domain: p.domain,
                standing: p.standing,
                ns: p.ns.clone(),
                pos: Pos2::ZERO,
                hits: p.hits,
                mass: p.mass,
                tint: Kind(p.kind),
                ink: p.ink,
                hue: p.hue,
                agent: p.agent.clone(),
                label: p.label.clone(),
                heat: p.heat,
                hot_ms: p.hot_ms,
            })
            .collect();
        layout(&mut nodes, &world.domains, world.side);
        let mut domain_n = vec![0usize; world.domains.len()];
        for n in &nodes {
            let i = n.domain as usize;
            if i < domain_n.len() {
                domain_n[i] += 1;
            }
        }
        let mut drops: Vec<DropEv> = world
            .landings
            .iter()
            .map(|l| DropEv {
                node: l.node,
                at: l.at,
                kind: Kind(l.kind),
                mag: l.mag,
            })
            .collect();
        drops.sort_by(|a, b| a.at.partial_cmp(&b.at).unwrap_or(std::cmp::Ordering::Equal));
        let mut beads = Vec::with_capacity(nodes.len());
        for _ in 0..nodes.len() {
            beads.push(Vec::new());
        }
        for d in &drops {
            if (d.node as usize) < beads.len() {
                beads[d.node as usize].push(Bead {
                    kind: d.kind,
                    mag: d.mag,
                    at: d.at,
                });
            }
        }
        let events: Vec<sound::Event> = drops
            .iter()
            .filter_map(|d| {
                let node = nodes.get(d.node as usize)?;
                Some(sound::Event {
                    at: d.at,
                    kind: d.kind.0,
                    mag: d.mag,
                    group: node.domain.min(255) as u8,
                    x: node.pos.x,
                    y: node.pos.y,
                    len: sound::sounding_len(node.ns.chars().count() as u32, node.hits as u32)
                        as u16,
                })
            })
            .collect();
        let said = world
            .phrases
            .iter()
            .map(|p| Said {
                at: p.at,
                clause: p.clause.clone(),
            })
            .collect();
        let chord = codegraph::chord_strings(&graph, now_ms);
        let bed = sound::Player::start(
            &events,
            &world.residents,
            world.bpm,
            world.pace,
            world.cadence,
            world.coherence,
            !world.failing.is_empty(),
            world.release_approach,
            world.release_landed,
            heat,
            &chord,
        );
        let speaker = Speaker::spawn(bed.speaking_handle());
        Self {
            nodes,
            drops,
            beads,
            domains: world.domains,
            domain_n,
            side: world.side,
            zoom: 1.0,
            pan: Vec2::ZERO,
            look: Vec2::new(0.5, 0.5),
            yaw: 0.2,
            pitch: 0.58,
            lift: 0.0,
            flat: false,
            by_kind: false,
            show_synth: true,
            files: false,
            show_graph: true,
            graph,
            ask: String::new(),
            answer: String::new(),
            fallback: String::new(),
            polish: None,
            follow: None,
            tab: 0,
            voice: Voice::Quiet,
            soundtrack: true,
            master: 0.8,
            vols: [0.7, 0.55, 0.4, 0.45, 0.6, 0.5, 0.45, 0.35, 0.58],
            sound_on: [true; 9],
            inst: [0, 1, 2, 0, 3, 4, 0, 0],
            brush: 0.55,
            bpm: world.bpm,
            release_approach: world.release_approach,
            release_landed: world.release_landed,
            failing: world.failing,
            sources: world.sources,
            bed,
            speaker,
            said,
            note: world.note,
            pending: Vec::new(),
            last_line: String::new(),
            shown: false,
            since_spoke: Instant::now(),
            prev_now: 0.0,
            t0: Instant::now(),
            hover: None,
            paint_ms: 0.0,
            drawn: 0,
            beads_drawn: 0,
            ground_px: Vec::new(),
            ground_r: Vec::new(),
            order: Vec::new(),
        }
    }

    fn frame_live(&mut self) {
        let real: Vec<Pos2> = self
            .nodes
            .iter()
            .filter(|n| !n.standing)
            .map(|n| n.pos)
            .collect();
        if real.is_empty() {
            return;
        }
        let n = real.len() as f32;
        let mid_x = real.iter().map(|p| p.x).sum::<f32>() / n;
        let mid_y = real.iter().map(|p| p.y).sum::<f32>() / n;
        self.look = Vec2::new(mid_x, mid_y);
        self.zoom = 1.7;
        self.pitch = 0.66;
        self.lift = 0.0;
        self.flat = false;
        let min_x = real.iter().map(|p| p.x).fold(1.0f32, f32::min);
        let max_x = real.iter().map(|p| p.x).fold(0.0f32, f32::max);
        let min_y = real.iter().map(|p| p.y).fold(1.0f32, f32::min);
        let max_y = real.iter().map(|p| p.y).fold(0.0f32, f32::max);
        self.pan = Vec2::new(0.5 - (min_x + max_x) * 0.5, 0.5 - (min_y + max_y) * 0.5);
    }

    fn look_down(&mut self) {
        self.pitch = PITCH_MAX;
        self.flat = false;
    }

    fn frame_overview(&mut self) {
        self.look = Vec2::new(0.5, 0.5);
        self.zoom = 1.0;
        self.pan = Vec2::ZERO;
        self.yaw = 0.2;
        self.pitch = 0.58;
        self.lift = 0.0;
        self.flat = false;
        self.follow = None;
    }

    fn run_ask(&mut self) {
        let ask = activity::parse_ask(&self.ask);
        if matches!(ask, activity::FieldAsk::Empty) {
            return;
        }
        match &ask {
            activity::FieldAsk::Follow(name) => self.follow = Some(name.clone()),
            activity::FieldAsk::Stop => self.follow = None,
            activity::FieldAsk::Overview => self.frame_overview(),
            _ => {}
        }
        let now = activity::now_ms();
        let sights: Vec<activity::Sight> = self
            .nodes
            .iter()
            .map(|n| activity::Sight {
                agent: n.agent.clone(),
                project: self
                    .domains
                    .get(n.domain as usize)
                    .map(|d| d.project.clone())
                    .unwrap_or_default(),
                ns: n.ns.clone(),
                kind: n.tint.0,
                heat: activity::live_heat(n.heat, n.hot_ms, now),
                standing: n.standing,
            })
            .collect();
        let line = activity::answer(&ask, &sights);
        if line.is_empty() {
            return;
        }
        let recast = matches!(
            ask,
            activity::FieldAsk::Happening | activity::FieldAsk::Doing(_)
        ) && !line.contains("holds and waits");
        if recast {
            let names = names_of(&line);
            self.queue_reading(&line, names);
        } else {
            self.answer = line.clone();
            if self.voice != Voice::Quiet {
                let _ = self.speaker.say(line);
            }
        }
        self.ask.clear();
    }

    /// Clean the chunk, show it, and say it. The reader may replace the line
    /// and may nudge the band. One reading at a time. Quiet does not call the
    /// model and does not speak. The raw line is never spoken.
    fn queue_reading(&mut self, raw: &str, names: Vec<String>) {
        let cleaned = stream::plain_speech(raw);
        if word_count(&cleaned) < 4 {
            self.answer = cleaned;
            return;
        }
        self.answer = cleaned.clone();
        self.last_line = cleaned.clone();
        if self.voice != Voice::Quiet {
            let _ = self.speaker.say(cleaned.clone());
        }
        if self.voice == Voice::Quiet || self.polish.is_some() {
            return;
        }
        self.fallback = cleaned.clone();
        self.polish = Some(stream::spawn_reading(cleaned, names));
    }

    fn narrate(&mut self, now: f32) {
        if self.voice == Voice::Quiet {
            self.pending.clear();
            self.prev_now = now;
            return;
        }
        for s in &self.said {
            if crossed(self.prev_now, now, s.at) {
                self.pending.push(s.clause.clone());
            }
        }
        self.prev_now = now;
        // A flood keeps only the latest breath. The rest is already past.
        shed_to_fit(&mut self.pending);
        if self.pending.is_empty() || self.since_spoke.elapsed().as_secs_f32() < BREATH_GAP {
            return;
        }
        let chosen = take_breath(&mut self.pending);
        self.pending.clear();
        if chosen.is_empty() {
            return;
        }
        let line = breath_line(self.voice, &chosen);
        if line.is_empty() {
            return;
        }
        // A litany breath has no required name. The poet is not told to say "who".
        self.queue_reading(&line, Vec::new());
        self.since_spoke = Instant::now();
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if !self.shown {
            self.shown = true;
            come_forward(ui.ctx());
        }
        let full = ui.max_rect();
        ui.painter()
            .rect_filled(full, 0.0, Color32::from_rgb(14, 15, 18));
        let side = Rect::from_min_max(
            Pos2::new(full.right() - 330.0, full.top()),
            full.right_bottom(),
        );
        if let Some(got) = self.polish.as_ref().map(|rx| rx.try_recv()) {
            match got {
                Ok(reading) => {
                    let still = !self.fallback.is_empty()
                        && (self.answer == self.fallback || self.last_line == self.fallback);
                    if still {
                        self.answer = reading.line.clone();
                        self.last_line = reading.line.clone();
                        if self.voice != Voice::Quiet && reading.line != self.fallback {
                            let _ = self.speaker.say(reading.line);
                        }
                    }
                    self.bed.set_steer(reading.tempo, reading.scale);
                    self.fallback.clear();
                    self.polish = None;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    // The cleaned line was already spoken when it was shown.
                    self.fallback.clear();
                    self.polish = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
        }
        let mut gates = 0u16;
        for (i, on) in self.sound_on.iter().enumerate() {
            if *on {
                gates |= 1u16 << i;
            }
        }
        self.bed.set_mix(
            self.master,
            &self.vols,
            &self.inst,
            self.brush,
            !self.failing.is_empty(),
            sound::error_rate(self.failing.len(), self.nodes.len()),
            gates,
        );
        self.bed
            .set_ear(self.look.x, self.look.y, self.yaw, self.pitch);
        self.bed.set_graph(self.show_graph);
        let plot = Rect::from_min_max(full.left_top(), Pos2::new(side.left(), full.bottom()));

        let pointer = ui.input(|i| {
            (
                i.smooth_scroll_delta.y,
                i.pointer.delta(),
                i.pointer.hover_pos(),
                i.pointer.primary_down(),
                i.pointer.secondary_down(),
                i.modifiers.shift,
            )
        });
        let over_side = pointer.2.is_some_and(|p| side.contains(p));
        if !over_side && pointer.0.abs() > 0.0 {
            self.zoom = (self.zoom * (1.0 + pointer.0 * 0.0012)).clamp(0.08, 16.0);
        }
        let orbit = pointer.3 && !pointer.5 && !over_side;
        let slide = (pointer.4 || (pointer.3 && pointer.5)) && !over_side;
        if self.flat {
            if pointer.3 && !over_side {
                self.pan += pointer.1 / plot.size().min_elem().max(1.0) / self.zoom;
            }
        } else if orbit {
            self.yaw += pointer.1.x * 0.012;
            self.pitch = orbit_pitch(self.pitch, pointer.1.y);
        } else if slide {
            let (sy, cy) = self.yaw.sin_cos();
            let gain = 0.0015;
            let right = Vec2::new(cy, -sy);
            let fwd = Vec2::new(sy, cy);
            self.look += right * pointer.1.x * gain - fwd * pointer.1.y * gain * 0.45;
            self.look.x = self.look.x.clamp(-8.0, 8.0);
            self.look.y = self.look.y.clamp(-8.0, 8.0);
        }
        if let Some(agent) = &self.follow {
            let mut sx = 0.0;
            let mut sy = 0.0;
            let mut n = 0.0;
            for node in &self.nodes {
                if node.standing || !node.agent.eq_ignore_ascii_case(agent) {
                    continue;
                }
                sx += node.pos.x;
                sy += node.pos.y;
                n += 1.0;
            }
            if n > 0.0 {
                let target = Vec2::new(sx / n, sy / n);
                self.look += (target - self.look) * 0.15;
            }
        }

        let now = (self.t0.elapsed().as_secs_f32() % REPLAY) / REPLAY;
        let lod = if self.zoom < 1.25 { LOD_FAR } else { LOD_NEAR };
        let t_paint = Instant::now();
        let mut mesh = Mesh::default();
        let mut drawn = 0usize;
        let mut beads_drawn = 0usize;

        let n_nodes = self.nodes.len();
        self.ground_px.resize(n_nodes, Pos2::new(-1.0e6, -1.0e6));
        self.ground_r.resize(n_nodes, 0.0);
        self.order.clear();

        let scale = plot.size().min_elem() * self.zoom;
        let origin = plot.center() + self.pan * scale;

        for i in 0..n_nodes {
            let node = &self.nodes[i];
            if node.standing && (!self.show_synth || self.files) {
                self.ground_r[i] = 0.0;
                continue;
            }
            let (sp, px, zc) = if self.flat {
                let sp = origin + (node.pos - Pos2::new(0.5, 0.5)) * scale;
                (sp, self.zoom.sqrt(), 1.0)
            } else {
                let Some(hit) = project(
                    plot, node.pos, 0.0, self.look, self.yaw, self.pitch, self.zoom, self.lift,
                ) else {
                    self.ground_r[i] = 0.0;
                    continue;
                };
                hit
            };
            if !plot.expand(48.0).contains(sp) {
                self.ground_r[i] = 0.0;
                continue;
            }
            let boost = if node.standing { 1.0 } else { 1.35 };
            let cool = if node.hot_ms == 0 {
                1.0
            } else {
                let h = activity::live_heat(node.heat, node.hot_ms, activity::now_ms());
                0.40 + 0.60 * h.clamp(0.0, 1.0)
            };
            let r = dot_radius(node.mass, px) * boost * cool;
            self.ground_px[i] = sp;
            self.ground_r[i] = r;
            self.order.push((zc, i as u32));
        }
        self.order
            .sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

        for &(zc, id) in &self.order {
            let i = id as usize;
            let node = &self.nodes[i];
            let ground = self.ground_px[i];
            let r = self.ground_r[i];
            let gcol = ground_color(node, self.by_kind);
            if !self.flat {
                let stalk = stalk_samples(node, &self.beads[i], lod, now);
                if stalk.n > 0 {
                    let top = stalk.y[stalk.n - 1];
                    if let Some((crown, _, _)) = project(
                        plot, node.pos, top, self.look, self.yaw, self.pitch, self.zoom, self.lift,
                    ) {
                        let tw = (1.1 + (node.hits as f32).sqrt() * 0.28).clamp(1.0, 4.2);
                        ribbon(
                            &mut mesh,
                            ground,
                            crown,
                            tw * (1.15 / zc).clamp(0.45, 1.4),
                            fade(gcol, 78),
                        );
                    }
                }
                for k in 0..stalk.n {
                    let Some((mut sp, px, _)) = project(
                        plot, node.pos, stalk.y[k], self.look, self.yaw, self.pitch, self.zoom,
                        self.lift,
                    ) else {
                        continue;
                    };
                    if !plot.expand(24.0).contains(sp) {
                        continue;
                    }
                    let mag = stalk.mag[k];
                    let mut br = (px * (0.55 + mag * 1.15)).clamp(0.8, 8.0);
                    let col = if node.standing {
                        Color32::from_rgb(node.ink[0], node.ink[1], node.ink[2])
                    } else {
                        stalk.kind[k].color()
                    };
                    if stalk.hot[k] {
                        let a = age(now, stalk.at[k]).min(0.04) / 0.04;
                        let fall = (1.0 - a) * (1.0 - a);
                        sp.y -= fall * (14.0 + mag * 64.0);
                        br *= 1.15 + fall * 0.85;
                    }
                    dot(&mut mesh, sp, br, col);
                    beads_drawn += 1;
                }
            }
            dot(&mut mesh, ground, r, gcol);
            drawn += 1;
        }
        ui.painter().add(Shape::mesh(mesh));
        self.draw_graph(ui, plot);
        self.drawn = drawn;
        self.beads_drawn = beads_drawn;
        self.paint_ms = t_paint.elapsed().as_secs_f32() * 1000.0;

        let grid = self.side.max(1);
        for row in 0..grid {
            for col in 0..grid {
                let (x0, y0, x1, y1) = feed::cell_at(col, row, grid);
                let corners = [
                    Pos2::new(x0, y0),
                    Pos2::new(x1, y0),
                    Pos2::new(x1, y1),
                    Pos2::new(x0, y1),
                ];
                let mut screen = [Pos2::ZERO; 4];
                let mut ok = true;
                for (i, corner) in corners.iter().enumerate() {
                    let sp = if self.flat {
                        origin + (*corner - Pos2::new(0.5, 0.5)) * scale
                    } else {
                        match project(
                            plot, *corner, 0.0, self.look, self.yaw, self.pitch, self.zoom,
                            self.lift,
                        ) {
                            Some((sp, _, _)) => sp,
                            None => {
                                ok = false;
                                break;
                            }
                        }
                    };
                    screen[i] = sp;
                }
                if ok {
                    for i in 0..4 {
                        ui.painter().line_segment(
                            [screen[i], screen[(i + 1) % 4]],
                            egui::Stroke::new(1.0, Color32::from_white_alpha(36)),
                        );
                    }
                }
                let Some(domain) = self.domains.iter().find(|d| d.col == col && d.row == row)
                else {
                    continue;
                };
                let at = Pos2::new((x0 + x1) * 0.5, y0 + (y1 - y0) * 0.06);
                let label_at = if self.flat {
                    origin + (at - Pos2::new(0.5, 0.5)) * scale
                } else {
                    let Some((sp, _, _)) = project(
                        plot, at, 0.02, self.look, self.yaw, self.pitch, self.zoom, self.lift,
                    ) else {
                        continue;
                    };
                    sp
                };
                if plot.contains(label_at) {
                    ui.painter().text(
                        label_at,
                        egui::Align2::CENTER_CENTER,
                        &domain.project,
                        egui::FontId::proportional(13.0),
                        Color32::from_white_alpha(140),
                    );
                }
            }
        }

        self.hover = pointer
            .2
            .filter(|p| plot.contains(*p))
            .and_then(|p| pick(&self.ground_px, &self.ground_r, p));
        if let Some(id) = self.hover {
            let sp = self.ground_px[id as usize];
            let rr = self.ground_r[id as usize] + 3.0;
            ui.painter()
                .circle_stroke(sp, rr, egui::Stroke::new(1.0, Color32::WHITE));
            let node = &self.nodes[id as usize];
            let hover = if node.label.is_empty() {
                node.ns.clone()
            } else {
                format!("{} · {}", node.ns, node.label)
            };
            ui.painter().text(
                sp + Vec2::new(10.0, -8.0),
                egui::Align2::LEFT_BOTTOM,
                &hover,
                egui::FontId::monospace(12.0),
                Color32::WHITE,
            );
        }

        if self.voice != Voice::Quiet && !self.last_line.is_empty() {
            ui.painter().text(
                Pos2::new(plot.left() + 16.0, plot.bottom() - 18.0),
                egui::Align2::LEFT_BOTTOM,
                &self.last_line,
                egui::FontId::proportional(14.0),
                Color32::from_white_alpha(200),
            );
        }

        let mut recent: Vec<(Kind, String, f32)> = Vec::new();
        for d in &self.drops {
            if age(now, d.at) < 0.08 {
                let n = &self.nodes[d.node as usize];
                recent.push((d.kind, n.ns.clone(), d.mag));
            }
        }
        recent.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
        recent.truncate(6);

        self.side(ui, side, &recent);
        self.narrate(now);
        ui.ctx().request_repaint();
    }
}

impl App {
    fn side(&mut self, ui: &mut egui::Ui, side: Rect, recent: &[(Kind, String, f32)]) {
        ui.painter()
            .rect_filled(side, 0.0, Color32::from_rgb(22, 23, 26));
        ui.scope_builder(egui::UiBuilder::new().max_rect(side.shrink(12.0)), |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.horizontal(|ui| {
                    if ui.selectable_label(self.tab == 0, "field").clicked() {
                        self.tab = 0;
                    }
                    if ui.selectable_label(self.tab == 1, "soundtrack").clicked() {
                        self.tab = 1;
                    }
                });
                ui.add_space(6.0);
                if self.tab == 0 {
                    self.field_tab(ui, recent);
                } else {
                    self.soundtrack_tab(ui);
                }
            });
        });
    }

    fn field_tab(&mut self, ui: &mut egui::Ui, recent: &[(Kind, String, f32)]) {
        ui.label(egui::RichText::new("namespace drops").strong());
        ui.label(egui::RichText::new(self.sources.join(" · ")).small().weak());
        ui.label(egui::RichText::new(&self.note).small().weak());
        ui.add_space(8.0);
        ui.checkbox(&mut self.flat, "flat, sized dots");
        ui.checkbox(&mut self.by_kind, "color dots by kind");
        ui.checkbox(&mut self.show_synth, "branches and cards");
        ui.checkbox(&mut self.files, "open the files");
        ui.checkbox(&mut self.show_graph, "code graph");
        if ui.button("frame the live sessions").clicked() {
            self.frame_live();
        }
        if ui.button("look down on the plane").clicked() {
            self.look_down();
        }
        ui.add_space(8.0);
        ui.label(egui::RichText::new("ask the field").strong());
        let ask_box = ui.add(
            egui::TextEdit::singleline(&mut self.ask)
                .hint_text("what is claude doing")
                .desired_width(f32::INFINITY),
        );
        let ask_now = ui.button("ask").clicked()
            || (ask_box.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
        if ask_now {
            self.run_ask();
        }
        if !self.answer.is_empty() {
            ui.label(&self.answer);
            let mark = if stream::meter(&self.answer) == 0 {
                "station keeping"
            } else {
                "the room is moving"
            };
            ui.label(
                egui::RichText::new(format!(
                    "{mark}. a line that ends and waits is keeping station. a merge or a deploy runs longer. a local model may recast the line."
                ))
                .small()
                .weak(),
            );
        }
        if let Some(agent) = &self.follow {
            ui.label(format!("following {agent}"));
        }
        ui.add_space(8.0);
        ui.label(format!(
            "{} stalks · {} beads · mesh {:.1} ms",
            self.drawn, self.beads_drawn, self.paint_ms
        ));
        ui.label(format!(
            "{} projects on a {}×{} grid",
            self.domains.len(),
            self.side.max(1),
            self.side.max(1)
        ));
        for (i, domain) in self.domains.iter().enumerate().take(8) {
            let n = self.domain_n.get(i).copied().unwrap_or(0);
            ui.label(format!(
                "{}   {n}   {}",
                domain.project,
                seat_line(&domain.seats)
            ));
        }
        ui.add_space(6.0);
        let hint = if self.flat {
            "scroll zooms · drag pans"
        } else {
            "drag orbits · scroll zooms · right-drag slides"
        };
        ui.label(egui::RichText::new(hint).weak());
        ui.add_space(8.0);
        ui.label(egui::RichText::new("failing").strong());
        if self.failing.is_empty() {
            ui.label(egui::RichText::new("none").weak());
        } else {
            for line in self.failing.iter().take(8) {
                ui.colored_label(Color32::from_rgb(214, 96, 88), line);
            }
        }
        ui.add_space(8.0);
        if let Some(id) = self.hover {
            let n = &self.nodes[id as usize];
            ui.label(egui::RichText::new("under the pointer").strong());
            if n.label.is_empty() {
                ui.label(&n.ns);
            } else {
                ui.label(format!("{} · {}", n.ns, n.label));
            }
            ui.label(format!(
                "{} landed · {}",
                n.hits,
                size_word(magnitude(n.tint, n.mass.max(1)))
            ));
        }
        ui.add_space(8.0);
        ui.label(egui::RichText::new("landing now").strong());
        for (kind, ns, mag) in recent {
            ui.colored_label(
                kind.color(),
                format!("{} {}  {ns}", size_word(*mag), kind.label()),
            );
        }
    }

    fn soundtrack_tab(&mut self, ui: &mut egui::Ui) {
        if ui.checkbox(&mut self.soundtrack, "play").changed() {
            self.bed.set_enabled(self.soundtrack);
        }
        ui.label(format!("tempo  {:.0} bpm", self.bpm));
        ui.label(
            egui::RichText::new(
                "from the last hour of commits, as a 15-minute rolling average. quiet is 60. about four commits in that average is 140.",
            )
            .small()
            .weak(),
        );
        ui.label(
            egui::RichText::new(
                "when a release is in motion the pace still moves the tempo, and the room builds. the release runs up the scale into a held tonic on the ci / deploy stem.",
            )
            .small()
            .weak(),
        );
        ui.label(
            egui::RichText::new(
                "the repeating line follows coltrane's countdown. three stations a major third apart, eight then four then home on D. the first half of each bar is the dominant, the second half the major. a release stays home.",
            )
            .small()
            .weak(),
        );
        if self.release_landed {
            ui.label("this hour the release has landed. it plays after one phrase.");
        } else if self.release_approach > 0.0 {
            ui.label("this hour a release is building.");
        }
        ui.add_space(6.0);
        ui.label(egui::RichText::new("voice").strong());
        ui.radio_value(&mut self.voice, Voice::Quiet, "quiet");
        ui.radio_value(&mut self.voice, Voice::Litany, "litany");
        ui.radio_value(&mut self.voice, Voice::Census, "census");
        ui.label(
            egui::RichText::new(
                "Spoken lines wait in a queue. Each one is generated, played, then a short pause, and the next begins. Litany opens each breath with one who. Census speaks the phrases and does not announce them. A local reader turns each breath into a short line and may nudge the tempo and the scale. Quiet until you ask.",
            )
            .small()
            .weak(),
        );
        ui.add_space(8.0);
        ui.add(egui::Slider::new(&mut self.master, 0.0..=1.0).text("master"));
        const NAMES: [&str; 9] = [
            "drone",
            "bass",
            "chords",
            "melody",
            "landings",
            "ci / deploy",
            "brushes",
            "failure noise",
            "half-time drum",
        ];
        const INST: [&str; 5] = ["vibe", "sine", "pad", "marimba", "bell"];
        for i in 0..9 {
            ui.add_space(4.0);
            ui.checkbox(&mut self.sound_on[i], NAMES[i]);
            if i < 6 {
                let mut choice = self.inst[i];
                egui::ComboBox::from_id_salt(("inst", i))
                    .width(110.0)
                    .selected_text(INST[choice as usize % 5])
                    .show_ui(ui, |ui| {
                        for (k, name) in INST.iter().enumerate() {
                            ui.selectable_value(&mut choice, k as u8, *name);
                        }
                    });
                self.inst[i] = choice;
            }
            ui.add(egui::Slider::new(&mut self.vols[i], 0.0..=1.0).text("volume"));
            if i == 6 {
                ui.add(egui::Slider::new(&mut self.brush, 0.0..=1.0).text("density"));
            }
        }
        ui.add_space(6.0);
        ui.label(
            egui::RichText::new(
                "The bass walks the station, and rests on a euclidean pattern that rotates each phrase. Chords change shape with the phrase, and the scale walks dorian, lydian, aeolian, phrygian, melodic minor, harmonic minor, pentatonic, and whole tone after the opening Coltrane phrase. A release still comes home to D. A landing is the string on that stalk: a longer name, or a taller stalk, sits lower, and the note moves as you orbit. Once a bar the code graph plucks a chord: short connector to long, root then third, fifth, and seventh of the scale the band is on. Brushes follow the same rotating pattern, and they thin while the voice is up. The half-time drum is its own hit, once each eight beats. Failure noise is a wash, on only while a branch or card is failing. A higher error rate swells louder, washes darker, and stretches further. Each row has its own switch.",
            )
            .small()
            .weak(),
        );
    }
}

impl App {
    fn draw_graph(&self, ui: &mut egui::Ui, plot: Rect) {
        if !self.show_graph {
            return;
        }
        let now_ms = activity::now_ms();
        let painter = ui.painter();
        for edge in &self.graph.edges {
            let Some(a) = self.graph_screen(&edge.from, plot, now_ms) else {
                continue;
            };
            let Some(b) = self.graph_screen(&edge.to, plot, now_ms) else {
                continue;
            };
            painter.line_segment(
                [a, b],
                egui::Stroke::new(1.1, Color32::from_rgba_unmultiplied(196, 186, 150, 140)),
            );
        }
        for point in &self.graph.points {
            if point.x < 0.0 {
                continue;
            }
            let warm = if point.kind == 0 {
                activity::live_heat(point.heat, point.hot_ms, now_ms)
            } else {
                1.0
            };
            if warm < 0.08 {
                continue;
            }
            let z = if point.z > 0.0 {
                point.z
            } else {
                codegraph::plane_of(point.kind)
            };
            let Some(sp) = self.world_on(point.x, point.y, z, plot) else {
                continue;
            };
            if !plot.expand(8.0).contains(sp) {
                continue;
            }
            let alpha = (40.0 + 215.0 * warm.min(1.0)) as u8;
            let col = match point.kind {
                1 => Color32::from_rgba_unmultiplied(120, 176, 198, alpha),
                2 => Color32::from_rgba_unmultiplied(214, 168, 92, alpha),
                3 => Color32::from_rgba_unmultiplied(206, 130, 156, alpha),
                _ => Color32::from_rgba_unmultiplied(214, 210, 198, alpha),
            };
            painter.circle_filled(sp, if point.kind == 0 { 3.2 } else { 2.6 }, col);
            if point.kind >= 1 || warm > 0.35 {
                painter.text(
                    sp + Vec2::new(6.0, 0.0),
                    egui::Align2::LEFT_CENTER,
                    &point.label,
                    egui::FontId::proportional(11.0),
                    col,
                );
            }
        }
    }

    fn graph_screen(&self, key: &str, plot: Rect, now_ms: u64) -> Option<Pos2> {
        if let Some(tag) = key.strip_prefix("card:") {
            for (i, node) in self.nodes.iter().enumerate() {
                if self.ground_r.get(i).copied().unwrap_or(0.0) <= 0.0 {
                    continue;
                }
                if node.ns == tag || node.ns.ends_with(&format!("/{tag}")) {
                    return Some(self.ground_px[i]);
                }
            }
        }
        let point = self.graph.points.iter().find(|p| p.key == key)?;
        if point.x < 0.0 {
            return None;
        }
        if point.kind == 0 && !codegraph::file_visible(point.heat, point.hot_ms, now_ms) {
            return None;
        }
        let z = if point.z > 0.0 {
            point.z
        } else {
            codegraph::plane_of(point.kind)
        };
        self.world_on(point.x, point.y, z, plot)
    }

    fn world_on(&self, x: f32, y: f32, z: f32, plot: Rect) -> Option<Pos2> {
        if self.flat {
            let scale = plot.size().min_elem() * self.zoom;
            let origin = plot.center() + self.pan * scale;
            return Some(origin + (Pos2::new(x, y) - Pos2::new(0.5, 0.5)) * scale);
        }
        project(
            plot,
            Pos2::new(x, y),
            z,
            self.look,
            self.yaw,
            self.pitch,
            self.zoom,
            self.lift,
        )
        .map(|hit| hit.0)
    }
}

fn crossed(prev: f32, now: f32, at: f32) -> bool {
    if (now - prev).abs() < 1.0e-4 {
        return false;
    }
    if prev <= now {
        at > prev && at <= now
    } else {
        at > prev || at <= now
    }
}

fn age(now: f32, at: f32) -> f32 {
    let a = now - at;
    if a < 0.0 { a + 1.0 } else { a }
}

fn size_word(mag: f32) -> &'static str {
    if mag < 0.28 {
        "small"
    } else if mag < 0.48 {
        "modest"
    } else if mag < 0.72 {
        "long"
    } else {
        "heavy"
    }
}

fn word_count(s: &str) -> usize {
    s.split_whitespace().count()
}

/// First plain word of each clause. Field lines join on " — ".
fn names_of(line: &str) -> Vec<String> {
    let mut names = Vec::new();
    for part in line.split(" — ") {
        let Some(word) = part.split_whitespace().next() else {
            continue;
        };
        let word = word.trim_matches(|c: char| !c.is_ascii_alphabetic());
        if word.len() < 2 || !word.chars().all(|c| c.is_ascii_alphabetic()) {
            continue;
        }
        if names.iter().any(|n: &String| n.eq_ignore_ascii_case(word)) {
            continue;
        }
        names.push(word.to_ascii_lowercase());
    }
    names
}

/// One exhalation. The "who" is the riff that starts the breath, then the
/// clauses run on commas. It is not repeated inside the line.
fn breath_line(voice: Voice, clauses: &[String]) -> String {
    if clauses.is_empty() {
        return String::new();
    }
    let mut line = match voice {
        Voice::Quiet => return String::new(),
        Voice::Litany => {
            let mut line = String::from("who ");
            for (i, c) in clauses.iter().enumerate() {
                if i > 0 {
                    line.push_str(", and ");
                }
                line.push_str(&soften(c));
            }
            line.push(',');
            line
        }
        Voice::Census => {
            let mut line = String::new();
            for c in clauses {
                let s = c.trim().trim_end_matches(['.', '!', '?', ',', ';']);
                if s.is_empty() {
                    continue;
                }
                if !line.is_empty() {
                    line.push(' ');
                }
                line.push_str(s);
                line.push('.');
            }
            line
        }
    };
    if line.len() > 420 {
        line.truncate(420);
    }
    line
}

fn soften(clause: &str) -> String {
    let s = clause
        .trim()
        .trim_end_matches(['.', '!', '?', ',', ';', ':']);
    let s = s.strip_prefix("who ").unwrap_or(s);
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_lowercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Wall time for one spoken line, including the synth. The next breath waits.
const BREATH_GAP: f32 = 14.0;
/// Words and clauses that fit in that gap. A beat-poet line, not a transcript.
const BREATH_WORDS: usize = 32;
const BREATH_CLAUSES: usize = 5;

/// Drop clauses from the front until what remains fits one breath.
fn shed_to_fit(pending: &mut Vec<String>) {
    while pending.len() > 1 {
        let words: usize = pending.iter().map(|c| word_count(c)).sum();
        if pending.len() <= BREATH_CLAUSES && words <= BREATH_WORDS {
            break;
        }
        pending.remove(0);
    }
}

/// Take one breath from the front of an already-shed queue.
fn take_breath(pending: &mut Vec<String>) -> Vec<String> {
    let mut out = Vec::new();
    let mut words = 0usize;
    while let Some(c) = pending.first() {
        let w = word_count(c);
        if !out.is_empty() && (out.len() >= BREATH_CLAUSES || words + w > BREATH_WORDS) {
            break;
        }
        words += w;
        out.push(pending.remove(0));
        if words >= BREATH_WORDS || out.len() >= BREATH_CLAUSES {
            break;
        }
    }
    out
}

fn magnitude(kind: Kind, size: u32) -> f32 {
    let w = match kind.0 {
        0 => 0.34,
        1 => 0.62,
        2 => 0.52,
        3 => 0.88,
        5 => 0.78,
        6 => 0.7,
        7 => 0.84,
        _ => 0.28,
    };
    let s = ((size.max(1) as f32).ln() / 11.0).clamp(0.2, 1.15);
    (w * s).clamp(0.08, 1.0)
}

fn ground_color(n: &Node, by_kind: bool) -> Color32 {
    if n.standing {
        return Color32::from_rgb(n.ink[0], n.ink[1], n.ink[2]);
    }
    if by_kind {
        n.tint.color()
    } else {
        Color32::from_rgb(n.hue[0], n.hue[1], n.hue[2])
    }
}

fn fade(c: Color32, a: u8) -> Color32 {
    let [r, g, b, _] = c.to_array();
    Color32::from_rgba_unmultiplied(r, g, b, a)
}

fn dot_radius(mass: u32, px: f32) -> f32 {
    let s = (mass.max(1) as f32).sqrt().clamp(8.0, 180.0);
    let t = ((s - 8.0) / 172.0).clamp(0.0, 1.0);
    (px * (1.35 + t.sqrt() * 7.2)).clamp(1.3, 16.0)
}

/// Screen y grows downward. A negative `pointer_dy` is a drag upward.
/// Yaw is unbounded. Pitch runs from looking up ([`PITCH_MIN`]) through
/// the side and on to straight down ([`PITCH_MAX`]).
fn orbit_pitch(pitch: f32, pointer_dy: f32) -> f32 {
    (pitch - pointer_dy * ORBIT_GAIN).clamp(PITCH_MIN, PITCH_MAX)
}

/// Atlas forest camera. `y` is height above the ground dot.
///
/// The eye aims at `look`. Distance stays put while the gaze moves;
/// scroll changes `zoom`. `lift` only adds height.
fn project(
    plot: Rect,
    p: Pos2,
    y: f32,
    look: Vec2,
    yaw: f32,
    pitch: f32,
    zoom: f32,
    lift: f32,
) -> Option<(Pos2, f32, f32)> {
    let pitch = pitch.clamp(PITCH_MIN, PITCH_MAX);
    let (sy, cy) = yaw.sin_cos();
    let (sp, cp) = pitch.sin_cos();
    let x = p.x - look.x;
    let z = p.y - look.y;
    let (xr, zr) = (x * cy - z * sy, x * sy + z * cy);
    let dist = 1.55 + lift * 0.5;
    let (yr, zc) = (y * cp - zr * sp, y * sp + zr * cp + dist);
    if zc < 0.12 {
        return None;
    }
    let f = plot.height() * 1.7 * zoom;
    let screen = plot.center() + Vec2::new(xr, -yr) * (f / zc);
    let px = (2.2 / zc).clamp(0.35, 14.0);
    Some((screen, px, zc))
}

struct Stalk {
    n: usize,
    y: [f32; 36],
    kind: [Kind; 36],
    mag: [f32; 36],
    at: [f32; 36],
    hot: [bool; 36],
}

fn stalk_samples(node: &Node, beads: &[Bead], lod: usize, now: f32) -> Stalk {
    let synth = node.standing;
    let full = if synth {
        node.hits.max(1) as usize
    } else {
        beads.len().max(1)
    };
    let n = full.min(if synth { lod } else { lod.max(full).min(36) });
    let step = full as f32 / n.max(1) as f32;
    let mut stalk = Stalk {
        n,
        y: [0.0; 36],
        kind: [Kind::OTHER; 36],
        mag: [0.0; 36],
        at: [0.0; 36],
        hot: [false; 36],
    };
    let mut y = 0.012;
    let boost = if synth { 1.0 } else { 1.45 };
    for i in 0..n {
        let src = ((i as f32) * step) as usize;
        let (kind, mag, at) = if synth {
            let kind = Kind(((node.hits as usize + i * 5 + src) % 6) as u8);
            let mag = 0.22 + ((src * 5 + i * 7) % 10) as f32 * 0.07;
            let at = (src as f32 / full as f32).clamp(0.0, 0.999);
            (kind, mag, at)
        } else if beads.is_empty() {
            (node.tint, 0.4, 0.0)
        } else {
            let b = &beads[src.min(beads.len() - 1)];
            (b.kind, b.mag, b.at)
        };
        y += (0.011 + mag * 0.022) * boost;
        let is_hot = if synth {
            let trav = ((now + (node.mass % 97) as f32 * 0.01).fract() * n as f32) as usize;
            i == trav
        } else {
            age(now, at) < 0.04
        };
        stalk.y[i] = y;
        stalk.kind[i] = kind;
        stalk.mag[i] = mag;
        stalk.at[i] = at;
        stalk.hot[i] = is_hot;
    }
    if y > 0.46 {
        let s = 0.46 / y;
        for h in stalk.y.iter_mut().take(n) {
            *h *= s;
        }
    }
    stalk
}

fn role_degree(name: &str) -> i32 {
    let stem = name.trim_end_matches(".jsonl");
    let part = stem.split('-').nth(1).unwrap_or("");
    let role = if part.starts_with('a') && part.len() > 1 {
        &part[1..]
    } else {
        part
    };
    match role {
        "build" => 1,
        "fix" => 2,
        "stew" => 3,
        "rev" => 4,
        "relnotes" => 5,
        "doc" => 6,
        _ => 8,
    }
}

fn residents_from(dir: &Path, group: u8, into: &mut Vec<(u8, i32)>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<_> = rd.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    files.retain(|p| p.extension().is_some_and(|x| x == "jsonl"));
    files.sort_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());
    files.reverse();
    let mut n = 0;
    for p in files.into_iter().take(8) {
        let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
        let deg = role_degree(name);
        if into.iter().any(|&(g, d)| g == group && d == deg) {
            continue;
        }
        into.push((group, deg));
        n += 1;
        if n >= 3 {
            break;
        }
    }
}

fn pick(px: &[Pos2], r: &[f32], at: Pos2) -> Option<u32> {
    let mut best: Option<(u32, f32)> = None;
    for (i, p) in px.iter().enumerate() {
        if r[i] <= 0.0 {
            continue;
        }
        let d = p.distance_sq(at);
        let lim = (r[i] + 4.0).max(5.0);
        if d < lim * lim && best.is_none_or(|(_, bd)| d < bd) {
            best = Some((i as u32, d));
        }
    }
    best.map(|(id, _)| id)
}

fn dot(mesh: &mut Mesh, c: Pos2, r: f32, col: Color32) {
    let base = mesh.vertices.len() as u32;
    mesh.colored_vertex(c, col);
    for k in 0..6 {
        let a = k as f32 * std::f32::consts::TAU / 6.0;
        mesh.colored_vertex(c + Vec2::new(a.cos(), a.sin()) * r, col);
    }
    for k in 0..6u32 {
        mesh.add_triangle(base, base + 1 + k, base + 1 + (k + 1) % 6);
    }
}

fn ribbon(mesh: &mut Mesh, a: Pos2, b: Pos2, w: f32, col: Color32) {
    let d = b - a;
    let len = d.length();
    if len < 1.0 {
        return;
    }
    let n = Vec2::new(-d.y / len, d.x / len) * w;
    let base = mesh.vertices.len() as u32;
    mesh.colored_vertex(a - n, col);
    mesh.colored_vertex(a + n, col);
    mesh.colored_vertex(b + n, col);
    mesh.colored_vertex(b - n, col);
    mesh.add_triangle(base, base + 1, base + 2);
    mesh.add_triangle(base, base + 2, base + 3);
}

/// Each project fills one cell of the square. Inside the cell, stalks take
/// a grid that grows with how many of them landed there.
fn layout(nodes: &mut [Node], domains: &[feed::Domain], side: u32) {
    let mut counts = vec![0usize; domains.len()];
    for n in nodes.iter() {
        let g = n.domain as usize;
        if g < counts.len() {
            counts[g] += 1;
        }
    }
    let mut seen = vec![0usize; domains.len()];
    for n in nodes.iter_mut() {
        let g = n.domain as usize;
        if g >= domains.len() || counts[g] == 0 {
            continue;
        }
        let d = &domains[g];
        let (x0, y0, x1, y1) = feed::cell_at(d.col, d.row, side.max(1));
        let c = Rect::from_min_max(Pos2::new(x0, y0), Pos2::new(x1, y1));
        let cols = ((counts[g] as f32 * c.width() / c.height().max(1e-4))
            .sqrt()
            .ceil() as usize)
            .max(1);
        let rows = counts[g].div_ceil(cols).max(1);
        let i = seen[g];
        seen[g] += 1;
        let col = (i % cols) as f32;
        let row = (i / cols) as f32;
        // Depth jitter crosses rows so the field does not read as ruled paper.
        let spread = if n.standing { 1.45 } else { 0.85 };
        let jx = hash01(i as u32) - 0.5;
        let jy = hash01(i as u32 + 97) - 0.5;
        let x = c.min.x + (col + 0.5 + jx * 0.7) / cols as f32 * c.width();
        let y = c.min.y + (row + 0.5 + jy * spread) / rows as f32 * c.height();
        n.pos = Pos2::new(
            x.clamp(c.min.x + 0.004, c.max.x - 0.004),
            y.clamp(c.min.y + 0.004, c.max.y - 0.004),
        );
    }
}

fn seat_tone(project: &str, agent: &str, stem: u8, instrument: u8, degree: i32) -> feed::Seat {
    let mut seat = feed::Seat::new(project, agent);
    seat.stem = stem;
    seat.instrument = instrument;
    seat.degree = Some(degree);
    seat
}

fn seat_line(seats: &[feed::Seat]) -> String {
    const INST: [&str; 5] = ["vibe", "sine", "pad", "marimba", "bell"];
    const STEM: [&str; 8] = [
        "drone", "bass", "chords", "melody", "landings", "ci", "brushes", "noise",
    ];
    let mut parts = Vec::new();
    for seat in seats.iter().take(3) {
        let inst = INST[seat.instrument as usize % 5];
        let stem = if (seat.stem as usize) < STEM.len() {
            STEM[seat.stem as usize]
        } else {
            "visual"
        };
        parts.push(format!("{} {inst} {stem}", seat.agent));
    }
    parts.join(" · ")
}

fn hash01(i: u32) -> f32 {
    let mut x = i.wrapping_mul(747796405).wrapping_add(2_891_336_453);
    x = (x >> 16) ^ x;
    (x as f32) / (u32::MAX as f32)
}

struct Raw {
    ts: String,
    group: u8,
    ns: String,
    kind: Kind,
    /// Byte length of the record. The record is not stored.
    size: u32,
}

fn load() -> feed::World {
    let dir = spike_dir();
    let mut parts = vec![parse_sessions(), parse_history::pull(&dir)];
    let touched = activity::pull(&dir, activity::now_ms());
    if !touched.signals.is_empty() {
        parts.push(touched);
    }
    let seats = parse_seats(&dir);
    if !seats.seats.is_empty() {
        parts.insert(0, seats);
    }
    feed::merge(parts)
}

/// Registrar file. An agent, or later an MCP tool, writes seats here.
/// The parsers do not read each other.
fn parse_seats(dir: &Path) -> feed::Contribution {
    let mut out = feed::Contribution::empty("seats");
    let Ok(text) = std::fs::read_to_string(dir.join("namespace-drops-seats.json")) else {
        return out;
    };
    out.seats = feed::seats_from_json(&text);
    if !out.seats.is_empty() {
        out.note = format!(
            "{} seats registered. a seat claims a project and names its stem.",
            out.seats.len()
        );
    }
    out
}

fn parse_sessions() -> feed::Contribution {
    // `NAMESPACE_DROPS_SESSIONS` is a colon list of Claude session transcript
    // files. Entries alternate between the two seat groups.
    let mut raw = Vec::new();
    let mut heard = Vec::new();
    let sessions = std::env::var("NAMESPACE_DROPS_SESSIONS").unwrap_or_default();
    for (i, path) in sessions.split(':').filter(|p| !p.is_empty()).enumerate() {
        let group = (i % 2) as u8;
        ingest_claude(Path::new(path), group, &mut raw, &mut heard);
        ingest_subagents(
            Path::new(path).with_extension("").join("subagents"),
            group,
            &mut raw,
            &mut heard,
        );
    }
    ingest_codex(&mut raw, &mut heard);
    raw.sort_by(|a, b| a.ts.cmp(&b.ts));
    if raw.len() > 500 {
        raw.drain(0..raw.len() - 500);
    }

    let mut out = feed::Contribution::empty("sessions");
    let mut seen = std::collections::HashSet::new();
    for ev in &raw {
        seen.insert((ev.group, ev.ns.clone()));
        let (project, agent) = match ev.group {
            0 => ("weftos", "claude"),
            1 => ("example-project", "claude"),
            _ => ("example-project", "codex"),
        };
        out.signals.push(feed::Signal {
            id: format!("{}:{}", ev.group, ev.ns),
            group: ev.group,
            project: project.into(),
            agent: agent.into(),
            ns: ev.ns.clone(),
            kind: ev.kind.0,
            mass: ev.size.max(1),
            hits: 1,
            mag: magnitude(ev.kind, ev.size),
            order: Some(ev.ts.clone()),
            pace: 0.0,
            quality: 0.0,
            cadence: 0.0,
            coherence: 0.0,
            failing: ev.kind.0 == 5,
            label: String::new(),
            heat: 0.0,
            hot_ms: 0,
        });
    }
    let mut residents = Vec::new();
    for (i, path) in sessions.split(':').filter(|p| !p.is_empty()).enumerate() {
        let subs = Path::new(path).with_extension("").join("subagents");
        residents_from(&subs, (i % 2) as u8, &mut residents);
    }
    let groups: std::collections::HashSet<u8> = raw.iter().map(|e| e.group).collect();
    if groups.contains(&2) {
        residents.push((2, 8));
        residents.push((2, 9));
        residents.push((2, 18));
    }
    for (g, deg) in [(0u8, 2i32), (1, 4), (2, 8)] {
        if groups.contains(&g) && residents.iter().all(|r| r.0 != g) {
            residents.push((g, deg));
        }
    }
    out.residents = residents
        .into_iter()
        .map(|(group, degree)| feed::Resident {
            instrument: if group == 2 { 4 } else { 0 },
            stem: 0,
            degree,
        })
        .collect();
    if groups.contains(&0) {
        out.seats.push(seat_tone("weftos", "claude", 0, 0, 2));
    }
    if groups.contains(&1) {
        out.seats.push(seat_tone("example-project", "claude", 0, 0, 4));
    }
    if groups.contains(&2) {
        out.seats.push(seat_tone("example-project", "codex", 0, 4, 8));
    }
    heard.sort_by(|a, b| a.ts.cmp(&b.ts));
    if heard.len() > 80 {
        let drop_n = heard.len() - 80;
        heard.drain(0..drop_n);
    }
    out.phrases = heard
        .into_iter()
        .map(|h| feed::Phrase {
            order: h.ts,
            at: 0.0,
            clause: h.clause,
        })
        .collect();
    out.note = format!(
        "{} live namespaces. bodies stayed out. dot size is how much landed.",
        seen.len()
    );
    out
}

fn ingest_subagents(dir: PathBuf, group: u8, out: &mut Vec<Raw>, heard: &mut Vec<Heard>) {
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return;
    };
    let mut files: Vec<_> = rd
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .collect();
    files.sort_by_key(|p| {
        std::fs::metadata(p)
            .ok()
            .and_then(|m| m.modified().ok())
    });
    files.reverse();
    for p in files.into_iter().take(8) {
        ingest_claude(&p, group, out, heard);
    }
}

fn ingest_claude(path: &Path, group: u8, out: &mut Vec<Raw>, heard: &mut Vec<Heard>) {
    for line in tail_lines(path, TAIL) {
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if v.get("type").and_then(|t| t.as_str()) != Some("assistant") {
            continue;
        }
        let ts = v
            .get("timestamp")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();
        let Some(content) = v.pointer("/message/content").and_then(|c| c.as_array()) else {
            continue;
        };
        for block in content {
            let block_ty = block.get("type").and_then(|t| t.as_str());
            // Assistant prose only. Thinking and tool results stay out.
            if block_ty == Some("text") {
                if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                    push_heard(heard, &ts, text);
                }
                continue;
            }
            if block_ty != Some("tool_use") {
                continue;
            }
            let name = block.get("name").and_then(|n| n.as_str()).unwrap_or("tool");
            let kind = if name == "Bash" {
                let cmd = block
                    .pointer("/input/command")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                job_kind(cmd)
            } else {
                kind_of(name)
            };
            let ns = block
                .get("input")
                .and_then(|inp| prefix_from_input(name, inp))
                .unwrap_or_else(|| format!("tool/{name}"));
            out.push(Raw {
                ts: ts.clone(),
                group,
                ns,
                kind,
                size: line.len() as u32,
            });
        }
    }
}

fn ingest_codex(out: &mut Vec<Raw>, heard: &mut Vec<Heard>) {
    // `NAMESPACE_DROPS_CODEX_DIR` names a directory of Codex session files.
    let Ok(dir) = std::env::var("NAMESPACE_DROPS_CODEX_DIR") else {
        return;
    };
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return;
    };
    let mut files: Vec<_> = rd.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    files.retain(|p| {
        let Ok(mut f) = File::open(p) else {
            return false;
        };
        let mut buf = [0u8; 2400];
        let n = f.read(&mut buf).unwrap_or(0);
        let head = String::from_utf8_lossy(&buf[..n]);
        head.contains("example-project")
    });
    files.sort_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());
    files.reverse();
    for p in files.into_iter().take(4) {
        for line in tail_lines(&p, 500_000) {
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let ts = v
                .get("timestamp")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string();
            let payload = v.get("payload");
            let item_ty = payload
                .and_then(|p| p.pointer("/item/type"))
                .and_then(|t| t.as_str())
                .or_else(|| payload.and_then(|p| p.get("type")).and_then(|t| t.as_str()));
            match item_ty {
                Some("CommandExecution") => {
                    let failed = payload
                        .and_then(|p| p.pointer("/item/status"))
                        .and_then(|s| s.as_str())
                        == Some("failed");
                    // cwd only. The command string and its output stay out.
                    let cmd = payload
                        .and_then(|p| p.pointer("/item/command"))
                        .and_then(|c| c.as_str())
                        .unwrap_or("");
                    let ns = payload
                        .and_then(|p| p.pointer("/item/cwd"))
                        .and_then(|c| c.as_str())
                        .and_then(two_segments)
                        .unwrap_or_else(|| "codex/exec".to_string());
                    out.push(Raw {
                        ts,
                        group: 2,
                        ns,
                        kind: if failed { Kind::FAIL } else { job_kind(cmd) },
                        size: line.len() as u32,
                    });
                }
                Some("FileChange") => {
                    let mut landed = false;
                    if let Some(obj) = payload
                        .and_then(|p| p.pointer("/item/changes"))
                        .and_then(|c| c.as_object())
                    {
                        for path in obj.keys().take(6) {
                            let Some(ns) = two_segments(path) else {
                                continue;
                            };
                            out.push(Raw {
                                ts: ts.clone(),
                                group: 2,
                                ns,
                                kind: Kind::EDIT,
                                size: line.len() as u32,
                            });
                            landed = true;
                        }
                    }
                    if !landed {
                        out.push(Raw {
                            ts,
                            group: 2,
                            ns: "codex/file".to_string(),
                            kind: Kind::EDIT,
                            size: line.len() as u32,
                        });
                    }
                }
                Some("AgentMessage") => {
                    let Some(parts) = payload
                        .and_then(|p| p.pointer("/item/content"))
                        .and_then(|c| c.as_array())
                    else {
                        continue;
                    };
                    for part in parts {
                        if part.get("type").and_then(|t| t.as_str()) != Some("text") {
                            continue;
                        }
                        let Some(text) = part.get("text").and_then(|t| t.as_str()) else {
                            continue;
                        };
                        push_heard(heard, &ts, text);
                    }
                }
                Some("SubAgentActivity") | Some("function_call") => {
                    out.push(Raw {
                        ts,
                        group: 2,
                        ns: "codex/agent".to_string(),
                        kind: Kind::AGENT,
                        size: line.len() as u32,
                    });
                }
                _ => continue,
            }
        }
    }
}

/// Classify a command, then drop the text. `cargo test` stays a command.
fn job_kind(cmd: &str) -> Kind {
    let l = cmd.to_ascii_lowercase();
    let deploy = l.contains("deploy")
        || l.contains("flyctl")
        || l.contains("vercel")
        || l.contains("kubectl apply")
        || l.contains("helm upgrade")
        || l.contains("gh release")
        || l.contains("railway up");
    let ci = l.contains("gh workflow")
        || l.contains("gh run")
        || l.contains("buildkite")
        || l.contains("circleci")
        || l.contains("gitlab-ci")
        || l.split_whitespace().any(|w| w == "act");
    if deploy {
        Kind::DEPLOY
    } else if ci {
        Kind::CI
    } else {
        Kind::BASH
    }
}

fn kind_of(name: &str) -> Kind {
    match name {
        "Read" | "Grep" | "Glob" | "ToolSearch" => Kind::READ,
        "Edit" | "Write" | "StrReplace" | "NotebookEdit" => Kind::EDIT,
        "Bash" => Kind::BASH,
        "Agent" | "SendMessage" | "Task" => Kind::AGENT,
        _ => Kind::OTHER,
    }
}

/// Two path segments, or nothing. Command text is not kept.
fn prefix_from_input(name: &str, input: &Value) -> Option<String> {
    for key in ["path", "file_path", "target_file", "notebook_path"] {
        if let Some(p) = input
            .get(key)
            .and_then(|v| v.as_str())
            .and_then(two_segments)
        {
            return Some(p);
        }
    }
    if name == "Bash" {
        let cmd = input.get("command").and_then(|v| v.as_str())?;
        if cmd.contains("BEGIN ") || cmd.contains("API_KEY") || cmd.contains("sk-") {
            return None;
        }
        return bash_prefix(cmd);
    }
    None
}

/// Prefer a path inside the repo over the checkout root. The command text
/// itself is not kept.
fn bash_prefix(cmd: &str) -> Option<String> {
    let mut root: Option<String> = None;
    for tok in cmd.split_whitespace().take(40) {
        let Some(p) = two_segments(tok) else {
            continue;
        };
        if is_checkout_root(&p) {
            if root.is_none() {
                root = Some(p);
            }
            continue;
        }
        return Some(p);
    }
    root
}

fn is_checkout_root(p: &str) -> bool {
    matches!(
        p,
        "example-project/root"
            | "weftos/root"
            | "dev/null"
            | "bin/bash"
            | "bin/zsh"
            | "usr/bin"
            | "usr/local"
            | "var/folders"
            | "private/tmp"
    ) || p.starts_with("weftos-worktrees/")
}

/// Two segments inside a repo. `…/example-project/apps/web/…` becomes `apps/web`.
/// A checkout with nothing under it becomes `example-project/root` or `weftos/root`.
/// Shell assignments and redirects are not paths.
fn two_segments(raw: &str) -> Option<String> {
    let s = raw
        .trim()
        .trim_matches(|c| c == '"' || c == '\'' || c == ',' || c == ';');
    if s.contains("://")
        || s.len() > 220
        || s.contains('=')
        || s.contains('>')
        || s.contains('<')
        || s.contains('|')
        || s.contains('&')
        || s.contains('#')
        || s.contains('(')
        || s.contains(')')
        || s.contains('\'')
        || s.contains('`')
        || s.contains('{')
        || s.contains('}')
    {
        return None;
    }
    let s = s
        .strip_prefix("$HOME/")
        .or_else(|| s.strip_prefix("${HOME}/"))
        .unwrap_or(s);
    if s.contains('$') || s.contains("claude-501") {
        return None;
    }
    let s = s.strip_prefix("~/").unwrap_or(s);
    let s = std::env::var("HOME")
        .ok()
        .and_then(|h| s.strip_prefix(&format!("{}/", h.trim_end_matches('/'))).map(str::to_string))
        .unwrap_or_else(|| s.to_string());
    let s = s.as_str();
    let s = s.strip_prefix("/Volumes/ai-models/").unwrap_or(s);
    let s = s.strip_prefix("/private/").unwrap_or(s);
    let s = s.trim_start_matches('/').trim_start_matches("./");
    if let Some(ns) = after_anchor(s, ".codex/worktrees/")
        .and_then(|rest| rest.find('/').map(|i| &rest[i + 1..]))
        .and_then(|rest| rest.strip_prefix("example-project/").or(Some(rest)))
        .and_then(pair_or_root)
    {
        return Some(ns);
    }
    if let Some(ns) = after_anchor(s, "weftos-worktrees/")
        .and_then(|rest| rest.find('/').map(|i| &rest[i + 1..]))
        .and_then(pair_or_root)
    {
        return Some(ns);
    }
    if let Some(rest) = after_anchor(s, "example-project/") {
        return pair_or_root(rest).or_else(|| Some("example-project/root".to_string()));
    }
    if s.ends_with("/example-project") || s == "example-project" {
        return Some("example-project/root".to_string());
    }
    if let Some(rest) = after_anchor(s, "weftos/") {
        return pair_or_root(rest).or_else(|| Some("weftos/root".to_string()));
    }
    if s.ends_with("/weftos") || s == "weftos" {
        return Some("weftos/root".to_string());
    }
    pair_or_root(s)
}

fn after_anchor<'a>(s: &'a str, anchor: &str) -> Option<&'a str> {
    let marked = format!("/{anchor}");
    if let Some(i) = s.find(&marked) {
        return Some(s[i + marked.len()..].trim_start_matches('/'));
    }
    s.strip_prefix(anchor)
        .map(|rest| rest.trim_start_matches('/'))
}

fn pair_or_root(s: &str) -> Option<String> {
    let parts: Vec<&str> = s
        .split('/')
        .filter(|p| !p.is_empty() && *p != "~" && *p != ".")
        .collect();
    if parts.len() >= 2 {
        let (a, b) = (parts[0], parts[1]);
        if a.len() > 40
            || b.len() > 48
            || a.starts_with('.')
            || a.starts_with('-')
            || b.starts_with('-')
            || a.contains(':')
            || a.contains("Users")
            || b.contains("Users")
        {
            return None;
        }
        return Some(format!("{a}/{b}"));
    }
    if parts.is_empty() {
        return None;
    }
    None
}

fn tail_lines(path: &Path, max_bytes: u64) -> Vec<String> {
    let Ok(mut f) = File::open(path) else {
        return Vec::new();
    };
    let Ok(len) = f.metadata().map(|m| m.len()) else {
        return Vec::new();
    };
    let start = len.saturating_sub(max_bytes);
    if f.seek(SeekFrom::Start(start)).is_err() {
        return Vec::new();
    }
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).is_err() {
        return Vec::new();
    }
    // A mid-file seek can split a UTF-8 sequence. Lossy keeps the rest of the tail.
    let text = String::from_utf8_lossy(&buf);
    let mut lines: Vec<String> = text.lines().skip(1).map(|s| s.to_string()).collect();
    // Keep a line long enough to hold a diff, and throw the body away after
    // the path prefix is taken. Multi-megabyte lines are not a signal.
    lines.retain(|l| l.len() < 512_000);
    lines
}

fn push_heard(heard: &mut Vec<Heard>, ts: &str, text: &str) {
    for clause in clauses_from(text) {
        if heard.last().is_some_and(|h| h.clause == clause) {
            continue;
        }
        heard.push(Heard {
            ts: ts.to_string(),
            clause,
        });
    }
}

/// Assistant prose cut into breath-sized clauses. Code, tables, links, and
/// anything that looks like a secret are left out. A clause is at least four
/// words and at most twenty-two.
fn clauses_from(text: &str) -> Vec<String> {
    let mut prose = String::new();
    let mut fence = false;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with("```") {
            fence = !fence;
            continue;
        }
        if fence || t.is_empty() || t.starts_with('|') {
            continue;
        }
        if t.contains("http://") || t.contains("https://") {
            continue;
        }
        let t = t.trim_start_matches('#').trim();
        if t.is_empty() || symbol_heavy(t) || codeish(t) || pathish(t) || looks_secret(t) {
            continue;
        }
        prose.push_str(t);
        prose.push(' ');
    }
    let mut out = Vec::new();
    for sentence in split_sentences(&prose) {
        if looks_secret(&sentence) {
            continue;
        }
        let spoken = stream::plain_speech(&sentence);
        if spoken.is_empty() || looks_secret(&spoken) {
            continue;
        }
        let words: Vec<&str> = spoken.split_whitespace().collect();
        if words.len() < 4 {
            continue;
        }
        for piece in chunk_words(&words) {
            if out.len() >= 6 {
                return out;
            }
            out.push(piece);
        }
    }
    out
}

fn looks_secret(s: &str) -> bool {
    let l = s.to_ascii_lowercase();
    l.contains("sk-")
        || l.contains("api_key")
        || l.contains("api key")
        || l.contains("-----begin")
        || l.contains("bearer")
        || l.contains("token=")
        || l.contains("password")
        || l.contains("secret key")
        || l.contains("private key")
        || l.contains("api token")
        || l.contains("access token")
        || l.contains("auth token")
        || l.split(|c: char| !c.is_ascii_alphanumeric())
            .any(|w| matches!(w, "passwd" | "secret"))
}

fn symbol_heavy(t: &str) -> bool {
    let chars: Vec<char> = t.chars().filter(|c| !c.is_whitespace()).collect();
    if chars.len() < 8 {
        return false;
    }
    let letters = chars.iter().filter(|c| c.is_alphabetic()).count();
    letters * 100 / chars.len() < 62
}

fn codeish(t: &str) -> bool {
    let words: Vec<&str> = t.split_whitespace().collect();
    if words.is_empty() {
        return true;
    }
    let odd = words
        .iter()
        .filter(|w| w.contains("::") || w.contains("()") || w.contains("->") || w.contains("=>"))
        .count();
    odd * 2 >= words.len()
}

fn pathish(t: &str) -> bool {
    t.contains('/') && word_count(t) < 4
}

fn split_sentences(prose: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let chars: Vec<char> = prose.chars().collect();
    for (i, c) in chars.iter().enumerate() {
        cur.push(*c);
        let boundary =
            matches!(c, '.' | '!' | '?') && chars.get(i + 1).is_none_or(|n| n.is_whitespace());
        if boundary {
            let s = cur
                .trim()
                .trim_end_matches(['.', '!', '?'])
                .trim()
                .to_string();
            if !s.is_empty() {
                out.push(s);
            }
            cur.clear();
        }
    }
    let tail = cur
        .trim()
        .trim_end_matches(['.', '!', '?'])
        .trim()
        .to_string();
    if !tail.is_empty() {
        out.push(tail);
    }
    out
}

fn chunk_words(words: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < words.len() && out.len() < 6 {
        let rest = words.len() - i;
        let end = if rest <= 22 {
            words.len()
        } else {
            let window = (i + 22).min(words.len());
            words[i + 8..window]
                .iter()
                .rposition(|w| w.ends_with(','))
                .map(|rel| i + 8 + rel + 1)
                .unwrap_or((i + 16).min(words.len()))
        };
        if end <= i || end - i < 4 {
            break;
        }
        let piece = words[i..end]
            .join(" ")
            .trim()
            .trim_end_matches(',')
            .trim()
            .to_string();
        if word_count(&piece) >= 4 && !looks_secret(&piece) {
            out.push(piece);
        }
        i = end;
    }
    out
}

fn spike_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| {
            p.parent()
                .and_then(|p| p.parent())
                .and_then(|p| p.parent())
                .map(|p| p.to_path_buf())
        })
        .unwrap_or_else(|| PathBuf::from("target"))
}

fn mlx_up() -> bool {
    Command::new("curl")
        .args(["-fsS", "--max-time", "1", "http://127.0.0.1:7795/v1/health"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// 0 unknown, 1 ready, 2 given up. The weights stay up between lines. A quiet
/// launch does not start them; the first spoken line does, if they are down.
fn ensure_mlx() -> bool {
    static STATE: AtomicU8 = AtomicU8::new(0);
    if STATE.load(Ordering::Relaxed) == 1 || mlx_up() {
        STATE.store(1, Ordering::Relaxed);
        return true;
    }
    if STATE.load(Ordering::Relaxed) == 2 {
        return false;
    }
    let Ok(home) = std::env::var("NAMESPACE_DROPS_LLM_DIR") else {
        STATE.store(2, Ordering::Relaxed);
        return false;
    };
    let script = PathBuf::from(home).join("var/start-s2-mlx.sh");
    if !script.is_file() {
        STATE.store(2, Ordering::Relaxed);
        return false;
    }
    let _ = Command::new(&script)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    // The 8-bit weights take a few seconds to settle the first time.
    for _ in 0..180 {
        if mlx_up() {
            STATE.store(1, Ordering::Relaxed);
            return true;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    STATE.store(2, Ordering::Relaxed);
    false
}

/// Ginsberg on the warm MLX mouth. One wav, then afplay on top of the bed.
/// A failure leaves the line for Supertonic, then Reed.
fn mlx_say(line: &str) -> bool {
    if !ensure_mlx() {
        return false;
    }
    let body = serde_json::json!({ "text": line }).to_string();
    let wav = spike_dir().join("namespace-drops-mlx.wav");
    let wav_arg = wav.display().to_string();
    let Ok(mut child) = Command::new("curl")
        .args([
            "-sS",
            "--max-time",
            "30",
            "-o",
            &wav_arg,
            "-w",
            "%{http_code}",
            "-H",
            "content-type: application/json",
            "--data-binary",
            "@-",
            "http://127.0.0.1:7795/v1/tts",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    if let Some(mut stdin) = child.stdin.take()
        && stdin.write_all(body.as_bytes()).is_err()
    {
        return false;
    }
    let Ok(finished) = child.wait_with_output() else {
        return false;
    };
    if String::from_utf8_lossy(&finished.stdout).trim() != "200" || !wav.is_file() {
        return false;
    }
    Command::new("afplay")
        .arg(&wav)
        .status()
        .is_ok_and(|s| s.success())
}

fn tts_up() -> bool {
    Command::new("curl")
        .args(["-fsS", "--max-time", "1", "http://127.0.0.1:7791/v1/health"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// 0 unknown, 1 ready, 2 given up. The server is started once, on the first
/// phrase, so a quiet launch does not pay for the model.
fn ensure_tts() -> bool {
    static STATE: AtomicU8 = AtomicU8::new(0);
    if STATE.load(Ordering::Relaxed) == 1 || tts_up() {
        STATE.store(1, Ordering::Relaxed);
        return true;
    }
    if STATE.load(Ordering::Relaxed) == 2 {
        return false;
    }
    let home = std::env::var("NAMESPACE_DROPS_LLM_DIR").unwrap_or_default();
    let bin = PathBuf::from(home).join(".venv/bin/supertonic");
    if !bin.is_file() {
        STATE.store(2, Ordering::Relaxed);
        return false;
    }
    let dir = spike_dir();
    let _ = std::fs::create_dir_all(&dir);
    let mut cmd = Command::new(&bin);
    cmd.args([
        "serve",
        "--host",
        "127.0.0.1",
        "--port",
        "7791",
        "--model",
        "supertonic-3",
    ])
    .stdin(Stdio::null());
    match File::create(dir.join("namespace-drops-tts.log")) {
        Ok(log) => match log.try_clone() {
            Ok(err) => {
                cmd.stdout(Stdio::from(log));
                cmd.stderr(Stdio::from(err));
            }
            Err(_) => {
                cmd.stdout(Stdio::null());
                cmd.stderr(Stdio::null());
            }
        },
        Err(_) => {
            cmd.stdout(Stdio::null());
            cmd.stderr(Stdio::null());
        }
    }
    let Ok(child) = cmd.spawn() else {
        STATE.store(2, Ordering::Relaxed);
        return false;
    };
    let _ = std::fs::write(dir.join("namespace-drops-tts.pid"), child.id().to_string());
    for _ in 0..90 {
        if tts_up() {
            STATE.store(1, Ordering::Relaxed);
            return true;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    STATE.store(2, Ordering::Relaxed);
    false
}

/// Fish Audio S2 through `$NAMESPACE_DROPS_LLM_DIR/bin/fish-s2`. The wrapper writes a wav and
/// does not play it. Left off the live mouth: one sentence was still
/// synthesizing after several minutes, and every later line was dropped.
#[allow(dead_code)]
fn fish_say(line: &str) -> bool {
    let Ok(home) = std::env::var("NAMESPACE_DROPS_LLM_DIR") else {
        return false;
    };
    let bin = PathBuf::from(home).join("bin/fish-s2");
    if !bin.is_file() {
        return false;
    }
    let wav = spike_dir().join("namespace-drops-fish.wav");
    let wav_arg = wav.display().to_string();
    let ok = Command::new(&bin)
        .args(["say", "--out", &wav_arg, "--", line])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !ok || !wav.is_file() {
        return false;
    }
    Command::new("afplay")
        .arg(&wav)
        .status()
        .is_ok_and(|s| s.success())
}

fn supertonic_say(line: &str) -> bool {
    if !ensure_tts() {
        return false;
    }
    let body = serde_json::json!({
        "text": line,
        "voice": "M1",
        "lang": "en",
        "speed": 0.9,
        "steps": 6,
        "max_chunk_length": 400,
        "silence_duration": 0.05,
        "response_format": "wav",
    })
    .to_string();
    let wav = spike_dir().join("namespace-drops-voice.wav");
    let wav_arg = wav.display().to_string();
    let Ok(mut child) = Command::new("curl")
        .args([
            "-sS",
            "--max-time",
            "20",
            "-o",
            &wav_arg,
            "-w",
            "%{http_code}",
            "-H",
            "content-type: application/json",
            "--data-binary",
            "@-",
            "http://127.0.0.1:7791/v1/tts",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    if let Some(mut stdin) = child.stdin.take()
        && stdin.write_all(body.as_bytes()).is_err()
    {
        return false;
    }
    let Ok(finished) = child.wait_with_output() else {
        return false;
    };
    if String::from_utf8_lossy(&finished.stdout).trim() != "200" {
        return false;
    }
    Command::new("afplay")
        .arg(&wav)
        .status()
        .is_ok_and(|s| s.success())
}

fn come_forward(ctx: &egui::Context) {
    #[cfg(target_os = "macos")]
    raise_regular_app();
    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    ctx.send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(
        egui::UserAttentionType::Informational,
    ));
}

#[cfg(target_os = "macos")]
#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn TransformProcessType(psn: *const ProcessSerial, transform_state: u32) -> i32;
}

#[cfg(target_os = "macos")]
#[link(name = "objc")]
unsafe extern "C" {
    fn objc_getClass(name: *const std::ffi::c_char) -> *mut std::ffi::c_void;
    fn sel_registerName(name: *const std::ffi::c_char) -> *mut std::ffi::c_void;
    fn objc_msgSend();
}

#[cfg(target_os = "macos")]
#[repr(C)]
struct ProcessSerial {
    high: u32,
    low: u32,
}

/// Make a binary that was started under an agent into an ordinary app: Dock
/// icon, cmd-tab, and a focus request. Control Center is not involved.
#[cfg(target_os = "macos")]
fn raise_regular_app() {
    unsafe {
        // kCurrentProcess = {0, 2}. kProcessTransformToForegroundApplication = 1.
        let psn = ProcessSerial { high: 0, low: 2 };
        let _ = TransformProcessType(&psn, 1);

        let cls = objc_getClass(c"NSApplication".as_ptr());
        if cls.is_null() {
            return;
        }
        let send = objc_msgSend as *const ();
        let msg0: unsafe extern "C" fn(
            *mut std::ffi::c_void,
            *mut std::ffi::c_void,
        ) -> *mut std::ffi::c_void = std::mem::transmute(send);
        let app = msg0(cls, sel_registerName(c"sharedApplication".as_ptr()));
        if app.is_null() {
            return;
        }
        // NSApplicationActivationPolicyRegular = 0.
        let set_pol: unsafe extern "C" fn(
            *mut std::ffi::c_void,
            *mut std::ffi::c_void,
            isize,
        ) -> bool = std::mem::transmute(send);
        let _ = set_pol(app, sel_registerName(c"setActivationPolicy:".as_ptr()), 0);
        let act: unsafe extern "C" fn(*mut std::ffi::c_void, *mut std::ffi::c_void, bool) =
            std::mem::transmute(send);
        act(
            app,
            sel_registerName(c"activateIgnoringOtherApps:".as_ptr()),
            true,
        );
        let run_cls = objc_getClass(c"NSRunningApplication".as_ptr());
        if run_cls.is_null() {
            return;
        }
        let cur = msg0(run_cls, sel_registerName(c"currentApplication".as_ptr()));
        if cur.is_null() {
            return;
        }
        // AllWindows | IgnoringOtherApps.
        let opt: unsafe extern "C" fn(*mut std::ffi::c_void, *mut std::ffi::c_void, usize) =
            std::mem::transmute(send);
        opt(cur, sel_registerName(c"activateWithOptions:".as_ptr()), 3);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sentence_becomes_one_clause_and_a_secret_does_not() {
        let got = clauses_from("The panel now draws the live sessions in the left band.");
        assert_eq!(
            got,
            vec!["The panel now draws the live sessions in the left band".to_string()]
        );
        assert!(clauses_from("set API_KEY=sk-live and the token=abc").is_empty());
        assert!(clauses_from("```\nfn main() {}\n```").is_empty());
    }

    #[test]
    fn a_long_line_stays_inside_one_breath() {
        let text = "one two three four five six seven eight nine ten eleven twelve \
                    thirteen fourteen fifteen sixteen. seventeen eighteen nineteen twenty \
                    twentyone twentytwo twentythree twentyfour.";
        let got = clauses_from(text);
        assert!(!got.is_empty());
        assert!(got.iter().all(|c| word_count(c) <= 22));
        assert!(got.iter().all(|c| word_count(c) >= 4));
    }

    #[test]
    fn dragging_up_looks_down_on_the_plane() {
        // The old rail stopped at 1.28. A further upward drag still climbs.
        let parked = 1.28f32;
        let climbed = orbit_pitch(parked, -8.0);
        assert!(
            (climbed - (parked + 8.0 * ORBIT_GAIN)).abs() < 1e-4,
            "{climbed}"
        );
        let mut high = 0.3f32;
        for _ in 0..40 {
            high = orbit_pitch(high, -6.0);
        }
        assert!((high - PITCH_MAX).abs() < 1e-3, "{high}");
        assert!(high < std::f32::consts::FRAC_PI_2);
        let mut low = 0.22f32;
        for _ in 0..40 {
            low = orbit_pitch(low, 6.0);
        }
        assert!((low - PITCH_MIN).abs() < 1e-3, "{low}");
        assert!(low < 0.0);
        let plot = Rect::from_min_max(Pos2::ZERO, Pos2::new(800.0, 800.0));
        let look = Vec2::new(0.5, 0.5);
        let at = |y: f32, py: f32, elev: f32| {
            project(plot, Pos2::new(0.5, py), y, look, 0.0, elev, 1.0, 0.0)
                .unwrap()
                .0
        };
        let down_base = at(0.0, 0.5, PITCH_MAX);
        let down_top = at(0.40, 0.5, PITCH_MAX);
        let side_base = at(0.0, 0.5, 0.3);
        let side_top = at(0.40, 0.5, 0.3);
        let up_base = at(0.0, 0.5, -0.6);
        let up_top = at(0.40, 0.5, -0.6);
        assert!(down_top.y < down_base.y, "straight down stays upright");
        assert!(side_top.y < side_base.y, "the side view stays upright");
        assert!(up_top.y < up_base.y, "looking up stays upright");
        assert!(
            (side_base.y - side_top.y) > (down_base.y - down_top.y) * 3.0,
            "overhead shortens the stalks"
        );
        // The gaze stays on the look point at every elevation. Pitch
        // does not walk the camera backward.
        let mid_side = at(0.0, 0.5, 0.2);
        assert!((mid_side.x - down_base.x).abs() < 1.0);
        assert!((mid_side.y - down_base.y).abs() < 1.0);
    }

    #[test]
    fn litany_opens_with_one_who() {
        let line = breath_line(
            Voice::Litany,
            &["The stalk rose".into(), "The bead fell".into()],
        );
        assert_eq!(line, "who the stalk rose, and the bead fell,");
        assert_eq!(line.matches("who").count(), 1);
        let census = breath_line(Voice::Census, &["The stalk rose".into()]);
        assert_eq!(census, "The stalk rose.");
        assert!(
            !census
                .to_ascii_lowercase()
                .contains("this is what was said")
        );
        assert!(!census.contains("who"));
    }

    #[test]
    fn spoken_lines_wait_in_order_with_a_short_gap() {
        let mut queue = VecDeque::new();
        assert!(enqueue_line(&mut queue, "the stalk rose"));
        assert!(enqueue_line(&mut queue, "and the field held"));
        assert!(!enqueue_line(&mut queue, "   "));
        assert_eq!(queue.pop_front().as_deref(), Some("the stalk rose"));
        assert_eq!(queue.pop_front().as_deref(), Some("and the field held"));
        assert!(queue.is_empty());
        assert_eq!(SPEECH_GAP, Duration::from_millis(450));
    }

    #[test]
    fn a_flood_keeps_the_latest_breath() {
        let mut pending: Vec<String> = (0..8)
            .map(|i| format!("clause number {i} lands here"))
            .collect();
        shed_to_fit(&mut pending);
        assert!(pending.len() <= BREATH_CLAUSES);
        assert!(pending.last().unwrap().contains('7'));
        assert!(!pending.iter().any(|c| c.contains("number 0")));
        let words: usize = pending.iter().map(|c| word_count(c)).sum();
        assert!(words <= BREATH_WORDS);
        let taken = take_breath(&mut pending);
        assert!(!taken.is_empty());
        assert!(taken.len() <= BREATH_CLAUSES);
    }

    #[test]
    fn numbers_paths_and_a_hash_leave_the_spoken_clause() {
        let got = clauses_from(
            "Updated EX-1842 at line 423 in src/foo.rs commit a1b2c3d4e5 and the suite exited 101.",
        );
        let joined = got.join(" ");
        assert!(!got.is_empty(), "{joined}");
        assert!(
            got.iter().all(|c| !c.chars().any(|ch| ch.is_ascii_digit())),
            "{joined}"
        );
        assert!(!joined.contains('/'), "{joined}");
        assert!(!joined.to_ascii_lowercase().contains("commit"), "{joined}");
        assert!(joined.contains("suite"), "{joined}");
        assert!(joined.contains("foo"), "{joined}");
    }

    #[test]
    fn deploy_and_ci_are_their_own_kinds_and_cargo_test_is_not() {
        assert_eq!(job_kind("flyctl deploy -a app").0, Kind::DEPLOY.0);
        assert_eq!(job_kind("gh run watch").0, Kind::CI.0);
        assert_eq!(job_kind("kubectl apply -f ship.yaml").0, Kind::DEPLOY.0);
        assert_eq!(job_kind("cargo test -p clawft-gui-egui").0, Kind::BASH.0);
    }
}
