//! Shared real-embedder helper for tests (kernel and its dependents).
//!
//! Recall and retrieval are judged with a REAL embedding model, never a fake
//! or word-hashing one (owner rule 2026-11-11). Tests that rank text call
//! [`real_embedder`]; when the gate is closed it prints why and returns
//! `None`, and the test returns early. There is no hashing substitute.
//!
//! The gate is two things, both required:
//! - the `onnx-embeddings` feature (turned on by `real-embedder-tests`), and
//! - `WEFTOS_TEST_EMBED_MODEL_DIR` pointing at a nomic-embed-text-v1.5 bundle
//!   (`onnx/model.onnx` + `tokenizer.json`).
//!
//! Plumbing-only tests (anything that needs equal text to map to equal
//! vectors and nothing more) keep using
//! [`MockEmbeddingProvider`](crate::embedding::MockEmbeddingProvider) and say
//! so in their names; they must not assert on similarity or ranking.

use std::sync::Arc;

use crate::embedding::EmbeddingProvider;

/// Environment variable naming the test model bundle directory.
pub const TEST_MODEL_DIR_ENV: &str = "WEFTOS_TEST_EMBED_MODEL_DIR";

/// Width of the test model (nomic-embed-text-v1.5).
pub const TEST_MODEL_DIMS: usize = 768;

/// The real test embedder, or `None` with the skip reason printed to stderr.
pub fn real_embedder() -> Option<Arc<dyn EmbeddingProvider>> {
    #[cfg(not(any(feature = "onnx-embeddings", feature = "tract-embeddings")))]
    {
        eprintln!(
            "SKIP real-embedder test: built without a local embedding engine (onnx-embeddings or tract-embeddings) \
             (use --features real-embedder-tests); not falling back to hashing"
        );
        None
    }
    #[cfg(any(feature = "onnx-embeddings", feature = "tract-embeddings"))]
    {
        use std::sync::OnceLock;
        static MODEL: OnceLock<Option<Arc<dyn EmbeddingProvider>>> = OnceLock::new();
        MODEL
            .get_or_init(|| {
                let Some(dir) = std::env::var_os(TEST_MODEL_DIR_ENV) else {
                    eprintln!(
                        "SKIP real-embedder test: {TEST_MODEL_DIR_ENV} is not set; \
                         not falling back to hashing"
                    );
                    return None;
                };
                let dir = std::path::PathBuf::from(dir);
                let p = crate::embedding_onnx::OnnxEmbeddingProvider::nomic_embed_v1_5(&dir);
                if !p.is_runtime_available() {
                    panic!(
                        "{TEST_MODEL_DIR_ENV}={} is set but the model did not load: {}",
                        dir.display(),
                        p.unavailable_reason().unwrap_or("unknown")
                    );
                }
                Some(Arc::new(p) as Arc<dyn EmbeddingProvider>)
            })
            .clone()
    }
}

/// Cosine similarity of two vectors.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 { 0.0 } else { dot / (na * nb) }
}
