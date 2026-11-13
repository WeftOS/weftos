//! Pluggable embedding backends for ECC vector operations (K3c-G2).
//!
//! Provides the [`EmbeddingProvider`] trait that the [`WeaverEngine`] uses
//! to convert text into vector embeddings for HNSW storage and similarity
//! search. Ships with [`MockEmbeddingProvider`] for deterministic testing.

use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// EmbeddingError
// ---------------------------------------------------------------------------

/// Errors that embedding backends may produce.
#[non_exhaustive]
#[derive(Debug)]
pub enum EmbeddingError {
    /// The underlying model has not been loaded yet.
    ModelNotLoaded,
    /// Vector dimensionality does not match the expected value.
    DimensionMismatch {
        /// Expected dimensionality.
        expected: usize,
        /// Actual dimensionality returned.
        got: usize,
    },
    /// Generic backend failure.
    BackendError(String),
    /// A real embedding model was requested (or is the only acceptable kind)
    /// but is not available. There is no silent fallback to hashing: the
    /// message names what was looked for and where.
    ModelUnavailable {
        /// What was being loaded (provider / model name).
        what: String,
        /// Why it is unavailable, naming the paths searched.
        detail: String,
    },
    /// Rate-limited; caller should retry after the given duration.
    RateLimited {
        /// How long to wait before retrying.
        retry_after: Duration,
    },
}

impl fmt::Display for EmbeddingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ModelNotLoaded => write!(f, "embedding model not loaded"),
            Self::DimensionMismatch { expected, got } => {
                write!(f, "dimension mismatch: expected {expected}, got {got}")
            }
            Self::BackendError(msg) => write!(f, "embedding backend error: {msg}"),
            Self::ModelUnavailable { what, detail } => write!(
                f,
                "semantic search unavailable: embedding model `{what}` is not available ({detail}); \
                 configure an embedding model, or opt in to lexical mode with WEFTOS_EMBEDDING_MODE=lexical"
            ),
            Self::RateLimited { retry_after } => {
                write!(f, "rate limited, retry after {}ms", retry_after.as_millis())
            }
        }
    }
}

impl std::error::Error for EmbeddingError {}

// ---------------------------------------------------------------------------
// EmbeddingProvider trait
// ---------------------------------------------------------------------------

/// Trait for pluggable embedding backends.
///
/// Implementations convert text into fixed-dimensionality float vectors
/// suitable for HNSW indexing and cosine similarity search.
#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    /// Embed a single text chunk into a vector.
    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbeddingError>;

    /// Embed a batch of text chunks.
    ///
    /// The default implementation calls [`embed`](Self::embed) in a loop.
    /// Backends that support native batching should override this for
    /// efficiency.
    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        let mut results = Vec::with_capacity(texts.len());
        for text in texts {
            results.push(self.embed(text).await?);
        }
        Ok(results)
    }

    /// Dimensionality of the output vectors.
    fn dimensions(&self) -> usize;

    /// Name of the embedding model (for metadata tracking).
    fn model_name(&self) -> &str;

    /// Embed a **query** (asymmetric retrieval lane).
    ///
    /// Default = [`embed`](Self::embed) (symmetric backends: Mock, MiniLM, LLM).
    /// Providers with asymmetric training (e5 `query:` / `passage:`, Qwen3
    /// instruction prefix) **must** override so graft/search queries land in the
    /// correct half of the space (WEFT-640 / ADR-059).
    async fn embed_query(&self, query: &str) -> Result<Vec<f32>, EmbeddingError> {
        self.embed(query).await
    }

    /// Pre-warm the backend: run one throwaway [`embed`](Self::embed) so the
    /// model graph, runtime thread pool, and allocator are hot before the first
    /// real call. Best-effort — errors are swallowed (a backend with nothing to
    /// warm, like the Mock, just pays one cheap call). Intended to be called
    /// once at daemon startup so the first conversation turn doesn't pay the
    /// model's first-inference cost (ADR-058 Phase 5.3; mirrors the 6.2 STT
    /// pre-warm).
    async fn warm(&self) {
        let _ = self.embed("warm").await;
    }
}

// ---------------------------------------------------------------------------
// MockEmbeddingProvider
// ---------------------------------------------------------------------------

