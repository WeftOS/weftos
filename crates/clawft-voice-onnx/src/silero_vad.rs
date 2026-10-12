//! Silero VAD v5 — neural voice activity detection behind the [`Voiceness`] seam.
//!
//! Implements stateful ONNX inference matching
//! [`snakers4/silero-vad`](https://github.com/snakers4/silero-vad) `OnnxWrapper`:
//!
//! * 16 kHz fixed **512-sample** windows (256 @ 8 kHz — we only drive 16 kHz)
//! * Recurrent **state** `[2, 1, 128]` (h/c) carried across frames
//! * **Context** of the last 64 samples prepended to each window
//! * Inputs: `input` `[1, 576]`, `state` `[2, 1, 128]`, `sr` `int64` 16000
//! * Output: speech probability in `[0, 1]` (graph already sigmoid-activated)
//!
//! Weights are out-of-band (~2.3 MB, not in the repo). Construction never fails
//! and never requires the model: without the `onnx` feature, the model file, or
//! a loadable session, [`SileroVoiceness`] degrades to [`SpectralVoiceness`].
//! [`preferred_voiceness`] selects Silero when the model is present, else
//! spectral — the runtime-detect path Talk Mode wires via
//! [`TalkModeController::with_voiceness`](clawft_channels::voice::talkmode::TalkModeController::with_voiceness).
//!
//! # Staging
//!
//! Place `silero_vad.onnx` at:
//!
//! * `$WEFTOS_SILERO_VAD_MODEL` (file path override), or
//! * `./.weftos/models/silero-vad/silero_vad.onnx`, or
//! * `$HOME/.weftos/models/silero-vad/silero_vad.onnx`, or
//! * `$HOME/.clawft/models/voice/silero-vad-v5` (plugin catalog cache)
//!
//! Catalog entry: `clawft_plugin::voice::models` `silero-vad-v5`.
//! [`stage_model_file`] copies a downloaded ONNX into the home weftos dir.

use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(feature = "onnx")]
use std::sync::Mutex;

use clawft_channels::voice::voiceness::{SpectralVoiceness, Voiceness};

/// Default model filename under `.weftos/models/silero-vad/`.
pub const MODEL_FILE: &str = "silero_vad.onnx";
/// Catalog / cache id used by `ModelDownloadManager` (`~/.clawft/models/voice/`).
pub const CATALOG_ID: &str = "silero-vad-v5";
/// Environment override for the model path.
pub const MODEL_ENV: &str = "WEFTOS_SILERO_VAD_MODEL";

/// 16 kHz window size required by Silero v5.
#[cfg(feature = "onnx")]
const WINDOW_SAMPLES: usize = 512;
/// Context samples prepended each step (16 kHz path).
#[cfg(feature = "onnx")]
const CONTEXT_SAMPLES: usize = 64;
/// LSTM state: 2 × batch × 128.
#[cfg(feature = "onnx")]
const STATE_LEN: usize = 2 * 128;
#[cfg(feature = "onnx")]
const STATE_SHAPE: [i64; 3] = [2, 1, 128];
/// Target sample rate for the staged model.
#[cfg(feature = "onnx")]
const SAMPLE_RATE: u32 = 16_000;

/// Stateful Silero-VAD scorer implementing [`Voiceness`].
///
/// When ONNX is unavailable, every `score` call routes through an embedded
/// [`SpectralVoiceness`] so the AND-gate never goes deaf.
pub struct SileroVoiceness {
    model_path: PathBuf,
    runtime_available: bool,
    /// Spectral fallback (always constructed — used when model is absent or
    /// an inference step fails).
    fallback: SpectralVoiceness,
    #[cfg(feature = "onnx")]
    session: Option<Arc<Mutex<ort::session::Session>>>,
    #[cfg(feature = "onnx")]
    inner: Mutex<SileroInner>,
}

/// Mutable recurrent + streaming state for one session.
#[cfg(feature = "onnx")]
struct SileroInner {
    /// Flattened `[2, 1, 128]` h/c state.
    state: Vec<f32>,
    /// Last `CONTEXT_SAMPLES` of the previous (context‖window) input.
    context: Vec<f32>,
    /// Accumulator for partial capture frames until a full 512-sample window.
    pending: Vec<f32>,
    /// Last speech probability (returned while still filling a window).
    last_prob: f32,
    /// Warm-up: true until the first full window has been scored.
    warmed: bool,
}

impl SileroVoiceness {
    /// Build with auto model discovery (env → cwd → `$HOME` → clawft cache).
    pub fn new() -> Self {
        Self::with_model(default_model_path())
    }

