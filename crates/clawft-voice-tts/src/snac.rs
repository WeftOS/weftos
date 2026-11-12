//! SNAC-24kHz neural audio-codec decoder (ADR-062 Phase 4.1) — `custom_token`
//! SNAC codes → PCM, native Rust via ONNX Runtime.
//!
//! The Orpheus slow layer emits its audio as a flat stream of SNAC codes, **7
//! per frame** (the Orpheus token layout). SNAC's decoder is *hierarchical*:
//! those 7 codes split across three codebooks of increasing temporal
//! resolution — 1 coarse code, 2 mid codes, 4 fine codes per frame
//! (`hubertsiuzdak/snac`). [`regroup_frames`] performs that split (the pure,
//! model-free "frame math" — unit-tested here); [`SnacOnnxDecoder`] feeds the
//! three codebooks to the SNAC ONNX graph and converts the 24 kHz float audio
//! to `s16le` PCM.
//!
//! [`SnacDecode`] is the seam so the slow engine ([`crate::orpheus::OrpheusTts`])
//! is testable without the model: a fake decoder drives the token→chunk path.
//! Inference mirrors the `ort` idiom in `clawft-voice-onnx` (session build,
//! tensor IO, model-absent graceful fallback).

#[cfg(feature = "onnx")]
use std::path::Path;
use std::path::PathBuf;
#[cfg(feature = "onnx")]
use std::sync::{Arc, Mutex};

use clawft_channels::voice::types::VoiceError;

/// SNAC codes per Orpheus frame (1 coarse + 2 mid + 4 fine).
pub const FRAME_TOKENS: usize = 7;
/// SNAC codebook size; every valid code is in `0..SNAC_CODEBOOK_SIZE`.
pub const SNAC_CODEBOOK_SIZE: i64 = 4096;
/// SNAC-24kHz output sample rate.
pub const SNAC_SAMPLE_RATE: u32 = 24_000;

/// Default model filename under a `.weftos/models/snac/` directory.
const MODEL_FILE: &str = "snac_24khz.onnx";
/// Environment override for the model path.
const MODEL_ENV: &str = "WEFTOS_SNAC_MODEL";

/// A SNAC decoder: a flat, frame-ordered code stream → 24 kHz mono PCM. The
/// seam the Orpheus slow layer renders through (so the token→chunk path is
/// testable without the model).
pub trait SnacDecode: Send + Sync {
    /// Decode `flat_codes` (length a multiple of [`FRAME_TOKENS`], Orpheus
    /// layout) into `s16le` PCM at [`Self::sample_rate`].
    fn decode_frames(&self, flat_codes: &[i64]) -> Result<Vec<i16>, VoiceError>;

    /// Sample rate of the produced PCM (24 kHz for SNAC-24kHz).
    fn sample_rate(&self) -> u32 {
        SNAC_SAMPLE_RATE
    }
}

/// Split a flat, frame-ordered SNAC code stream into the three hierarchical
/// codebooks SNAC's decoder expects. For each 7-code frame `[c0..c6]`:
///
/// - codebook 0 (coarse): `c0`
/// - codebook 1 (mid):    `c1, c4`
/// - codebook 2 (fine):   `c2, c3, c5, c6`
///
/// (`canopylabs/orpheus-tts` `convert_to_audio` / `hubertsiuzdak/snac`.) Excess
/// codes that do not complete a frame are ignored.
pub fn regroup_frames(flat_codes: &[i64]) -> (Vec<i64>, Vec<i64>, Vec<i64>) {
    let frames = flat_codes.len() / FRAME_TOKENS;
    let mut c0 = Vec::with_capacity(frames);
    let mut c1 = Vec::with_capacity(frames * 2);
    let mut c2 = Vec::with_capacity(frames * 4);
    for f in 0..frames {
        let i = f * FRAME_TOKENS;
        c0.push(flat_codes[i]);
        c1.push(flat_codes[i + 1]);
        c1.push(flat_codes[i + 4]);
        c2.push(flat_codes[i + 2]);
        c2.push(flat_codes[i + 3]);
        c2.push(flat_codes[i + 5]);
        c2.push(flat_codes[i + 6]);
    }
    (c0, c1, c2)
}

