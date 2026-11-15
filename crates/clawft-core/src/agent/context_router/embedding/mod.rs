//! v2 [`ContextRouter`] backed by embedding similarity over skill descriptors.
//!
//! Phase E2 of `docs/plans/agent-core-v1.md`. The router builds an
//! in-memory index over hand-authored skill descriptors at construction
//! time. For each turn, it embeds the user's request via a crate-local
//! [`Embedder`](crate::embeddings::Embedder) (the production path uses
//! [`ApiEmbedder`](crate::embeddings::api_embedder::ApiEmbedder) talking
//! to an OpenAI-compat `/embeddings` endpoint;
//! [`HashEmbedder`](crate::embeddings::hash_embedder::HashEmbedder) is
//! the deterministic floor for offline / no-API-key environments) and
//! retrieves the top-K nearest skills.
//!
//! ## Hard contract (do not break)
//!
//! From `docs/research/rvf-context-router.md`: the router NEVER picks a
//! model and NEVER escalates a tier. The clamped `complexity_hint` only
//! nudges the downstream
//! [`TieredRouter`](crate::pipeline::tiered_router::TieredRouter)
//! classifier; the router itself emits an empty / informational decision
//! everywhere else.
//!
//! ## Index abstraction
//!
//! The production index is `ruvector-diskann@2.1` (Vamana graph + PQ +
//! mmap), pinned at workspace `Cargo.toml:176`. We adapt to its
//! L2-squared distance semantics by storing **L2-normalised** descriptor
//! vectors so `||a-b||² = 2 - 2·cos(θ)` — that is, smaller distance ↔
//! higher cosine similarity, with similarity = `1.0 - dist / 2.0` ∈
//! `[-1.0, +1.0]` for unit vectors.
//!
//! Tests use a tiny brute-force index so they don't pay the Vamana
//! build cost. The trait seam matches what C3 / E1 did: production uses
//! the heavyweight backend; tests use a deterministic stub.

use std::sync::Arc;

use async_trait::async_trait;
use clawft_types::skill::SkillDefinition;
use tracing::{debug, info, warn};

use super::{ContextDecision, ContextRequest, ContextRouter, clamp_complexity};
use crate::agent::skills_v2::SkillRegistry;
use crate::embeddings::Embedder;

mod index;
#[cfg(test)]
mod tests;

#[cfg(not(feature = "embedding-router"))]
use index::BruteForceIndex;
#[cfg(feature = "embedding-router")]
use index::DiskAnnEmbeddingIndex;
use index::Index;

// ── Tunables ─────────────────────────────────────────────────────────────

/// Default number of skills retrieved per turn.
///
/// The clawft skill catalog is ~35 entries today; 5 strongly-relevant
/// matches comfortably fit in the system prompt without flooding the
/// LLM. Tunable via [`EmbeddingRouter::with_top_k`].
pub const DEFAULT_TOP_K: usize = 5;

/// Default cosine-similarity floor below which the router falls back to
/// [`ContextDecision::default()`].
///
/// ~0.6 is the empirical sweet-spot from the v1 → v2 promotion-gate
/// research in `docs/research/rvf-context-router.md` — high enough to
/// reject off-topic skills, low enough that paraphrased requests still
/// match. Tunable via [`EmbeddingRouter::with_confidence_threshold`].
pub const DEFAULT_CONFIDENCE_THRESHOLD: f32 = 0.6;

/// Tracing target the v2 → v2.5 promotion gate's 7-day fallback metric
/// listens on. Emitted whenever the router falls back to an empty
/// decision (low confidence, embed error, empty index, etc.).
pub const FALLBACK_TRACING_TARGET: &str = "context_router.fallback";

/// Which vector-index backend this build compiled into
/// [`EmbeddingRouter`].
///
/// - `"diskann"` when the `embedding-router` cargo feature is enabled
///   (default via `full`): production Vamana + PQ via `ruvector-diskann`.
/// - `"brute-force"` when the feature is **off**: in-memory O(n) cosine
///   scan (see [`index::BruteForceIndex`]). Still correct for the ~35
///   skill catalog; chosen so offline / slim builds never silently
///   panic or refuse `Config.routing.context_router = "embedding"`.
///
/// WEFT-51: operators and tests assert this constant (not a runtime
/// probe) so a mis-linked feature matrix cannot claim diskann while
/// shipping the linear floor.
#[cfg(feature = "embedding-router")]
pub const INDEX_BACKEND: &str = "diskann";
#[cfg(not(feature = "embedding-router"))]
pub const INDEX_BACKEND: &str = "brute-force";

// ── Errors ────────────────────────────────────────────────────────────────

/// Failures from constructing an [`EmbeddingRouter`].
#[derive(Debug)]
pub enum EmbeddingRouterError {
    /// The skill registry was empty at construction time.
    EmptyRegistry,
    /// Embedding a skill descriptor failed.
    EmbedError(String),
    /// The underlying index implementation rejected the build.
    IndexError(String),
}

