//! The two non-model embedding providers, both explicit.
//!
//! - [`UnavailableEmbeddingProvider`]: what boot paths hold when no real
//!   embedding model could be found. Every call fails with
//!   [`EmbeddingError::ModelUnavailable`] naming what was searched. It exists so
//!   an offline boot succeeds without quietly substituting hashed vectors.
//! - [`LexicalEmbeddingProvider`]: signed hashed bag-of-words. It matches on
//!   shared tokens and nothing else, so it is a keyword index in vector form,
//!   not a semantic embedder. It is only ever built on explicit opt-in
//!   (`WEFTOS_EMBEDDING_MODE=lexical`) and logs once that results are lexical.

use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use sha2::{Digest, Sha256};

use crate::embedding::{EmbeddingError, EmbeddingProvider};

/// Fails every call; see the module docs.
pub struct UnavailableEmbeddingProvider {
    dims: usize,
    reason: String,
}

impl UnavailableEmbeddingProvider {
    /// Width reported by [`EmbeddingProvider::dimensions`] (nothing is ever produced).
    pub const DEFAULT_DIMS: usize = 384;

    /// `reason` is the full "semantic search unavailable ..." message.
    pub fn new(dims: usize, reason: impl Into<String>) -> Self {
        Self { dims, reason: reason.into() }
    }

    fn error(&self) -> EmbeddingError {
        EmbeddingError::ModelUnavailable {
            what: "any embedding model".to_string(),
            detail: self.reason.clone(),
        }
    }
}

#[async_trait]
impl EmbeddingProvider for UnavailableEmbeddingProvider {
    async fn embed(&self, _text: &str) -> Result<Vec<f32>, EmbeddingError> {
        Err(self.error())
    }

    async fn embed_batch(&self, _texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        Err(self.error())
    }

    fn dimensions(&self) -> usize {
        self.dims
    }

    fn model_name(&self) -> &str {
        "unavailable"
    }

    /// Nothing to warm; do not log an error per boot beyond the selection log.
    async fn warm(&self) {}
}

static LEXICAL_WARNED: AtomicBool = AtomicBool::new(false);

/// Signed hashed bag-of-words embedder (explicit opt-in only).
pub struct LexicalEmbeddingProvider {
    dims: usize,
}

impl LexicalEmbeddingProvider {
    /// Default width.
    pub const DEFAULT_DIMS: usize = 384;

    /// Build a lexical provider. Logs once per process that results are
    /// lexical, not semantic.
    pub fn new(dims: usize) -> Self {
        if !LEXICAL_WARNED.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                "embedding mode is LEXICAL (WEFTOS_EMBEDDING_MODE=lexical): retrieval matches shared \
                 words only and is NOT semantic; configure an embedding model for semantic search"
            );
        }
        Self { dims: dims.max(8) }
    }

    fn embed_text(&self, text: &str) -> Vec<f32> {
        let mut v = vec![0.0f32; self.dims];
        for tok in text
            .to_lowercase()
            .split(|c: char| !c.is_alphanumeric())
            .filter(|t| t.chars().count() >= 2)
        {
            let h = Sha256::digest(tok.as_bytes());
            let idx = u32::from_le_bytes([h[0], h[1], h[2], h[3]]) as usize % self.dims;
            let sign = if h[4] & 1 == 0 { 1.0 } else { -1.0 };
            v[idx] += sign;
        }
        let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if n > 0.0 {
            v.iter_mut().for_each(|x| *x /= n);
        }
        v
    }
}

#[async_trait]
impl EmbeddingProvider for LexicalEmbeddingProvider {
    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        Ok(self.embed_text(text))
    }

    fn dimensions(&self) -> usize {
        self.dims
    }

    fn model_name(&self) -> &str {
        "lexical-hash"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unavailable_provider_errors_and_names_the_reason() {
        let p = UnavailableEmbeddingProvider::new(384, "searched: /nowhere/onnx/model.onnx");
        for err in [
            p.embed("x").await.unwrap_err(),
            p.embed_batch(&["x"]).await.unwrap_err(),
            p.embed_query("x").await.unwrap_err(),
        ] {
            assert!(matches!(err, EmbeddingError::ModelUnavailable { .. }));
            let msg = err.to_string();
            assert!(msg.contains("semantic search unavailable"), "{msg}");
            assert!(msg.contains("/nowhere/onnx/model.onnx"), "{msg}");
        }
        assert_eq!(p.model_name(), "unavailable");
    }

    /// Building the explicit lexical provider logs the one-time "results are
    /// lexical, not semantic" warning (the flag records that it was logged).
    #[test]
    fn explicit_lexical_mode_warns_once_that_results_are_lexical() {
        let _ = LexicalEmbeddingProvider::new(64);
        assert!(LEXICAL_WARNED.load(Ordering::Relaxed), "the lexical warning must have been logged");
    }

    #[tokio::test]
    async fn lexical_provider_is_named_lexical_and_matches_shared_words_only() {
        let p = LexicalEmbeddingProvider::new(256);
        assert_eq!(p.model_name(), "lexical-hash");
        let dot = |a: &[f32], b: &[f32]| a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>();
        let a = p.embed("rotate the signing key").await.unwrap();
        let same_words = p.embed("the key signing rotate").await.unwrap();
        let synonyms = p.embed("renew the cryptographic credential").await.unwrap();
        assert!(dot(&a, &same_words) > 0.99);
        // No shared word, no similarity: this is what "lexical, not semantic" means.
        assert!(dot(&a, &synonyms).abs() < 0.5, "a synonym must not look similar");
    }
}
