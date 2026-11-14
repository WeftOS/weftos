//! Live mix. Volumes, tempo, the ear, and the vocal flag are atomics, so a
//! slider or an orbit moves the room on the next buffer. One beat clock
//! drives the brushes, the walking bass, the chords, and the melody.
//! A landing is a plucked string on the visual replay: its pitch is the
//! length of the name on that stalk, and its place in the stereo field is
//! the stalk's place under the camera.

use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI8, AtomicU8, AtomicU16, AtomicU32, Ordering};

use super::feed::{self, Resident};

const SR: f32 = 44_100.0;
/// About a fifth of a second. The failure sea stretches inside this.
const WASH: usize = 8192;
const FRAMES: usize = 512;
const DORIAN: [i32; 7] = [0, 2, 3, 5, 7, 9, 10];
const COLOR: [i32; 7] = [0, 2, 3, 6, 7, 9, 10];
/// 0 dorian, 1 mixolydian, 2 lydian, 3 melodic minor, 4 aeolian. All rooted on D.
const MODES: [[i32; 7]; 5] = [
    DORIAN,
    [0, 2, 4, 5, 7, 9, 10],
    [0, 2, 4, 6, 7, 9, 11],
    [0, 2, 3, 5, 7, 9, 11],
    [0, 2, 3, 5, 7, 8, 10],
];
const PATTERNS: [[i32; 8]; 5] = [
    [0, 1, 2, 3, 4, 3, 2, 0],
    [2, 0, 3, 1, 4, 2, 0, 0],
    [4, 3, 2, 4, 1, 2, 0, 0],
    [0, 2, 4, 2, 1, 0, -1, 0],
    [5, 4, 3, 2, 1, 0, 1, 0],
];
const PROGRESS: [[i32; 4]; 5] = [
    [0, 6, 3, 0],
    [0, 6, 3, 4],
    [0, 1, 4, 0],
    [0, 3, 6, 0],
    [0, 5, 2, 0],
];
/// Ionian. Degree 0 is the same semitone as dorian degree 0, so home stays D.
const IONIAN: [i32; 7] = [0, 2, 4, 5, 7, 9, 11];
const MIXOLYDIAN: [i32; 7] = [0, 2, 4, 5, 7, 9, 10];
const PHRYGIAN: [i32; 7] = [0, 1, 3, 5, 7, 8, 10];
const HARMONIC_MINOR: [i32; 7] = [0, 2, 3, 5, 7, 8, 11];
/// Minor pentatonic written on seven degrees. The last two sit up an octave.
const MINOR_PENTA: [i32; 7] = [0, 3, 5, 7, 10, 12, 15];
const WHOLE_TONE: [i32; 7] = [0, 2, 4, 6, 8, 10, 12];

#[derive(Clone, Copy)]
pub struct Event {
    pub at: f32,
    pub kind: u8,
    pub mag: f32,
    pub group: u8,
    /// Ground position of the stalk this landing belongs to.
    pub x: f32,
    pub y: f32,
    /// Sounding length. Longer is lower. See [`sounding_len`].
    pub len: u16,
}

struct Ctl {
    on: AtomicBool,
    speaking: Arc<AtomicBool>,
    failing: AtomicBool,
    /// Share of the field that is failing, 0 to 1. The wash listens to this.
    err: AtomicU32,
    master: AtomicU32,
    vol: [AtomicU32; 9],
    /// Bit i set means stem i is sounding. Stem 8 is the half-time drum.
    gate: AtomicU16,
    inst: [AtomicU8; 8],
    bpm: AtomicU32,
    brush: AtomicU32,
    pace: AtomicU32,
    cadence: AtomicU32,
    coherence: AtomicU32,
    /// Listener on the ground plane. The picture writes these every frame.
    ear_x: AtomicU32,
    ear_y: AtomicU32,
    ear_yaw: AtomicU32,
    ear_pitch: AtomicU32,
    /// Code-graph chord. The picture turns this off with the checkbox.
    graph: AtomicBool,
    /// Added to the cadence tempo. The reader sets this. It does not replace the cadence.
    nudge: AtomicI8,
    /// 0 keeps the Coltrane scale. 1 dorian, 2 mixolydian, 3 lydian, 4 aeolian, 5 ionian.
    scale_hint: AtomicU8,
}

pub struct Player {
    ctl: Arc<Ctl>,
    aq: *mut c_void,
    raw: *mut Inner,
}

unsafe impl Send for Player {}

struct Inner {
    ctl: Arc<Ctl>,
    voice: Voice,
    events: Vec<Ev>,
}

#[derive(Clone, Copy)]
struct Ev {
    frame: u64,
    kind: u8,
    mag: f32,
    x: f32,
    y: f32,
    len: u16,
}

/// One plucked string. The buffer length is the pitch.
struct StringVoice {
    on: bool,
    age: f32,
    x: f32,
    y: f32,
    amp: f32,
    damp: f32,
    n: usize,
    i: usize,
    buf: [f32; 640],
}

#[derive(Clone, Copy)]
struct Pluck {
    on: bool,
    wait: f32,
    age: f32,
    freq: f32,
    amp: f32,
    decay: f32,
    phase: f32,
    inst: u8,
    /// 4 landings, 5 ci and deploy. Each has its own volume.
    stem: u8,
    noise: f32,
}

struct Voice {
    sample: u64,
    prev: u64,
    ev_i: usize,
    bpm: f32,
    beat: f64,
    beat_i: i64,
    phrase: i64,
    root: i32,
    mel: i32,
    rng: u32,
    was_speaking: bool,
    fill: f32,
    kick: f32,
    /// Age of the half-time drum. Starts high so the room is quiet until a hit.
    drum: f32,
    drum_phase: f32,
    brush: f32,
    ghost: f32,
    noise_z: f32,
    /// Two-pole wash, a slow swell, and a short stretch of the failure sea.
    wash_a: f32,
    wash_b: f32,
    wash_lfo: f32,
    wash_i: usize,
    wash_buf: [f32; WASH],
    residents: [(f32, u8, u8); 6],
    n_res: usize,
    approach: f32,
    /// Beats remaining before the release figure. Zero means none is due.
    release_left: i32,
    /// Beats of tonic after the release figure.
    resolve: i32,
    /// Live heat, decaying with a 60 second half-life.
    heat: f32,
    plucks: [Pluck; 24],
    strings: [StringVoice; 12],
    /// Connectors from the code graph: ground midpoint and world length.
    graph_n: usize,
    graph: [(f32, f32, f32); 16],
}

impl Player {
    pub fn start(
        events: &[Event],
        residents: &[Resident],
        bpm: f32,
        pace: f32,
        cadence: f32,
        coherence: f32,
        failing: bool,
        approach: f32,
        release: bool,
        heat: f32,
        graph: &[(f32, f32, f32)],
    ) -> Self {
        let ctl = Arc::new(Ctl {
            on: AtomicBool::new(true),
            speaking: Arc::new(AtomicBool::new(false)),
            failing: AtomicBool::new(failing),
            err: AtomicU32::new(0f32.to_bits()),
            master: AtomicU32::new(0.8f32.to_bits()),
            vol: [0.7f32, 0.55, 0.4, 0.45, 0.6, 0.5, 0.45, 0.35, 0.58]
                .map(|v| AtomicU32::new(v.to_bits())),
            gate: AtomicU16::new(0x01ff),
            inst: [0, 1, 2, 0, 3, 4, 0, 0].map(AtomicU8::new),
            bpm: AtomicU32::new(bpm.clamp(60.0, 140.0).to_bits()),
            brush: AtomicU32::new(0.55f32.to_bits()),
            pace: AtomicU32::new(pace.to_bits()),
            cadence: AtomicU32::new(cadence.to_bits()),
            coherence: AtomicU32::new(coherence.to_bits()),
            ear_x: AtomicU32::new(0.5f32.to_bits()),
            ear_y: AtomicU32::new(0.5f32.to_bits()),
            ear_yaw: AtomicU32::new(0.2f32.to_bits()),
            ear_pitch: AtomicU32::new(0.58f32.to_bits()),
            graph: AtomicBool::new(true),
            nudge: AtomicI8::new(0),
            scale_hint: AtomicU8::new(0),
        });
        let period = (feed::REPLAY_SECS * SR) as u64;
        let mut evs: Vec<Ev> = events
            .iter()
            .map(|e| Ev {
                frame: ((e.at.clamp(0.0, 0.999) * period as f32) as u64)
                    .min(period.saturating_sub(1)),
                kind: e.kind,
                mag: e.mag,
                x: e.x,
                y: e.y,
                len: e.len.max(1),
            })
            .collect();
        evs.sort_by_key(|e| e.frame);
        let mut voice = Voice::new(bpm);
        for row in graph.iter().take(voice.graph.len()) {
            if row.2.is_finite() && row.2 > 0.02 {
                voice.graph[voice.graph_n] = *row;
                voice.graph_n += 1;
            }
        }
        voice.approach = approach.clamp(0.0, 1.0);
        voice.heat = heat.clamp(0.0, 4.0);
        // One phrase of build, then the figure on the next downbeat.
        voice.release_left = if release { 17 } else { 0 };
        for (i, r) in residents.iter().take(6).enumerate() {
            voice.residents[i] = (hz(r.degree), r.instrument.min(4), r.stem.min(7));
            voice.n_res = i + 1;
        }
        let inner = Box::new(Inner {
            ctl: Arc::clone(&ctl),
            voice,
            events: evs,
        });
        let raw = Box::into_raw(inner);
        let aq = start_queue(raw);
        Self { ctl, aq, raw }
    }

