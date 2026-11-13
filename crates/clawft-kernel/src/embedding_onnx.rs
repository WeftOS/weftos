//! ONNX, sentence-transformer, and AST-aware embedding backends (K3c-G2).
//!
//! These are alternative [`EmbeddingProvider`] implementations that complement
//! the existing [`LlmEmbeddingProvider`].
//!
//! - [`OnnxEmbeddingProvider`] -- local BERT-family model inference
//!   (all-MiniLM-L6-v2 384-d, nomic-embed-text-v1.5 768-d, e5). Tokenisation is
//!   the model's own `tokenizer.json` when present, else a `vocab.txt`
//!   WordPiece (BERT-uncased ids only). With neither, or without the model
//!   file or the `onnx-embeddings` feature, every call fails with
//!   [`EmbeddingError::ModelUnavailable`] naming the path. There is NO hash
//!   fallback.
//! - [`SentenceTransformerProvider`] -- documentation-optimised paragraph embedder.
//! - [`AstEmbeddingProvider`] -- hybrid structural + semantic embedder for Rust code.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
#[cfg(any(feature = "onnx-embeddings", feature = "tract-embeddings"))]
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::embedding::{EmbeddingError, EmbeddingProvider};

// ---------------------------------------------------------------------------
// WordPiece tokenizer for BERT / all-MiniLM-L6-v2
// ---------------------------------------------------------------------------

/// BERT-compatible WordPiece tokenizer.
///
/// Loads a `vocab.txt` file (one token per line, ID = line number) and performs:
/// 1. Lowercasing and Unicode NFD accent stripping
/// 2. Whitespace + punctuation pre-tokenization
/// 3. Greedy longest-match WordPiece splitting (subwords prefixed with `##`)
/// 4. [CLS] / [SEP] framing and [PAD] / truncation to `max_length`
///
/// Special-token ids are the BERT **uncased** ones (101/102/100/0), so this
/// only suits models sharing that vocabulary (all-MiniLM-L6-v2, e5-small-v2).
/// Other BERT-family models (nomic, ...) use their own `tokenizer.json`.
pub struct WordPieceTokenizer {
    /// Token string -> vocab ID.
    vocab: HashMap<String, i64>,
    /// Maximum sequence length including [CLS] and [SEP].
    max_length: usize,
    /// Maximum characters per word before treating as [UNK].
    max_word_chars: usize,
}

/// Special token IDs for the BERT uncased vocabulary.
const CLS_ID: i64 = 101;
const SEP_ID: i64 = 102;
const UNK_ID: i64 = 100;
const PAD_ID: i64 = 0;

impl WordPieceTokenizer {
    /// Try to load a `vocab.txt` from the given path.
    ///
    /// Returns `None` if the file does not exist or cannot be read.
    pub fn load(vocab_path: &Path) -> Option<Self> {
        let content = std::fs::read_to_string(vocab_path).ok()?;
        let line_count = content.lines().count();
        let mut vocab = HashMap::with_capacity(line_count);
        for (id, line) in content.lines().enumerate() {
            vocab.insert(line.to_string(), id as i64);
        }
        if vocab.len() < 1000 {
            // Suspiciously small — probably not a real BERT vocab.
            tracing::warn!(
                "vocab.txt at {} has only {} entries, expected ~30k",
                vocab_path.display(),
                vocab.len()
            );
            return None;
        }
        tracing::info!(
            "WordPiece vocab loaded: {} tokens from {}",
            vocab.len(),
            vocab_path.display()
        );
        Some(Self {
            vocab,
            max_length: 128,
            max_word_chars: 100,
        })
    }

    /// Create a tokenizer with a custom max sequence length.
    pub fn with_max_length(mut self, max_length: usize) -> Self {
        self.max_length = max_length;
        self
    }

    /// Encode text into (input_ids, attention_mask, token_type_ids).
    ///
    /// All three vectors have length `self.max_length`, padded or truncated
    /// as needed. Returns `None` if the vocab is empty.
    pub fn encode(&self, text: &str) -> Option<(Vec<i64>, Vec<i64>, Vec<i64>)> {
        if self.vocab.is_empty() {
            return None;
        }

        let mut token_ids: Vec<i64> = Vec::with_capacity(self.max_length);
        token_ids.push(CLS_ID);

        // Pre-tokenize: lowercase, split on whitespace and punctuation.
        let words = self.pre_tokenize(text);

        for word in &words {
            if token_ids.len() >= self.max_length - 1 {
                break; // Reserve space for [SEP].
            }
            let sub_ids = self.wordpiece_split(word);
            for id in sub_ids {
                if token_ids.len() >= self.max_length - 1 {
                    break;
                }
                token_ids.push(id);
            }
        }

        token_ids.push(SEP_ID);

        let seq_len = token_ids.len();
        let mut attention_mask = vec![1i64; seq_len];
        let mut token_type_ids = vec![0i64; seq_len];

        // Pad to max_length.
        while token_ids.len() < self.max_length {
            token_ids.push(PAD_ID);
            attention_mask.push(0);
            token_type_ids.push(0);
        }

        Some((token_ids, attention_mask, token_type_ids))
    }

    /// Pre-tokenize: lowercase, split on whitespace and punctuation.
    fn pre_tokenize(&self, text: &str) -> Vec<String> {
        let lower = text.to_lowercase();
        let mut words = Vec::new();
        let mut current = String::new();

        for ch in lower.chars() {
            if ch.is_whitespace() {
                if !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                }
            } else if ch.is_ascii_punctuation() || is_cjk_char(ch) {
                // Punctuation and CJK chars become individual tokens.
                if !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                }
                words.push(ch.to_string());
            } else if is_accent_char(ch) {
                // Strip combining marks (basic accent removal).
                continue;
            } else if ch.is_control() {
                continue;
            } else {
                current.push(ch);
            }
        }
        if !current.is_empty() {
            words.push(current);
        }
        words
    }

    /// WordPiece greedy longest-match splitting for a single pre-token.
    fn wordpiece_split(&self, word: &str) -> Vec<i64> {
        if word.len() > self.max_word_chars {
            return vec![UNK_ID];
        }

        let chars: Vec<char> = word.chars().collect();
        let mut ids = Vec::new();
        let mut start = 0;

        while start < chars.len() {
            let mut end = chars.len();
            let mut found = false;

            while start < end {
                let substr: String = if start == 0 {
                    chars[start..end].iter().collect()
                } else {
                    format!("##{}", chars[start..end].iter().collect::<String>())
                };

                if self.vocab.contains_key(&substr) {
                    ids.push(self.vocab[&substr]);
                    found = true;
                    start = end;
                    break;
                }
                end -= 1;
            }

            if !found {
                ids.push(UNK_ID);
                start += 1;
            }
        }

        ids
    }
}

/// Check if a character is in the CJK Unified Ideographs range.
fn is_cjk_char(ch: char) -> bool {
    let cp = ch as u32;
    matches!(cp,
        0x4E00..=0x9FFF
        | 0x3400..=0x4DBF
        | 0x20000..=0x2A6DF
        | 0x2A700..=0x2B73F
        | 0x2B740..=0x2B81F
        | 0x2B820..=0x2CEAF
        | 0xF900..=0xFAFF
        | 0x2F800..=0x2FA1F
    )
}

