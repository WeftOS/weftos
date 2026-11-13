//! SimHash-based local embedder.
//!
//! Produces deterministic, fixed-dimension embeddings by hashing each word
//! with [`FnvHasher`] (FNV-1a) and spreading the hash bits across a
//! float vector. The result is normalized to unit length.
//!
//! FNV-1a is deterministic and platform-independent, producing the same
//! output for the same input regardless of Rust version or target triple.
//! This is critical for persisted embeddings that must remain stable across
//! toolchain upgrades.
//!
//! This embedder requires no API calls, no model files, and no network access.
//! It is LEXICAL, not semantic: texts match to the extent they share words, and
//! synonyms or paraphrases do not match at all. It must never be the silent
//! default of a recall or retrieval path, and recall quality must not be judged
//! with it; see [`super::select`] for the explicit lexical opt-in and
//! [`super::test_support`] for the real-embedder test helper.

use std::hash::{Hash, Hasher};

use fnv::FnvHasher;

use async_trait::async_trait;

use super::{Embedder, EmbeddingError};

/// SimHash-based embedder that produces deterministic embeddings locally.
///
/// # Algorithm
///
/// For each word in the input text:
/// 1. Hash the word with [`FnvHasher`] (FNV-1a, deterministic cross-platform).
/// 2. For each dimension `i` in 0..dimension, check bit `(i % 64)` of the hash
///    XORed with `i`. If the bit is set, add +1.0 to that dimension; otherwise
///    add -1.0.
/// 3. After processing all words, normalize the vector to unit length (L2 norm).
///
/// The default dimension is 384.
pub struct HashEmbedder {
    dimension: usize,
}

impl HashEmbedder {
    /// Create a new `HashEmbedder` with the specified embedding dimension.
    pub fn new(dimension: usize) -> Self {
        Self { dimension }
    }

    /// Create a `HashEmbedder` with the default dimension of 384.
    pub fn default_dimension() -> Self {
        Self::new(384)
    }

    /// Compute the SimHash embedding synchronously.
    ///
    /// This is the underlying synchronous implementation. The async
    /// [`Embedder::embed`] trait method delegates to this.
    pub fn compute_embedding(&self, text: &str) -> Vec<f32> {
        let mut vector = vec![0.0f32; self.dimension];

        let words: Vec<&str> = text.split_whitespace().collect();

        if words.is_empty() {
            // Return a zero vector for empty input (cannot normalize).
            return vector;
        }

        for word in &words {
            let mut hasher = FnvHasher::default();
            word.to_lowercase().hash(&mut hasher);
            let hash = hasher.finish();

            for (i, val) in vector.iter_mut().enumerate() {
                // Mix the dimension index into the hash to spread bits
                let mixed = hash ^ (i as u64);
                let bit = (mixed >> (i % 64)) & 1;
                if bit == 1 {
                    *val += 1.0;
                } else {
                    *val -= 1.0;
                }
            }
        }

        // Normalize to unit length.
        let norm: f32 = vector.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for val in &mut vector {
                *val /= norm;
            }
        }

        vector
    }
}

