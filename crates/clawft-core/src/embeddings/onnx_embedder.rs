//! Real in-process embedder: nomic-embed-text-v1.5 ONNX (768-d).
//!
//! Tokenised with the model's own `tokenizer.json`, run through [`super::onnx_graph`]
//! (ONNX Runtime or the pure-Rust tract engine), masked
//! mean pooled over the last hidden state, then L2 normalised. The model's task
//! prefixes (`search_document: ` for stored text, `search_query: ` for queries)
//! are applied by [`OnnxEmbedder::document_vector`] / [`OnnxEmbedder::query_vector`]
//! (and the [`Embedder::embed`] / [`Embedder::embed_query`] trait methods).
//!
//! This is the recall embedder for crates that cannot reach the kernel's
//! providers (core, tools, cos). Opt-in via `onnx-embeddings` (ONNX Runtime)
//! or `tract-embeddings` (pure-Rust tract engine, for musl / Intel-mac / the
//! release archives); see [`super::onnx_graph`].
//! When the bundle is missing, [`OnnxEmbedder::from_dir`] returns an error naming
//! the path; nothing here ever substitutes hashed vectors.
//!
//! Bundle layout: `<dir>/onnx/model.onnx` + `<dir>/tokenizer.json`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use async_trait::async_trait;

use super::{Embedder, EmbeddingError};

/// Environment variable naming the bundle directory for production use.
pub const EMBED_MODEL_DIR_ENV: &str = "WEFTOS_EMBED_MODEL_DIR";

/// nomic-embed-text-v1.5 output width.
pub const NOMIC_DIMS: usize = 768;

const DOC_PREFIX: &str = "search_document: ";
const QUERY_PREFIX: &str = "search_query: ";
const MAX_TOKENS: usize = 512;

/// Directories searched for the bundle, in order: `$WEFTOS_EMBED_MODEL_DIR`,
/// `.weftos/models/nomic-embed-text-v1.5`, `$HOME/.weftos/models/nomic-embed-text-v1.5`.
///
/// The last one is the default install location of the shipped bundle
/// (owner decision 2026-11-12): `~/.weftos/models/nomic-embed-text-v1.5`.
pub fn default_model_dirs() -> Vec<PathBuf> {
    model_dirs_for(
        std::env::var(EMBED_MODEL_DIR_ENV).ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    )
}

/// [`default_model_dirs`] with the environment passed in (testable without
/// touching the process environment or the real home directory).
fn model_dirs_for(env_dir: Option<&str>, home: Option<&str>) -> Vec<PathBuf> {
    let sub = "nomic-embed-text-v1.5";
    let mut v = Vec::new();
    if let Some(d) = env_dir
        && !d.is_empty()
    {
        v.push(PathBuf::from(d));
    }
    v.push(PathBuf::from(format!(".weftos/models/{sub}")));
    if let Some(home) = home {
        v.push(PathBuf::from(home).join(format!(".weftos/models/{sub}")));
    }
    v
}

/// First bundle in [`default_model_dirs`] that holds `onnx/model.onnx`, or the
/// error naming every path looked at.
pub fn find_model_dir() -> Result<PathBuf, EmbeddingError> {
    let dirs = default_model_dirs();
    dirs.iter()
        .find(|d| d.join("onnx/model.onnx").exists())
        .cloned()
        .ok_or_else(|| {
            let tried: Vec<String> =
                dirs.iter().map(|d| d.join("onnx/model.onnx").display().to_string()).collect();
            EmbeddingError::Internal(format!(
                "semantic search unavailable: no embedding model found (looked for {}); \
                 configure an embedding model, or opt in to lexical mode explicitly",
                tried.join(", ")
            ))
        })
}

/// nomic-embed-text-v1.5 ONNX embedder (see module docs).
pub struct OnnxEmbedder {
    tok: tokenizers::Tokenizer,
    graph: super::onnx_graph::OnnxGraph,
    cache: Mutex<HashMap<String, Vec<f32>>>,
    dir: PathBuf,
}

impl OnnxEmbedder {
    /// Load the bundle in `dir`. Errors name the missing or unloadable path.
    pub fn from_dir(dir: impl AsRef<Path>) -> Result<Self, EmbeddingError> {
        let dir = dir.as_ref().to_path_buf();
        let (onnx, tokf) = (dir.join("onnx/model.onnx"), dir.join("tokenizer.json"));
        for p in [&onnx, &tokf] {
            if !p.exists() {
                return Err(EmbeddingError::Internal(format!(
                    "semantic search unavailable: embedding model file not found at {}",
                    p.display()
                )));
            }
        }
        let tok = tokenizers::Tokenizer::from_file(&tokf).map_err(|e| {
            EmbeddingError::Internal(format!("tokenizer {} failed to load: {e}", tokf.display()))
        })?;
        // Fixed thread count so float reduction order does not depend on the
        // host core count (bit-identical embeddings across machines).
        let graph = super::onnx_graph::OnnxGraph::load(&onnx, Some(4)).map_err(EmbeddingError::Internal)?;
        Ok(Self { tok, graph, cache: Mutex::new(HashMap::new()), dir })
    }