    /// Build pointing at a specific model file. Degrades gracefully when the
    /// file is missing or the `onnx` feature is off.
    pub fn with_model(model_path: impl Into<PathBuf>) -> Self {
        let model_path = model_path.into();
        #[cfg(feature = "onnx")]
        let session = Self::try_load_session(&model_path);
        #[cfg(feature = "onnx")]
        let runtime_available = session.is_some();
        #[cfg(not(feature = "onnx"))]
        let runtime_available = false;

        Self {
            model_path,
            runtime_available,
            fallback: SpectralVoiceness::new(),
            #[cfg(feature = "onnx")]
            session,
            #[cfg(feature = "onnx")]
            inner: Mutex::new(SileroInner::fresh()),
        }
    }

    /// Whether real ONNX inference is active.
    pub fn is_runtime_available(&self) -> bool {
        self.runtime_available
    }

    /// The resolved model path (may not exist).
    pub fn model_path(&self) -> &Path {
        &self.model_path
    }

    /// Reset recurrent state + pending buffer (e.g. between sessions).
    pub fn reset(&self) {
        #[cfg(feature = "onnx")]
        {
            if let Ok(mut g) = self.inner.lock() {
                *g = SileroInner::fresh();
            }
        }
    }

    #[cfg(feature = "onnx")]
    fn try_load_session(model_path: &Path) -> Option<Arc<Mutex<ort::session::Session>>> {
        if !model_path.exists() {
            tracing::debug!(
                "Silero VAD ONNX model not found at {}",
                model_path.display()
            );
            return None;
        }
        match ort::session::Session::builder().and_then(|mut b| b.commit_from_file(model_path)) {
            Ok(session) => {
                tracing::info!(
                    "Silero VAD ONNX session loaded from {}",
                    model_path.display()
                );
                Some(Arc::new(Mutex::new(session)))
            }
            Err(e) => {
                tracing::warn!("failed to load Silero VAD ONNX session: {e}");
                None
            }
        }
    }

    /// Run one 512-sample window through the graph; updates `inner` state.
    #[cfg(feature = "onnx")]
    fn infer_window(&self, window: &[f32], inner: &mut SileroInner) -> Result<f32, String> {
        use ort::value::Tensor;

        debug_assert_eq!(window.len(), WINDOW_SAMPLES);
        let session = self.session.as_ref().ok_or("session unavailable")?;

        // context ‖ window → [1, 576]
        let mut input = Vec::with_capacity(CONTEXT_SAMPLES + WINDOW_SAMPLES);
        if inner.context.len() == CONTEXT_SAMPLES {
            input.extend_from_slice(&inner.context);
        } else {
            input.extend(std::iter::repeat_n(0.0f32, CONTEXT_SAMPLES));
        }
        input.extend_from_slice(window);

        let input_t = Tensor::from_array((
            vec![1_i64, input.len() as i64],
            input.clone(),
        ))
        .map_err(|e| format!("input tensor: {e}"))?;

        let state_t = Tensor::from_array((STATE_SHAPE.to_vec(), inner.state.clone()))
            .map_err(|e| format!("state tensor: {e}"))?;

        // Scalar int64 sample rate (matches silero OnnxWrapper: np.array(sr, int64)).
        let sr_t = Tensor::from_array((Vec::<i64>::new(), vec![i64::from(SAMPLE_RATE)]))
            .map_err(|e| format!("sr tensor: {e}"))?;

        let inputs = ort::inputs![
            "input" => input_t,
            "state" => state_t,
            "sr" => sr_t,
        ];

        let mut guard = session.lock().map_err(|_| "session mutex poisoned")?;
        let outputs = guard.run(inputs).map_err(|e| format!("inference: {e}"))?;

        // Prefer named outputs; fall back to positional (out, stateN).
        let prob = {
            let out = match outputs.get("output") {
                Some(v) => v,
                None if outputs.len() > 0 => &outputs[0],
                None => return Err("no output tensor".into()),
            };
            let (_, data) = out
                .try_extract_tensor::<f32>()
                .map_err(|e| format!("extract output: {e}"))?;
            *data.first().ok_or("empty output")?
        };

        let new_state = {
            let st = match outputs.get("stateN") {
                Some(v) => v,
                None if outputs.len() > 1 => &outputs[1],
                None => return Err("no stateN tensor".into()),
            };
            let (_, data) = st
                .try_extract_tensor::<f32>()
                .map_err(|e| format!("extract state: {e}"))?;
            if data.len() < STATE_LEN {
                return Err(format!("short state tensor ({})", data.len()));
            }
            data[..STATE_LEN].to_vec()
        };

        // Carry the tail of (context‖window) as next context.
        inner.context = input[input.len() - CONTEXT_SAMPLES..].to_vec();
        inner.state = new_state;
        Ok(prob.clamp(0.0, 1.0))
    }
}

