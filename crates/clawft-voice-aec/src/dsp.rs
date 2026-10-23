//! AEC DSP core — resampling, the echo-canceller abstraction, and the
//! in-process [`AecProcessor`] lib API.
//!
//! This module is **always compiled** (it has no `cpal` dependency), so the
//! library API is available in the default workspace build. The real AEC3
//! echo canceller is gated behind `webrtc-aec`; without it the processor is
//! a transparent passthrough (still useful for wiring / round-trip tests).
//!
//! The stdio [`bridge`](crate::bridge) (cpal device I/O) and any in-process
//! weftos consumer (e.g. `clawft-channels::voice`) share this same core so
//! the echo-cancel behaviour is identical whichever way audio is fed in.

use std::collections::VecDeque;

use crate::consts::TARGET_SR;

/// The WebRTC APM is hardwired to 48 kHz, 10 ms frames (480 samples). We run
/// the whole echo-cancel pipeline at this rate and resample the cleaned
/// capture back down to the 16 kHz wire format on the way out.
pub(crate) const APM_SR: u32 = 48_000;
pub(crate) const APM_FRAME: usize = 480;

/// Streaming linear resampler. Maintains fractional read position and one
/// sample of history across calls so chunk boundaries don't click.
pub(crate) struct LinResampler {
    step: f64, // input samples consumed per output sample = in_rate / out_rate
    t: f64,    // absolute fractional input position (in input-sample units)
    prev: f32, // input sample at absolute index (consumed - 1)
    consumed: u64,
}

impl LinResampler {
    pub(crate) fn new(in_rate: u32, out_rate: u32) -> Self {
        Self {
            step: f64::from(in_rate) / f64::from(out_rate.max(1)),
            t: 0.0,
            prev: 0.0,
            consumed: 0,
        }
    }

    fn sample_at(&self, idx: i64, input: &[f32], base: u64) -> f32 {
        if idx < base as i64 {
            self.prev // idx == base - 1
        } else {
            input[(idx as u64 - base) as usize]
        }
    }

    /// Resample `input` (absolute indices `[consumed, consumed+len)`),
    /// appending output samples to `out`.
    pub(crate) fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if input.is_empty() {
            return;
        }
        let base = self.consumed;
        let end = base + input.len() as u64; // one past last available index
        loop {
            let i0 = self.t.floor() as i64;
            let i1 = i0 + 1;
            if (i1 as u64) >= end {
                break; // need i1 to be available for interpolation
            }
            let frac = (self.t - i0 as f64) as f32;
            let s0 = self.sample_at(i0, input, base);
            let s1 = self.sample_at(i1, input, base);
            out.push(s0 + (s1 - s0) * frac);
            self.t += self.step;
        }
        self.prev = *input.last().unwrap();
        self.consumed = end;
    }
}

/// 10 ms-frame echo canceller abstraction. Both impls operate on 48 kHz mono
/// `APM_FRAME`-sample (480) frames, in place — the WebRTC APM's fixed native
/// format.
pub(crate) trait Aec: Send {
    /// Feed a far-end (played) reference frame.
    fn render(&mut self, frame: &[f32]);
    /// Echo-cancel a near-end (mic) frame in place.
    fn capture(&mut self, frame: &mut [f32]);
    fn label(&self) -> &'static str;
}

/// Passthrough — no echo cancellation (used when `webrtc-aec` is off).
pub(crate) struct Passthrough;
impl Aec for Passthrough {
    fn render(&mut self, _frame: &[f32]) {}
    fn capture(&mut self, _frame: &mut [f32]) {}
    fn label(&self) -> &'static str {
        "passthrough (NO AEC — build with --features webrtc-aec)"
    }
}

#[cfg(feature = "webrtc-aec")]
mod webrtc {
    use super::Aec;
    use webrtc_audio_processing::config::{
        EchoCanceller, GainController, GainController1, GainControllerMode, HighPassFilter,
        NoiseSuppression, NoiseSuppressionLevel,
    };
    use webrtc_audio_processing::{Config as ApmConfig, Processor};

    /// The APM's native rate: this module works on 48 kHz mono 10 ms frames.
    const APM_SAMPLE_RATE_HZ: u32 = 48_000;

    pub struct WebrtcAec {
        proc: Processor,
    }

