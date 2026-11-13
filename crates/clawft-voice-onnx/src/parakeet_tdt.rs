//! parakeet-tdt ONNX inference engine: the encoder/decoder/joiner sessions plus
//! the Token-and-Duration Transducer (TDT) greedy decode. Split from
//! [`crate::parakeet`] (which owns discovery, the [`crate::nemo_mel`] front-end,
//! and the public [`clawft_channels::voice::stt::SttBackend`] surface) to keep
//! each file focused. Compiled only under the `onnx` feature.
//!
//! The decode mirrors sherpa-onnx's
//! `OfflineTransducerGreedySearchNeMoDecoder::DecodeOneTDT`: prime the
//! prediction net with `<blk>` + zero LSTM state, then per encoder frame run the
//! joiner (emitting `[token logits.. | duration logits..]`), append the argmax
//! token unless it is blank, and advance time by the argmax duration. A
//! `max_tokens_per_frame` cap and a blank/zero-skip guard prevent stalls.

use std::path::Path;
use std::sync::{Arc, Mutex};

use clawft_channels::voice::types::VoiceError;
use ort::session::{Session, SessionOutputs};
use ort::value::Tensor;

/// Encoder output (FastConformer) embedding dimension.
const ENCODER_DIM: usize = 1024;
/// Prediction-network hidden size (`pred_hidden`) and layer count
/// (`pred_rnn_layers`) — pinned from the model metadata.
const PRED_HIDDEN: usize = 640;
const PRED_LAYERS: usize = 2;
/// Safety bound mirroring sherpa's `max_tokens_per_frame`.
const MAX_TOKENS_PER_FRAME: usize = 5;

/// Prediction-net step output: `(decoder_out[640], h', c')`.
type DecoderStep = (Vec<f32>, Vec<f32>, Vec<f32>);

/// FastConformer subsampling factor: the encoder emits one frame per 8 mel
/// frames, so an encoder frame index `t` maps to `t · HOP · 8` samples. Pinned
/// from the parakeet-tdt-0.6b model config (`subsampling_factor: 8`).
pub(crate) const SUBSAMPLING_FACTOR: usize = 8;

/// One emitted token with its encoder-frame timing and confidence — the data
/// the TDT decode already computes but historically discarded. Frame units
/// (not ms); the caller converts via [`SUBSAMPLING_FACTOR`] and the mel hop.
pub(crate) struct DecodedToken {
    /// Token id (index into `tokens`).
    pub id: usize,
    /// Encoder frame index at emission (onset).
    pub t_frame: usize,
    /// Frames advanced by the TDT duration head after this token (≥ 1).
    pub dur_frames: usize,
    /// Softmax confidence of the emitted token over the token logits.
    pub conf: f32,
}

/// Loaded encoder/decoder/joiner sessions + the token table. Constructed only
/// when every artifact is present (see [`TdtEngine::load`]).
pub(crate) struct TdtEngine {
    encoder: Arc<Mutex<Session>>,
    decoder: Arc<Mutex<Session>>,
    joiner: Arc<Mutex<Session>>,
    /// `id → token`; the last entry is `<blk>`.
    tokens: Vec<String>,
}

impl TdtEngine {
    /// Load the three ONNX sessions from a bundle directory. Returns `None` (so
    /// the backend degrades gracefully) if any file or the token table is
    /// missing/unloadable.
    pub(crate) fn load(dir: &Path, tokens: Vec<String>) -> Option<Self> {
        if tokens.is_empty() {
            return None;
        }
        Some(Self {
            encoder: try_load(&dir.join("encoder.int8.onnx"))?,
            decoder: try_load(&dir.join("decoder.int8.onnx"))?,
            joiner: try_load(&dir.join("joiner.int8.onnx"))?,
            tokens,
        })
    }

    /// Encoder → TDT greedy decode → text for one utterance's `[N_MELS, frames]`
    /// (mel-major) feature buffer.
    pub(crate) fn decode(&self, feats: &[f32], frames: usize) -> Result<String, VoiceError> {
        Ok(self.decode_detailed(feats, frames)?.0)
    }

