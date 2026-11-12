//! Native-Rust ECAPA-TDNN speaker embedder (192-d d-vectors via ONNX Runtime).
//!
//! Implements [`clawft_channels::voice::speaker::SpeakerEmbedder`] backed by the
//! `speechbrain/spkrec-ecapa-voxceleb` graph exported to ONNX (feature-input:
//! `feats [1, T, 80] → embedding [1, 1, 192]`). The front-end is the in-crate
//! [`Fbank`] (SpeechBrain-parity log-mel); inference mirrors the established
//! `ort` idiom in `clawft-kernel/src/embedding_qwen3.rs` (session build, tensor
//! IO, L2-norm).
//!
//! Weights are out-of-band (~83 MB, not in the repo). Construction never fails
//! and never requires the model: without the `onnx` feature, the model file, or
//! a loadable session, [`EcapaEmbedder::embed`] returns a clear `VoiceError`
//! rather than panicking, so the default build and unit tests need no weights.

use std::path::{Path, PathBuf};
#[cfg(feature = "onnx")]
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use clawft_channels::voice::speaker::SpeakerEmbedder;
use clawft_channels::voice::types::VoiceError;

use crate::fbank::Fbank;
#[cfg(feature = "onnx")]
use crate::fbank::{N_MELS, SAMPLE_RATE};

/// ECAPA embedding dimensionality (d-vector length).
pub const EMBED_DIM: usize = 192;

/// Default model filename under a `.weftos/models/ecapa/` directory.
const MODEL_FILE: &str = "ecapa_tdnn.onnx";
/// Environment override for the model path.
const MODEL_ENV: &str = "WEFTOS_ECAPA_MODEL";

/// Native ECAPA speaker embedder. Build with [`EcapaEmbedder::new`] (auto model
/// discovery) or [`EcapaEmbedder::with_model`].
pub struct EcapaEmbedder {
    /// Front-end; only read on the inference path (gated by `onnx`).
    #[cfg_attr(not(feature = "onnx"), allow(dead_code))]
    fbank: Fbank,
    model_path: PathBuf,
    runtime_available: bool,
    #[cfg(feature = "onnx")]
    session: Option<Arc<Mutex<ort::session::Session>>>,
}

impl EcapaEmbedder {
    /// Build with auto model discovery: `$WEFTOS_ECAPA_MODEL`, then
    /// `./.weftos/models/ecapa/ecapa_tdnn.onnx`, then the same under `$HOME`.
    pub fn new() -> Self {
        Self::with_model(default_model_path())
    }

    /// Build pointing at a specific model file. Degrades gracefully (a missing
    /// file or absent `onnx` feature simply leaves the runtime unavailable).
    pub fn with_model(model_path: impl Into<PathBuf>) -> Self {
        let model_path = model_path.into();
        #[cfg(feature = "onnx")]
        let session = Self::try_load_session(&model_path);
        #[cfg(feature = "onnx")]
        let runtime_available = session.is_some();
        #[cfg(not(feature = "onnx"))]
        let runtime_available = false;

        Self {
            fbank: Fbank::new(),
            model_path,
            runtime_available,
            #[cfg(feature = "onnx")]
            session,
        }
    }

    /// Whether real ONNX inference is active (vs. the model-absent error path).
    pub fn is_runtime_available(&self) -> bool {
        self.runtime_available
    }

    /// The resolved model path (may not exist).
    pub fn model_path(&self) -> &Path {
        &self.model_path
    }

    #[cfg(feature = "onnx")]
    fn try_load_session(model_path: &Path) -> Option<Arc<Mutex<ort::session::Session>>> {
        if !model_path.exists() {
            tracing::debug!("ECAPA ONNX model not found at {}", model_path.display());
            return None;
        }
        match crate::ort_env().and_then(ort::session::Session::builder).and_then(|mut b| b.commit_from_file(model_path)) {
            Ok(session) => {
                tracing::info!("ECAPA ONNX session loaded from {}", model_path.display());
                Some(Arc::new(Mutex::new(session)))
            }
            Err(e) => {
                tracing::warn!("failed to load ECAPA ONNX session: {e}");
                None
            }
        }
    }

    /// Run fbank → ONNX → L2-normalized 192-d embedding for 16 kHz mono PCM.
    #[cfg(feature = "onnx")]
    fn ecapa_embed(&self, samples_16k: &[f32]) -> Result<Vec<f32>, VoiceError> {
        use ort::value::Tensor;

        let session = self
            .session
            .as_ref()
            .ok_or_else(|| model_absent_error(&self.model_path))?;

        let (frames, feats) = self.fbank.compute(samples_16k);
        if frames == 0 {
            return Err(VoiceError::Malformed("ECAPA: utterance too short".into()));
        }

        let shape = vec![1_i64, frames as i64, N_MELS as i64];
        let feats_t = Tensor::from_array((shape, feats))
            .map_err(|e| VoiceError::Audio(format!("ECAPA feats tensor: {e}")))?;
        let inputs = ort::inputs!["feats" => feats_t];

        let mut guard = session
            .lock()
            .map_err(|_| VoiceError::Audio("ECAPA session mutex poisoned".into()))?;
        let outputs = guard
            .run(inputs)
            .map_err(|e| VoiceError::Audio(format!("ECAPA inference: {e}")))?;

        let output = match outputs.get("embedding") {
            Some(v) => v,
            None if outputs.len() > 0 => &outputs[0],
            None => return Err(VoiceError::Audio("ECAPA: no output tensor".into())),
        };
        let (_, data) = output
            .try_extract_tensor::<f32>()
            .map_err(|e| VoiceError::Audio(format!("ECAPA extract: {e}")))?;
        if data.len() < EMBED_DIM {
            return Err(VoiceError::Audio(format!(
                "ECAPA: short embedding ({})",
                data.len()
            )));
        }
        let mut emb = data[..EMBED_DIM].to_vec();
        l2_normalize(&mut emb);
        Ok(emb)
    }
}