/// Check if a character is a Unicode combining mark (accent).
fn is_accent_char(ch: char) -> bool {
    let cp = ch as u32;
    matches!(cp, 0x0300..=0x036F | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0xFE20..=0xFE2F)
}

/// Search paths for the vocab.txt file alongside an ONNX model.
///
/// Looks for `vocab.txt` in the same directory as the model, and in a
/// sibling directory named after the model (e.g., `all-MiniLM-L6-v2/vocab.txt`).
#[cfg_attr(not(feature = "onnx-embeddings"), allow(dead_code))]
fn vocab_search_paths(model_path: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();

    if let Some(parent) = model_path.parent() {
        // Same directory as the model.
        paths.push(parent.join("vocab.txt"));

        // Sibling directory named after the model.
        if let Some(stem) = model_path.file_stem() {
            paths.push(parent.join(stem).join("vocab.txt"));
        }
    }

    // Also check standard WeftOS model paths.
    let model_dir_name = "all-MiniLM-L6-v2";
    paths.push(PathBuf::from(format!(
        ".weftos/models/{model_dir_name}/vocab.txt"
    )));
    if let Ok(home) = std::env::var("HOME") {
        paths.push(PathBuf::from(format!(
            "{home}/.weftos/models/{model_dir_name}/vocab.txt"
        )));
    }
    if let Ok(env_dir) = std::env::var("WEFTOS_VOCAB_PATH") {
        paths.push(PathBuf::from(env_dir));
    }

    paths
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// In-place L2 normalisation.
fn l2_normalize(vec: &mut [f32]) {
    let norm: f32 = vec.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        vec.iter_mut().for_each(|x| *x /= norm);
    }
}

// =========================================================================
// Backend 1: OnnxEmbeddingProvider
// =========================================================================

/// How a model's text becomes ids.
#[cfg(any(feature = "onnx-embeddings", feature = "tract-embeddings"))]
enum Tokenization {
    /// The model's own HuggingFace `tokenizer.json` (nomic, BERT, e5, ...).
    HfJson(Box<tokenizers::Tokenizer>),
    /// A BERT-uncased `vocab.txt` WordPiece (MiniLM-class models only).
    WordPiece(WordPieceTokenizer),
}

/// ONNX Runtime embedding provider for BERT-family encoders.
///
/// Defaults to all-MiniLM-L6-v2 (384-d); [`OnnxEmbeddingProvider::nomic_embed_v1_5`]
/// builds a 768-d nomic-embed-text-v1.5 provider with its task prefixes. The
/// model's own `tokenizer.json` is used when present (next to the model file or
/// one directory up, which covers the `onnx/model.onnx` layout); otherwise a
/// `vocab.txt` WordPiece for BERT-uncased models.
///
/// If the model, its tokenizer, or the `onnx-embeddings` feature is missing,
/// [`is_runtime_available`](Self::is_runtime_available) is false and every
/// embed call returns [`EmbeddingError::ModelUnavailable`] naming the path.
/// It never substitutes hashed vectors.
pub struct OnnxEmbeddingProvider {
    /// Path to the ONNX model file.
    model_path: PathBuf,
    /// Output dimensions.
    dimensions: usize,
    /// Model name for identification.
    model_name: String,
    /// Whether the ONNX runtime session and a tokenizer are both live.
    runtime_available: bool,
    /// Why the runtime is not available (names the path), when it is not.
    unavailable_reason: Option<String>,
    /// Max input tokens.
    max_tokens: usize,
    /// Prefix applied by [`EmbeddingProvider::embed`] (document lane).
    doc_prefix: String,
    /// Prefix applied by [`EmbeddingProvider::embed_query`] (query lane).
    query_prefix: String,
    #[cfg(any(feature = "onnx-embeddings", feature = "tract-embeddings"))]
    tokenization: Option<Tokenization>,
    /// The loaded graph (only present when a local engine feature is on and the
    /// model loaded): ONNX Runtime under `onnx-embeddings`, the pure-Rust tract
    /// engine under `tract-embeddings` (clawft-core's `onnx_graph`, ADR-132).
    #[cfg(any(feature = "onnx-embeddings", feature = "tract-embeddings"))]
    session: Option<Arc<clawft_core::embeddings::onnx_graph::OnnxGraph>>,
}

impl OnnxEmbeddingProvider {
    /// Default output dimensionality (all-MiniLM-L6-v2).
    pub const DEFAULT_DIMS: usize = 384;
    /// Default maximum input tokens for code snippets.
    pub const DEFAULT_MAX_TOKENS: usize = 128;
    /// Model identifier.
    pub const MODEL_NAME: &'static str = "all-MiniLM-L6-v2";
    /// nomic-embed-text-v1.5 output dimensionality.
    pub const NOMIC_DIMS: usize = 768;
    /// nomic-embed-text-v1.5 identifier.
    pub const NOMIC_MODEL_NAME: &'static str = "nomic-embed-text-v1.5";

    /// Create a provider pointing at the given model path (all-MiniLM-L6-v2 defaults).
    ///
    /// Never panics and never needs the model to construct; check
    /// [`is_runtime_available`](Self::is_runtime_available).
    pub fn new(model_path: impl Into<PathBuf>) -> Self {
        Self::build(model_path.into(), Self::DEFAULT_DIMS, Self::DEFAULT_MAX_TOKENS, Self::MODEL_NAME)
    }

    /// Create a provider with custom dimensions and max tokens.
    pub fn with_config(
        model_path: impl Into<PathBuf>,
        dimensions: usize,
        max_tokens: usize,
    ) -> Self {
        Self::build(model_path.into(), dimensions, max_tokens, Self::MODEL_NAME)
    }

    /// nomic-embed-text-v1.5 from a bundle directory holding `onnx/model.onnx`
    /// and `tokenizer.json`. 768-d, masked mean pooling, L2 normalised; applies
    /// the model's required `search_document: ` / `search_query: ` prefixes.
    pub fn nomic_embed_v1_5(model_dir: impl AsRef<Path>) -> Self {
        let mut p = Self::build(
            model_dir.as_ref().join("onnx/model.onnx"),
            Self::NOMIC_DIMS,
            510,
            Self::NOMIC_MODEL_NAME,
        );
        p.doc_prefix = "search_document: ".to_string();
        p.query_prefix = "search_query: ".to_string();
        p
    }

    fn build(model_path: PathBuf, dimensions: usize, max_tokens: usize, name: &str) -> Self {
        #[cfg(any(feature = "onnx-embeddings", feature = "tract-embeddings"))]
        let (session, session_err) = Self::try_load_session(&model_path);
        #[cfg(any(feature = "onnx-embeddings", feature = "tract-embeddings"))]
        let (tokenization, tok_err) = Self::try_load_tokenization(&model_path, max_tokens);

        #[cfg(any(feature = "onnx-embeddings", feature = "tract-embeddings"))]
        let (runtime_available, unavailable_reason) = {
            let errs: Vec<String> = session_err.into_iter().chain(tok_err).collect();
            if session.is_some() && tokenization.is_some() {
                (true, None)
            } else {
                (false, Some(errs.join("; ")))
            }
        };
        #[cfg(not(any(feature = "onnx-embeddings", feature = "tract-embeddings")))]
        let (runtime_available, unavailable_reason) = (
            false,
            Some(format!(
                "this build has no local embedding engine (onnx-embeddings or tract-embeddings), so {} cannot be loaded",
                model_path.display()
            )),
        );

        Self {
            model_name: if runtime_available {
                name.to_string()
            } else {
                format!("{name}-unavailable")
            },
            model_path,
            dimensions,
            runtime_available,
            unavailable_reason,
            max_tokens,
            doc_prefix: String::new(),
            query_prefix: String::new(),
            #[cfg(any(feature = "onnx-embeddings", feature = "tract-embeddings"))]
            tokenization,
            #[cfg(any(feature = "onnx-embeddings", feature = "tract-embeddings"))]
            session,
        }
    }

