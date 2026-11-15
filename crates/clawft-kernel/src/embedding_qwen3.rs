//! Qwen3-Embedding-0.6B provider (ADR-059) — the preferred ECC embedder.
//!
//! Unlike the BERT [`OnnxEmbeddingProvider`](crate::embedding_onnx::OnnxEmbeddingProvider),
//! Qwen3-Embedding is a **causal decoder-with-KV-cache** ONNX export. This
//! provider therefore:
//!
//! - feeds `input_ids` / `attention_mask` / `position_ids` (`0..seq_len`) **plus
//!   empty, dtype-matched `past_key_values.*` tensors** for every layer (shape
//!   `[1, n_kv_heads, 0, head_dim]`, derived from the graph — never hardcoded);
//! - pools the **last non-pad token** hidden state (not mean/CLS);
//! - applies **MRL**: slice to the first [`QWEN3_DIMS`] dims, *then* L2-renormalize;
//! - uses the HF `tokenizers` crate (Qwen3 BPE from `tokenizer.json`);
//! - exposes [`embed_query`](Qwen3EmbeddingProvider::embed_query) for the
//!   asymmetric query lane (prepends the Qwen3 instruction prefix); the
//!   [`EmbeddingProvider::embed`] trait method is document mode.
//!
//! Without the `onnx-embeddings` feature, the model file, or the tokenizer, the
//! provider is unavailable: construction still succeeds (builds and tests never
//! need the ~1.2 GB artifact) but every embed call returns
//! `EmbeddingError::ModelUnavailable` naming the missing path. There is no hash
//! fallback. The BERT provider stays as a lower-priority real model (see
//! [`select_embedding_provider`](crate::embedding::select_embedding_provider)).

use std::path::{Path, PathBuf};
#[cfg(feature = "onnx-embeddings")]
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::embedding::{EmbeddingError, EmbeddingProvider};

/// MRL target dimensionality — the empirical sweet spot (ADR-059, recall@10 0.983).
pub const QWEN3_DIMS: usize = 512;

/// Canonical model identifier (pins the consistency contract; ADR-059).
pub const QWEN3_MODEL_NAME: &str = "Qwen3-Embedding-0.6B";

/// Qwen3 asymmetric **query** instruction prefix. Documents are embedded raw;
/// queries are prefixed with this (`Instruct: {task}\nQuery: {q}` format). The
/// task description is the code-search instruction pinned by the lab probe set
/// (ADR-059 consistency contract) so the lab and prod share one vector space;
/// `embed_query` appends the raw query directly after the trailing space.
pub const QUERY_INSTRUCTION: &str = "Instruct: Given a code-search query, retrieve the relevant code, log, diff, traceback or config chunk.\nQuery: ";

// ---------------------------------------------------------------------------
// Pure helpers (always compiled + unit-tested; only wired into production
// inference under `onnx-embeddings`, hence the conditional dead-code allow).
// ---------------------------------------------------------------------------

/// In-place L2 normalisation.
#[cfg_attr(not(feature = "onnx-embeddings"), allow(dead_code))]
fn l2_normalize(vec: &mut [f32]) {
    let norm: f32 = vec.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        vec.iter_mut().for_each(|x| *x /= norm);
    }
}

/// Last-token pooling: return the hidden state at the **last non-pad position**.
///
/// `hidden` is the flattened `(seq, hidden_dim)` last-hidden-state for a single
/// sequence (batch 1). The last attended position is located via
/// `attention_mask`; if the mask is empty/all-zero the final row is used.
#[cfg_attr(not(feature = "onnx-embeddings"), allow(dead_code))]
fn last_token_pool(
    hidden: &[f32],
    seq: usize,
    hidden_dim: usize,
    attention_mask: &[i64],
) -> Vec<f32> {
    let last = attention_mask
        .iter()
        .take(seq)
        .rposition(|&m| m != 0)
        .unwrap_or(seq.saturating_sub(1));
    let start = last * hidden_dim;
    let end = start + hidden_dim;
    if end <= hidden.len() {
        hidden[start..end].to_vec()
    } else {
        // Defensive: shape mismatch — fall back to a zero vector.
        vec![0.0; hidden_dim]
    }
}

/// MRL: slice to the first `dims` dimensions **then** L2-renormalize (never the
/// reverse). Shorter inputs are zero-padded so the output is always `dims` long.
#[cfg_attr(not(feature = "onnx-embeddings"), allow(dead_code))]
fn mrl_slice_l2norm(full: &[f32], dims: usize) -> Vec<f32> {
    let mut v: Vec<f32> = full.iter().take(dims).copied().collect();
    if v.len() < dims {
        v.resize(dims, 0.0);
    }
    l2_normalize(&mut v);
    v
}