    pub fn speaking_handle(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.ctl.speaking)
    }

    pub fn set_enabled(&self, on: bool) {
        self.ctl.on.store(on, Ordering::Relaxed);
    }

    pub fn set_mix(
        &self,
        master: f32,
        vol: &[f32; 9],
        inst: &[u8; 8],
        brush: f32,
        failing: bool,
        err: f32,
        gates: u16,
    ) {
        self.ctl.master.store(master.to_bits(), Ordering::Relaxed);
        for i in 0..9 {
            self.ctl.vol[i].store(vol[i].to_bits(), Ordering::Relaxed);
        }
        for i in 0..8 {
            self.ctl.inst[i].store(inst[i], Ordering::Relaxed);
        }
        self.ctl.brush.store(brush.to_bits(), Ordering::Relaxed);
        self.ctl.failing.store(failing, Ordering::Relaxed);
        self.ctl
            .err
            .store(err.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
        self.ctl.gate.store(gates, Ordering::Relaxed);
    }

    /// Where the camera is. Landings pan and thin from this on the next buffer.
    pub fn set_ear(&self, x: f32, y: f32, yaw: f32, pitch: f32) {
        self.ctl.ear_x.store(x.to_bits(), Ordering::Relaxed);
        self.ctl.ear_y.store(y.to_bits(), Ordering::Relaxed);
        self.ctl.ear_yaw.store(yaw.to_bits(), Ordering::Relaxed);
        self.ctl.ear_pitch.store(pitch.to_bits(), Ordering::Relaxed);
    }

    pub fn set_graph(&self, on: bool) {
        self.ctl.graph.store(on, Ordering::Relaxed);
    }

    /// The reader's steer. Tempo is a nudge on the cadence. Scale 0 leaves
    /// Coltrane in charge. A release still comes home.
    pub fn set_steer(&self, tempo: i8, scale: u8) {
        self.ctl.nudge.store(tempo.clamp(-8, 8), Ordering::Relaxed);
        self.ctl.scale_hint.store(scale.min(5), Ordering::Relaxed);
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.ctl.on.store(false, Ordering::Relaxed);
        unsafe {
            if !self.aq.is_null() {
                AudioQueueStop(self.aq, 1);
                AudioQueueDispose(self.aq, 1);
                self.aq = std::ptr::null_mut();
            }
            if !self.raw.is_null() {
                drop(Box::from_raw(self.raw));
                self.raw = std::ptr::null_mut();
            }
        }
    }
}

impl Voice {
    fn new(bpm: f32) -> Self {
        Self {
            sample: 0,
            prev: 0,
            ev_i: 0,
            bpm: bpm.clamp(60.0, 140.0),
            beat: 0.0,
            beat_i: -1,
            phrase: 0,
            root: 0,
            mel: 4,
            rng: 0xA5A5_1234,
            was_speaking: false,
            fill: 0.0,
            kick: 1.0,
            drum: 4.0,
            drum_phase: 0.0,
            brush: 1.0,
            ghost: 1.0,
            noise_z: 0.0,
            wash_a: 0.0,
            wash_b: 0.0,
            wash_lfo: 0.0,
            wash_i: 0,
            wash_buf: [0.0; WASH],
            residents: [(0.0, 0, 0); 6],
            n_res: 0,
            approach: 0.0,
            release_left: 0,
            resolve: 0,
            heat: 0.0,
            plucks: [Pluck::off(); 24],
            strings: [const { StringVoice::off() }; 12],
            graph_n: 0,
            graph: [(0.0, 0.0, 0.0); 16],
        }
    }
}

impl StringVoice {
    const fn off() -> Self {
        Self {
            on: false,
            age: 0.0,
            x: 0.0,
            y: 0.0,
            amp: 0.0,
            damp: 0.9998,
            n: 0,
            i: 0,
            buf: [0.0; 640],
        }
    }
}

impl Pluck {
    const fn off() -> Self {
        Self {
            on: false,
            wait: 0.0,
            age: 0.0,
            freq: 0.0,
            amp: 0.0,
            decay: 0.2,
            phase: 0.0,
            inst: 0,
            stem: 4,
            noise: 0.0,
        }
    }
}

fn bits(a: &AtomicU32) -> f32 {
    f32::from_bits(a.load(Ordering::Relaxed))
}

/// Share of the field that is failing. None is silence. One failure still
/// washes, and a field that is mostly red washes hardest.
pub fn error_rate(failing: usize, field: usize) -> f32 {
    if failing == 0 || field == 0 {
        return 0.0;
    }
    (failing as f32 / field as f32).clamp(1.0 / 24.0, 1.0)
}

/// Failure noise as surf. A two-pole wash darkens it, a slow LFO swells the
/// volume, and a lag stretches it. A higher error rate is darker, deeper,
/// and longer.
fn wash(v: &mut Voice, rate: f32, dt: f32) -> (f32, f32) {
    if rate <= 0.0 {
        return (0.0, 0.0);
    }
    let n = noise(&mut v.rng);
    let c = 0.03 + (1.0 - rate) * 0.17;
    v.wash_a += c * (n - v.wash_a);
    v.wash_b += c * (v.wash_a - v.wash_b);
    let lag = ((0.03 + rate * 0.12) * SR) as usize;
    let lag = lag.clamp(8, WASH / 2);
    let stretch = (lag + ((0.02 + rate * 0.05) * SR) as usize).min(WASH - 2);
    let i = v.wash_i & (WASH - 1);
    let prev = v.wash_buf[(i + WASH - lag) & (WASH - 1)];
    let far = v.wash_buf[(i + WASH - stretch) & (WASH - 1)];
    let fb = 0.15 + rate * 0.5;
    // The darker wash is quieter. Give the worse hour its weight back.
    let drive = 1.2 + rate * 5.0;
    v.wash_buf[i] = (v.wash_b * drive + prev * fb).clamp(-1.2, 1.2);
    v.wash_i = v.wash_i.wrapping_add(1);
    let wet = prev * (1.0 - 0.55 * rate) + far * (0.55 * rate);
    let hz = 0.08 + rate * 0.35;
    v.wash_lfo = (v.wash_lfo + hz * dt).fract();
    let depth = 0.25 + 0.65 * rate;
    let swell = (1.0 - depth) + depth * (0.5 + 0.5 * (v.wash_lfo * std::f32::consts::TAU).sin());
    let y = wet * swell * (0.4 + 0.6 * rate);
    let side = (far - prev) * swell * (0.1 + 0.35 * rate);
    (y + side, y - side)
}

/// Diatonic walk: keep the sequence, optionally invert it, then transpose.
pub fn develop(seq: &[i32], invert: bool, shift: i32) -> Vec<i32> {
    let mut out: Vec<i32> = seq.to_vec();
    if invert {
        out.reverse();
    }
    for degree in &mut out {
        *degree = (*degree + shift).rem_euclid(7);
    }
    out
}

pub fn hz(degree: i32) -> f32 {
    let scale = &DORIAN;
    let d = degree.rem_euclid(7);
    let oct = degree.div_euclid(7);
    let semi = scale[d as usize] + 12 * oct;
    146.83 * 2f32.powf(semi as f32 / 12.0)
}

fn hz_scale(scale: &[i32; 7], degree: i32) -> f32 {
    let d = degree.rem_euclid(7);
    let oct = degree.div_euclid(7);
    let semi = scale[d as usize] + 12 * oct;
    146.83 * 2f32.powf(semi as f32 / 12.0)
}