    /// Candidate locations for the model's own `tokenizer.json`.
    #[cfg_attr(not(any(feature = "onnx-embeddings", feature = "tract-embeddings")), allow(dead_code))]
    fn tokenizer_json_candidates(model_path: &Path) -> Vec<PathBuf> {
        let mut v = Vec::new();
        if let Some(dir) = model_path.parent() {
            v.push(dir.join("tokenizer.json"));
            if let Some(up) = dir.parent() {
                v.push(up.join("tokenizer.json"));
            }
        }
        v
    }

    /// Load the tokenizer: the model's `tokenizer.json`, else a `vocab.txt`
    /// WordPiece. Returns the reason (naming the paths) when neither loads.
    #[cfg(any(feature = "onnx-embeddings", feature = "tract-embeddings"))]
    fn try_load_tokenization(
        model_path: &Path,
        max_tokens: usize,
    ) -> (Option<Tokenization>, Option<String>) {
        let candidates = Self::tokenizer_json_candidates(model_path);
        for path in &candidates {
            if path.exists() {
                return match tokenizers::Tokenizer::from_file(path) {
                    Ok(t) => {
                        tracing::info!("tokenizer.json loaded from {}", path.display());
                        (Some(Tokenization::HfJson(Box::new(t))), None)
                    }
                    Err(e) => (None, Some(format!("tokenizer {} failed to load: {e}", path.display()))),
                };
            }
        }
        for path in vocab_search_paths(model_path) {
            if path.exists()
                && let Some(tok) = WordPieceTokenizer::load(&path)
            {
                // max_tokens is the token count limit; WordPiece max_length
                // includes [CLS]/[SEP] and is capped at 512 for BERT models.
                let max_len = (max_tokens + 2).min(512);
                return (Some(Tokenization::WordPiece(tok.with_max_length(max_len))), None);
            }
        }
        let tried: Vec<String> = candidates.iter().map(|p| p.display().to_string()).collect();
        (
            None,
            Some(format!(
                "no tokenizer for {}: looked for tokenizer.json at [{}] and a BERT vocab.txt",
                model_path.display(),
                tried.join(", ")
            )),
        )
    }

    /// Load the model graph (ONNX Runtime or tract, per the build's engine).
    #[cfg(any(feature = "onnx-embeddings", feature = "tract-embeddings"))]
    fn try_load_session(
        model_path: &Path,
    ) -> (Option<Arc<clawft_core::embeddings::onnx_graph::OnnxGraph>>, Option<String>) {
        if !model_path.exists() {
            return (None, Some(format!("model file not found at {}", model_path.display())));
        }
        // `None`: the engine's default thread count, as before the shared runner.
        match clawft_core::embeddings::onnx_graph::OnnxGraph::load(model_path, None) {
            Ok(graph) => {
                tracing::info!(
                    "ONNX session loaded from {} ({})",
                    model_path.display(),
                    clawft_core::embeddings::onnx_graph::ENGINE
                );
                (Some(Arc::new(graph)), None)
            }
            Err(e) => (
                None,
                Some(format!("ONNX session failed to load from {}: {e}", model_path.display())),
            ),
        }
    }

    /// Whether the real ONNX runtime is active.
    pub fn is_runtime_available(&self) -> bool {
        self.runtime_available
    }

    /// Why the runtime is unavailable (names the path), if it is.
    pub fn unavailable_reason(&self) -> Option<&str> {
        self.unavailable_reason.as_deref()
    }

    /// Path to the configured model file.
    pub fn model_path(&self) -> &PathBuf {
        &self.model_path
    }

    /// Maximum input token count.
    pub fn max_tokens(&self) -> usize {
        self.max_tokens
    }

    fn unavailable_error(&self) -> EmbeddingError {
        EmbeddingError::ModelUnavailable {
            what: self.model_name.clone(),
            detail: self
                .unavailable_reason
                .clone()
                .unwrap_or_else(|| format!("model at {} is not loaded", self.model_path.display())),
        }
    }

    /// Embed `text` exactly as given (callers apply any task prefix).
    fn embed_raw(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        #[cfg(any(feature = "onnx-embeddings", feature = "tract-embeddings"))]
        if self.runtime_available {
            return self.onnx_embed(text);
        }
        let _ = text;
        Err(self.unavailable_error())
    }

    /// Run real ONNX inference: tokenise, run the graph, mask-weighted mean
    /// pool the last hidden state, L2 normalise. A width other than
    /// `dimensions` is an error, never a silent truncate or zero pad.
    #[cfg(any(feature = "onnx-embeddings", feature = "tract-embeddings"))]
    fn onnx_embed(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        let session = self
            .session
            .as_ref()
            .ok_or_else(|| EmbeddingError::BackendError("ONNX session not loaded".to_string()))?;
        let tokenization = self
            .tokenization
            .as_ref()
            .ok_or_else(|| EmbeddingError::BackendError("tokenizer not loaded".to_string()))?;

        let (input_ids, attention_mask, token_type_ids) = match tokenization {
            Tokenization::WordPiece(wp) => wp.encode(text).ok_or_else(|| {
                EmbeddingError::BackendError("WordPiece tokenizer has an empty vocabulary".to_string())
            })?,
            Tokenization::HfJson(tok) => {
                let enc = tok
                    .encode(text, true)
                    .map_err(|e| EmbeddingError::BackendError(format!("tokenize error: {e}")))?;
                let ids = enc.get_ids();
                let mask = enc.get_attention_mask();
                let max_len = (self.max_tokens + 2).min(512);
                let n = ids.len().min(max_len);
                let mut ids: Vec<i64> = ids[..n].iter().map(|&x| i64::from(x)).collect();
                let mut mask: Vec<i64> = mask[..n].iter().map(|&x| i64::from(x)).collect();
                // Truncation must keep the closing special token ([SEP]).
                if enc.get_ids().len() > n
                    && let Some(&last) = enc.get_ids().last()
                {
                    if let Some(slot) = ids.last_mut() {
                        *slot = i64::from(last);
                    }
                    if let Some(slot) = mask.last_mut() {
                        *slot = 1;
                    }
                }
                let types = vec![0i64; ids.len()];
                (ids, mask, types)
            }
        };

        let (out_shape, data) = session
            .run(input_ids, attention_mask.clone(), token_type_ids)
            .map_err(EmbeddingError::BackendError)?;
        if out_shape.len() < 2 {
            return Err(EmbeddingError::BackendError(format!(
                "unexpected output shape: {out_shape:?}"
            )));
        }
        let hidden_dim = out_shape[out_shape.len() - 1];
        let seq = out_shape[1];
        if hidden_dim != self.dimensions {
            return Err(EmbeddingError::DimensionMismatch {
                expected: self.dimensions,
                got: hidden_dim,
            });
        }

        // Attention-masked mean pooling: only average over non-padding tokens.
        let mut embedding = vec![0.0f32; hidden_dim];
        let mut active_count: f32 = 0.0;
        for s in 0..seq {
            let mask_val = attention_mask.get(s).copied().unwrap_or(0) as f32;
            if mask_val > 0.0 {
                for d in 0..hidden_dim {
                    embedding[d] += data[s * hidden_dim + d];
                }
                active_count += 1.0;
            }
        }
        if active_count > 0.0 {
            for val in &mut embedding {
                *val /= active_count;
            }
        }
        l2_normalize(&mut embedding);
        Ok(embedding)
    }
}

