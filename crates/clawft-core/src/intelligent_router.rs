//! 3-tier intelligent routing with complexity scoring (ADR-026).
//!
//! Routes requests to one of three tiers:
//! - **Tier 1**: Agent Booster (WASM) -- <1ms latency, $0 cost
//! - **Tier 2**: Haiku -- ~500ms latency, low cost (complexity < 0.30)
//! - **Tier 3**: Sonnet/Opus -- 2-5s latency, higher cost (complexity >= 0.30)
//!
//! The router maintains a policy store of cached routing decisions and a
//! cost record history for analytics.
//!
//! This module is gated behind the `vector-memory` feature flag.

use std::collections::HashMap;

use crate::embeddings::{Embedder, EmbeddingError};
use crate::vector_store::VectorStore;

/// A routing decision made by the intelligent router.
#[derive(Debug, Clone)]
pub struct RoutingDecision {
    /// The selected tier (1=WASM, 2=Haiku, 3=Sonnet/Opus).
    pub tier: u8,
    /// The model identifier to use.
    pub model: String,
    /// Human-readable reason for the routing choice.
    pub reason: String,
    /// Computed complexity score (0.0..1.0).
    pub complexity_score: f32,
}

/// A record of cost/latency for a completed request.
#[derive(Debug, Clone)]
pub struct CostRecord {
    /// The model that was used.
    pub model: String,
    /// Number of tokens consumed.
    pub tokens: u64,
    /// Monetary cost in USD.
    pub cost: f32,
    /// End-to-end latency in milliseconds.
    pub latency_ms: u64,
    /// Unix timestamp (seconds since epoch).
    pub timestamp: u64,
}

/// Aggregate statistics for a model.
#[derive(Debug, Clone)]
pub struct ModelStats {
    /// Total number of calls made to the model.
    pub total_calls: u64,
    /// Total monetary cost in USD.
    pub total_cost: f32,
    /// Average latency in milliseconds.
    pub avg_latency_ms: f64,
}

/// Context provided to the router to influence the routing decision.
#[derive(Debug, Clone, Default)]
pub struct RoutingContext {
    /// Tags that can influence routing (e.g. "AGENT_BOOSTER_AVAILABLE").
    pub tags: Vec<String>,
}

/// Errors that can occur during intelligent routing.
#[non_exhaustive]
#[derive(Debug)]
pub enum RoutingError {
    /// An embedding operation failed.
    Embedding(EmbeddingError),
}

impl std::fmt::Display for RoutingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RoutingError::Embedding(e) => write!(f, "routing embedding error: {e}"),
        }
    }
}

impl std::error::Error for RoutingError {}

impl From<EmbeddingError> for RoutingError {
    fn from(e: EmbeddingError) -> Self {
        RoutingError::Embedding(e)
    }
}

/// 3-tier intelligent router per ADR-026.
///
/// Uses a vector store for cached routing policies and an embedder for
/// computing prompt similarity against those policies.
pub struct IntelligentRouter {
    policy_store: VectorStore,
    embedder: Box<dyn Embedder>,
    cost_records: Vec<CostRecord>,
}

impl IntelligentRouter {
    /// Create a new intelligent router with the given embedder.
    pub async fn new(embedder: Box<dyn Embedder>) -> Self {
        Self {
            policy_store: VectorStore::new(),
            embedder,
            cost_records: Vec::new(),
        }
    }