/// Countdown into home: +8, +4, 0. A major third apart, and 8 + 4 wraps home.
pub fn station_shift(bar: i64) -> i32 {
    match bar.rem_euclid(3) {
        0 => 8,
        1 => 4,
        _ => 0,
    }
}

pub fn placed(scale: &[i32; 7], degree: i32, shift: i32) -> f32 {
    hz_scale(scale, degree) * 2f32.powf(shift as f32 / 12.0)
}

/// Characters in the namespace, plus the beads already on the stalk.
/// A taller stalk is a longer string.
pub fn sounding_len(chars: u32, beads: u32) -> u32 {
    chars.max(1).saturating_add(beads.min(36))
}

/// Longer string, lower degree. Short names sit up high.
pub fn string_degree(len: u32) -> i32 {
    let n = (len.max(1) as i32).min(72);
    let t = (n - 1) as f32 / 71.0;
    (11.0 - t * 18.0).round() as i32
}

pub fn string_hz(scale: &[i32; 7], len: u32, shift: i32) -> f32 {
    placed(scale, string_degree(len), shift)
}

/// Root, third, fifth, seventh. The quality follows whichever scale is current.
const CHORD_INTERVALS: [i32; 4] = [0, 2, 4, 6];

/// Voice connectors as a chord. Shortest string is the root, longest is the
/// seventh. Each note is an interval on the scale, not a free pitch.
pub fn voice_graph(strings: &[(f32, f32, f32)]) -> (usize, [(f32, f32, i32); 4]) {
    let mut rows = [(0.0f32, 0.0f32, 0.0f32); 16];
    let mut n = 0usize;
    for s in strings.iter().take(rows.len()) {
        if s.2.is_finite() && s.2 > 0.02 {
            rows[n] = *s;
            n += 1;
        }
    }
    if n == 0 {
        return (0, [(0.0, 0.0, 0); 4]);
    }
    rows[..n].sort_by(|a, b| a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal));
    let k = n.min(CHORD_INTERVALS.len());
    let mut picked = [(0.0f32, 0.0f32, 0.0f32); 4];
    let mut c = 0usize;
    for i in 0..k {
        let at = if k == 1 { 0 } else { i * (n - 1) / (k - 1) };
        let row = rows[at];
        let crowded = picked[..c]
            .iter()
            .any(|p| (p.0 - row.0).hypot(p.1 - row.1) < 0.04);
        if crowded {
            continue;
        }
        picked[c] = row;
        c += 1;
    }
    picked[..c].sort_by(|a, b| a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal));
    let mut out = [(0.0f32, 0.0f32, 0i32); 4];
    for i in 0..c {
        out[i] = (picked[i].0, picked[i].1, CHORD_INTERVALS[i]);
    }
    (c, out)
}

pub fn string_samples(hz: f32) -> usize {
    let n = (SR / hz.clamp(70.0, 1400.0)).round() as usize;
    n.clamp(32, 640)
}

/// Pan is screen-right, matching the picture's yaw. Gain falls with distance
/// from the look point.
pub fn ear(x: f32, y: f32, look_x: f32, look_y: f32, yaw: f32) -> (f32, f32) {
    let dx = x - look_x;
    let dz = y - look_y;
    let (sy, cy) = yaw.sin_cos();
    let xr = dx * cy - dz * sy;
    let zr = dx * sy + dz * cy;
    let dist = (xr * xr + zr * zr).sqrt();
    let pan = if dist < 1.0e-4 {
        0.0
    } else {
        (xr / dist).clamp(-1.0, 1.0)
    };
    let gain = 1.0 / (1.0 + dist * 4.0);
    (pan, gain)
}

/// Looking down opens the strings. Looking up thins them.
pub fn ear_body(pitch: f32) -> f32 {
    let t = ((pitch + 1.45) / 3.0).clamp(0.0, 1.0);
    0.35 + 0.65 * t
}

pub fn split_ear(sample: f32, pan: f32) -> (f32, f32) {
    let pan = pan.clamp(-1.0, 1.0);
    let angle = (pan + 1.0) * 0.25 * std::f32::consts::PI;
    (sample * angle.cos(), sample * angle.sin())
}

/// Sounding scale, drone scale, station shift, sounding shift, chord degree.
/// While a release is resolving, the shift is 0 and the scale is dorian.
fn coltrane_key(
    beat: i64,
    resolving: bool,
    coherence: f32,
    root: i32,
) -> (&'static [i32; 7], &'static [i32; 7], i32, i32, i32) {
    if resolving {
        let scale: &'static [i32; 7] = if coherence < 0.45 { &COLOR } else { &DORIAN };
        return (scale, scale, 0, 0, root.rem_euclid(7));
    }
    let beat = beat.max(0);
    let station = station_shift(beat / 4);
    let dominant = beat.rem_euclid(4) < 2;
    let (scale, drone) = color_for(beat, dominant, coherence);
    let sound = if dominant { station + 7 } else { station };
    (scale, drone, station, sound, 0)
}

/// The first phrase keeps the Coltrane half-bar. Each phrase after that
/// picks another scale, still on D, the way the FM-1 vamp changes color
/// without restarting the clock.
fn color_for(beat: i64, dominant: bool, coherence: f32) -> (&'static [i32; 7], &'static [i32; 7]) {
    if coherence < 0.45 {
        return (&COLOR, &COLOR);
    }
    let phrase = beat.max(0) / 16;
    match phrase.rem_euclid(8) {
        0 => {
            if dominant {
                (&MIXOLYDIAN, &IONIAN)
            } else {
                (&IONIAN, &IONIAN)
            }
        }
        1 => (&DORIAN, &DORIAN),
        2 => (&MODES[2], &MODES[2]),
        3 => (&MODES[4], &MODES[4]),
        4 => (&PHRYGIAN, &PHRYGIAN),
        5 => (&MODES[3], &MODES[3]),
        6 => (&HARMONIC_MINOR, &HARMONIC_MINOR),
        _ => {
            if phrase.div_euclid(8).rem_euclid(2) == 0 {
                (&MINOR_PENTA, &MINOR_PENTA)
            } else {
                (&WHOLE_TONE, &WHOLE_TONE)
            }
        }
    }
}

/// How full the bar is. Heat, cadence, and the release approach raise it.
fn room_density(cadence: f32, approach: f32, heat: f32, pace: f32) -> f32 {
    (0.34 + 0.28 * cadence + 0.20 * heat + 0.18 * approach + 0.12 * pace).clamp(0.28, 0.92)
}

/// Bjorklund spacing on at most 16 steps, then a rotation. Same idea as the
/// other-project euclidean sequencer and the FM-1 density control: more pulses
/// means more hits, and the rotation moves them.
pub fn euclid_hit(pulses: usize, steps: usize, rotation: usize, step: usize) -> bool {
    let steps = steps.clamp(1, 16);
    let pulses = pulses.min(steps);
    if pulses == 0 {
        return false;
    }
    let mut pat = [0u8; 16];
    if pulses >= steps {
        pat[..steps].fill(1);
    } else {
        let mut slope = 0isize;
        for i in 0..steps {
            slope += pulses as isize;
            if slope >= steps as isize {
                slope -= steps as isize;
                pat[i] = 1;
            }
        }
    }
    let at = (step.wrapping_add(rotation)) % steps;
    pat[at] == 1
}

/// Seventh, sixth, sus, cluster, and an upper structure. One shape per phrase.
fn voicing(phrase: i64) -> [i32; 4] {
    const SHAPES: [[i32; 4]; 6] = [
        [0, 2, 4, 6],
        [0, 2, 4, 5],
        [0, 3, 5, 6],
        [0, 2, 3, 6],
        [0, 1, 4, 6],
        [2, 4, 6, 8],
    ];
    SHAPES[phrase.rem_euclid(6) as usize]
}

/// Cadence tempo plus the reader's nudge, still inside the band.
fn steer_target(bpm: f32, nudge: i8) -> f32 {
    (bpm + f32::from(nudge)).clamp(60.0, 140.0)
}

/// Coltrane still picks the station. A hint replaces only the scale, and a
/// resolving release ignores the hint so home stays dorian.
fn sounding_scales(
    beat: i64,
    resolving: bool,
    coherence: f32,
    root: i32,
    hint: u8,
) -> (&'static [i32; 7], &'static [i32; 7], i32, i32, i32) {
    let (scale, drone, station, sound, chord) = coltrane_key(beat, resolving, coherence, root);
    if resolving {
        return (scale, drone, station, sound, chord);
    }
    match hint {
        1 => (&DORIAN, &DORIAN, station, sound, chord),
        2 => (&MIXOLYDIAN, &MIXOLYDIAN, station, sound, chord),
        3 => (&MODES[2], &MODES[2], station, sound, chord),
        4 => (&MODES[4], &MODES[4], station, sound, chord),
        5 => (&IONIAN, &IONIAN, station, sound, chord),
        _ => (scale, drone, station, sound, chord),
    }
}