#[async_trait]
impl EmbeddingProvider for OnnxEmbeddingProvider {
    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        if self.doc_prefix.is_empty() {
            self.embed_raw(text)
        } else {
            self.embed_raw(&format!("{}{text}", self.doc_prefix))
        }
    }

    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        let mut results = Vec::with_capacity(texts.len());
        for text in texts {
            results.push(self.embed(text).await?);
        }
        Ok(results)
    }

    /// Query lane: applies the query prefix when the model defines one.
    async fn embed_query(&self, query: &str) -> Result<Vec<f32>, EmbeddingError> {
        if self.query_prefix.is_empty() {
            self.embed_raw(query)
        } else {
            self.embed_raw(&format!("{}{query}", self.query_prefix))
        }
    }

    fn dimensions(&self) -> usize {
        self.dimensions
    }

    fn model_name(&self) -> &str {
        &self.model_name
    }
}

// =========================================================================
// Backend 2: SentenceTransformerProvider
// =========================================================================

/// Sentence-transformer embedding provider for documentation.
///
/// Optimised for natural language paragraphs rather than code.  Pre-processes
/// markdown, splits into sentences, embeds each, and averages (mean pooling).
pub struct SentenceTransformerProvider {
    /// Base ONNX provider (reuses the same infrastructure).
    base: OnnxEmbeddingProvider,
    /// Max token length (longer for docs than code).
    max_tokens: usize,
    /// Whether sentence splitting is enabled.
    split_sentences: bool,
}

impl SentenceTransformerProvider {
    /// Default max tokens for documentation (longer context than code).
    pub const DEFAULT_MAX_TOKENS: usize = 512;

    /// Create a new sentence-transformer provider.
    pub fn new(model_path: impl Into<PathBuf>) -> Self {
        Self {
            base: OnnxEmbeddingProvider::with_config(
                model_path,
                OnnxEmbeddingProvider::DEFAULT_DIMS,
                Self::DEFAULT_MAX_TOKENS,
            ),
            max_tokens: Self::DEFAULT_MAX_TOKENS,
            split_sentences: true,
        }
    }

    /// Create with custom max tokens and optional sentence splitting.
    pub fn with_config(
        model_path: impl Into<PathBuf>,
        max_tokens: usize,
        split_sentences: bool,
    ) -> Self {
        Self {
            base: OnnxEmbeddingProvider::with_config(
                model_path,
                OnnxEmbeddingProvider::DEFAULT_DIMS,
                max_tokens,
            ),
            max_tokens,
            split_sentences,
        }
    }

    /// Whether sentence splitting is enabled.
    pub fn split_sentences(&self) -> bool {
        self.split_sentences
    }

    /// Max token length.
    pub fn max_tokens(&self) -> usize {
        self.max_tokens
    }

    /// Embed a single sentence/paragraph through the base (real-model) provider.
    async fn embed_text(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        self.base.embed(text).await
    }
}

/// Pre-process markdown text by stripping structural elements.
pub fn preprocess_markdown(text: &str) -> String {
    text.lines()
        .filter(|l| !l.starts_with('#')) // skip headers
        .filter(|l| !l.starts_with("```")) // skip code fences
        .filter(|l| !l.starts_with('|')) // skip tables
        .filter(|l| !l.starts_with("- [")) // skip checklists
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Simple sentence splitting on ". " and newlines.
pub fn split_sentences(text: &str) -> Vec<&str> {
    text.split(". ")
        .flat_map(|s| s.split('\n'))
        .map(|s| s.trim())
        .filter(|s| s.len() > 10) // skip tiny fragments
        .collect()
}

#[async_trait]
impl EmbeddingProvider for SentenceTransformerProvider {
    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        let cleaned = preprocess_markdown(text);

        if !self.split_sentences {
            return self.embed_text(&cleaned).await;
        }

        let sentences = split_sentences(&cleaned);
        if sentences.is_empty() {
            // Fall through to full-text embedding.
            return self.embed_text(&cleaned).await;
        }

        // Mean pooling across sentence embeddings.
        let dims = self.base.dimensions;
        let mut summed = vec![0.0f32; dims];
        let count = sentences.len() as f32;

        for sentence in &sentences {
            let vec = self.embed_text(sentence).await?;
            for (i, val) in vec.iter().enumerate() {
                summed[i] += val;
            }
        }

        // Average and re-normalise.
        summed.iter_mut().for_each(|x| *x /= count);
        l2_normalize(&mut summed);

        Ok(summed)
    }

    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        let mut results = Vec::with_capacity(texts.len());
        for text in texts {
            results.push(self.embed(text).await?);
        }
        Ok(results)
    }

    fn dimensions(&self) -> usize {
        self.base.dimensions
    }

    fn model_name(&self) -> &str {
        "sentence-transformer"
    }
}

// =========================================================================
// Backend 3: AstEmbeddingProvider
// =========================================================================

/// Structural features extracted from Rust source code via regex parsing.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RustCodeFeatures {
    /// Function/type/impl signature.
    pub signature: Option<String>,
    /// Return type.
    pub return_type: Option<String>,
    /// Parameter types.
    pub param_types: Vec<String>,
    /// Visibility (pub, pub(crate), private).
    pub visibility: String,
    /// Whether it is async.
    pub is_async: bool,
    /// Whether it is generic.
    pub is_generic: bool,
    /// Trait bounds.
    pub trait_bounds: Vec<String>,
    /// Attributes (#[test], #[cfg(...)], etc.)
    pub attributes: Vec<String>,
    /// Item kind (fn, struct, enum, impl, trait, mod).
    pub item_kind: String,
}