impl Default for SileroVoiceness {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for SileroVoiceness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SileroVoiceness")
            .field("model_path", &self.model_path)
            .field("runtime_available", &self.runtime_available)
            .finish_non_exhaustive()
    }
}

impl Voiceness for SileroVoiceness {
    fn score(&self, frame: &[i16], sample_rate: u32) -> f32 {
        if frame.is_empty() || sample_rate == 0 {
            return 0.0;
        }

        #[cfg(feature = "onnx")]
        {
            if self.runtime_available {
                // Only 16 kHz path is implemented; non-16k falls back so the
                // gate still has a shape estimate.
                if sample_rate != SAMPLE_RATE {
                    tracing::trace!(
                        "SileroVoiceness: sample_rate {sample_rate} != {SAMPLE_RATE}; spectral fallback"
                    );
                    return self.fallback.score(frame, sample_rate);
                }

                let mono: Vec<f32> = frame.iter().map(|&s| f32::from(s) / 32_768.0).collect();
                let mut inner = match self.inner.lock() {
                    Ok(g) => g,
                    Err(_) => return self.fallback.score(frame, sample_rate),
                };
                inner.pending.extend_from_slice(&mono);

                let mut ran = false;
                while inner.pending.len() >= WINDOW_SAMPLES {
                    let window: Vec<f32> = inner.pending.drain(..WINDOW_SAMPLES).collect();
                    match self.infer_window(&window, &mut inner) {
                        Ok(p) => {
                            inner.last_prob = p;
                            inner.warmed = true;
                            ran = true;
                        }
                        Err(e) => {
                            tracing::warn!(
                                "Silero VAD inference failed ({e}); spectral fallback this frame"
                            );
                            drop(inner);
                            return self.fallback.score(frame, sample_rate);
                        }
                    }
                }

                // Warm-up (no full window yet): don't block the energy gate.
                if !inner.warmed && !ran {
                    return 1.0;
                }
                return inner.last_prob;
            }
        }

        #[cfg(not(feature = "onnx"))]
        {
            let _ = sample_rate;
        }

        self.fallback.score(frame, sample_rate)
    }
}

#[cfg(feature = "onnx")]
impl SileroInner {
    fn fresh() -> Self {
        Self {
            state: vec![0.0; STATE_LEN],
            context: Vec::new(),
            pending: Vec::with_capacity(WINDOW_SAMPLES),
            last_prob: 1.0,
            warmed: false,
        }
    }
}

/// Prefer Silero when the ONNX model is loadable; otherwise the model-free
/// spectral gate. The value Talk Mode injects via `with_voiceness`.
pub fn preferred_voiceness() -> Arc<dyn Voiceness> {
    let silero = SileroVoiceness::new();
    if silero.is_runtime_available() {
        tracing::info!(
            path = %silero.model_path().display(),
            "voiceness backend: Silero VAD (neural)"
        );
        Arc::new(silero)
    } else {
        tracing::debug!(
            path = %silero.model_path().display(),
            "voiceness backend: SpectralVoiceness (Silero model absent)"
        );
        Arc::new(SpectralVoiceness::new())
    }
}

/// Canonical home staging directory: `$HOME/.weftos/models/silero-vad/`.
pub fn home_model_dir() -> PathBuf {
    if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".weftos/models/silero-vad")
    } else {
        PathBuf::from(".weftos/models/silero-vad")
    }
}

/// Copy `src` ONNX file into the home staging dir as `silero_vad.onnx`.
///
/// Used by setup tooling / tests after a catalog download so runtime discovery
/// finds the model under `~/.weftos/models/silero-vad/`.
pub fn stage_model_file(src: &Path) -> std::io::Result<PathBuf> {
    let dir = home_model_dir();
    std::fs::create_dir_all(&dir)?;
    let dest = dir.join(MODEL_FILE);
    std::fs::copy(src, &dest)?;
    Ok(dest)
}

