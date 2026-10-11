//! Offline comparison of Smart Turn end-of-turn probability against our
//! existing turn gate. No runtime behaviour is touched: the tool replays
//! recorded audio through the *real* gate pieces (`NoiseFloor` +
//! `SpectralVoiceness` for the voiced decision, `SemanticEndpointer` for the
//! finalize decision) and, at every candidate pause, asks a [`TurnModel`]
//! (Smart Turn in production, a stub in tests) for P(turn complete).
//!
//! # What "our decision" means here
//!
//! Offline there is no transcript, so the live heuristic endpointer has
//! nothing to read. "Ours" is the gate's silence-only behaviour: the real
//! [`SemanticEndpointer`] driven by [`PauseOnly`] (always "complete"), which
//! finalizes at the first pause of `short_silence_ms` once the turn carries
//! 400 ms of voiced audio. That is the cut-in behaviour Smart Turn is meant to
//! soften. Comparing against the live gate with the model attached would
//! compare the model with itself.
//!
//! # Input
//!
//! Talk Mode does not record audio today (`voice.trace` carries timings and
//! text, not PCM), so the input is a directory of WAVs. Start each file with
//! about a second of room tone: the gate calibrates its noise floor on the
//! first 500 ms. The gate's 400 ms voiced hangover is part of the replay, so a
//! silence here is the gate's view of it, as live.
//!
//! # Candidates
//!
//! Every silence run that follows voiced audio and lasts at least
//! `short_silence_ms` is a candidate. For each, the model is probed at
//! `short_silence_ms`, then every `step_ms`, up to `max_silence_ms` or the end
//! of the silence. The probability at the first probe is the "at pause"
//! verdict; the first probe at or above the threshold gives the model's end
//! point.

use std::time::Instant;

use async_trait::async_trait;
use clawft_channels::voice::turn::{EndpointModel, SemanticEndpointer, TurnDecision};
use clawft_channels::voice::vad::{EnergyVad, NoiseFloor};
use clawft_channels::voice::voiceness::{SpectralVoiceness, Voiceness};
use serde_json::{Value, json};

use crate::smart_turn::SmartTurnEndpoint;
use crate::whisper_mel::N_SAMPLES;

/// Live gate constants, mirrored from `clawft-channels` talkmode (private
/// there): onset margin over the tracked floor, and the voiceness onset floor.
const VAD_NOISE_MARGIN_DB: f32 = 10.0;
const VOICENESS_MIN: f32 = 0.5;
const SAMPLE_RATE: u32 = 16_000;

/// Anything that scores P(turn complete) for the trailing 16 kHz mono audio.
pub trait TurnModel {
    fn probability(&mut self, pcm_16k: &[f32]) -> Result<f32, String>;
}

impl TurnModel for SmartTurnEndpoint {
    fn probability(&mut self, pcm_16k: &[f32]) -> Result<f32, String> {
        SmartTurnEndpoint::probability(self, pcm_16k)
    }
}

/// Silence-only gate model: every pause reads "complete".
struct PauseOnly;

#[async_trait]
impl EndpointModel for PauseOnly {
    async fn completion_prob(&self, _audio: &[i16], _text: &str) -> f32 {
        1.0
    }
    fn source(&self) -> &'static str {
        "pause-only"
    }
}

/// Tool parameters.
#[derive(Debug, Clone)]
pub struct CompareConfig {
    /// Completion probability at or above which a turn counts as complete.
    pub threshold: f32,
    /// Pause length that makes a silence a candidate (live default 500 ms).
    pub short_silence_ms: u32,
    /// Longest silence probed (live default 2500 ms).
    pub max_silence_ms: u32,
    /// Spacing between probes inside one pause.
    pub step_ms: u32,
    /// Our end this much later than the model's end counts as late.
    pub late_margin_ms: u32,
    /// Replay frame size.
    pub frame_ms: u32,
}

impl Default for CompareConfig {
    fn default() -> Self {
        Self {
            threshold: 0.5,
            short_silence_ms: 500,
            max_silence_ms: 2_500,
            step_ms: 100,
            late_margin_ms: 300,
            frame_ms: 20,
        }
    }
}