/// Extract structural features from a Rust code snippet using simple regex-like
/// parsing.  Does not depend on tree-sitter so the kernel stays lightweight.
pub fn extract_rust_features(code: &str) -> RustCodeFeatures {
    let mut features = RustCodeFeatures::default();

    // -- Item kind --------------------------------------------------------
    // Order matters: check trait/struct/enum before fn, because trait and
    // impl bodies often contain `fn` keywords.
    if code.contains("pub trait ") || code.contains("trait ") {
        features.item_kind = "trait".into();
    } else if code.contains("pub struct ") || code.contains("struct ") {
        features.item_kind = "struct".into();
    } else if code.contains("pub enum ") || code.contains("enum ") {
        features.item_kind = "enum".into();
    } else if code.contains("pub fn ") || code.contains("fn ") {
        features.item_kind = "fn".into();
    } else if code.contains("impl ") {
        features.item_kind = "impl".into();
    } else if code.contains("pub mod ") || code.contains("mod ") {
        features.item_kind = "mod".into();
    }

    // -- Visibility -------------------------------------------------------
    features.visibility = if code.contains("pub(crate)") {
        "pub(crate)".into()
    } else if code.contains("pub(super)") {
        "pub(super)".into()
    } else if code.contains("pub ") {
        "pub".into()
    } else {
        "private".into()
    };

    // -- Async ------------------------------------------------------------
    features.is_async = code.contains("async fn");

    // -- Generics ---------------------------------------------------------
    features.is_generic = code.contains('<') && code.contains('>');

    // -- Signature (first fn/struct/enum/trait line) -----------------------
    for line in code.lines() {
        let trimmed = line.trim();
        if trimmed.contains("fn ")
            || trimmed.starts_with("pub struct ")
            || trimmed.starts_with("struct ")
            || trimmed.starts_with("pub enum ")
            || trimmed.starts_with("enum ")
            || trimmed.starts_with("pub trait ")
            || trimmed.starts_with("trait ")
        {
            // Take up to '{' or end of line.
            let sig = if let Some(brace) = trimmed.find('{') {
                trimmed[..brace].trim()
            } else {
                trimmed.trim_end_matches(';').trim()
            };
            features.signature = Some(sig.to_string());
            break;
        }
    }

    // -- Return type (after -> before { or ;) -----------------------------
    if let Some(arrow) = code.find("->") {
        let after = &code[arrow + 2..];
        if let Some(brace) = after.find('{') {
            features.return_type = Some(after[..brace].trim().to_string());
        } else if let Some(semi) = after.find(';') {
            features.return_type = Some(after[..semi].trim().to_string());
        }
    }

    // -- Parameter types (inside parentheses of fn) -----------------------
    if features.item_kind == "fn"
        && let Some(open) = code.find('(')
        && let Some(close) = code.find(')')
        && close > open
    {
        let params = &code[open + 1..close];
        for param in params.split(',') {
            let param = param.trim();
            if param == "&self" || param == "&mut self" || param == "self" {
                features.param_types.push(param.to_string());
            } else if let Some(colon) = param.find(':') {
                let ty = param[colon + 1..].trim().to_string();
                if !ty.is_empty() {
                    features.param_types.push(ty);
                }
            }
        }
    }

    // -- Trait bounds (where clause) --------------------------------------
    if let Some(where_idx) = code.find("where") {
        let after = &code[where_idx + 5..];
        let end = after.find('{').unwrap_or(after.len());
        let clause = &after[..end];
        for bound in clause.split(',') {
            let bound = bound.trim();
            if !bound.is_empty() {
                features.trait_bounds.push(bound.to_string());
            }
        }
    }

    // -- Attributes -------------------------------------------------------
    for line in code.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("#[") {
            features.attributes.push(trimmed.to_string());
        }
    }

    features
}

/// AST-aware embedding provider for Rust source code.
///
/// Combines structural features (signature, types, visibility) with semantic
/// text embeddings for hybrid code understanding.
pub struct AstEmbeddingProvider {
    /// Base text embedding provider.
    text_provider: OnnxEmbeddingProvider,
    /// Dimensions allocated to structural features.
    structural_dims: usize,
    /// Total output dimensions.
    total_dims: usize,
    /// Weight for structural vs. text features [0.0, 1.0].
    structural_weight: f32,
}

impl AstEmbeddingProvider {
    /// Default structural feature dimensions.
    pub const DEFAULT_STRUCTURAL_DIMS: usize = 64;
    /// Default total output dimensionality: the structural block plus the text
    /// model's native width (no truncating a real embedding to fit).
    pub const DEFAULT_TOTAL_DIMS: usize =
        Self::DEFAULT_STRUCTURAL_DIMS + OnnxEmbeddingProvider::DEFAULT_DIMS;
    /// Default weight for structural features.
    pub const DEFAULT_STRUCTURAL_WEIGHT: f32 = 0.3;

    /// Create a new AST-aware provider with default configuration.
    pub fn new(model_path: impl Into<PathBuf>) -> Self {
        Self {
            text_provider: OnnxEmbeddingProvider::with_config(
                model_path,
                Self::DEFAULT_TOTAL_DIMS - Self::DEFAULT_STRUCTURAL_DIMS,
                OnnxEmbeddingProvider::DEFAULT_MAX_TOKENS,
            ),
            structural_dims: Self::DEFAULT_STRUCTURAL_DIMS,
            total_dims: Self::DEFAULT_TOTAL_DIMS,
            structural_weight: Self::DEFAULT_STRUCTURAL_WEIGHT,
        }
    }

    /// Create with custom configuration.
    pub fn with_config(
        model_path: impl Into<PathBuf>,
        total_dims: usize,
        structural_dims: usize,
        structural_weight: f32,
    ) -> Self {
        assert!(
            structural_dims < total_dims,
            "structural_dims must be less than total_dims"
        );
        let text_dims = total_dims - structural_dims;
        Self {
            text_provider: OnnxEmbeddingProvider::with_config(
                model_path,
                text_dims,
                OnnxEmbeddingProvider::DEFAULT_MAX_TOKENS,
            ),
            structural_dims,
            total_dims,
            structural_weight: structural_weight.clamp(0.0, 1.0),
        }
    }

    /// Total output dimensions.
    pub fn total_dims(&self) -> usize {
        self.total_dims
    }

    /// Weight applied to structural features.
    pub fn structural_weight(&self) -> f32 {
        self.structural_weight
    }

    /// Encode [`RustCodeFeatures`] into a fixed-size structural vector.
    fn encode_structural(&self, features: &RustCodeFeatures) -> Vec<f32> {
        let dims = self.structural_dims;
        let mut vec = vec![0.0f32; dims];

        // Hash each feature category into different regions of the vector.
        let mut write_hash = |label: &str, offset: usize, slots: usize| {
            let mut hasher = Sha256::new();
            hasher.update(label.as_bytes());
            let hash = hasher.finalize();
            for (j, &byte) in hash.iter().enumerate().take(slots.min(32)) {
                let dim = (offset + j) % dims;
                vec[dim] += (byte as f32 / 128.0) - 1.0;
            }
        };

        // Item kind (fn, struct, enum, ...).
        write_hash(&format!("kind:{}", features.item_kind), 0, 8);

        // Visibility.
        write_hash(&format!("vis:{}", features.visibility), 8, 6);

        // Async flag.
        if features.is_async {
            write_hash("async:true", 14, 4);
        }

        // Generic flag.
        if features.is_generic {
            write_hash("generic:true", 18, 4);
        }

        // Return type.
        if let Some(ref rt) = features.return_type {
            write_hash(&format!("ret:{rt}"), 22, 8);
        }

        // Parameter types.
        for (i, pt) in features.param_types.iter().enumerate() {
            write_hash(&format!("param{i}:{pt}"), 30 + i * 6, 6);
        }

        // Attributes.
        for (i, attr) in features.attributes.iter().enumerate() {
            write_hash(&format!("attr{i}:{attr}"), 48 + i * 4, 4);
        }

        l2_normalize(&mut vec);
        vec
    }

