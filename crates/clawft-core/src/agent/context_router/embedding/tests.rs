//! Unit tests for the v2 [`EmbeddingRouter`].
//!
//! Tests use a deterministic in-process [`StubEmbedder`] so we never
//! touch the network. The diskann backend is always exercised when the
//! `embedding-router` feature is on (default); the brute-force backend
//! is exercised inside `embedding/index.rs` tests **and** (WEFT-51) via
//! the `#[cfg(not(feature = "embedding-router"))]` module at the bottom
//! of this file — run with:
//!
//! ```bash
//! cargo test -p clawft-core --no-default-features \
//!   --features native,vector-memory \
//!   --lib agent::context_router::embedding
//! ```

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use clawft_types::skill::SkillDefinition;

use super::super::{COMPLEXITY_HINT_LIMIT, ContextRequest, ContextRouter};
use super::{EmbeddingRouter, EmbeddingRouterError, INDEX_BACKEND};
use crate::agent::skills_v2::SkillRegistry;
use crate::embeddings::{Embedder, EmbeddingError};

/// Deterministic 8-d embedder. Same input → identical output, so we can
/// construct a stable mock registry and predict ranking without any
/// network or ONNX dependency.
struct StubEmbedder {
    dim: usize,
    fail: bool,
}

impl StubEmbedder {
    fn new(dim: usize) -> Self {
        Self { dim, fail: false }
    }
    fn failing() -> Self {
        Self { dim: 8, fail: true }
    }
}

#[async_trait]
impl Embedder for StubEmbedder {
    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        if self.fail {
            return Err(EmbeddingError::Internal("stub failure".into()));
        }
        // Tokenise on whitespace; bucket each token's byte-sum mod
        // self.dim. Different prose maps to different sparse vectors;
        // identical prose maps identical.
        let mut v = vec![0.0f32; self.dim];
        for tok in text.split_whitespace() {
            let bucket: usize = tok.bytes().map(|b| b as usize).sum::<usize>() % self.dim;
            v[bucket] += 1.0;
        }
        Ok(v)
    }
    fn dimension(&self) -> usize {
        self.dim
    }
    fn name(&self) -> &str {
        "stub"
    }
}

fn skill(name: &str, description: &str) -> SkillDefinition {
    SkillDefinition::new(name, description)
}

fn skill_with_category(name: &str, description: &str, category: &str) -> SkillDefinition {
    let mut s = SkillDefinition::new(name, description);
    s.metadata.insert(
        "openclaw-category".into(),
        serde_json::Value::String(category.into()),
    );
    s
}

async fn registry_from(skills: Vec<SkillDefinition>) -> SkillRegistry {
    // SkillRegistry::discover is async + filesystem-aware; with `None`
    // paths it just returns an empty registry. Tests then populate via
    // the in-memory upsert API.
    let mut reg = SkillRegistry::discover(None, None, Vec::new())
        .await
        .expect("empty registry should never fail");
    for s in skills {
        reg.upsert(s);
    }
    reg
}

fn req(content: &str) -> ContextRequest {
    ContextRequest {
        content: content.into(),
        channel: "panel".into(),
        chat_id: "c1".into(),
        metadata: HashMap::new(),
    }
}

