//! Sona-backed [`SkillReranker`](super::rerank::SkillReranker) (WEFT-46).
//!
//! Gated behind the `hybrid-rerank` feature. Wraps `ruvector-sona`'s
//! [`SonaEngine`](ruvector_sona::SonaEngine):
//!
//! 1. Hash-embed the query (deterministic, offline-friendly floor —
//!    production can later swap in real skill embeddings).
//! 2. Apply MicroLoRA residual transform on the query embedding.
//! 3. Score each candidate with prior + cosine(query_opt, skill_emb) +
//!    ReasoningBank pattern boost.
//! 4. Day-0 fail-open: when the ReasoningBank is empty **and** MicroLoRA
//!    has not moved the query embedding, preserve primary order so
//!    untrained SONA never regresses retrieval quality.
//!
//! Learning is optional and offline to the hot path: call
//! [`SonaSkillReranker::observe`] after a turn outcome so future
//! reranks can crystallize patterns via `force_learn` / background tick.
//!
//! Follow-on (ws06-memory, RMM arXiv:2503.08026): generalize `observe` from
//! one `(skill, quality)` to per-retrieved-memory `(id, +1 cited | −1
//! ignored)` and attach a memory-candidate rerank, not only skills. Do not
//! RL-update the dense retriever (paper ablation collapsed Acc 58.8→31.0).
//! See `docs/research/rmm-reflective-memory-management.md`.

use std::sync::Arc;
use std::time::Instant;

use ruvector_sona::{SonaConfig, SonaEngine};

use super::rerank::{RerankCandidate, SkillReranker};

/// Default hidden / embedding dim for the hash floor + MicroLoRA.
///
/// Matches SONA's documented "edge" sweet-spot (small enough for
/// sub-ms forward, large enough for ~35 skill ids to separate under
/// hashing). Tunable via [`SonaSkillReranker::with_config`].
pub const DEFAULT_SONA_DIM: usize = 64;

/// Weight applied to cosine(query_opt, skill_emb) when combining with
/// the primary prior. Primary prior is O(1); cosine is ∈ [-1, 1].
const COSINE_WEIGHT: f32 = 0.35;

/// Weight applied to ReasoningBank pattern boost.
const PATTERN_WEIGHT: f32 = 0.5;

/// Minimum L2 delta on the MicroLoRA residual before we treat the
/// adapter as "trained enough to move ranking". Below this and with
/// an empty pattern bank we preserve primary order.
const LORA_DELTA_EPS: f32 = 1e-6;

/// Tracing target for latency / accuracy instrumentation.
pub const RERANK_TRACING_TARGET: &str = "context_router.hybrid_rerank";

/// Text -> vector function used to embed queries and skill ids.
pub type TextEmbedFn = Arc<dyn Fn(&str) -> Vec<f32> + Send + Sync>;

/// Sona-backed skill reranker for HybridRouter v2.5.
///
/// [`new`](Self::new), [`with_config`](Self::with_config) and
/// [`from_engine`](Self::from_engine) embed with the LEXICAL [`hash_embed`]
/// (shared words only, not semantic). For semantic reranking build it with
/// [`with_text_embedder`](Self::with_text_embedder) and a real model.
pub struct SonaSkillReranker {
    engine: Arc<SonaEngine>,
    dim: usize,
    /// How many ReasoningBank neighbours to consult.
    pattern_k: usize,
    embed: TextEmbedFn,
}

impl SonaSkillReranker {
    /// Build with a fresh default-configured [`SonaEngine`].
    pub fn new() -> Self {
        Self::with_config(SonaConfig {
            hidden_dim: DEFAULT_SONA_DIM,
            embedding_dim: DEFAULT_SONA_DIM,
            // Smaller cluster count for the ~35-skill catalog; the
            // crate default of 100 is tuned for large LLM-router corpora.
            pattern_clusters: 16,
            trajectory_capacity: 1024,
            quality_threshold: 0.15,
            ..SonaConfig::default()
        })
    }

    /// Build with an explicit SONA config (tests / tuning). Lexical hash embedding.
    pub fn with_config(config: SonaConfig) -> Self {
        let dim = config.hidden_dim.max(1);
        Self {
            engine: Arc::new(SonaEngine::with_config(config)),
            dim,
            pattern_k: 3,
            embed: Arc::new(move |t| hash_embed(t, dim)),
        }
    }

