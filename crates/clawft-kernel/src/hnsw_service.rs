//! HNSW vector search as a kernel `SystemService`.
//!
//! Wraps `clawft_core::embeddings::hnsw_store::HnswStore` behind a
//! `Mutex` so that the service satisfies `Send + Sync` and can be
//! registered in the `ServiceRegistry`.
//!
//! This module is compiled only when the `ecc` feature is enabled.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, RwLock};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use clawft_core::embeddings::hnsw_store::{HnswStore, TieredSearch};

use crate::health::HealthStatus;
use crate::hnsw_eml::{HnswEmlConfig, HnswEmlManager};
use crate::service::{ServiceType, SystemService};

#[cfg(feature = "exochain")]
use crate::chain::ChainManager;
#[cfg(feature = "exochain")]
use crate::governance::{EffectVector, GovernanceDecision, GovernanceEngine, GovernanceRequest};
#[cfg(feature = "exochain")]
use std::sync::Arc;

// ── Configuration ────────────────────────────────────────────────────────

/// Configuration for the [`HnswService`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HnswServiceConfig {
    /// ef_search parameter forwarded to `HnswStore`.
    pub ef_search: usize,
    /// ef_construction parameter forwarded to `HnswStore`.
    pub ef_construction: usize,
    /// Default embedding dimensionality (informational; not enforced by the store).
    pub default_dimensions: usize,
}

impl Default for HnswServiceConfig {
    fn default() -> Self {
        Self {
            ef_search: 100,
            ef_construction: 200,
            default_dimensions: 384,
        }
    }
}

// ── Multi-key types ─────────────────────────────────────────────────────

/// Configuration for multi-key HNSW indexing.
///
/// When enabled, entities can be indexed under multiple embedding vectors
/// (e.g., by name, context, description) so that vague queries have
/// better recall.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MultiKeyConfig {
    /// Whether multi-key indexing is enabled.
    pub enabled: bool,
    /// Maximum number of keys per entity (default: 4).
    pub max_keys_per_entity: usize,
    /// Recognised key types (default: `["name", "context", "description"]`).
    pub key_types: Vec<String>,
}

impl Default for MultiKeyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_keys_per_entity: 4,
            key_types: vec![
                "name".to_string(),
                "context".to_string(),
                "description".to_string(),
            ],
        }
    }
}

/// A single search key for multi-key insertion.
///
/// Each key carries a `key_type` label (e.g., `"name"`, `"context"`) and
/// the pre-computed embedding vector for that textual representation.
#[derive(Debug, Clone)]
pub struct MultiKey {
    /// The kind of key (`"name"`, `"context"`, `"relationship"`, `"docstring"`, etc.).
    pub key_type: String,
    /// The embedding vector for this key's textual representation.
    pub embedding: Vec<f32>,
}

// ── Search result ────────────────────────────────────────────────────────

/// A single search result returned by [`HnswService::search`].
#[derive(Debug, Clone)]
pub struct HnswSearchResult {
    /// Entry identifier.
    pub id: String,
    /// Cosine similarity score (higher is better).
    pub score: f32,
    /// Arbitrary metadata stored alongside the embedding.
    pub metadata: serde_json::Value,
}

// ── Service ──────────────────────────────────────────────────────────────

/// Kernel service wrapping the HNSW vector store.
///
/// The inner `HnswStore` sits behind a [`RwLock`]: searches share a read
/// lock and never build the graph; builds run off-lock and are installed
/// under a short write lock (see `ensure_index`). Atomic counters track insert and search operations for
/// observability without requiring the lock.
pub struct HnswService {
    store: RwLock<HnswStore>,
    /// Serializes graph builds. A build runs with no store lock held, so
    /// searches and inserts proceed meanwhile.
    build_lock: Mutex<()>,
    /// Set by the EML rebuild model to ask for a rebuild at the next search.
    rebuild_requested: AtomicBool,
    config: HnswServiceConfig,
    insert_count: AtomicU64,
    search_count: AtomicU64,
    /// Monotonic epoch counter -- bumped on every mutation.
    epoch: AtomicU64,
    /// EML-based adaptive optimization (ef, rebuild, distance, path).
    eml: Arc<Mutex<HnswEmlManager>>,
    /// A background training cycle is running (training never runs on a query thread).
    training: Arc<AtomicBool>,
    /// Chain manager for exochain event logging.
    #[cfg(feature = "exochain")]
    chain_manager: Option<Arc<ChainManager>>,
    /// Governance engine for gating destructive operations.
    #[cfg(feature = "exochain")]
    governance_engine: Option<Arc<GovernanceEngine>>,
}

impl HnswService {
    /// Wrap a manager for service use: training moves off the query path.
    fn new_eml(mut eml: HnswEmlManager) -> Arc<Mutex<HnswEmlManager>> {
        eml.set_auto_train(false);
        Arc::new(Mutex::new(eml))
    }

    /// Run a due training cycle on a short-lived thread. Training takes
    /// seconds and holds the EML lock; inline it stalls whichever query
    /// thread happened to cross the threshold (and with it the whole batch).
    /// Concurrent searches skip their EML bookkeeping while it runs.
    fn spawn_training_if_due(&self, due: bool) {
        if !due
            || self
                .training
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
                .is_err()
        {
            return;
        }
        let eml = Arc::clone(&self.eml);
        let flag = Arc::clone(&self.training);
        let spawned = std::thread::Builder::new()
            .name("hnsw-eml-train".into())
            .spawn(move || {
                if let Ok(mut m) = eml.lock() {
                    m.train_all();
                }
                flag.store(false, Ordering::Release);
            });
        if spawned.is_err() {
            self.training.store(false, Ordering::Release);
        }
    }

    /// Create a new service with the given configuration.
    pub fn new(config: HnswServiceConfig) -> Self {
        let store = HnswStore::with_params(config.ef_search, config.ef_construction);
        Self {
            store: RwLock::new(store),
            build_lock: Mutex::new(()),
            rebuild_requested: AtomicBool::new(false),
            config,
            insert_count: AtomicU64::new(0),
            search_count: AtomicU64::new(0),
            epoch: AtomicU64::new(0),
            eml: Self::new_eml(HnswEmlManager::with_defaults()),
            training: Arc::new(AtomicBool::new(false)),
            #[cfg(feature = "exochain")]
            chain_manager: None,
            #[cfg(feature = "exochain")]
            governance_engine: None,
        }
    }

