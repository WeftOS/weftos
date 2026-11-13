//! Shared real-embedder helper for recall and retrieval tests.
//!
//! Owner rule (2026-11-11): recall is judged with a REAL embedding model, never
//! a fake or word-hashing one. Tests that rank text call [`real_embedder`]; when
//! the gate is closed it prints why and returns `None`, and the test returns
//! early. Hashing is never a substitute.
//!
//! The gate is both of:
//! - the `onnx-embeddings` feature of `clawft-core` (forwarded by dependents'
//!   `real-embedder-tests` features), and
//! - `WEFTOS_TEST_EMBED_MODEL_DIR` pointing at a nomic-embed-text-v1.5 bundle
//!   (`onnx/model.onnx` + `tokenizer.json`).
//!
//! Tests that only need equal text to give equal vectors (plumbing) keep
//! [`HashEmbedder`](super::hash_embedder::HashEmbedder) and must say so in their
//! names; they must not assert on similarity or ranking.

#[cfg(feature = "onnx-embeddings")]
use std::sync::Arc;

/// Environment variable naming the test model bundle directory.
pub const TEST_MODEL_DIR_ENV: &str = "WEFTOS_TEST_EMBED_MODEL_DIR";

/// Width of the test model.
pub const TEST_MODEL_DIMS: usize = 768;

/// The real test embedder (document and query lanes). Cheap to clone.
#[derive(Clone)]
pub struct TestEmbedder {
    #[cfg(feature = "onnx-embeddings")]
    inner: Arc<super::onnx_embedder::OnnxEmbedder>,
}

impl TestEmbedder {
    /// Embed stored text (document lane).
    pub fn doc(&self, text: &str) -> Vec<f32> {
        #[cfg(feature = "onnx-embeddings")]
        {
            self.inner.document_vector(text).expect("real embedder: document embed")
        }
        #[cfg(not(feature = "onnx-embeddings"))]
        {
            let _ = text;
            unreachable!("TestEmbedder cannot exist without onnx-embeddings")
        }
    }

    /// Embed a query (query lane).
    pub fn query(&self, text: &str) -> Vec<f32> {
        #[cfg(feature = "onnx-embeddings")]
        {
            self.inner.query_vector(text).expect("real embedder: query embed")
        }
        #[cfg(not(feature = "onnx-embeddings"))]
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

/// The real test embedder, or `None` with the skip reason printed to stderr.
pub fn real_embedder() -> Option<TestEmbedder> {
    #[cfg(not(feature = "onnx-embeddings"))]
    {
        eprintln!(
            "SKIP real-embedder test: built without the onnx-embeddings feature \
             (use --features real-embedder-tests / onnx-embeddings); not falling back to hashing"
        );
        None
    }
    #[cfg(feature = "onnx-embeddings")]
    {
        use std::sync::OnceLock;
        static MODEL: OnceLock<Option<TestEmbedder>> = OnceLock::new();
        MODEL
            .get_or_init(|| {
                let Some(dir) = std::env::var_os(TEST_MODEL_DIR_ENV) else {
                    eprintln!(
                        "SKIP real-embedder test: {TEST_MODEL_DIR_ENV} is not set; \
                         not falling back to hashing"
                    );
                    return None;
                };
                match super::onnx_embedder::OnnxEmbedder::from_dir(&dir) {
                    Ok(inner) => Some(TestEmbedder { inner: Arc::new(inner) }),
                    Err(e) => panic!("{TEST_MODEL_DIR_ENV} is set but the model did not load: {e}"),
                }
            })
            .clone()
    }
}
