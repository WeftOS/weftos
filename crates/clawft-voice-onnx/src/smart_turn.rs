//! Native-Rust smart-turn-v3 semantic endpointer (Whisper-Tiny encoder + linear
//! head via ONNX Runtime).
//!
//! Implements [`clawft_channels::voice::turn::EndpointModel`]: given the recent
//! audio window it returns the probability in `[0, 1]` that the speaker has
//! finished their turn — a *semantic* read of the waveform, not the transcript
//! (`pipecat-ai/smart-turn-v3`). The front-end is the in-crate [`WhisperMel`];
//! inference mirrors the `ort` idiom in [`crate::ecapa`] (session build, tensor
//! IO). The ONNX graph already applies the final sigmoid, so its `logits`
//! output *is* the completion probability.
//!
//! Weights are out-of-band (~8 MB int8 ONNX, not in the repo). Construction
//! never fails and never requires the model: without the `onnx` feature, the
//! model file, or a loadable session, [`completion_prob`](EndpointModel::completion_prob)
//! falls back to the no-model [`HeuristicEndpoint`] so the default build and the
//! controller keep working without weights.

use std::path::{Path, PathBuf};
#[cfg(feature = "onnx")]
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use clawft_channels::voice::turn::{EndpointModel, HeuristicEndpoint};

use crate::whisper_mel::WhisperMel;
#[cfg(feature = "onnx")]
use crate::whisper_mel::{N_FRAMES, N_MELS};

/// Default model filename under a `.weftos/models/smart-turn/` directory
/// (the int8 CPU export from `pipecat-ai/smart-turn-v3`).
const MODEL_FILE: &str = "smart-turn-v3.1-cpu.onnx";
/// Environment override for the model path.
const MODEL_ENV: &str = "WEFTOS_SMART_TURN_MODEL";

/// Native smart-turn-v3 endpoint model. Build with [`SmartTurnEndpoint::new`]
/// (auto model discovery) or [`SmartTurnEndpoint::with_model`]. Falls back to
/// [`HeuristicEndpoint`] whenever the ONNX runtime/model is unavailable.
pub struct SmartTurnEndpoint {
    /// Front-end; only read on the inference path (gated by `onnx`).
    #[cfg_attr(not(feature = "onnx"), allow(dead_code))]
    mel: WhisperMel,
    model_path: PathBuf,
    runtime_available: bool,
    /// No-model heuristic used when ONNX is unavailable or inference errors.
    fallback: HeuristicEndpoint,
    #[cfg(feature = "onnx")]
    session: Option<Arc<Mutex<ort::session::Session>>>,
}

impl SmartTurnEndpoint {
    /// Build with auto model discovery: `$WEFTOS_SMART_TURN_MODEL`, then
    /// `./.weftos/models/smart-turn/…`, then the same under `$HOME`.
    pub fn new() -> Self {
        Self::with_model(default_model_path())
    }

    /// Build pointing at a specific model file. Degrades gracefully (a missing
    /// file or absent `onnx` feature simply leaves the runtime unavailable and
    /// routes through the heuristic fallback).
    pub fn with_model(model_path: impl Into<PathBuf>) -> Self {
        let model_path = model_path.into();
        #[cfg(feature = "onnx")]
        let session = Self::try_load_session(&model_path);
        #[cfg(feature = "onnx")]
        let runtime_available = session.is_some();
        #[cfg(not(feature = "onnx"))]
        let runtime_available = false;

        Self {
            mel: WhisperMel::new(),
            model_path,
            runtime_available,
            fallback: HeuristicEndpoint,
            #[cfg(feature = "onnx")]
            session,
        }
    }

    /// Whether real ONNX inference is active (vs. the heuristic fallback path).
    pub fn is_runtime_available(&self) -> bool {
        self.runtime_available
    }

    /// The resolved model path (may not exist).
    pub fn model_path(&self) -> &Path {
        &self.model_path
    }

    /// Strict model probability for 16 kHz mono f32 PCM: no heuristic fallback,
    /// so offline tooling can tell "model said X" from "model unavailable".
    /// Returns `Err` when the `onnx` feature, the model file or the session is
    /// missing, or inference fails.
    pub fn probability(&self, samples_16k: &[f32]) -> Result<f32, String> {
        #[cfg(feature = "onnx")]
        {
            self.endpoint_prob(samples_16k)
        }
        #[cfg(not(feature = "onnx"))]
        {
            let _ = samples_16k;
            Err("built without the `onnx` feature".into())
        }
    }

