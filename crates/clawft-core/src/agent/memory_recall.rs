//! Retrieved long-term memory with citation feedback (WEFT-732, RMM retrospective).
//!
//! Instead of dumping all of `MEMORY.md` into the prompt, the context gets at most `top_m`
//! snippets: a frozen retriever picks `top_k` candidates for the user's message, a reranker
//! orders them, and the survivors are injected tagged `[m1]`…`[mM]`. The generator is asked to
//! cite the ids it used in the same completion. After the turn, [`MemoryRecall::attribute`] turns
//! the citations into per-candidate rewards (`+1` cited, `-1` retrieved but not cited; ids that
//! were never retrieved are not touched) and hands them to the reranker. The retriever is never
//! updated from them: the paper's ablation collapsed accuracy 58.8 → 31.0 when it was
//! (arXiv:2503.08026; `docs/research/rmm-reflective-memory-management.md`).
//!
//! Fail-open is the caller's: with no recall attached, no query, or nothing retrieved,
//! `ContextBuilder` injects the full file as before. An untrained reranker preserves the
//! retriever's order (WEFT-46 day-0 contract).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// One paragraph of `MEMORY.md` with a stable key (hash of its text).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemorySnippet {
    /// Stable across turns while the text is unchanged (FNV-1a of the trimmed text).
    pub key: String,
    pub text: String,
}

/// Split a memory file into snippets: blank-line separated paragraphs, empty ones dropped.
/// A heading line on its own is kept with the paragraph that follows it. Comment-only lines
/// (`<!-- topic:key=… -->`, consolidator fingerprints) are removed.
pub fn split_snippets(md: &str) -> Vec<MemorySnippet> {
    let mut out = Vec::new();
    let mut pending_heading: Option<String> = None;
    for para in md.split("\n\n") {
        // Bookkeeping comments (topic keys, consolidator fingerprints) never reach the prompt.
        let kept: Vec<&str> = para
            .lines()
            .filter(|l| {
                let l = l.trim();
                !(l.starts_with("<!--") && l.ends_with("-->"))
            })
            .collect();
        let joined = kept.join("\n");
        let t = joined.trim();
        if t.is_empty() {
            continue;
        }
        if t.starts_with('#') && !t.contains('\n') {
            pending_heading = Some(match pending_heading.take() {
                Some(h) => format!("{h}\n{t}"),
                None => t.to_owned(),
            });
            continue;
        }
        let text = match pending_heading.take() {
            Some(h) => format!("{h}\n{t}"),
            None => t.to_owned(),
        };
        out.push(MemorySnippet { key: fnv_key(&text), text });
    }
    if let Some(h) = pending_heading {
        out.push(MemorySnippet { key: fnv_key(&h), text: h });
    }
    out
}

