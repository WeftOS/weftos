//! Tests for the L2 [`SessionTier`] (ADR-058 Phase 5). Deterministic: a Mock
//! embedder + an in-memory `ChainManager`, no live model. Split to a sibling
//! file for the <500-line rule.

use super::*;
use clawft_kernel::chain::ChainManager;
use clawft_kernel::embedding::MockEmbeddingProvider;
use clawft_kernel::{CausalEdgeType, CausalGraph, CrossRefStore, CrossRefType, ViewResolver};
use serde_json::json;

use crate::session_forest::{speaker_universal_id, turn_universal_id};

fn make_tier(dims: usize) -> (SessionTier, Arc<ChainManager>) {
    let embedder: Arc<dyn EmbeddingProvider> = Arc::new(MockEmbeddingProvider::new(dims));
    let chain = Arc::new(ChainManager::new(0, 1000));
    (SessionTier::new(embedder, chain.clone(), None), chain)
}

/// Build a forest-joined tier (ADR-062 §1.1) plus handles to assert against.
fn make_forest_tier(
    dims: usize,
) -> (
    SessionTier,
    Arc<ChainManager>,
    Arc<CausalGraph>,
    Arc<CrossRefStore>,
) {
    let embedder: Arc<dyn EmbeddingProvider> = Arc::new(MockEmbeddingProvider::new(dims));
    let chain = Arc::new(ChainManager::new(0, 1000));
    let causal = Arc::new(CausalGraph::new());
    let crossrefs = Arc::new(CrossRefStore::new());
    let tier = SessionTier::new(embedder, chain.clone(), None)
        .with_forest(causal.clone(), crossrefs.clone());
    (tier, chain, causal, crossrefs)
}

/// Append a turn to the chain (as the substrate sink does) and index it into
/// the tier; returns the assigned chain sequence (the witness).
async fn append_and_index(tier: &SessionTier, chain: &ChainManager, conv: &str, text: &str) -> u64 {
    append_and_index_as(tier, chain, conv, "user", text).await
}

/// Append + index a turn with an explicit speaker role.
async fn append_and_index_as(
    tier: &SessionTier,
    chain: &ChainManager,
    conv: &str,
    role: &str,
    text: &str,
) -> u64 {
    let ev = chain.append(
        "agent",
        "agent.chat.turn",
        Some(json!({ "conv": conv, "content": text })),
    );
    tier.index_turn(conv, ev.sequence, "agent.chat.turn", role, text, None)
        .await;
    ev.sequence
}

/// PLUMBING (Mock embedder): index then graft the identical string and check the
/// witness citation is rendered. Recall quality is judged by
/// `graft_recalls_a_paraphrase_with_witness_real_embedder`.
#[tokio::test]
async fn index_then_graft_exact_text_cites_witness_plumbing_mock_embedder() {
    let (tier, chain) = make_tier(64);
    let s = append_and_index(&tier, &chain, "c1", "the launch code is ZULU-7").await;

    let block = tier.graft_block("c1", "the launch code is ZULU-7").await;
    assert_eq!(block.len(), 1);
    let body = &block[0].content;
    assert!(
        body.contains("ZULU-7"),
        "graft should recall the fact: {body}"
    );
    assert!(
        body.contains(&format!("chain_seq {s}")),
        "graft should cite the witness seq: {body}"
    );
}

/// Recall with a REAL embedding model (WEFTOS_TEST_EMBED_MODEL_DIR; skipped with
/// a printed reason otherwise): the query is a paraphrase sharing no keyword
/// with the stored turn, and the graft still cites that turn's witness seq.
#[tokio::test]
async fn graft_recalls_a_paraphrase_with_witness_real_embedder() {
    let Some(embedder) = clawft_kernel::embedding_support::real_embedder() else { return };
    let chain = Arc::new(ChainManager::new(0, 1000));
    // k = 1: only the single nearest turn may come back, so the ranking is tested.
    let tier = SessionTier::new(embedder, chain.clone(), None).with_graft_top_k(1);
    let conv = "real-recall";
    let s_fact = append_and_index(&tier, &chain, conv, "the launch code for the rocket is ZULU-7").await;
    for text in [
        "please remind me to water the plants on Friday",
        "the quarterly invoice total was ninety thousand dollars",
        "I prefer my coffee black with no sugar",
    ] {
        append_and_index(&tier, &chain, conv, text).await;
    }

    let block = tier.graft_block(conv, "what is the secret to start the spacecraft?").await;
    assert_eq!(block.len(), 1, "a real embedder must recall the top turn");
    let top = &block[0].content;
    assert!(!top.contains("water the plants") && !top.contains("coffee"), "only the nearest turn: {top}");
    assert!(top.contains("ZULU-7"), "top graft should be the launch-code turn: {top}");
    assert!(top.contains(&format!("chain_seq {s_fact}")), "must cite the witness seq: {top}");
}