impl std::fmt::Display for EmbeddingRouterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyRegistry => write!(f, "skill registry was empty at construction"),
            Self::EmbedError(m) => write!(f, "skill descriptor embed failed: {m}"),
            Self::IndexError(m) => write!(f, "embedding index build failed: {m}"),
        }
    }
}

impl std::error::Error for EmbeddingRouterError {}

// ── EmbeddingRouter ───────────────────────────────────────────────────────

/// v2 router: embedding similarity over hand-authored skill descriptors.
///
/// See module docs for the contract. Construction is async because both
/// the [`Embedder`] and the index build steps may do I/O (API calls
/// for `ApiEmbedder`, vector graph build for diskann). The router is
/// cheap to clone (`Arc` everywhere); the hot path is purely read-side.
pub struct EmbeddingRouter {
    pub(super) embedder: Arc<dyn Embedder>,
    index: Arc<dyn Index>,
    /// Side table: `skill name → skill category`. The category lives in
    /// `SkillDefinition.metadata["openclaw-category"]` (see the existing
    /// SKILL.md examples in `skills_v2.rs:705`); we cache it at build
    /// time so the route hot path doesn't re-read the registry.
    archetypes: std::collections::HashMap<String, String>,
    top_k: usize,
    confidence_threshold: f32,
    /// Embedding dimension recorded at build time. The index rejects
    /// dimension-mismatched queries; we keep it here purely for tracing.
    dim: usize,
}

impl EmbeddingRouter {
    /// Construct an [`EmbeddingRouter`] from an embedder and a populated
    /// [`SkillRegistry`]. Embeds every skill's `(name, description)` and
    /// builds a fresh in-memory index.
    ///
    /// Returns [`EmbeddingRouterError::EmptyRegistry`] when the registry
    /// has no skills — a router with zero skills is useless and the
    /// caller should fall back to [`NullRouter`](super::NullRouter)
    /// rather than ship a no-op.
    pub async fn new(
        embedder: Arc<dyn Embedder>,
        skills: &SkillRegistry,
    ) -> Result<Self, EmbeddingRouterError> {
        if skills.is_empty() {
            return Err(EmbeddingRouterError::EmptyRegistry);
        }

        let dim = embedder.dimension();
        let mut entries: Vec<(String, Vec<f32>)> = Vec::with_capacity(skills.len());
        let mut archetypes: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();

        for skill in skills.list() {
            let descriptor = describe_skill(skill);
            let vector = embedder
                .embed(&descriptor)
                .await
                .map_err(|e| EmbeddingRouterError::EmbedError(e.to_string()))?;
            if vector.len() != dim {
                return Err(EmbeddingRouterError::EmbedError(format!(
                    "embedder returned {} dims for skill {}; expected {}",
                    vector.len(),
                    skill.name,
                    dim
                )));
            }
            entries.push((skill.name.clone(), vector));
            if let Some(cat) = extract_category(skill) {
                archetypes.insert(skill.name.clone(), cat);
            }
        }

        let index: Arc<dyn Index> = build_index(dim, entries)?;
        let n = index.len();

        info!(
            backend = INDEX_BACKEND,
            skills = n,
            dim,
            "EmbeddingRouter: index built"
        );

        Ok(Self {
            embedder,
            index,
            archetypes,
            top_k: DEFAULT_TOP_K,
            confidence_threshold: DEFAULT_CONFIDENCE_THRESHOLD,
            dim,
        })
    }

    /// Compile-time index backend for this build (`"diskann"` or
    /// `"brute-force"`). See [`INDEX_BACKEND`].
    #[must_use]
    pub fn index_backend(&self) -> &'static str {
        INDEX_BACKEND
    }

    /// Override the top-K retrieval count.
    #[must_use]
    pub fn with_top_k(mut self, k: usize) -> Self {
        self.top_k = k.max(1);
        self
    }

    /// Override the cosine-similarity floor.
    #[must_use]
    pub fn with_confidence_threshold(mut self, t: f32) -> Self {
        // Clamp into the legal cosine range so a bad operator config
        // can't push the threshold above 1.0 (always rejecting) or
        // below -1.0 (always accepting).
        self.confidence_threshold = t.clamp(-1.0, 1.0);
        self
    }
}