fn fnv_key(s: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// The frozen first-stage retriever. Never updated from citation rewards.
pub trait MemoryRetriever: Send + Sync {
    /// Up to `k` snippets for `query`, best first.
    fn top_k(&self, query: &str, snippets: &[MemorySnippet], k: usize) -> Vec<MemorySnippet>;
}

/// The trainable second stage.
pub trait MemoryReranker: Send + Sync {
    /// Reorder `candidates` (retriever order in). Untrained: return them unchanged.
    fn rerank(&self, query: &str, candidates: Vec<MemorySnippet>) -> Vec<MemorySnippet>;
    /// Per-candidate rewards for one turn: `(snippet, +1 cited | -1 ignored)`.
    fn observe(&self, query: &str, rewards: &[(MemorySnippet, i8)]);
}

/// Reranker that keeps the retriever's order and learns nothing (no SONA built in).
#[derive(Debug, Default, Clone, Copy)]
pub struct IdentityMemoryReranker;

impl MemoryReranker for IdentityMemoryReranker {
    fn rerank(&self, _query: &str, candidates: Vec<MemorySnippet>) -> Vec<MemorySnippet> {
        candidates
    }
    fn observe(&self, _query: &str, _rewards: &[(MemorySnippet, i8)]) {}
}

/// What one turn retrieved, kept until the turn's reply is attributed.
#[derive(Debug, Clone)]
struct Pending {
    query: String,
    /// `(label, snippet)` in injection order: `m1`, `m2`, …
    picked: Vec<(String, MemorySnippet)>,
}

/// Retrieval + rerank + citation attribution for one agent.
pub struct MemoryRecall {
    retriever: Arc<dyn MemoryRetriever>,
    reranker: Arc<dyn MemoryReranker>,
    top_k: usize,
    top_m: usize,
    /// Per session key: what the last context build injected.
    pending: Mutex<HashMap<String, Pending>>,
}

/// Header of the injected block; the citation instruction is part of the generator prompt.
pub const RECALL_HEADER: &str = "# Relevant Memory (retrieved)\n\n\
Each entry below has an id like [m1]. If you use an entry in your answer, cite its id in \
square brackets, e.g. [m2]. Do not cite ids you did not use.";

impl MemoryRecall {
    pub fn new(retriever: Arc<dyn MemoryRetriever>, reranker: Arc<dyn MemoryReranker>, top_k: usize, top_m: usize) -> Self {
        Self { retriever, reranker, top_k: top_k.max(1), top_m: top_m.max(1), pending: Mutex::new(HashMap::new()) }
    }

    /// Pick at most `top_m` snippets for `query` and remember them for `session_key`.
    /// Returns the `(label, snippet)` list; empty means "nothing retrieved, fail open".
    pub fn select(&self, session_key: &str, query: &str, memory: &str) -> Vec<(String, MemorySnippet)> {
        let snippets = split_snippets(memory);
        if query.trim().is_empty() || snippets.is_empty() {
            self.pending.lock().unwrap().remove(session_key);
            return Vec::new();
        }
        let candidates = self.retriever.top_k(query, &snippets, self.top_k);
        let ranked = self.reranker.rerank(query, candidates);
        let picked: Vec<(String, MemorySnippet)> =
            ranked.into_iter().take(self.top_m).enumerate().map(|(i, s)| (format!("m{}", i + 1), s)).collect();
        let mut pending = self.pending.lock().unwrap();
        if picked.is_empty() {
            pending.remove(session_key);
        } else {
            pending.insert(session_key.to_owned(), Pending { query: query.to_owned(), picked: picked.clone() });
        }
        picked
    }

    /// The system-message body for a selection.
    pub fn render(picked: &[(String, MemorySnippet)]) -> String {
        let mut s = String::from(RECALL_HEADER);
        for (label, snip) in picked {
            s.push_str(&format!("\n\n[{label}] {}", snip.text));
        }
        s
    }

    /// Attribute the turn's reply for `session_key`: reward the reranker (`+1` cited, `-1`
    /// retrieved but not cited) and return the reply with citation markers for the turn's
    /// labels removed (for the user-facing message). Without a pending selection the reply is
    /// returned unchanged and nothing is observed.
    pub fn attribute(&self, session_key: &str, reply: &str) -> String {
        let Some(p) = self.pending.lock().unwrap().remove(session_key) else { return reply.to_owned() };
        let cited = cited_labels(reply);
        let rewards: Vec<(MemorySnippet, i8)> = p
            .picked
            .iter()
            .map(|(label, snip)| (snip.clone(), if cited.iter().any(|c| c == label) { 1 } else { -1 }))
            .collect();
        self.reranker.observe(&p.query, &rewards);
        strip_labels(reply, p.picked.iter().map(|(l, _)| l.as_str()))
    }
}

/// Labels cited as `[m<digits>]` in `text`, in order, without duplicates.
pub fn cited_labels(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let b = text.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'[' && i + 1 < b.len() && b[i + 1] == b'm' {
            let mut j = i + 2;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            if j > i + 2 && j < b.len() && b[j] == b']' {
                let label = &text[i + 1..j];
                if !out.iter().any(|l| l == label) {
                    out.push(label.to_owned());
                }
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Remove ` [mN]` markers for `labels` (and the space before them) from `text`.
fn strip_labels<'a>(text: &str, labels: impl Iterator<Item = &'a str>) -> String {
    let mut s = text.to_owned();
    for l in labels {
        let tag = format!("[{l}]");
        s = s.replace(&format!(" {tag}"), "").replace(&tag, "");
    }
    s
}

/// Rank `snippets` by cosine of `embed(snippet)` against `q`, best first, document
/// order on ties so results are stable.
#[cfg(feature = "vector-memory")]
fn rank_by_cosine(
    q: &[f32],
    snippets: &[MemorySnippet],
    k: usize,
    mut embed: impl FnMut(&str) -> Option<Vec<f32>>,
) -> Vec<MemorySnippet> {
    let mut scored: Vec<(f32, usize)> = snippets
        .iter()
        .enumerate()
        .filter_map(|(i, s)| embed(&s.text).map(|e| (q.iter().zip(&e).map(|(a, b)| a * b).sum::<f32>(), i)))
        .collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal).then(a.1.cmp(&b.1)));
    scored.into_iter().take(k).map(|(_, i)| snippets[i].clone()).collect()
}

/// Retriever over a REAL embedding model (nomic-embed-text-v1.5, cosine). This is
/// the production retriever. Needs the `onnx-embeddings` feature.
#[cfg(any(feature = "onnx-embeddings", feature = "tract-embeddings"))]
pub struct OnnxRetriever {
    embedder: crate::embeddings::onnx_embedder::OnnxEmbedder,
}

#[cfg(any(feature = "onnx-embeddings", feature = "tract-embeddings"))]
impl OnnxRetriever {
    /// Load the model bundle in `dir`. Errors name the missing path.
    pub fn from_dir(dir: &std::path::Path) -> Result<Self, crate::embeddings::EmbeddingError> {
        Ok(Self { embedder: crate::embeddings::onnx_embedder::OnnxEmbedder::from_dir(dir)? })
    }
}

#[cfg(any(feature = "onnx-embeddings", feature = "tract-embeddings"))]
impl MemoryRetriever for OnnxRetriever {
    fn top_k(&self, query: &str, snippets: &[MemorySnippet], k: usize) -> Vec<MemorySnippet> {
        let q = match self.embedder.query_vector(query) {
            Ok(q) => q,
            Err(e) => {
                // Empty selection makes the caller fail open (full MEMORY.md); say so loudly.
                tracing::error!("memory recall: query embedding failed ({e}); injecting the full MEMORY.md");
                return Vec::new();
            }
        };
        rank_by_cosine(&q, snippets, k, |t| match self.embedder.document_vector(t) {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::error!("memory recall: snippet embedding failed ({e}); snippet skipped");
                None
            }
        })
    }
}

