//! DEMOCRITUS continuous cognitive loop (ECC decision D5).
//!
//! The [`DemocritusLoop`] is the nervous system of WeftOS — an integration
//! layer that orchestrates the ECC subsystems on every cognitive tick:
//!
//! ```text
//! SENSE → EMBED → SEARCH → UPDATE → COMMIT
//! ```
//!
//! It drains the [`ImpulseQueue`] for new events, embeds them via the
//! configured [`EmbeddingProvider`], queries the pluggable [`VectorBackend`]
//! for nearest neighbors (default: HNSW), updates the [`CausalGraph`] with
//! inferred edges, registers cross-refs in the [`CrossRefStore`], and logs
//! the result.
//!
//! This module is compiled only when the `ecc` feature is enabled.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::causal::{CausalEdgeType, CausalGraph};
use crate::crossref::{CrossRef, CrossRefStore, CrossRefType, StructureTag, UniversalNodeId};
use crate::embedding::EmbeddingProvider;
use crate::impulse::{Impulse, ImpulseQueue, ImpulseType};
use crate::vector_backend::VectorBackend;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration for the DEMOCRITUS loop.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DemocritusConfig {
    /// Maximum number of impulses to process per tick.
    pub max_impulses_per_tick: usize,
    /// Number of nearest neighbors to retrieve during SEARCH phase.
    pub search_k: usize,
    /// Cosine similarity threshold above which two events are considered correlated.
    pub correlation_threshold: f32,
    /// Budget for a single tick in microseconds. If exceeded, the tick stops early.
    pub tick_budget_us: u64,
}

impl Default for DemocritusConfig {
    fn default() -> Self {
        Self {
            max_impulses_per_tick: 64,
            search_k: 5,
            correlation_threshold: 0.7,
            tick_budget_us: 15_000, // 15ms
        }
    }
}

// ---------------------------------------------------------------------------
// Tick result
// ---------------------------------------------------------------------------

/// Summary of a single DEMOCRITUS tick cycle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DemocritusTickResult {
    /// Number of impulses drained in the SENSE phase.
    pub impulses_sensed: usize,
    /// Number of embeddings produced in the EMBED phase.
    pub embeddings_produced: usize,
    /// Number of vector searches performed in the SEARCH phase.
    pub searches_performed: usize,
    /// Number of causal edges added in the UPDATE phase.
    pub edges_added: usize,
    /// Number of cross-refs registered in the UPDATE phase.
    pub crossrefs_added: usize,
    /// Whether the tick was cut short due to budget exhaustion.
    pub budget_exceeded: bool,
    /// Wall-clock duration of the tick in microseconds.
    pub duration_us: u64,
}

// ---------------------------------------------------------------------------
// DemocritusLoop
// ---------------------------------------------------------------------------

/// The DEMOCRITUS continuous cognitive loop.
///
/// Runs every CognitiveTick cycle: Sense -> Embed -> Search -> Update -> Commit.
///
/// Vector recall is pluggable via [`VectorBackend`]. Production default is
/// [`HnswBackend`](crate::vector_hnsw::HnswBackend); DiskANN / Hybrid can be
/// swapped in without changing this loop (WEFT-124).
pub struct DemocritusLoop {
    // ECC subsystem references
    causal_graph: Arc<CausalGraph>,
    /// Pluggable vector index (default: HNSW backend).
    vector: Arc<dyn VectorBackend>,
    impulse_queue: Arc<ImpulseQueue>,
    crossref_store: Arc<CrossRefStore>,
    embedding_provider: Arc<dyn EmbeddingProvider>,
    // Configuration
    config: DemocritusConfig,
    // Tick statistics
    total_ticks: AtomicU64,
    total_nodes_added: AtomicU64,
    total_edges_added: AtomicU64,
}

impl DemocritusLoop {
    /// Create a new DEMOCRITUS loop wired to the given ECC subsystems.
    ///
    /// `vector` is typically `Arc::new(HnswBackend::new(...))` for the
    /// production default; any [`VectorBackend`] implementor works.
    pub fn new(
        causal_graph: Arc<CausalGraph>,
        vector: Arc<dyn VectorBackend>,
        impulse_queue: Arc<ImpulseQueue>,
        crossref_store: Arc<CrossRefStore>,
        embedding_provider: Arc<dyn EmbeddingProvider>,
        config: DemocritusConfig,
    ) -> Self {
        Self {
            causal_graph,
            vector,
            impulse_queue,
            crossref_store,
            embedding_provider,
            config,
            total_ticks: AtomicU64::new(0),
            total_nodes_added: AtomicU64::new(0),
            total_edges_added: AtomicU64::new(0),
        }
    }

    /// Borrow the active vector backend (for diagnostics / tests).
    pub fn vector_backend(&self) -> &dyn VectorBackend {
        self.vector.as_ref()
    }

    /// Execute one full tick cycle: Sense -> Embed -> Search -> Update -> Commit.
    ///
    /// Returns a summary of what was processed. This is the method the
    /// [`CognitiveTick`] loop should call on each cycle.
    pub async fn tick(&self) -> DemocritusTickResult {
        let start = Instant::now();
        let mut result = DemocritusTickResult {
            impulses_sensed: 0,
            embeddings_produced: 0,
            searches_performed: 0,
            edges_added: 0,
            crossrefs_added: 0,
            budget_exceeded: false,
            duration_us: 0,
        };

        // ── SENSE ────────────────────────────────────────────────────
        let impulses = self.sense();
        result.impulses_sensed = impulses.len();

        if impulses.is_empty() {
            result.duration_us = start.elapsed().as_micros() as u64;
            self.commit(&result);
            return result;
        }

        // ── EMBED ────────────────────────────────────────────────────
        let embedded = self.embed(&impulses).await;
        result.embeddings_produced = embedded.len();

        if self.budget_exceeded(start) {
            result.budget_exceeded = true;
            result.duration_us = start.elapsed().as_micros() as u64;
            self.commit(&result);
            return result;
        }

        // ── SEARCH ───────────────────────────────────────────────────
        // Query the pluggable VectorBackend per non-empty embedding.
        // Neighbor keys are causal node id strings (see update()); scores
        // are cosine similarity recovered from backend distance so that
        // correlation_threshold stays calibrated for the HNSW default.
        let non_empty_queries: Vec<(usize, &[f32])> = embedded
            .iter()
            .enumerate()
            .filter(|(_, emb)| !emb.is_empty())
            .map(|(i, emb)| (i, emb.as_slice()))
            .collect();

        let mut search_results_by_index: Vec<Vec<(String, f32)>> = vec![Vec::new(); embedded.len()];
        for &(orig_idx, query) in &non_empty_queries {
            search_results_by_index[orig_idx] = self.search(query);
        }
        result.searches_performed = non_empty_queries.len();

        type NeighborTriple<'a> = (&'a Impulse, &'a Vec<f32>, Vec<(String, f32)>);
        let mut neighbors_per_event: Vec<NeighborTriple<'_>> = Vec::with_capacity(embedded.len());
        for (i, (impulse, embedding)) in impulses.iter().zip(embedded.iter()).enumerate() {
            if self.budget_exceeded(start) {
                result.budget_exceeded = true;
                break;
            }
            let neighbors = std::mem::take(&mut search_results_by_index[i]);
            neighbors_per_event.push((impulse, embedding, neighbors));
        }

        // ── UPDATE ───────────────────────────────────────────────────
        for (impulse, embedding, neighbors) in &neighbors_per_event {
            if self.budget_exceeded(start) {
                result.budget_exceeded = true;
                break;
            }
            let (edges, crossrefs) = self.update(impulse, embedding, neighbors);
            result.edges_added += edges;
            result.crossrefs_added += crossrefs;
        }

        // ── COMMIT ───────────────────────────────────────────────────
        result.duration_us = start.elapsed().as_micros() as u64;
        self.commit(&result);
        result
    }