fn half_time(beat: i64) -> bool {
    beat.rem_euclid(8) == 0
}

fn stem_gain(on: bool, vol: f32) -> f32 {
    if on { vol } else { 0.0 }
}

fn gated(mask: u16, stem: usize, vol: f32) -> f32 {
    stem_gain((mask & (1u16 << stem)) != 0, vol)
}

fn boom(age: f32, phase: f32) -> f32 {
    let env = (-age / 0.42).exp() * (age / 0.003).min(1.0);
    let click = (-age / 0.01).exp();
    (phase * std::f32::consts::TAU).sin() * env + click * 0.18
}

fn partials(inst: u8) -> [(f32, f32); 4] {
    match inst {
        1 => [(1.0, 1.0), (0.0, 0.0), (0.0, 0.0), (0.0, 0.0)],
        2 => [(1.0, 0.75), (2.0, 0.22), (3.0, 0.08), (0.0, 0.0)],
        3 => [(1.0, 1.0), (4.0, 0.2), (9.0, 0.05), (0.0, 0.0)],
        4 => [(1.0, 0.55), (2.0, 0.2), (2.76, 0.32), (5.4, 0.12)],
        _ => [(1.0, 1.0), (2.0, 0.16), (4.0, 0.03), (0.0, 0.0)],
    }
}

fn tone(phase: f32, inst: u8) -> f32 {
    let mut s = 0.0;
    for (mult, w) in partials(inst) {
        if w == 0.0 {
            continue;
        }
        s += w * (phase * mult * std::f32::consts::TAU).sin();
    }
    s
}

fn noise(rng: &mut u32) -> f32 {
    *rng = rng.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
    (*rng >> 8) as f32 / 16_777_216.0 * 2.0 - 1.0
}

/// Resolving stays in dorian so the held tonic is home. A release in motion
/// opens into lydian. A hot room uses mixolydian. Otherwise the phrase walks
/// the five modes in pairs.
pub fn mode_for(phrase: i64, approach: f32, resolving: bool, heat: f32) -> usize {
    if resolving {
        return 0;
    }
    if approach > 0.65 {
        return 2;
    }
    if heat > 0.7 {
        return 1;
    }
    match phrase.rem_euclid(8) {
        0 | 1 => 0,
        2 | 3 => 1,
        4 => 4,
        5 | 6 => 3,
        _ => 2,
    }
}

pub fn pattern_at(mode: usize, beat: i64) -> i32 {
    PATTERNS[mode.min(4)][beat.rem_euclid(8) as usize]
}

#[allow(dead_code)]
fn mode_scale(mode: usize) -> &'static [i32; 7] {
    &MODES[mode.min(4)]
}

fn chord_degree(mode: usize, invert: bool, root: i32, beat: i64) -> i32 {
    let mut seq = PROGRESS[mode.min(4)];
    if invert {
        seq.swap(1, 2);
    }
    let degree = seq[(beat.rem_euclid(16) / 4) as usize];
    (degree + root).rem_euclid(7)
}

fn push_pluck(
    v: &mut Voice,
    freq: f32,
    wait: f32,
    decay: f32,
    amp: f32,
    inst: u8,
    stem: u8,
    noise_amt: f32,
) {
    let slot = v.plucks.iter().position(|p| !p.on).unwrap_or(0);
    v.plucks[slot] = Pluck {
        on: true,
        wait,
        age: 0.0,
        freq,
        amp,
        decay,
        phase: 0.0,
        inst,
        stem,
        noise: noise_amt,
    };
}

/// The release: a run up the scale into a held tonic, then a brush fill.
fn fire_release(v: &mut Voice, scale: &[i32; 7], inst: u8) {
    for i in 0..7 {
        push_pluck(
            v,
            hz_scale(scale, v.root + i),
            i as f32 * 0.11,
            0.14,
            0.12,
            inst,
            5,
            0.0,
        );
    }
    push_pluck(
        v,
        hz_scale(scale, v.root + 7),
        0.77,
        0.9,
        0.18,
        inst,
        5,
        0.0,
    );
    v.fill = 0.7;
    v.kick = 0.0;
}

fn string_damp(inst: u8) -> f32 {
    match inst {
        1 => 0.9992,
        2 => 0.99992,
        4 => 0.9995,
        _ => 0.9998,
    }
}

fn string_busy(v: &Voice, x: f32, y: f32) -> bool {
    v.strings
        .iter()
        .any(|s| s.on && s.age < 0.22 && (s.x - x).abs() < 0.008 && (s.y - y).abs() < 0.008)
}