    /// [`decode`](Self::decode) plus the per-token detail the decode already
    /// computes (onset frame, duration frames, softmax confidence). The text
    /// is identical to [`decode`]; the tokens are the enrichment §W1.3 surfaces
    /// for the `VoiceAnalysis` record on the native path.
    pub(crate) fn decode_detailed(
        &self,
        feats: &[f32],
        frames: usize,
    ) -> Result<(String, Vec<DecodedToken>), VoiceError> {
        let (enc, t_out) = self.run_encoder(feats, frames)?;
        let tokens = self.greedy_tdt(&enc, t_out)?;
        let ids: Vec<usize> = tokens.iter().map(|t| t.id).collect();
        Ok((self.ids_to_text(&ids), tokens))
    }

    /// Blank token id (`<blk>`, the last entry in `tokens.txt`).
    fn blank_id(&self) -> usize {
        self.tokens.len() - 1
    }

    /// Encoder: `audio_signal [1, 128, T]` + `length [1]` → frame-major
    /// `[T_out, ENCODER_DIM]` (transposed from the `[1, 1024, T_out]` output).
    fn run_encoder(&self, feats: &[f32], frames: usize) -> Result<(Vec<f32>, usize), VoiceError> {
        let sig = Tensor::from_array((
            vec![1_i64, crate::nemo_mel::N_MELS as i64, frames as i64],
            feats.to_vec(),
        ))
        .map_err(|e| VoiceError::Audio(format!("parakeet audio_signal: {e}")))?;
        let len = Tensor::from_array((vec![1_i64], vec![frames as i64]))
            .map_err(|e| VoiceError::Audio(format!("parakeet length: {e}")))?;

        let mut guard = self.encoder.lock().map_err(|_| poisoned())?;
        let outputs = guard
            .run(ort::inputs!["audio_signal" => sig, "length" => len])
            .map_err(|e| VoiceError::Audio(format!("parakeet encoder: {e}")))?;
        let out = outputs
            .get("outputs")
            .ok_or_else(|| VoiceError::Audio("parakeet: no encoder output".into()))?;
        let (shape, data) = out
            .try_extract_tensor::<f32>()
            .map_err(|e| VoiceError::Audio(format!("parakeet encoder extract: {e}")))?;
        // shape = [1, ENCODER_DIM, T_out]; transpose to [T_out, ENCODER_DIM].
        let dim = shape[1] as usize;
        let t_out = shape[2] as usize;
        let mut enc = vec![0.0f32; t_out * dim];
        for d in 0..dim {
            for t in 0..t_out {
                enc[t * dim + d] = data[d * t_out + t];
            }
        }
        Ok((enc, t_out))
    }

    /// TDT greedy search (`DecodeOneTDT`).
    ///
    /// Returns the emitted tokens with their onset frame, advance (duration)
    /// frames, and softmax confidence — all computed inline at ~zero extra
    /// cost. The token *sequence* and the decode's time advance are byte-for-
    /// byte what the text-only path produced; the record just captures the
    /// intermediates it used to throw away.
    fn greedy_tdt(&self, enc: &[f32], t_out: usize) -> Result<Vec<DecodedToken>, VoiceError> {
        let blank = self.blank_id();
        let n_tokens = self.tokens.len(); // token logits incl. blank

        // Prime the decoder with the blank id and zero state.
        let (mut h, mut c) = init_states();
        let (mut dec_out, nh, nc) = self.run_decoder(blank, &h, &c)?;
        h = nh;
        c = nc;

        let mut out: Vec<DecodedToken> = Vec::new();
        let mut t: usize = 0;
        let mut tokens_this_frame = 0usize;
        while t < t_out {
            let frame = &enc[t * ENCODER_DIM..(t + 1) * ENCODER_DIM];
            let logits = self.run_joiner(frame, &dec_out)?;
            let y = argmax(&logits[..n_tokens]);
            let mut skip = argmax(&logits[n_tokens..]);

            // Confidence of the emitted token (only meaningful for a non-blank).
            let conf = if y != blank {
                softmax_at(&logits[..n_tokens], y)
            } else {
                0.0
            };

            if y != blank {
                let (d, nh, nc) = self.run_decoder(y, &h, &c)?;
                dec_out = d;
                h = nh;
                c = nc;
                tokens_this_frame += 1;
            }
            if skip > 0 {
                tokens_this_frame = 0;
            }
            if tokens_this_frame >= MAX_TOKENS_PER_FRAME {
                tokens_this_frame = 0;
                skip = 1;
            }
            if y == blank && skip == 0 {
                tokens_this_frame = 0;
                skip = 1;
            }
            // Record the token once the final advance for this step is known.
            if y != blank {
                out.push(DecodedToken {
                    id: y,
                    t_frame: t,
                    dur_frames: skip.max(1),
                    conf,
                });
            }
            t += skip;
        }
        Ok(out)
    }