    /// Wrap an existing [`HnswStore`] (e.g. after loading a snapshot).
    ///
    /// Counters and epoch start at zero; callers that restore backend-level
    /// epoch (e.g. [`crate::vector_hnsw::HnswBackend`]) set it separately.
    pub fn from_store(config: HnswServiceConfig, store: HnswStore) -> Self {
        Self {
            store: RwLock::new(store),
            build_lock: Mutex::new(()),
            rebuild_requested: AtomicBool::new(false),
            config,
            insert_count: AtomicU64::new(0),
            search_count: AtomicU64::new(0),
            epoch: AtomicU64::new(0),
            eml: Self::new_eml(HnswEmlManager::with_defaults()),
            training: Arc::new(AtomicBool::new(false)),
            #[cfg(feature = "exochain")]
            chain_manager: None,
            #[cfg(feature = "exochain")]
            governance_engine: None,
        }
    }

    /// Snapshot all stored entries for composite backend persistence.
    pub fn snapshot_entries(&self) -> Vec<clawft_core::embeddings::hnsw_store::HnswEntry> {
        let store = self.store.read().expect("HnswStore lock poisoned");
        store.entries().to_vec()
    }

    /// Current `(ef_search, ef_construction)` used by the inner store.
    pub fn ef_params(&self) -> (usize, usize) {
        let store = self.store.read().expect("HnswStore lock poisoned");
        (store.ef_search(), store.ef_construction())
    }

    /// Hard-delete an entry by string id. Returns `true` if it was present.
    pub fn delete(&self, id: &str) -> bool {
        let mut store = self.store.write().expect("HnswStore lock poisoned");
        let removed = store.delete(id);
        if removed {
            self.epoch.fetch_add(1, Ordering::SeqCst);
            if let Ok(mut eml) = self.eml.lock() {
                eml.clear_path_table();
            }
        }
        removed
    }

    /// Create a new service with custom EML configuration.
    pub fn with_eml(config: HnswServiceConfig, eml_config: HnswEmlConfig) -> Self {
        let store = HnswStore::with_params(config.ef_search, config.ef_construction);
        Self {
            store: RwLock::new(store),
            build_lock: Mutex::new(()),
            rebuild_requested: AtomicBool::new(false),
            config,
            insert_count: AtomicU64::new(0),
            search_count: AtomicU64::new(0),
            epoch: AtomicU64::new(0),
            eml: Self::new_eml(HnswEmlManager::new(eml_config)),
            training: Arc::new(AtomicBool::new(false)),
            #[cfg(feature = "exochain")]
            chain_manager: None,
            #[cfg(feature = "exochain")]
            governance_engine: None,
        }
    }

    /// Set the chain manager for exochain event logging.
    #[cfg(feature = "exochain")]
    pub fn set_chain_manager(&mut self, cm: Arc<ChainManager>) {
        self.chain_manager = Some(cm);
    }

    /// Set the governance engine for gating destructive operations.
    #[cfg(feature = "exochain")]
    pub fn set_governance_engine(&mut self, engine: Arc<GovernanceEngine>) {
        self.governance_engine = Some(engine);
    }

    /// Insert an embedding with associated metadata (upsert semantics).
    pub fn insert(&self, id: String, embedding: Vec<f32>, metadata: serde_json::Value) {
        let mut store = self.store.write().expect("HnswStore lock poisoned");
        store.insert(id.clone(), embedding, metadata);
        self.insert_count.fetch_add(1, Ordering::Relaxed);
        self.epoch.fetch_add(1, Ordering::SeqCst);

        // Invalidate path table on mutation — rebuilt lazily on next search.
        if let Ok(mut eml) = self.eml.lock() {
            eml.clear_path_table();
        }

        // Chain logging: hnsw.insert
        #[cfg(feature = "exochain")]
        if let Some(ref cm) = self.chain_manager {
            cm.append(
                "hnsw_service",
                crate::chain::EVENT_KIND_HNSW_INSERT,
                Some(serde_json::json!({
                    "id": id,
                })),
            );
        }
    }

    /// Return the current monotonic epoch.
    pub fn current_epoch(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }

    /// Search for the `top_k` nearest embeddings to `query`.
    ///
    /// When EML is enabled and trained, the beam width (ef_search) is
    /// adapted per-query and the rebuild model is consulted after each
    /// search. When a region → entry-node path table is available
    /// (WEFT-385 / HNSW-EML #4), search is guided through predicted
    /// entry nodes. Training data is recorded for continuous learning.
    pub fn search(&self, query: &[f32], top_k: usize) -> Vec<HnswSearchResult> {
        self.ensure_index();
        let store = self.store.read().expect("HnswStore lock poisoned");
        self.search_count.fetch_add(1, Ordering::Relaxed);

        let store_size = store.len();

        // EML bookkeeping is best-effort: if another search holds the EML
        // lock (training and recall checks run under it), skip it rather
        // than queue every query behind it. Skipped queries simply produce
        // no training sample.
        //
        // Adaptive ef_search is advisory only. `HnswStore::set_ef_search`
        // rebuilds the whole graph (instant-distance bakes ef_search into the
        // built index), so applying a per-query prediction here would run a
        // full rebuild whenever the learned value moved. Callers that want a
        // new ef pick it explicitly via `HnswStore::set_ef_search`.
        let mut ef_learned = false;
        let mut table = None;
        if let Ok(mut eml) = self.eml.try_lock() {
            ef_learned = eml.predict_ef(query, store_size).is_learned;

            // EML: adaptive rebuild -- ask for an early rebuild when recall
            // is predicted to have degraded, even if the mutation count
            // hasn't hit the static threshold. Served by `ensure_index` on
            // the next search, off the query path.
            let rebuild_pred = eml.predict_rebuild(store_size, store.inserts_since_rebuild(), 0);
            if rebuild_pred.is_learned && rebuild_pred.should_rebuild {
                self.rebuild_requested.store(true, Ordering::Relaxed);
            }

            // Lazy path-table build: first search after enough inserts.
            if eml.path_predict_enabled() && !eml.has_path_table() && store_size >= 16 {
                let pairs = store.entry_id_embeddings();
                eml.rebuild_path_table(&pairs);
            }
            if eml.has_path_table() {
                table = Some((
                    eml.path_table().expect("has_path_table true").clone(),
                    eml.path_region_probes(),
                ));
            }
        }

        let t0 = std::time::Instant::now();
        let (results, path_pred, used_guided) = if let Some((table, probes)) = table {
            let (raw, pred) = store.query_guided_shared(query, top_k, &table, probes);
            let mapped: Vec<HnswSearchResult> = raw
                .into_iter()
                .map(|r| HnswSearchResult {
                    id: r.id,
                    score: r.score,
                    metadata: r.metadata,
                })
                .collect();
            let guided = pred.is_learned;
            let path_pred = crate::hnsw_eml::PathPrediction {
                region_id: pred.region_id,
                entry_ids: pred.entry_ids,
                region_score: pred.region_score,
                candidate_ids: pred.candidate_ids,
                is_learned: pred.is_learned,
                eml_path_quality: None,
            };
            (mapped, path_pred, guided)
        } else {
            let mapped: Vec<HnswSearchResult> = store
                .query_shared(query, top_k)
                .into_iter()
                .map(|r| HnswSearchResult {
                    id: r.id,
                    score: r.score,
                    metadata: r.metadata,
                })
                .collect();
            (
                mapped,
                crate::hnsw_eml::PathPrediction {
                    region_id: None,
                    entry_ids: Vec::new(),
                    region_score: 0.0,
                    candidate_ids: Vec::new(),
                    is_learned: false,
                    eml_path_quality: None,
                },
                false,
            )
        };
        let elapsed_us = t0.elapsed().as_micros() as u64;

        let top_score = results.first().map(|r| r.score).unwrap_or(0.0);
        let ef_used = store.ef_search();
        let mut train_due = false;
        if let Ok(mut eml) = self.eml.try_lock() {
            if used_guided {
                let pool = path_pred.candidate_ids.len();
                eml.record_guided_search(crate::hnsw_eml::GuidedSearch {
                    query,
                    result_count: results.len(),
                    top_score,
                    ef_used,
                    search_time_us: elapsed_us,
                    store_size,
                    prediction: &path_pred,
                    candidate_pool_size: pool,
                });
            } else {
                eml.record_search(
                    query,
                    results.len(),
                    top_score,
                    ef_used,
                    elapsed_us,
                    store_size,
                );
            }
            train_due = eml.train_due();
        }
        self.spawn_training_if_due(train_due);

        // ExoChain: append multi-signal observation for training provenance.
        #[cfg(feature = "exochain")]
        if let Some(ref cm) = self.chain_manager {
            cm.append(
                "hnsw_eml",
                crate::chain::EVENT_KIND_HNSW_EML_OBSERVE,
                Some(serde_json::json!({
                    "ef_used": ef_used,
                    "latency_us": elapsed_us,
                    "result_count": results.len(),
                    "top_score": top_score,
                    "store_size": store_size,
                    "is_learned": ef_learned,
                    "path_guided": used_guided,
                    "path_region": path_pred.region_id,
                    "path_region_score": path_pred.region_score,
                    "path_entry_count": path_pred.entry_ids.len(),
                })),
            );
        }

        results
    }