impl CompareConfig {
    /// Reject values that would make the replay meaningless.
    pub fn validate(&self) -> Result<(), String> {
        if !(0.0..=1.0).contains(&self.threshold) {
            return Err(format!("threshold {} outside [0, 1]", self.threshold));
        }
        if self.frame_ms == 0 || self.step_ms == 0 || self.short_silence_ms == 0 {
            return Err("frame_ms, step_ms and short_silence_ms must be > 0".into());
        }
        if self.max_silence_ms < self.short_silence_ms {
            return Err("max_silence_ms must be >= short_silence_ms".into());
        }
        Ok(())
    }

    pub fn to_json(&self) -> Value {
        json!({
            "threshold": self.threshold,
            "short_silence_ms": self.short_silence_ms,
            "max_silence_ms": self.max_silence_ms,
            "step_ms": self.step_ms,
            "late_margin_ms": self.late_margin_ms,
            "frame_ms": self.frame_ms,
        })
    }
}

/// One candidate pause, with both verdicts.
#[derive(Debug, Clone, PartialEq)]
pub struct PauseRecord {
    pub file: String,
    pub index: usize,
    /// Time of the last voiced sample before the pause.
    pub speech_end_ms: u64,
    pub silence_ms: u64,
    /// Voiced audio in the turn so far (resets when our gate finalizes).
    pub turn_voiced_ms: u64,
    /// Speech came back after this silence (as opposed to end of file).
    pub resumed: bool,
    pub ours_finalized: bool,
    /// Silence length at which our gate finalized.
    pub ours_end_silence_ms: Option<u64>,
    /// Model probability at the first probe (`short_silence_ms`).
    pub model_prob: f32,
    pub model_complete: bool,
    /// Silence length of the first probe at or above the threshold.
    pub model_end_silence_ms: Option<u64>,
    pub agree: bool,
    /// Our gate ended the turn, the model says incomplete.
    pub premature_cut_in: bool,
    /// Premature, and the speaker did continue afterwards.
    pub confirmed_cut_in: bool,
    /// The model would have ended the turn clearly before ours (or ours never did).
    pub late_end: bool,
    /// First-probe inference time.
    pub latency_ms: f64,
}

impl PauseRecord {
    pub fn to_json(&self) -> Value {
        json!({
            "file": self.file,
            "index": self.index,
            "speech_end_ms": self.speech_end_ms,
            "silence_ms": self.silence_ms,
            "turn_voiced_ms": self.turn_voiced_ms,
            "resumed": self.resumed,
            "ours_finalized": self.ours_finalized,
            "ours_end_silence_ms": self.ours_end_silence_ms,
            "model_prob": self.model_prob,
            "model_complete": self.model_complete,
            "model_end_silence_ms": self.model_end_silence_ms,
            "agree": self.agree,
            "premature_cut_in": self.premature_cut_in,
            "confirmed_cut_in": self.confirmed_cut_in,
            "late_end": self.late_end,
            "latency_ms": self.latency_ms,
        })
    }
}

/// Result of replaying one recording.
#[derive(Debug, Clone, Default)]
pub struct FileResult {
    pub file: String,
    pub duration_ms: u64,
    /// Silences shorter than `short_silence_ms` (never candidates).
    pub skipped_short_pauses: usize,
    pub pauses: Vec<PauseRecord>,
    /// Every inference call's duration, including the extra probes.
    pub latencies_ms: Vec<f64>,
}

/// Voiced flag per `frame_len`-sample frame, using the live gate.
pub fn voiced_flags(samples: &[i16], frame_len: usize) -> Vec<bool> {
    let mut floor = NoiseFloor::new(VAD_NOISE_MARGIN_DB, SAMPLE_RATE);
    let voiceness = SpectralVoiceness::new();
    let mut in_run = false;
    samples
        .chunks_exact(frame_len)
        .map(|f| {
            let energy = floor.classify(EnergyVad::rms_dbfs(f), f.len() as u64);
            let score = voiceness.score(f, SAMPLE_RATE);
            let voiced = if in_run {
                energy
            } else {
                energy && score >= VOICENESS_MIN
            };
            in_run = voiced;
            voiced
        })
        .collect()
}