    /// Produce the hybrid embedding for a Rust code snippet.
    ///
    /// The structural block is a feature-hash of parsed signature facts (item
    /// kind, visibility, types); that is deliberate structure encoding, not a
    /// stand-in for semantics. The text block is the real model's embedding and
    /// fails with `ModelUnavailable` when the model is absent.
    async fn hybrid_embed(&self, code: &str) -> Result<Vec<f32>, EmbeddingError> {
        let features = extract_rust_features(code);
        let structural = self.encode_structural(&features);
        let text = self.text_provider.embed(code).await?;

        let w_s = self.structural_weight;
        let w_t = 1.0 - w_s;

        // Concatenate weighted structural + text vectors.
        let mut combined = Vec::with_capacity(self.total_dims);
        for val in &structural {
            combined.push(val * w_s);
        }
        for val in &text {
            combined.push(val * w_t);
        }

        // Ensure exact dimensionality.
        combined.truncate(self.total_dims);
        while combined.len() < self.total_dims {
            combined.push(0.0);
        }

        l2_normalize(&mut combined);
        Ok(combined)
    }
}

#[async_trait]
impl EmbeddingProvider for AstEmbeddingProvider {
    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        self.hybrid_embed(text).await
    }

    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        let mut out = Vec::with_capacity(texts.len());
        for t in texts {
            out.push(self.hybrid_embed(t).await?);
        }
        Ok(out)
    }

    fn dimensions(&self) -> usize {
        self.total_dims
    }

    fn model_name(&self) -> &str {
        "ast-aware-hybrid"
    }
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(feature = "onnx-embeddings")]
/// The process-wide ONNX Runtime environment. ort rc.15 allows one
/// `ort::init().build()` per process and offers no way to fetch it back, so the
/// first crate to need it builds it and every other crate reuses it through
/// `clawft_types::singleton` (core, kernel and the voice crates share a daemon).
pub(crate) fn ort_env() -> ort::Result<&'static ort::environment::Environment> {
    clawft_types::singleton::process_singleton(|| ort::init().build())
}

#[cfg(all(test, feature = "onnx-embeddings"))]
mod ort_env_tests {
    use super::ort_env;