fn start_string(v: &mut Voice, hz: f32, x: f32, y: f32, mag: f32, inst: u8) {
    let n = string_samples(hz);
    let slot = v.strings.iter().position(|s| !s.on).unwrap_or_else(|| {
        v.strings
            .iter()
            .enumerate()
            .max_by(|a, b| {
                a.1.age
                    .partial_cmp(&b.1.age)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|(i, _)| i)
            .unwrap_or(0)
    });
    let s = &mut v.strings[slot];
    s.on = true;
    s.age = 0.0;
    s.x = x;
    s.y = y;
    s.amp = 0.55 + mag.clamp(0.0, 1.0) * 0.35;
    s.damp = string_damp(inst);
    s.n = n;
    s.i = 0;
    for i in 0..n {
        let window = 1.0 - i as f32 / n as f32;
        s.buf[i] = noise(&mut v.rng) * window;
    }
}

fn tick_string(s: &mut StringVoice) -> f32 {
    let n = s.n;
    if n < 2 {
        return 0.0;
    }
    let i = s.i % n;
    let j = (i + 1) % n;
    let y = 0.5 * (s.buf[i] + s.buf[j]) * s.damp;
    s.buf[i] = y;
    s.i = j;
    y
}

fn fire(v: &mut Voice, e: &Ev, inst_land: u8, inst_ci: u8, scale: &[i32; 7], shift: i32) {
    if e.kind == 6 {
        for i in 0..3 {
            push_pluck(
                v,
                hz_scale(scale, v.root + 7),
                i as f32 * 0.07,
                0.045,
                0.12,
                inst_ci,
                5,
                0.0,
            );
        }
        return;
    }
    if e.kind == 7 {
        for i in 0..4 {
            push_pluck(
                v,
                hz_scale(scale, v.root + i),
                i as f32 * 0.09,
                0.16,
                0.1,
                inst_ci,
                5,
                0.0,
            );
        }
        return;
    }
    if string_busy(v, e.x, e.y) {
        return;
    }
    let hz = string_hz(scale, e.len as u32, shift);
    start_string(v, hz, e.x, e.y, e.mag, inst_land);
}

fn render(inner: &mut Inner, out: &mut [f32]) {
    let ctl = &inner.ctl;
    let v = &mut inner.voice;
    let frames = out.len() / 2;
    let nudge = ctl.nudge.load(Ordering::Relaxed);
    let hint = ctl.scale_hint.load(Ordering::Relaxed);
    let target = steer_target(bits(&ctl.bpm), nudge);
    if v.bpm < target {
        v.bpm = (v.bpm + 0.02).min(target);
    } else if v.bpm > target {
        v.bpm = (v.bpm - 0.02).max(target);
    }
    let on = ctl.on.load(Ordering::Relaxed);
    let speaking = ctl.speaking.load(Ordering::Relaxed);
    let err = bits(&ctl.err).clamp(0.0, 1.0);
    let master = bits(&ctl.master);
    let gate = ctl.gate.load(Ordering::Relaxed);
    let vols = [0, 1, 2, 3, 4, 5, 6, 7, 8].map(|i| gated(gate, i, bits(&ctl.vol[i])));
    let inst = [0, 1, 2, 3, 4, 5, 6, 7].map(|i| ctl.inst[i].load(Ordering::Relaxed));
    let brush_amt = bits(&ctl.brush).clamp(0.0, 1.0);
    let pace = bits(&ctl.pace);
    let cadence = bits(&ctl.cadence);
    let coherence = bits(&ctl.coherence);
    let ear_x = bits(&ctl.ear_x);
    let ear_y = bits(&ctl.ear_y);
    let ear_yaw = bits(&ctl.ear_yaw);
    let ear_pitch = bits(&ctl.ear_pitch);
    let body = ear_body(ear_pitch);
    let dt = 1.0 / SR;
    let frames_f = frames as f32;
    if frames_f > 0.0 {
        v.heat *= 0.5f32.powf(frames_f / SR / 60.0);
    }
    let period = (feed::REPLAY_SECS * SR) as u64;
    if !speaking && v.was_speaking {
        v.fill = 0.45;
    }
    v.was_speaking = speaking;

    for i in 0..frames {
        let mode = mode_for(v.phrase, v.approach, v.resolve > 0, v.heat);
        let (land_scale, _, _, land_shift, _) =
            sounding_scales(v.beat_i.max(0), v.resolve > 0, coherence, v.root, hint);
        let pos = v.sample % period;
        if pos < v.prev {
            v.ev_i = 0;
        }
        while v.ev_i < inner.events.len() && inner.events[v.ev_i].frame <= pos {
            let ev = inner.events[v.ev_i];
            fire(v, &ev, inst[4], inst[5], land_scale, land_shift);
            v.ev_i += 1;
        }
        v.prev = pos;

        let step = (v.bpm / 60.0) as f64 / SR as f64;
        let prev_beat = v.beat;
        v.beat += step;
        let beat_i = v.beat as i64;
        let new_beat = beat_i != v.beat_i;
        if new_beat {
            v.beat_i = beat_i;
            let slot = beat_i.rem_euclid(4);
            let step8 = beat_i.rem_euclid(8) as usize;
            let phrase_i = beat_i.div_euclid(16);
            let dens = room_density(cadence, v.approach, v.heat, pace);
            let pulses = ((dens * 8.0).round() as usize).clamp(2, 7);
            let rot = phrase_i.rem_euclid(8) as usize;
            if !speaking && euclid_hit((pulses / 2).max(1), 8, rot.wrapping_add(3), step8) {
                v.kick = 0.0;
            }
            if half_time(beat_i) {
                v.drum = 0.0;
            }
            if euclid_hit(pulses, 8, rot, step8) {
                v.brush = 0.0;
            }
            if slot == 3 && beat_i.rem_euclid(32) == 31 {
                v.fill = 0.4;
            }
            if v.approach > 0.5 && beat_i.rem_euclid(16) == 15 {
                v.fill = v.fill.max(0.35);
            }
            let step16 = beat_i.rem_euclid(16) as usize;
            let lead_n = ((dens * 16.0).round() as usize).clamp(5, 14);
            if euclid_hit(lead_n, 16, rot.wrapping_add(5), step16) {
                let base = pattern_at(mode, beat_i);
                let invert = phrase_i.rem_euclid(4) >= 2;
                let shift = phrase_i.rem_euclid(5) as i32;
                v.mel = (if invert { -base } else { base }) + shift;
            }
            if v.release_left > 0 {
                v.release_left -= 1;
                if v.release_left == 0 {
                    fire_release(v, &DORIAN, inst[5]);
                    v.resolve = 16;
                }
            } else if v.resolve > 0 {
                v.resolve -= 1;
            }
        }
        let phrase = (v.beat / 16.0) as i64;
        if phrase != v.phrase {
            v.phrase = phrase;
        }
        let (scale, drone_scale, station, sound_shift, held) =
            sounding_scales(beat_i.max(0), v.resolve > 0, coherence, v.root, hint);
        let phrase_i = beat_i.max(0).div_euclid(16);
        let chord_deg = if v.resolve > 0 {
            held
        } else {
            chord_degree(mode, phrase_i.rem_euclid(4) >= 2, 0, beat_i.max(0))
        };
        let shape = voicing(phrase_i);
        if new_beat && beat_i.rem_euclid(4) == 0 && ctl.graph.load(Ordering::Relaxed) {
            let (n_chord, chord) = voice_graph(&v.graph[..v.graph_n]);
            for i in 0..n_chord {
                let (x, y, degree) = chord[i];
                if string_busy(v, x, y) {
                    continue;
                }
                start_string(v, placed(scale, degree, sound_shift), x, y, 0.4, inst[4]);
            }
        }
        let step8 = beat_i.max(0).rem_euclid(8) as usize;
        let dens = room_density(cadence, v.approach, v.heat, pace);
        let pulses = ((dens * 8.0).round() as usize).clamp(2, 7);
        let rot = phrase_i.rem_euclid(8) as usize;
        let pulse_on = beat_i.rem_euclid(4) == 0 || euclid_hit(pulses, 8, rot, step8);
        let walk = if v.resolve > 0 {
            chord_deg
        } else if pulse_on {
            chord_deg + shape[step8 % 4]
        } else {
            chord_deg
        };
        let bass_hz = placed(scale, walk - 7, sound_shift);
        let fifth = placed(drone_scale, -3, station);
        let root_hz = placed(drone_scale, -7, station);
        let beat_frac = v.beat.fract();
        let prev_frac = prev_beat.fract();
        if prev_frac < 2.0 / 3.0 && beat_frac >= 2.0 / 3.0 {
            v.ghost = 0.0;
        }
        if v.fill > 0.0 {
            v.fill -= dt;
            if ((v.fill * 12.0) as i32) != (((v.fill + dt) * 12.0) as i32) {
                v.brush = 0.0;
            }
        }

        let t = v.sample as f32 / SR;
        let trem = 0.84 + 0.16 * (t * 5.0 * std::f32::consts::TAU).sin();
        let mut drone = tone(root_hz * t, inst[0]) * 0.55
            + tone(fifth * t, inst[0]) * (0.35 + 0.25 * v.approach);
        let mut agent = [0.0f32; 8];
        for r in 0..v.n_res {
            let (freq, who, stem) = v.residents[r];
            if freq <= 0.0 {
                continue;
            }
            agent[stem as usize] += tone(freq * t, who) * 0.18;
        }
        drone *= trem;

        let bass_env = {
            let age = beat_frac as f32;
            let env = (-age / 0.34).exp() * (age / 0.012).min(1.0);
            if pulse_on { env } else { 0.0 }
        };
        let bass = tone(bass_hz * t, inst[1]) * bass_env;

        let chord = {
            let weights = [1.0f32, 0.8, 0.32, 0.28];
            let mut acc = 0.0f32;
            for (i, w) in weights.iter().enumerate() {
                acc += tone(
                    placed(scale, chord_deg + shape[i], sound_shift) * t,
                    inst[2],
                ) * w;
            }
            let ninth = if v.resolve > 0 {
                0.0
            } else {
                tone(
                    placed(scale, chord_deg + shape[3] + 2, sound_shift) * t,
                    inst[2],
                ) * v.approach
                    * 0.25
            };
            (acc + ninth) * 0.45
        };

        let mel_env = {
            let age = beat_frac as f32;
            (-age / 0.2).exp() * (age / 0.01).min(1.0)
        };
        let spark_on =
            dens >= 0.65 && euclid_hit(3, 16, rot, beat_i.max(0).rem_euclid(16) as usize);
        let mel = (tone(placed(scale, v.mel, sound_shift) * t, inst[3])
            + if spark_on {
                tone(placed(scale, v.mel + 7, sound_shift) * t, inst[3]) * 0.35
            } else {
                0.0
            })
            * mel_env
            * (1.0 + 0.4 * v.approach);

        let mut cue = 0.0;
        for p in &mut v.plucks {
            if !p.on {
                continue;
            }
            if p.wait > 0.0 {
                p.wait -= dt;
                continue;
            }
            p.age += dt;
            p.phase = (p.phase + p.freq * dt).fract();
            let env = (-p.age / p.decay).exp() * (p.age / 0.008).min(1.0);
            if env < 0.001 {
                p.on = false;
                continue;
            }
            let mut s = tone(p.phase, p.inst);
            if p.noise != 0.0 {
                s += p.noise * noise(&mut v.rng);
            }
            let sample = s * env * p.amp;
            if p.stem >= 5 {
                cue += sample;
            }
        }
        let mut land_l = 0.0;
        let mut land_r = 0.0;
        for s in &mut v.strings {
            if !s.on {
                continue;
            }
            s.age += dt;
            if s.age > 0.85 {
                s.on = false;
                continue;
            }
            let y = tick_string(s);
            let (pan, gain) = ear(s.x, s.y, ear_x, ear_y, ear_yaw);
            let (l, r) = split_ear(y * s.amp * gain * body, pan);
            land_l += l;
            land_r += r;
        }

        let kick = {
            v.kick += dt;
            let env = (-v.kick / 0.08).exp();
            (60.0 * t * std::f32::consts::TAU).sin() * env
        };
        v.drum += dt;
        let drum_age = v.drum;
        let drum_freq = 38.0 + 62.0 * (-drum_age / 0.05).exp();
        v.drum_phase = (v.drum_phase + drum_freq * dt).fract();
        let boom_s = boom(drum_age, v.drum_phase);
        let brush_s = {
            v.brush += dt;
            v.ghost += dt;
            let n = noise(&mut v.rng);
            let hp = n - v.noise_z;
            v.noise_z = n;
            let accent = (-v.brush / (0.05 + 0.08 * (1.0 - brush_amt))).exp();
            let ghost = (-v.ghost / 0.03).exp() * 0.35;
            let voice_room = if speaking { 0.4 } else { 1.0 };
            hp * (accent + ghost) * (0.35 + brush_amt) * voice_room
        };
        let (wash_l, wash_r) = wash(v, err, dt);

        let mut s = 0.0;
        s += (drone + agent[0] * trem) * 0.09 * vols[0];
        for stem in 1..8 {
            s += agent[stem] * 0.09 * vols[stem];
        }
        s += bass * 0.16 * vols[1];
        s += chord * 0.07 * vols[2];
        s += mel * 0.11 * vols[3];
        s += cue * vols[5];
        s += brush_s * 0.14 * vols[6];
        s += kick * 0.2 * vols[6];
        s += boom_s * 0.55 * vols[8];
        s *= master;
        let sea = 0.22 * vols[7] * master;
        let mut left = s + land_l * 0.22 * vols[4] * master + wash_l * sea;
        let mut right = s + land_r * 0.22 * vols[4] * master + wash_r * sea;
        if !on {
            left = 0.0;
            right = 0.0;
        }
        left = (left / (1.0 + left.abs())).clamp(-1.0, 1.0);
        right = (right / (1.0 + right.abs())).clamp(-1.0, 1.0);
        out[i * 2] = left;
        out[i * 2 + 1] = right;
        v.sample = v.sample.wrapping_add(1);
    }
}

#[cfg(target_os = "macos")]
#[link(name = "AudioToolbox", kind = "framework")]
unsafe extern "C" {
    fn AudioQueueNewOutput(
        format: *const Asbd,
        callback: extern "C" fn(*mut c_void, *mut c_void, *mut c_void),
        user: *mut c_void,
        runloop: *mut c_void,
        mode: *mut c_void,
        flags: u32,
        out_aq: *mut *mut c_void,
    ) -> i32;
    fn AudioQueueAllocateBuffer(aq: *mut c_void, bytes: u32, out_buf: *mut *mut c_void) -> i32;
    fn AudioQueueEnqueueBuffer(
        aq: *mut c_void,
        buf: *mut c_void,
        packets: u32,
        desc: *const c_void,
    ) -> i32;
    fn AudioQueueStart(aq: *mut c_void, time: *const c_void) -> i32;
    fn AudioQueueStop(aq: *mut c_void, immediate: u8) -> i32;
    fn AudioQueueDispose(aq: *mut c_void, immediate: u8) -> i32;
}

#[cfg(not(target_os = "macos"))]
unsafe fn AudioQueueEnqueueBuffer(_: *mut c_void, _: *mut c_void, _: u32, _: *const c_void) -> i32 {
    0
}
#[cfg(not(target_os = "macos"))]
unsafe fn AudioQueueStop(_: *mut c_void, _: u8) -> i32 {
    0
}
#[cfg(not(target_os = "macos"))]
unsafe fn AudioQueueDispose(_: *mut c_void, _: u8) -> i32 {
    0
}

#[repr(C)]
struct Asbd {
    sample_rate: f64,
    format_id: u32,
    format_flags: u32,
    bytes_per_packet: u32,
    frames_per_packet: u32,
    bytes_per_frame: u32,
    channels: u32,
    bits: u32,
    reserved: u32,
}

#[cfg(target_os = "macos")]
fn start_queue(raw: *mut Inner) -> *mut c_void {
    unsafe {
        let fmt = Asbd {
            sample_rate: SR as f64,
            format_id: 1_819_304_813,
            format_flags: 9,
            bytes_per_packet: 8,
            frames_per_packet: 1,
            bytes_per_frame: 8,
            channels: 2,
            bits: 32,
            reserved: 0,
        };
        let mut aq: *mut c_void = std::ptr::null_mut();
        let st = AudioQueueNewOutput(
            &fmt,
            on_audio,
            raw.cast(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
            &mut aq,
        );
        if st != 0 || aq.is_null() {
            return std::ptr::null_mut();
        }
        for _ in 0..3 {
            let mut buf: *mut c_void = std::ptr::null_mut();
            if AudioQueueAllocateBuffer(aq, (FRAMES * 8) as u32, &mut buf) != 0 || buf.is_null() {
                continue;
            }
            on_audio(raw.cast(), aq, buf);
        }
        if AudioQueueStart(aq, std::ptr::null()) != 0 {
            AudioQueueDispose(aq, 1);
            return std::ptr::null_mut();
        }
        aq
    }
}

#[cfg(not(target_os = "macos"))]
fn start_queue(_: *mut Inner) -> *mut c_void {
    std::ptr::null_mut()
}

extern "C" fn on_audio(user: *mut c_void, aq: *mut c_void, buf: *mut c_void) {
    if user.is_null() || buf.is_null() {
        return;
    }
    unsafe {
        let cap = *buf.cast::<u32>() as usize;
        let data = *buf.byte_add(8).cast::<*mut f32>();
        if data.is_null() || cap < 8 {
            return;
        }
        let frames = (cap / 8).min(FRAMES * 2);
        let slice = std::slice::from_raw_parts_mut(data, frames * 2);
        render(&mut *user.cast::<Inner>(), slice);
        *buf.byte_add(16).cast::<u32>() = (frames * 8) as u32;
        if !aq.is_null() {
            AudioQueueEnqueueBuffer(aq, buf, 0, std::ptr::null());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn develop_inverts_and_transposes_inside_the_scale() {
        assert_eq!(develop(&[0, 6, 3, 0], false, 0), vec![0, 6, 3, 0]);
        assert_eq!(develop(&[0, 6, 3, 0], true, 0), vec![0, 3, 6, 0]);
        assert_eq!(develop(&[0, 6, 3, 0], false, 1), vec![1, 0, 4, 1]);
    }

    #[test]
    fn the_home_tone_is_d() {
        let d = hz(0);
        assert!((d - 146.83).abs() < 0.05, "{d}");
        assert!(hz(-7) < 80.0);
    }

    #[test]
    fn a_silent_buffer_stays_finite() {
        let ctl = Arc::new(Ctl {
            on: AtomicBool::new(true),
            speaking: Arc::new(AtomicBool::new(false)),
            failing: AtomicBool::new(true),
            err: AtomicU32::new(0f32.to_bits()),
            master: AtomicU32::new(0.8f32.to_bits()),
            vol: [0.5f32; 9].map(|v| AtomicU32::new(v.to_bits())),
            gate: AtomicU16::new(0x01ff),
            inst: [0u8; 8].map(AtomicU8::new),
            bpm: AtomicU32::new(96f32.to_bits()),
            brush: AtomicU32::new(0.4f32.to_bits()),
            pace: AtomicU32::new(0.8f32.to_bits()),
            cadence: AtomicU32::new(0.7f32.to_bits()),
            coherence: AtomicU32::new(0.3f32.to_bits()),
            ear_x: AtomicU32::new(0.5f32.to_bits()),
            ear_y: AtomicU32::new(0.5f32.to_bits()),
            ear_yaw: AtomicU32::new(0f32.to_bits()),
            ear_pitch: AtomicU32::new(0.58f32.to_bits()),
            graph: AtomicBool::new(false),
            nudge: AtomicI8::new(0),
            scale_hint: AtomicU8::new(0),
        });
        let mut inner = Inner {
            ctl,
            voice: Voice::new(96.0),
            events: vec![Ev {
                frame: 0,
                kind: 7,
                mag: 0.5,
                x: 0.5,
                y: 0.5,
                len: 8,
            }],
        };
        let mut out = vec![0f32; 256];
        render(&mut inner, &mut out);
        assert!(out.iter().all(|s| s.is_finite()));
        assert!(out.iter().any(|s| s.abs() > 0.0));
    }

    #[test]
    fn a_due_release_holds_the_tonic_and_stays_finite() {
        let ctl = Arc::new(Ctl {
            on: AtomicBool::new(true),
            speaking: Arc::new(AtomicBool::new(false)),
            failing: AtomicBool::new(false),
            err: AtomicU32::new(0f32.to_bits()),
            master: AtomicU32::new(0.8f32.to_bits()),
            vol: [0.5f32; 9].map(|v| AtomicU32::new(v.to_bits())),
            gate: AtomicU16::new(0x01ff),
            inst: [0u8; 8].map(AtomicU8::new),
            bpm: AtomicU32::new(96f32.to_bits()),
            brush: AtomicU32::new(0.4f32.to_bits()),
            pace: AtomicU32::new(0.4f32.to_bits()),
            cadence: AtomicU32::new(0.4f32.to_bits()),
            coherence: AtomicU32::new(0.8f32.to_bits()),
            ear_x: AtomicU32::new(0.5f32.to_bits()),
            ear_y: AtomicU32::new(0.5f32.to_bits()),
            ear_yaw: AtomicU32::new(0f32.to_bits()),
            ear_pitch: AtomicU32::new(0.58f32.to_bits()),
            graph: AtomicBool::new(false),
            nudge: AtomicI8::new(0),
            scale_hint: AtomicU8::new(0),
        });
        let mut voice = Voice::new(96.0);
        voice.beat = 0.99;
        voice.beat_i = 0;
        voice.approach = 1.0;
        voice.release_left = 1;
        let mut inner = Inner {
            ctl,
            voice,
            events: Vec::new(),
        };
        let mut out = vec![0f32; 1024];
        render(&mut inner, &mut out);
        assert_eq!(inner.voice.release_left, 0);
        assert_eq!(inner.voice.resolve, 16);
        assert!(inner.voice.plucks.iter().any(|p| p.on && p.decay > 0.5));
        assert!(out.iter().all(|s| s.is_finite()));
    }

    #[test]
    fn the_line_changes_mode_and_comes_home() {
        assert_eq!(mode_for(0, 0.0, false, 0.0), 0);
        assert_eq!(mode_for(2, 0.0, false, 0.0), 1);
        assert_eq!(mode_for(4, 0.0, false, 0.0), 4);
        assert_eq!(mode_for(5, 0.0, false, 0.0), 3);
        assert_eq!(mode_for(7, 0.0, false, 0.0), 2);
        assert_eq!(mode_for(7, 0.0, true, 0.9), 0);
        assert_eq!(mode_for(4, 0.8, false, 0.0), 2);
        assert_eq!(mode_for(4, 0.0, false, 0.9), 1);
        assert_eq!(pattern_at(0, 0), 0);
        assert_eq!(pattern_at(0, 4), 4);
        assert_eq!(pattern_at(0, 7), 0);
        for mode in 0..5 {
            for beat in 0..8 {
                let degree = pattern_at(mode, beat);
                assert!((-1..=7).contains(&degree), "{mode} {beat} {degree}");
            }
        }
        assert_ne!(PROGRESS[1], PROGRESS[0]);
        assert!((hz(0) - 146.83).abs() < 0.05);
    }

    #[test]
    fn coltrane_counts_down_by_major_thirds_into_d() {
        assert_eq!(station_shift(0), 8);
        assert_eq!(station_shift(1), 4);
        assert_eq!(station_shift(2), 0);
        assert_eq!(station_shift(3), 8);
        assert_eq!(station_shift(4), 4);
        assert_eq!(station_shift(5), 0);
        assert_eq!((8 + 4) % 12, 0);
        let (_, _, station, sound, chord) = coltrane_key(0, false, 1.0, 0);
        assert_eq!(station, 8);
        assert_eq!(sound, 15);
        assert_eq!(chord, 0);
        let (_, _, _, tonic, _) = coltrane_key(2, false, 1.0, 0);
        assert_eq!(tonic, 8);
        let home = placed(&IONIAN, 0, 0);
        assert!((home - hz(0)).abs() < 0.05, "{home}");
        let up = placed(&IONIAN, 0, 4);
        assert!((up / home - 2f32.powf(4.0 / 12.0)).abs() < 1.0e-3);
        let (_, _, held_station, held_sound, held_chord) = coltrane_key(4, true, 1.0, 0);
        assert_eq!(held_station, 0);
        assert_eq!(held_sound, 0);
        assert_eq!(held_chord, 0);
    }

    #[test]
    fn the_half_time_drum_hits_on_the_eight_and_can_be_gated() {
        assert!(half_time(0));
        assert!(half_time(8));
        assert!(!half_time(4));
        assert!(!half_time(1));
        assert_eq!(stem_gain(false, 0.6), 0.0);
        assert_eq!(stem_gain(true, 0.6), 0.6);
        assert!(drum_env(0.01) > drum_env(0.48));
        assert!(drum_at(0.01).is_finite());
        assert!(drum_at(0.2).is_finite());
        assert!(drum_at(0.45).is_finite());
    }

    fn drum_env(age: f32) -> f32 {
        (-age / 0.42).exp() * (age / 0.003).min(1.0) + (-age / 0.01).exp() * 0.18
    }

    fn drum_at(age: f32) -> f32 {
        let freq = 38.0 + 62.0 * (-age / 0.05).exp();
        boom(age, freq * age)
    }

    #[test]
    fn a_longer_string_sits_lower_and_the_ear_follows_the_orbit() {
        let short = string_hz(&IONIAN, sounding_len(6, 1), 0);
        let long = string_hz(&IONIAN, sounding_len(6, 30), 0);
        assert!(short > long, "{short} {long}");
        assert!(string_samples(long) > string_samples(short));
        let stationed = string_hz(&IONIAN, sounding_len(6, 1), 8);
        assert!((stationed / short - 2f32.powf(8.0 / 12.0)).abs() < 1.0e-3);
        let (pan_r, near) = ear(0.85, 0.5, 0.5, 0.5, 0.0);
        let (pan_l, _) = ear(0.85, 0.5, 0.5, 0.5, std::f32::consts::PI);
        assert!(pan_r > 0.5, "{pan_r}");
        assert!(pan_l < -0.5, "{pan_l}");
        let (_, far) = ear(2.2, 0.5, 0.5, 0.5, 0.0);
        assert!(near > far);
        assert!(ear_body(1.5) > ear_body(-1.2));
        let (l, r) = split_ear(1.0, pan_r);
        assert!(r > l);
    }

    #[test]
    fn a_landing_plays_from_its_place_and_does_not_restrike() {
        let ctl = Arc::new(Ctl {
            on: AtomicBool::new(true),
            speaking: Arc::new(AtomicBool::new(false)),
            failing: AtomicBool::new(false),
            err: AtomicU32::new(0f32.to_bits()),
            master: AtomicU32::new(1.0f32.to_bits()),
            vol: {
                let mut v = [0.0f32; 9];
                v[4] = 1.0;
                v.map(|x| AtomicU32::new(x.to_bits()))
            },
            gate: AtomicU16::new(1 << 4),
            inst: [0u8; 8].map(AtomicU8::new),
            bpm: AtomicU32::new(96f32.to_bits()),
            brush: AtomicU32::new(0.0f32.to_bits()),
            pace: AtomicU32::new(0.2f32.to_bits()),
            cadence: AtomicU32::new(0.2f32.to_bits()),
            coherence: AtomicU32::new(0.8f32.to_bits()),
            ear_x: AtomicU32::new(0.5f32.to_bits()),
            ear_y: AtomicU32::new(0.5f32.to_bits()),
            ear_yaw: AtomicU32::new(0f32.to_bits()),
            ear_pitch: AtomicU32::new(0.9f32.to_bits()),
            graph: AtomicBool::new(false),
            nudge: AtomicI8::new(0),
            scale_hint: AtomicU8::new(0),
        });
        let ev = Ev {
            frame: 0,
            kind: 1,
            mag: 0.6,
            x: 0.85,
            y: 0.5,
            len: sounding_len(12, 2) as u16,
        };
        let mut inner = Inner {
            ctl,
            voice: Voice::new(96.0),
            events: vec![ev, ev],
        };
        let mut out = vec![0f32; 2048 * 2];
        render(&mut inner, &mut out);
        assert_eq!(inner.voice.strings.iter().filter(|s| s.on).count(), 1);
        let mut left_e = 0.0f32;
        let mut right_e = 0.0f32;
        for i in 0..2048 {
            left_e += out[i * 2].abs();
            right_e += out[i * 2 + 1].abs();
            assert!(out[i * 2].is_finite() && out[i * 2 + 1].is_finite());
        }
        assert!(right_e > 0.05, "{right_e}");
        assert!(right_e > left_e * 4.0, "right {right_e} left {left_e}");
    }

    #[test]
    fn graph_strings_stack_a_seventh_on_the_current_scale() {
        let (n, chord) = voice_graph(&[
            (0.15, 0.20, 0.25),
            (0.40, 0.55, 0.55),
            (0.70, 0.30, 0.85),
            (0.25, 0.80, 1.15),
        ]);
        assert_eq!(n, 4);
        assert_eq!(
            [chord[0].2, chord[1].2, chord[2].2, chord[3].2],
            [0, 2, 4, 6]
        );
        assert!((chord[3].0 - 0.25).abs() < 1e-4);
        let root = placed(&IONIAN, 0, 0);
        let third = placed(&IONIAN, 2, 0);
        let fifth = placed(&IONIAN, 4, 0);
        let seventh = placed(&IONIAN, 6, 0);
        assert!((third / root - 2f32.powf(4.0 / 12.0)).abs() < 1.0e-3);
        assert!((fifth / root - 2f32.powf(7.0 / 12.0)).abs() < 1.0e-3);
        assert!((seventh / root - 2f32.powf(11.0 / 12.0)).abs() < 1.0e-3);
        let flat_seven = placed(&MIXOLYDIAN, 6, 0);
        assert!((flat_seven / root - 2f32.powf(10.0 / 12.0)).abs() < 1.0e-3);
        let (two, pair) = voice_graph(&[(0.1, 0.1, 0.2), (0.6, 0.4, 0.9)]);
        assert_eq!(two, 2);
        assert_eq!([pair[0].2, pair[1].2], [0, 2]);

        let ctl = Arc::new(Ctl {
            on: AtomicBool::new(true),
            speaking: Arc::new(AtomicBool::new(false)),
            failing: AtomicBool::new(false),
            err: AtomicU32::new(0f32.to_bits()),
            master: AtomicU32::new(1.0f32.to_bits()),
            vol: {
                let mut v = [0.0f32; 9];
                v[4] = 1.0;
                v.map(|x| AtomicU32::new(x.to_bits()))
            },
            gate: AtomicU16::new(1 << 4),
            inst: [0u8; 8].map(AtomicU8::new),
            bpm: AtomicU32::new(96f32.to_bits()),
            brush: AtomicU32::new(0.0f32.to_bits()),
            pace: AtomicU32::new(0.2f32.to_bits()),
            cadence: AtomicU32::new(0.2f32.to_bits()),
            coherence: AtomicU32::new(1.0f32.to_bits()),
            ear_x: AtomicU32::new(0.5f32.to_bits()),
            ear_y: AtomicU32::new(0.5f32.to_bits()),
            ear_yaw: AtomicU32::new(0f32.to_bits()),
            ear_pitch: AtomicU32::new(0.9f32.to_bits()),
            graph: AtomicBool::new(true),
            nudge: AtomicI8::new(0),
            scale_hint: AtomicU8::new(0),
        });
        let mut voice = Voice::new(96.0);
        voice.beat = 3.999;
        voice.beat_i = 3;
        voice.graph_n = 4;
        voice.graph[0] = (0.15, 0.20, 0.25);
        voice.graph[1] = (0.40, 0.55, 0.55);
        voice.graph[2] = (0.70, 0.30, 0.85);
        voice.graph[3] = (0.25, 0.80, 1.15);
        let mut inner = Inner {
            ctl,
            voice,
            events: Vec::new(),
        };
        let mut out = vec![0f32; 1024 * 2];
        render(&mut inner, &mut out);
        let expect = [0, 2, 4, 6].map(|d| string_samples(placed(&MIXOLYDIAN, d, 11)));
        let mut got: Vec<usize> = inner
            .voice
            .strings
            .iter()
            .filter(|s| s.on)
            .map(|s| s.n)
            .collect();
        got.sort();
        let mut want = expect.to_vec();
        want.sort();
        assert_eq!(got, want, "periods {got:?} want {want:?}");
        assert!(out.iter().all(|s| s.is_finite()));
    }

    #[test]
    fn a_reading_can_name_the_scale_without_moving_the_station() {
        let (scale, _, station, sound, _) = sounding_scales(4, false, 1.0, 0, 5);
        assert_eq!(station, 4);
        assert_eq!(sound, 11);
        assert_eq!(scale, &IONIAN);
        let (home, _, _, _, _) = sounding_scales(4, true, 1.0, 0, 5);
        assert_eq!(home, &DORIAN);
        let (first, _, station4, sound4, _) = sounding_scales(4, false, 1.0, 0, 0);
        assert_eq!(first, &MIXOLYDIAN);
        assert_eq!(station4, 4);
        assert_eq!(sound4, 11);
        let (next, _, _, _, _) = sounding_scales(16, false, 1.0, 0, 0);
        assert_eq!(next, &DORIAN);
        let (later, _, _, _, _) = sounding_scales(32, false, 1.0, 0, 0);
        assert_eq!(later, &MODES[2]);
        let (pent, _, _, _, _) = sounding_scales(16 * 7, false, 1.0, 0, 0);
        assert_eq!(pent, &MINOR_PENTA);
        let (whole, _, _, _, _) = sounding_scales(16 * 15, false, 1.0, 0, 0);
        assert_eq!(whole, &WHOLE_TONE);
        assert_ne!(voicing(0), voicing(1));
        let hits = (0..8).filter(|s| euclid_hit(3, 8, 0, *s)).count();
        assert_eq!(hits, 3);
        let turned = (0..8).filter(|s| euclid_hit(3, 8, 1, *s)).count();
        assert_eq!(turned, 3);
        assert_ne!(
            (0..8).map(|s| euclid_hit(3, 8, 0, s)).collect::<Vec<_>>(),
            (0..8).map(|s| euclid_hit(3, 8, 1, s)).collect::<Vec<_>>()
        );
        assert!(!(0..8).any(|s| euclid_hit(0, 8, 0, s)));
        assert!((steer_target(100.0, 4) - 104.0).abs() < 0.01);
        assert!((steer_target(138.0, 8) - 140.0).abs() < 0.01);
        assert!((steer_target(62.0, -8) - 60.0).abs() < 0.01);
    }

    fn sea(rate: f32) -> Vec<f32> {
        let ctl = Arc::new(Ctl {
            on: AtomicBool::new(true),
            speaking: Arc::new(AtomicBool::new(false)),
            failing: AtomicBool::new(rate > 0.0),
            err: AtomicU32::new(rate.to_bits()),
            master: AtomicU32::new(1.0f32.to_bits()),
            vol: {
                let mut v = [0.0f32; 9];
                v[7] = 1.0;
                v.map(|x| AtomicU32::new(x.to_bits()))
            },
            gate: AtomicU16::new(1 << 7),
            inst: [0u8; 8].map(AtomicU8::new),
            bpm: AtomicU32::new(96f32.to_bits()),
            brush: AtomicU32::new(0.0f32.to_bits()),
            pace: AtomicU32::new(0.2f32.to_bits()),
            cadence: AtomicU32::new(0.2f32.to_bits()),
            coherence: AtomicU32::new(1.0f32.to_bits()),
            ear_x: AtomicU32::new(0.5f32.to_bits()),
            ear_y: AtomicU32::new(0.5f32.to_bits()),
            ear_yaw: AtomicU32::new(0f32.to_bits()),
            ear_pitch: AtomicU32::new(0.58f32.to_bits()),
            graph: AtomicBool::new(false),
            nudge: AtomicI8::new(0),
            scale_hint: AtomicU8::new(0),
        });
        let mut inner = Inner {
            ctl,
            voice: Voice::new(96.0),
            events: Vec::new(),
        };
        let mut out = vec![0f32; SR as usize * 2];
        render(&mut inner, &mut out);
        out
    }

    fn energy(buf: &[f32]) -> f32 {
        buf.iter().map(|s| s * s).sum()
    }

    #[test]
    fn failure_washes_harder_as_the_error_rate_rises() {
        assert_eq!(error_rate(0, 40), 0.0);
        assert_eq!(error_rate(1, 0), 0.0);
        assert!((error_rate(1, 100) - 1.0 / 24.0).abs() < 1e-6);
        assert!((error_rate(12, 24) - 0.5).abs() < 1e-6);
        assert_eq!(error_rate(30, 24), 1.0);
        let quiet = sea(0.0);
        let light = sea(0.12);
        let heavy = sea(1.0);
        assert!(quiet.iter().all(|s| s.is_finite()));
        assert!(light.iter().all(|s| s.is_finite()));
        assert!(heavy.iter().all(|s| s.is_finite()));
        assert!(energy(&quiet) < 1.0e-8);
        let crest = |buf: &[f32]| buf.chunks(2048).map(energy).fold(0.0f32, |a, e| a.max(e));
        let light_c = crest(&light);
        let heavy_c = crest(&heavy);
        assert!(heavy_c > light_c, "heavy {heavy_c} light {light_c}");
    }
}