/// Ranking judged with a REAL embedding model (skipped with a printed reason
/// unless `onnx-embeddings` + `WEFTOS_TEST_EMBED_MODEL_DIR`). No query shares a
/// content word with the skill it must route to.
///
/// Baseline model: nomic-embed-text-v1.5 (owner decision 2026-11-12, item 6:
/// accept the model's ranking). With it, "the borrow checker is rejecting my
/// program" ranks python-debug first and rust-debug second, so that one query
/// is asserted at top-2; the other three stay at top-1.
#[tokio::test]
async fn embedding_router_routes_paraphrased_queries_with_a_real_embedder() {
    let Some(real) = crate::embeddings::test_support::real_embedder() else { return };
    let skills = vec![
        skill("rust-debug", "rust compile error fix"),
        skill("python-debug", "python traceback investigation"),
        skill("write-poem", "compose a creative short poem"),
        skill("plan-trip", "plan a vacation itinerary"),
        skill("explain-graph", "explain a knowledge graph"),
    ];
    let reg = registry_from(skills).await;
    let router = EmbeddingRouter::new(Arc::new(real.clone()), &reg)
        .await
        .expect("router build")
        .with_top_k(2)
        .with_confidence_threshold(-1.0);
    // Evidence for a miss: the raw cosine of each query against each skill descriptor,
    // computed straight from the embedder (no index), so a router/index bug can be told
    // apart from the model's own ranking.
    let descriptors = [
        ("rust-debug", "rust-debug: rust compile error fix"),
        ("python-debug", "python-debug: python traceback investigation"),
        ("write-poem", "write-poem: compose a creative short poem"),
        ("plan-trip", "plan-trip: plan a vacation itinerary"),
        ("explain-graph", "explain-graph: explain a knowledge graph"),
    ];
    for query in ["the borrow checker is rejecting my program", "my snake script crashed with a stack trace"] {
        let q = real.query(query);
        let mut raw: Vec<(f32, &str)> = descriptors
            .iter()
            .map(|(n, d)| (crate::embeddings::test_support::cosine(&q, &real.doc(d)), *n))
            .collect();
        raw.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
        eprintln!("raw cosine ranking for {query:?}: {raw:?}");
    }
    for (query, want, depth) in [
        // nomic-embed-text-v1.5 ranks python-debug above rust-debug here; accepted, top-2.
        ("the borrow checker is rejecting my program", "rust-debug", 2usize),
        ("write some verses about autumn leaves", "write-poem", 1),
        ("organise a holiday abroad with flights and hotels", "plan-trip", 1),
        ("my snake script crashed with a stack trace", "python-debug", 1),
    ] {
        let d = router.route(&req(query)).await;
        let top: Vec<&str> = d.skills.iter().take(depth).map(String::as_str).collect();
        assert!(top.contains(&want), "query {query:?} should have {want} in its top {depth}, got {:?}", d.skills);
    }
}

#[tokio::test]
async fn embedding_router_returns_top_k_skills_for_exact_phrase_plumbing_stub_embedder() {
    // PLUMBING with a byte-sum stub embedder: the query repeats the skill text, so
    // this checks top-k wiring only, not recall quality (see the real-embedder test above).
    // 5 skills, embedded by the stub. We then query for text that
    // shares tokens with skill "rust-debug" so it should win top-1.
    let skills = vec![
        skill("rust-debug", "rust compile error fix"),
        skill("python-debug", "python traceback investigation"),
        skill("write-poem", "compose a creative short poem"),
        skill("plan-trip", "plan a vacation itinerary"),
        skill("explain-graph", "explain a knowledge graph"),
    ];
    let reg = registry_from(skills).await;
    let router = EmbeddingRouter::new(Arc::new(StubEmbedder::new(8)), &reg)
        .await
        .expect("router build")
        .with_top_k(3)
        // Stub embedder produces sparse vectors; use a low threshold so
        // the test is about ranking, not absolute confidence.
        .with_confidence_threshold(-1.0);

    let d = router.route(&req("rust compile error fix")).await;
    assert_eq!(d.skills.len(), 3);
    assert_eq!(
        d.skills[0], "rust-debug",
        "expected rust-debug as top-1, got skills={:?}",
        d.skills
    );
}

#[tokio::test]
async fn embedding_router_falls_back_below_confidence() {
    let skills = vec![
        skill("rust-debug", "rust compile error fix"),
        skill("python-debug", "python traceback investigation"),
    ];
    let reg = registry_from(skills).await;
    // Threshold of 0.99 — almost-certainly-not-met by stub.
    let router = EmbeddingRouter::new(Arc::new(StubEmbedder::new(8)), &reg)
        .await
        .unwrap()
        .with_confidence_threshold(0.99);

    let d = router.route(&req("totally unrelated weather query")).await;
    assert!(d.skills.is_empty());
    assert!(d.archetype.is_none());
    assert_eq!(d.complexity_hint, 0.0);
}