    /// Decoder (prediction net): token + LSTM `(h, c)` → `(decoder_out[640],
    /// h', c')`.
    fn run_decoder(&self, token: usize, h: &[f32], c: &[f32]) -> Result<DecoderStep, VoiceError> {
        let state_shape = vec![PRED_LAYERS as i64, 1, PRED_HIDDEN as i64];
        let targets = Tensor::from_array((vec![1_i64, 1], vec![token as i32]))
            .map_err(|e| VoiceError::Audio(format!("parakeet targets: {e}")))?;
        let target_len = Tensor::from_array((vec![1_i64], vec![1_i32]))
            .map_err(|e| VoiceError::Audio(format!("parakeet target_length: {e}")))?;
        let h_t = Tensor::from_array((state_shape.clone(), h.to_vec()))
            .map_err(|e| VoiceError::Audio(format!("parakeet state h: {e}")))?;
        let c_t = Tensor::from_array((state_shape, c.to_vec()))
            .map_err(|e| VoiceError::Audio(format!("parakeet state c: {e}")))?;

        let mut guard = self.decoder.lock().map_err(|_| poisoned())?;
        let outputs = guard
            .run(ort::inputs![
                "targets" => targets,
                "target_length" => target_len,
                "states.1" => h_t,
                "onnx::Slice_3" => c_t,
            ])
            .map_err(|e| VoiceError::Audio(format!("parakeet decoder: {e}")))?;

        let dec = extract(&outputs, "outputs")?;
        let new_h = extract(&outputs, "states")?;
        let new_c = extract(&outputs, "162")?;
        Ok((dec, new_h, new_c))
    }

    /// Joiner: `encoder_out[1024]` + `decoder_out[640]` → joint logits
    /// (`[tokens.. | durations..]`).
    fn run_joiner(&self, enc_frame: &[f32], dec_out: &[f32]) -> Result<Vec<f32>, VoiceError> {
        let enc_t = Tensor::from_array((vec![1_i64, ENCODER_DIM as i64, 1], enc_frame.to_vec()))
            .map_err(|e| VoiceError::Audio(format!("parakeet enc joiner in: {e}")))?;
        let dec_t = Tensor::from_array((vec![1_i64, PRED_HIDDEN as i64, 1], dec_out.to_vec()))
            .map_err(|e| VoiceError::Audio(format!("parakeet dec joiner in: {e}")))?;

        let mut guard = self.joiner.lock().map_err(|_| poisoned())?;
        let outputs = guard
            .run(ort::inputs![
                "encoder_outputs" => enc_t,
                "decoder_outputs" => dec_t,
            ])
            .map_err(|e| VoiceError::Audio(format!("parakeet joiner: {e}")))?;
        extract(&outputs, "outputs")
    }

    /// One token id → its surface text: the `tokens.txt` symbol with the
    /// SentencePiece `▁` word-start turned into a leading space, or empty for a
    /// special (`<blk>`, `<unk>`, …). Used to label per-token `VoiceAnalysis`
    /// entries; the whole-utterance join stays in [`ids_to_text`].
    pub(crate) fn token_text(&self, id: usize) -> String {
        match self.tokens.get(id) {
            Some(tok) if tok.starts_with('<') && tok.ends_with('>') => String::new(),
            Some(tok) => tok.replace('\u{2581}', " "),
            None => String::new(),
        }
    }