    impl WebrtcAec {
        pub fn new() -> Result<Self, String> {
            let proc = Processor::new(APM_SAMPLE_RATE_HZ).map_err(|e| format!("APM init: {e}"))?;

            let config = ApmConfig {
                echo_canceller: Some(EchoCanceller::Full {
                    stream_delay_ms: None,
                }),
                high_pass_filter: Some(HighPassFilter::default()),
                noise_suppression: Some(NoiseSuppression {
                    level: NoiseSuppressionLevel::Moderate,
                    analyze_linear_aec_output: false,
                }),
                gain_controller: Some(GainController::GainController1(GainController1 {
                    mode: GainControllerMode::AdaptiveDigital,
                    target_level_dbfs: 3,
                    compression_gain_db: 9,
                    enable_limiter: true,
                    analog_gain_controller: None,
                })),
                ..ApmConfig::default()
            };
            proc.set_config(config);
            Ok(Self { proc })
        }
    }

    impl Aec for WebrtcAec {
        fn render(&mut self, frame: &[f32]) {
            // APM mutates the render frame in place; clone our reference.
            let mut buf = frame.to_vec();
            let _ = self.proc.process_render_frame([&mut buf[..]]);
        }
        fn capture(&mut self, frame: &mut [f32]) {
            let _ = self.proc.process_capture_frame([frame]);
        }
        fn label(&self) -> &'static str {
            "WebRTC AEC3 (echo cancel + NS + AGC)"
        }
    }
}

/// Build the active echo canceller: real AEC3 under `webrtc-aec`, else
/// passthrough. Falls back to passthrough if APM init fails.
pub(crate) fn make_aec() -> Box<dyn Aec> {
    #[cfg(feature = "webrtc-aec")]
    {
        match webrtc::WebrtcAec::new() {
            Ok(a) => return Box::new(a),
            Err(e) => {
                let _ = std::io::Write::write_all(
                    &mut std::io::stderr(),
                    format!("[aec] WARNING: WebRTC APM init failed ({e}); passthrough.\n")
                        .as_bytes(),
                );
            }
        }
    }
    Box::new(Passthrough)
}

/// In-process, device-agnostic echo canceller.
///
/// Operates entirely on the 16 kHz mono `int16` wire format weftos uses, so
/// a consumer (e.g. `clawft-channels::voice`) owns its own capture/playback
/// and routes samples through here:
///
/// 1. Whatever PCM is about to be **played** (the bot's TTS) is pushed via
///    [`push_render`](Self::push_render) — it becomes the AEC *reference*.
/// 2. The raw **mic** PCM is fed through [`process_capture`](Self::process_capture),
///    which returns the echo-cancelled mic (the bot's own voice removed),
///    ready for VAD / STT.
/// 3. On **barge-in** (user interrupts), call [`flush`](Self::flush) the
///    moment playback is silenced so stale reference frames stop cancelling
///    the user's onset.
///
/// Internally it lifts both streams to the APM's native 48 kHz / 480-sample
/// frames, runs the echo canceller frame-aligned, and resamples the cleaned
/// capture back to 16 kHz. Because output is produced one APM frame at a
/// time, a single `process_capture` call may return zero, one, or several
/// 16 kHz chunks depending on how the input lines up with the 480-sample
/// frame boundary — callers should treat the returned `Vec` as a stream.
pub struct AecProcessor {
    aec: Box<dyn Aec>,
    /// Whether the active canceller does real work (false = passthrough).
    active: bool,
    up_ref: LinResampler,    // 16k -> 48k (render reference)
    up_cap: LinResampler,    // 16k -> 48k (capture)
    down_wire: LinResampler, // 48k cleaned -> 16k wire
    render48k: VecDeque<f32>,
    pending48k: Vec<f32>,
}

impl Default for AecProcessor {
    fn default() -> Self {
        Self::new()
    }
}

impl AecProcessor {
    /// Build a processor for the 16 kHz mono wire format (real AEC3 when the
    /// `webrtc-aec` feature is on).
    pub fn new() -> Self {
        Self::with_aec(make_aec(), cfg!(feature = "webrtc-aec"))
    }

    /// Build a processor that does **no** echo cancellation, noise suppression,
    /// or AGC — only the resample round-trip (level-preserving).
    ///
    /// Use for listen-only capture. There is no playback to cancel, so the
    /// render reference is always empty; the WebRTC APM would then run pure
    /// NS + AGC on the mic, and at low SNR its noise suppressor cannot tell
    /// marginal speech from room tone and attenuates it below the VAD gate.
    /// That deafened `weft voice listen` (WebRTC path) while raw `test-mic`
    /// (no APM) heard the same voice fine. Talk mode keeps the real AEC — it
    /// has genuine playback echo to remove.
    pub fn passthrough() -> Self {
        Self::with_aec(Box::new(Passthrough), false)
    }