/// Native SNAC-24kHz decoder. Build with [`SnacOnnxDecoder::new`] (auto model
/// discovery) or [`SnacOnnxDecoder::with_model`]. Degrades gracefully: a missing
/// file or absent `onnx` feature leaves the runtime unavailable and
/// [`decode_frames`](SnacDecode::decode_frames) returns a clear `VoiceError`.
pub struct SnacOnnxDecoder {
    model_path: PathBuf,
    runtime_available: bool,
    #[cfg(feature = "onnx")]
    session: Option<Arc<Mutex<ort::session::Session>>>,
}

impl SnacOnnxDecoder {
    /// Build with auto model discovery: `$WEFTOS_SNAC_MODEL`, then
    /// `./.weftos/models/snac/snac_24khz.onnx`, then the same under `$HOME`.
    pub fn new() -> Self {
        Self::with_model(default_model_path())
    }

    /// Build pointing at a specific model file. Never fails; an absent file or
    /// `onnx` feature simply routes through the model-absent error path.
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
            #[cfg(feature = "onnx")]
            session,
        }
    }

    /// Whether real ONNX inference is active (vs. the model-absent error path).
    pub fn is_runtime_available(&self) -> bool {
        self.runtime_available
    }

    /// The resolved model path (may not exist).
    pub fn model_path(&self) -> &std::path::Path {
        &self.model_path
    }

    #[cfg(feature = "onnx")]
    fn try_load_session(model_path: &Path) -> Option<Arc<Mutex<ort::session::Session>>> {
        if !model_path.exists() {
            tracing::debug!("SNAC ONNX model not found at {}", model_path.display());
            return None;
        }
        match crate::ort_env().and_then(ort::session::Session::builder).and_then(|mut b| b.commit_from_file(model_path)) {
            Ok(session) => {
                tracing::info!("SNAC ONNX session loaded from {}", model_path.display());
                Some(Arc::new(Mutex::new(session)))
            }
            Err(e) => {
                tracing::warn!("failed to load SNAC ONNX session: {e}");
                None
            }
        }
    }

    /// Regroup → ONNX → 24 kHz `s16le` PCM. Inputs are bound *positionally* to
    /// the model's three code inputs (codebook 0/1/2 order), so it is robust to
    /// the export's input names. Codes are clamped to the valid codebook range
    /// so a stray/misaligned token can never index out of the embedding table.
    #[cfg(feature = "onnx")]
    fn run(&self, flat_codes: &[i64]) -> Result<Vec<i16>, VoiceError> {
        use ort::value::Tensor;

        let session = self
            .session
            .as_ref()
            .ok_or_else(|| model_absent_error(&self.model_path))?;

        let (c0, c1, c2) = regroup_frames(flat_codes);
        if c0.is_empty() {
            return Ok(Vec::new());
        }
        let clamp = |v: Vec<i64>| {
            v.into_iter()
                .map(|x| x.clamp(0, SNAC_CODEBOOK_SIZE - 1))
                .collect::<Vec<i64>>()
        };
        let (c0, c1, c2) = (clamp(c0), clamp(c1), clamp(c2));

        let t0 = Tensor::from_array((vec![1_i64, c0.len() as i64], c0))
            .map_err(|e| VoiceError::Audio(format!("SNAC code0 tensor: {e}")))?;
        let t1 = Tensor::from_array((vec![1_i64, c1.len() as i64], c1))
            .map_err(|e| VoiceError::Audio(format!("SNAC code1 tensor: {e}")))?;
        let t2 = Tensor::from_array((vec![1_i64, c2.len() as i64], c2))
            .map_err(|e| VoiceError::Audio(format!("SNAC code2 tensor: {e}")))?;

        let mut guard = session
            .lock()
            .map_err(|_| VoiceError::Audio("SNAC session mutex poisoned".into()))?;
        let outputs = guard
            .run(ort::inputs![t0, t1, t2])
            .map_err(|e| VoiceError::Audio(format!("SNAC inference: {e}")))?;

        let output = outputs
            .iter()
            .next()
            .map(|(_, v)| v)
            .ok_or_else(|| VoiceError::Audio("SNAC: no output tensor".into()))?;
        let (_, data) = output
            .try_extract_tensor::<f32>()
            .map_err(|e| VoiceError::Audio(format!("SNAC extract: {e}")))?;
        Ok(data
            .iter()
            .map(|&x| (x.clamp(-1.0, 1.0) * 32_767.0).round() as i16)
            .collect())
    }
}