// ---------------------------------------------------------------------------
// KV-cache spec (derived from the ONNX graph, never hardcoded)
// ---------------------------------------------------------------------------

/// One `past_key_values.*` graph input: its name and the concrete head geometry.
#[cfg(feature = "onnx-embeddings")]
#[derive(Debug, Clone)]
struct KvInput {
    name: String,
    n_kv_heads: i64,
    head_dim: i64,
    fp16: bool,
}

// ---------------------------------------------------------------------------
// Qwen3EmbeddingProvider
// ---------------------------------------------------------------------------

/// Qwen3-Embedding-0.6B provider — decoder-ONNX, last-token pooling, MRL-512.
pub struct Qwen3EmbeddingProvider {
    /// Path to `model_fp16.onnx`.
    model_path: PathBuf,
    /// Path to `tokenizer.json`.
    tokenizer_path: PathBuf,
    /// Output dimensionality (always [`QWEN3_DIMS`]).
    dimensions: usize,
    /// Model name (real name when live, `*-unavailable` otherwise).
    model_name: String,
    /// Whether real decoder-ONNX inference is active (model + tokenizer loaded).
    runtime_available: bool,
    /// Why the runtime is unavailable, naming the paths (None when live).
    unavailable_reason: Option<String>,
    /// Qwen3 BPE tokenizer loaded from `tokenizer.json`.
    #[cfg(feature = "onnx-embeddings")]
    tokenizer: Option<tokenizers::Tokenizer>,
    /// Decoder-ONNX session (`run` needs `&mut`, hence the mutex).
    #[cfg(feature = "onnx-embeddings")]
    session: Option<Arc<Mutex<ort::session::Session>>>,
    /// Empty-KV-cache spec derived from the graph inputs.
    #[cfg(feature = "onnx-embeddings")]
    kv_spec: Vec<KvInput>,
}

impl Qwen3EmbeddingProvider {
    /// Create a provider pointing at `model_fp16.onnx` + `tokenizer.json`.
    ///
    /// When the feature is off, files are missing, or loading fails the provider
    /// is unavailable (calls return `ModelUnavailable`) — never panics, never
    /// requires the model to construct.
    pub fn new(model_path: impl Into<PathBuf>, tokenizer_path: impl Into<PathBuf>) -> Self {
        let model_path = model_path.into();
        let tokenizer_path = tokenizer_path.into();

        #[cfg(feature = "onnx-embeddings")]
        let tokenizer = Self::try_load_tokenizer(&tokenizer_path);
        #[cfg(feature = "onnx-embeddings")]
        let session = Self::try_load_session(&model_path);
        #[cfg(feature = "onnx-embeddings")]
        let kv_spec = session
            .as_ref()
            .and_then(|s| s.lock().ok().map(|g| Self::derive_kv_spec(&g)))
            .unwrap_or_default();

        #[cfg(feature = "onnx-embeddings")]
        let runtime_available = tokenizer.is_some() && session.is_some();
        #[cfg(not(feature = "onnx-embeddings"))]
        let runtime_available = false;
        let unavailable_reason = if runtime_available {
            None
        } else {
            let mut why = Vec::new();
            if !cfg!(feature = "onnx-embeddings") {
                why.push("this build lacks the onnx-embeddings feature".to_string());
            }
            if !model_path.exists() {
                why.push(format!("model file not found at {}", model_path.display()));
            }
            if !tokenizer_path.exists() {
                why.push(format!("tokenizer not found at {}", tokenizer_path.display()));
            }
            if why.is_empty() {
                why.push(format!(
                    "model {} or tokenizer {} are present but failed to load (see the log)",
                    model_path.display(),
                    tokenizer_path.display()
                ));
            }
            Some(why.join("; "))
        };

        Self {
            model_name: if runtime_available {
                QWEN3_MODEL_NAME.to_string()
            } else {
                format!("{QWEN3_MODEL_NAME}-unavailable")
            },
            model_path,
            tokenizer_path,
            dimensions: QWEN3_DIMS,
            runtime_available,
            unavailable_reason,
            #[cfg(feature = "onnx-embeddings")]
            tokenizer,
            #[cfg(feature = "onnx-embeddings")]
            session,
            #[cfg(feature = "onnx-embeddings")]
            kv_spec,
        }
    }

    /// Whether real decoder-ONNX inference is active.
    pub fn is_runtime_available(&self) -> bool {
        self.runtime_available
    }

    /// Path to the configured ONNX model file.
    pub fn model_path(&self) -> &Path {
        &self.model_path
    }

    /// Path to the configured tokenizer file.
    pub fn tokenizer_path(&self) -> &Path {
        &self.tokenizer_path
    }