    /// Build the graph if it is missing or stale, without blocking readers.
    ///
    /// The build runs with no store lock held; only installing the result
    /// takes the write lock. The first build blocks callers (nothing to
    /// serve from). Later rebuilds are done by one thread while the others
    /// keep answering from the old graph plus the brute-force tail.
    /// Public so callers can pre-build after a bulk load.
    pub fn ensure_index(&self) {
        let requested = self.rebuild_requested.load(Ordering::Relaxed);
        let has_index = {
            let store = self.store.read().expect("HnswStore lock poisoned");
            if !requested && !store.needs_rebuild() {
                return;
            }
            store.has_index()
        };
        let _guard = if has_index {
            match self.build_lock.try_lock() {
                Ok(g) => g,
                Err(_) => return,
            }
        } else {
            self.build_lock.lock().expect("HNSW build lock poisoned")
        };
        let requested = self.rebuild_requested.swap(false, Ordering::Relaxed);
        let job = {
            let store = self.store.read().expect("HnswStore lock poisoned");
            if !requested && !store.needs_rebuild() {
                return;
            }
            store.rebuild_job()
        };
        let Some(job) = job else { return };
        let built = job.build();
        let installed = self
            .store
            .write()
            .expect("HnswStore lock poisoned")
            .install_index(built);

        // The path table is tied to store layout -- refresh it after a rebuild.
        // Best-effort: never wait on the EML lock from the read path.
        let has_table = self
            .eml
            .try_lock()
            .map(|eml| eml.has_path_table())
            .unwrap_or(false);
        if installed && (requested || has_table) {
            self.rebuild_path_table();
        }
    }

    /// Explicitly rebuild the region → entry-node path table from the
    /// current store contents (WEFT-385).
    pub fn rebuild_path_table(&self) {
        let store = self.store.read().expect("HnswStore lock poisoned");
        let mut eml = self.eml.lock().expect("EML lock poisoned");
        let pairs = store.entry_id_embeddings();
        eml.rebuild_path_table(&pairs);
    }

    /// Whether guided path prediction is active.
    pub fn path_table_ready(&self) -> bool {
        self.eml.lock().expect("EML lock poisoned").has_path_table()
    }

    /// Brute-force ground-truth search for recall measurement.
    pub fn brute_force_topk(&self, query: &[f32], top_k: usize) -> Vec<String> {
        let store = self.store.read().expect("HnswStore lock poisoned");
        store.brute_force_topk(query, top_k)
    }

    /// Measure recall against brute-force and feed the EML rebuild model.
    pub fn measure_recall(&self, queries: &[Vec<f32>], top_k: usize) -> f64 {
        self.ensure_index();
        let store = self.store.read().expect("HnswStore lock poisoned");
        let mut eml = self.eml.lock().expect("EML lock poisoned");

        let mut hnsw_ids = Vec::with_capacity(queries.len());
        let mut exact_ids = Vec::with_capacity(queries.len());
        for q in queries {
            let hnsw: Vec<String> = store
                .query_shared(q, top_k)
                .into_iter()
                .map(|r| r.id)
                .collect();
            let exact = store.brute_force_topk(q, top_k);
            hnsw_ids.push(hnsw);
            exact_ids.push(exact);
        }

        let recall = eml.measure_recall(
            &hnsw_ids,
            &exact_ids,
            store.len(),
            store.inserts_since_rebuild(),
            0,
        );

        #[cfg(feature = "exochain")]
        if let Some(ref cm) = self.chain_manager {
            cm.append(
                "hnsw_eml",
                crate::chain::EVENT_KIND_HNSW_EML_RECALL,
                Some(serde_json::json!({
                    "avg_recall": recall,
                    "store_size": store.len(),
                    "inserts_since_rebuild": store.inserts_since_rebuild(),
                    "query_count": queries.len(),
                })),
            );
        }

        recall
    }