#[async_trait]
impl Embedder for HashEmbedder {
    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        Ok(self.compute_embedding(text))
    }

    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        let results: Vec<Vec<f32>> = texts.iter().map(|t| self.compute_embedding(t)).collect();
        Ok(results)
    }

    fn dimension(&self) -> usize {
        self.dimension
    }

    fn name(&self) -> &str {
        "hash"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn deterministic_same_text_same_embedding() {
        let embedder = HashEmbedder::default_dimension();
        let e1 = embedder.embed("hello world").await.unwrap();
        let e2 = embedder.embed("hello world").await.unwrap();
        assert_eq!(e1, e2, "same text must produce identical embeddings");
    }

    #[tokio::test]
    async fn different_text_different_embedding() {
        let embedder = HashEmbedder::default_dimension();
        let e1 = embedder.embed("hello world").await.unwrap();
        let e2 = embedder.embed("goodbye moon").await.unwrap();
        assert_ne!(e1, e2, "different text should produce different embeddings");
    }

    #[tokio::test]
    async fn correct_dimension() {
        let embedder = HashEmbedder::new(128);
        let emb = embedder.embed("test text").await.unwrap();
        assert_eq!(emb.len(), 128);
        assert_eq!(embedder.dimension(), 128);
    }

    #[tokio::test]
    async fn default_dimension_is_384() {
        let embedder = HashEmbedder::default_dimension();
        let emb = embedder.embed("test").await.unwrap();
        assert_eq!(emb.len(), 384);
        assert_eq!(embedder.dimension(), 384);
    }

    #[tokio::test]
    async fn unit_length_norm() {
        let embedder = HashEmbedder::default_dimension();
        let emb = embedder.embed("the quick brown fox").await.unwrap();
        let norm: f32 = emb.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(
            (norm - 1.0).abs() < 0.01,
            "L2 norm should be ~1.0, got {norm}"
        );
    }

    #[tokio::test]
    async fn empty_string_handled() {
        let embedder = HashEmbedder::default_dimension();
        let emb = embedder.embed("").await.unwrap();
        assert_eq!(emb.len(), 384);
        // All zeros for empty input.
        let sum: f32 = emb.iter().map(|x| x.abs()).sum();
        assert!(
            sum < f32::EPSILON,
            "empty string should produce zero vector, sum={sum}"
        );
    }

    #[tokio::test]
    async fn whitespace_only_handled() {
        let embedder = HashEmbedder::default_dimension();
        let emb = embedder.embed("   \t\n  ").await.unwrap();
        assert_eq!(emb.len(), 384);
        let sum: f32 = emb.iter().map(|x| x.abs()).sum();
        assert!(
            sum < f32::EPSILON,
            "whitespace-only should produce zero vector"
        );
    }

    #[tokio::test]
    async fn embed_batch_correctness() {
        let embedder = HashEmbedder::default_dimension();
        let texts = vec!["hello world".to_string(), "goodbye moon".to_string()];
        let batch = embedder.embed_batch(&texts).await.unwrap();

        assert_eq!(batch.len(), 2);

        let e1 = embedder.embed("hello world").await.unwrap();
        let e2 = embedder.embed("goodbye moon").await.unwrap();

        assert_eq!(batch[0], e1);
        assert_eq!(batch[1], e2);
    }

    #[tokio::test]
    async fn embed_batch_empty() {
        let embedder = HashEmbedder::default_dimension();
        let batch = embedder.embed_batch(&[]).await.unwrap();
        assert!(batch.is_empty());
    }

    #[tokio::test]
    async fn case_insensitive_hash() {
        let embedder = HashEmbedder::default_dimension();
        let e1 = embedder.embed("Hello World").await.unwrap();
        let e2 = embedder.embed("hello world").await.unwrap();
        assert_eq!(e1, e2, "hashing should be case-insensitive");
    }

    #[tokio::test]
    async fn single_word_produces_unit_vector() {
        let embedder = HashEmbedder::default_dimension();
        let emb = embedder.embed("rust").await.unwrap();
        let norm: f32 = emb.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(
            (norm - 1.0).abs() < 0.01,
            "single word norm should be ~1.0, got {norm}"
        );
    }

    #[tokio::test]
    async fn similar_text_higher_cosine() {
        let embedder = HashEmbedder::default_dimension();
        let e1 = embedder.embed("the quick brown fox").await.unwrap();
        let e2 = embedder.embed("the quick brown dog").await.unwrap();
        let e3 = embedder
            .embed("quantum computing algorithms")
            .await
            .unwrap();

        let sim_close = cosine_similarity(&e1, &e2);
        let sim_far = cosine_similarity(&e1, &e3);

        assert!(
            sim_close > sim_far,
            "similar text should have higher cosine similarity: close={sim_close}, far={sim_far}"
        );
    }

    /// Golden test: verify that FNV-1a produces deterministic, platform-independent
    /// embeddings for a known input. These values were recorded from the FNV-1a
    /// implementation and should produce identical results on x86_64-linux,
    /// aarch64-linux, and x86_64-darwin.
    ///
    /// If this test fails after a change, it means persisted embeddings are
    /// silently invalidated and must be re-computed.
    #[test]
    fn golden_test_hello_world() {
        let embedder = HashEmbedder::default_dimension();
        let emb = embedder.compute_embedding("hello world");

        // Golden values recorded from FNV-1a on x86_64-linux (Rust 1.85).
        // These must be identical on all platforms.
        let expected_first_8: [f32; 8] = [
            -0.07715168,
            -0.07715168,
            0.07715168,
            0.0,
            0.0,
            0.0,
            0.0,
            0.0,
        ];
        let expected_last_8: [f32; 8] = [
            0.0,
            0.0,
            -0.07715168,
            0.0,
            -0.07715168,
            0.07715168,
            0.0,
            0.07715168,
        ];

        assert_eq!(
            &emb[..8],
            &expected_first_8[..],
            "first 8 dimensions changed -- FNV-1a determinism broken"
        );
        assert_eq!(
            &emb[376..384],
            &expected_last_8[..],
            "last 8 dimensions changed -- FNV-1a determinism broken"
        );

        // Verify the embedding is non-zero and normalized
        let norm: f32 = emb.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 0.01);

        // Verify determinism: calling twice yields identical output.
        let emb2 = embedder.compute_embedding("hello world");
        assert_eq!(emb, emb2, "FNV-1a should be deterministic");

        // Verify dimensionality
        assert_eq!(emb.len(), 384);
    }

    fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
        let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
        let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm_a == 0.0 || norm_b == 0.0 {
            return 0.0;
        }
        dot / (norm_a * norm_b)
    }
}