/// Retriever over SimHash embeddings (cosine) in the EXPLICIT lexical mode.
///
/// Matches shared words only; a paraphrase scores nothing. It is constructed
/// only on opt-in (`agents.memory_recall.lexical` or `WEFTOS_EMBEDDING_MODE=lexical`)
/// and warns once that results are lexical, not semantic. There is no `Default`.
#[cfg(feature = "vector-memory")]
pub struct LexicalRetriever {
    embedder: crate::embeddings::hash_embedder::HashEmbedder,
}

#[cfg(feature = "vector-memory")]
// No `Default` on purpose: constructing it is an explicit opt-in that logs a warning.
#[allow(clippy::new_without_default)]
impl LexicalRetriever {
    /// Build the lexical retriever (logs once that results are lexical).
    pub fn new() -> Self {
        crate::embeddings::select::warn_lexical_once();
        Self { embedder: crate::embeddings::hash_embedder::HashEmbedder::default_dimension() }
    }
}

#[cfg(feature = "vector-memory")]
impl MemoryRetriever for LexicalRetriever {
    fn top_k(&self, query: &str, snippets: &[MemorySnippet], k: usize) -> Vec<MemorySnippet> {
        let q = self.embedder.compute_embedding(query);
        rank_by_cosine(&q, snippets, k, |t| Some(self.embedder.compute_embedding(t)))
    }
}

/// Error from [`try_from_config`]. Without `vector-memory` (for example the
/// browser build) no retriever is built in and nothing can fail, so the error
/// type is uninhabited; with it, the embedding error names what was looked for.
#[cfg(feature = "vector-memory")]
pub type RecallBuildError = crate::embeddings::EmbeddingError;
/// Error from [`try_from_config`]; uninhabited without `vector-memory`.
#[cfg(not(feature = "vector-memory"))]
pub type RecallBuildError = std::convert::Infallible;