    // ── Phase implementations ────────────────────────────────────────

    /// SENSE: drain the impulse queue up to the per-tick limit.
    fn sense(&self) -> Vec<Impulse> {
        let mut impulses = self.impulse_queue.drain_ready();
        impulses.truncate(self.config.max_impulses_per_tick);
        impulses
    }

    /// EMBED: convert each impulse's payload to a vector embedding.
    ///
    /// On embedding failure, falls back to an empty vector (the impulse
    /// will still be recorded in the causal graph but won't participate
    /// in similarity search).
    async fn embed(&self, impulses: &[Impulse]) -> Vec<Vec<f32>> {
        let texts: Vec<String> = impulses
            .iter()
            .map(|imp| {
                // Build a text representation from the impulse payload.
                let type_str = imp.impulse_type.to_string();
                let payload_str = imp.payload.to_string();
                format!("{type_str}:{payload_str}")
            })
            .collect();

        let text_refs: Vec<&str> = texts.iter().map(|s| s.as_str()).collect();

        match self.embedding_provider.embed_batch(&text_refs).await {
            Ok(vecs) => vecs,
            Err(e) => {
                warn!("DEMOCRITUS embed phase failed, falling back to empty vectors: {e}");
                vec![Vec::new(); impulses.len()]
            }
        }
    }

    /// SEARCH: query the vector backend for k nearest neighbors.
    ///
    /// Returns `(neighbor_key, cosine_similarity)` pairs. Backends that
    /// report distance (HNSW: `distance = 1 - cosine_sim`) are converted
    /// so `correlation_threshold` keeps its original similarity semantics.
    fn search(&self, embedding: &[f32]) -> Vec<(String, f32)> {
        if embedding.is_empty() {
            return Vec::new();
        }
        self.vector
            .search(embedding, self.config.search_k)
            .into_iter()
            .map(|r| {
                // Prefer the string key (causal node id) so neighbor → link
                // parsing stays identical to the pre-VectorBackend path.
                let key = if r.key.is_empty() {
                    r.id.to_string()
                } else {
                    r.key
                };
                let score = 1.0 - r.distance;
                (key, score)
            })
            .collect()
    }

    /// UPDATE: add a causal node for the impulse, insert into the vector
    /// backend, create causal edges based on neighbor similarity, and
    /// register cross-references.
    ///
    /// Returns (edges_added, crossrefs_added).
    fn update(
        &self,
        impulse: &Impulse,
        embedding: &[f32],
        neighbors: &[(String, f32)],
    ) -> (usize, usize) {
        let mut edges_added = 0usize;
        let mut crossrefs_added = 0usize;

        // Atom-spine sequence for this event (WEFT-642). Prefer the explicit
        // `payload.chain_seq` minted by session_tier / talk-mode after the
        // ExoChain append; without it the projection cannot reverse-resolve
        // to the witness spine (the panopticon "looks alive but does not
        // join back" class). Zero is retained only when the emitter never
        // supplied a sequence — ADR-069 audit will flag those rows.
        let chain_seq = resolve_chain_seq(impulse);

        // Add a causal node for this impulse. Payload already carries
        // `chain_seq` when present (session_tier); re-assert it on the
        // metadata object so non-object payloads still become joinable.
        let label = format!("impulse:{}:{}", impulse.impulse_type, impulse.id);
        let node_metadata = node_metadata_with_chain_seq(impulse, chain_seq);
        let node_id = self.causal_graph.add_node(label.clone(), node_metadata);
        self.total_nodes_added.fetch_add(1, Ordering::Relaxed);

        // Insert embedding into the vector backend. Numeric id + string key
        // are both the causal node ID so neighbor hits can re-link by
        // NodeId. `chain_seq` in metadata is the join key back to the atom
        // spine — never hardcode 0 here.
        if !embedding.is_empty() {
            let meta = serde_json::json!({
                "impulse_id": impulse.id,
                "impulse_type": impulse.impulse_type.to_string(),
                "hlc": impulse.hlc_timestamp,
                "chain_seq": chain_seq,
                "causal_node_id": node_id,
            });
            if let Err(e) = self.vector.insert(
                node_id,
                &node_id.to_string(),
                embedding,
                meta,
            ) {
                warn!(
                    "DEMOCRITUS vector insert failed for node {node_id}: {e}"
                );
            }
        }

        // Create causal edges based on neighbor similarity.
        for (neighbor_id_str, score) in neighbors {
            let edge_type = self.classify_edge(impulse, *score);

            if let Ok(neighbor_node_id) = neighbor_id_str.parse::<u64>() {
                let linked = self.causal_graph.link(
                    node_id,
                    neighbor_node_id,
                    edge_type,
                    *score,
                    impulse.hlc_timestamp,
                    chain_seq,
                );
                if linked {
                    edges_added += 1;
                    self.total_edges_added.fetch_add(1, Ordering::Relaxed);
                }
            }
        }

        // Register a cross-reference linking the causal node to its source structure.
        let source_tag = structure_tag_from_u8(impulse.source_structure);
        let uni_id = UniversalNodeId::new(
            &StructureTag::CausalGraph,
            label.as_bytes(),
            // Prefer chain_seq for UID material when available so the
            // identity aligns with turn-style spine UIDs (session_forest).
            if chain_seq > 0 {
                chain_seq
            } else {
                impulse.hlc_timestamp
            },
            &impulse.source_node,
            &[0u8; 32],
        );
        let source_uni_id = UniversalNodeId::from_bytes(impulse.source_node);
        self.crossref_store.insert(CrossRef {
            source: uni_id,
            source_structure: StructureTag::CausalGraph,
            target: source_uni_id,
            target_structure: source_tag,
            ref_type: CrossRefType::TriggeredBy,
            created_at: impulse.hlc_timestamp,
            chain_seq,
        });
        crossrefs_added += 1;

        (edges_added, crossrefs_added)
    }

    /// COMMIT: update tick statistics and log the result.
    fn commit(&self, result: &DemocritusTickResult) {
        self.total_ticks.fetch_add(1, Ordering::Relaxed);
        debug!(
            "DEMOCRITUS tick #{}: sensed={}, embedded={}, searched={}, edges={}, crossrefs={}, budget_exceeded={}, duration={}us",
            self.total_ticks.load(Ordering::Relaxed),
            result.impulses_sensed,
            result.embeddings_produced,
            result.searches_performed,
            result.edges_added,
            result.crossrefs_added,
            result.budget_exceeded,
            result.duration_us,
        );
    }

    // ── Helpers ──────────────────────────────────────────────────────