/// Deterministic embedding provider for **plumbing tests only**.
///
/// Produces vectors derived from a SHA-256 hash of the input text, so equal
/// text maps to equal vectors and nothing else. It carries NO similarity
/// signal and must never be used to judge retrieval or recall (owner rule
/// 2026-11-11): use a real model (`embedding_support`) for that. It is not
/// selected by [`select_embedding_provider`] under any circumstances.
pub struct MockEmbeddingProvider {
    /// Output vector dimensionality.
    pub dims: usize,
}

impl MockEmbeddingProvider {
    /// Create a mock provider with the given output dimensionality.
    pub fn new(dims: usize) -> Self {
        Self { dims }
    }

    /// Deterministic hash-based embedding generation.
    fn hash_embed(&self, text: &str) -> Vec<f32> {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(text.as_bytes());
        let hash = hasher.finalize();

        let mut vec = Vec::with_capacity(self.dims);
        for i in 0..self.dims {
            // Cycle through hash bytes, normalise to [-1, 1]
            let byte = hash[i % 32];
            vec.push((byte as f32 / 128.0) - 1.0);
        }
        vec
    }
}

#[async_trait]
impl EmbeddingProvider for MockEmbeddingProvider {
    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        Ok(self.hash_embed(text))
    }

    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        Ok(texts.iter().map(|t| self.hash_embed(t)).collect())
    }

    fn dimensions(&self) -> usize {
        self.dims
    }

    fn model_name(&self) -> &str {
        "mock-sha256"
    }
}

// ---------------------------------------------------------------------------
// LlmEmbeddingProvider
// ---------------------------------------------------------------------------

/// Configuration for the LLM API embedding backend.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmEmbeddingConfig {
    /// Model identifier (e.g., "text-embedding-3-small").
    pub model: String,
    /// Output vector dimensionality (e.g., 384 or 1536).
    pub dimensions: usize,
    /// Maximum texts per API call for batching.
    pub batch_size: usize,
    /// Whether the API is currently available.
    pub api_available: bool,
}

impl Default for LlmEmbeddingConfig {
    fn default() -> Self {
        Self {
            model: "text-embedding-3-small".to_string(),
            dimensions: 384,
            batch_size: 16,
            api_available: false,
        }
    }
}

/// LLM-backed embedding provider that calls the clawft-llm provider layer.
///
/// Uses the model's embedding endpoint to produce real semantic vectors.
/// When no API is configured (or the API is unavailable) every call returns
/// [`EmbeddingError::ModelUnavailable`]; there is no hash fallback.
pub struct LlmEmbeddingProvider {
    config: LlmEmbeddingConfig,
}

impl LlmEmbeddingProvider {
    /// Create a new LLM embedding provider with the given configuration.
    pub fn new(config: LlmEmbeddingConfig) -> Self {
        Self { config }
    }

    /// Create from a weave.toml-style configuration table.
    ///
    /// Expected keys: `model` (string), `dimensions` (int), `batch_size` (int).
    /// If the table is missing or incomplete, returns a provider with defaults
    /// whose calls fail with `ModelUnavailable` until the API is configured.
    pub fn from_config(table: &std::collections::HashMap<String, String>) -> Self {
        let model = table
            .get("model")
            .cloned()
            .unwrap_or_else(|| "text-embedding-3-small".to_string());
        let dimensions = table
            .get("dimensions")
            .and_then(|d| d.parse::<usize>().ok())
            .unwrap_or(384);
        let batch_size = table
            .get("batch_size")
            .and_then(|b| b.parse::<usize>().ok())
            .unwrap_or(16);
        let api_available = table
            .get("api_available")
            .map(|v| v == "true")
            .unwrap_or(false);

        Self::new(LlmEmbeddingConfig {
            model,
            dimensions,
            batch_size,
            api_available,
        })
    }

    /// Whether the LLM API is available (non-fallback mode).
    pub fn is_api_available(&self) -> bool {
        self.config.api_available
    }

    /// Get the underlying configuration.
    pub fn config(&self) -> &LlmEmbeddingConfig {
        &self.config
    }

    fn unavailable(&self) -> EmbeddingError {
        EmbeddingError::ModelUnavailable {
            what: self.config.model.clone(),
            detail: "LLM embedding API is not configured (api_available = false)".to_string(),
        }
    }