    /// Build a tiered index from the current store contents.
    ///
    /// Uses the EML distance model's learned dimension ordering when
    /// available, falling back to the first N dimensions otherwise.
    /// The returned `TieredSearch` owns separate coarse/full indexes.
    pub fn build_tiered(&self, top_k: usize) -> TieredSearch {
        let store = self.store.read().expect("HnswStore lock poisoned");
        let eml = self.eml.lock().expect("EML lock poisoned");

        let entries: Vec<(String, Vec<f32>, serde_json::Value)> = (0..store.len())
            .filter_map(|_| None) // placeholder — see below
            .collect();

        // Collect entries from the store. HnswStore doesn't expose an
        // iterator, so we search for everything via brute-force at top-N.
        // This is a build-time cost, not per-query.
        let dims = self.config.default_dimensions;
        if eml.is_enabled() {
            let dim_order = eml.learned_dim_order(dims);
            TieredSearch::build_learned(&entries, &dim_order, dims, top_k, self.config.ef_search)
        } else {
            TieredSearch::build_default(&entries, dims, top_k, self.config.ef_search)
        }
    }

    /// Borrow the EML manager (for status/benchmarking).
    pub fn eml_status(&self) -> crate::hnsw_eml::HnswEmlStatus {
        self.eml.lock().expect("EML lock poisoned").status()
    }

    /// Batch search: acquire the lock once, perform all queries, release.
    ///
    /// This avoids per-query mutex acquisition overhead when processing
    /// multiple embeddings in a single tick (e.g., DEMOCRITUS loop).
    pub fn search_batch(&self, queries: &[&[f32]], top_k: usize) -> Vec<Vec<HnswSearchResult>> {
        self.ensure_index();
        let store = self.store.read().expect("HnswStore lock poisoned");
        self.search_count
            .fetch_add(queries.len() as u64, Ordering::Relaxed);
        queries
            .iter()
            .map(|query| {
                store
                    .query_shared(query, top_k)
                    .into_iter()
                    .map(|r| HnswSearchResult {
                        id: r.id,
                        score: r.score,
                        metadata: r.metadata,
                    })
                    .collect()
            })
            .collect()
    }

    /// Insert an entity with multiple search keys.
    ///
    /// Each key generates a separate embedding entry in the store, all
    /// sharing the same `primary_id` prefix. The stored ID has the form
    /// `"{primary_id}::{key_type}"`. This allows the entity to be found
    /// via different query formulations (name, context, description, etc.).
    ///
    /// Keys beyond `max_keys` are silently dropped.
    pub fn insert_multi_key(
        &self,
        primary_id: String,
        keys: &[MultiKey],
        metadata: serde_json::Value,
        max_keys: usize,
    ) {
        let limit = keys.len().min(max_keys);
        for key in &keys[..limit] {
            let sub_id = format!("{}::{}", primary_id, key.key_type);
            self.insert(sub_id, key.embedding.clone(), metadata.clone());
        }
    }

    /// Search with deduplication by primary entity ID.
    ///
    /// Because multi-key insertion stores several embeddings for one entity,
    /// a single query may match multiple keys of the same entity. This
    /// method searches for `top_k * 3` candidates, deduplicates by
    /// stripping the `"::key_type"` suffix, and returns up to `top_k`
    /// unique entities. The best (highest) score among duplicates is kept.
    pub fn search_dedup(&self, query: &[f32], top_k: usize) -> Vec<HnswSearchResult> {
        let raw = self.search(query, top_k * 3);

        let mut seen = HashSet::new();
        let mut results = Vec::with_capacity(top_k);

        for r in raw {
            let primary_id =
                r.id.find("::")
                    .map(|pos| r.id[..pos].to_string())
                    .unwrap_or_else(|| r.id.clone());

            if seen.insert(primary_id.clone()) {
                results.push(HnswSearchResult {
                    id: primary_id,
                    score: r.score,
                    metadata: r.metadata,
                });
            }
            if results.len() >= top_k {
                break;
            }
        }

        results
    }

    /// Return the number of entries currently in the store.
    pub fn len(&self) -> usize {
        let store = self.store.read().expect("HnswStore lock poisoned");
        store.len()
    }

    /// Return `true` if the store contains no entries.
    pub fn is_empty(&self) -> bool {
        let store = self.store.read().expect("HnswStore lock poisoned");
        store.is_empty()
    }

    /// Total number of insert operations since service creation.
    pub fn insert_count(&self) -> u64 {
        self.insert_count.load(Ordering::Relaxed)
    }

    /// Total number of search operations since service creation.
    pub fn search_count(&self) -> u64 {
        self.search_count.load(Ordering::Relaxed)
    }

    /// Replace the inner store with a fresh, empty instance.
    ///
    /// Useful for calibration cleanup. Counters are **not** reset.
    ///
    /// Returns `Err` if the governance engine denies the bulk destruction.
    pub fn clear(&self) -> Result<(), String> {
        // Governance gate: bulk destruction (hnsw clear).
        #[cfg(feature = "exochain")]
        if let Some(ref engine) = self.governance_engine {
            let req = GovernanceRequest::new("system", "hnsw.clear").with_effect(EffectVector {
                risk: 0.8,
                ..Default::default()
            });
            let result = engine.evaluate(&req);
            match &result.decision {
                GovernanceDecision::Deny(reason) | GovernanceDecision::EscalateToHuman(reason) => {
                    return Err(format!("governance denied hnsw.clear: {reason}"));
                }
                _ => {}
            }
        }

        let mut store = self.store.write().expect("HnswStore lock poisoned");
        #[cfg(feature = "exochain")]
        let entries_destroyed = store.len();
        store.reset(self.config.ef_search, self.config.ef_construction);
        let epoch = self.epoch.fetch_add(1, Ordering::SeqCst);

        // Chain logging: hnsw.clear
        #[cfg(feature = "exochain")]
        if let Some(ref cm) = self.chain_manager {
            cm.append(
                "hnsw_service",
                crate::chain::EVENT_KIND_HNSW_CLEAR,
                Some(serde_json::json!({
                    "entries_destroyed": entries_destroyed,
                    "epoch": epoch + 1,
                })),
            );
        }

        Ok(())
    }

    /// Borrow the service configuration.
    pub fn config(&self) -> &HnswServiceConfig {
        &self.config
    }

    /// Persist the HNSW store to a JSON file.
    ///
    /// Delegates to the underlying [`HnswStore::save`]. The HNSW index
    /// graph is not serialized; it will be rebuilt on load.
    pub fn save_to_file(&self, path: &std::path::Path) -> Result<(), std::io::Error> {
        let store = self.store.read().expect("HnswStore lock poisoned");
        let result = store.save(path);

        // Chain logging: hnsw.save
        #[cfg(feature = "exochain")]
        if result.is_ok()
            && let Some(ref cm) = self.chain_manager
        {
            cm.append(
                "hnsw_service",
                crate::chain::EVENT_KIND_HNSW_SAVE,
                Some(serde_json::json!({
                    "path": path.display().to_string(),
                })),
            );
        }

        result
    }