#[tokio::test]
async fn graft_empty_for_unknown_conversation() {
    let (tier, _chain) = make_tier(64);
    assert!(tier.graft_block("never-seen", "q").await.is_empty());
}

/// HEADLINE (ADR-058 Phase 5.2): a fact stated early, pruned from the live
/// window, is re-grafted later in the SAME session WITH its witness chain.
///
/// Deterministic — a Mock embedder makes embeddings content-addressed, so a
/// content-identical re-query exercises the index→prune→regraft machinery
/// without a live model. (Semantic, non-identical recall needs the live Qwen3
/// embedder; that path is covered by the #[ignore]'d live tests.)
#[tokio::test]
async fn pruned_fact_is_regrafted_with_witness_chain() {
    let (tier, chain) = make_tier(64);
    let conv = "recall-session";
    let fact = "my favorite color is teal";

    // Turn 1: state the fact early — appended to the chain (witness) + indexed.
    let s_fact = append_and_index(&tier, &chain, conv, fact).await;

    // ... several later turns grow the conversation ...
    for i in 0..4 {
        append_and_index(
            &tier,
            &chain,
            conv,
            &format!("unrelated chatter number {i}"),
        )
        .await;
    }

    // Window overflow: prune to the single most-recent live chunk. The early
    // fact is evicted (Stale) but stays on the chain and in the index.
    let pruned = tier.prune_to_recent(conv, 1);
    assert!(pruned >= 1, "older chunks should be pruned");

    // Later in the SAME session: recall the fact. Despite being pruned from the
    // live window, it is re-grafted via retrieval.
    let block = tier.graft_block(conv, fact).await;
    assert_eq!(block.len(), 1);
    let body = &block[0].content;
    assert!(
        body.contains("teal"),
        "pruned fact must be recalled: {body}"
    );
    assert!(
        body.contains(&format!("chain_seq {s_fact}")),
        "must cite the witness seq {s_fact}: {body}"
    );

    // The witness chain is verifiable: the cited sequence is a real
    // agent.chat.turn event on the chain.
    let witness = chain
        .tail_from(s_fact - 1)
        .into_iter()
        .find(|e| e.sequence == s_fact)
        .expect("witness event present on chain");
    assert_eq!(witness.kind, "agent.chat.turn");
}

#[tokio::test]
async fn promote_emits_memory_promote_and_drops_view() {
    let (tier, chain) = make_tier(64);
    let conv = "promote-session";
    let s = append_and_index(&tier, &chain, conv, "remember the api base is https://x").await;
    assert!(tier.mark_important(conv, s));

    let promoted_seq = tier
        .promote_and_drop(conv, "api base = https://x")
        .expect("promotion event emitted");

    // The promotion is a real memory.promote event on the chain (lineage).
    let ev = chain
        .tail_from(promoted_seq - 1)
        .into_iter()
        .find(|e| e.sequence == promoted_seq)
        .expect("memory.promote event present");
    assert_eq!(ev.kind, "memory.promote");

    // View dropped after promotion: a subsequent graft finds nothing.
    assert!(tier.graft_block(conv, "api base").await.is_empty());
}

#[tokio::test]
async fn prune_noop_when_under_keep() {
    let (tier, chain) = make_tier(64);
    append_and_index(&tier, &chain, "c", "only one").await;
    assert_eq!(tier.prune_to_recent("c", 5), 0);
}