    /// Perform an LLM API embedding call.
    ///
    /// In a production deployment this would call the clawft-llm provider's
    /// embed endpoint. Until that is wired it returns an error, which `embed()`
    /// surfaces to the caller unchanged.
    async fn call_llm_api(&self, _text: &str) -> Result<Vec<f32>, EmbeddingError> {
        if !self.config.api_available {
            return Err(self.unavailable());
        }
        // Production implementation would call:
        //   provider.embed(EmbedRequest { model, input: vec![text], dimensions })
        // For now, the API path is gated behind api_available.
        Err(EmbeddingError::BackendError(
            "LLM API call not yet wired to clawft-llm provider".to_string(),
        ))
    }

    /// Perform a batched LLM API embedding call.
    async fn call_llm_api_batch(&self, _texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        if !self.config.api_available {
            return Err(self.unavailable());
        }
        Err(EmbeddingError::BackendError(
            "LLM API batch call not yet wired to clawft-llm provider".to_string(),
        ))
    }
}

#[async_trait]
impl EmbeddingProvider for LlmEmbeddingProvider {
    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        let vec = self.call_llm_api(text).await?;
        if vec.len() != self.config.dimensions {
            return Err(EmbeddingError::DimensionMismatch {
                expected: self.config.dimensions,
                got: vec.len(),
            });
        }
        Ok(vec)
    }

    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        let vecs = self.call_llm_api_batch(texts).await?;
        for v in &vecs {
            if v.len() != self.config.dimensions {
                return Err(EmbeddingError::DimensionMismatch {
                    expected: self.config.dimensions,
                    got: v.len(),
                });
            }
        }
        Ok(vecs)
    }

    fn dimensions(&self) -> usize {
        self.config.dimensions
    }

    fn model_name(&self) -> &str {
        &self.config.model
    }
}

// ---------------------------------------------------------------------------
// select_embedding_provider
// ---------------------------------------------------------------------------

/// Environment variable that opts in to lexical (hash) embeddings. The only
/// accepted value is `lexical`. Without it a missing model is an error.
pub const EMBEDDING_MODE_ENV: &str = "WEFTOS_EMBEDDING_MODE";

/// Environment variable naming a nomic-embed-text-v1.5 bundle directory
/// (holding `onnx/model.onnx` and `tokenizer.json`).
pub const EMBED_MODEL_DIR_ENV: &str = "WEFTOS_EMBED_MODEL_DIR";

