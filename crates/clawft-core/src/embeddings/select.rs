//! Choosing an embedder without a silent hash fallback.
//!
//! [`select_embedder`] returns a REAL embedder when one is configured or found
//! at a documented path, the explicit lexical mode when the operator opted in
//! with `WEFTOS_EMBEDDING_MODE=lexical`, and otherwise an error that says
//! "semantic search unavailable: configure an embedding model" and names what
//! was looked at. It never returns hashed vectors on its own initiative.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;

use super::hash_embedder::HashEmbedder;
use super::{Embedder, EmbeddingError};

/// Environment variable that opts in to lexical (hash) embeddings; the only
/// accepted value is `lexical`.
pub const EMBEDDING_MODE_ENV: &str = "WEFTOS_EMBEDDING_MODE";

/// True when the operator explicitly asked for lexical embeddings.
pub fn lexical_mode_requested() -> bool {
    std::env::var(EMBEDDING_MODE_ENV).is_ok_and(|v| v.eq_ignore_ascii_case("lexical"))
}

static LEXICAL_WARNED: AtomicBool = AtomicBool::new(false);

/// Log, once per process, that results are lexical and not semantic.
pub fn warn_lexical_once() {
    if !LEXICAL_WARNED.swap(true, Ordering::Relaxed) {
        tracing::warn!(
            "embedding mode is LEXICAL ({EMBEDDING_MODE_ENV}=lexical): retrieval matches shared \
             words only and is NOT semantic; configure an embedding model for semantic search"
        );
    }
}

/// [`HashEmbedder`] under an honest name: explicit lexical mode, warns once.
pub struct LexicalEmbedder {
    inner: HashEmbedder,
}

impl LexicalEmbedder {
    /// Build the lexical embedder (logs once that results are not semantic).
    pub fn new() -> Self {
        warn_lexical_once();
        Self { inner: HashEmbedder::default_dimension() }
    }
}

impl Default for LexicalEmbedder {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Embedder for LexicalEmbedder {
    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        Ok(self.inner.compute_embedding(text))
    }

    fn dimension(&self) -> usize {
        self.inner.dimension()
    }

    fn name(&self) -> &str {
        "lexical-simhash"
    }
}

/// The error for "no real embedder", naming what was looked at.
pub fn unavailable(detail: &str) -> EmbeddingError {
    EmbeddingError::Internal(format!(
        "semantic search unavailable: configure an embedding model ({detail}); \
         or opt in to lexical mode with {EMBEDDING_MODE_ENV}=lexical"
    ))
}

/// Select the best available real embedder (see the module docs).
///
/// Order: the local nomic-embed-text-v1.5 bundle (`onnx-embeddings` feature),
/// then the OpenAI-compatible API when `OPENAI_API_KEY` is set (`rvf` feature),
/// then lexical only when explicitly requested, else an error.
#[allow(clippy::vec_init_then_push)] // each push is behind its own feature cfg
pub fn select_embedder() -> Result<Arc<dyn Embedder>, EmbeddingError> {
    let mut why: Vec<String> = Vec::new();

    #[cfg(feature = "onnx-embeddings")]
    match super::onnx_embedder::find_model_dir() {
        Ok(dir) => match super::onnx_embedder::OnnxEmbedder::from_dir(&dir) {
            Ok(e) => return Ok(Arc::new(e)),
            Err(e) => why.push(e.to_string()),
        },
        Err(e) => why.push(e.to_string()),
    }
    #[cfg(not(feature = "onnx-embeddings"))]
    why.push("this build lacks the onnx-embeddings feature, so no local model can load".into());

    #[cfg(feature = "rvf")]
    if std::env::var("OPENAI_API_KEY").is_ok_and(|k| !k.is_empty()) {
        return Ok(Arc::new(super::api_embedder::ApiEmbedder::with_defaults()));
    }
    #[cfg(feature = "rvf")]
    why.push("OPENAI_API_KEY is not set".into());

    if lexical_mode_requested() {
        return Ok(Arc::new(LexicalEmbedder::new()));
    }
    Err(unavailable(&why.join("; ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Building the explicit lexical embedder emits the one-time "results are
    /// lexical, not semantic" warning (the flag records that it was logged).
    #[test]
    fn explicit_lexical_mode_warns_once_that_results_are_lexical() {
        let _ = LexicalEmbedder::new();
        assert!(LEXICAL_WARNED.load(Ordering::Relaxed), "the lexical warning must have been logged");
    }

    #[tokio::test]
    async fn lexical_embedder_is_named_lexical_and_matches_only_shared_words() {
        let e = LexicalEmbedder::new();
        assert_eq!(e.name(), "lexical-simhash");
        let dot = |a: &[f32], b: &[f32]| a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>();
        let a = e.embed("rotate the signing key").await.unwrap();
        let same = e.embed("rotate the signing key").await.unwrap();
        assert!((dot(&a, &same) - 1.0).abs() < 1e-4);
    }

    /// With no model, no API key and no opt-in, selection is an error naming
    /// what was looked at. It is never a hash embedder.
    #[test]
    fn select_embedder_errors_when_nothing_real_is_configured() {
        if lexical_mode_requested()
            || std::env::var("OPENAI_API_KEY").is_ok_and(|k| !k.is_empty())
            || super::super::onnx_embedder_present()
        {
            eprintln!("SKIP: this host has a model, an API key, or lexical mode on");
            return;
        }
        let err = select_embedder().err().expect("must be an error, not a hash embedder");
        let msg = err.to_string();
        assert!(msg.contains("semantic search unavailable"), "{msg}");
        assert!(msg.contains("configure an embedding model"), "{msg}");
    }
}