    fn with_aec(aec: Box<dyn Aec>, active: bool) -> Self {
        Self {
            aec,
            active,
            up_ref: LinResampler::new(TARGET_SR, APM_SR),
            up_cap: LinResampler::new(TARGET_SR, APM_SR),
            down_wire: LinResampler::new(APM_SR, TARGET_SR),
            render48k: VecDeque::new(),
            pending48k: Vec::new(),
        }
    }

    /// Human-readable description of the active echo canceller.
    pub fn label(&self) -> &'static str {
        self.aec.label()
    }

    /// Whether real echo cancellation is active (false = passthrough / listen-
    /// only bypass).
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Push far-end (playback) PCM — 16 kHz mono `int16`. These samples are
    /// used as the AEC render reference; call this with exactly the audio
    /// being sent to the speaker.
    pub fn push_render(&mut self, samples: &[i16]) {
        if samples.is_empty() {
            return;
        }
        let f: Vec<f32> = samples.iter().map(|&s| f32::from(s) / 32_768.0).collect();
        let mut out = Vec::new();
        self.up_ref.process(&f, &mut out);
        self.render48k.extend(out);
    }

    /// Echo-cancel near-end (mic) PCM — 16 kHz mono `int16`. Returns the
    /// cleaned mic as 16 kHz mono `int16` (possibly empty until a full APM
    /// frame accumulates).
    pub fn process_capture(&mut self, mic: &[i16]) -> Vec<i16> {
        let mut out: Vec<i16> = Vec::new();
        if !mic.is_empty() {
            let f: Vec<f32> = mic.iter().map(|&s| f32::from(s) / 32_768.0).collect();
            let mut up = Vec::new();
            self.up_cap.process(&f, &mut up);
            self.pending48k.extend(up);
        }

        let mut render_frame = [0.0f32; APM_FRAME];
        while self.pending48k.len() >= APM_FRAME {
            let mut cap_frame: Vec<f32> = self.pending48k.drain(..APM_FRAME).collect();

            // Pull the matching 48k render reference (zeros if none queued).
            for slot in render_frame.iter_mut() {
                *slot = 0.0;
            }
            let take = self.render48k.len().min(APM_FRAME);
            for slot in render_frame.iter_mut().take(take) {
                *slot = self.render48k.pop_front().unwrap();
            }

            self.aec.render(&render_frame);
            self.aec.capture(&mut cap_frame);

            let mut wire = Vec::new();
            self.down_wire.process(&cap_frame, &mut wire);
            for &s in &wire {
                out.push((s.clamp(-1.0, 1.0) * 32_767.0) as i16);
            }
        }
        out
    }

    /// Number of queued render-reference samples (at the APM's 48 kHz rate)
    /// awaiting cancellation. Used by device-path consumers to confirm the
    /// playback sink is feeding the echo reference and that [`flush`](Self::flush)
    /// cleared it.
    pub fn render_ref_len(&self) -> usize {
        self.render48k.len()
    }

    /// Barge-in flush: drop all queued render reference. Call this the moment
    /// playback is silenced so the canceller stops subtracting audio that is
    /// no longer reaching the user's ears.
    pub fn flush(&mut self) {
        self.render48k.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resampler_roundtrip_length() {
        // 16k -> 48k should roughly triple the sample count.
        let mut up = LinResampler::new(16_000, 48_000);
        let input: Vec<f32> = (0..1_600).map(|i| (i as f32 * 0.01).sin()).collect();
        let mut out = Vec::new();
        up.process(&input, &mut out);
        // ~3x, allow slack for the fractional warm-up.
        assert!(out.len() > 4_700 && out.len() < 4_900, "got {}", out.len());
    }

    #[test]
    fn processor_passthrough_preserves_signal_shape() {
        // Without webrtc-aec, capture should pass through (after resample
        // round-trip) — energy preserved, not zeroed.
        let mut p = AecProcessor::new();
        let mic: Vec<i16> = (0..4_800)
            .map(|i| ((i as f32 * 0.05).sin() * 8_000.0) as i16)
            .collect();
        let cleaned = p.process_capture(&mic);
        assert!(!cleaned.is_empty(), "expected cleaned output frames");
        let energy: i64 = cleaned.iter().map(|&s| i64::from(s) * i64::from(s)).sum();
        assert!(energy > 0, "passthrough must preserve mic energy");
    }

    #[test]
    fn flush_drops_render_reference() {
        let mut p = AecProcessor::new();
        p.push_render(&vec![5_000i16; 1_600]);
        assert!(!p.render48k.is_empty());
        p.flush();
        assert!(p.render48k.is_empty());
    }

    #[test]
    fn frame_boundary_streaming() {
        // Feeding sub-frame chunks should still eventually produce output and
        // not panic on the 480-sample framing.
        let mut p = AecProcessor::new();
        let mut total = 0usize;
        for _ in 0..20 {
            let chunk = vec![1_000i16; 80]; // 80 samples @16k = 5ms
            total += p.process_capture(&chunk).len();
        }
        assert!(total > 0, "streaming sub-frame input produced no output");
    }

    #[test]
    fn is_active_matches_feature() {
        let p = AecProcessor::new();
        assert_eq!(p.is_active(), cfg!(feature = "webrtc-aec"));
    }

    /// With the real canceller the pure-echo mic signal (render reference
    /// played straight back, no near-end speech) must be attenuated once AEC3
    /// has converged. Guards the webrtc-audio-processing 2.x port.
    #[cfg(feature = "webrtc-aec")]
    #[test]
    fn real_aec_attenuates_pure_echo() {
        let mut p = AecProcessor::new();
        assert!(p.is_active());
        let chunk = 160; // 10 ms @ 16 kHz
        let mut n = 0usize;
        let (mut in_e, mut out_e) = (0f64, 0f64);
        for block in 0..300 {
            // Broadband-ish reference: two incommensurate tones plus a slow AM.
            let sig: Vec<i16> = (0..chunk)
                .map(|_| {
                    let t = n as f32;
                    n += 1;
                    let am = 0.6 + 0.4 * (t * 0.0007).sin();
                    (((t * 0.11).sin() + 0.7 * (t * 0.37).sin()) * 5_000.0 * am) as i16
                })
                .collect();
            p.push_render(&sig);
            let cleaned = p.process_capture(&sig);
            if block >= 200 {
                in_e += sig.iter().map(|&s| f64::from(s).powi(2)).sum::<f64>();
                out_e += cleaned.iter().map(|&s| f64::from(s).powi(2)).sum::<f64>();
            }
        }
        assert!(
            out_e < in_e * 0.5,
            "echo not attenuated: in={in_e:.0} out={out_e:.0}"
        );
    }

    #[test]
    fn passthrough_is_never_active() {
        assert!(!AecProcessor::passthrough().is_active());
    }

    #[test]
    fn passthrough_preserves_rms_within_1db() {
        // The listen-only bypass must NOT attenuate: post-AEC RMS parity within
        // ~1 dB of the input (the resample round-trip is the only processing).
        // This is the property the WebRTC NS+AGC path violates on marginal
        // speech — the round-8 listen-vs-test-mic delta.
        fn rms_dbfs(s: &[i16]) -> f32 {
            let ss: f64 = s.iter().map(|&x| (x as f64).powi(2)).sum();
            let r = (ss / s.len().max(1) as f64).sqrt();
            20.0 * (r / 32_768.0).log10() as f32
        }
        let mut p = AecProcessor::passthrough();
        // 400 Hz in-band tone at ~-14 dBFS, fed in 100 ms chunks.
        let chunk: Vec<i16> = (0..1_600)
            .map(|i| {
                let t = i as f32 / 16_000.0;
                ((std::f32::consts::TAU * 400.0 * t).sin() * 6_000.0) as i16
            })
            .collect();
        let in_dbfs = rms_dbfs(&chunk);
        let mut out: Vec<i16> = Vec::new();
        for _ in 0..10 {
            out.extend(p.process_capture(&chunk));
        }
        // Drop the resampler warm-up head where the round-trip ramps.
        let steady = &out[out.len() / 4..];
        let out_dbfs = rms_dbfs(steady);
        assert!(
            (out_dbfs - in_dbfs).abs() < 1.0,
            "passthrough must preserve level: in {in_dbfs:.1} vs out {out_dbfs:.1} dBFS"
        );
    }
}