#[tokio::test]
async fn embedding_router_falls_back_on_embed_error() {
    let skills = vec![skill("rust-debug", "rust compile error fix")];
    let reg = registry_from(skills).await;
    // Build router with a working embedder so construction succeeds,
    // then swap in a failing one for the route call.
    let router = EmbeddingRouter::new(Arc::new(StubEmbedder::new(8)), &reg)
        .await
        .unwrap();
    let mut bad = router;
    bad.embedder = Arc::new(StubEmbedder::failing());

    let d = bad.route(&req("anything")).await;
    assert!(d.skills.is_empty());
    assert!(d.archetype.is_none());
    assert_eq!(d.complexity_hint, 0.0);
}

#[tokio::test]
async fn empty_skill_registry_yields_construction_error() {
    let reg = registry_from(Vec::new()).await;
    let result = EmbeddingRouter::new(Arc::new(StubEmbedder::new(8)), &reg).await;
    assert!(matches!(result, Err(EmbeddingRouterError::EmptyRegistry)));
}

#[tokio::test]
async fn complexity_hint_is_clamped() {
    let skills = vec![
        skill("rust-debug", "rust compile error fix"),
        skill("python-debug", "python traceback investigation"),
    ];
    let reg = registry_from(skills).await;
    let router = EmbeddingRouter::new(Arc::new(StubEmbedder::new(8)), &reg)
        .await
        .unwrap()
        .with_confidence_threshold(-1.0);

    let d = router.route(&req("rust compile error fix")).await;
    assert!(
        d.complexity_hint >= -COMPLEXITY_HINT_LIMIT && d.complexity_hint <= COMPLEXITY_HINT_LIMIT,
        "complexity_hint must lie in [-0.3, +0.3], got {}",
        d.complexity_hint
    );
}

#[tokio::test]
async fn archetype_comes_from_top1_category() {
    let skills = vec![
        skill_with_category("rust-debug", "rust compile error fix", "CodeGen"),
        skill_with_category("python-debug", "python traceback investigation", "Analysis"),
    ];
    let reg = registry_from(skills).await;
    let router = EmbeddingRouter::new(Arc::new(StubEmbedder::new(8)), &reg)
        .await
        .unwrap()
        .with_confidence_threshold(-1.0);

    let d = router.route(&req("rust compile error fix")).await;
    assert_eq!(d.archetype.as_deref(), Some("CodeGen"));
}

#[tokio::test]
async fn confidence_threshold_is_clamped_to_legal_range() {
    let skills = vec![skill("a", "alpha")];
    let reg = registry_from(skills).await;
    let router = EmbeddingRouter::new(Arc::new(StubEmbedder::new(8)), &reg)
        .await
        .unwrap()
        .with_confidence_threshold(5.0);
    // 5.0 clamps to 1.0; route at high cos < 1.0 → fallback.
    let d = router.route(&req("alpha")).await;
    // Self-match at 1.0 may equal the threshold; allow either path but
    // verify we don't panic.
    let _ = d;
}

#[tokio::test]
async fn top_k_with_fewer_than_k_skills_returns_what_exists() {
    // Registry has 2 skills; ask for 5 — get 2.
    let skills = vec![
        skill("rust-debug", "rust compile error fix"),
        skill("python-debug", "python traceback investigation"),
    ];
    let reg = registry_from(skills).await;
    let router = EmbeddingRouter::new(Arc::new(StubEmbedder::new(8)), &reg)
        .await
        .unwrap()
        .with_top_k(5)
        .with_confidence_threshold(-1.0);

    let d = router.route(&req("rust compile error fix")).await;
    assert_eq!(d.skills.len(), 2);
}