// ── ADR-062 §1.1 — dual-write turns into the forest ──────────────────────

/// HEADLINE (ADR-062 §1.1): indexing a turn lands a chain event AND a causal
/// node; the second turn draws a `Follows` lineage edge to the first AND
/// registers a speaker cross-ref.
#[tokio::test]
async fn index_dual_writes_causal_node_follows_edge_and_speaker_crossref() {
    let (tier, chain, causal, crossrefs) = make_forest_tier(64);
    let conv = "forest-c1";

    let s1 = append_and_index_as(&tier, &chain, conv, "user", "first turn").await;
    let s2 = append_and_index_as(&tier, &chain, conv, "assistant", "second turn").await;

    // Chain events landed (witness): both sequences are real agent.chat.turn
    // events on the chain.
    for s in [s1, s2] {
        let ev = chain
            .tail_from(s - 1)
            .into_iter()
            .find(|e| e.sequence == s)
            .expect("witness event present");
        assert_eq!(ev.kind, "agent.chat.turn");
    }

    // Two causal nodes, one Follows edge between them.
    assert_eq!(causal.node_count(), 2, "one causal node per indexed turn");
    assert_eq!(causal.edge_count(), 1, "one Follows edge prev → new");
    let follows: Vec<_> = causal
        .node_ids()
        .into_iter()
        .flat_map(|n| causal.get_forward_edges(n))
        .filter(|e| e.edge_type == CausalEdgeType::Follows)
        .collect();
    assert_eq!(follows.len(), 1, "exactly one Follows edge");
    assert_eq!(
        follows[0].chain_seq, s2,
        "the Follows edge is stamped with the new turn's witness seq"
    );

    // Speaker cross-ref: turn 1 → the conversation's `user` identity node.
    let turn1_uid = turn_universal_id(conv, s1, "first turn");
    let speaker = crossrefs.by_type(&turn1_uid, &CrossRefType::Speaker);
    assert_eq!(speaker.len(), 1, "one speaker cross-ref per turn");
    assert_eq!(
        speaker[0].target,
        speaker_universal_id(conv, "user"),
        "speaker cross-ref points at the user identity node"
    );
}

/// The forest join is opt-in: a tier without `with_forest` writes nothing to a
/// causal graph (legacy cosine-only L2 is unchanged).
#[tokio::test]
async fn no_forest_means_no_causal_write() {
    let (tier, chain) = make_tier(64);
    let causal = Arc::new(CausalGraph::new());
    append_and_index(&tier, &chain, "c", "a turn").await;
    assert_eq!(causal.node_count(), 0, "no forest handle ⇒ no causal write");
}

// ── ADR-062 §1.2 — lineage-fused graft ───────────────────────────────────

/// HEADLINE (ADR-062 §1.2): recall surfaces a `Follows`-ancestor that cosine
/// alone misses. With top_k=1 the cosine recall of turn B returns only B; the
/// forest walk follows B's lineage back to its parent A and grafts it too —
/// with A's witness provenance intact. A second, forest-less tier proves cosine
/// alone leaves A out.
#[tokio::test]
async fn graft_fuses_follows_ancestor_cosine_alone_misses() {
    let dims = 64;
    let conv = "fuse-c1";
    let fact = "the launch code is ZULU-7";
    let reply = "understood, I have noted that down";

    // Forest-joined tier with a tight top_k so cosine returns a single hit.
    let (tier, chain, _causal, _crossrefs) = make_forest_tier(dims);
    let tier = tier.with_graft_top_k(1);
    // A (user, the fact) then B (assistant, the reply) — index order draws a
    // Follows edge A → B. Different roles so the speaker cross-ref can't be what
    // surfaces A — only the causal lineage can.
    let s_fact = append_and_index_as(&tier, &chain, conv, "user", fact).await;
    let _s_reply = append_and_index_as(&tier, &chain, conv, "assistant", reply).await;

    // Query with B's exact text: cosine's single hit is B. Fusion walks B's
    // reverse lineage to A and grafts it.
    let block = tier.graft_block(conv, reply).await;
    let body = &block[0].content;
    assert!(
        body.contains("ZULU-7"),
        "lineage fusion must surface the Follows-ancestor: {body}"
    );
    assert!(
        body.contains(&format!("chain_seq {s_fact}")),
        "the surfaced ancestor keeps its witness provenance: {body}"
    );

    // Control: a forest-less tier (cosine only) with the same data + top_k=1
    // does NOT surface A.
    let (plain, chain2) = make_tier(dims);
    let plain = plain.with_graft_top_k(1);
    append_and_index_as(&plain, &chain2, conv, "user", fact).await;
    append_and_index_as(&plain, &chain2, conv, "assistant", reply).await;
    let plain_block = plain.graft_block(conv, reply).await;
    let plain_body = &plain_block[0].content;
    assert!(
        !plain_body.contains("ZULU-7"),
        "cosine alone (top_k=1) must miss the ancestor: {plain_body}"
    );
}