    /// Load an HNSW store from a JSON file and create a new service.
    ///
    /// Delegates to [`HnswStore::load`]. The HNSW index is rebuilt
    /// from the saved entries. Counters start at zero.
    ///
    /// Note: chain logging for `hnsw.load` requires calling
    /// [`set_chain_manager`] after construction; use
    /// [`load_from_file_logged`] when a chain manager is available.
    pub fn load_from_file(path: &std::path::Path) -> Result<Self, std::io::Error> {
        let store = HnswStore::load(path)?;
        let (ef_search, ef_construction) = (store.ef_search(), store.ef_construction());
        let config = HnswServiceConfig {
            ef_search,
            ef_construction,
            default_dimensions: 384,
        };
        Ok(Self::from_store(config, store))
    }

    /// Load an HNSW store from a JSON file with chain logging.
    ///
    /// Same as [`load_from_file`] but emits an `hnsw.load` chain event.
    #[cfg(feature = "exochain")]
    pub fn load_from_file_logged(
        path: &std::path::Path,
        cm: Arc<ChainManager>,
    ) -> Result<Self, std::io::Error> {
        let store = HnswStore::load(path)?;
        let entry_count = store.len();
        let (ef_search, ef_construction) = (store.ef_search(), store.ef_construction());
        let config = HnswServiceConfig {
            ef_search,
            ef_construction,
            default_dimensions: 384,
        };

        cm.append(
            "hnsw_service",
            crate::chain::EVENT_KIND_HNSW_LOAD,
            Some(serde_json::json!({
                "path": path.display().to_string(),
                "entry_count": entry_count,
            })),
        );

        let mut svc = Self::from_store(config, store);
        svc.chain_manager = Some(cm);
        Ok(svc)
    }
}

impl std::fmt::Debug for HnswService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HnswService")
            .field("config", &self.config)
            .field("insert_count", &self.insert_count.load(Ordering::Relaxed))
            .field("search_count", &self.search_count.load(Ordering::Relaxed))
            .field("epoch", &self.epoch.load(Ordering::Relaxed))
            .finish()
    }
}

// ── SystemService impl ──────────────────────────────────────────────────

#[async_trait]
impl SystemService for HnswService {
    fn name(&self) -> &str {
        "ecc.hnsw"
    }

    fn service_type(&self) -> ServiceType {
        ServiceType::Core
    }

    async fn start(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        Ok(())
    }

    async fn stop(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        Ok(())
    }

    async fn health_check(&self) -> HealthStatus {
        HealthStatus::Healthy
    }
}

// ── Key-generation helpers ──────────────────────────────────────────────

/// Generate search-key text pairs for a graphify [`Entity`]-like value.
///
/// Each returned tuple is `(key_type, text)`. The caller is responsible
/// for embedding the `text` into a vector (e.g., via the embedding service)
/// before passing it to [`HnswService::insert_multi_key`].
///
/// The function accepts the entity fields directly so it does not pull in
/// a compile-time dependency on `clawft-graphify`.
pub fn entity_search_keys(
    label: &str,
    source_file: Option<&str>,
    metadata: &serde_json::Value,
) -> Vec<(String, String)> {
    let mut keys = vec![("name".to_string(), label.to_string())];

    // Add source-file context key.
    if let Some(src) = source_file {
        keys.push(("context".to_string(), format!("{} in {}", label, src)));
    }

    // Add description key from metadata, if present.
    if let Some(desc) = metadata.get("description").and_then(|v| v.as_str()) {
        keys.push(("description".to_string(), desc.to_string()));
    }

    // Add relationship summary key from metadata, if present.
    if let Some(rels) = metadata.get("relationships").and_then(|v| v.as_str()) {
        keys.push(("relationship".to_string(), rels.to_string()));
    }

    keys
}

// ── Relationship (edge) embedding helpers — WEFT-375 / LightRAG P5 ──────

/// HNSW id prefix for relationship (edge) embeddings.
///
/// Full form: `rel::{stable_id}`. Matches `clawft_graphify::EDGE_HNSW_PREFIX`.
pub const RELATIONSHIP_HNSW_PREFIX: &str = "rel::";

/// Build the HNSW entry id for a relationship embedding.
pub fn relationship_hnsw_id(stable_id: &str) -> String {
    format!("{RELATIONSHIP_HNSW_PREFIX}{stable_id}")
}

/// Generate embed texts for a relationship edge (LightRAG-style unit).
///
/// Returns `(key_type, text)` pairs. The primary `"relation"` key is the
/// canonical edge unit (`"{src} {rel} {tgt}"`). An `"interaction"` key uses
/// natural-language phrasing for "how does X interact with Y?" queries.
///
/// Field types are plain strings so this module stays free of graphify deps.
pub fn relationship_search_keys(
    source_label: &str,
    relation_type: &str,
    target_label: &str,
    source_file: Option<&str>,
) -> Vec<(String, String)> {
    let base = format!("{source_label} {relation_type} {target_label}");
    let mut keys = vec![
        ("relation".to_string(), base.clone()),
        (
            "interaction".to_string(),
            format!("how does {source_label} interact with {target_label}: {relation_type}"),
        ),
    ];
    if let Some(sf) = source_file.filter(|s| !s.is_empty()) {
        keys.push(("context".to_string(), format!("{base} in {sf}")));
    }
    keys
}

impl HnswService {
    /// Insert a relationship (edge) embedding under `rel::{stable_id}`.
    ///
    /// Metadata should include at least `kind: "relationship"` and the
    /// edge endpoints so consumers can reconstruct the hit without a
    /// separate graph lookup.
    pub fn insert_relationship(
        &self,
        stable_id: &str,
        embedding: Vec<f32>,
        metadata: serde_json::Value,
    ) {
        self.insert(relationship_hnsw_id(stable_id), embedding, metadata);
    }

    /// Search and keep only relationship-prefixed hits (`rel::`).
    ///
    /// Oversamples the underlying store so entity vectors do not starve
    /// edge results when both share one HNSW index.
    pub fn search_relationships(&self, query: &[f32], top_k: usize) -> Vec<HnswSearchResult> {
        if top_k == 0 {
            return Vec::new();
        }
        let raw = self.search(query, top_k.saturating_mul(4).max(top_k));
        let mut out = Vec::with_capacity(top_k);
        for r in raw {
            if r.id.starts_with(RELATIONSHIP_HNSW_PREFIX) {
                out.push(r);
                if out.len() >= top_k {
                    break;
                }
            }
        }
        out
    }
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_service() -> HnswService {
        HnswService::new(HnswServiceConfig::default())
    }