/// Select the best available **real** embedding provider.
///
/// Priority order (ADR-059 + WEFT-640):
/// 1. **Qwen3-Embedding-0.6B** (`model_fp16.onnx` + `tokenizer.json`).
/// 2. **nomic-embed-text-v1.5** (768-d; `onnx/model.onnx` + `tokenizer.json`
///    under `$WEFTOS_EMBED_MODEL_DIR`, `.weftos/models/nomic-embed-text-v1.5/`
///    or `$HOME/.weftos/models/nomic-embed-text-v1.5/`).
/// 3. **e5-small-v2** ONNX (384-d).
/// 4. **all-MiniLM-L6-v2** ONNX (legacy BERT encoder).
/// 5. **LLM API**, only when its config says the API is available.
/// 6. **Lexical**, only when `WEFTOS_EMBEDDING_MODE=lexical` is set; logs once
///    that results are lexical, not semantic.
///
/// There is no silent fallback. When nothing above is live the result is
/// `Err(EmbeddingError::ModelUnavailable)` naming every path searched. A model
/// provider is only returned when its runtime actually loaded.
pub fn select_embedding_provider(
    llm_config: Option<LlmEmbeddingConfig>,
) -> Result<Box<dyn EmbeddingProvider>, EmbeddingError> {
    let mut searched: Vec<String> = Vec::new();

    // 1. Qwen3 (preferred / project-canonical): needs model_fp16.onnx + tokenizer.json.
    for dir in qwen3_model_search_dirs() {
        let model = dir.join("model_fp16.onnx");
        let tokenizer = dir.join("tokenizer.json");
        searched.push(model.display().to_string());
        if model.exists() && tokenizer.exists() {
            let provider = crate::embedding_qwen3::Qwen3EmbeddingProvider::new(&model, &tokenizer);
            if provider.is_runtime_available() {
                tracing::info!("Using Qwen3 embedding provider from {}", dir.display());
                return Ok(Box::new(provider));
            }
            searched.push(format!("{} (present but failed to load)", model.display()));
        }
    }

    // 2. nomic-embed-text-v1.5: 768-d, search_document:/search_query: prefixes.
    for dir in nomic_model_search_dirs() {
        let model = dir.join("onnx/model.onnx");
        searched.push(model.display().to_string());
        if model.exists() {
            let provider = crate::embedding_onnx::OnnxEmbeddingProvider::nomic_embed_v1_5(&dir);
            if provider.is_runtime_available() {
                tracing::info!("Using nomic-embed-text-v1.5 embedding provider from {}", dir.display());
                return Ok(Box::new(provider));
            }
            searched.push(format!("{} (present but failed to load)", model.display()));
        }
    }

    // 3. e5-small-v2 (WEFT-640): 384-d, query:/passage: asymmetric prefixes.
    for path in &e5_model_search_paths() {
        if path.exists() {
            let provider = crate::embedding_e5::E5EmbeddingProvider::new(path);
            if provider.is_runtime_available() {
                tracing::info!("Using e5-small-v2 embedding provider from {}", path.display());
                return Ok(Box::new(provider));
            }
            searched.push(format!("{} (present but failed to load)", path.display()));
        }
    }

    // 4. all-MiniLM-L6-v2 ONNX (legacy BERT encoder).
    for path in &onnx_model_search_paths() {
        if path.exists() {
            let provider = crate::embedding_onnx::OnnxEmbeddingProvider::new(path);
            if provider.is_runtime_available() {
                tracing::info!("Using ONNX embedding provider from {}", path.display());
                return Ok(Box::new(provider));
            }
            searched.push(format!("{} (present but failed to load)", path.display()));
        }
    }

    // 5. LLM API: only when it is actually available.
    if let Some(config) = llm_config {
        if config.api_available {
            return Ok(Box::new(LlmEmbeddingProvider::new(config)));
        }
        searched.push(format!("LLM embedding API `{}` (api_available = false)", config.model));
    }

    // 6. Explicit opt-in lexical mode.
    if lexical_mode_requested() {
        return Ok(Box::new(crate::embedding_unavailable::LexicalEmbeddingProvider::new(
            crate::embedding_unavailable::LexicalEmbeddingProvider::DEFAULT_DIMS,
        )));
    }

    let mut detail = format!("searched: {}", searched.join(", "));
    if !cfg!(feature = "onnx-embeddings") {
        detail.push_str("; this build lacks the onnx-embeddings feature, so no local model can load");
    }
    Err(EmbeddingError::ModelUnavailable { what: "any embedding model".to_string(), detail })
}

/// Like [`select_embedding_provider`], for boot paths that must not fail
/// offline. When no real model is available it logs an error and returns an
/// [`UnavailableEmbeddingProvider`](crate::embedding_unavailable::UnavailableEmbeddingProvider)
/// whose every call fails with `ModelUnavailable`; it never returns a hashing
/// provider unless lexical mode was opted in explicitly.
pub fn select_embedding_provider_or_unavailable(
    llm_config: Option<LlmEmbeddingConfig>,
) -> Box<dyn EmbeddingProvider> {
    match select_embedding_provider(llm_config) {
        Ok(p) => p,
        Err(e) => {
            tracing::error!("{e}");
            Box::new(crate::embedding_unavailable::UnavailableEmbeddingProvider::new(
                crate::embedding_unavailable::UnavailableEmbeddingProvider::DEFAULT_DIMS,
                e.to_string(),
            ))
        }
    }
}

/// True when the operator explicitly asked for lexical (hash) embeddings.
pub fn lexical_mode_requested() -> bool {
    std::env::var(EMBEDDING_MODE_ENV).is_ok_and(|v| v.eq_ignore_ascii_case("lexical"))
}