    /// Route a request to the appropriate tier.
    ///
    /// Routing algorithm (in order):
    /// 1. If context tags contain "AGENT_BOOSTER_AVAILABLE" -> Tier 1
    /// 2. Search policy store; if best match > 0.85 -> use cached tier
    /// 3. Compute complexity; < 0.30 -> Tier 2, else -> Tier 3
    pub async fn route_request(
        &self,
        prompt: &str,
        context: &RoutingContext,
    ) -> Result<RoutingDecision, RoutingError> {
        // Rule 1: Agent Booster tag.
        if context.tags.iter().any(|t| t == "AGENT_BOOSTER_AVAILABLE") {
            return Ok(RoutingDecision {
                tier: 1,
                model: "agent-booster-wasm".into(),
                reason: "AGENT_BOOSTER_AVAILABLE tag present".into(),
                complexity_score: 0.0,
            });
        }

        // Rule 2: Check policy store for cached routing.
        let prompt_embedding = self.embedder.embed_query(prompt).await?;
        let policy_results = self.policy_store.search(&prompt_embedding, 5);

        if let Some(best) = policy_results.first()
            && best.score > 0.85
            && let Some(tier_tag) = best.tags.first()
            && let Ok(tier) = tier_tag.parse::<u8>()
        {
            let model = tier_to_model(tier);
            return Ok(RoutingDecision {
                tier,
                model,
                reason: format!(
                    "cached policy match (score={:.3}): {}",
                    best.score, best.text
                ),
                complexity_score: compute_complexity(prompt),
            });
        }

        // Rule 3: Compute complexity.
        let complexity = compute_complexity(prompt);
        if complexity < 0.30 {
            Ok(RoutingDecision {
                tier: 2,
                model: "claude-haiku-3.5".into(),
                reason: format!("low complexity ({complexity:.3}) -> Tier 2"),
                complexity_score: complexity,
            })
        } else {
            Ok(RoutingDecision {
                tier: 3,
                model: "claude-sonnet-4.5".into(),
                reason: format!("high complexity ({complexity:.3}) -> Tier 3"),
                complexity_score: complexity,
            })
        }
    }

    /// Record a cost/latency observation for analytics.
    pub async fn record_cost(&mut self, model: &str, tokens: u64, cost: f32, latency_ms: u64) {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        self.cost_records.push(CostRecord {
            model: model.into(),
            tokens,
            cost,
            latency_ms,
            timestamp,
        });
    }

    /// Update or add a routing policy based on observed patterns.
    ///
    /// If `success` is true, the pattern is cached with the given tier.
    /// If `success` is false, the opposite action is taken (tier is adjusted).
    pub async fn update_policy(&mut self, pattern: &str, tier: u8, success: bool) {
        if !success {
            // On failure, do not cache this routing decision.
            return;
        }

        let embedding = match self.embedder.embed(pattern).await {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!("intelligent_router: policy not cached, embedding failed: {e}");
                return;
            }
        };

        let id = format!("policy-{}", self.policy_store.len());
        self.policy_store.add(
            id,
            pattern.into(),
            embedding,
            vec![tier.to_string()],
            HashMap::new(),
        );
    }

    /// Get aggregate statistics for a specific model.
    pub fn get_model_stats(&self, model: &str) -> ModelStats {
        let matching: Vec<&CostRecord> = self
            .cost_records
            .iter()
            .filter(|r| r.model == model)
            .collect();

        let total_calls = matching.len() as u64;
        let total_cost: f32 = matching.iter().map(|r| r.cost).sum();
        let avg_latency_ms = if matching.is_empty() {
            0.0
        } else {
            matching.iter().map(|r| r.latency_ms as f64).sum::<f64>() / matching.len() as f64
        };

        ModelStats {
            total_calls,
            total_cost,
            avg_latency_ms,
        }
    }
}