/// Replay `samples` (16 kHz mono) and compare `model` with the gate.
pub async fn compare_audio(
    name: &str,
    samples: &[i16],
    cfg: &CompareConfig,
    model: &mut dyn TurnModel,
) -> Result<FileResult, String> {
    cfg.validate()?;
    let frame_len = (SAMPLE_RATE * cfg.frame_ms / 1_000) as usize;
    let flags = voiced_flags(samples, frame_len);
    let frame_ms = u64::from(cfg.frame_ms);
    let mut endpointer = SemanticEndpointer::new(
        PauseOnly,
        SAMPLE_RATE,
        cfg.short_silence_ms,
        cfg.max_silence_ms,
        cfg.threshold,
    );

    let mut out = FileResult {
        file: name.to_string(),
        duration_ms: samples.len() as u64 * 1_000 / u64::from(SAMPLE_RATE),
        ..Default::default()
    };
    let mut turn_voiced_ms = 0u64;
    let mut i = 0;
    while i < flags.len() {
        if flags[i] {
            turn_voiced_ms += frame_ms;
            endpointer
                .observe(&samples[i * frame_len..(i + 1) * frame_len], true, "")
                .await;
            i += 1;
            continue;
        }
        // Silence run [i, j).
        let mut j = i;
        while j < flags.len() && !flags[j] {
            j += 1;
        }
        let had_speech = i > 0 && flags[i - 1];
        let mut ours_end = None;
        for (k, idx) in (i..j).enumerate() {
            let d = endpointer
                .observe(&samples[idx * frame_len..(idx + 1) * frame_len], false, "")
                .await;
            if d == TurnDecision::Finalize && ours_end.is_none() {
                ours_end = Some((k as u64 + 1) * frame_ms);
            }
        }
        let silence_ms = (j - i) as u64 * frame_ms;
        if had_speech {
            if silence_ms < u64::from(cfg.short_silence_ms) {
                out.skipped_short_pauses += 1;
            } else {
                let speech_end = i * frame_len;
                let rec = probe_pause(
                    name,
                    out.pauses.len(),
                    samples,
                    speech_end,
                    silence_ms,
                    turn_voiced_ms,
                    j < flags.len(),
                    ours_end,
                    cfg,
                    model,
                    &mut out.latencies_ms,
                )?;
                out.pauses.push(rec);
            }
        }
        if ours_end.is_some() {
            turn_voiced_ms = 0;
        }
        i = j;
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn probe_pause(
    name: &str,
    index: usize,
    samples: &[i16],
    speech_end: usize,
    silence_ms: u64,
    turn_voiced_ms: u64,
    resumed: bool,
    ours_end: Option<u64>,
    cfg: &CompareConfig,
    model: &mut dyn TurnModel,
    latencies: &mut Vec<f64>,
) -> Result<PauseRecord, String> {
    let last = silence_ms.min(u64::from(cfg.max_silence_ms));
    let mut probe_at = u64::from(cfg.short_silence_ms);
    let (mut first_prob, mut first_latency, mut end_at) = (None, 0.0, None);
    while probe_at <= last {
        let end =
            (speech_end + (probe_at * u64::from(SAMPLE_RATE) / 1_000) as usize).min(samples.len());
        let start = end.saturating_sub(N_SAMPLES);
        let pcm: Vec<f32> = samples[start..end]
            .iter()
            .map(|&s| f32::from(s) / 32_768.0)
            .collect();
        let t = Instant::now();
        let p = model
            .probability(&pcm)
            .map_err(|e| format!("{name}: model: {e}"))?;
        let ms = t.elapsed().as_secs_f64() * 1_000.0;
        latencies.push(ms);
        if first_prob.is_none() {
            first_prob = Some(p);
            first_latency = ms;
        }
        if p >= cfg.threshold {
            end_at = Some(probe_at);
            break;
        }
        probe_at += u64::from(cfg.step_ms);
    }
    let model_prob = first_prob.ok_or_else(|| format!("{name}: pause without a probe"))?;
    let model_complete = model_prob >= cfg.threshold;
    let ours_finalized = ours_end.is_some();
    let premature = ours_finalized && !model_complete;
    let late = match (ours_end, end_at) {
        (None, Some(_)) => true,
        (Some(o), Some(m)) => o > m + u64::from(cfg.late_margin_ms),
        _ => false,
    };
    Ok(PauseRecord {
        file: name.to_string(),
        index,
        speech_end_ms: speech_end as u64 * 1_000 / u64::from(SAMPLE_RATE),
        silence_ms,
        turn_voiced_ms,
        resumed,
        ours_finalized,
        ours_end_silence_ms: ours_end,
        model_prob,
        model_complete,
        model_end_silence_ms: end_at,
        agree: ours_finalized == model_complete,
        premature_cut_in: premature,
        confirmed_cut_in: premature && resumed,
        late_end: late,
        latency_ms: first_latency,
    })
}

#[cfg(test)]
#[path = "turn_compare_tests.rs"]
mod tests;