/// Search directories for a nomic-embed-text-v1.5 bundle (each expected to hold
/// `onnx/model.onnx` + `tokenizer.json`).
pub fn nomic_model_search_dirs() -> Vec<std::path::PathBuf> {
    let subdir = "nomic-embed-text-v1.5";
    let mut dirs = Vec::new();
    if let Ok(env_dir) = std::env::var(EMBED_MODEL_DIR_ENV)
        && !env_dir.is_empty()
    {
        dirs.push(std::path::PathBuf::from(env_dir));
    }
    dirs.push(std::path::PathBuf::from(format!(".weftos/models/{subdir}")));
    if let Some(home) = dirs_home() {
        dirs.push(home.join(format!(".weftos/models/{subdir}")));
    }
    if let Ok(env_path) = std::env::var("WEFTOS_MODEL_PATH") {
        dirs.push(std::path::PathBuf::from(env_path).join(subdir));
    }
    dirs
}

/// Standard search directories for the Qwen3 model bundle (each expected to hold
/// `model_fp16.onnx` + `tokenizer.json`):
/// 1. `.weftos/models/Qwen3-Embedding-0.6B/` (project-local)
/// 2. `$HOME/.weftos/models/Qwen3-Embedding-0.6B/` (user-global)
/// 3. `$WEFTOS_MODEL_PATH` (as a models-root and as the bundle dir directly)
fn qwen3_model_search_dirs() -> Vec<std::path::PathBuf> {
    let subdir = "Qwen3-Embedding-0.6B";
    let mut dirs = Vec::new();

    dirs.push(std::path::PathBuf::from(format!(".weftos/models/{subdir}")));

    if let Some(home) = dirs_home() {
        dirs.push(home.join(format!(".weftos/models/{subdir}")));
    }

    if let Ok(env_path) = std::env::var("WEFTOS_MODEL_PATH") {
        let p = std::path::PathBuf::from(env_path);
        dirs.push(p.join(subdir));
        dirs.push(p);
    }

    dirs
}

/// Standard search paths for the ONNX embedding model.
///
/// Looks in (in order):
/// 1. `.weftos/models/all-MiniLM-L6-v2.onnx` (project-local)
/// 2. `$HOME/.weftos/models/all-MiniLM-L6-v2.onnx` (user-global)
/// 3. `$WEFTOS_MODEL_PATH` environment variable
fn onnx_model_search_paths() -> Vec<std::path::PathBuf> {
    let model_name = "all-MiniLM-L6-v2.onnx";
    let mut paths = Vec::new();

    // Project-local.
    paths.push(std::path::PathBuf::from(format!(
        ".weftos/models/{model_name}"
    )));

    // User-global.
    if let Some(home) = dirs_home() {
        paths.push(home.join(format!(".weftos/models/{model_name}")));
    }

    // Env override.
    if let Ok(env_path) = std::env::var("WEFTOS_MODEL_PATH") {
        paths.push(std::path::PathBuf::from(env_path));
    }

    paths
}

/// Search paths for the e5-small-v2 ONNX bundle (WEFT-640).
///
/// Each entry is a candidate **model file** path. Staging layout:
/// - `.weftos/models/e5-small-v2/model.onnx` (or `model_int8.onnx`)
/// - sibling `vocab.txt` for WordPiece (reuses MiniLM/BERT uncased vocab)
///
/// Order: project-local → user-global → `$WEFTOS_MODEL_PATH` (as models root
/// and as a direct file path).
fn e5_model_search_paths() -> Vec<std::path::PathBuf> {
    let subdir = "e5-small-v2";
    let files = ["model.onnx", "model_int8.onnx", "model_fp16.onnx"];
    let mut paths = Vec::new();

    let mut push_dir = |dir: std::path::PathBuf| {
        for f in files {
            paths.push(dir.join(f));
        }
    };

    push_dir(std::path::PathBuf::from(format!(".weftos/models/{subdir}")));
    if let Some(home) = dirs_home() {
        push_dir(home.join(format!(".weftos/models/{subdir}")));
    }
    if let Ok(env_path) = std::env::var("WEFTOS_MODEL_PATH") {
        let p = std::path::PathBuf::from(env_path);
        push_dir(p.join(subdir));
        // Direct file path override.
        paths.push(p.clone());
        for f in files {
            paths.push(p.join(f));
        }
    }

    paths
}