// ── M2 D2/D4 — text ImpulseSource + view resolution ──────────────────────

/// M2 D4: `ViewResolver::view_for` returns the *same* stable `Arc<SessionView>`
/// the graft path reads — one view per conversation, not a second copy. Before
/// any turn is indexed there is no view (`None`); after, repeated resolves are
/// pointer-identical.
#[tokio::test]
async fn view_for_returns_the_stable_graft_view() {
    let (tier, chain) = make_tier(64);
    let conv = "vr-c1";

    // No view yet → resolver returns None (loop would log a no-op).
    assert!(ViewResolver::view_for(&tier, conv).is_none());

    // Indexing a turn creates the conversation's single view.
    append_and_index(&tier, &chain, conv, "hello").await;

    let v1 = ViewResolver::view_for(&tier, conv).expect("view present after index");
    let v2 = ViewResolver::view_for(&tier, conv).expect("view present");
    assert!(
        Arc::ptr_eq(&v1, &v2),
        "view_for must return the one stable view graft also reads"
    );
}

/// M2 D2 (HEADLINE): a tier with a `TalkModeLoop` attached, on `index_turn`,
/// registers the turn with the loop AND emits exactly one `EndOfUtterance`
/// carrying the right `{chain_seq, conv_id}` (HLC-stamped with the chain_seq).
#[tokio::test]
async fn index_turn_registers_and_emits_one_eou() {
    use clawft_kernel::context_graft::SessionView;
    use clawft_kernel::{
        CognitiveTick, CognitiveTickConfig, ImpulseQueue, ImpulseType, SingleViewResolver,
        TalkModeConfig, TalkModeLoop,
    };

    let (tier, chain, causal, crossrefs) = make_forest_tier(64);
    let conv = "loop-c1";

    // Build a multiplexed loop over a queue we hold, so we can inspect emits.
    // Commit routing is not exercised here, so a trivial view resolver suffices.
    let impulses = Arc::new(ImpulseQueue::new());
    let resolver: Arc<dyn ViewResolver> =
        Arc::new(SingleViewResolver::new(Arc::new(SessionView::new(conv, 64))));
    let tick = Arc::new(CognitiveTick::new(CognitiveTickConfig::default()));
    let talk_loop = Arc::new(TalkModeLoop::new(
        impulses.clone(),
        causal.clone(),
        crossrefs.clone(),
        resolver,
        tick,
        TalkModeConfig::default(),
    ));
    let tier = tier.with_talk_loop(talk_loop.clone());

    let seq = append_and_index_as(&tier, &chain, conv, "user", "hello there").await;

    // register_turn fired: the loop tracks this conversation's in-flight turn.
    assert_eq!(
        talk_loop.current_turn(conv),
        Some(seq),
        "register_turn must make the indexed turn the conv's in-flight turn"
    );

    // Exactly one EndOfUtterance, carrying {chain_seq, conv_id}, HLC = chain_seq.
    let drained = impulses.drain_ready();
    assert_eq!(drained.len(), 1, "exactly one impulse emitted per indexed turn");
    let imp = &drained[0];
    assert_eq!(imp.impulse_type, ImpulseType::EndOfUtterance);
    assert_eq!(
        imp.payload.get("chain_seq").and_then(|v| v.as_u64()),
        Some(seq),
        "EOU payload carries the turn's chain_seq"
    );
    assert_eq!(
        imp.payload.get("conv_id").and_then(|v| v.as_str()),
        Some(conv),
        "EOU payload carries the conv_id for multiplex routing"
    );
    assert_eq!(
        imp.hlc_timestamp, seq,
        "EOU is HLC-stamped with the kernel-global chain_seq"
    );
}

