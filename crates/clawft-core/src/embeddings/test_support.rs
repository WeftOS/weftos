//! Shared real-embedder helper for recall and retrieval tests.
//!
//! Owner rule (2026-11-11): recall is judged with a REAL embedding model, never
//! a fake or word-hashing one. Tests that rank text call [`real_embedder`]; when
//! the `real-embedder-tests` feature is on it fails loudly if the model is
//! missing; without the feature the tests are `#[ignore]`d. Hashing is never a
//! substitute.
//!
//! The gate is both of:
//! - the `real-embedder-tests` feature of `clawft-core` (implies
//!   `onnx-embeddings`; forwarded by dependents' `real-embedder-tests`), and
//! - `WEFTOS_TEST_EMBED_MODEL_DIR` pointing at a nomic-embed-text-v1.5 bundle
//!   (`onnx/model.onnx` + `tokenizer.json`).
//!
//! Tests that only need equal text to give equal vectors (plumbing) keep
//! [`HashEmbedder`](super::hash_embedder::HashEmbedder) and must say so in their
//! names; they must not assert on similarity or ranking.

#[cfg(any(feature = "onnx-embeddings", feature = "tract-embeddings"))]
use std::sync::Arc;

/// Environment variable naming the test model bundle directory.
pub const TEST_MODEL_DIR_ENV: &str = "WEFTOS_TEST_EMBED_MODEL_DIR";

/// Width of the test model.
pub const TEST_MODEL_DIMS: usize = 768;

/// The real test embedder (document and query lanes). Cheap to clone.
#[derive(Clone)]
pub struct TestEmbedder {
    #[cfg(any(feature = "onnx-embeddings", feature = "tract-embeddings"))]
    inner: Arc<super::onnx_embedder::OnnxEmbedder>,
}

impl TestEmbedder {
    /// Embed stored text (document lane).
    pub fn doc(&self, text: &str) -> Vec<f32> {
        #[cfg(any(feature = "onnx-embeddings", feature = "tract-embeddings"))]
        {
            self.inner.document_vector(text).expect("real embedder: document embed")
        }
        #[cfg(not(any(feature = "onnx-embeddings", feature = "tract-embeddings")))]
        {
            let _ = text;
            unreachable!("TestEmbedder cannot exist without onnx-embeddings")
        }
    }

    /// Embed a query (query lane).
    pub fn query(&self, text: &str) -> Vec<f32> {
        #[cfg(any(feature = "onnx-embeddings", feature = "tract-embeddings"))]
        {
            self.inner.query_vector(text).expect("real embedder: query embed")
        }
        #[cfg(not(any(feature = "onnx-embeddings", feature = "tract-embeddings")))]
        {
            let _ = text;
            unreachable!("TestEmbedder cannot exist without onnx-embeddings")
        }
    }

    /// Index of the document in `docs` nearest to `query` (cosine; vectors are unit length).
    pub fn nearest(&self, query: &str, docs: &[&str]) -> usize {
        let q = self.query(query);
        let mut best = (f32::MIN, 0usize);
        for (i, d) in docs.iter().enumerate() {
            let s = cosine(&q, &self.doc(d));
            if s > best.0 {
                best = (s, i);
            }
        }
        best.1
    }
}

#[async_trait::async_trait]
impl super::Embedder for TestEmbedder {
    async fn embed(&self, text: &str) -> Result<Vec<f32>, super::EmbeddingError> {
        Ok(self.doc(text))
    }

    async fn embed_query(&self, text: &str) -> Result<Vec<f32>, super::EmbeddingError> {
        Ok(self.query(text))
    }

    fn dimension(&self) -> usize {
        TEST_MODEL_DIMS
    }

    fn name(&self) -> &str {
        "nomic-embed-text-v1.5"
    }
}

/// Cosine similarity.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let (na, nb) = (
        a.iter().map(|x| x * x).sum::<f32>().sqrt(),
        b.iter().map(|x| x * x).sum::<f32>().sqrt(),
    );
    if na == 0.0 || nb == 0.0 { 0.0 } else { dot / (na * nb) }
}

/// The real test embedder.
///
/// Never a silent skip: with `real-embedder-tests` on, a missing
/// `WEFTOS_TEST_EMBED_MODEL_DIR` (or an unloadable bundle) panics with the env
/// var and the expected files, and without the feature it panics too (the
/// calling tests are `#[ignore]`d then, so this is only reached by
/// `--ignored`). The `Option` is kept so call sites stay `let Some(..) = .. else`;
/// it is always `Some` when this returns.
pub fn real_embedder() -> Option<TestEmbedder> {
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
        static MODEL: OnceLock<TestEmbedder> = OnceLock::new();
        let model = MODEL.get_or_init(|| {
            let Some(dir) = std::env::var_os(TEST_MODEL_DIR_ENV) else {
                panic!(
                    "{TEST_MODEL_DIR_ENV} is not set: real-embedder-tests needs the \
                     nomic-embed-text-v1.5 bundle (expected <dir>/onnx/model.onnx and \
                     <dir>/tokenizer.json); refusing to pass without it"
                );
            };
            match super::onnx_embedder::OnnxEmbedder::from_dir(&dir) {
                Ok(inner) => TestEmbedder { inner: Arc::new(inner) },
                Err(e) => panic!(
                    "{TEST_MODEL_DIR_ENV}={} is set but the model did not load: {e}",
                    std::path::Path::new(&dir).display()
                ),
            }
        });
        Some(model.clone())
    }
}