/// Get the user's home directory.
fn dirs_home() -> Option<std::path::PathBuf> {
    std::env::var("HOME").ok().map(std::path::PathBuf::from)
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_embed_returns_correct_dimensions() {
        let provider = MockEmbeddingProvider::new(64);
        let vec = provider.embed("hello world").await.unwrap();
        assert_eq!(vec.len(), 64);
    }

    #[tokio::test]
    async fn mock_embed_deterministic() {
        let provider = MockEmbeddingProvider::new(32);
        let v1 = provider.embed("test input").await.unwrap();
        let v2 = provider.embed("test input").await.unwrap();
        assert_eq!(v1, v2);
    }

    #[tokio::test]
    async fn mock_embed_different_inputs_differ() {
        let provider = MockEmbeddingProvider::new(32);
        let v1 = provider.embed("alpha").await.unwrap();
        let v2 = provider.embed("beta").await.unwrap();
        assert_ne!(v1, v2);
    }

    #[tokio::test]
    async fn mock_embed_batch() {
        let provider = MockEmbeddingProvider::new(16);
        let results = provider.embed_batch(&["a", "b", "c"]).await.unwrap();
        assert_eq!(results.len(), 3);
        for v in &results {
            assert_eq!(v.len(), 16);
        }
    }

    #[tokio::test]
    async fn mock_embed_batch_matches_individual() {
        let provider = MockEmbeddingProvider::new(8);
        let batch = provider.embed_batch(&["x", "y"]).await.unwrap();
        let x = provider.embed("x").await.unwrap();
        let y = provider.embed("y").await.unwrap();
        assert_eq!(batch[0], x);
        assert_eq!(batch[1], y);
    }

    #[test]
    fn mock_model_name() {
        let provider = MockEmbeddingProvider::new(16);
        assert_eq!(provider.model_name(), "mock-sha256");
    }

    #[test]
    fn mock_dimensions() {
        let provider = MockEmbeddingProvider::new(128);
        assert_eq!(provider.dimensions(), 128);
    }

    #[test]
    fn embedding_error_display() {
        let err = EmbeddingError::DimensionMismatch {
            expected: 384,
            got: 256,
        };
        assert!(err.to_string().contains("384"));
        assert!(err.to_string().contains("256"));

        let err2 = EmbeddingError::ModelNotLoaded;
        assert!(err2.to_string().contains("not loaded"));
    }

    // ── LlmEmbeddingProvider tests ───────────────────────────────────

    #[tokio::test]
    async fn llm_provider_errors_when_api_unavailable_and_never_hashes() {
        let config = LlmEmbeddingConfig {
            api_available: false,
            dimensions: 64,
            ..Default::default()
        };
        let provider = LlmEmbeddingProvider::new(config);
        let err = provider.embed("hello world").await.unwrap_err();
        assert!(matches!(err, EmbeddingError::ModelUnavailable { .. }), "{err}");
        assert!(err.to_string().contains("semantic search unavailable"));
        let err = provider.embed_batch(&["a", "b"]).await.unwrap_err();
        assert!(matches!(err, EmbeddingError::ModelUnavailable { .. }), "{err}");
    }

    #[test]
    fn llm_provider_reports_model_name() {
        let config = LlmEmbeddingConfig {
            model: "custom-embed-v1".to_string(),
            ..Default::default()
        };
        let provider = LlmEmbeddingProvider::new(config);
        assert_eq!(provider.model_name(), "custom-embed-v1");
    }

    #[test]
    fn llm_provider_reports_dimensions() {
        let config = LlmEmbeddingConfig {
            dimensions: 1536,
            ..Default::default()
        };
        let provider = LlmEmbeddingProvider::new(config);
        assert_eq!(provider.dimensions(), 1536);
    }

    #[test]
    fn llm_provider_api_availability_check() {
        let unavailable = LlmEmbeddingProvider::new(LlmEmbeddingConfig::default());
        assert!(!unavailable.is_api_available());

        let available = LlmEmbeddingProvider::new(LlmEmbeddingConfig {
            api_available: true,
            ..Default::default()
        });
        assert!(available.is_api_available());
    }

    #[test]
    fn llm_provider_from_config_defaults() {
        let table = std::collections::HashMap::new();
        let provider = LlmEmbeddingProvider::from_config(&table);
        assert_eq!(provider.dimensions(), 384);
        assert_eq!(provider.model_name(), "text-embedding-3-small");
        assert!(!provider.is_api_available());
    }

    #[test]
    fn llm_provider_from_config_custom() {
        let mut table = std::collections::HashMap::new();
        table.insert("model".to_string(), "my-model".to_string());
        table.insert("dimensions".to_string(), "768".to_string());
        table.insert("batch_size".to_string(), "32".to_string());
        table.insert("api_available".to_string(), "true".to_string());
        let provider = LlmEmbeddingProvider::from_config(&table);
        assert_eq!(provider.model_name(), "my-model");
        assert_eq!(provider.dimensions(), 768);
        assert_eq!(provider.config().batch_size, 32);
        assert!(provider.is_api_available());
    }

    /// True when this host has a model the selector could load, in which
    /// case the "nothing available" assertions below do not apply.
    fn host_has_a_model() -> bool {
        lexical_mode_requested()
            || nomic_model_search_dirs().iter().any(|d| d.join("onnx/model.onnx").exists())
            || qwen3_model_search_dirs().iter().any(|d| d.join("model_fp16.onnx").exists())
            || e5_model_search_paths().iter().any(|p| p.exists())
            || onnx_model_search_paths().iter().any(|p| p.exists())
    }

    #[test]
    fn select_provider_errors_naming_paths_when_no_model_is_available() {
        if host_has_a_model() {
            eprintln!("SKIP: this host has an embedding model; the no-model case does not apply");
            return;
        }
        let err = select_embedding_provider(None).err().expect("must be an error, not a hash provider");
        assert!(matches!(err, EmbeddingError::ModelUnavailable { .. }), "{err}");
        let msg = err.to_string();
        assert!(msg.contains("semantic search unavailable"), "{msg}");
        assert!(msg.contains("nomic-embed-text-v1.5"), "names the searched paths: {msg}");
    }

    #[test]
    fn select_provider_or_unavailable_never_returns_a_hashing_provider() {
        if host_has_a_model() {
            eprintln!("SKIP: this host has an embedding model");
            return;
        }
        let p = select_embedding_provider_or_unavailable(None);
        assert_eq!(p.model_name(), "unavailable");
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let err = rt.block_on(p.embed("anything")).unwrap_err();
        assert!(matches!(err, EmbeddingError::ModelUnavailable { .. }), "{err}");
    }

    #[test]
    fn nomic_search_dirs_include_the_users_default_bundle_location() {
        // Path strings only; nothing under the real home directory is read.
        let dirs = nomic_model_search_dirs();
        assert!(
            dirs.iter().any(|d| d.ends_with(".weftos/models/nomic-embed-text-v1.5")),
            "expected ~/.weftos/models/nomic-embed-text-v1.5 in the search path, got: {dirs:?}"
        );
    }

    #[test]
    fn qwen3_search_dirs_include_project_local_bundle() {
        let dirs = qwen3_model_search_dirs();
        assert!(
            dirs.iter()
                .any(|d| d.ends_with(".weftos/models/Qwen3-Embedding-0.6B")),
            "expected project-local Qwen3 bundle dir, got: {dirs:?}"
        );
    }

    #[test]
    fn e5_search_paths_include_project_local_model() {
        let paths = e5_model_search_paths();
        assert!(
            paths
                .iter()
                .any(|p| p.ends_with(".weftos/models/e5-small-v2/model.onnx")),
            "expected project-local e5 model path, got: {paths:?}"
        );
    }

    #[tokio::test]
    async fn mock_embed_query_defaults_to_embed() {
        let provider = MockEmbeddingProvider::new(16);
        let a = provider.embed("hello").await.unwrap();
        let b = provider.embed_query("hello").await.unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn select_provider_returns_llm_only_when_api_is_available() {
        if host_has_a_model() {
            eprintln!("SKIP: this host has an embedding model");
            return;
        }
        let config = LlmEmbeddingConfig {
            model: "test-embed".to_string(),
            dimensions: 256,
            api_available: true,
            ..Default::default()
        };
        let provider = select_embedding_provider(Some(config)).expect("api available");
        assert_eq!(provider.dimensions(), 256);
        assert_eq!(provider.model_name(), "test-embed");

        // An LLM config whose API is not available is NOT a provider.
        let down = LlmEmbeddingConfig { api_available: false, ..Default::default() };
        assert!(select_embedding_provider(Some(down)).is_err());
    }
}