impl Default for EcapaEmbedder {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SpeakerEmbedder for EcapaEmbedder {
    async fn embed(&self, audio: &[i16], sample_rate: u32) -> Result<Vec<f32>, VoiceError> {
        #[cfg(feature = "onnx")]
        {
            if !self.runtime_available {
                return Err(model_absent_error(&self.model_path));
            }
            let mono: Vec<f32> = audio.iter().map(|&s| s as f32 / 32_768.0).collect();
            let samples = resample_linear(&mono, sample_rate, SAMPLE_RATE);
            self.ecapa_embed(&samples)
        }
        #[cfg(not(feature = "onnx"))]
        {
            let _ = (audio, sample_rate);
            Err(model_absent_error(&self.model_path))
        }
    }

    fn dim(&self) -> usize {
        EMBED_DIM
    }
}

/// Resolve the model path: env override → cwd `.weftos` → `$HOME/.weftos`.
fn default_model_path() -> PathBuf {
    if let Ok(p) = std::env::var(MODEL_ENV) {
        return PathBuf::from(p);
    }
    let rel = PathBuf::from(".weftos/models/ecapa").join(MODEL_FILE);
    if rel.exists() {
        return rel;
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home)
            .join(".weftos/models/ecapa")
            .join(MODEL_FILE);
    }
    rel
}

/// Uniform "model unavailable" error (feature off or file missing).
fn model_absent_error(path: &Path) -> VoiceError {
    VoiceError::Audio(format!(
        "ECAPA ONNX model unavailable (looked for {}); build with --features onnx \
         and place the model there or set {MODEL_ENV}",
        path.display()
    ))
}

/// In-place L2 normalization (zero vector stays zero).
#[cfg_attr(not(feature = "onnx"), allow(dead_code))]
fn l2_normalize(v: &mut [f32]) {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        v.iter_mut().for_each(|x| *x /= norm);
    }
}

/// Linear-interpolation resampler to bring arbitrary input rates to 16 kHz.
/// Off the parity path (fixtures are already 16 kHz); adequate for speaker
/// embeddings, which are robust to mild resampling artefacts.
#[cfg_attr(not(feature = "onnx"), allow(dead_code))]
fn resample_linear(samples: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || samples.is_empty() {
        return samples.to_vec();
    }
    let ratio = to as f64 / from as f64;
    let out_len = ((samples.len() as f64) * ratio).round() as usize;
    (0..out_len)
        .map(|i| {
            let src = i as f64 / ratio;
            let i0 = src.floor() as usize;
            let frac = (src - i0 as f64) as f32;
            let a = samples.get(i0).copied().unwrap_or(0.0);
            let b = samples.get(i0 + 1).copied().unwrap_or(a);
            a + (b - a) * frac
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn construction_never_requires_model() {
        let e = EcapaEmbedder::with_model("/nonexistent/ecapa.onnx");
        assert_eq!(e.dim(), EMBED_DIM);
        assert!(!e.is_runtime_available());
    }

    #[tokio::test]
    async fn embed_without_model_errors_gracefully() {
        let e = EcapaEmbedder::with_model("/nonexistent/ecapa.onnx");
        let pcm = vec![0i16; 16_000];
        let err = e.embed(&pcm, crate::fbank::SAMPLE_RATE).await.unwrap_err();
        assert!(matches!(err, VoiceError::Audio(_)));
        assert!(err.to_string().contains("ECAPA"));
    }

    #[test]
    fn l2_normalize_unit_and_zero() {
        let mut v = vec![3.0, 4.0];
        l2_normalize(&mut v);
        let norm = (v[0] * v[0] + v[1] * v[1]).sqrt();
        assert!((norm - 1.0).abs() < 1e-6);
        let mut z = vec![0.0, 0.0];
        l2_normalize(&mut z);
        assert_eq!(z, vec![0.0, 0.0]);
    }

    #[test]
    fn resample_identity_and_ratio() {
        let s = vec![0.0, 1.0, 0.0, -1.0];
        assert_eq!(resample_linear(&s, 16_000, 16_000), s);
        let up = resample_linear(&s, 8_000, 16_000);
        assert_eq!(up.len(), 8);
    }

    #[test]
    fn default_model_path_honors_env() {
        // SAFETY: single-threaded test; restore afterwards.
        unsafe { std::env::set_var(MODEL_ENV, "/tmp/custom-ecapa.onnx") };
        assert_eq!(
            default_model_path(),
            PathBuf::from("/tmp/custom-ecapa.onnx")
        );
        unsafe { std::env::remove_var(MODEL_ENV) };
    }
}