    fn unavailable_error(&self) -> EmbeddingError {
        EmbeddingError::ModelUnavailable {
            what: self.model_name.clone(),
            detail: self.unavailable_reason.clone().unwrap_or_else(|| {
                format!("model at {} is not loaded", self.model_path.display())
            }),
        }
    }

    /// Embed a **query** (asymmetric lane): prepends the Qwen3 instruction prefix.
    pub async fn embed_query(&self, query: &str) -> Result<Vec<f32>, EmbeddingError> {
        #[cfg(feature = "onnx-embeddings")]
        if self.runtime_available {
            let prompted = format!("{QUERY_INSTRUCTION}{query}");
            return self.qwen3_embed(&prompted);
        }
        let _ = query;
        Err(self.unavailable_error())
    }

    #[cfg(feature = "onnx-embeddings")]
    fn try_load_tokenizer(path: &Path) -> Option<tokenizers::Tokenizer> {
        if !path.exists() {
            tracing::debug!("Qwen3 tokenizer.json not found at {}", path.display());
            return None;
        }
        match tokenizers::Tokenizer::from_file(path) {
            Ok(t) => {
                tracing::info!("Qwen3 tokenizer loaded from {}", path.display());
                Some(t)
            }
            Err(e) => {
                tracing::warn!("Failed to load Qwen3 tokenizer: {e}");
                None
            }
        }
    }

    #[cfg(feature = "onnx-embeddings")]
    fn try_load_session(model_path: &Path) -> Option<Arc<Mutex<ort::session::Session>>> {
        if !model_path.exists() {
            tracing::debug!("Qwen3 ONNX model not found at {}", model_path.display());
            return None;
        }
        match crate::embedding_onnx::ort_env().and_then(ort::session::Session::builder)
            .and_then(|mut builder| builder.commit_from_file(model_path))
        {
            Ok(session) => {
                tracing::info!("Qwen3 ONNX session loaded from {}", model_path.display());
                Some(Arc::new(Mutex::new(session)))
            }
            Err(e) => {
                tracing::warn!("Failed to load Qwen3 ONNX session: {e}");
                None
            }
        }
    }

    /// Derive the empty-KV-cache spec from the graph's `past_key_values.*`
    /// inputs — layer count, kv-heads, head-dim, and dtype are all read from the
    /// model, never hardcoded.
    #[cfg(feature = "onnx-embeddings")]
    fn derive_kv_spec(session: &ort::session::Session) -> Vec<KvInput> {
        let mut out = Vec::new();
        for input in session.inputs() {
            let name = input.name();
            if !name.starts_with("past_key_values") && !name.starts_with("past_key") {
                continue;
            }
            if let ort::value::ValueType::Tensor { ty, shape, .. } = input.dtype() {
                // [batch, n_kv_heads, past_len, head_dim]
                if shape.len() == 4 {
                    out.push(KvInput {
                        name: name.to_string(),
                        n_kv_heads: shape[1],
                        head_dim: shape[3],
                        fp16: matches!(ty, ort::value::TensorElementType::Float16),
                    });
                }
            }
        }
        out
    }