    /// Classify the edge type based on impulse context and similarity score.
    fn classify_edge(&self, impulse: &Impulse, score: f32) -> CausalEdgeType {
        // High similarity → Correlates (statistically similar events).
        if score >= self.config.correlation_threshold {
            return CausalEdgeType::Correlates;
        }

        // Impulse type hints at causal direction.
        match &impulse.impulse_type {
            ImpulseType::BeliefUpdate | ImpulseType::NoveltyDetected => CausalEdgeType::Follows,
            ImpulseType::EdgeConfirmed => CausalEdgeType::Causes,
            ImpulseType::CoherenceAlert => CausalEdgeType::EvidenceFor,
            ImpulseType::EmbeddingRefined => CausalEdgeType::Enables,
            // Turn-taking signals (ADR-062 D5) are conversational-flow events;
            // the Talk-Mode tick (Phase 2) interprets them — here they read as
            // temporal succession on the general substrate.
            ImpulseType::EndOfUtterance
            | ImpulseType::TurnClaim
            | ImpulseType::TurnShift
            | ImpulseType::Backchannel => CausalEdgeType::Follows,
            ImpulseType::Custom(_) => CausalEdgeType::Follows,
        }
    }

    /// Check if the tick budget has been exceeded.
    fn budget_exceeded(&self, start: Instant) -> bool {
        start.elapsed().as_micros() as u64 > self.config.tick_budget_us
    }

    // ── Statistics accessors ─────────────────────────────────────────

    /// Total number of ticks executed.
    pub fn total_ticks(&self) -> u64 {
        self.total_ticks.load(Ordering::Relaxed)
    }

    /// Total number of causal nodes added across all ticks.
    pub fn total_nodes_added(&self) -> u64 {
        self.total_nodes_added.load(Ordering::Relaxed)
    }

    /// Total number of causal edges added across all ticks.
    pub fn total_edges_added(&self) -> u64 {
        self.total_edges_added.load(Ordering::Relaxed)
    }
}

/// Map a raw `u8` structure tag back to a [`StructureTag`] variant.
fn structure_tag_from_u8(tag: u8) -> StructureTag {
    match tag {
        0x01 => StructureTag::ExoChain,
        0x02 => StructureTag::ResourceTree,
        0x03 => StructureTag::CausalGraph,
        0x04 => StructureTag::HnswIndex,
        other => StructureTag::Custom(other),
    }
}

/// Resolve the atom-spine `chain_seq` for an impulse (WEFT-642).
///
/// Session-tier / talk-mode emitters put the ExoChain sequence into
/// `payload.chain_seq` at emit time (see `session_tier::index_turn`).
/// That is the only durable join key back to the witness spine; hardcoding
/// `0` made the ECC brain HNSW look populated while remaining unjoinable.
///
/// Returns `0` when the emitter did not supply a sequence (legacy / synthetic
/// impulses). Callers must still record that zero rather than inventing one.
fn resolve_chain_seq(impulse: &Impulse) -> u64 {
    impulse
        .payload
        .get("chain_seq")
        .and_then(|v| v.as_u64())
        .unwrap_or(0)
}