impl Default for SnacOnnxDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl SnacDecode for SnacOnnxDecoder {
    fn decode_frames(&self, flat_codes: &[i64]) -> Result<Vec<i16>, VoiceError> {
        #[cfg(feature = "onnx")]
        {
            if self.runtime_available {
                return self.run(flat_codes);
            }
        }
        #[cfg(not(feature = "onnx"))]
        let _ = flat_codes;
        Err(model_absent_error(&self.model_path))
    }

    fn sample_rate(&self) -> u32 {
        SNAC_SAMPLE_RATE
    }
}

/// Resolve the model path: env override → cwd `.weftos` → `$HOME/.weftos`.
fn default_model_path() -> PathBuf {
    if let Ok(p) = std::env::var(MODEL_ENV) {
        return PathBuf::from(p);
    }
    let rel = PathBuf::from(".weftos/models/snac").join(MODEL_FILE);
    if rel.exists() {
        return rel;
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home)
            .join(".weftos/models/snac")
            .join(MODEL_FILE);
    }
    rel
}

/// Uniform "model unavailable" error (feature off or file missing).
fn model_absent_error(path: &std::path::Path) -> VoiceError {
    VoiceError::Audio(format!(
        "SNAC ONNX model unavailable (looked for {}); build with --features onnx \
         and place the SNAC-24kHz export there or set {MODEL_ENV}",
        path.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regroup_splits_one_frame_into_three_codebooks() {
        // One frame [c0..c6] → coarse [c0], mid [c1,c4], fine [c2,c3,c5,c6].
        let (c0, c1, c2) = regroup_frames(&[10, 11, 12, 13, 14, 15, 16]);
        assert_eq!(c0, vec![10]);
        assert_eq!(c1, vec![11, 14]);
        assert_eq!(c2, vec![12, 13, 15, 16]);
    }

    #[test]
    fn regroup_handles_multiple_frames_and_drops_partial() {
        // 2 full frames + a 3-code partial tail (ignored).
        let mut flat: Vec<i64> = (0..14).collect();
        flat.extend([99, 99, 99]);
        let (c0, c1, c2) = regroup_frames(&flat);
        assert_eq!(c0.len(), 2, "one coarse code per frame");
        assert_eq!(c1.len(), 4, "two mid codes per frame");
        assert_eq!(c2.len(), 8, "four fine codes per frame");
        // Frame 1 starts at index 7: coarse=7, mid=[8,11], fine=[9,10,12,13].
        assert_eq!(c0, vec![0, 7]);
        assert_eq!(c1, vec![1, 4, 8, 11]);
        assert_eq!(c2, vec![2, 3, 5, 6, 9, 10, 12, 13]);
    }

    #[test]
    fn empty_input_yields_empty_codebooks() {
        let (c0, c1, c2) = regroup_frames(&[]);
        assert!(c0.is_empty() && c1.is_empty() && c2.is_empty());
    }

    #[test]
    fn construction_never_requires_model() {
        let d = SnacOnnxDecoder::with_model("/nonexistent/snac.onnx");
        assert!(!d.is_runtime_available());
        assert_eq!(d.sample_rate(), SNAC_SAMPLE_RATE);
        assert_eq!(
            d.model_path(),
            std::path::Path::new("/nonexistent/snac.onnx")
        );
    }

    #[test]
    fn decode_without_model_errors_gracefully() {
        let d = SnacOnnxDecoder::with_model("/nonexistent/snac.onnx");
        let err = d.decode_frames(&[0; FRAME_TOKENS]).unwrap_err();
        assert!(matches!(err, VoiceError::Audio(_)));
        assert!(err.to_string().contains("SNAC"));
    }

    #[test]
    fn default_path_honors_env() {
        // SAFETY: single-threaded test; restore afterwards.
        unsafe { std::env::set_var(MODEL_ENV, "/tmp/custom-snac.onnx") };
        assert_eq!(default_model_path(), PathBuf::from("/tmp/custom-snac.onnx"));
        unsafe { std::env::remove_var(MODEL_ENV) };
    }
}