    /// Run real decoder-ONNX inference: tokenize → feed ids/mask/position_ids +
    /// empty past_key_values → last-token pool → MRL-512 slice + L2-norm.
    #[cfg(feature = "onnx-embeddings")]
    fn qwen3_embed(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        use std::borrow::Cow;

        use ort::session::SessionInputValue;
        use ort::value::Tensor;

        let session = self
            .session
            .as_ref()
            .ok_or_else(|| EmbeddingError::BackendError("Qwen3 session not loaded".to_string()))?;
        let tokenizer = self.tokenizer.as_ref().ok_or_else(|| {
            EmbeddingError::BackendError("Qwen3 tokenizer not loaded".to_string())
        })?;

        let encoding = tokenizer
            .encode(text, true)
            .map_err(|e| EmbeddingError::BackendError(format!("tokenize error: {e}")))?;
        let mut input_ids: Vec<i64> = encoding.get_ids().iter().map(|&u| u as i64).collect();
        let mut attention_mask: Vec<i64> = encoding
            .get_attention_mask()
            .iter()
            .map(|&u| u as i64)
            .collect();
        // Append the Qwen3 EOS (`<|im_end|>`) for last-token pooling if the
        // tokenizer's post-processor didn't already terminate on it. The fp16
        // export pools the hidden state at this EOS position; omitting it pools
        // the wrong token and destroys lab parity (~0.72 cosine without it).
        if let Some(eos) = tokenizer.token_to_id("<|im_end|>").map(|u| u as i64)
            && input_ids.last() != Some(&eos)
        {
            input_ids.push(eos);
            attention_mask.push(1);
        }
        let seq_len = input_ids.len();
        if seq_len == 0 {
            return Ok(vec![0.0; self.dimensions]);
        }
        let position_ids: Vec<i64> = (0..seq_len as i64).collect();
        let shape = vec![1_i64, seq_len as i64];

        let ids_t = Tensor::from_array((shape.clone(), input_ids))
            .map_err(|e| EmbeddingError::BackendError(format!("input_ids tensor: {e}")))?;
        let mask_t = Tensor::from_array((shape.clone(), attention_mask.clone()))
            .map_err(|e| EmbeddingError::BackendError(format!("attention_mask tensor: {e}")))?;
        let pos_t = Tensor::from_array((shape, position_ids))
            .map_err(|e| EmbeddingError::BackendError(format!("position_ids tensor: {e}")))?;

        let mut inputs = ort::inputs![
            "input_ids" => ids_t,
            "attention_mask" => mask_t,
            "position_ids" => pos_t,
        ];

        // Empty, dtype-matched KV cache for every layer (past_len = 0). ort
        // forbids a 0-dim in the (shape, data) `from_array` path, so allocate a
        // 0-element tensor via the default CPU allocator instead.
        let allocator = ort::memory::Allocator::default();
        for kv in &self.kv_spec {
            let kv_shape = ort::value::Shape::new([1, kv.n_kv_heads, 0, kv.head_dim]);
            let value: SessionInputValue<'_> = if kv.fp16 {
                Tensor::<half::f16>::new(&allocator, kv_shape)
                    .map_err(|e| EmbeddingError::BackendError(format!("kv tensor: {e}")))?
                    .into()
            } else {
                Tensor::<f32>::new(&allocator, kv_shape)
                    .map_err(|e| EmbeddingError::BackendError(format!("kv tensor: {e}")))?
                    .into()
            };
            inputs.push((Cow::from(kv.name.clone()), value));
        }

        let mut guard = session.lock().map_err(|_| {
            EmbeddingError::BackendError("Qwen3 session mutex poisoned".to_string())
        })?;
        let outputs = guard
            .run(inputs)
            .map_err(|e| EmbeddingError::BackendError(format!("inference error: {e}")))?;

        let output = match outputs.get("last_hidden_state") {
            Some(v) => v,
            None if outputs.len() > 0 => &outputs[0],
            None => {
                return Err(EmbeddingError::BackendError("no output tensor".to_string()));
            }
        };

        // fp16 model → output is likely fp16; try f32 first, then f16→f32.
        let (out_shape, hidden): (Vec<i64>, Vec<f32>) = match output.try_extract_tensor::<f32>() {
            Ok((s, d)) => (s.to_vec(), d.to_vec()),
            Err(_) => {
                let (s, d) = output
                    .try_extract_tensor::<half::f16>()
                    .map_err(|e| EmbeddingError::BackendError(format!("extract error: {e}")))?;
                (s.to_vec(), d.iter().map(|h| h.to_f32()).collect())
            }
        };

        if out_shape.len() < 3 {
            return Err(EmbeddingError::BackendError(format!(
                "unexpected output shape: {out_shape:?}"
            )));
        }
        let seq = out_shape[out_shape.len() - 2] as usize;
        let hidden_dim = out_shape[out_shape.len() - 1] as usize;

        let pooled = last_token_pool(&hidden, seq, hidden_dim, &attention_mask);
        Ok(mrl_slice_l2norm(&pooled, self.dimensions))
    }
}

#[async_trait]
impl EmbeddingProvider for Qwen3EmbeddingProvider {
    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        #[cfg(feature = "onnx-embeddings")]
        if self.runtime_available {
            return self.qwen3_embed(text);
        }
        let _ = text;
        Err(self.unavailable_error())
    }

    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        let mut results = Vec::with_capacity(texts.len());
        for text in texts {
            results.push(self.embed(text).await?);
        }
        Ok(results)
    }

    /// Asymmetric query lane — delegates to the inherent
    /// [`Self::embed_query`] so trait-object call sites
    /// (`SessionView::graft_text`) get the instruction prefix.
    async fn embed_query(&self, query: &str) -> Result<Vec<f32>, EmbeddingError> {
        // Call the inherent method body directly (avoid infinite recursion on
        // the trait method of the same name).
        #[cfg(feature = "onnx-embeddings")]
        if self.runtime_available {
            let prompted = format!("{QUERY_INSTRUCTION}{query}");
            return self.qwen3_embed(&prompted);
        }
        let _ = query;
        Err(self.unavailable_error())
    }

    fn dimensions(&self) -> usize {
        self.dimensions
    }

    fn model_name(&self) -> &str {
        &self.model_name
    }
}

// ── Tests (no model required; split to a sibling file for the <500-line rule) ─

#[cfg(test)]
#[path = "embedding_qwen3_tests.rs"]
mod tests;
