//! Embedding trait definitions and implementations.
//!
//! Provides the [`Embedder`] trait for generating vector embeddings from text,
//! the real in-process [`onnx_embedder::OnnxEmbedder`] (opt-in), and a
//! [`hash_embedder::HashEmbedder`] that uses SimHash for deterministic LEXICAL
//! vectors (equal words match; it is not semantic and is never a silent default).
//!
//! All types in this module are gated behind the `vector-memory` feature flag.

#[cfg(feature = "vector-memory")]
pub mod hash_embedder;
#[cfg(feature = "vector-memory")]
pub mod hnsw_store;
#[cfg(feature = "vector-memory")]
pub mod micro_hnsw;
/// Real in-process nomic-embed-text-v1.5 embedder (opt-in `onnx-embeddings`).
#[cfg(feature = "onnx-embeddings")]
pub mod onnx_embedder;
/// Embedder selection with no silent hash fallback.
#[cfg(feature = "vector-memory")]
pub mod select;
/// Shared real-embedder helper for recall tests (core, tools, cos).
#[cfg(feature = "vector-memory")]
pub mod test_support;
/// WEFT-385 / HNSW-EML #4: region → entry-node search-path prediction.
#[cfg(feature = "vector-memory")]
pub mod path_predict;
// `rvf_stub` has no dependency on the real `rvf-runtime` crate (just serde +
// tracing, both unconditional deps), so it is gated on the broader
// `vector-memory` feature rather than `rvf`. This keeps it available as the
// vector-memory backend for builds that can't link `rvf-runtime` (wasm,
// browser, no-default) -- see `rvf_backend` below for how the two
// implementations are selected.
#[cfg(feature = "vector-memory")]
pub mod rvf_stub;

#[cfg(feature = "rvf")]
pub mod api_embedder;
#[cfg(feature = "rvf")]
pub mod progressive;
#[cfg(feature = "rvf")]
pub mod quantization;
#[cfg(feature = "rvf")]
pub mod rvf_real;
#[cfg(feature = "rvf")]
pub mod witness;

// Historical note: a macOS `__errno_location` shim used to be required here
// (and was deliberately defined only once, in clawft-cow-memory, to avoid
// "symbol multiply defined" under fat-LTO -- WEFT-615/WEFT-662). rvf-runtime
// 0.3.2 fixed the underlying portability bug by cfg-gating `__error` for
// macOS, so no shim exists in either crate now. Do not reintroduce one.

/// Backend-selecting alias: real `rvf-runtime`-backed store when the `rvf`
/// feature is enabled (native builds that can link the crate), the in-memory
/// stub otherwise. Consumers should import [`RvfStore`]/[`RvfError`] from
/// here rather than reaching into `rvf_real`/`rvf_stub` directly, so they
/// automatically follow whichever backend the active feature set selects.
///
/// See `crates/clawft-core/src/embeddings/rvf_real.rs` module docs for the
/// on-disk layout and the real-vs-stub semantic gaps this bridges.
pub mod rvf_backend {
    #[cfg(feature = "rvf")]
    pub use super::rvf_real::{RvfEntry, RvfError, RvfQueryResult, RvfStore};
    #[cfg(all(feature = "vector-memory", not(feature = "rvf")))]
    pub use super::rvf_stub::{RvfEntry, RvfError, RvfQueryResult, RvfStore};
}

/// True when a local embedding model bundle is present on this host.
#[cfg(all(test, feature = "vector-memory"))]
pub(crate) fn onnx_embedder_present() -> bool {
    #[cfg(feature = "onnx-embeddings")]
    {
        onnx_embedder::find_model_dir().is_ok()
    }
    #[cfg(not(feature = "onnx-embeddings"))]
    {
        false
    }
}

use async_trait::async_trait;
use std::fmt;

/// Errors that can occur during embedding generation.
#[non_exhaustive]
#[derive(Debug)]
pub enum EmbeddingError {
    /// The input text could not be processed.
    InvalidInput(String),
    /// An internal error occurred in the embedder.
    Internal(String),
}

impl fmt::Display for EmbeddingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EmbeddingError::InvalidInput(msg) => write!(f, "invalid input: {msg}"),
            EmbeddingError::Internal(msg) => write!(f, "internal error: {msg}"),
        }
    }
}

impl std::error::Error for EmbeddingError {}

/// Trait for generating vector embeddings from text.
///
/// Implementations can be local (e.g. SimHash) or remote (e.g. OpenAI embeddings API).
/// All implementations must be `Send + Sync` for use across async tasks.
#[async_trait]
pub trait Embedder: Send + Sync {
    /// Generate a vector embedding for the given text.
    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbeddingError>;

    /// Embed a **query** (the asymmetric retrieval lane).
    ///
    /// Defaults to [`embed`](Embedder::embed). Models trained with separate
    /// document and query prefixes (nomic `search_document:`/`search_query:`,
    /// e5 `passage:`/`query:`) override this; retrieval call sites must use it
    /// for the text being searched WITH.
    async fn embed_query(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        self.embed(text).await
    }

    /// Batch embed multiple texts.
    ///
    /// Default implementation calls [`embed`](Embedder::embed) for each text sequentially.
    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        let mut results = Vec::with_capacity(texts.len());
        for text in texts {
            results.push(self.embed(text).await?);
        }
        Ok(results)
    }

    /// Return the dimensionality of embeddings produced by this embedder.
    fn dimension(&self) -> usize;

    /// Return the name/identifier of this embedder (e.g. "hash", "openai-text-embedding-3-small").
    ///
    /// Used for logging and configuration. Default returns "unknown".
    fn name(&self) -> &str {
        "unknown"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedding_error_display_invalid_input() {
        let err = EmbeddingError::InvalidInput("bad text".into());
        assert_eq!(format!("{err}"), "invalid input: bad text");
    }

    #[test]
    fn embedding_error_display_internal() {
        let err = EmbeddingError::Internal("something broke".into());
        assert_eq!(format!("{err}"), "internal error: something broke");
    }

    #[test]
    fn embedding_error_is_error_trait() {
        let err: Box<dyn std::error::Error> = Box::new(EmbeddingError::Internal("test".into()));
        assert!(err.to_string().contains("test"));
    }
}