/// Compute a complexity score for a prompt.
///
/// Heuristic:
/// - Token count: words / 1000, capped at 0.3
/// - +0.2 if contains code fences (```)
/// - +0.1 if contains reasoning words ("analyze", "design", "architect", "explain", "compare")
/// - +0.2 if contains multi-step indicators ("first", "then", "finally", "step 1")
/// - Result clamped to 0.0..1.0
pub fn compute_complexity(prompt: &str) -> f32 {
    let lower = prompt.to_lowercase();
    let word_count = lower.split_whitespace().count();

    // Base: word count / 1000, capped at 0.3
    let base = (word_count as f32 / 1000.0).min(0.3);

    // Code fences.
    let code_bonus = if lower.contains("```") { 0.2 } else { 0.0 };

    // Reasoning words.
    let reasoning_words = ["analyze", "design", "architect", "explain", "compare"];
    let reasoning_bonus = if reasoning_words.iter().any(|w| lower.contains(w)) {
        0.1
    } else {
        0.0
    };

    // Multi-step indicators.
    let step_words = ["first", "then", "finally", "step 1"];
    let step_bonus = if step_words.iter().any(|w| lower.contains(w)) {
        0.2
    } else {
        0.0
    };

    (base + code_bonus + reasoning_bonus + step_bonus).clamp(0.0, 1.0)
}

