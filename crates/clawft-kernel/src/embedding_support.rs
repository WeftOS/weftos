//! Shared real-embedder helper for tests (kernel and its dependents).
//!
//! Recall and retrieval are judged with a REAL embedding model, never a fake
//! or word-hashing one (owner rule 2026-11-11). Tests that rank text call
//! [`real_embedder`]; with `real-embedder-tests` on it fails loudly if the model
//! is missing, and without the feature the tests are `#[ignore]`d. There is no
//! hashing substitute.
//!
//! The gate is two things, both required:
//! - the `real-embedder-tests` feature (implies `onnx-embeddings`), and
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

/// The real test embedder.
///
/// Never a silent skip: with `real-embedder-tests` on, a missing
/// `WEFTOS_TEST_EMBED_MODEL_DIR` (or an unloadable bundle) panics naming the
/// env var and the expected files; without the feature it panics too (callers
/// are `#[ignore]`d then, so only `--ignored` reaches it). The `Option` is kept
/// so call sites stay `let Some(..) = .. else`; it is always `Some` on return.
pub fn real_embedder() -> Option<Arc<dyn EmbeddingProvider>> {
    #[cfg(not(feature = "real-embedder-harness"))]
    {
        panic!(
            "real-embedder test run without the `real-embedder-tests` feature; \
             rerun with --features real-embedder-tests (ort) or real-embedder-tests-tract (tract) and {TEST_MODEL_DIR_ENV} set \
             (a nomic-embed-text-v1.5 bundle: onnx/model.onnx + tokenizer.json)"
        )
    }
    #[cfg(feature = "real-embedder-harness")]
    {
        use std::sync::OnceLock;
        static MODEL: OnceLock<Arc<dyn EmbeddingProvider>> = OnceLock::new();
        let model = MODEL.get_or_init(|| {
            let Some(dir) = std::env::var_os(TEST_MODEL_DIR_ENV) else {
                panic!(
                    "{TEST_MODEL_DIR_ENV} is not set: real-embedder-tests needs the \
                     nomic-embed-text-v1.5 bundle (expected <dir>/onnx/model.onnx and \
                     <dir>/tokenizer.json); refusing to pass without it"
                );
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
            Arc::new(p) as Arc<dyn EmbeddingProvider>
        });
        Some(model.clone())
    }
}

/// Cosine similarity of two vectors.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 { 0.0 } else { dot / (na * nb) }
}