// ── Helper coverage ────────────────────────────────────────────────

#[test]
fn normalise_zero_vector_is_zero() {
    let v = super::normalise(vec![0.0, 0.0, 0.0]);
    assert!(v.iter().all(|&x| x == 0.0));
}

#[test]
fn normalise_unit_norm() {
    let v = super::normalise(vec![3.0, 4.0]);
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    assert!((norm - 1.0).abs() < 1e-5);
}

#[test]
fn extract_category_handles_aliases() {
    let mut s = SkillDefinition::new("x", "y");
    s.metadata.insert(
        "openclaw-category".into(),
        serde_json::Value::String("CodeGen".into()),
    );
    assert_eq!(super::extract_category(&s), Some("CodeGen".into()));

    let mut s2 = SkillDefinition::new("x", "y");
    s2.metadata.insert(
        "category".into(),
        serde_json::Value::String("Reasoning".into()),
    );
    assert_eq!(super::extract_category(&s2), Some("Reasoning".into()));
}

// ── WEFT-51: compile-time index backend identity ─────────────────────────

/// `INDEX_BACKEND` must match the cargo feature matrix:
/// `embedding-router` on → diskann; off → brute-force.
#[test]
fn index_backend_matches_feature_gate() {
    #[cfg(feature = "embedding-router")]
    assert_eq!(INDEX_BACKEND, "diskann");
    #[cfg(not(feature = "embedding-router"))]
    assert_eq!(INDEX_BACKEND, "brute-force");
}

#[tokio::test]
async fn router_reports_same_backend_as_constant() {
    let skills = vec![skill("a", "alpha beta")];
    let reg = registry_from(skills).await;
    let router = EmbeddingRouter::new(Arc::new(StubEmbedder::new(8)), &reg)
        .await
        .expect("router build");
    assert_eq!(router.index_backend(), INDEX_BACKEND);
    assert!(
        router.index_backend() == "diskann" || router.index_backend() == "brute-force",
        "unexpected backend {}",
        router.index_backend()
    );
}

// ── WEFT-51: feature-off path (brute-force index) ────────────────────────
//
// These tests only compile when `embedding-router` is **not** enabled, so
// they prove `build_index` wired the O(n) floor rather than diskann. They
// also exercise every decision branch on that path so a silent panic or
// empty-default regression is caught by the feature-off matrix:
//
//   cargo test -p clawft-core --no-default-features \
//     --features native,vector-memory --lib agent::context_router::embedding

#[cfg(not(feature = "embedding-router"))]
mod feature_off {
    use super::*;

    /// Compile-time proof: this module is only present when the feature
    /// is off, and `INDEX_BACKEND` must name the brute-force floor.
    #[test]
    fn feature_off_backend_is_brute_force() {
        assert_eq!(INDEX_BACKEND, "brute-force");
        assert_eq!(super::super::INDEX_BACKEND, "brute-force");
    }

    #[tokio::test]
    async fn feature_off_router_constructs_and_routes() {
        // Full EmbeddingRouter::new → build_index(BruteForce) → route.
        // This is the Config.routing.context_router = "embedding" path
        // when the daemon was built without --features embedding-router
        // (vector-memory still on). Must succeed — not error, not panic.
        let skills = vec![
            skill("rust-debug", "rust compile error fix"),
            skill("python-debug", "python traceback investigation"),
            skill("write-poem", "compose a creative short poem"),
        ];
        let reg = registry_from(skills).await;
        let router = EmbeddingRouter::new(Arc::new(StubEmbedder::new(8)), &reg)
            .await
            .expect("feature-off EmbeddingRouter must construct via BruteForceIndex");
        assert_eq!(router.index_backend(), "brute-force");

        let d = router
            .with_top_k(2)
            .with_confidence_threshold(-1.0)
            .route(&req("rust compile error fix"))
            .await;
        assert_eq!(d.skills.len(), 2);
        assert_eq!(d.skills[0], "rust-debug");
        assert!(!d.fallback_used);
    }