    /// Load from [`find_model_dir`].
    pub fn from_default_dirs() -> Result<Self, EmbeddingError> {
        Self::from_dir(find_model_dir()?)
    }

    /// Bundle directory this embedder was loaded from.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Inference engine this build runs the model on (`onnxruntime` or `tract`).
    pub fn engine(&self) -> &'static str {
        super::onnx_graph::ENGINE
    }

    /// Embed stored text (document lane).
    pub fn document_vector(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        self.embed_raw(&format!("{DOC_PREFIX}{text}"))
    }

    /// Embed a query (query lane).
    pub fn query_vector(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        self.embed_raw(&format!("{QUERY_PREFIX}{text}"))
    }

    fn embed_raw(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        if let Some(v) = self.cache.lock().unwrap_or_else(|e| e.into_inner()).get(text) {
            return Ok(v.clone());
        }
        let err = |what: &str, e: &dyn std::fmt::Display| EmbeddingError::Internal(format!("{what}: {e}"));
        let enc = self.tok.encode(text, true).map_err(|e| err("tokenize", &e))?;
        let n = enc.get_ids().len().min(MAX_TOKENS);
        let mut ids: Vec<i64> = enc.get_ids()[..n].iter().map(|&x| i64::from(x)).collect();
        let mut mask: Vec<i64> = enc.get_attention_mask()[..n].iter().map(|&x| i64::from(x)).collect();
        if enc.get_ids().len() > n
            && let (Some(&last), Some(slot), Some(m)) = (enc.get_ids().last(), ids.last_mut(), mask.last_mut())
        {
            // Truncation keeps the closing special token.
            *slot = i64::from(last);
            *m = 1;
        }
        let (shp, data) = self
            .graph
            .run(ids, mask.clone(), vec![0i64; n])
            .map_err(|e| EmbeddingError::Internal(format!("inference: {e}")))?;
        let hid = shp.last().copied().unwrap_or(0);
        if hid != NOMIC_DIMS {
            return Err(EmbeddingError::Internal(format!(
                "model {} produced {hid}-wide vectors, expected {NOMIC_DIMS}",
                self.dir.display()
            )));
        }
        let mut v = vec![0f32; hid];
        let mut cnt = 0f32;
        for (s, m) in mask.iter().enumerate() {
            if *m > 0 {
                for d in 0..hid {
                    v[d] += data[s * hid + d];
                }
                cnt += 1.0;
            }
        }
        v.iter_mut().for_each(|x| *x /= cnt.max(1.0));
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-12);
        v.iter_mut().for_each(|x| *x /= norm);
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if cache.len() > 4096 {
            cache.clear();
        }
        cache.insert(text.to_string(), v.clone());
        Ok(v)
    }
}

#[async_trait]
impl Embedder for OnnxEmbedder {
    /// Document lane (stored text). Use `embed_query` for queries.
    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        self.document_vector(text)
    }

    /// Query lane (`search_query: ` prefix).
    async fn embed_query(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        self.query_vector(text)
    }

    fn dimension(&self) -> usize {
        NOMIC_DIMS
    }

    fn name(&self) -> &str {
        "nomic-embed-text-v1.5"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shipped bundle's default location is found with no environment
    /// override, and `WEFTOS_EMBED_MODEL_DIR` wins when set. Uses a temp dir as
    /// HOME; the real `~/.weftos` is never read or written.
    #[test]
    fn default_location_is_the_users_weftos_models_dir_and_the_env_dir_wins() {
        let home = tempfile::tempdir().unwrap();
        let bundle = home.path().join(".weftos/models/nomic-embed-text-v1.5");
        std::fs::create_dir_all(bundle.join("onnx")).unwrap();
        std::fs::write(bundle.join("onnx/model.onnx"), b"stub").unwrap();
        let home_s = home.path().to_str().unwrap();

        let dirs = model_dirs_for(None, Some(home_s));
        let found = dirs.iter().find(|d| d.join("onnx/model.onnx").exists());
        assert_eq!(found, Some(&bundle), "default lookup must reach ~/.weftos/models/nomic-embed-text-v1.5");

        let other = tempfile::tempdir().unwrap();
        let dirs = model_dirs_for(other.path().to_str(), Some(home_s));
        assert_eq!(dirs[0], other.path(), "WEFTOS_EMBED_MODEL_DIR is searched first");
        assert_eq!(dirs.last(), Some(&bundle));
    }
}