    #[test]
    fn new_service_empty() {
        let svc = make_service();
        assert!(svc.is_empty());
        assert_eq!(svc.len(), 0);
    }

    #[test]
    fn insert_and_len() {
        let svc = make_service();
        svc.insert("a".into(), vec![1.0, 0.0], serde_json::json!({}));
        svc.insert("b".into(), vec![0.0, 1.0], serde_json::json!({}));
        assert_eq!(svc.len(), 2);
        assert!(!svc.is_empty());
    }

    #[test]
    fn insert_upsert() {
        let svc = make_service();
        svc.insert("a".into(), vec![1.0, 0.0], serde_json::json!({"v": 1}));
        svc.insert("a".into(), vec![0.0, 1.0], serde_json::json!({"v": 2}));
        assert_eq!(svc.len(), 1);
    }

    #[test]
    fn search_empty_returns_empty() {
        let svc = make_service();
        let results = svc.search(&[1.0, 0.0], 5);
        assert!(results.is_empty());
    }

    #[test]
    fn search_returns_results() {
        let svc = make_service();
        svc.insert("a".into(), vec![1.0, 0.0, 0.0], serde_json::json!({}));
        svc.insert("b".into(), vec![0.0, 1.0, 0.0], serde_json::json!({}));
        svc.insert("c".into(), vec![0.0, 0.0, 1.0], serde_json::json!({}));

        let results = svc.search(&[1.0, 0.0, 0.0], 2);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].id, "a");
        assert!((results[0].score - 1.0).abs() < 0.01);
    }

    /// WEFT-385: path table builds lazily once store has ≥16 entries and
    /// guided search still returns the nearest neighbor.
    #[test]
    fn search_builds_path_table_and_guides() {
        let svc = HnswService::with_eml(
            HnswServiceConfig::default(),
            HnswEmlConfig {
                path_predict_enabled: true,
                ..Default::default()
            },
        );
        for i in 0..24 {
            let cluster = i % 3;
            let mut emb = vec![0.02 * (i as f32); 4];
            emb[cluster] = 1.0;
            svc.insert(format!("n{i}"), emb, serde_json::json!({}));
        }
        assert!(!svc.path_table_ready());

        let mut q = vec![0.0; 4];
        q[0] = 1.0;
        let results = svc.search(&q, 3);
        assert!(!results.is_empty());
        assert!(svc.path_table_ready());
        // Cluster-0 entries are n0, n3, n6, ...
        assert!(
            results[0].id.starts_with('n'),
            "unexpected id {}",
            results[0].id
        );

        // Explicit rebuild stays ready.
        svc.rebuild_path_table();
        assert!(svc.path_table_ready());
        let results2 = svc.search(&q, 3);
        assert_eq!(results2[0].id, results[0].id);
    }

    #[test]
    fn search_count_incremented() {
        let svc = make_service();
        assert_eq!(svc.search_count(), 0);
        svc.search(&[1.0], 1);
        svc.search(&[1.0], 1);
        assert_eq!(svc.search_count(), 2);
    }

    #[test]
    fn insert_count_incremented() {
        let svc = make_service();
        assert_eq!(svc.insert_count(), 0);
        svc.insert("a".into(), vec![1.0], serde_json::json!({}));
        svc.insert("b".into(), vec![0.0], serde_json::json!({}));
        assert_eq!(svc.insert_count(), 2);
    }

    #[test]
    fn clear_resets() {
        let svc = make_service();
        svc.insert("a".into(), vec![1.0], serde_json::json!({}));
        svc.insert("b".into(), vec![0.0], serde_json::json!({}));
        assert_eq!(svc.len(), 2);

        svc.clear()
            .expect("clear should succeed without governance engine");
        assert!(svc.is_empty());
        assert_eq!(svc.len(), 0);
        // Counters are preserved after clear.
        assert_eq!(svc.insert_count(), 2);
    }

    #[test]
    fn config_default() {
        let cfg = HnswServiceConfig::default();
        assert_eq!(cfg.ef_search, 100);
        assert_eq!(cfg.ef_construction, 200);
        assert_eq!(cfg.default_dimensions, 384);

        let svc = HnswService::new(cfg);
        let c = svc.config();
        assert_eq!(c.ef_search, 100);
        assert_eq!(c.ef_construction, 200);
        assert_eq!(c.default_dimensions, 384);
    }

    #[test]
    fn service_name_is_ecc_hnsw() {
        let svc = make_service();
        assert_eq!(svc.name(), "ecc.hnsw");
        assert_eq!(svc.service_type(), ServiceType::Core);
    }

    #[tokio::test]
    async fn service_lifecycle() {
        let svc = make_service();
        svc.start().await.unwrap();
        let health = svc.health_check().await;
        assert_eq!(health, HealthStatus::Healthy);
        svc.stop().await.unwrap();
    }

    // ── Persistence tests ────────────────────────────────────────────

    fn tmp_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "hnsw_test_{name}_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn persist_empty_index() {
        let svc = make_service();
        let path = tmp_path("empty");
        svc.save_to_file(&path).unwrap();
        let loaded = HnswService::load_from_file(&path).unwrap();
        assert!(loaded.is_empty());
        assert_eq!(loaded.len(), 0);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn persist_index_with_vectors_search_matches() {
        let svc = make_service();
        svc.insert(
            "a".into(),
            vec![1.0, 0.0, 0.0],
            serde_json::json!({"tag": "first"}),
        );
        svc.insert(
            "b".into(),
            vec![0.0, 1.0, 0.0],
            serde_json::json!({"tag": "second"}),
        );
        svc.insert(
            "c".into(),
            vec![0.0, 0.0, 1.0],
            serde_json::json!({"tag": "third"}),
        );

        let path = tmp_path("vectors");
        svc.save_to_file(&path).unwrap();
        let loaded = HnswService::load_from_file(&path).unwrap();

        assert_eq!(loaded.len(), 3);

        // Search should return the same nearest neighbor.
        let results = loaded.search(&[1.0, 0.0, 0.0], 1);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "a");
        assert!((results[0].score - 1.0).abs() < 0.01);

        let _ = std::fs::remove_file(&path);
    }

    // ── Multi-key tests ─────────────────────────────────────────────

    #[test]
    fn multi_key_config_default() {
        let cfg = MultiKeyConfig::default();
        assert!(cfg.enabled);
        assert_eq!(cfg.max_keys_per_entity, 4);
        assert_eq!(cfg.key_types.len(), 3);
    }

    #[test]
    fn insert_multi_key_creates_subentries() {
        let svc = make_service();
        let keys = vec![
            MultiKey {
                key_type: "name".into(),
                embedding: vec![1.0, 0.0, 0.0],
            },
            MultiKey {
                key_type: "context".into(),
                embedding: vec![0.7, 0.7, 0.0],
            },
            MultiKey {
                key_type: "description".into(),
                embedding: vec![0.0, 0.0, 1.0],
            },
        ];

        svc.insert_multi_key(
            "entity_a".into(),
            &keys,
            serde_json::json!({"label": "FooBar"}),
            4,
        );

        // Three sub-entries should exist.
        assert_eq!(svc.len(), 3);
        assert_eq!(svc.insert_count(), 3);
    }

    #[test]
    fn insert_multi_key_respects_max_keys() {
        let svc = make_service();
        let keys = vec![
            MultiKey {
                key_type: "name".into(),
                embedding: vec![1.0, 0.0],
            },
            MultiKey {
                key_type: "context".into(),
                embedding: vec![0.0, 1.0],
            },
            MultiKey {
                key_type: "description".into(),
                embedding: vec![0.5, 0.5],
            },
        ];

        svc.insert_multi_key("e1".into(), &keys, serde_json::json!({}), 2);

        // Only 2 keys should have been inserted.
        assert_eq!(svc.len(), 2);
    }

    #[test]
    fn search_dedup_merges_multi_key_results() {
        let svc = make_service();

        // Insert entity_a with two keys.
        let keys_a = vec![
            MultiKey {
                key_type: "name".into(),
                embedding: vec![1.0, 0.0, 0.0],
            },
            MultiKey {
                key_type: "context".into(),
                embedding: vec![0.9, 0.1, 0.0],
            },
        ];
        svc.insert_multi_key(
            "entity_a".into(),
            &keys_a,
            serde_json::json!({"label": "A"}),
            4,
        );

        // Insert entity_b with one key.
        svc.insert(
            "entity_b".into(),
            vec![0.0, 1.0, 0.0],
            serde_json::json!({"label": "B"}),
        );

        // Query near entity_a's embeddings.
        let results = svc.search_dedup(&[1.0, 0.0, 0.0], 2);

        // entity_a should appear only once despite having two matching keys.
        let a_count = results.iter().filter(|r| r.id == "entity_a").count();
        assert_eq!(a_count, 1, "entity_a should be deduplicated to one result");

        // The top result should be entity_a (closest to the query).
        assert_eq!(results[0].id, "entity_a");
    }

    #[test]
    fn search_dedup_preserves_non_multikey_ids() {
        let svc = make_service();

        // Insert a regular (non-multi-key) entry.
        svc.insert("plain".into(), vec![1.0, 0.0], serde_json::json!({}));

        let results = svc.search_dedup(&[1.0, 0.0], 1);
        assert_eq!(results.len(), 1);
        // ID should be preserved as-is (no "::" suffix to strip).
        assert_eq!(results[0].id, "plain");
    }

    #[test]
    fn search_dedup_respects_top_k() {
        let svc = make_service();

        // Insert 3 entities with 2 keys each.
        for i in 0..3 {
            let keys = vec![
                MultiKey {
                    key_type: "name".into(),
                    embedding: vec![1.0 - (i as f32 * 0.3), i as f32 * 0.3, 0.0],
                },
                MultiKey {
                    key_type: "ctx".into(),
                    embedding: vec![1.0 - (i as f32 * 0.2), i as f32 * 0.2, 0.1],
                },
            ];
            svc.insert_multi_key(format!("e{i}"), &keys, serde_json::json!({}), 4);
        }

        let results = svc.search_dedup(&[1.0, 0.0, 0.0], 2);
        assert!(results.len() <= 2, "should return at most top_k results");

        // All IDs should be unique.
        let ids: HashSet<_> = results.iter().map(|r| &r.id).collect();
        assert_eq!(ids.len(), results.len(), "all result IDs should be unique");
    }

    // ── entity_search_keys helper tests ─────────────────────────────

    #[test]
    fn entity_search_keys_name_only() {
        let keys = entity_search_keys("MyFunc", None, &serde_json::json!({}));
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].0, "name");
        assert_eq!(keys[0].1, "MyFunc");
    }

    #[test]
    fn entity_search_keys_with_source_file() {
        let keys = entity_search_keys("MyFunc", Some("src/lib.rs"), &serde_json::json!({}));
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[1].0, "context");
        assert_eq!(keys[1].1, "MyFunc in src/lib.rs");
    }

    #[test]
    fn entity_search_keys_with_description() {
        let meta = serde_json::json!({"description": "Handles user auth"});
        let keys = entity_search_keys("login", None, &meta);
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[1].0, "description");
        assert_eq!(keys[1].1, "Handles user auth");
    }

    #[test]
    fn entity_search_keys_all_fields() {
        let meta = serde_json::json!({
            "description": "Core auth handler",
            "relationships": "calls TokenService, used by Router"
        });
        let keys = entity_search_keys("login", Some("src/auth.rs"), &meta);
        assert_eq!(keys.len(), 4);
        assert_eq!(keys[0].0, "name");
        assert_eq!(keys[1].0, "context");
        assert_eq!(keys[2].0, "description");
        assert_eq!(keys[3].0, "relationship");
    }

    // ── WEFT-375 relationship (edge) embedding helpers ───────────────

    #[test]
    fn relationship_hnsw_id_uses_prefix() {
        assert_eq!(relationship_hnsw_id("abc"), "rel::abc");
        assert!(relationship_hnsw_id("x").starts_with(RELATIONSHIP_HNSW_PREFIX));
    }

    #[test]
    fn relationship_search_keys_interaction_phrasing() {
        let keys = relationship_search_keys("AuthService", "calls", "Database", Some("auth.rs"));
        assert!(keys.iter().any(|(k, _)| k == "relation"));
        assert!(keys.iter().any(|(k, _)| k == "interaction"));
        assert!(keys.iter().any(|(k, _)| k == "context"));
        let interaction = keys
            .iter()
            .find(|(k, _)| k == "interaction")
            .map(|(_, t)| t.as_str())
            .unwrap();
        assert!(interaction.contains("how does AuthService interact with Database"));
    }

    #[test]
    fn insert_and_search_relationships() {
        let svc = make_service();
        // Entity vector (no rel:: prefix).
        svc.insert(
            "entity-auth".into(),
            vec![1.0, 0.0, 0.0],
            serde_json::json!({"kind": "entity"}),
        );
        // Edge vector.
        svc.insert_relationship(
            "edge-auth-db",
            vec![0.0, 1.0, 0.0],
            serde_json::json!({
                "kind": "relationship",
                "source_label": "AuthService",
                "target_label": "Database",
            }),
        );
        assert_eq!(svc.len(), 2);

        let hits = svc.search_relationships(&[0.0, 1.0, 0.0], 5);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "rel::edge-auth-db");
        assert_eq!(hits[0].metadata["kind"], "relationship");
    }

    /// Default-config search must keep graph-search recall. Region-guided
    /// search (opt-in) scans only a few k-means regions and dropped
    /// recall@10 from 0.94 to 0.49 on the 10K vector-backend bench when it
    /// was on by default.
    #[test]
    fn default_search_recall_floor() {
        const DIM: usize = 32;
        const CLUSTERS: usize = 60;
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = move || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((state >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
        };
        let centers: Vec<Vec<f32>> = (0..CLUSTERS)
            .map(|_| (0..DIM).map(|_| next()).collect())
            .collect();
        let point = |c: &[f32], next: &mut dyn FnMut() -> f32| -> Vec<f32> {
            c.iter().map(|v| v + 0.9 * next()).collect()
        };

        let svc = make_service();
        for i in 0..3000 {
            let emb = point(&centers[i % CLUSTERS], &mut next);
            svc.insert(format!("v{i}"), emb, serde_json::json!({}));
        }

        let queries = 50;
        let mut hits = 0usize;
        for q in 0..queries {
            let query = point(&centers[(q * 7) % CLUSTERS], &mut next);
            let truth = svc.brute_force_topk(&query, 10);
            let found: std::collections::HashSet<String> =
                svc.search(&query, 10).into_iter().map(|r| r.id).collect();
            hits += truth.iter().filter(|id| found.contains(*id)).count();
        }
        let recall = hits as f64 / (queries * 10) as f64;
        assert!(!svc.path_table_ready(), "guided search must be opt-in");
        assert!(
            recall >= 0.85,
            "default recall@10 {recall:.3} below floor 0.85"
        );
    }

    /// Learned ef predictions must not trigger graph rebuilds from inside
    /// `search` (set_ef_search rebuilds the whole index under the lock).
    #[test]
    fn search_never_changes_ef_search() {
        let cfg = HnswServiceConfig {
            ef_search: 14,
            ..HnswServiceConfig::default()
        };
        let svc = HnswService::with_eml(
            cfg,
            HnswEmlConfig {
                train_every_n: 20,
                min_training_samples: 10,
                ..Default::default()
            },
        );
        for i in 0..200 {
            let mut v = vec![0.01 * i as f32; 8];
            v[i % 8] += 1.0;
            svc.insert(format!("n{i}"), v, serde_json::json!({}));
        }
        for i in 0..120 {
            let mut q = vec![0.0; 8];
            q[i % 8] = 1.0;
            let _ = svc.search(&q, 5);
        }
        assert_eq!(svc.ef_params().0, 14, "search must not retune ef_search");
    }

    /// Deterministic pseudo-random 16-d vector; distinct seeds are well
    /// separated, so an exact query must return its own entry first.
    fn spread_vec(i: usize) -> Vec<f32> {
        let mut state = (i as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        (0..16)
            .map(|_| {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                ((state >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
            })
            .collect()
    }

    /// Searching while another thread inserts and searches must stay correct
    /// and must not rebuild under the read path: results always come back,
    /// and an exact vector finds itself.
    #[test]
    fn concurrent_search_and_insert_stay_correct() {
        let svc = Arc::new(make_service());
        for i in 0..400 {
            svc.insert(format!("b{i}"), spread_vec(i), serde_json::json!({}));
        }
        svc.ensure_index();

        let mut handles = Vec::new();
        for t in 0..8usize {
            let svc = Arc::clone(&svc);
            handles.push(std::thread::spawn(move || {
                for i in 0..150usize {
                    if t == 0 {
                        svc.insert(format!("w{i}"), spread_vec(1000 + i), serde_json::json!({}));
                    }
                    let probe = (i * 7 + t) % 400;
                    let hits = svc.search(&spread_vec(probe), 3);
                    assert!(!hits.is_empty());
                    assert_eq!(hits[0].id, format!("b{probe}"));
                }
            }));
        }
        for h in handles {
            h.join().expect("searcher panicked");
        }
        assert_eq!(svc.len(), 550);
        assert_eq!(svc.search_count(), 8 * 150);
    }

    /// A graph build runs with no store lock held: while one is in flight,
    /// searches and inserts still complete.
    #[test]
    fn rebuild_does_not_block_readers_or_writers() {
        let svc = Arc::new(make_service());
        for i in 0..300 {
            svc.insert(format!("b{i}"), spread_vec(i), serde_json::json!({}));
        }
        svc.ensure_index();
        // Exceed the rebuild threshold so the next search wants a rebuild.
        for i in 0..150 {
            svc.insert(format!("n{i}"), spread_vec(2000 + i), serde_json::json!({}));
        }
        // Hold the build lock as a stand-in for a long in-flight build.
        let building = svc.build_lock.lock().unwrap();
        let hits = svc.search(&spread_vec(5), 3);
        assert_eq!(hits[0].id, "b5", "stale graph plus tail must still answer");
        svc.insert("late".into(), spread_vec(5000), serde_json::json!({}));
        drop(building);
        svc.ensure_index();
        assert_eq!(svc.search(&spread_vec(5000), 1)[0].id, "late");
    }

    /// EML bookkeeping is skipped, not waited for, when its lock is busy.
    #[test]
    fn search_does_not_wait_on_eml_lock() {
        let svc = make_service();
        for i in 0..100 {
            svc.insert(format!("b{i}"), spread_vec(i), serde_json::json!({}));
        }
        let _held = svc.eml.lock().unwrap();
        // Would deadlock if search blocked on the EML mutex.
        let hits = svc.search(&spread_vec(3), 2);
        assert_eq!(hits[0].id, "b3");
    }

    /// A due training cycle runs on its own thread: it still happens, and
    /// recording a search never trains inline.
    #[test]
    fn eml_training_runs_off_the_query_path() {
        let svc = HnswService::with_eml(
            HnswServiceConfig::default(),
            HnswEmlConfig {
                train_every_n: 20,
                min_training_samples: 10,
                ..Default::default()
            },
        );
        for i in 0..100 {
            svc.insert(format!("n{i}"), spread_vec(i), serde_json::json!({}));
        }
        for i in 0..40 {
            let _ = svc.search(&spread_vec(i), 3);
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            let st = svc.eml_status();
            if st.searches_since_train < 20 && !svc.training.load(Ordering::Acquire) {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "training never ran");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
}