    #[cfg(feature = "onnx")]
    fn try_load_session(model_path: &Path) -> Option<Arc<Mutex<ort::session::Session>>> {
        if !model_path.exists() {
            tracing::debug!(
                "smart-turn ONNX model not found at {}",
                model_path.display()
            );
            return None;
        }
        match ort::session::Session::builder().and_then(|mut b| b.commit_from_file(model_path)) {
            Ok(session) => {
                tracing::info!(
                    "smart-turn ONNX session loaded from {}",
                    model_path.display()
                );
                Some(Arc::new(Mutex::new(session)))
            }
            Err(e) => {
                tracing::warn!("failed to load smart-turn ONNX session: {e}");
                None
            }
        }
    }

    /// Run the Whisper front-end + ONNX graph for 16 kHz mono PCM, returning the
    /// completion probability the graph emits (already sigmoid-activated).
    #[cfg(feature = "onnx")]
    fn endpoint_prob(&self, samples_16k: &[f32]) -> Result<f32, String> {
        use ort::value::Tensor;

        let session = self.session.as_ref().ok_or("session unavailable")?;
        let feats = self.mel.input_features(samples_16k);
        let shape = vec![1_i64, N_MELS as i64, N_FRAMES as i64];
        let feats_t =
            Tensor::from_array((shape, feats)).map_err(|e| format!("input tensor: {e}"))?;
        let inputs = ort::inputs!["input_features" => feats_t];

        let mut guard = session.lock().map_err(|_| "session mutex poisoned")?;
        let outputs = guard.run(inputs).map_err(|e| format!("inference: {e}"))?;
        let output = match outputs.get("logits") {
            Some(v) => v,
            None if outputs.len() > 0 => &outputs[0],
            None => return Err("no output tensor".into()),
        };
        let (_, data) = output
            .try_extract_tensor::<f32>()
            .map_err(|e| format!("extract: {e}"))?;
        let p = *data.first().ok_or("empty output")?;
        Ok(p.clamp(0.0, 1.0))
    }
}

impl Default for SmartTurnEndpoint {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl EndpointModel for SmartTurnEndpoint {
    async fn completion_prob(&self, recent_audio: &[i16], partial_text: &str) -> f32 {
        #[cfg(feature = "onnx")]
        {
            if self.runtime_available {
                let mono: Vec<f32> = recent_audio.iter().map(|&s| s as f32 / 32_768.0).collect();
                match self.endpoint_prob(&mono) {
                    Ok(p) => return p,
                    Err(e) => tracing::warn!("smart-turn inference failed ({e}); using heuristic"),
                }
            }
        }
        self.fallback
            .completion_prob(recent_audio, partial_text)
            .await
    }

    /// `smart-turn-v3` when the ONNX runtime is live, else the heuristic path
    /// this model degrades to — matching what `completion_prob` actually used.
    fn source(&self) -> &'static str {
        if self.runtime_available {
            "smart-turn-v3"
        } else {
            "heuristic"
        }
    }
}

/// Resolve the model path: env override → cwd `.weftos` → `$HOME/.weftos`.
fn default_model_path() -> PathBuf {
    if let Ok(p) = std::env::var(MODEL_ENV) {
        return PathBuf::from(p);
    }
    let rel = PathBuf::from(".weftos/models/smart-turn").join(MODEL_FILE);
    if rel.exists() {
        return rel;
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home)
            .join(".weftos/models/smart-turn")
            .join(MODEL_FILE);
    }
    rel
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn construction_never_requires_model() {
        let ep = SmartTurnEndpoint::with_model("/nonexistent/smart-turn.onnx");
        assert!(!ep.is_runtime_available());
        assert_eq!(ep.model_path(), Path::new("/nonexistent/smart-turn.onnx"));
    }

    #[tokio::test]
    async fn falls_back_to_heuristic_without_model() {
        let ep = SmartTurnEndpoint::with_model("/nonexistent/smart-turn.onnx");
        // The heuristic reads the trailing token: terminal punctuation → done,
        // open word → not done. This proves the fallback path is wired.
        let done = ep.completion_prob(&[0i16; 1_600], "what time is it?").await;
        let open = ep.completion_prob(&[0i16; 1_600], "tell me about").await;
        assert!(done > 0.9, "fallback should read complete sentence as done");
        assert!(open < 0.3, "fallback should read open word as not done");
    }

    #[test]
    fn default_path_honors_env() {
        // SAFETY: single-threaded test; restore afterwards.
        unsafe { std::env::set_var(MODEL_ENV, "/tmp/custom-smart-turn.onnx") };
        assert_eq!(
            default_model_path(),
            PathBuf::from("/tmp/custom-smart-turn.onnx")
        );
        unsafe { std::env::remove_var(MODEL_ENV) };
    }
}