/// A tier WITHOUT a talk loop indexes turns but emits nothing — the legacy
/// index-only path is unchanged (turns stay `Frontier`, no commit actor).
#[tokio::test]
async fn no_talk_loop_means_no_impulse() {
    use clawft_kernel::ImpulseQueue;
    let (tier, chain, _causal, _crossrefs) = make_forest_tier(64);
    // A queue not wired to the tier: indexing must not touch it.
    let queue = Arc::new(ImpulseQueue::new());
    append_and_index_as(&tier, &chain, "no-loop", "user", "hi").await;
    assert!(queue.is_empty(), "no talk loop ⇒ no impulse emitted");
}

/// HEADLINE (D2 + D4, daemon wiring path): build the tier `Arc`-first, hand the
/// loop a `Weak` resolver to that tier, attach the loop via `set_talk_loop`
/// (no strong cycle), then index a turn. One tick commits it Frontier→Committed
/// on the tier's OWN view — the same object `graft_block` reads — proving the
/// loop resolves through the weak handle to the live view.
#[tokio::test]
async fn daemon_wiring_commits_on_the_tiers_own_view() {
    use clawft_kernel::context_graft::NodeState;
    use clawft_kernel::{
        CognitiveTick, CognitiveTickConfig, ImpulseQueue, TalkModeConfig, TalkModeLoop,
    };

    // Build the tier exactly as the daemon does: Arc-wrapped, forest-joined.
    let embedder: Arc<dyn EmbeddingProvider> = Arc::new(MockEmbeddingProvider::new(64));
    let chain = Arc::new(ChainManager::new(0, 1000));
    let causal = Arc::new(CausalGraph::new());
    let crossrefs = Arc::new(CrossRefStore::new());
    let tier = Arc::new(
        SessionTier::new(embedder, chain.clone(), None)
            .with_forest(causal.clone(), crossrefs.clone()),
    );

    // Loop resolves the tier through a Weak handle (no tier↔loop strong cycle);
    // attach the loop to the tier via interior mutability.
    let impulses = Arc::new(ImpulseQueue::new());
    let resolver = SessionTier::weak_view_resolver(&tier);
    let tick = Arc::new(CognitiveTick::new(CognitiveTickConfig::default()));
    let talk_loop = Arc::new(TalkModeLoop::new(
        impulses.clone(),
        causal.clone(),
        crossrefs.clone(),
        resolver,
        tick,
        TalkModeConfig::default(),
    ));
    tier.set_talk_loop(talk_loop.clone());

    // Drive a turn through the anchor seam → Frontier chunk + one queued EOU.
    let conv = "wire-c1";
    let ev = chain.append(
        "agent",
        "agent.chat.turn",
        Some(json!({ "conv": conv, "content": "hello" })),
    );
    let seq = ev.sequence;
    tier.index_turn(conv, seq, "agent.chat.turn", "user", "hello", None)
        .await;

    let view = ViewResolver::view_for(&*tier, conv).expect("view exists after index");
    assert_eq!(
        view.chunk(seq).expect("chunk indexed").state,
        NodeState::Frontier,
        "turn is born Frontier before the loop ticks"
    );

    // One tick: the loop resolves the tier's own view through the Weak handle
    // and commits Frontier→Committed.
    talk_loop.tick();
    assert_eq!(
        view.chunk(seq).expect("chunk still present").state,
        NodeState::Committed,
        "loop committed the turn on the tier's own view via the weak resolver"
    );
}

// ── 5.3 budget validation ────────────────────────────────────────────