    /// Map token ids → text: join `tokens.txt` symbols, turning SentencePiece
    /// `▁` word-starts into spaces, dropping specials.
    fn ids_to_text(&self, ids: &[usize]) -> String {
        let mut s = String::new();
        for &id in ids {
            let Some(tok) = self.tokens.get(id) else {
                continue;
            };
            if tok.starts_with('<') && tok.ends_with('>') {
                continue; // <unk>, <blk>, etc.
            }
            s.push_str(&tok.replace('\u{2581}', " "));
        }
        s.trim().to_string()
    }
}

/// Zero LSTM `(h, c)` init states, each `[PRED_LAYERS, 1, PRED_HIDDEN]`.
fn init_states() -> (Vec<f32>, Vec<f32>) {
    let n = PRED_LAYERS * PRED_HIDDEN;
    (vec![0.0f32; n], vec![0.0f32; n])
}

/// Argmax over a logit slice (first index on ties).
fn argmax(v: &[f32]) -> usize {
    let mut best = 0;
    let mut best_v = f32::MIN;
    for (i, &x) in v.iter().enumerate() {
        if x > best_v {
            best_v = x;
            best = i;
        }
    }
    best
}

/// Numerically-stable softmax probability of `logits[idx]` over the slice —
/// the emitted token's confidence. `idx` is assumed the argmax, so the result
/// is the largest probability in `[0, 1]`.
fn softmax_at(logits: &[f32], idx: usize) -> f32 {
    if logits.is_empty() {
        return 0.0;
    }
    let max = logits.iter().copied().fold(f32::MIN, f32::max);
    let sum: f32 = logits.iter().map(|&x| (x - max).exp()).sum();
    if sum <= 0.0 {
        return 0.0;
    }
    ((logits[idx] - max).exp() / sum).clamp(0.0, 1.0)
}

/// Extract a named float output tensor as an owned `Vec<f32>`.
fn extract(outputs: &SessionOutputs, name: &str) -> Result<Vec<f32>, VoiceError> {
    let v = outputs
        .get(name)
        .ok_or_else(|| VoiceError::Audio(format!("parakeet: missing output `{name}`")))?;
    let (_, data) = v
        .try_extract_tensor::<f32>()
        .map_err(|e| VoiceError::Audio(format!("parakeet extract `{name}`: {e}")))?;
    Ok(data.to_vec())
}

fn try_load(path: &Path) -> Option<Arc<Mutex<Session>>> {
    if !path.exists() {
        tracing::debug!("parakeet ONNX not found at {}", path.display());
        return None;
    }
    match crate::ort_env().and_then(Session::builder).and_then(|mut b| b.commit_from_file(path)) {
        Ok(s) => Some(Arc::new(Mutex::new(s))),
        Err(e) => {
            tracing::warn!("failed to load parakeet ONNX {}: {e}", path.display());
            None
        }
    }
}

fn poisoned() -> VoiceError {
    VoiceError::Audio("parakeet session mutex poisoned".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn softmax_at_is_a_probability() {
        // A clearly-dominant logit yields high confidence; the softmax over the
        // slice sums to 1 so the peak sits in (0, 1].
        let logits = [0.1, 4.0, 0.2, -1.0];
        let p = softmax_at(&logits, 1);
        assert!(p > 0.9, "dominant logit should be confident, got {p}");
        assert!(p <= 1.0);
    }

    #[test]
    fn softmax_at_flat_is_uniform() {
        // Equal logits → uniform 1/N confidence.
        let logits = [1.0, 1.0, 1.0, 1.0];
        let p = softmax_at(&logits, 2);
        assert!((p - 0.25).abs() < 1e-5, "uniform should be 1/N, got {p}");
    }

    #[test]
    fn softmax_at_empty_is_zero() {
        assert_eq!(softmax_at(&[], 0), 0.0);
    }

    #[test]
    fn subsampling_factor_pinned() {
        // 8× subsampling over a 10 ms mel hop → 80 ms per encoder frame; the
        // record's t_ms/dur_ms conversion rides on this.
        assert_eq!(SUBSAMPLING_FACTOR, 8);
    }
}