    /// Two calls give the same environment and neither panics.
    #[test]
    fn ort_env_is_built_once_and_shared() {
        let a = ort_env().expect("environment builds");
        let b = ort_env().expect("environment is reused");
        assert!(std::ptr::eq(a, b));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- Helper -----------------------------------------------------------

    fn vec_magnitude(v: &[f32]) -> f32 {
        v.iter().map(|x| x * x).sum::<f32>().sqrt()
    }

    // =====================================================================
    // OnnxEmbeddingProvider tests
    // =====================================================================

    use crate::embedding_support::{cosine, real_embedder};

    #[test]
    fn onnx_construction_default() {
        let p = OnnxEmbeddingProvider::new("/nonexistent/model.onnx");
        assert_eq!(p.dimensions(), 384);
        assert!(!p.is_runtime_available());
        assert!(p.model_name().ends_with("-unavailable"));
    }

    #[test]
    fn onnx_construction_custom() {
        let p = OnnxEmbeddingProvider::with_config("/tmp/model.onnx", 128, 64);
        assert_eq!(p.dimensions(), 128);
        assert_eq!(p.max_tokens(), 64);
    }

    /// A missing model is an error naming the path. It is never a hash vector.
    #[tokio::test]
    async fn onnx_missing_model_is_an_error_naming_the_path_not_a_hash_result() {
        let p = OnnxEmbeddingProvider::new("/nonexistent/model.onnx");
        for err in [
            p.embed("hello world").await.unwrap_err(),
            p.embed_query("hello world").await.unwrap_err(),
            p.embed_batch(&["a", "b"]).await.unwrap_err(),
        ] {
            assert!(matches!(err, EmbeddingError::ModelUnavailable { .. }), "{err}");
            let msg = err.to_string();
            assert!(msg.contains("semantic search unavailable"), "{msg}");
            assert!(msg.contains("/nonexistent/model.onnx"), "must name the path: {msg}");
        }
        assert!(p.unavailable_reason().unwrap().contains("/nonexistent/model.onnx"));
    }

    #[tokio::test]
    async fn nomic_provider_missing_bundle_names_the_bundle_path() {
        let p = OnnxEmbeddingProvider::nomic_embed_v1_5("/nonexistent/nomic");
        assert_eq!(p.dimensions(), 768);
        let err = p.embed("x").await.unwrap_err();
        assert!(err.to_string().contains("/nonexistent/nomic/onnx/model.onnx"), "{err}");
    }

    /// Real model: the vector is the right width, unit length, deterministic,
    /// and similarity tracks meaning rather than shared spelling.
    #[tokio::test]
    #[cfg_attr(not(feature = "real-embedder-harness"), ignore = "needs --features real-embedder-tests and WEFTOS_TEST_EMBED_MODEL_DIR (nomic-embed-text-v1.5 bundle)")]
    async fn real_model_embeds_semantically() {
        let Some(p) = real_embedder() else { return };
        let v = p.embed("The deploy ran out of memory while loading the model").await.unwrap();
        assert_eq!(v.len(), 768);
        assert!((vec_magnitude(&v) - 1.0).abs() < 1e-3);
        assert_eq!(v, p.embed("The deploy ran out of memory while loading the model").await.unwrap());

        let q = p.embed_query("why did the release crash with OOM?").await.unwrap();
        let on_topic = p.embed("The deploy ran out of memory while loading the model").await.unwrap();
        let off_topic = p.embed("Sourdough needs a long cold proof overnight").await.unwrap();
        let (s_on, s_off) = (cosine(&q, &on_topic), cosine(&q, &off_topic));
        assert!(s_on > s_off + 0.05, "on-topic {s_on} should clearly beat off-topic {s_off}");
    }

    // =====================================================================
    // SentenceTransformerProvider tests
    // =====================================================================

    #[test]
    fn sentence_construction() {
        let p = SentenceTransformerProvider::new("/tmp/model.onnx");
        assert_eq!(p.dimensions(), 384);
        assert_eq!(p.max_tokens(), 512);
        assert!(p.split_sentences());
        assert_eq!(p.model_name(), "sentence-transformer");
    }

    #[tokio::test]
    async fn sentence_missing_model_is_an_error_not_a_hash_result() {
        let p = SentenceTransformerProvider::new("/nonexistent/model.onnx");
        let err = p
            .embed("This is a test paragraph with enough words.")
            .await
            .unwrap_err();
        assert!(matches!(err, EmbeddingError::ModelUnavailable { .. }), "{err}");
        assert!(err.to_string().contains("/nonexistent/model.onnx"), "{err}");
    }

    #[test]
    fn preprocess_markdown_strips_headers() {
        let md = "# Title\nSome text.\n## Subtitle\nMore text.";
        let result = preprocess_markdown(md);
        assert!(!result.contains("Title"));
        assert!(!result.contains("Subtitle"));
        assert!(result.contains("Some text."));
        assert!(result.contains("More text."));
    }

    #[test]
    fn preprocess_markdown_strips_code_fences() {
        let md = "Before.\n```rust\nlet x = 1;\n```\nAfter.";
        let result = preprocess_markdown(md);
        assert!(!result.contains("```"));
        // Code line itself is kept (only fence markers are stripped).
        assert!(result.contains("Before."));
        assert!(result.contains("After."));
    }

    #[test]
    fn preprocess_markdown_strips_tables() {
        let md = "Intro.\n| Col1 | Col2 |\n|------|------|\n| A | B |\nOutro.";
        let result = preprocess_markdown(md);
        assert!(!result.contains("Col1"));
        assert!(result.contains("Intro."));
        assert!(result.contains("Outro."));
    }

    #[test]
    fn preprocess_markdown_strips_checklists() {
        let md = "Text here.\n- [x] Done item\n- [ ] Todo item\nMore text.";
        let result = preprocess_markdown(md);
        assert!(!result.contains("Done item"));
        assert!(result.contains("Text here."));
    }

    #[test]
    fn split_sentences_basic() {
        let text = "First sentence here. Second sentence here. Third.";
        let sentences = split_sentences(text);
        // "Third." is only 6 chars, below the 10-char minimum.
        assert_eq!(sentences.len(), 2);
        assert!(sentences[0].contains("First"));
        assert!(sentences[1].contains("Second"));
    }

    #[test]
    fn split_sentences_newlines() {
        let text = "Line one is long enough.\nLine two is also long enough.";
        let sentences = split_sentences(text);
        assert_eq!(sentences.len(), 2);
    }

    // =====================================================================
    // AstEmbeddingProvider tests
    // =====================================================================

    #[test]
    fn ast_construction_default() {
        let p = AstEmbeddingProvider::new("/tmp/model.onnx");
        assert_eq!(p.dimensions(), 448);
        assert_eq!(p.total_dims(), AstEmbeddingProvider::DEFAULT_TOTAL_DIMS);
        assert!((p.structural_weight() - 0.3).abs() < 0.001);
        assert_eq!(p.model_name(), "ast-aware-hybrid");
    }

    #[tokio::test]
    async fn ast_missing_model_is_an_error_not_a_hash_result() {
        let p = AstEmbeddingProvider::new("/nonexistent/model.onnx");
        let err = p.embed("pub fn hello() -> String { }").await.unwrap_err();
        assert!(matches!(err, EmbeddingError::ModelUnavailable { .. }), "{err}");
        assert!(err.to_string().contains("/nonexistent/model.onnx"), "{err}");
        assert!(p.embed_batch(&["fn a() {}"]).await.is_err());
    }

    #[test]
    fn ast_structural_block_encodes_signature_facts_deterministically() {
        // The structural half is a deliberate feature-hash of parsed signature
        // facts (kind, visibility, types); same facts, same block.
        let p = AstEmbeddingProvider::new("/tmp/model.onnx");
        let f1 = extract_rust_features("pub async fn foo(&self, x: u32) -> Result<(), Error> {}");
        let f2 = extract_rust_features("pub async fn bar(&self, x: u32) -> Result<(), Error> {}");
        let f3 = extract_rust_features("pub struct Baz { count: usize }");
        let (a, b, c) = (p.encode_structural(&f1), p.encode_structural(&f2), p.encode_structural(&f3));
        assert_eq!(a, b, "same signature facts give the same structural block");
        assert_ne!(a, c);
        assert!((vec_magnitude(&a) - 1.0).abs() < 0.01);
    }

    // =====================================================================
    // extract_rust_features tests
    // =====================================================================

    #[test]
    fn rust_features_pub_async_fn() {
        let code = r#"
#[test]
pub async fn process_batch(&self, items: Vec<Item>) -> Result<(), Error> {
    // body
}
"#;
        let f = extract_rust_features(code);
        assert_eq!(f.item_kind, "fn");
        assert_eq!(f.visibility, "pub");
        assert!(f.is_async);
        assert!(f.is_generic);
        assert_eq!(f.return_type.as_deref(), Some("Result<(), Error>"));
        assert!(f.attributes.contains(&"#[test]".to_string()));
        assert!(f.param_types.contains(&"&self".to_string()));
        assert!(f.param_types.iter().any(|p| p.contains("Vec<Item>")));
    }

    #[test]
    fn rust_features_struct() {
        let code = "pub struct Config { pub name: String, pub value: u64 }";
        let f = extract_rust_features(code);
        assert_eq!(f.item_kind, "struct");
        assert_eq!(f.visibility, "pub");
        assert!(!f.is_async);
        assert!(!f.is_generic); // no < > in this struct
        assert!(f.return_type.is_none());
    }

    #[test]
    fn rust_features_private_fn() {
        let code = "fn helper(x: &str) -> bool { true }";
        let f = extract_rust_features(code);
        assert_eq!(f.item_kind, "fn");
        assert_eq!(f.visibility, "private");
        assert!(!f.is_async);
        assert_eq!(f.return_type.as_deref(), Some("bool"));
        assert!(f.param_types.iter().any(|p| p.contains("&str")));
    }

    #[test]
    fn rust_features_enum() {
        let code = "pub enum Status { Active, Inactive, Pending }";
        let f = extract_rust_features(code);
        assert_eq!(f.item_kind, "enum");
        assert_eq!(f.visibility, "pub");
    }

    #[test]
    fn rust_features_trait() {
        let code = "pub trait Displayable { fn display(&self) -> String; }";
        let f = extract_rust_features(code);
        assert_eq!(f.item_kind, "trait");
        assert_eq!(f.visibility, "pub");
    }

    #[test]
    fn rust_features_impl_block() {
        let code = "impl MyStruct { fn new() -> Self { Self {} } }";
        let f = extract_rust_features(code);
        // "fn" is detected before "impl" because code.contains("fn ")
        assert_eq!(f.item_kind, "fn");
    }

    #[test]
    fn rust_features_where_clause() {
        let code = "pub fn serialize<T>(val: T) -> String where T: Serialize + Debug { }";
        let f = extract_rust_features(code);
        assert!(f.is_generic);
        assert!(!f.trait_bounds.is_empty());
        assert!(f.trait_bounds.iter().any(|b| b.contains("Serialize")));
    }

    #[test]
    fn rust_features_pub_crate() {
        let code = "pub(crate) fn internal_helper() {}";
        let f = extract_rust_features(code);
        assert_eq!(f.visibility, "pub(crate)");
    }

    #[test]
    fn rust_features_multiple_attributes() {
        let code = "#[cfg(test)]\n#[allow(dead_code)]\nfn test_fn() {}";
        let f = extract_rust_features(code);
        assert_eq!(f.attributes.len(), 2);
        assert!(f.attributes.contains(&"#[cfg(test)]".to_string()));
        assert!(f.attributes.contains(&"#[allow(dead_code)]".to_string()));
    }


    // =====================================================================
    // WordPiece tokenizer tests
    // =====================================================================

    /// Create a small test vocab file for WordPiece tests.
    /// Returns the path to the written file.
    fn make_test_vocab() -> PathBuf {
        use std::fmt::Write as FmtWrite;
        let mut content = String::new();
        // Build a minimal BERT-style vocab (needs >1000 entries).
        // IDs 0-99: [unused0]..[unused99]
        for i in 0..100 {
            writeln!(content, "[unused{}]", i).unwrap();
        }
        writeln!(content, "[UNK]").unwrap(); // ID 100
        writeln!(content, "[CLS]").unwrap(); // ID 101
        writeln!(content, "[SEP]").unwrap(); // ID 102
        writeln!(content, "[MASK]").unwrap(); // ID 103
        for i in 104..1000 {
            writeln!(content, "[unused{}]", i).unwrap();
        }
        // ID 1000+: real tokens
        let words = [
            "the",
            "a",
            "is",
            "of",
            "and",
            "to",
            "in",
            "for",
            "that",
            "it",
            "hello",
            "world",
            "test",
            "input",
            "embedding",
            "model",
            "token",
            "##s",
            "##ing",
            "##ed",
            "##er",
            "##tion",
            "##ly",
            "##ize",
            ".",
            ",",
            "!",
            "?",
            "quick",
            "brown",
            "fox",
            "dog",
            "cat",
            "rust",
            "code",
            "function",
            "struct",
            "pub",
            "async",
            "fn",
        ];
        for w in &words {
            writeln!(content, "{}", w).unwrap();
        }
        // Pad to >1000 entries total.
        for i in 0..100 {
            writeln!(content, "extra{}", i).unwrap();
        }

        // Unique path per call. All tests in this binary share one
        // process, so a PID-only filename made them race on a single
        // file: `fs::write` truncates before writing, and a concurrent
        // `load()` could read the half-written file (<1000 entries) and
        // return None — the wordpiece tests then flaked under parallel
        // execution. A per-call counter gives each test its own file.
        static VOCAB_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = VOCAB_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "clawft_test_vocab_{}_{}.txt",
            std::process::id(),
            seq
        ));
        std::fs::write(&path, &content).expect("failed to write test vocab");
        path
    }

    #[test]
    fn wordpiece_load_valid_vocab() {
        let f = make_test_vocab();
        let tok = WordPieceTokenizer::load(&f);
        assert!(tok.is_some(), "should load a vocab with >1000 entries");
    }

    #[test]
    fn wordpiece_load_missing_file() {
        let tok = WordPieceTokenizer::load(Path::new("/nonexistent/vocab.txt"));
        assert!(tok.is_none());
    }

    #[test]
    fn wordpiece_encode_produces_cls_sep() {
        let f = make_test_vocab();
        let tok = WordPieceTokenizer::load(&f).unwrap().with_max_length(32);
        let (ids, mask, types) = tok.encode("hello world").unwrap();
        assert_eq!(ids.len(), 32, "should be padded to max_length");
        assert_eq!(ids[0], CLS_ID, "first token must be [CLS]");
        // Find [SEP] -- it should be after the content tokens.
        let sep_pos = ids.iter().position(|&x| x == SEP_ID);
        assert!(sep_pos.is_some(), "must contain [SEP]");
        let sep_pos = sep_pos.unwrap();
        assert!(
            sep_pos >= 2,
            "[SEP] should come after at least one content token"
        );
        // Attention mask: 1s up to and including [SEP], then 0s.
        assert_eq!(mask[0], 1);
        assert_eq!(mask[sep_pos], 1);
        if sep_pos + 1 < 32 {
            assert_eq!(mask[sep_pos + 1], 0, "padding should have mask=0");
        }
        // Token type IDs should all be 0 for single-sentence input.
        assert!(types.iter().all(|&t| t == 0));
    }

    #[test]
    fn wordpiece_encode_known_tokens() {
        let f = make_test_vocab();
        let tok = WordPieceTokenizer::load(&f).unwrap().with_max_length(16);
        let (ids, _, _) = tok.encode("hello").unwrap();
        // "hello" is in our test vocab -- should NOT be [UNK].
        let content_ids: Vec<i64> = ids[1..]
            .iter()
            .take_while(|&&x| x != SEP_ID)
            .cloned()
            .collect();
        assert!(
            !content_ids.is_empty(),
            "should tokenize 'hello' to at least one token"
        );
        assert!(
            content_ids.iter().any(|&id| id != UNK_ID),
            "known word 'hello' should not be all [UNK]"
        );
    }

    #[test]
    fn wordpiece_encode_unknown_token_uses_unk() {
        let f = make_test_vocab();
        let tok = WordPieceTokenizer::load(&f).unwrap().with_max_length(16);
        let (ids, _, _) = tok.encode("xyzzyplugh").unwrap();
        // "xyzzyplugh" is not in our vocab, so it should produce [UNK].
        let content_ids: Vec<i64> = ids[1..]
            .iter()
            .take_while(|&&x| x != SEP_ID)
            .cloned()
            .collect();
        assert!(
            content_ids.contains(&UNK_ID),
            "unknown word should produce [UNK] token"
        );
    }

    #[test]
    fn wordpiece_encode_truncates_long_input() {
        let f = make_test_vocab();
        let tok = WordPieceTokenizer::load(&f).unwrap().with_max_length(8);
        // Input much longer than max_length=8.
        let long_input = "the quick brown fox hello world test input embedding model";
        let (ids, mask, _) = tok.encode(long_input).unwrap();
        assert_eq!(ids.len(), 8, "output must be exactly max_length");
        assert_eq!(mask.len(), 8);
        assert_eq!(ids[0], CLS_ID);
        // [SEP] must be present.
        assert!(ids.contains(&SEP_ID));
    }

    #[test]
    fn wordpiece_encode_empty_input() {
        let f = make_test_vocab();
        let tok = WordPieceTokenizer::load(&f).unwrap().with_max_length(16);
        let (ids, mask, _) = tok.encode("").unwrap();
        assert_eq!(ids[0], CLS_ID);
        assert_eq!(ids[1], SEP_ID);
        // Rest should be padding.
        assert!(ids[2..].iter().all(|&x| x == PAD_ID));
        assert_eq!(mask[0], 1);
        assert_eq!(mask[1], 1);
        assert!(mask[2..].iter().all(|&x| x == 0));
    }

    #[test]
    fn wordpiece_pre_tokenize_punctuation() {
        let f = make_test_vocab();
        let tok = WordPieceTokenizer::load(&f).unwrap();
        let words = tok.pre_tokenize("Hello, World!");
        // Should split into: ["hello", ",", "world", "!"]
        assert!(words.contains(&",".to_string()));
        assert!(words.contains(&"!".to_string()));
        assert!(words.contains(&"hello".to_string()));
        assert!(words.contains(&"world".to_string()));
    }

    #[test]
    fn wordpiece_subword_splitting() {
        let f = make_test_vocab();
        let tok = WordPieceTokenizer::load(&f).unwrap();
        // "tokens" should split into "token" + "##s" since both are in vocab.
        let ids = tok.wordpiece_split("tokens");
        // If "token" and "##s" are in the vocab, we should get 2 IDs (neither UNK).
        assert!(!ids.is_empty(), "should produce at least one subword token");
    }

    #[test]
    fn wordpiece_deterministic() {
        let f = make_test_vocab();
        let tok = WordPieceTokenizer::load(&f).unwrap().with_max_length(32);
        let (ids1, _, _) = tok.encode("the quick brown fox").unwrap();
        let (ids2, _, _) = tok.encode("the quick brown fox").unwrap();
        assert_eq!(ids1, ids2, "encoding must be deterministic");
    }

    #[test]
    fn vocab_search_paths_finds_sibling() {
        let paths = vocab_search_paths(Path::new("/models/all-MiniLM-L6-v2.onnx"));
        assert!(paths.iter().any(|p| p.ends_with("vocab.txt")));
        assert!(
            paths
                .iter()
                .any(|p| p.to_string_lossy().contains("all-MiniLM-L6-v2/vocab.txt")),
            "should check sibling directory: {:?}",
            paths
        );
    }
}