#[async_trait]
impl ContextRouter for EmbeddingRouter {
    async fn route(&self, request: &ContextRequest) -> ContextDecision {
        if self.index.len() == 0 {
            info!(
                target: FALLBACK_TRACING_TARGET,
                reason = "empty_index",
                "EmbeddingRouter: index has no skills; falling back"
            );
            return ContextDecision::default();
        }

        let query = match self.embedder.embed_query(&request.content).await {
            Ok(v) if v.len() == self.dim => v,
            Ok(v) => {
                warn!(
                    expected = self.dim,
                    got = v.len(),
                    "EmbeddingRouter: query dimension mismatch; falling back"
                );
                info!(
                    target: FALLBACK_TRACING_TARGET,
                    reason = "dim_mismatch",
                    "EmbeddingRouter: dim mismatch; falling back"
                );
                return ContextDecision::default();
            }
            Err(e) => {
                warn!(
                    error = %e,
                    "EmbeddingRouter: query embed failed; falling back to empty decision"
                );
                info!(
                    target: FALLBACK_TRACING_TARGET,
                    reason = "embed_error",
                    "EmbeddingRouter: embed error; falling back"
                );
                return ContextDecision::default();
            }
        };

        let hits = self.index.search(&query, self.top_k);
        if hits.is_empty() {
            info!(
                target: FALLBACK_TRACING_TARGET,
                reason = "no_hits",
                "EmbeddingRouter: index returned no hits; falling back"
            );
            return ContextDecision::default();
        }

        // Convert L2² on unit vectors to cosine: cos = 1 - dist / 2.
        // Numerical wobble can push values slightly above 1.0 or below
        // -1.0; clamp to the legal range so threshold comparisons stay
        // well-behaved.
        let top1_cos = (1.0 - hits[0].distance / 2.0).clamp(-1.0, 1.0);

        if top1_cos < self.confidence_threshold {
            info!(
                target: FALLBACK_TRACING_TARGET,
                reason = "low_confidence",
                top1_score = top1_cos,
                threshold = self.confidence_threshold,
                "EmbeddingRouter: top-1 below threshold; falling back"
            );
            return ContextDecision::default();
        }

        let skills: Vec<String> = hits.iter().map(|h| h.key.clone()).collect();
        let archetype = self.archetypes.get(&hits[0].key).cloned();

        // Complexity-hint mapping (documented per E2 plan):
        //
        // High top-1 cosine (≥ confidence_threshold) means the request
        // matches a known skill — the routing decision is unambiguous,
        // so we have no opinion on tier (hint = 0.0).
        //
        // Below threshold we already returned default(); the only
        // remaining case is a borderline match. We never push a *negative*
        // hint from this router (that would prefer cheaper models when
        // we're uncertain — wrong direction). When confidence is barely
        // above threshold we nudge slightly positive (+0.1) to favour
        // a more capable model that can disambiguate. The B1
        // `clamp_complexity` keeps the result inside `[-0.3, +0.3]`
        // regardless.
        let denom = (1.0 - self.confidence_threshold).max(f32::EPSILON);
        let confidence_band = (top1_cos - self.confidence_threshold) / denom;
        let hint = if confidence_band < 0.25 { 0.1 } else { 0.0 };

        debug!(
            top1 = %hits[0].key,
            top1_cos = top1_cos,
            archetype = ?archetype,
            skills = ?skills,
            "EmbeddingRouter: emitting decision"
        );

        let mut decision = ContextDecision::new(skills, None, clamp_complexity(hint));
        decision.archetype = archetype;
        decision
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────

/// Build the descriptor string a skill is embedded under.
///
/// `{name}: {description}` is the minimum the v2 contract requires; we
/// also fold in the openclaw-category metadata field (when present) so
/// the embedding picks up the archetype signal, not just the prose.
fn describe_skill(skill: &SkillDefinition) -> String {
    let mut s = format!("{}: {}", skill.name, skill.description);
    if let Some(cat) = extract_category(skill) {
        s.push_str(&format!(" [category: {cat}]"));
    }
    s
}

/// Read the archetype label off a [`SkillDefinition`].
///
/// Today this lives in `metadata["openclaw-category"]` as a JSON string
/// (see `skills_v2.rs::tests` line 706 for an example). We accept both
/// `openclaw-category` and `openclaw_category` for snake-case compat,
/// plus a plain `category` alias.
fn extract_category(skill: &SkillDefinition) -> Option<String> {
    skill
        .metadata
        .get("openclaw-category")
        .or_else(|| skill.metadata.get("openclaw_category"))
        .or_else(|| skill.metadata.get("category"))
        .and_then(|v| v.as_str())
        .map(str::to_owned)
}

/// L2-normalise a vector. Zero vectors stay zero (avoids div-by-zero).
pub(crate) fn normalise(mut v: Vec<f32>) -> Vec<f32> {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > f32::EPSILON {
        for x in &mut v {
            *x /= norm;
        }
    }
    v
}

/// Build the production index. Diskann when the feature is on; brute
/// force otherwise.
#[cfg(feature = "embedding-router")]
fn build_index(
    dim: usize,
    entries: Vec<(String, Vec<f32>)>,
) -> Result<Arc<dyn Index>, EmbeddingRouterError> {
    let idx = DiskAnnEmbeddingIndex::build(dim, entries)?;
    Ok(Arc::new(idx))
}

#[cfg(not(feature = "embedding-router"))]
fn build_index(
    _dim: usize,
    entries: Vec<(String, Vec<f32>)>,
) -> Result<Arc<dyn Index>, EmbeddingRouterError> {
    let mut idx = BruteForceIndex::new();
    for (k, v) in entries {
        idx.insert(k, v);
    }
    Ok(Arc::new(idx))
}