/// Map a tier number to a default model name.
fn tier_to_model(tier: u8) -> String {
    match tier {
        1 => "agent-booster-wasm".into(),
        2 => "claude-haiku-3.5".into(),
        _ => "claude-sonnet-4.5".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embeddings::hash_embedder::HashEmbedder;

    /// PLUMBING embedder: equal text maps to equal vectors and nothing more.
    /// Similarity-threshold behaviour is judged with a real embedder below.
    fn make_embedder() -> Box<dyn Embedder> {
        Box::new(HashEmbedder::new(64))
    }

    /// Policy-cache similarity judged with a REAL embedding model (skipped with a
    /// printed reason unless `onnx-embeddings` + `WEFTOS_TEST_EMBED_MODEL_DIR`):
    /// an unrelated prompt must NOT hit a cached policy, and the identical prompt
    /// must. Where a close paraphrase lands relative to the 0.85 threshold is
    /// reported, not asserted: the threshold was calibrated for exact-text hits.
    #[tokio::test]
    #[cfg_attr(not(feature = "real-embedder-tests"), ignore = "needs --features real-embedder-tests and WEFTOS_TEST_EMBED_MODEL_DIR (nomic-embed-text-v1.5 bundle)")]
    async fn real_embedder_policy_cache_hits_identical_and_misses_unrelated() {
        let Some(real) = crate::embeddings::test_support::real_embedder() else { return };
        let mut router = IntelligentRouter::new(Box::new(real.clone())).await;
        let ctx = RoutingContext::default();
        let cached = "convert temperature from celsius to fahrenheit";
        router.update_policy(cached, 2, true).await;

        let same = router.route_request(cached, &ctx).await.unwrap();
        assert!(same.reason.contains("cached policy match"), "{}", same.reason);

        let unrelated = router
            .route_request("summarise the quarterly sales report for the board", &ctx)
            .await
            .unwrap();
        assert!(!unrelated.reason.contains("cached policy match"), "{}", unrelated.reason);

        let para = "change degrees C into degrees F";
        let score = crate::embeddings::test_support::cosine(&real.query(para), &real.doc(cached));
        eprintln!("real-embedder policy cache: paraphrase cosine {score:.3} (threshold 0.85)");
    }

    // ── Tier 1: Agent Booster ──────────────────────────────────────

    #[tokio::test]
    async fn tier1_agent_booster_available() {
        let router = IntelligentRouter::new(make_embedder()).await;
        let ctx = RoutingContext {
            tags: vec!["AGENT_BOOSTER_AVAILABLE".into()],
        };

        let decision = router.route_request("simple task", &ctx).await.unwrap();
        assert_eq!(decision.tier, 1);
        assert_eq!(decision.model, "agent-booster-wasm");
        assert!(decision.reason.contains("AGENT_BOOSTER_AVAILABLE"));
    }

    #[tokio::test]
    async fn tier1_booster_takes_priority_over_complexity() {
        let router = IntelligentRouter::new(make_embedder()).await;
        let ctx = RoutingContext {
            tags: vec!["AGENT_BOOSTER_AVAILABLE".into()],
        };

        // Even a complex prompt should go to Tier 1 with the tag.
        let decision = router
            .route_request(
                "Analyze and design a complex architecture with ```code``` first then finally",
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(decision.tier, 1);
    }

    // ── Tier 2: Simple prompt ──────────────────────────────────────

    #[tokio::test]
    async fn tier2_simple_prompt() {
        let router = IntelligentRouter::new(make_embedder()).await;
        let ctx = RoutingContext::default();

        let decision = router.route_request("hello", &ctx).await.unwrap();
        assert_eq!(decision.tier, 2);
        assert_eq!(decision.model, "claude-haiku-3.5");
        assert!(decision.complexity_score < 0.30);
    }

    #[tokio::test]
    async fn tier2_short_question() {
        let router = IntelligentRouter::new(make_embedder()).await;
        let ctx = RoutingContext::default();

        let decision = router.route_request("what is 2 + 2?", &ctx).await.unwrap();
        assert_eq!(decision.tier, 2);
    }

    // ── Tier 3: Complex prompt ─────────────────────────────────────

    #[tokio::test]
    async fn tier3_complex_prompt_with_code() {
        let router = IntelligentRouter::new(make_embedder()).await;
        let ctx = RoutingContext::default();

        let decision = router
            .route_request("explain this ```rust\nfn main() {}```", &ctx)
            .await
            .unwrap();
        assert_eq!(decision.tier, 3);
        assert_eq!(decision.model, "claude-sonnet-4.5");
        assert!(decision.complexity_score >= 0.30);
    }

    #[tokio::test]
    async fn tier3_complex_prompt_with_reasoning_and_code() {
        let router = IntelligentRouter::new(make_embedder()).await;
        let ctx = RoutingContext::default();

        // Reasoning (0.1) + code fences (0.2) => 0.3+
        let decision = router
            .route_request(
                "analyze the performance of this ```rust\nfn main() {}``` and compare approaches",
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(decision.tier, 3);
        assert!(decision.complexity_score >= 0.30);
    }

    #[tokio::test]
    async fn tier3_complex_prompt_with_steps_and_code() {
        let router = IntelligentRouter::new(make_embedder()).await;
        let ctx = RoutingContext::default();

        // Steps (0.2) + code fences (0.2) => 0.4+
        let decision = router
            .route_request(
                "first set up the database schema ```sql\nCREATE TABLE users```, then create the API, finally deploy",
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(decision.tier, 3);
        assert!(decision.complexity_score >= 0.30);
    }

    // ── Policy cache ───────────────────────────────────────────────

    #[tokio::test]
    async fn policy_cache_update_and_match_plumbing() {
        let mut router = IntelligentRouter::new(make_embedder()).await;
        let ctx = RoutingContext::default();

        // Cache a policy for a specific pattern.
        router
            .update_policy("convert temperature from celsius to fahrenheit", 2, true)
            .await;

        // Route a very similar request.
        let decision = router
            .route_request("convert temperature from celsius to fahrenheit", &ctx)
            .await
            .unwrap();

        // Should use cached policy (Tier 2).
        assert_eq!(decision.tier, 2);
        assert!(decision.reason.contains("cached policy match"));
    }

    #[tokio::test]
    async fn policy_cache_failure_not_stored_plumbing() {
        let mut router = IntelligentRouter::new(make_embedder()).await;

        // Report a failure -- should NOT be cached.
        router
            .update_policy("test pattern that failed", 2, false)
            .await;

        // Policy store should be empty.
        assert!(router.policy_store.is_empty());
    }

    // ── Cost recording and stats ────────────────────────────────────

    #[tokio::test]
    async fn cost_recording_and_stats() {
        let mut router = IntelligentRouter::new(make_embedder()).await;

        router
            .record_cost("claude-haiku-3.5", 100, 0.001, 500)
            .await;
        router
            .record_cost("claude-haiku-3.5", 200, 0.002, 600)
            .await;
        router
            .record_cost("claude-sonnet-4.5", 500, 0.01, 2000)
            .await;

        let haiku_stats = router.get_model_stats("claude-haiku-3.5");
        assert_eq!(haiku_stats.total_calls, 2);
        assert!((haiku_stats.total_cost - 0.003).abs() < 0.0001);
        assert!((haiku_stats.avg_latency_ms - 550.0).abs() < 0.01);

        let sonnet_stats = router.get_model_stats("claude-sonnet-4.5");
        assert_eq!(sonnet_stats.total_calls, 1);
        assert!((sonnet_stats.total_cost - 0.01).abs() < 0.0001);
        assert!((sonnet_stats.avg_latency_ms - 2000.0).abs() < 0.01);
    }

    #[tokio::test]
    async fn stats_for_unknown_model() {
        let router = IntelligentRouter::new(make_embedder()).await;
        let stats = router.get_model_stats("unknown-model");
        assert_eq!(stats.total_calls, 0);
        assert!((stats.total_cost - 0.0).abs() < f32::EPSILON);
        assert!((stats.avg_latency_ms - 0.0).abs() < f64::EPSILON);
    }

    // ── compute_complexity tests ───────────────────────────────────

    #[test]
    fn complexity_empty_prompt() {
        let c = compute_complexity("");
        assert!((c - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn complexity_short_prompt() {
        let c = compute_complexity("hello world");
        assert!(c < 0.30, "short prompt should be low complexity: {c}");
    }

    #[test]
    fn complexity_code_fences() {
        let c = compute_complexity("look at this ```code here```");
        assert!(c >= 0.2, "code fences should add 0.2: {c}");
    }

    #[test]
    fn complexity_reasoning_words() {
        let c = compute_complexity("analyze this data");
        assert!(c >= 0.1, "reasoning words should add 0.1: {c}");
    }

    #[test]
    fn complexity_multi_step() {
        let c = compute_complexity("first do this then do that");
        assert!(c >= 0.2, "multi-step indicators should add 0.2: {c}");
    }

    #[test]
    fn complexity_all_heuristics() {
        let c = compute_complexity(
            "first analyze the code ```fn main()``` then design the architecture finally deploy",
        );
        // base (small) + code(0.2) + reasoning(0.1) + steps(0.2) = ~0.5+
        assert!(c >= 0.5, "all heuristics should sum up: {c}");
        assert!(c <= 1.0, "should be clamped to 1.0: {c}");
    }

    #[test]
    fn complexity_clamped_to_max() {
        // Even with everything, should not exceed 1.0.
        let c = compute_complexity(
            "first analyze then design the architect explain compare ```code``` finally step 1",
        );
        assert!(c <= 1.0, "must be clamped to 1.0: {c}");
    }

    #[test]
    fn complexity_case_insensitive() {
        let c1 = compute_complexity("ANALYZE this");
        let c2 = compute_complexity("analyze this");
        assert!(
            (c1 - c2).abs() < f32::EPSILON,
            "should be case-insensitive: {c1} vs {c2}"
        );
    }

    // ── tier_to_model ──────────────────────────────────────────────

    #[test]
    fn tier_to_model_mapping() {
        assert_eq!(tier_to_model(1), "agent-booster-wasm");
        assert_eq!(tier_to_model(2), "claude-haiku-3.5");
        assert_eq!(tier_to_model(3), "claude-sonnet-4.5");
        assert_eq!(tier_to_model(99), "claude-sonnet-4.5"); // fallback
    }

    // ── RoutingContext default ──────────────────────────────────────

    #[test]
    fn routing_context_default() {
        let ctx = RoutingContext::default();
        assert!(ctx.tags.is_empty());
    }

    // ── RoutingError display ───────────────────────────────────────

    #[test]
    fn routing_error_display() {
        let err = RoutingError::Embedding(EmbeddingError::Internal("test".into()));
        assert!(format!("{err}").contains("test"));
    }
}