/// Build the recall for an agent from config.
///
/// `Ok(None)` when disabled or when no retriever is built in. When enabled, the
/// retriever is a REAL embedding model (`cfg.model_dir`, else the documented
/// search path), or the lexical retriever only when explicitly opted in. With
/// neither, this is an `Err` naming what was looked for; it never quietly
/// hashes.
pub fn try_from_config(
    cfg: &clawft_types::config::MemoryRecallConfig,
) -> Result<Option<Arc<MemoryRecall>>, RecallBuildError> {
    if !cfg.enabled {
        return Ok(None);
    }
    #[cfg(feature = "vector-memory")]
    {
        let retriever: Arc<dyn MemoryRetriever> =
            if cfg.lexical || crate::embeddings::select::lexical_mode_requested() {
                Arc::new(LexicalRetriever::new())
            } else {
                real_retriever(cfg)?
            };
        let reranker: Arc<dyn MemoryReranker> = default_reranker();
        Ok(Some(Arc::new(MemoryRecall::new(retriever, reranker, cfg.top_k, cfg.top_m))))
    }
    #[cfg(not(feature = "vector-memory"))]
    {
        tracing::warn!("agents.memory_recall is enabled but this build has no retriever (vector-memory); injecting the full MEMORY.md");
        Ok(None)
    }
}

#[cfg(all(feature = "vector-memory", any(feature = "onnx-embeddings", feature = "tract-embeddings")))]
fn real_retriever(
    cfg: &clawft_types::config::MemoryRecallConfig,
) -> Result<Arc<dyn MemoryRetriever>, crate::embeddings::EmbeddingError> {
    let dir = match cfg.model_dir.as_deref().filter(|d| !d.is_empty()) {
        Some(d) => std::path::PathBuf::from(d),
        None => crate::embeddings::onnx_embedder::find_model_dir()?,
    };
    Ok(Arc::new(OnnxRetriever::from_dir(&dir)?))
}

#[cfg(all(feature = "vector-memory", not(any(feature = "onnx-embeddings", feature = "tract-embeddings"))))]
fn real_retriever(
    _cfg: &clawft_types::config::MemoryRecallConfig,
) -> Result<Arc<dyn MemoryRetriever>, crate::embeddings::EmbeddingError> {
    Err(crate::embeddings::select::unavailable(
        "this build has no local embedding engine (onnx-embeddings or tract-embeddings), so no local model can load",
    ))
}

/// Like [`try_from_config`] for boot paths that must not fail offline: on error it
/// logs the reason and returns `None`, so the agent injects the full `MEMORY.md`
/// (the same as recall being off). It never substitutes hashed retrieval.
pub fn from_config(cfg: &clawft_types::config::MemoryRecallConfig) -> Option<Arc<MemoryRecall>> {
    match try_from_config(cfg) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("agents.memory_recall is enabled but {e}; memory recall is OFF and the full MEMORY.md is injected");
            None
        }
    }
}

/// Default rerank: SONA over REAL embeddings when a model is available, else no rerank
/// (retriever order). It never reranks over hashed vectors.
#[cfg(all(feature = "vector-memory", feature = "hybrid-rerank", any(feature = "onnx-embeddings", feature = "tract-embeddings")))]
fn default_reranker() -> Arc<dyn MemoryReranker> {
    use crate::agent::context_router::sona_rerank::SonaSkillReranker;
    use crate::embeddings::onnx_embedder::{NOMIC_DIMS, OnnxEmbedder, find_model_dir};
    let embedder = find_model_dir().and_then(|d| OnnxEmbedder::from_dir(&d));
    match embedder {
        Ok(e) => {
            let e = Arc::new(e);
            Arc::new(SonaSkillReranker::with_text_embedder(
                ruvector_sona::SonaConfig {
                    hidden_dim: NOMIC_DIMS,
                    embedding_dim: NOMIC_DIMS,
                    pattern_clusters: 16,
                    trajectory_capacity: 1024,
                    quality_threshold: 0.15,
                    ..ruvector_sona::SonaConfig::default()
                },
                Arc::new(move |t: &str| e.document_vector(t).unwrap_or_default()),
            ))
        }
        Err(e) => {
            tracing::warn!("memory recall rerank disabled: {e}");
            Arc::new(IdentityMemoryReranker)
        }
    }
}
#[cfg(all(feature = "vector-memory", feature = "hybrid-rerank", not(any(feature = "onnx-embeddings", feature = "tract-embeddings"))))]
fn default_reranker() -> Arc<dyn MemoryReranker> {
    tracing::warn!("memory recall rerank disabled: this build lacks onnx-embeddings, and SONA is not run over hashed vectors");
    Arc::new(IdentityMemoryReranker)
}
#[cfg(all(feature = "vector-memory", not(feature = "hybrid-rerank")))]
fn default_reranker() -> Arc<dyn MemoryReranker> {
    Arc::new(IdentityMemoryReranker)
}

#[cfg(test)]
#[path = "memory_recall_tests.rs"]
mod tests;