/// Per-turn graft hot-path budget (embed + scoped HNSW search), against the
/// **real** ADR-059 Qwen3 embedder.
///
/// Justification (measured, release, M-series CPU; weights staged + onnx-embeddings):
/// - warm Qwen3 embed: ~22ms (the model loads eagerly in the provider ctor, so
///   the per-call embed is the dominant per-turn cost, not a cold start);
/// - per-turn graft with a built index: ~22ms; the lone HNSW graph build at the
///   32-chunk threshold (`HNSW_THRESHOLD`) costs ~35ms, the worst single turn;
/// - bulk-building 64 chunks at once (an artifact of the old test shape, never
///   the per-turn path) was ~142ms release / ~2.6s debug — this is the build
///   cost, NOT the steady-state query, and is why this test now warms first.
///
/// 50ms gives ~1.4x headroom over the measured 35ms worst turn.
const GRAFT_HOT_PATH_BUDGET_MS: u128 = 50;

/// ADR-058 Phase 5.3 — per-turn graft latency budget against the real embedder.
///
/// #[ignore]'d: needs the Qwen3 weights (staged out-of-tree, gitignored) AND the
/// `clawft-kernel/onnx-embeddings` feature; without both, `select_embedding_provider`
/// returns an error and this test panics (it never measures a hash embedder). Run live with:
/// `cargo test -p clawft-service-agent --features clawft-kernel/onnx-embeddings \
///   -- --ignored budget_graft_latency`
///
/// The hot path measured is the **per-turn** query against an already-built
/// index — production warms the embedder at daemon startup ([`SessionTier::warm`])
/// and the per-conversation index builds incrementally as turns accumulate, so a
/// turn never pays the one-shot bulk build. This test mirrors that: pre-warm the
/// embedder, then warm the index with one throwaway graft, then time a graft.
#[tokio::test]
#[ignore = "needs real Qwen3 weights + clawft-kernel/onnx-embeddings feature"]
async fn budget_graft_latency_under_hot_path() {
    use clawft_kernel::embedding::select_embedding_provider;
    use std::time::Instant;

    // Real embedder when weights + feature are present; otherwise this is an error
    // (the #[ignore] gate means this only runs intentionally).
    let embedder: Arc<dyn EmbeddingProvider> =
        Arc::from(select_embedding_provider(None).expect("a real embedding model is required for this measurement"));
    let chain = Arc::new(ChainManager::new(0, 1000));
    let tier = SessionTier::new(embedder, chain.clone(), None);
    let conv = "budget";

    // Pre-warm the embedder (as the daemon does at startup).
    tier.warm().await;

    // Populate a realistic session window.
    for i in 0..64 {
        append_and_index(
            &tier,
            &chain,
            conv,
            &format!("turn {i}: some representative conversation content for indexing"),
        )
        .await;
    }

    // Warm the index: the first graft after a batch of inserts pays the one-shot
    // HNSW graph build (not part of the per-turn steady state — in production the
    // index is built incrementally across turns). Discard this measurement.
    let _ = tier
        .graft_block(conv, "warm-up graft to build the session index")
        .await;

    // Measure the steady-state per-turn graft latency (embed + scoped search).
    let start = Instant::now();
    let _ = tier
        .graft_block(conv, "representative query for the hot path")
        .await;
    let elapsed = start.elapsed().as_millis();
    assert!(
        elapsed <= GRAFT_HOT_PATH_BUDGET_MS,
        "graft hot-path latency {elapsed}ms exceeded budget {GRAFT_HOT_PATH_BUDGET_MS}ms"
    );
}

/// Wave 2 §W2.3: `emit_cancel_prune` degrades to a no-op (returns no seq) when
/// no `TalkModeLoop` is attached — the forest prune rides the loop that owns the
/// `TurnClaim` machinery. `witness_cancel` still records the turn-level cancel
/// marker regardless (the chain is always present on the daemon tier).
#[tokio::test]
async fn cancel_prune_noops_without_a_talk_loop_but_witness_records() {
    let (tier, _chain, _causal, _crossrefs) = make_forest_tier(64);
    // No loop wired (make_forest_tier attaches none) ⇒ nothing to prune.
    assert_eq!(tier.emit_cancel_prune("c1", None, None), None);
    // The cancel marker is still witnessed (M2-D8 durable record, not silent).
    assert!(tier.witness_cancel("c1", None));
}