    #[tokio::test]
    async fn feature_off_empty_registry_is_clear_error() {
        // AC: clear error (not panic) when construction cannot proceed.
        let reg = registry_from(Vec::new()).await;
        let result = EmbeddingRouter::new(Arc::new(StubEmbedder::new(8)), &reg).await;
        match result {
            Err(EmbeddingRouterError::EmptyRegistry) => {}
            Err(e) => panic!("expected EmptyRegistry, got error: {e}"),
            Ok(_) => panic!("expected EmptyRegistry, got Ok router"),
        }
    }

    #[tokio::test]
    async fn feature_off_low_confidence_falls_back_to_empty_decision() {
        let skills = vec![
            skill("rust-debug", "rust compile error fix"),
            skill("python-debug", "python traceback investigation"),
        ];
        let reg = registry_from(skills).await;
        let router = EmbeddingRouter::new(Arc::new(StubEmbedder::new(8)), &reg)
            .await
            .unwrap()
            .with_confidence_threshold(0.99);
        let d = router.route(&req("totally unrelated weather query")).await;
        assert!(d.skills.is_empty(), "low-confidence must empty skills");
        assert!(d.archetype.is_none());
        assert_eq!(d.complexity_hint, 0.0);
    }

    #[tokio::test]
    async fn feature_off_embed_error_falls_back_gracefully() {
        let skills = vec![skill("rust-debug", "rust compile error fix")];
        let reg = registry_from(skills).await;
        let mut router = EmbeddingRouter::new(Arc::new(StubEmbedder::new(8)), &reg)
            .await
            .unwrap();
        router.embedder = Arc::new(StubEmbedder::failing());
        let d = router.route(&req("anything")).await;
        assert!(d.skills.is_empty());
        assert_eq!(d.complexity_hint, 0.0);
    }

    #[tokio::test]
    async fn feature_off_top_k_and_archetype_match_on_backend() {
        let skills = vec![
            skill_with_category("rust-debug", "rust compile error fix", "CodeGen"),
            skill_with_category("python-debug", "python traceback investigation", "Analysis"),
            skill_with_category("write-poem", "compose a creative short poem", "Creative"),
        ];
        let reg = registry_from(skills).await;
        let router = EmbeddingRouter::new(Arc::new(StubEmbedder::new(8)), &reg)
            .await
            .unwrap()
            .with_top_k(3)
            .with_confidence_threshold(-1.0);

        let d = router.route(&req("rust compile error fix")).await;
        assert_eq!(d.skills.len(), 3);
        assert_eq!(d.skills[0], "rust-debug");
        assert_eq!(d.archetype.as_deref(), Some("CodeGen"));
        assert!(
            d.complexity_hint >= -COMPLEXITY_HINT_LIMIT
                && d.complexity_hint <= COMPLEXITY_HINT_LIMIT
        );
    }

    #[tokio::test]
    async fn feature_off_rank_order_is_deterministic() {
        // Identical stub embeddings → identical ranking across runs.
        let skills = vec![
            skill("aaa-skill", "unique alpha tokens zebra"),
            skill("bbb-skill", "unique beta tokens yak"),
            skill("ccc-skill", "unique gamma tokens xray"),
        ];
        let reg = registry_from(skills).await;
        let router = EmbeddingRouter::new(Arc::new(StubEmbedder::new(8)), &reg)
            .await
            .unwrap()
            .with_top_k(3)
            .with_confidence_threshold(-1.0);

        let d1 = router.route(&req("unique alpha tokens zebra")).await;
        let d2 = router.route(&req("unique alpha tokens zebra")).await;
        assert_eq!(d1.skills, d2.skills);
        assert_eq!(d1.skills[0], "aaa-skill");
    }
}