/// Build causal-node metadata that always surfaces `chain_seq` when known.
///
/// Clones the impulse payload and ensures a top-level `chain_seq` field so
/// reverse scans (`nodes_for_conv`, panopticon audit) can join without
/// reading HNSW metadata.
fn node_metadata_with_chain_seq(impulse: &Impulse, chain_seq: u64) -> serde_json::Value {
    let mut meta = impulse.payload.clone();
    if chain_seq == 0 {
        return meta;
    }
    match meta.as_object_mut() {
        Some(obj) => {
            obj.insert("chain_seq".into(), serde_json::json!(chain_seq));
            meta
        }
        None => serde_json::json!({
            "chain_seq": chain_seq,
            "payload": impulse.payload,
        }),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embedding::MockEmbeddingProvider;
    use crate::hnsw_service::HnswServiceConfig;
    use crate::vector_backend::{SearchResult, VectorResult};
    use crate::vector_hnsw::HnswBackend;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;

    /// A wired loop plus the stores it was built over.
    type LoopParts = (
        Arc<CausalGraph>,
        Arc<dyn VectorBackend>,
        Arc<ImpulseQueue>,
        Arc<CrossRefStore>,
        DemocritusLoop,
    );

    /// One stored `(id, key, vector, metadata)` entry of [`CountingBackend`].
    type CountedEntry = (u64, String, Vec<f32>, serde_json::Value);

    /// Helper: build a fully wired DemocritusLoop with default HNSW backend.
    fn make_loop() -> LoopParts {
        make_loop_with_config(DemocritusConfig::default())
    }

    fn make_loop_with_config(config: DemocritusConfig) -> LoopParts {
        let cg = Arc::new(CausalGraph::new());
        let vector: Arc<dyn VectorBackend> = Arc::new(HnswBackend::new(HnswServiceConfig {
            default_dimensions: 8,
            ..HnswServiceConfig::default()
        }));
        let iq = Arc::new(ImpulseQueue::new());
        let crs = Arc::new(CrossRefStore::new());
        let emb: Arc<dyn EmbeddingProvider> = Arc::new(MockEmbeddingProvider::new(8));

        let democritus = DemocritusLoop::new(
            Arc::clone(&cg),
            Arc::clone(&vector),
            Arc::clone(&iq),
            Arc::clone(&crs),
            emb,
            config,
        );
        (cg, vector, iq, crs, democritus)
    }

    /// In-memory counting backend used to prove DemocritusLoop is
    /// wiring-only over `VectorBackend` (no HNSW-specific path).
    struct CountingBackend {
        name: &'static str,
        entries: Mutex<Vec<CountedEntry>>,
        inserts: AtomicUsize,
        searches: AtomicUsize,
    }

    impl CountingBackend {
        fn new(name: &'static str) -> Self {
            Self {
                name,
                entries: Mutex::new(Vec::new()),
                inserts: AtomicUsize::new(0),
                searches: AtomicUsize::new(0),
            }
        }

        fn insert_count(&self) -> usize {
            self.inserts.load(Ordering::Relaxed)
        }

        fn search_count(&self) -> usize {
            self.searches.load(Ordering::Relaxed)
        }
    }

    impl VectorBackend for CountingBackend {
        fn insert(
            &self,
            id: u64,
            key: &str,
            vector: &[f32],
            metadata: serde_json::Value,
        ) -> VectorResult<()> {
            self.inserts.fetch_add(1, Ordering::Relaxed);
            let mut entries = self.entries.lock().expect("entries lock");
            entries.retain(|(eid, _, _, _)| *eid != id);
            entries.push((id, key.to_owned(), vector.to_vec(), metadata));
            Ok(())
        }

        fn search(&self, query: &[f32], k: usize) -> Vec<SearchResult> {
            self.searches.fetch_add(1, Ordering::Relaxed);
            let entries = self.entries.lock().expect("entries lock");
            let mut scored: Vec<(f32, &CountedEntry)> = entries
                .iter()
                .map(|e| {
                    // Cosine similarity → distance = 1 - sim (match HnswBackend).
                    let sim = cosine_sim(query, &e.2);
                    (1.0 - sim, e)
                })
                .collect();
            scored.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
            scored
                .into_iter()
                .take(k)
                .map(|(dist, e)| SearchResult::new(e.0, e.1.clone(), dist, e.3.clone()))
                .collect()
        }

        fn len(&self) -> usize {
            self.entries.lock().expect("entries lock").len()
        }

        fn contains(&self, id: u64) -> bool {
            self.entries
                .lock()
                .expect("entries lock")
                .iter()
                .any(|(eid, _, _, _)| *eid == id)
        }

        fn remove(&self, id: u64) -> bool {
            let mut entries = self.entries.lock().expect("entries lock");
            let before = entries.len();
            entries.retain(|(eid, _, _, _)| *eid != id);
            before != entries.len()
        }

        fn flush(&self) -> VectorResult<()> {
            Ok(())
        }

        fn backend_name(&self) -> &str {
            self.name
        }
    }

    fn cosine_sim(a: &[f32], b: &[f32]) -> f32 {
        if a.is_empty() || b.is_empty() || a.len() != b.len() {
            return 0.0;
        }
        let mut dot = 0.0f32;
        let mut na = 0.0f32;
        let mut nb = 0.0f32;
        for i in 0..a.len() {
            dot += a[i] * b[i];
            na += a[i] * a[i];
            nb += b[i] * b[i];
        }
        let denom = na.sqrt() * nb.sqrt();
        if denom == 0.0 {
            0.0
        } else {
            dot / denom
        }
    }

    fn emit_test_impulse(iq: &ImpulseQueue, impulse_type: ImpulseType, ts: u64) -> u64 {
        iq.emit(
            StructureTag::CausalGraph.as_u8(),
            [0u8; 32],
            StructureTag::HnswIndex.as_u8(),
            impulse_type,
            serde_json::json!({"test": true}),
            ts,
        )
    }

    // ── Test 1: Empty impulse queue — tick completes with no new nodes ──

    #[tokio::test]
    async fn empty_queue_produces_no_work() {
        let (_cg, _vector, _iq, _crs, demo) = make_loop();
        let result = demo.tick().await;

        assert_eq!(result.impulses_sensed, 0);
        assert_eq!(result.embeddings_produced, 0);
        assert_eq!(result.searches_performed, 0);
        assert_eq!(result.edges_added, 0);
        assert_eq!(result.crossrefs_added, 0);
        assert!(!result.budget_exceeded);
        assert_eq!(demo.total_ticks(), 1);
        assert_eq!(demo.total_nodes_added(), 0);
    }

    // ── Test 2: Single impulse → full pipeline ──

    #[tokio::test]
    async fn single_impulse_full_pipeline() {
        let (cg, vector, iq, crs, demo) = make_loop();

        emit_test_impulse(&iq, ImpulseType::BeliefUpdate, 100);

        let result = demo.tick().await;

        assert_eq!(result.impulses_sensed, 1);
        assert_eq!(result.embeddings_produced, 1);
        assert_eq!(result.searches_performed, 1);
        // No pre-existing neighbors, so no edges added.
        assert_eq!(result.edges_added, 0);
        // One cross-ref should be registered.
        assert_eq!(result.crossrefs_added, 1);
        // Causal graph should have one node.
        assert_eq!(cg.node_count(), 1);
        // Vector backend should have one entry.
        assert_eq!(vector.len(), 1);
        // CrossRefStore should have one entry.
        assert_eq!(crs.count(), 1);
        assert_eq!(demo.total_nodes_added(), 1);
        // Default binding is HNSW.
        assert_eq!(demo.vector_backend().backend_name(), "hnsw");
    }

    // ── Test 3: Multiple impulses in one tick — batch processing ──

    #[tokio::test]
    async fn multiple_impulses_batch_processing() {
        let (cg, _vector, iq, crs, demo) = make_loop();

        emit_test_impulse(&iq, ImpulseType::BeliefUpdate, 100);
        emit_test_impulse(&iq, ImpulseType::CoherenceAlert, 200);
        emit_test_impulse(&iq, ImpulseType::NoveltyDetected, 300);

        let result = demo.tick().await;

        assert_eq!(result.impulses_sensed, 3);
        assert_eq!(result.embeddings_produced, 3);
        assert_eq!(result.searches_performed, 3);
        assert_eq!(result.crossrefs_added, 3);
        assert_eq!(cg.node_count(), 3);
        assert_eq!(crs.count(), 3);
    }

    // ── Test 4: Tick respects budget (stops early if budget exceeded) ──

    #[tokio::test]
    async fn tick_respects_budget() {
        // Use a budget of 0 microseconds so the tick must stop immediately.
        let config = DemocritusConfig {
            tick_budget_us: 0,
            ..DemocritusConfig::default()
        };
        let (_cg, _vector, iq, _crs, demo) = make_loop_with_config(config);

        // Emit several impulses.
        for i in 0..10 {
            emit_test_impulse(&iq, ImpulseType::BeliefUpdate, i);
        }

        let result = demo.tick().await;

        // With a zero budget, the tick should have been cut short.
        assert!(result.budget_exceeded);
        // Tick counter still increments.
        assert_eq!(demo.total_ticks(), 1);
    }

    // ── Test 5: CrossRef created linking new node to source entity ──

    #[tokio::test]
    async fn crossref_links_node_to_source() {
        let (_cg, _vector, iq, crs, demo) = make_loop();

        let source_node = [42u8; 32];
        iq.emit(
            StructureTag::ExoChain.as_u8(),
            source_node,
            StructureTag::HnswIndex.as_u8(),
            ImpulseType::EdgeConfirmed,
            serde_json::json!({"chain": "test"}),
            500,
        );

        demo.tick().await;

        // Verify cross-ref exists with the correct target (the source node).
        let target_uni = UniversalNodeId::from_bytes(source_node);
        let refs = crs.get_reverse(&target_uni);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].target_structure, StructureTag::ExoChain);
        assert_eq!(refs[0].ref_type, CrossRefType::TriggeredBy);
    }

    // ── Test 6: Tick statistics increment correctly ──

    #[tokio::test]
    async fn tick_statistics_increment() {
        let (_cg, _vector, iq, _crs, demo) = make_loop();

        assert_eq!(demo.total_ticks(), 0);
        assert_eq!(demo.total_nodes_added(), 0);
        assert_eq!(demo.total_edges_added(), 0);

        // Tick 1: one impulse.
        emit_test_impulse(&iq, ImpulseType::BeliefUpdate, 10);
        demo.tick().await;
        assert_eq!(demo.total_ticks(), 1);
        assert_eq!(demo.total_nodes_added(), 1);

        // Tick 2: two impulses.
        emit_test_impulse(&iq, ImpulseType::CoherenceAlert, 20);
        emit_test_impulse(&iq, ImpulseType::NoveltyDetected, 30);
        demo.tick().await;
        assert_eq!(demo.total_ticks(), 2);
        assert_eq!(demo.total_nodes_added(), 3);
    }

    // ── Test 7: Vector search wiring (plumbing, Mock embedder) ──
    //
    // The Mock embedder is a hash of the whole string: equal text maps to equal
    // vectors, nothing more. This proves the second tick's search sees the first
    // tick's insert. Neighbour QUALITY is judged by
    // `real_embedder_neighbour_is_the_semantically_related_impulse` below.

    #[tokio::test]
    async fn vector_returns_prior_entries_on_second_tick_plumbing_mock_embedder() {
        let (_cg, vector, iq, _crs, demo) = make_loop();

        // First tick: insert a node.
        emit_test_impulse(&iq, ImpulseType::BeliefUpdate, 100);
        demo.tick().await;
        assert_eq!(vector.len(), 1);

        // Second tick: same impulse type/payload should find the first as neighbor.
        emit_test_impulse(&iq, ImpulseType::BeliefUpdate, 200);
        let result = demo.tick().await;

        assert_eq!(result.searches_performed, 1);
        // Two inserts: one per tick.
        assert_eq!(vector.len(), 2);
        // Search finds the prior entry (wiring exercised).
        let emb: Arc<dyn EmbeddingProvider> = Arc::new(MockEmbeddingProvider::new(8));
        let query = emb
            .embed(&format!(
                "{}:{}",
                ImpulseType::BeliefUpdate,
                serde_json::json!({"test": true})
            ))
            .await
            .expect("mock embed");
        let hits = vector.search(&query, 5);
        assert!(!hits.is_empty(), "vector backend must return prior entries");
    }

    // ── Test 7b: neighbour quality with a REAL embedding model ──

    /// Skipped with a printed reason unless WEFTOS_TEST_EMBED_MODEL_DIR points at
    /// a nomic-embed-text-v1.5 bundle. The query impulse shares no keyword with
    /// the impulse it must be nearest to.
    #[tokio::test]
    async fn real_embedder_neighbour_is_the_semantically_related_impulse() {
        let Some(emb) = crate::embedding_support::real_embedder() else { return };
        let cg = Arc::new(CausalGraph::new());
        let vector: Arc<dyn VectorBackend> = Arc::new(HnswBackend::new(HnswServiceConfig {
            default_dimensions: crate::embedding_support::TEST_MODEL_DIMS,
            ..HnswServiceConfig::default()
        }));
        let iq = Arc::new(ImpulseQueue::new());
        let crs = Arc::new(CrossRefStore::new());
        let demo = DemocritusLoop::new(
            Arc::clone(&cg),
            Arc::clone(&vector),
            Arc::clone(&iq),
            Arc::clone(&crs),
            Arc::clone(&emb),
            // The 15 ms default tick budget cannot cover a real model: embedding three
            // impulses takes hundreds of ms, so the tick aborts after EMBED and inserts
            // nothing. The budget is raised here so this test judges neighbour quality;
            // the budget finding is reported separately.
            DemocritusConfig { tick_budget_us: 120_000_000, ..DemocritusConfig::default() },
        );

        let payloads = [
            "the nightly deploy crashed because the process ran out of memory",
            "the customer asked for a dark colour theme on the dashboard",
            "invoices go to the finance team on the first working day of the month",
        ];
        let ids: Vec<u64> = payloads
            .iter()
            .enumerate()
            .map(|(i, text)| {
                iq.emit(
                    StructureTag::CausalGraph.as_u8(),
                    [0u8; 32],
                    StructureTag::HnswIndex.as_u8(),
                    ImpulseType::BeliefUpdate,
                    serde_json::json!({ "text": text }),
                    100 + i as u64,
                )
            })
            .collect();
        demo.tick().await;
        assert_eq!(vector.len(), 3);

        // Same text shape the EMBED phase builds, with different words.
        let query = emb
            .embed(&format!(
                "{}:{}",
                ImpulseType::BeliefUpdate,
                serde_json::json!({"text": "why did the release blow up with an OOM kill?"})
            ))
            .await
            .expect("real embed");
        let hits = vector.search(&query, 1);
        assert_eq!(hits.len(), 1);
        assert_eq!(
            hits[0].metadata["impulse_id"].as_u64(),
            Some(ids[0]),
            "nearest neighbour must be the out-of-memory impulse, got {:?}",
            hits[0].metadata
        );
    }

    // ── Test 8: Causal edge type selection ──

    #[tokio::test]
    async fn edge_type_classification() {
        let (_, _, _, _, demo) = make_loop();

        let impulse_belief = Impulse {
            id: 1,
            source_structure: 0,
            source_node: [0u8; 32],
            target_structure: 2,
            impulse_type: ImpulseType::BeliefUpdate,
            payload: serde_json::json!({}),
            hlc_timestamp: 0,
            acknowledged: std::sync::atomic::AtomicBool::new(false),
        };

        // High similarity → Correlates.
        assert_eq!(
            demo.classify_edge(&impulse_belief, 0.9),
            CausalEdgeType::Correlates
        );

        // Below threshold, BeliefUpdate → Follows.
        assert_eq!(
            demo.classify_edge(&impulse_belief, 0.3),
            CausalEdgeType::Follows
        );

        // EdgeConfirmed → Causes.
        let impulse_confirmed = Impulse {
            impulse_type: ImpulseType::EdgeConfirmed,
            ..impulse_belief.clone()
        };
        assert_eq!(
            demo.classify_edge(&impulse_confirmed, 0.3),
            CausalEdgeType::Causes
        );

        // CoherenceAlert → EvidenceFor.
        let impulse_coherence = Impulse {
            impulse_type: ImpulseType::CoherenceAlert,
            ..impulse_belief.clone()
        };
        assert_eq!(
            demo.classify_edge(&impulse_coherence, 0.3),
            CausalEdgeType::EvidenceFor
        );

        // EmbeddingRefined → Enables.
        let impulse_refined = Impulse {
            impulse_type: ImpulseType::EmbeddingRefined,
            ..impulse_belief.clone()
        };
        assert_eq!(
            demo.classify_edge(&impulse_refined, 0.3),
            CausalEdgeType::Enables
        );
    }

    // ── Test 9: Commit phase logs and updates total_ticks ──

    #[tokio::test]
    async fn commit_updates_tick_counter() {
        let (_, _, _, _, demo) = make_loop();

        // Empty ticks still increment the tick counter.
        demo.tick().await;
        demo.tick().await;
        demo.tick().await;

        assert_eq!(demo.total_ticks(), 3);
    }

    // ── Test 10: Embedding errors handled gracefully ──

    #[tokio::test]
    async fn embedding_error_falls_back_gracefully() {
        use crate::embedding::EmbeddingError;

        /// Provider that always fails.
        struct FailingProvider;

        #[async_trait::async_trait]
        impl EmbeddingProvider for FailingProvider {
            async fn embed(&self, _text: &str) -> Result<Vec<f32>, EmbeddingError> {
                Err(EmbeddingError::BackendError("test failure".into()))
            }
            async fn embed_batch(&self, _texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
                Err(EmbeddingError::BackendError("test failure".into()))
            }
            fn dimensions(&self) -> usize {
                8
            }
            fn model_name(&self) -> &str {
                "failing-test"
            }
        }

        let cg = Arc::new(CausalGraph::new());
        let vector: Arc<dyn VectorBackend> =
            Arc::new(HnswBackend::new(HnswServiceConfig::default()));
        let iq = Arc::new(ImpulseQueue::new());
        let crs = Arc::new(CrossRefStore::new());
        let emb: Arc<dyn EmbeddingProvider> = Arc::new(FailingProvider);

        let demo = DemocritusLoop::new(
            Arc::clone(&cg),
            Arc::clone(&vector),
            Arc::clone(&iq),
            Arc::clone(&crs),
            emb,
            DemocritusConfig::default(),
        );

        emit_test_impulse(&iq, ImpulseType::BeliefUpdate, 100);

        let result = demo.tick().await;

        // Embedding failed → empty vectors, but tick still completes.
        assert_eq!(result.impulses_sensed, 1);
        assert_eq!(result.embeddings_produced, 1); // fallback produces empty vecs
        // Search with empty vector is skipped (non_empty_queries filter).
        assert_eq!(result.searches_performed, 0);
        // Node still added to causal graph.
        assert_eq!(cg.node_count(), 1);
        // But no vector insertion (empty embedding skipped).
        assert_eq!(vector.len(), 0);
        // Cross-ref still created.
        assert_eq!(result.crossrefs_added, 1);
    }

    // ── Test 11: max_impulses_per_tick truncation ──

    #[tokio::test]
    async fn max_impulses_per_tick_truncation() {
        let config = DemocritusConfig {
            max_impulses_per_tick: 2,
            ..DemocritusConfig::default()
        };
        let (_cg, _vector, iq, _crs, demo) = make_loop_with_config(config);

        // Emit 5 impulses.
        for i in 0..5 {
            emit_test_impulse(&iq, ImpulseType::BeliefUpdate, i);
        }

        let result = demo.tick().await;

        // Only 2 should be processed due to truncation.
        assert_eq!(result.impulses_sensed, 2);
        assert_eq!(result.embeddings_produced, 2);
    }

    // ── Test 12: structure_tag_from_u8 mapping ──

    #[test]
    fn structure_tag_roundtrip() {
        assert_eq!(structure_tag_from_u8(0x01), StructureTag::ExoChain);
        assert_eq!(structure_tag_from_u8(0x02), StructureTag::ResourceTree);
        assert_eq!(structure_tag_from_u8(0x03), StructureTag::CausalGraph);
        assert_eq!(structure_tag_from_u8(0x04), StructureTag::HnswIndex);
        assert_eq!(structure_tag_from_u8(0xFF), StructureTag::Custom(0xFF));
    }

    // ── Sprint 11: Budget exhaustion tests ──────────────────────────

    #[tokio::test]
    async fn budget_exhaustion_with_many_impulses() {
        // Use a budget of 0 microseconds with many impulses to force
        // budget exhaustion at different phases.
        let config = DemocritusConfig {
            tick_budget_us: 0,
            max_impulses_per_tick: 100,
            ..DemocritusConfig::default()
        };
        let (cg, _vector, iq, _crs, demo) = make_loop_with_config(config);

        for i in 0..50 {
            emit_test_impulse(&iq, ImpulseType::BeliefUpdate, i);
        }

        let result = demo.tick().await;
        assert!(result.budget_exceeded);
        // Even with budget exceeded, tick count increments.
        assert_eq!(demo.total_ticks(), 1);
        // Some impulses may have been sensed before budget check.
        assert!(result.impulses_sensed <= 50);
        // Causal graph nodes added should match embeddings completed
        // (may be fewer than sensed due to budget).
        assert!(cg.node_count() <= result.impulses_sensed as u64);
    }

    #[tokio::test]
    async fn budget_exceeded_flag_only_set_when_needed() {
        // Large budget should not trigger budget_exceeded.
        let config = DemocritusConfig {
            tick_budget_us: 10_000_000, // 10 seconds
            ..DemocritusConfig::default()
        };
        let (_, _, iq, _, demo) = make_loop_with_config(config);

        emit_test_impulse(&iq, ImpulseType::BeliefUpdate, 1);
        let result = demo.tick().await;
        assert!(!result.budget_exceeded);
    }

    // ── Sprint 11: ImpulseQueue overflow tests ──────────────────────

    #[tokio::test]
    async fn impulse_queue_large_burst() {
        let config = DemocritusConfig {
            max_impulses_per_tick: 10,
            ..DemocritusConfig::default()
        };
        let (_, _, iq, _, demo) = make_loop_with_config(config);

        // Emit far more impulses than per-tick limit.
        for i in 0..500 {
            emit_test_impulse(&iq, ImpulseType::BeliefUpdate, i);
        }

        // First tick processes at most 10.
        let r1 = demo.tick().await;
        assert_eq!(r1.impulses_sensed, 10);

        // Queue was drained fully (drain_ready takes all), but only 10 processed.
        // Remaining impulses are gone (drain clears the queue).
        let r2 = demo.tick().await;
        assert_eq!(r2.impulses_sensed, 0);
    }

    #[tokio::test]
    async fn impulse_queue_interleaved_emit_and_tick() {
        let (cg, _, iq, _, demo) = make_loop();

        // Emit, tick, emit, tick — verify state accumulates.
        emit_test_impulse(&iq, ImpulseType::BeliefUpdate, 10);
        let r1 = demo.tick().await;
        assert_eq!(r1.impulses_sensed, 1);
        assert_eq!(cg.node_count(), 1);

        emit_test_impulse(&iq, ImpulseType::CoherenceAlert, 20);
        emit_test_impulse(&iq, ImpulseType::NoveltyDetected, 30);
        let r2 = demo.tick().await;
        assert_eq!(r2.impulses_sensed, 2);
        assert_eq!(cg.node_count(), 3);

        assert_eq!(demo.total_ticks(), 2);
        assert_eq!(demo.total_nodes_added(), 3);
    }

    // ── Sprint 11: Embed failure recovery tests ─────────────────────

    #[tokio::test]
    async fn embed_failure_still_creates_crossrefs() {
        use crate::embedding::EmbeddingError;

        struct FailingProvider;

        #[async_trait::async_trait]
        impl EmbeddingProvider for FailingProvider {
            async fn embed(&self, _text: &str) -> Result<Vec<f32>, EmbeddingError> {
                Err(EmbeddingError::BackendError("test failure".into()))
            }
            async fn embed_batch(&self, _texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
                Err(EmbeddingError::BackendError("test failure".into()))
            }
            fn dimensions(&self) -> usize {
                8
            }
            fn model_name(&self) -> &str {
                "failing-test"
            }
        }

        let cg = Arc::new(CausalGraph::new());
        let vector: Arc<dyn VectorBackend> =
            Arc::new(HnswBackend::new(HnswServiceConfig::default()));
        let iq = Arc::new(ImpulseQueue::new());
        let crs = Arc::new(CrossRefStore::new());
        let emb: Arc<dyn EmbeddingProvider> = Arc::new(FailingProvider);

        let demo = DemocritusLoop::new(
            Arc::clone(&cg),
            Arc::clone(&vector),
            Arc::clone(&iq),
            Arc::clone(&crs),
            emb,
            DemocritusConfig::default(),
        );

        // Emit multiple impulses.
        for i in 0..5 {
            emit_test_impulse(&iq, ImpulseType::BeliefUpdate, i * 100);
        }

        let result = demo.tick().await;
        assert_eq!(result.impulses_sensed, 5);
        // Fallback: 5 empty vectors produced.
        assert_eq!(result.embeddings_produced, 5);
        // Causal nodes still created despite embed failure.
        assert_eq!(cg.node_count(), 5);
        // Cross-refs still created.
        assert_eq!(result.crossrefs_added, 5);
        assert_eq!(crs.count(), 5);
        // No vector insertions (empty embeddings skipped).
        assert_eq!(vector.len(), 0);
    }

    #[tokio::test]
    async fn embed_failure_no_edges_added() {
        use crate::embedding::EmbeddingError;

        struct FailingProvider;

        #[async_trait::async_trait]
        impl EmbeddingProvider for FailingProvider {
            async fn embed(&self, _text: &str) -> Result<Vec<f32>, EmbeddingError> {
                Err(EmbeddingError::BackendError("fail".into()))
            }
            async fn embed_batch(&self, _texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
                Err(EmbeddingError::BackendError("fail".into()))
            }
            fn dimensions(&self) -> usize {
                8
            }
            fn model_name(&self) -> &str {
                "fail"
            }
        }

        let cg = Arc::new(CausalGraph::new());
        let vector: Arc<dyn VectorBackend> =
            Arc::new(HnswBackend::new(HnswServiceConfig::default()));
        let iq = Arc::new(ImpulseQueue::new());
        let crs = Arc::new(CrossRefStore::new());
        let emb: Arc<dyn EmbeddingProvider> = Arc::new(FailingProvider);

        let demo = DemocritusLoop::new(
            Arc::clone(&cg),
            Arc::clone(&vector),
            Arc::clone(&iq),
            Arc::clone(&crs),
            emb,
            DemocritusConfig::default(),
        );

        emit_test_impulse(&iq, ImpulseType::BeliefUpdate, 100);
        let result = demo.tick().await;
        // With empty embeddings, search returns no neighbors, so no edges.
        assert_eq!(result.edges_added, 0);
        assert_eq!(demo.total_edges_added(), 0);
    }

    // ── Sprint 11: Multiple sequential ticks with accumulated state ──

    #[tokio::test]
    async fn multiple_sequential_ticks_accumulate_state() {
        let (cg, vector, iq, crs, demo) = make_loop();

        // Tick 1: single impulse.
        emit_test_impulse(&iq, ImpulseType::BeliefUpdate, 100);
        let r1 = demo.tick().await;
        assert_eq!(r1.impulses_sensed, 1);
        let nodes_after_1 = cg.node_count();
        let vector_after_1 = vector.len();

        // Tick 2: two more impulses.
        emit_test_impulse(&iq, ImpulseType::CoherenceAlert, 200);
        emit_test_impulse(&iq, ImpulseType::NoveltyDetected, 300);
        let r2 = demo.tick().await;
        assert_eq!(r2.impulses_sensed, 2);
        assert_eq!(cg.node_count(), nodes_after_1 + 2);
        assert_eq!(vector.len(), vector_after_1 + 2);

        // Tick 3: three more.
        emit_test_impulse(&iq, ImpulseType::EdgeConfirmed, 400);
        emit_test_impulse(&iq, ImpulseType::EmbeddingRefined, 500);
        emit_test_impulse(&iq, ImpulseType::Custom(42), 600);
        let r3 = demo.tick().await;
        assert_eq!(r3.impulses_sensed, 3);
        assert_eq!(cg.node_count(), nodes_after_1 + 5);

        // Total statistics.
        assert_eq!(demo.total_ticks(), 3);
        assert_eq!(demo.total_nodes_added(), 6);
        // Cross-refs: one per impulse.
        assert_eq!(crs.count(), 6);
    }

    #[tokio::test]
    async fn sequential_ticks_can_find_prior_neighbors() {
        let config = DemocritusConfig {
            correlation_threshold: 0.0, // accept all as correlated
            ..DemocritusConfig::default()
        };
        let (cg, vector, iq, _, demo) = make_loop_with_config(config);

        // Tick 1: insert a node.
        emit_test_impulse(&iq, ImpulseType::BeliefUpdate, 100);
        demo.tick().await;
        assert_eq!(vector.len(), 1);

        // Tick 2: same type should find tick-1's node as neighbor.
        emit_test_impulse(&iq, ImpulseType::BeliefUpdate, 200);
        let r2 = demo.tick().await;
        assert_eq!(r2.searches_performed, 1);
        // With threshold=0.0, any non-zero similarity creates an edge.
        // The mock provider produces deterministic vectors, so same impulse
        // type gets same embedding, yielding high similarity.
        // Edges depend on whether neighbor_id parses as a valid node_id.
        assert!(cg.node_count() >= 2);
    }

    // ── Sprint 11: Config edge cases ────────────────────────────────

    #[tokio::test]
    async fn zero_max_impulses_per_tick() {
        let config = DemocritusConfig {
            max_impulses_per_tick: 0,
            ..DemocritusConfig::default()
        };
        let (_, _, iq, _, demo) = make_loop_with_config(config);

        emit_test_impulse(&iq, ImpulseType::BeliefUpdate, 100);
        let result = demo.tick().await;
        // Truncation to 0 means no impulses processed.
        assert_eq!(result.impulses_sensed, 0);
        assert_eq!(result.embeddings_produced, 0);
    }

    #[tokio::test]
    async fn tick_result_duration_is_positive() {
        let (_, _, iq, _, demo) = make_loop();
        emit_test_impulse(&iq, ImpulseType::BeliefUpdate, 100);
        let result = demo.tick().await;
        // Duration should be non-negative (may be 0 on very fast systems).
        assert!(
            result.duration_us < 10_000_000,
            "tick should complete within 10s"
        );
    }

    #[test]
    fn classify_edge_custom_impulse_type() {
        let (_, _, _, _, demo) = make_loop();
        let impulse = Impulse {
            id: 1,
            source_structure: 0,
            source_node: [0u8; 32],
            target_structure: 2,
            impulse_type: ImpulseType::Custom(99),
            payload: serde_json::json!({}),
            hlc_timestamp: 0,
            acknowledged: std::sync::atomic::AtomicBool::new(false),
        };

        // Custom type below threshold → Follows.
        assert_eq!(demo.classify_edge(&impulse, 0.3), CausalEdgeType::Follows);
        // Custom type above threshold → Correlates.
        assert_eq!(
            demo.classify_edge(&impulse, 0.9),
            CausalEdgeType::Correlates
        );
    }

    // ── WEFT-642: chain_seq joinability ──────────────────────────────

    fn emit_impulse_with_chain_seq(
        iq: &ImpulseQueue,
        impulse_type: ImpulseType,
        chain_seq: u64,
        conv_id: &str,
    ) -> u64 {
        // Mirror session_tier::index_turn: payload carries chain_seq and
        // HLC is stamped with the same sequence.
        iq.emit(
            StructureTag::ExoChain.as_u8(),
            [7u8; 32],
            StructureTag::HnswIndex.as_u8(),
            impulse_type,
            serde_json::json!({ "chain_seq": chain_seq, "conv_id": conv_id }),
            chain_seq,
        )
    }

    #[test]
    fn resolve_chain_seq_reads_payload() {
        let with_seq = Impulse {
            id: 1,
            source_structure: 0,
            source_node: [0u8; 32],
            target_structure: 2,
            impulse_type: ImpulseType::BeliefUpdate,
            payload: serde_json::json!({ "chain_seq": 8633u64, "conv_id": "c1" }),
            hlc_timestamp: 8633,
            acknowledged: std::sync::atomic::AtomicBool::new(false),
        };
        assert_eq!(resolve_chain_seq(&with_seq), 8633);

        let missing = Impulse {
            payload: serde_json::json!({ "test": true }),
            ..with_seq.clone()
        };
        assert_eq!(resolve_chain_seq(&missing), 0);

        let non_object = Impulse {
            payload: serde_json::json!("plain"),
            ..with_seq
        };
        assert_eq!(resolve_chain_seq(&non_object), 0);
    }

    /// Joinability assertion (WEFT-642): when the emitter supplies
    /// `payload.chain_seq`, every DEMOCRITUS projection must carry that
    /// sequence — causal node metadata, HNSW metadata, and CrossRef.
    /// A zero chain_seq on any of those would re-open the panopticon
    /// "looks alive but does not join back" class.
    #[tokio::test]
    async fn chain_seq_joinable_on_hnsw_node_and_crossref() {
        let (cg, vector, iq, crs, demo) = make_loop();
        const SEQ: u64 = 42_001;

        emit_impulse_with_chain_seq(&iq, ImpulseType::BeliefUpdate, SEQ, "conv-weft-642");
        let result = demo.tick().await;

        assert_eq!(result.impulses_sensed, 1);
        assert_eq!(result.crossrefs_added, 1);
        assert_eq!(cg.node_count(), 1);
        assert_eq!(vector.len(), 1);

        // Causal node metadata carries chain_seq.
        let node_ids = cg.node_ids();
        assert_eq!(node_ids.len(), 1);
        let node = cg.get_node(node_ids[0]).expect("node present");
        assert_eq!(
            node.metadata.get("chain_seq").and_then(|v| v.as_u64()),
            Some(SEQ),
            "causal node must join to atom spine via chain_seq"
        );

        // Vector backend entry metadata carries the same chain_seq (join key).
        // Use a real embed of the same shape as the loop (type:payload).
        let emb: Arc<dyn EmbeddingProvider> = Arc::new(MockEmbeddingProvider::new(8));
        let query = emb
            .embed(&format!(
                "{}:{}",
                ImpulseType::BeliefUpdate,
                serde_json::json!({ "chain_seq": SEQ, "conv_id": "conv-weft-642" })
            ))
            .await
            .expect("mock embed");
        let hits = vector.search(&query, 5);
        assert!(
            !hits.is_empty(),
            "vector backend must return the inserted brain entry"
        );
        let meta = &hits[0].metadata;
        assert_eq!(
            meta.get("chain_seq").and_then(|v| v.as_u64()),
            Some(SEQ),
            "vector metadata must carry real chain_seq (not hardcoded 0)"
        );
        assert_eq!(
            meta.get("causal_node_id").and_then(|v| v.as_u64()),
            Some(node_ids[0]),
            "vector metadata should point back at the causal node id"
        );

        // CrossRef chain_seq must match.
        let refs = crs.all();
        assert_eq!(refs.len(), 1);
        assert_eq!(
            refs[0].chain_seq, SEQ,
            "CrossRef must carry real chain_seq (not hardcoded 0)"
        );
    }

    /// Edge path (democritus.rs link site): neighbor edges must inherit the
    /// impulse's chain_seq so causal edges also join the atom spine.
    #[tokio::test]
    async fn chain_seq_joinable_on_causal_edges() {
        let config = DemocritusConfig {
            correlation_threshold: 0.0, // force edges on any neighbor hit
            ..DemocritusConfig::default()
        };
        let (cg, vector, iq, _, demo) = make_loop_with_config(config);

        // Seed a neighbor with a known sequence.
        emit_impulse_with_chain_seq(&iq, ImpulseType::BeliefUpdate, 100, "conv-edge");
        demo.tick().await;
        assert_eq!(vector.len(), 1);

        // Second tick: same impulse type → same mock embedding → neighbor hit.
        const SEQ2: u64 = 101;
        emit_impulse_with_chain_seq(&iq, ImpulseType::BeliefUpdate, SEQ2, "conv-edge");
        let r2 = demo.tick().await;
        assert_eq!(r2.searches_performed, 1);

        // Find the newer node and assert its outgoing edges carry SEQ2.
        let mut nodes: Vec<_> = cg
            .node_ids()
            .into_iter()
            .filter_map(|id| cg.get_node(id))
            .collect();
        nodes.sort_by_key(|n| n.id);
        assert!(nodes.len() >= 2, "expected at least two causal nodes");
        let newer = nodes.last().expect("newer node");
        assert_eq!(
            newer.metadata.get("chain_seq").and_then(|v| v.as_u64()),
            Some(SEQ2)
        );

        let edges = cg.get_forward_edges(newer.id);
        // With correlation_threshold=0 and a prior neighbor, at least one edge.
        assert!(
            !edges.is_empty(),
            "expected similarity edge(s) from second impulse to first"
        );
        for edge in &edges {
            assert_eq!(
                edge.chain_seq, SEQ2,
                "causal edge must carry the impulse chain_seq (not hardcoded 0)"
            );
        }
    }

    // ── WEFT-124: VectorBackend wiring ───────────────────────────────

    /// Backend swap is wiring-only: DemocritusLoop talks exclusively to
    /// `Arc<dyn VectorBackend>`. A non-HNSW counting backend still runs
    /// the full Sense→Embed→Search→Update→Commit path.
    #[tokio::test]
    async fn backend_swap_is_wiring_only() {
        let cg = Arc::new(CausalGraph::new());
        let counter = Arc::new(CountingBackend::new("counting-mock"));
        let vector: Arc<dyn VectorBackend> = counter.clone();
        let iq = Arc::new(ImpulseQueue::new());
        let crs = Arc::new(CrossRefStore::new());
        let emb: Arc<dyn EmbeddingProvider> = Arc::new(MockEmbeddingProvider::new(8));

        let demo = DemocritusLoop::new(
            Arc::clone(&cg),
            Arc::clone(&vector),
            Arc::clone(&iq),
            Arc::clone(&crs),
            emb,
            DemocritusConfig {
                correlation_threshold: 0.0,
                ..DemocritusConfig::default()
            },
        );

        assert_eq!(demo.vector_backend().backend_name(), "counting-mock");

        // Tick 1: insert via the swapped backend.
        emit_test_impulse(&iq, ImpulseType::BeliefUpdate, 100);
        let r1 = demo.tick().await;
        assert_eq!(r1.impulses_sensed, 1);
        assert_eq!(r1.searches_performed, 1);
        assert_eq!(counter.insert_count(), 1);
        assert_eq!(counter.search_count(), 1);
        assert_eq!(vector.len(), 1);
        assert_eq!(cg.node_count(), 1);

        // Tick 2: search should hit the prior entry through the same backend.
        emit_test_impulse(&iq, ImpulseType::BeliefUpdate, 200);
        let r2 = demo.tick().await;
        assert_eq!(r2.searches_performed, 1);
        assert_eq!(counter.insert_count(), 2);
        assert_eq!(counter.search_count(), 2);
        assert_eq!(vector.len(), 2);
        // With threshold=0 and a prior neighbor, an edge should form.
        assert!(
            r2.edges_added >= 1 || demo.total_edges_added() >= 1,
            "swapped backend neighbor hit should produce causal edges"
        );
    }

    #[tokio::test]
    async fn default_binding_is_hnsw_backend() {
        let (_cg, vector, _iq, _crs, demo) = make_loop();
        assert_eq!(vector.backend_name(), "hnsw");
        assert_eq!(demo.vector_backend().backend_name(), "hnsw");
    }
}