/// Resolve model path: env → cwd `.weftos` → `$HOME/.weftos` → clawft voice cache.
pub fn default_model_path() -> PathBuf {
    if let Ok(p) = std::env::var(MODEL_ENV) {
        return PathBuf::from(p);
    }
    let rel = PathBuf::from(".weftos/models/silero-vad").join(MODEL_FILE);
    if rel.exists() {
        return rel;
    }
    if let Ok(home) = std::env::var("HOME") {
        let home_path = PathBuf::from(&home)
            .join(".weftos/models/silero-vad")
            .join(MODEL_FILE);
        if home_path.exists() {
            return home_path;
        }
        // Plugin catalog cache (`weft voice setup` / ModelDownloadManager).
        let cache = PathBuf::from(home)
            .join(".clawft/models/voice")
            .join(CATALOG_ID);
        if cache.exists() {
            return cache;
        }
        return home_path;
    }
    rel
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    const SR: u32 = 16_000;

    fn speechlike(len: usize, amp: f32) -> Vec<i16> {
        let partials = [180.0f32, 360.0, 540.0, 900.0, 1400.0];
        (0..len)
            .map(|i| {
                let t = i as f32 / SR as f32;
                let s: f32 = partials.iter().map(|&f| (TAU * f * t).sin()).sum();
                (s / partials.len() as f32 * amp) as i16
            })
            .collect()
    }

    fn broadband(len: usize, amp: f32, seed: u32) -> Vec<i16> {
        let mut state = seed.wrapping_mul(2_654_435_761).max(1);
        (0..len)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let u = (state >> 8) as f32 / (1u32 << 24) as f32;
                ((u - 0.5) * 2.0 * amp) as i16
            })
            .collect()
    }

    #[test]
    fn construction_never_requires_model() {
        let v = SileroVoiceness::with_model("/nonexistent/silero_vad.onnx");
        assert!(!v.is_runtime_available());
        assert_eq!(
            v.model_path(),
            Path::new("/nonexistent/silero_vad.onnx")
        );
    }

    #[test]
    fn falls_back_to_spectral_without_model() {
        let v = SileroVoiceness::with_model("/nonexistent/silero_vad.onnx");
        // Spectral path: speech-like in-band energy scores high.
        let speech = v.score(&speechlike(1_600, 6_000.0), SR);
        assert!(
            speech > 0.5,
            "fallback spectral must score speech high, got {speech}"
        );
        // Fresh instance for noise so the rolling buffer doesn't mix.
        let v2 = SileroVoiceness::with_model("/nonexistent/silero_vad.onnx");
        let noise = v2.score(&broadband(1_600, 6_000.0, 7), SR);
        assert!(noise < 0.5, "fallback spectral must score noise low, got {noise}");
    }

    #[test]
    fn empty_frame_scores_zero() {
        let v = SileroVoiceness::with_model("/nonexistent/silero_vad.onnx");
        assert_eq!(v.score(&[], SR), 0.0);
    }

    #[test]
    fn default_path_honors_env() {
        // SAFETY: single-threaded test; restore afterwards.
        unsafe { std::env::set_var(MODEL_ENV, "/tmp/custom-silero.onnx") };
        assert_eq!(
            default_model_path(),
            PathBuf::from("/tmp/custom-silero.onnx")
        );
        unsafe { std::env::remove_var(MODEL_ENV) };
    }

    #[test]
    fn preferred_without_model_is_spectral_shaped() {
        // Force miss via env override so we don't pick up a staged model on the
        // developer machine and accidentally assert the wrong backend type.
        unsafe { std::env::set_var(MODEL_ENV, "/nonexistent/silero_for_preferred.onnx") };
        let v = preferred_voiceness();
        // SpectralVoiceness (or Silero with unavailable runtime if discovery
        // races) — either way scoring must work and speech must clear.
        let s = v.score(&speechlike(1_600, 6_000.0), SR);
        unsafe { std::env::remove_var(MODEL_ENV) };
        assert!(s > 0.5, "preferred fallback must score speech, got {s}");
    }

    #[test]
    fn stage_model_file_copies_into_home_dir() {
        let src = {
            let p = std::env::temp_dir().join("weft-644-silero-stage-src.onnx");
            std::fs::write(&p, b"fake-onnx-bytes-for-stage-test").unwrap();
            p
        };
        // Redirect HOME so we don't clobber a real staged model.
        let tmp_home = std::env::temp_dir().join("weft-644-home");
        let _ = std::fs::remove_dir_all(&tmp_home);
        std::fs::create_dir_all(&tmp_home).unwrap();
        let prev_home = std::env::var("HOME").ok();
        unsafe { std::env::set_var("HOME", &tmp_home) };

        let dest = stage_model_file(&src).expect("stage");
        assert!(dest.exists());
        assert_eq!(dest.file_name().unwrap(), "silero_vad.onnx");
        assert_eq!(std::fs::read(&dest).unwrap(), b"fake-onnx-bytes-for-stage-test");

        match prev_home {
            Some(h) => unsafe { std::env::set_var("HOME", h) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        let _ = std::fs::remove_file(&src);
        let _ = std::fs::remove_dir_all(&tmp_home);
    }

    /// Live ONNX path — requires staged weights + `--features onnx`.
    /// Uses the JFK fixture (real speech) vs silence; pure sines are not a
    /// reliable Silero trigger (the model expects natural speech statistics).
    #[cfg(feature = "onnx")]
    #[test]
    fn silero_speech_scores_above_noise_when_model_present() {
        let path = default_model_path();
        if !path.exists() {
            eprintln!("skip: no Silero model at {}", path.display());
            return;
        }
        let v = SileroVoiceness::with_model(&path);
        assert!(v.is_runtime_available(), "model present but session failed");

        // Prefer a real 16 kHz speech fixture when available; fall back to
        // loud in-band sines (weaker Silero trigger but still usable).
        let speech_pcm: Vec<i16> = {
            let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
            // onyx_a / dan are 16 kHz mono; jfk_en is 24 kHz — skip it.
            let pick = ["onyx_a.wav", "dan.wav"]
                .into_iter()
                .map(|n| fixtures.join(n))
                .find(|p| p.exists());
            if let Some(fixture) = pick {
                let mut reader = hound::WavReader::open(&fixture)
                    .unwrap_or_else(|e| panic!("open {}: {e}", fixture.display()));
                assert_eq!(
                    reader.spec().sample_rate,
                    16_000,
                    "fixture must be 16 kHz ({})",
                    fixture.display()
                );
                reader.samples::<i16>().map(|s| s.unwrap()).collect()
            } else {
                speechlike(16_000, 12_000.0)
            }
        };

        // Warm-up returns 1.0 until the first full 512-sample window so the
        // energy AND-gate isn't deafened; only score after that for assertions.
        let mut speech_max = 0.0f32;
        let mut speech_last = 0.0f32;
        let mut warmed_scores = 0u32;
        for chunk in speech_pcm.chunks(160) {
            speech_last = v.score(chunk, SR);
            warmed_scores = warmed_scores.saturating_add(1);
            // Skip the first ~4×160-sample frames (warm-up + one window).
            if warmed_scores > 4 {
                speech_max = speech_max.max(speech_last);
            }
        }
        v.reset();

        // Near-silence should stay low after warm-up.
        let mut silence_max = 0.0f32;
        for i in 0..40 {
            let silence = v.score(&[0i16; 160], SR);
            if i >= 4 {
                silence_max = silence_max.max(silence);
            }
        }
        v.reset();

        // Broadband noise — Silero usually keeps this well below speech.
        let mut noise_last = 1.0f32;
        let mut noise_max = 0.0f32;
        for (i, chunk) in broadband(speech_pcm.len().max(3_200), 4_000.0, 42)
            .chunks(160)
            .enumerate()
        {
            noise_last = v.score(chunk, SR);
            if i >= 4 {
                noise_max = noise_max.max(noise_last);
            }
        }

        println!(
            "silero speech_last={speech_last:.4} speech_max={speech_max:.4} \
             silence_max={silence_max:.4} noise_last={noise_last:.4} noise_max={noise_max:.4}"
        );
        assert!(
            speech_max > silence_max + 0.2,
            "speech max must clearly beat silence: speech_max={speech_max} silence_max={silence_max}"
        );
        assert!(
            speech_max > noise_max,
            "speech max must beat noise max: speech={speech_max} noise={noise_max}"
        );
        assert!(
            speech_max >= 0.5,
            "expected elevated speech prob on real speech, got max {speech_max}"
        );
        assert!(
            silence_max < 0.3,
            "silence should stay low after warm-up, got {silence_max}"
        );
    }

    /// First-window smoke: zero state + one 512-sample frame must return a
    /// finite probability without erroring (guards tensor wiring).
    #[cfg(feature = "onnx")]
    #[test]
    fn silero_single_window_inference_smoke() {
        let path = default_model_path();
        if !path.exists() {
            eprintln!("skip: no Silero model at {}", path.display());
            return;
        }
        let v = SileroVoiceness::with_model(&path);
        assert!(v.is_runtime_available());
        // One exact window in a single score call.
        let frame = speechlike(WINDOW_SAMPLES, 10_000.0);
        let p = v.score(&frame, SR);
        assert!(
            (0.0..=1.0).contains(&p),
            "prob out of range: {p}"
        );
        println!("single-window speechlike prob={p:.4}");
    }
}