    /// Build with a real text embedder. `config.hidden_dim` and
    /// `config.embedding_dim` must equal the embedder's output width.
    pub fn with_text_embedder(config: SonaConfig, embed: TextEmbedFn) -> Self {
        let dim = config.hidden_dim.max(1);
        Self {
            engine: Arc::new(SonaEngine::with_config(config)),
            dim,
            pattern_k: 3,
            embed,
        }
    }

    /// Wrap an existing shared engine (share one engine across
    /// observe + rerank + tick paths).
    pub fn from_engine(engine: Arc<SonaEngine>) -> Self {
        let dim = engine.config().hidden_dim.max(1);
        Self {
            engine,
            dim,
            pattern_k: 3,
            embed: Arc::new(move |t| hash_embed(t, dim)),
        }
    }

    /// Override how many ReasoningBank neighbours feed the pattern boost.
    #[must_use]
    pub fn with_pattern_k(mut self, k: usize) -> Self {
        self.pattern_k = k.max(1);
        self
    }

    /// Shared engine handle (for `tick` / `force_learn` / export).
    pub fn engine(&self) -> &Arc<SonaEngine> {
        &self.engine
    }

    /// Record a turn outcome so SONA can learn which skill worked.
    ///
    /// `quality` ∈ [0, 1]. Call from the agent-loop feedback path once
    /// available; the hot `rerank` path never depends on this.
    ///
    /// RMM follow-on: this scalar is the skill-routing analogue of
    /// citation rewards. Memory rerank wants a list of `(id, ±1)` instead
    /// of one quality — keep the retriever frozen.
    pub fn observe(&self, query: &str, selected_skill: &str, quality: f32) {
        let q = (self.embed)(query);
        let skill_emb = (self.embed)(selected_skill);
        if q.len() != self.dim || skill_emb.len() != self.dim {
            tracing::warn!(
                target: RERANK_TRACING_TARGET,
                expected = self.dim, got = q.len(),
                "SonaSkillReranker: embedder width does not match the SONA config; observation skipped"
            );
            return;
        }
        let mut builder = self.engine.begin_trajectory(q);
        builder.add_step(skill_emb, vec![], quality.clamp(0.0, 1.0));
        builder.add_context(selected_skill);
        builder.set_model_route(selected_skill);
        self.engine
            .end_trajectory(builder, quality.clamp(0.0, 1.0));
    }

    /// Force a background learning cycle (test / admin hook).
    pub fn force_learn(&self) -> String {
        self.engine.force_learn()
    }

    /// Flush instant-loop MicroLoRA updates.
    pub fn flush(&self) {
        self.engine.flush();
    }
}

/// Memory rerank (WEFT-732): the same engine scores retrieved memory snippets by their text, and
/// learns from per-candidate citation rewards. Day-0 fail-open applies (untrained = retriever
/// order). The retriever is never touched from here.
#[cfg(feature = "vector-memory")]
impl crate::agent::memory_recall::MemoryReranker for SonaSkillReranker {
    fn rerank(
        &self,
        query: &str,
        candidates: Vec<crate::agent::memory_recall::MemorySnippet>,
    ) -> Vec<crate::agent::memory_recall::MemorySnippet> {
        // Reciprocal-rank priors from the retriever order, scored by snippet text.
        let primary: Vec<RerankCandidate> = candidates
            .iter()
            .enumerate()
            .map(|(i, s)| RerankCandidate::new(s.text.clone(), 1.0 / (i as f32 + 1.0)))
            .collect();
        let order = SkillReranker::rerank(self, query, &primary);
        let mut pool = candidates;
        order
            .into_iter()
            .filter_map(|c| pool.iter().position(|s| s.text == c.id).map(|i| pool.remove(i)))
            .collect()
    }

    /// `+1` (cited) is quality 1.0, `-1` (retrieved, not cited) is 0.0 — one trajectory each.
    fn observe(&self, query: &str, rewards: &[(crate::agent::memory_recall::MemorySnippet, i8)]) {
        for (snip, r) in rewards {
            SonaSkillReranker::observe(self, query, &snip.text, if *r > 0 { 1.0 } else { 0.0 });
        }
    }
}

impl Default for SonaSkillReranker {
    fn default() -> Self {
        Self::new()
    }
}

impl SkillReranker for SonaSkillReranker {
    fn rerank(&self, query: &str, candidates: &[RerankCandidate]) -> Vec<RerankCandidate> {
        if candidates.len() <= 1 {
            return candidates.to_vec();
        }

        let started = Instant::now();
        let q = (self.embed)(query);
        if q.len() != self.dim {
            tracing::warn!(
                target: RERANK_TRACING_TARGET,
                expected = self.dim, got = q.len(),
                "SonaSkillReranker: embedder width does not match the SONA config; preserving primary order"
            );
            return candidates.to_vec();
        }

        // Residual MicroLoRA: start from the query embedding so an
        // untrained (zero up-proj) adapter leaves the vector intact.
        let mut q_opt = q.clone();
        self.engine.apply_micro_lora(&q, &mut q_opt);

        let lora_delta: f32 = q
            .iter()
            .zip(q_opt.iter())
            .map(|(a, b)| (a - b).abs())
            .sum();

        let patterns = self.engine.find_patterns(&q_opt, self.pattern_k);

        // Day-0 fail-open: no learned signal → preserve primary order.
        if patterns.is_empty() && lora_delta < LORA_DELTA_EPS {
            tracing::debug!(
                target: RERANK_TRACING_TARGET,
                latency_us = started.elapsed().as_micros() as u64,
                "SonaSkillReranker: no learned signal; preserving primary order"
            );
            return candidates.to_vec();
        }

        let mut scored: Vec<(usize, RerankCandidate)> = candidates
            .iter()
            .cloned()
            .enumerate()
            .map(|(idx, mut c)| {
                let skill_emb = (self.embed)(&c.id);
                let cos = cosine(&q_opt, &skill_emb);
                let mut pattern_boost = 0.0f32;
                for p in &patterns {
                    let sim = p.similarity(&skill_emb);
                    if sim > 0.0 {
                        pattern_boost += p.avg_quality * sim;
                    }
                }
                c.score = c.score + COSINE_WEIGHT * cos + PATTERN_WEIGHT * pattern_boost;
                (idx, c)
            })
            .collect();

        // Higher score wins; stable on ties via original index.
        scored.sort_by(|a, b| {
            b.1.score
                .partial_cmp(&a.1.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });

        let out: Vec<RerankCandidate> = scored.into_iter().map(|(_, c)| c).collect();

        tracing::debug!(
            target: RERANK_TRACING_TARGET,
            latency_us = started.elapsed().as_micros() as u64,
            patterns = patterns.len(),
            lora_delta,
            top = %out.first().map(|c| c.id.as_str()).unwrap_or(""),
            "SonaSkillReranker: reranked primary top-K"
        );

        out
    }
}

/// Deterministic hash embedding (FNV-1a seeded per-dim): LEXICAL, not semantic.
///
/// Same string always maps to the same unit vector; paraphrases are unrelated.
/// Only the `SonaSkillReranker` lexical constructors use it. Day-0 SONA still preserves primary order
/// (see fail-open above); learned patterns + MicroLoRA supply the
/// adaptive signal over time.
pub fn hash_embed(text: &str, dim: usize) -> Vec<f32> {
    let dim = dim.max(1);
    let mut v = vec![0.0f32; dim];
    if text.is_empty() {
        return v;
    }
    // Tokenise on whitespace + whole-string so short skill ids still
    // get mass in multiple dims.
    let mut tokens: Vec<&str> = text.split_whitespace().collect();
    tokens.push(text);
    for (ti, token) in tokens.iter().enumerate() {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325; // FNV offset basis
        for b in token.bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        // Spread one token across a few dims.
        for k in 0u64..4 {
            let salt = k.wrapping_mul(0x9e37_79b9_7f4a_7c15);
            let idx = (h.wrapping_add(salt) as usize) % dim;
            let sign = if (h >> (k * 8)) & 1 == 0 { 1.0 } else { -1.0 };
            // Slight position decay so the full-string token dominates.
            let w = 1.0 / (1.0 + ti as f32 * 0.25);
            v[idx] += sign * w;
        }
    }
    // L2 normalise.
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > f32::EPSILON {
        for x in &mut v {
            *x /= norm;
        }
    }
    v
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    let denom = na.sqrt() * nb.sqrt();
    if denom > f32::EPSILON {
        (dot / denom).clamp(-1.0, 1.0)
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn hash_embed_is_deterministic_and_unit() {
        let a = hash_embed("code-review", DEFAULT_SONA_DIM);
        let b = hash_embed("code-review", DEFAULT_SONA_DIM);
        assert_eq!(a, b);
        let n: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((n - 1.0).abs() < 1e-4, "norm={n}");
    }

    #[test]
    fn day0_preserves_primary_order() {
        let rr = SonaSkillReranker::new();
        let cands = vec![
            RerankCandidate::new("alpha", 1.0),
            RerankCandidate::new("beta", 0.5),
            RerankCandidate::new("gamma", 0.33),
        ];
        let out = rr.rerank("please help with X", &cands);
        assert_eq!(
            out.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            vec!["alpha", "beta", "gamma"]
        );
    }

    /// Reranking with a REAL embedder (skipped with a printed reason unless
    /// `onnx-embeddings` + `WEFTOS_TEST_EMBED_MODEL_DIR`): the untrained contract
    /// holds, the latency budget holds including the embedding, and after
    /// training toward one skill the full candidate set is still returned. It
    /// reports where the trained skill lands (hit@1 / rank) instead of asserting
    /// a learning-efficacy number SONA does not guarantee.
    #[test]
    fn bench_rerank_with_real_embedder_latency_and_contract() {
        use crate::embeddings::test_support::{TEST_MODEL_DIMS, real_embedder};
        let Some(emb) = real_embedder() else { return };
        const K: usize = 8;
        let skills = [
            "code-review", "write-poem", "plan-trip", "debug-rust-compile-error",
            "summarize-meeting", "translate-text", "sql-query-tuning", "draft-email",
        ];
        let cands: Vec<RerankCandidate> = skills
            .iter()
            .enumerate()
            .map(|(i, s)| RerankCandidate::new(*s, 1.0 / (i as f32 + 1.0)))
            .collect();
        let e = emb.clone();
        let sona = SonaSkillReranker::with_text_embedder(
            SonaConfig {
                hidden_dim: TEST_MODEL_DIMS,
                embedding_dim: TEST_MODEL_DIMS,
                pattern_clusters: 16,
                trajectory_capacity: 1024,
                quality_threshold: 0.15,
                ..SonaConfig::default()
            },
            Arc::new(move |t: &str| e.doc(t)),
        );
        let query = "the compiler keeps rejecting my borrow checker usage";

        // Warm the embedding cache so the budget measures rerank, not model load.
        let _ = sona.rerank(query, &cands);
        let t = Instant::now();
        let out = sona.rerank(query, &cands);
        let day0_us = t.elapsed().as_micros();
        assert_eq!(out[0].id, skills[0], "untrained SONA must preserve primary top-1 (day-0 fail-open)");
        assert!(day0_us < 3_000, "day-0 rerank with real embeddings took {day0_us} us, over the 3 ms budget");

        for i in 0..40 {
            sona.observe(&format!("{query} variant {i}"), "debug-rust-compile-error", 0.95);
        }
        sona.force_learn();
        sona.flush();
        let trained = sona.rerank(query, &cands);
        assert_eq!(trained.len(), K, "trained rerank must keep the full candidate set");
        let rank = trained.iter().position(|c| c.id == "debug-rust-compile-error").unwrap() + 1;
        eprintln!("real-embedder SONA bench: day0 {day0_us} us; trained skill rank {rank}/{K}");
    }

    #[test]
    fn observe_and_force_learn_do_not_panic() {
        let rr = SonaSkillReranker::new();
        for _ in 0..20 {
            rr.observe("refactor this rust module", "code-review", 0.9);
        }
        let msg = rr.force_learn();
        assert!(msg.contains("trajectories"), "got: {msg}");
        rr.flush();
        // After learning the path still returns a full id set.
        let cands = vec![
            RerankCandidate::new("chat", 1.0),
            RerankCandidate::new("code-review", 0.5),
        ];
        let out = rr.rerank("refactor this rust module", &cands);
        assert_eq!(out.len(), 2);
        let mut ids: Vec<_> = out.iter().map(|c| c.id.as_str()).collect();
        ids.sort_unstable();
        assert_eq!(ids, vec!["chat", "code-review"]);
    }

    /// Benchmark: rerank-on vs rerank-off latency + day-0 accuracy.
    ///
    /// - **Latency**: Identity (off) vs SonaSkillReranker (on) over N
    ///   calls; on path must stay under a generous microsecond budget
    ///   for the ~35-skill catalog (spec target ~1–3 ms).
    /// - **Accuracy**: day-0 (untrained) hit@1 equals primary top-1
    ///   (no regression). Trained path still returns a full set.
    #[test]
    fn bench_rerank_on_vs_off_latency_and_accuracy() {
        const N: usize = 200;
        const K: usize = 8;
        let skills: Vec<String> = (0..K).map(|i| format!("skill-{i}")).collect();
        let cands: Vec<RerankCandidate> = skills
            .iter()
            .enumerate()
            .map(|(i, s)| RerankCandidate::new(s.clone(), 1.0 / (i as f32 + 1.0)))
            .collect();
        let query = "how do I wire HybridRouter sona rerank";

        // --- off ---
        let identity = super::super::rerank::IdentityReranker;
        let t0 = Instant::now();
        let mut off_top1_hits = 0usize;
        for _ in 0..N {
            let out = identity.rerank(query, &cands);
            if out.first().map(|c| c.id.as_str()) == Some("skill-0") {
                off_top1_hits += 1;
            }
        }
        let off_us = t0.elapsed().as_micros() as f64 / N as f64;

        // --- on (day-0, untrained) ---
        let sona = SonaSkillReranker::new();
        let t1 = Instant::now();
        let mut on_top1_hits = 0usize;
        for _ in 0..N {
            let out = sona.rerank(query, &cands);
            if out.first().map(|c| c.id.as_str()) == Some("skill-0") {
                on_top1_hits += 1;
            }
        }
        let on_us = t1.elapsed().as_micros() as f64 / N as f64;

        // Day-0 accuracy: must match identity (no regression).
        assert_eq!(off_top1_hits, N, "identity must always keep primary top-1");
        assert_eq!(
            on_top1_hits, N,
            "untrained SONA must preserve primary top-1 (day-0 fail-open)"
        );

        // Latency budget: 3 ms = 3000 µs per call (rvf-context-router
        // budget for optional SONA rerank). Hash+MicroLoRA on dim=64
        // is typically well under 100 µs; use 3000 µs as the hard cap.
        assert!(
            on_us < 3000.0,
            "sona rerank mean latency {on_us:.1} µs exceeds 3 ms budget"
        );

        eprintln!(
            "WEFT-46 bench: off={off_us:.2} µs/call hit@1={off_top1_hits}/{N}; \
             on={on_us:.2} µs/call hit@1={on_top1_hits}/{N} (day-0)"
        );

        // --- on (after observe + force_learn) still contract-safe ---
        for i in 0..40 {
            // Prefer skill-3 for this query family.
            let q = format!("{query} variant {i}");
            sona.observe(&q, "skill-3", 0.95);
        }
        sona.force_learn();
        sona.flush();
        let trained = sona.rerank(query, &cands);
        assert_eq!(trained.len(), K, "trained rerank must keep full candidate set");
        let mut ids: Vec<_> = trained.iter().map(|c| c.id.clone()).collect();
        ids.sort();
        let mut expected = skills.clone();
        expected.sort();
        assert_eq!(ids, expected);
    }

    #[cfg(feature = "vector-memory")]
    #[test]
    fn untrained_memory_rerank_keeps_retriever_order_and_observe_accepts_rewards() {
        use crate::agent::memory_recall::{split_snippets, MemoryReranker};
        let r = SonaSkillReranker::new();
        let snips = split_snippets("alpha fact\n\nbeta fact\n\ngamma fact");
        let out = MemoryReranker::rerank(&r, "beta", snips.clone());
        assert_eq!(out, snips, "day-0 fail-open: retriever order preserved");
        MemoryReranker::observe(&r, "beta", &[(snips[1].clone(), 1), (snips[0].clone(), -1)]);
        // Reranking after observing still returns every candidate exactly once.
        let after = MemoryReranker::rerank(&r, "beta", snips.clone());
        assert_eq!(after.len(), 3);
        for s in &snips {
            assert_eq!(after.iter().filter(|x| *x == s).count(), 1);
        }
    }

}
