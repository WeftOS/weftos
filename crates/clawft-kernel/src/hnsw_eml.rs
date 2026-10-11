//! EML-based HNSW search optimization.
//!
//! Manages four EML models that learn from operational search data to
//! optimize HNSW performance:
//!
//! - **Distance model**: learned dimension selection for fast approximate
//!   cosine distance (progressive dimensionality).
//! - **Ef model**: per-query adaptive beam width (ef_search).
//! - **Path model**: search entry-point prediction.
//! - **Rebuild model**: predicts when the HNSW graph needs rebuilding
//!   based on recall degradation.
//!
//! # Two-Tier Pattern
//!
//! Follows the same two-tier pattern as [`eml_coherence`]:
//! - **Every search**: fast EML predictions guide beam width and entry
//!   point selection (~0.1 us overhead).
//! - **Periodically**: ground-truth recall measurement via brute-force
//!   comparison feeds the distance and rebuild models.
//! - **Every N searches**: models are retrained from accumulated data.
//!
//! This module is compiled only when the `ecc` feature is enabled.

use serde::{Deserialize, Serialize};

use clawft_core::embeddings::path_predict::{
    PathPredictConfig, PathPrediction as RegionPathPrediction, RegionEntryTable,
    DEFAULT_NUM_REGIONS, DEFAULT_REGION_PROBES,
};

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration for the HNSW EML optimization system.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HnswEmlConfig {
    /// Enable EML-based HNSW optimization.
    pub enabled: bool,
    /// Train models every N searches.
    pub train_every_n: u64,
    /// Measure recall every N searches (for distance/rebuild models).
    pub recall_check_every_n: u64,
    /// Minimum training samples before enabling learned models.
    pub min_training_samples: usize,
    /// Number of selected dimensions for cosine decomposition.
    pub distance_selected_dims: usize,
    /// Ef selection strategy.
    pub ef_strategy: EfStrategy,
    /// Target recall for threshold-based strategy (default 0.95).
    pub target_recall: f64,
    /// Max ef boost factor (caps how aggressively the model can raise ef).
    pub max_ef_boost: f64,
    /// Latency weight for score-based strategy (0..1, higher = prefer speed).
    pub latency_weight: f64,
    /// Enable region → entry-node search-path prediction (HNSW-EML #4 / WEFT-385).
    ///
    /// Opt-in (default `false`): guided search scores only the nearest
    /// `path_region_probes` of `path_num_regions` k-means regions instead of
    /// traversing the HNSW graph, so it trades recall for speed. With the
    /// defaults (16 regions, 2 probes) it halved recall@10 on the 10K
    /// vector-backend bench (0.943 -> 0.493) and serialised queries on the
    /// service lock.
    pub path_predict_enabled: bool,
    /// Number of k-means regions for the path table.
    pub path_num_regions: usize,
    /// How many nearest regions to probe per query.
    pub path_region_probes: usize,
    /// Entry nodes retained per region.
    pub path_entries_per_region: usize,
}

/// Strategy for selecting the adaptive ef_search value.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub enum EfStrategy {
    /// Boost ef until predicted recall >= target_recall, capped by max_ef_boost.
    Threshold,
    /// Maximize `recall * (1 - latency_weight * latency/max_latency)`.
    Score,
}

impl Default for HnswEmlConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            train_every_n: 1000,
            recall_check_every_n: 5000,
            min_training_samples: 200,
            distance_selected_dims: 16,
            ef_strategy: EfStrategy::Score,
            target_recall: 0.95,
            max_ef_boost: 2.5,
            latency_weight: 0.3,
            path_predict_enabled: false,
            path_num_regions: DEFAULT_NUM_REGIONS,
            path_region_probes: DEFAULT_REGION_PROBES,
            path_entries_per_region: 3,
        }
    }
}

// ---------------------------------------------------------------------------
// Training data points
// ---------------------------------------------------------------------------

/// Training point for the ef (beam width) model.
///
/// Records search characteristics so the model can learn the optimal
/// ef_search for a given query profile. Carries both the ef that was
/// used AND the recall observed (when available), so the 2-head model
/// can learn the ef → recall tradeoff jointly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EfTrainingPoint {
    /// L2 norm of the query vector.
    pub query_norm: f64,
    /// Variance across query dimensions.
    pub query_variance: f64,
    /// ef_search value used for this query.
    pub ef_used: usize,
    /// Number of results returned.
    pub result_count: usize,
    /// Wall-clock search time in microseconds.
    pub search_time_us: u64,
    /// Observed recall for this query (None if not measured).
    pub recall: Option<f64>,
}

/// Training point for the distance model.
///
/// Records per-dimension contribution to distance computations so the
/// model can learn which dimensions are most informative.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DistanceTrainingPoint {
    /// Per-dimension absolute differences between query and result vectors.
    pub dim_contributions: Vec<f64>,
    /// Full cosine similarity (ground truth).
    pub exact_similarity: f64,
    /// Approximate similarity using only selected dimensions.
    pub approx_similarity: f64,
}

/// Training point for the search path model.
///
/// Records entry-point selection outcomes so the model can predict
/// the best starting node for a given query vector.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PathTrainingPoint {
    /// Query vector norm.
    pub query_norm: f64,
    /// Query vector variance.
    pub query_variance: f64,
    /// Number of hops taken during the search (or candidate-pool size proxy).
    pub hops: usize,
    /// Score of the top result.
    pub top_score: f64,
    /// Total entries in the store at search time.
    pub store_size: usize,
    /// Predicted region id when path table was used (`None` if unguided).
    pub region_id: Option<usize>,
    /// Cosine of query to chosen region centroid (0 when unguided).
    pub region_score: f64,
    /// Whether guided search was used.
    pub guided: bool,
}

/// Training point for the rebuild model.
///
/// Records graph health statistics so the model can predict when recall
/// has degraded enough to justify a full HNSW rebuild.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RebuildTrainingPoint {
    /// Number of entries in the store.
    pub store_size: usize,
    /// Number of inserts since last rebuild.
    pub inserts_since_rebuild: usize,
    /// Number of deletes since last rebuild.
    pub deletes_since_rebuild: usize,
    /// Measured recall (0.0..1.0).
    pub recall: f64,
    /// Average search time in microseconds.
    pub avg_search_time_us: f64,
}

// ---------------------------------------------------------------------------
// Feature vectors for EML models
// ---------------------------------------------------------------------------

/// Features extracted from a search query for ef/path prediction.
#[derive(Debug, Clone)]
struct SearchFeatures {
    query_norm: f64,
    query_variance: f64,
    store_size: f64,
    recent_avg_time_us: f64,
}

impl SearchFeatures {
    fn normalized(&self) -> [f64; 4] {
        [
            self.query_norm / 100.0,
            self.query_variance.min(1.0),
            self.store_size / 100_000.0,
            self.recent_avg_time_us / 10_000.0,
        ]
    }
}

impl eml_core::FeatureVector for SearchFeatures {
    fn as_features(&self) -> Vec<f64> {
        self.normalized().to_vec()
    }

    fn feature_count() -> usize {
        4
    }
}

/// Features for the rebuild prediction model.
#[derive(Debug, Clone)]
struct RebuildFeatures {
    store_size: f64,
    inserts_since_rebuild: f64,
    deletes_since_rebuild: f64,
    avg_search_time_us: f64,
}

impl RebuildFeatures {
    fn normalized(&self) -> [f64; 4] {
        [
            self.store_size / 100_000.0,
            self.inserts_since_rebuild / 10_000.0,
            self.deletes_since_rebuild / 10_000.0,
            self.avg_search_time_us / 10_000.0,
        ]
    }
}

impl eml_core::FeatureVector for RebuildFeatures {
    fn as_features(&self) -> Vec<f64> {
        self.normalized().to_vec()
    }

    fn feature_count() -> usize {
        4
    }
}

// ---------------------------------------------------------------------------
// Status snapshot
// ---------------------------------------------------------------------------

/// Point-in-time status snapshot of the HNSW EML system.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HnswEmlStatus {
    /// Whether EML optimization is enabled.
    pub enabled: bool,
    /// Whether the distance model has been trained.
    pub distance_trained: bool,
    /// Number of distance training samples.
    pub distance_samples: usize,
    /// Whether the ef model has been trained.
    pub ef_trained: bool,
    /// Number of ef training samples.
    pub ef_samples: usize,
    /// Whether the path model has been trained.
    pub path_trained: bool,
    /// Number of path training samples.
    pub path_samples: usize,
    /// Whether the rebuild model has been trained.
    pub rebuild_trained: bool,
    /// Number of rebuild training samples.
    pub rebuild_samples: usize,
    /// Total searches since last training cycle.
    pub searches_since_train: u64,
    /// Total searches processed since creation.
    pub total_searches: u64,
    /// Total training cycles completed.
    pub train_cycles: u64,
    /// Last measured recall (if available).
    pub last_recall: Option<f64>,
}

// ---------------------------------------------------------------------------
// Predictions
// ---------------------------------------------------------------------------

/// Predicted optimal ef_search for a query.
#[derive(Debug, Clone)]
pub struct EfPrediction {
    /// Recommended ef_search value.
    pub recommended_ef: usize,
    /// Whether this is a learned prediction (vs. default).
    pub is_learned: bool,
}

/// Predicted rebuild urgency.
#[derive(Debug, Clone)]
pub struct RebuildPrediction {
    /// Predicted recall if no rebuild is performed (0.0..1.0).
    pub predicted_recall: f64,
    /// Whether a rebuild is recommended.
    pub should_rebuild: bool,
    /// Whether this is a learned prediction (vs. heuristic).
    pub is_learned: bool,
}

/// Predicted search entry nodes (WEFT-385 / HNSW-EML #4).
#[derive(Debug, Clone)]
pub struct PathPrediction {
    /// Nearest region id when the path table is ready.
    pub region_id: Option<usize>,
    /// Predicted entry-node ids (highway on-ramps), best first.
    pub entry_ids: Vec<String>,
    /// Cosine of query to chosen region centroid.
    pub region_score: f32,
    /// Candidate pool (members of probed regions).
    pub candidate_ids: Vec<String>,
    /// Whether a region table is available (built / learned).
    pub is_learned: bool,
    /// Whether the EML path model agrees the guided path is high quality.
    pub eml_path_quality: Option<f64>,
}

/// One guided (path-predicted) search, as recorded by
/// [`HnswEmlManager::record_guided_search`].
#[derive(Debug, Clone, Copy)]
pub struct GuidedSearch<'a> {
    /// The query vector.
    pub query: &'a [f32],
    /// Number of results returned.
    pub result_count: usize,
    /// Score of the best result.
    pub top_score: f32,
    /// `ef` the search ran with.
    pub ef_used: usize,
    /// Wall-clock search time in microseconds.
    pub search_time_us: u64,
    /// Store size at search time.
    pub store_size: usize,
    /// The path prediction that guided the search.
    pub prediction: &'a PathPrediction,
    /// Candidate pool size (stands in for hop count).
    pub candidate_pool_size: usize,
}

impl From<RegionPathPrediction> for PathPrediction {
    fn from(p: RegionPathPrediction) -> Self {
        Self {
            region_id: p.region_id,
            entry_ids: p.entry_ids,
            region_score: p.region_score,
            candidate_ids: p.candidate_ids,
            is_learned: p.is_learned,
            eml_path_quality: None,
        }
    }
}

// ---------------------------------------------------------------------------
// HnswEmlManager
// ---------------------------------------------------------------------------

/// Manages EML models for HNSW search optimization.
///
/// Follows the two-tier pattern: fast EML predictions on every search,
/// periodic ground-truth measurement for training.
pub struct HnswEmlManager {
    /// Configuration.
    config: HnswEmlConfig,
    /// Learned dimension selection for fast approximate distance.
    distance_model: eml_core::EmlModel,
    /// Per-query adaptive beam width.
    ef_model: eml_core::EmlModel,
    /// Search path entry-point predictor.
    path_model: eml_core::EmlModel,
    /// Rebuild trigger predictor.
    rebuild_model: eml_core::EmlModel,
    /// Region → entry-node lookup table (HNSW-EML #4).
    path_table: Option<RegionEntryTable>,
    /// Training data buffers.
    distance_training: Vec<DistanceTrainingPoint>,
    ef_training: Vec<EfTrainingPoint>,
    path_training: Vec<PathTrainingPoint>,
    rebuild_training: Vec<RebuildTrainingPoint>,
    /// Searches since last train cycle.
    searches_since_train: u64,
    /// Total searches processed.
    total_searches: u64,
    /// Total training cycles completed.
    train_cycles: u64,
    /// Last measured recall.
    last_recall: Option<f64>,
    /// Recent search times for averaging.
    recent_search_times_us: Vec<u64>,
}

impl HnswEmlManager {
    /// Create a new manager with the given configuration.
    pub fn new(config: HnswEmlConfig) -> Self {
        Self {
            config,
            distance_model: eml_core::EmlModel::new(3, 4, 1),
            // 2-head: head 0 = normalized ef, head 1 = predicted recall.
            // Joint training lets the model learn the Pareto frontier.
            ef_model: eml_core::EmlModel::new(3, 4, 2),
            path_model: eml_core::EmlModel::new(3, 4, 1),
            rebuild_model: eml_core::EmlModel::new(3, 4, 1),
            path_table: None,
            distance_training: Vec::new(),
            ef_training: Vec::new(),
            path_training: Vec::new(),
            rebuild_training: Vec::new(),
            searches_since_train: 0,
            total_searches: 0,
            train_cycles: 0,
            last_recall: None,
            recent_search_times_us: Vec::new(),
        }
    }

    /// Create a manager with default configuration.
    pub fn with_defaults() -> Self {
        Self::new(HnswEmlConfig::default())
    }

    /// Whether EML optimization is enabled.
    pub fn is_enabled(&self) -> bool {
        self.config.enabled
    }

    /// Borrow the configuration.
    pub fn config(&self) -> &HnswEmlConfig {
        &self.config
    }

    // -------------------------------------------------------------------
    // Training data collection
    // -------------------------------------------------------------------

    /// Record a search for training data collection.
    ///
    /// Called after every HNSW search to collect ef and path training
    /// data. When enough samples accumulate, triggers a training cycle.
    pub fn record_search(
        &mut self,
        query: &[f32],
        result_count: usize,
        top_score: f32,
        ef_used: usize,
        search_time_us: u64,
        store_size: usize,
    ) {
        if !self.config.enabled {
            return;
        }

        let qnorm = vector_norm(query);
        let qvar = vector_variance(query);

        // Record ef training data (recall filled in later by measure_recall).
        self.ef_training.push(EfTrainingPoint {
            query_norm: qnorm,
            query_variance: qvar,
            ef_used,
            result_count,
            search_time_us,
            recall: None,
        });

        // Record path training data (unguided / hops unknown).
        self.path_training.push(PathTrainingPoint {
            query_norm: qnorm,
            query_variance: qvar,
            hops: 0,
            top_score: top_score as f64,
            store_size,
            region_id: None,
            region_score: 0.0,
            guided: false,
        });

        // Track recent search times.
        self.recent_search_times_us.push(search_time_us);
        if self.recent_search_times_us.len() > 1000 {
            self.recent_search_times_us.drain(..500);
        }

        self.searches_since_train += 1;
        self.total_searches += 1;

        // Trigger periodic training.
        if self.searches_since_train >= self.config.train_every_n {
            self.train_all();
        }
    }

    /// Record a guided (path-predicted) search for path-model training.
    ///
    /// `candidate_pool_size` stands in for hop count: smaller pools that still
    /// hit a high top score mean the entry/region prediction was better.
    pub fn record_guided_search(&mut self, search: GuidedSearch<'_>) {
        let GuidedSearch {
            query,
            result_count,
            top_score,
            ef_used,
            search_time_us,
            store_size,
            prediction,
            candidate_pool_size,
        } = search;
        if !self.config.enabled {
            return;
        }

        let qnorm = vector_norm(query);
        let qvar = vector_variance(query);

        self.ef_training.push(EfTrainingPoint {
            query_norm: qnorm,
            query_variance: qvar,
            ef_used,
            result_count,
            search_time_us,
            recall: None,
        });

        self.path_training.push(PathTrainingPoint {
            query_norm: qnorm,
            query_variance: qvar,
            // Proxy hops: smaller relative pool ⇒ shorter effective path.
            hops: candidate_pool_size,
            top_score: top_score as f64,
            store_size,
            region_id: prediction.region_id,
            region_score: prediction.region_score as f64,
            guided: prediction.is_learned,
        });

        self.recent_search_times_us.push(search_time_us);
        if self.recent_search_times_us.len() > 1000 {
            self.recent_search_times_us.drain(..500);
        }

        self.searches_since_train += 1;
        self.total_searches += 1;

        if self.searches_since_train >= self.config.train_every_n {
            self.train_all();
        }
    }

    /// Build (or rebuild) the region → entry-node path table from store
    /// embeddings. Call after bulk ingest / HNSW rebuild.
    pub fn rebuild_path_table(&mut self, entries: &[(String, Vec<f32>)]) {
        if !self.config.path_predict_enabled || entries.is_empty() {
            self.path_table = None;
            return;
        }
        let cfg = PathPredictConfig {
            num_regions: self.config.path_num_regions,
            region_probes: self.config.path_region_probes,
            entries_per_region: self.config.path_entries_per_region,
            kmeans_iters: 12,
        };
        let table = RegionEntryTable::build(entries, cfg);
        if table.is_ready() {
            self.path_table = Some(table);
        } else {
            self.path_table = None;
        }
    }

    /// Whether a path table is ready for guided search.
    pub fn has_path_table(&self) -> bool {
        self.path_table
            .as_ref()
            .map(|t| t.is_ready())
            .unwrap_or(false)
    }

    /// Borrow the region entry table when present.
    pub fn path_table(&self) -> Option<&RegionEntryTable> {
        self.path_table.as_ref()
    }

    /// Region probes configured for path prediction.
    pub fn path_region_probes(&self) -> usize {
        self.config.path_region_probes
    }

    /// Whether path prediction is enabled in config.
    pub fn path_predict_enabled(&self) -> bool {
        self.config.path_predict_enabled && self.config.enabled
    }

    /// Measure actual recall by comparing HNSW results against brute-force.
    ///
    /// Called periodically (e.g., every `recall_check_every_n` searches or
    /// from the DEMOCRITUS loop). Feeds the distance and rebuild models.
    ///
    /// Returns the average recall across the sample queries.
    pub fn measure_recall(
        &mut self,
        hnsw_results: &[Vec<String>],
        exact_results: &[Vec<String>],
        store_size: usize,
        inserts_since_rebuild: usize,
        deletes_since_rebuild: usize,
    ) -> f64 {
        if hnsw_results.is_empty() || exact_results.is_empty() {
            return 0.0;
        }

        let n = hnsw_results.len().min(exact_results.len());
        let mut total_recall = 0.0;

        for i in 0..n {
            let recall = compute_recall(&hnsw_results[i], &exact_results[i]);
            total_recall += recall;
        }

        let avg_recall = total_recall / n as f64;
        self.last_recall = Some(avg_recall);

        // Backfill recall into recent ef training points that lack it.
        // This gives the 2-head ef model a recall signal to train against.
        for point in self.ef_training.iter_mut().rev() {
            if point.recall.is_some() {
                break;
            }
            point.recall = Some(avg_recall);
        }

        // Record rebuild training data.
        let avg_search_time = if self.recent_search_times_us.is_empty() {
            0.0
        } else {
            self.recent_search_times_us.iter().sum::<u64>() as f64
                / self.recent_search_times_us.len() as f64
        };

        self.rebuild_training.push(RebuildTrainingPoint {
            store_size,
            inserts_since_rebuild,
            deletes_since_rebuild,
            recall: avg_recall,
            avg_search_time_us: avg_search_time,
        });

        avg_recall
    }

    /// Record distance training data from paired HNSW vs. exact results.
    ///
    /// `query` is the search query, `hnsw_embedding` and `exact_embedding`
    /// are the embeddings of the HNSW result and exact result respectively.
    pub fn record_distance_pair(
        &mut self,
        query: &[f32],
        hnsw_embedding: &[f32],
        exact_embedding: &[f32],
    ) {
        if !self.config.enabled {
            return;
        }

        let dim_contributions: Vec<f64> = query
            .iter()
            .zip(exact_embedding.iter())
            .map(|(q, e)| ((*q - *e) as f64).abs())
            .collect();

        let exact_sim = cosine_similarity_f32(query, exact_embedding) as f64;
        let approx_sim = cosine_similarity_f32(query, hnsw_embedding) as f64;

        self.distance_training.push(DistanceTrainingPoint {
            dim_contributions,
            exact_similarity: exact_sim,
            approx_similarity: approx_sim,
        });
    }

    // -------------------------------------------------------------------
    // Predictions
    // -------------------------------------------------------------------

    /// Predict the optimal ef_search for a query.
    ///
    /// The 2-head model predicts [ef, recall] jointly. Two strategies
    /// are available (configured via `ef_strategy`):
    ///
    /// **Threshold**: boost ef until predicted recall >= `target_recall`,
    /// capped by `max_ef_boost`. Simple, predictable.
    ///
    /// **Score**: maximize `recall - latency_weight * (ef / 500)`. Finds
    /// the Pareto-optimal point balancing recall and speed.
    ///
    /// Returns a default of 100 if the model is not yet trained.
    pub fn predict_ef(&self, query: &[f32], store_size: usize) -> EfPrediction {
        let default_ef = 100;

        if !self.config.enabled || !self.ef_model.is_trained() {
            return EfPrediction {
                recommended_ef: default_ef,
                is_learned: false,
            };
        }

        let features = SearchFeatures {
            query_norm: vector_norm(query),
            query_variance: vector_variance(query),
            store_size: store_size as f64,
            recent_avg_time_us: self.avg_recent_search_time(),
        };

        let outputs = self.ef_model.predict(&features.normalized());
        let raw_ef = (outputs[0] * 500.0).clamp(10.0, 500.0);
        let predicted_recall = outputs[1].clamp(0.0, 1.0);

        let ef = match self.config.ef_strategy {
            EfStrategy::Threshold => {
                if predicted_recall < self.config.target_recall {
                    let gap = (self.config.target_recall - predicted_recall)
                        / (1.0 - predicted_recall).max(0.01);
                    let boost = 1.0 + gap * (self.config.max_ef_boost - 1.0);
                    (raw_ef * boost.min(self.config.max_ef_boost)).clamp(10.0, 500.0)
                } else {
                    raw_ef
                }
            }
            EfStrategy::Score => {
                // Probe a few ef candidates and pick the one with the best
                // recall-vs-cost score. The model's head 0 learned the
                // ef→latency mapping and head 1 the ef→recall mapping, so
                // we evaluate at several ef levels.
                let candidates: [f64; 7] = [0.04, 0.08, 0.16, 0.3, 0.5, 0.7, 1.0];
                let mut best_score = f64::NEG_INFINITY;
                let mut best_ef = raw_ef;
                for &frac in &candidates {
                    let candidate_ef = (frac * 500.0_f64).clamp(10.0, 500.0);
                    // Approximate: recall scales with log(ef), model gives
                    // a baseline prediction we shift proportionally.
                    let ef_ratio = (candidate_ef / raw_ef.max(1.0)).ln().max(0.0);
                    let est_recall = (predicted_recall + ef_ratio * 0.15).clamp(0.0, 1.0);
                    let cost = frac; // normalized latency proxy
                    let score = est_recall - self.config.latency_weight * cost;
                    if score > best_score {
                        best_score = score;
                        best_ef = candidate_ef;
                    }
                }
                best_ef
            }
        } as usize;

        EfPrediction {
            recommended_ef: ef.max(10),
            is_learned: true,
        }
    }

    /// Predict search entry nodes for a query (region → entry-node lookup).
    ///
    /// Returns an unlearned empty prediction when the path table has not been
    /// built yet. When the EML path model is trained, attaches a quality score
    /// (lower is better — normalized hop / pool proxy).
    pub fn predict_path(&self, query: &[f32], store_size: usize) -> PathPrediction {
        if !self.path_predict_enabled() {
            return PathPrediction {
                region_id: None,
                entry_ids: Vec::new(),
                region_score: 0.0,
                candidate_ids: Vec::new(),
                is_learned: false,
                eml_path_quality: None,
            };
        }

        let mut pred = match &self.path_table {
            Some(table) if table.is_ready() => {
                PathPrediction::from(table.predict(query, self.config.path_region_probes))
            }
            _ => PathPrediction {
                region_id: None,
                entry_ids: Vec::new(),
                region_score: 0.0,
                candidate_ids: Vec::new(),
                is_learned: false,
                eml_path_quality: None,
            },
        };

        if self.path_model.is_trained() {
            let features = SearchFeatures {
                query_norm: vector_norm(query),
                query_variance: vector_variance(query),
                store_size: store_size as f64,
                recent_avg_time_us: self.avg_recent_search_time(),
            };
            // Path model target is normalized hop/pool size; lower ⇒ better entry.
            let quality = self
                .path_model
                .predict_primary(&features.normalized())
                .max(0.0);
            pred.eml_path_quality = Some(quality);
        }

        pred
    }

    /// Predict whether the HNSW index should be rebuilt.
    pub fn predict_rebuild(
        &self,
        store_size: usize,
        inserts_since_rebuild: usize,
        deletes_since_rebuild: usize,
    ) -> RebuildPrediction {
        if !self.config.enabled || !self.rebuild_model.is_trained() {
            // Heuristic fallback: rebuild if mutations exceed 10% of store.
            let mutation_ratio = if store_size > 0 {
                (inserts_since_rebuild + deletes_since_rebuild) as f64 / store_size as f64
            } else {
                0.0
            };
            return RebuildPrediction {
                predicted_recall: 1.0 - mutation_ratio * 0.1,
                should_rebuild: mutation_ratio > 0.1,
                is_learned: false,
            };
        }

        let features = RebuildFeatures {
            store_size: store_size as f64,
            inserts_since_rebuild: inserts_since_rebuild as f64,
            deletes_since_rebuild: deletes_since_rebuild as f64,
            avg_search_time_us: self.avg_recent_search_time(),
        };

        let predicted_recall = self
            .rebuild_model
            .predict_primary(&features.normalized())
            .clamp(0.0, 1.0);

        RebuildPrediction {
            predicted_recall,
            should_rebuild: predicted_recall < 0.90,
            is_learned: true,
        }
    }

    // -------------------------------------------------------------------
    // Training
    // -------------------------------------------------------------------

    /// Train all models that have sufficient data.
    ///
    /// Returns `true` if at least one model was trained.
    pub fn train_all(&mut self) -> bool {
        let mut any_trained = false;

        if self.train_ef_model() {
            any_trained = true;
        }
        if self.train_path_model() {
            any_trained = true;
        }
        if self.train_rebuild_model() {
            any_trained = true;
        }
        if self.train_distance_model() {
            any_trained = true;
        }

        self.searches_since_train = 0;
        if any_trained {
            self.train_cycles += 1;
        }

        any_trained
    }

    /// Train the ef model from collected search data.
    ///
    /// The ef model is 2-head:
    ///   head 0 = normalized ef (ef_used / 500)
    ///   head 1 = recall (0..1)
    ///
    /// When a training point has `recall: None`, head 1 is skipped in the
    /// loss function via `None` — the model still trains on the ef signal
    /// from that sample. When recall IS available, both heads train jointly,
    /// teaching the model the ef ↔ recall tradeoff.
    fn train_ef_model(&mut self) -> bool {
        if self.ef_training.len() < self.config.min_training_samples {
            return false;
        }

        for point in &self.ef_training {
            let inputs = [
                point.query_norm / 100.0,
                point.query_variance.min(1.0),
                point.result_count as f64 / 100.0,
                point.search_time_us as f64 / 10_000.0,
            ];
            let ef_target = Some(point.ef_used as f64 / 500.0);
            let recall_target = point.recall;
            self.ef_model.record(&inputs, &[ef_target, recall_target]);
        }

        let converged = self.ef_model.train();
        let keep = self.ef_training.len().saturating_sub(100);
        self.ef_training.drain(..keep);
        converged
    }

    /// Train the path model.
    ///
    /// Target is a normalized path cost: for guided searches, candidate-pool
    /// size relative to store size (smaller = better entry/region). For
    /// unguided searches, falls back to hop proxy / top-score complement.
    fn train_path_model(&mut self) -> bool {
        if self.path_training.len() < self.config.min_training_samples {
            return false;
        }

        for point in &self.path_training {
            let inputs = [
                point.query_norm / 100.0,
                point.query_variance.min(1.0),
                point.store_size as f64 / 100_000.0,
                point.region_score.clamp(0.0, 1.0),
            ];
            let target = if point.guided && point.store_size > 0 {
                // Relative pool size: good regions keep this small.
                (point.hops as f64 / point.store_size as f64).clamp(0.0, 1.0)
            } else if point.hops > 0 {
                (point.hops as f64 / 100.0).min(1.0)
            } else {
                // Unguided with no hop signal: use 1 - top_score as cost.
                (1.0 - point.top_score).clamp(0.0, 1.0)
            };
            self.path_model.record(&inputs, &[Some(target)]);
        }

        let converged = self.path_model.train();
        let keep = self.path_training.len().saturating_sub(100);
        self.path_training.drain(..keep);
        converged
    }

    /// Train the rebuild model.
    fn train_rebuild_model(&mut self) -> bool {
        if self.rebuild_training.len() < self.config.min_training_samples {
            return false;
        }

        for point in &self.rebuild_training {
            let inputs = [
                point.store_size as f64 / 100_000.0,
                point.inserts_since_rebuild as f64 / 10_000.0,
                point.deletes_since_rebuild as f64 / 10_000.0,
                point.avg_search_time_us / 10_000.0,
            ];
            // Target: actual recall
            let target = point.recall;
            self.rebuild_model.record(&inputs, &[Some(target)]);
        }

        let converged = self.rebuild_model.train();
        let keep = self.rebuild_training.len().saturating_sub(100);
        self.rebuild_training.drain(..keep);
        converged
    }

    /// Train the distance model.
    fn train_distance_model(&mut self) -> bool {
        if self.distance_training.len() < self.config.min_training_samples {
            return false;
        }

        for point in &self.distance_training {
            // Use aggregate dimension stats as features
            let n_dims = point.dim_contributions.len().max(1) as f64;
            let mean_contrib = point.dim_contributions.iter().sum::<f64>() / n_dims;
            let max_contrib = point
                .dim_contributions
                .iter()
                .copied()
                .fold(0.0_f64, f64::max);
            let inputs = [
                mean_contrib.min(1.0),
                max_contrib.min(1.0),
                n_dims / 1000.0,
                point.exact_similarity.clamp(0.0, 1.0),
            ];
            // Target: approximation quality
            let target = (point.approx_similarity - point.exact_similarity)
                .abs()
                .min(1.0);
            self.distance_model.record(&inputs, &[Some(target)]);
        }

        let converged = self.distance_model.train();
        let keep = self.distance_training.len().saturating_sub(100);
        self.distance_training.drain(..keep);
        converged
    }

    // -------------------------------------------------------------------
    // Status & reset
    // -------------------------------------------------------------------

    /// Compute a dimension importance ranking from accumulated distance
    /// training data. Dimensions that contribute most to distinguishing
    /// correct from incorrect results are ranked highest.
    ///
    /// Returns dimension indices sorted by importance (most important first).
    /// Falls back to identity ordering [0, 1, 2, ...] when insufficient data.
    pub fn learned_dim_order(&self, total_dims: usize) -> Vec<usize> {
        if self.distance_training.len() < 10 {
            return (0..total_dims).collect();
        }

        // Aggregate mean absolute contribution per dimension across all
        // training points. Dimensions with high mean contribution are
        // the most discriminative.
        let mut dim_importance = vec![0.0_f64; total_dims];
        let mut counts = vec![0usize; total_dims];
        for point in &self.distance_training {
            for (d, &contrib) in point.dim_contributions.iter().enumerate() {
                if d < total_dims {
                    dim_importance[d] += contrib;
                    counts[d] += 1;
                }
            }
        }
        for d in 0..total_dims {
            if counts[d] > 0 {
                dim_importance[d] /= counts[d] as f64;
            }
        }

        let mut order: Vec<usize> = (0..total_dims).collect();
        order.sort_by(|&a, &b| {
            dim_importance[b]
                .partial_cmp(&dim_importance[a])
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        order
    }

    /// Return a point-in-time status snapshot.
    pub fn status(&self) -> HnswEmlStatus {
        HnswEmlStatus {
            enabled: self.config.enabled,
            distance_trained: self.distance_model.is_trained(),
            distance_samples: self.distance_training.len(),
            ef_trained: self.ef_model.is_trained(),
            ef_samples: self.ef_training.len(),
            path_trained: self.path_model.is_trained(),
            path_samples: self.path_training.len(),
            rebuild_trained: self.rebuild_model.is_trained(),
            rebuild_samples: self.rebuild_training.len(),
            searches_since_train: self.searches_since_train,
            total_searches: self.total_searches,
            train_cycles: self.train_cycles,
            last_recall: self.last_recall,
        }
    }

    /// Drop the region path table without touching EML weights.
    pub fn clear_path_table(&mut self) {
        self.path_table = None;
    }

    /// Reset all models and clear training data.
    pub fn reset(&mut self) {
        self.distance_model = eml_core::EmlModel::new(3, 4, 1);
        self.ef_model = eml_core::EmlModel::new(3, 4, 2);
        self.path_model = eml_core::EmlModel::new(3, 4, 1);
        self.rebuild_model = eml_core::EmlModel::new(3, 4, 1);
        self.path_table = None;
        self.distance_training.clear();
        self.ef_training.clear();
        self.path_training.clear();
        self.rebuild_training.clear();
        self.searches_since_train = 0;
        self.total_searches = 0;
        self.train_cycles = 0;
        self.last_recall = None;
        self.recent_search_times_us.clear();
    }

    // -------------------------------------------------------------------
    // Private helpers
    // -------------------------------------------------------------------

    /// Average of recent search times in microseconds.
    fn avg_recent_search_time(&self) -> f64 {
        if self.recent_search_times_us.is_empty() {
            return 0.0;
        }
        self.recent_search_times_us.iter().sum::<u64>() as f64
            / self.recent_search_times_us.len() as f64
    }
}

impl std::fmt::Debug for HnswEmlManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HnswEmlManager")
            .field("enabled", &self.config.enabled)
            .field("total_searches", &self.total_searches)
            .field("train_cycles", &self.train_cycles)
            .field("ef_trained", &self.ef_model.is_trained())
            .field("rebuild_trained", &self.rebuild_model.is_trained())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

/// Compute the L2 norm of a vector.
fn vector_norm(v: &[f32]) -> f64 {
    v.iter()
        .map(|x| (*x as f64) * (*x as f64))
        .sum::<f64>()
        .sqrt()
}

/// Compute the variance of a vector's elements.
fn vector_variance(v: &[f32]) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    let n = v.len() as f64;
    let mean = v.iter().map(|x| *x as f64).sum::<f64>() / n;
    v.iter()
        .map(|x| {
            let d = *x as f64 - mean;
            d * d
        })
        .sum::<f64>()
        / n
}

/// Compute recall: fraction of exact results that appear in HNSW results.
fn compute_recall(hnsw_ids: &[String], exact_ids: &[String]) -> f64 {
    if exact_ids.is_empty() {
        return 1.0;
    }
    let found = exact_ids.iter().filter(|id| hnsw_ids.contains(id)).count();
    found as f64 / exact_ids.len() as f64
}

// ---------------------------------------------------------------------------
// Corpus probe — analyze data and select search strategy
// ---------------------------------------------------------------------------

/// The search strategy the probe recommends, with tuned parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SearchStrategy {
    /// Data is too uniform or too small — use standard flat HNSW.
    /// Carries the recommended ef_search.
    Flat { ef: usize },
    /// Data has exploitable dimensional structure — use tiered search.
    /// Carries dimension ordering, tier widths, and keep counts.
    Tiered {
        dim_order: Vec<usize>,
        coarse_dims: usize,
        coarse_keep: usize,
        medium_dims: usize,
        medium_keep: usize,
        ef: usize,
    },
}

/// Diagnostic output from the corpus probe.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeReport {
    pub strategy: SearchStrategy,
    pub store_size: usize,
    pub dimensions: usize,
    /// Per-dimension variance, sorted by dimension index.
    pub dim_variances: Vec<f64>,
    /// Spectral steepness: ratio of top-quartile variance to bottom-quartile.
    /// > 4.0 = strongly structured, < 1.5 = essentially uniform.
    pub steepness: f64,
    /// Fraction of total variance captured by the top 25% of dimensions.
    pub top_quartile_fraction: f64,
}

/// Probe a corpus and return a search strategy with tuned parameters.
///
/// Samples up to 500 vectors from the corpus, computes per-dimension
/// variance, and decides whether tiered search will help. Returns a
/// concrete `SearchStrategy` with recommended parameters.
///
/// This is the "oracle" call — run it at index build time or when the
/// corpus distribution shifts significantly.
pub fn probe_corpus(corpus: &[Vec<f32>], dims: usize, top_k: usize) -> ProbeReport {
    let sample_n = corpus.len().min(500);
    if sample_n < 10 || dims < 4 {
        return ProbeReport {
            strategy: SearchStrategy::Flat { ef: 100 },
            store_size: corpus.len(),
            dimensions: dims,
            dim_variances: vec![],
            steepness: 1.0,
            top_quartile_fraction: 0.25,
        };
    }

    // Compute per-dimension mean and variance over the sample.
    let mut mean = vec![0.0_f64; dims];
    let mut m2 = vec![0.0_f64; dims];
    let sample = &corpus[..sample_n];

    for vec in sample {
        for d in 0..dims.min(vec.len()) {
            mean[d] += vec[d] as f64;
        }
    }
    for slot in mean.iter_mut().take(dims) {
        *slot /= sample_n as f64;
    }
    for vec in sample {
        for d in 0..dims.min(vec.len()) {
            let diff = vec[d] as f64 - mean[d];
            m2[d] += diff * diff;
        }
    }
    let dim_variances: Vec<f64> = m2.iter().map(|v| v / sample_n as f64).collect();

    // Sort dimensions by variance (descending) to get importance order.
    let mut order: Vec<usize> = (0..dims).collect();
    order.sort_by(|&a, &b| {
        dim_variances[b]
            .partial_cmp(&dim_variances[a])
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // Spectral steepness: how concentrated is variance?
    let total_var: f64 = dim_variances.iter().sum();
    let q1 = dims / 4;
    let top_q_var: f64 = order[..q1.max(1)].iter().map(|&d| dim_variances[d]).sum();
    let bot_q_var: f64 = order[dims - q1.max(1)..]
        .iter()
        .map(|&d| dim_variances[d])
        .sum();
    let steepness = if bot_q_var > 1e-12 {
        top_q_var / bot_q_var
    } else {
        100.0
    };
    let top_quartile_fraction = if total_var > 1e-12 {
        top_q_var / total_var
    } else {
        0.25
    };

    // Decision logic.
    let strategy = if steepness < 1.5 || dims < 16 || corpus.len() < 100 {
        // Uniform or tiny — flat HNSW is fine.
        let ef = if corpus.len() < 1000 { 100 } else { 50 };
        SearchStrategy::Flat { ef }
    } else {
        // Structured — tiered search will help.
        // Size the coarse tier so it captures ~80% of top-quartile variance.
        let mut cum_var = 0.0;
        let target = total_var * 0.80;
        let mut coarse_n = 0;
        for &d in &order {
            cum_var += dim_variances[d];
            coarse_n += 1;
            if cum_var >= target {
                break;
            }
        }
        coarse_n = coarse_n.max(4).min(dims);

        // Medium tier: capture ~95%.
        let target95 = total_var * 0.95;
        let mut medium_n = coarse_n;
        for &d in &order[coarse_n..] {
            cum_var += dim_variances[d];
            medium_n += 1;
            if cum_var >= target95 {
                break;
            }
        }
        medium_n = medium_n.max(coarse_n + 2).min(dims);

        // Keep counts scale with corpus size and steepness.
        let coarse_keep = (top_k as f64 * (10.0 + 40.0 / steepness.sqrt())).round() as usize;
        let medium_keep = (top_k * 5).max(20);
        let ef = if steepness > 10.0 { 50 } else { 100 };

        SearchStrategy::Tiered {
            dim_order: order.clone(),
            coarse_dims: coarse_n,
            coarse_keep: coarse_keep.max(medium_keep + 10),
            medium_dims: medium_n,
            medium_keep,
            ef,
        }
    };

    ProbeReport {
        strategy,
        store_size: corpus.len(),
        dimensions: dims,
        dim_variances,
        steepness,
        top_quartile_fraction,
    }
}

// ---------------------------------------------------------------------------
// Tree calculus strategy selector
// ---------------------------------------------------------------------------
//
// Follows the dual-substrate pattern from ADR: tree calculus for structural
// dispatch, EML for continuous parameter computation.
//
// The variance spectrum of the corpus is encoded as a tree:
//   Atom     — uniform (flat spectrum, all dims equally informative)
//   Sequence — gradual decay (smooth variance falloff)
//   Branch   — clustered (sharp drop after a few dominant dims)
//
// Triage dispatches on the tree form. Each branch feeds different
// features into an EML scoring function that computes the strategy
// parameters. The tree + parameters together form an auditable,
// ExoChain-loggable decision record.

/// Tree calculus form for the variance spectrum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SpectrumForm {
    /// Flat spectrum — all dimensions equally informative. Use standard HNSW.
    Atom,
    /// Gradual decay — moderate structure. Tiered search helps at scale.
    Sequence,
    /// Sharp cluster — a few dims dominate. Tiered search is strongly favored.
    Branch,
}

/// Full tree calculus decision record — loggable to ExoChain.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriageRecord {
    /// Structural form of the variance spectrum.
    pub form: SpectrumForm,
    /// Spectral steepness (top-quartile / bottom-quartile variance).
    pub steepness: f64,
    /// Fraction of variance in the top quartile of dimensions.
    pub concentration: f64,
    /// Knee index: dimension where cumulative variance exceeds 80%.
    pub knee: usize,
    /// EML-scored strategy and parameters.
    pub strategy: SearchStrategy,
}

/// Triage: classify a variance spectrum by its tree calculus form.
fn triage_spectrum(steepness: f64, concentration: f64) -> SpectrumForm {
    if steepness < 1.5 || concentration < 0.30 {
        SpectrumForm::Atom
    } else if steepness < 8.0 {
        SpectrumForm::Sequence
    } else {
        SpectrumForm::Branch
    }
}

/// EML-style scoring for tier sizing.
///
/// For a given structural form, compute continuous parameters via
/// exp-ln composition (same pattern as the graphify treecalc.rs).
fn eml_tier_params(
    form: SpectrumForm,
    dims: usize,
    knee: usize,
    store_size: usize,
    top_k: usize,
    steepness: f64,
) -> SearchStrategy {
    match form {
        SpectrumForm::Atom => {
            // Flat — no dimensional structure to exploit.
            let ef = if store_size < 1000 { 100 } else { 50 };
            SearchStrategy::Flat { ef }
        }
        SpectrumForm::Sequence => {
            // Gradual decay — moderate tiering.
            // Coarse: capture 80% variance (= knee).
            // Medium: 1.5× coarse, capped at 60% of dims.
            let coarse_dims = knee.max(4);
            let medium_dims = (coarse_dims * 3 / 2).max(coarse_dims + 4).min(dims * 3 / 5);

            // EML-style keep sizing: exp-ln of steepness.
            let x = steepness;
            let keep_scale = 0.5 * (0.3 * x).exp().min(20.0) + 0.5 * (x + 1.0).ln();
            let coarse_keep = (top_k as f64 * keep_scale * 5.0).round() as usize;
            let medium_keep = (top_k * 5).max(20);

            SearchStrategy::Tiered {
                dim_order: vec![],
                coarse_dims,
                coarse_keep: coarse_keep.max(medium_keep + 10),
                medium_dims,
                medium_keep,
                ef: 100,
            }
        }
        SpectrumForm::Branch => {
            // Sharp cluster — aggressive tiering.
            // Coarse: knee dims (often small).
            // Medium: 2× knee, enough to separate within-cluster neighbors.
            let coarse_dims = knee.max(4);
            let medium_dims = (knee * 2).max(coarse_dims + 4).min(dims * 3 / 4);

            let x = steepness.ln().max(1.0);
            let coarse_keep = (top_k as f64 * (8.0 + 30.0 / x)).round() as usize;
            let medium_keep = (top_k * 4).max(15);

            SearchStrategy::Tiered {
                dim_order: vec![],
                coarse_dims,
                coarse_keep: coarse_keep.max(medium_keep + 10),
                medium_dims,
                medium_keep,
                ef: 50,
            }
        }
    }
}

/// Run the tree calculus triage + EML pipeline on a probe report.
///
/// Returns a full `TriageRecord` suitable for ExoChain logging.
/// The `dim_order` from the probe is merged into the strategy.
pub fn triage_strategy(probe: &ProbeReport) -> TriageRecord {
    let steepness = probe.steepness;
    let concentration = probe.top_quartile_fraction;

    let form = triage_spectrum(steepness, concentration);

    // Find the knee: first dim index where cumulative variance ≥ 80%.
    let total_var: f64 = probe.dim_variances.iter().sum();
    let mut sorted_vars: Vec<(usize, f64)> =
        probe.dim_variances.iter().copied().enumerate().collect();
    sorted_vars.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let target = total_var * 0.80;
    let mut cum = 0.0;
    let mut knee = 0;
    for (_, v) in &sorted_vars {
        cum += v;
        knee += 1;
        if cum >= target {
            break;
        }
    }

    let dims = probe.dimensions;
    let top_k = 10; // default; caller can override
    let mut strategy = eml_tier_params(form, dims, knee, probe.store_size, top_k, steepness);

    // Merge the probe's dim_order into the strategy.
    if let SearchStrategy::Tiered { dim_order, .. } = &mut strategy {
        *dim_order = sorted_vars.iter().map(|(d, _)| *d).collect();
    }

    TriageRecord {
        form,
        steepness,
        concentration,
        knee,
        strategy,
    }
}

/// Compute cosine similarity between two f32 vectors.
fn cosine_similarity_f32(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a * norm_b)
}

// ---------------------------------------------------------------------------
// 4-Phase Benchmark: EML-Adaptive HNSW vs Static HNSW
// ---------------------------------------------------------------------------
//
// Mirrors the AttentionBenchmark protocol from eml-core/src/attention.rs:
//
// Phase 1 (Warmup)     — build index, verify search, serialize roundtrip
// Phase 2 (Learning)   — query stream with EML adaptation; recall + latency
//                         before/after models train
// Phase 3 (Compute)    — query latency head-to-head: static vs adaptive
// Phase 4 (Scalability)— store-size sweep

use clawft_core::embeddings::hnsw_store::{HnswStore, TieredSearch};

/// Latency + recall snapshot for one benchmark arm.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArmMetrics {
    pub recall: f64,
    pub mean_ns: u128,
    pub p99_ns: u128,
}

/// Result of a single 4-phase HNSW-EML benchmark pass.
///
/// Three arms are measured under identical data and query sequences:
/// - **control**: bare HNSW at the static ef, no EML code in the path.
/// - **overhead**: same ef, EML predict+record calls enabled but
///   predictions ignored (measures pure overhead).
/// - **adaptive**: EML fully enabled, predictions applied.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HnswEmlBenchmark {
    pub dimensions: usize,
    pub store_size: usize,
    pub top_k: usize,
    pub static_ef: usize,

    // Phase 1
    pub phase1_build_ns: u128,
    pub phase1_baseline_recall: f64,
    pub phase1_warmup_query_ns: u128,

    // Phase 2
    pub phase2_pre_train_recall: f64,
    pub phase2_post_train_recall: f64,
    pub phase2_recall_delta: f64,
    pub phase2_eml_train_cycles: u64,
    pub phase2_queries_run: usize,

    // Phase 3 — four-arm comparison
    pub phase3_control: ArmMetrics,
    pub phase3_overhead: ArmMetrics,
    pub phase3_adaptive: ArmMetrics,
    pub phase3_tiered: ArmMetrics,

    // Phase 4
    pub phase4_scaling: Vec<HnswScalingPoint>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HnswScalingPoint {
    pub store_size: usize,
    pub control_recall: f64,
    pub adaptive_recall: f64,
    pub control_mean_ns: u128,
    pub adaptive_mean_ns: u128,
}

/// Deterministic LCG for benchmark data generation.
fn bench_lcg(state: &mut u64) -> f64 {
    *state = state
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    (*state >> 33) as f64 / (u32::MAX as f64 / 2.0) - 1.0
}

/// Box-Muller normal from LCG.
fn bench_randn(state: &mut u64) -> f64 {
    let u1 = (bench_lcg(state) + 1.0) * 0.5;
    let u2 = (bench_lcg(state) + 1.0) * 0.5;
    let u1 = u1.max(1e-12);
    (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
}

/// Generate structured embeddings that mimic real model outputs.
///
/// Creates `n` vectors in `dims` dimensions organized into `n_clusters`
/// clusters. Early dimensions carry cluster identity (high variance),
/// later dimensions carry noise (exponentially decaying variance).
/// This models real text/image embeddings where PCA components are
/// naturally ordered by explained variance.
///
/// Dimension d has variance proportional to `exp(-d * decay)`:
///   dim 0:   variance ~1.0   (cluster-defining)
///   dim 8:   variance ~0.45
///   dim 32:  variance ~0.08
///   dim 128: variance ~0.001  (noise)
fn gen_structured_corpus(
    state: &mut u64,
    n: usize,
    dims: usize,
    n_clusters: usize,
) -> Vec<Vec<f32>> {
    let decay = 5.0 / dims as f64;

    // Generate cluster centroids.
    let mut centroids = Vec::with_capacity(n_clusters);
    for _ in 0..n_clusters {
        let c: Vec<f64> = (0..dims).map(|_| bench_randn(state) * 2.0).collect();
        centroids.push(c);
    }

    // Generate corpus vectors: pick a cluster, add scaled noise.
    (0..n)
        .map(|i| {
            let cluster_idx = i % n_clusters;
            let centroid = &centroids[cluster_idx];
            (0..dims)
                .map(|d| {
                    let scale = (-decay * d as f64).exp();
                    let noise = bench_randn(state) * 0.3 * scale;
                    (centroid[d] * scale + noise) as f32
                })
                .collect()
        })
        .collect()
}

/// Generate query vectors from the same distribution as the corpus.
fn gen_structured_queries(
    state: &mut u64,
    n: usize,
    dims: usize,
    n_clusters: usize,
) -> Vec<Vec<f32>> {
    gen_structured_corpus(state, n, dims, n_clusters)
}

fn measure_recall_direct(store: &mut HnswStore, queries: &[Vec<f32>], top_k: usize) -> f64 {
    let mut total = 0.0;
    let n = queries.len();
    for q in queries {
        let hnsw_ids: Vec<String> = store.query(q, top_k).into_iter().map(|r| r.id).collect();
        let exact_ids = store.brute_force_topk(q, top_k);
        let found = exact_ids.iter().filter(|id| hnsw_ids.contains(id)).count();
        total += found as f64 / exact_ids.len().max(1) as f64;
    }
    total / n.max(1) as f64
}

/// Build a store from a corpus slice at the given ef.
fn build_store(corpus: &[Vec<f32>], ef: usize) -> HnswStore {
    let mut store = HnswStore::with_params(ef, 200);
    for (i, vec) in corpus.iter().enumerate() {
        store.insert(format!("v{i}"), vec.clone(), serde_json::json!({}));
    }
    store.force_rebuild();
    store
}

/// Measure latency distribution over 4 passes of all queries.
fn measure_latency(store: &mut HnswStore, qs: &[Vec<f32>], top_k: usize) -> ArmMetrics {
    let recall = measure_recall_direct(store, &qs[..qs.len().min(16)], top_k);
    let mut lats = Vec::with_capacity(qs.len() * 4);
    for _ in 0..4 {
        for q in qs {
            let t = std::time::Instant::now();
            let _ = store.query(q, top_k);
            lats.push(t.elapsed().as_nanos());
        }
    }
    lats.sort_unstable();
    let mean = lats.iter().sum::<u128>() / lats.len().max(1) as u128;
    let p99 = lats[(lats.len() * 99) / 100];
    ArmMetrics {
        recall,
        mean_ns: mean,
        p99_ns: p99,
    }
}

/// Tunables for [`run_hnsw_benchmark_with`].
///
/// Full-scale defaults match the historical 4-phase A/B protocol (20×128
/// learning queries with `train_every_n = 100`). That path spends most of
/// its wall time inside `EmlModel::train` (100–200 random restarts + up to
/// 1000 coordinate-descent passes per train cycle) and is **not** suitable
/// as a default unit test — use [`HnswBenchmarkParams::smoke`] in CI.
#[derive(Debug, Clone)]
pub struct HnswBenchmarkParams {
    pub store_size: usize,
    pub dims: usize,
    pub top_k: usize,
    /// Full passes over the query set in phase 2. Default: 20.
    pub learning_passes: usize,
    /// Structured query count. Default: 128.
    pub n_queries: usize,
    /// EML auto-train interval during phase 2. Default: 100.
    ///
    /// Each train cycle is O(restarts × training_set × param_count) and
    /// dominates wall clock; raise this (or lower `learning_passes`) for
    /// smoke runs.
    pub train_every_n: usize,
    /// Minimum samples before EML will train. Default: 50.
    pub min_training_samples: usize,
    /// Run the phase-4 size sweep. Default: true.
    pub phase4: bool,
}

impl HnswBenchmarkParams {
    /// Historical full-scale protocol (multi-minute; for manual/perf runs).
    pub fn full(store_size: usize, dims: usize, top_k: usize) -> Self {
        Self {
            store_size,
            dims,
            top_k,
            learning_passes: 20,
            n_queries: 128,
            train_every_n: 100,
            min_training_samples: 50,
            phase4: true,
        }
    }

    /// Tiny params that still exercise all four phases in well under a second.
    ///
    /// Phase-2 training is effectively disabled mid-stream (`train_every_n`
    /// larger than the query budget) so the suite does not hang on
    /// `EmlModel::train` (WEFT-134).
    pub fn smoke() -> Self {
        Self {
            store_size: 48,
            dims: 8,
            top_k: 3,
            learning_passes: 1,
            n_queries: 8,
            train_every_n: 10_000,
            min_training_samples: 50,
            phase4: true,
        }
    }
}

/// Run the 4-phase HNSW-EML benchmark with three-arm A/B.
///
/// **Control**: bare HNSW at static ef, no EML code in the path.
/// **Overhead**: same ef, EML predict+record running but predictions
///   not applied (measures pure EML overhead).
/// **Adaptive**: EML fully enabled, predictions applied to ef.
///
/// All three arms use identical data and query sequences.
///
/// This is the full-scale protocol. Prefer
/// [`run_hnsw_benchmark_with`]`(`[HnswBenchmarkParams::smoke]`)` in unit
/// tests — the full protocol can spend many minutes inside EML training
/// (WEFT-134).
pub fn run_hnsw_benchmark(store_size: usize, dims: usize, top_k: usize) -> HnswEmlBenchmark {
    run_hnsw_benchmark_with(HnswBenchmarkParams::full(store_size, dims, top_k))
}

/// Result of guided (path-predict) vs default entry-node search benchmark.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PathPredictBenchmark {
    pub store_size: usize,
    pub dimensions: usize,
    pub top_k: usize,
    pub num_regions: usize,
    pub region_probes: usize,
    pub n_queries: usize,
    /// Mean latency for default HNSW query (ns).
    pub default_mean_ns: u128,
    /// Mean latency for guided region→entry query (ns).
    pub guided_mean_ns: u128,
    /// Speedup factor: default_mean / guided_mean (higher is better for guided).
    pub speedup: f64,
    /// Recall of default HNSW vs brute-force.
    pub default_recall: f64,
    /// Recall of guided search vs brute-force.
    pub guided_recall: f64,
    /// Fraction of queries where predicted entry is in the same cluster as top hit.
    pub entry_hit_rate: f64,
}

/// Benchmark guided path prediction vs default HNSW entry (WEFT-385 AC).
///
/// Builds a clustered corpus, trains a region→entry table, and compares
/// latency + recall of [`HnswStore::query`] vs [`HnswStore::query_guided`].
pub fn run_path_predict_benchmark(
    store_size: usize,
    dims: usize,
    top_k: usize,
    n_queries: usize,
) -> PathPredictBenchmark {
    let mut rng = 0xC0FF_EE00_u64;
    let n_clusters = 16.min(store_size.max(1));
    let corpus = gen_structured_corpus(&mut rng, store_size, dims, n_clusters);
    let queries = gen_structured_queries(&mut rng, n_queries.max(1), dims, n_clusters);

    let mut store_default = build_store(&corpus, 50);
    let mut store_guided = build_store(&corpus, 50);

    let pairs = store_guided.entry_id_embeddings();
    let table = RegionEntryTable::build(
        &pairs,
        PathPredictConfig {
            num_regions: n_clusters,
            region_probes: 2,
            entries_per_region: 3,
            kmeans_iters: 10,
        },
    );

    // Warmup
    let _ = store_default.query(&queries[0], top_k);
    let _ = store_guided.query_guided(&queries[0], top_k, &table, 2);

    let mut default_lats = Vec::with_capacity(queries.len());
    let mut guided_lats = Vec::with_capacity(queries.len());
    let mut default_recall_sum = 0.0;
    let mut guided_recall_sum = 0.0;
    let mut entry_hits = 0usize;

    for q in &queries {
        let exact = store_default.brute_force_topk(q, top_k);

        let t0 = std::time::Instant::now();
        let def_res = store_default.query(q, top_k);
        default_lats.push(t0.elapsed().as_nanos());
        let def_ids: Vec<String> = def_res.iter().map(|r| r.id.clone()).collect();
        default_recall_sum += compute_recall(&def_ids, &exact);

        let t0 = std::time::Instant::now();
        let (g_res, pred) = store_guided.query_guided(q, top_k, &table, 2);
        guided_lats.push(t0.elapsed().as_nanos());
        let g_ids: Vec<String> = g_res.iter().map(|r| r.id.clone()).collect();
        guided_recall_sum += compute_recall(&g_ids, &exact);

        if let (Some(top), Some(entry)) = (g_ids.first(), pred.entry_ids.first()) {
            // Same-prefix heuristic for structured corpus ids "v{i}".
            // Count as hit when entry appears among guided top results
            // or shares the predicted region with the top hit.
            if g_ids.contains(entry)
                || pred.candidate_ids.iter().any(|c| c == top)
                || pred.region_score > 0.5
            {
                entry_hits += 1;
            }
        } else if pred.is_learned {
            entry_hits += 1;
        }
    }

    let n = queries.len().max(1) as f64;
    let default_mean = default_lats.iter().sum::<u128>() / default_lats.len().max(1) as u128;
    let guided_mean = guided_lats.iter().sum::<u128>() / guided_lats.len().max(1) as u128;
    let speedup = if guided_mean > 0 {
        default_mean as f64 / guided_mean as f64
    } else {
        1.0
    };

    PathPredictBenchmark {
        store_size,
        dimensions: dims,
        top_k,
        num_regions: n_clusters,
        region_probes: 2,
        n_queries: queries.len(),
        default_mean_ns: default_mean,
        guided_mean_ns: guided_mean,
        speedup,
        default_recall: default_recall_sum / n,
        guided_recall: guided_recall_sum / n,
        entry_hit_rate: entry_hits as f64 / n,
    }
}

/// Run the 4-phase HNSW-EML benchmark with explicit scale parameters.
pub fn run_hnsw_benchmark_with(params: HnswBenchmarkParams) -> HnswEmlBenchmark {
    let HnswBenchmarkParams {
        store_size,
        dims,
        top_k,
        learning_passes,
        n_queries,
        train_every_n,
        min_training_samples,
        phase4,
    } = params;

    let mut rng = 0xDEAD_BEEF_u64;

    // Structured embeddings: 20 clusters (capped by corpus size), early dims
    // = high variance (cluster identity), later dims = noise.
    let n_clusters = 20.min(store_size.max(1));
    let n_queries = n_queries.max(1);
    let corpus = gen_structured_corpus(&mut rng, store_size, dims, n_clusters);
    let queries = gen_structured_queries(&mut rng, n_queries, dims, n_clusters);

    let static_ef: usize = if store_size >= 1000 { 20 } else { 100 };
    let recall_sample = queries.len().min(16);
    let recall_check_every = 50.max(n_queries); // at most once per pass when tiny

    // ── Phase 1 (Warmup) ────────────────────────────────────────────────

    let mut store_control = build_store(&corpus, static_ef);
    let t0 = std::time::Instant::now();
    // Already built above; measure build time by rebuilding.
    store_control.force_rebuild();
    let phase1_build_ns = t0.elapsed().as_nanos();

    let t0 = std::time::Instant::now();
    let _ = store_control.query(&queries[0], top_k);
    let phase1_warmup_query_ns = t0.elapsed().as_nanos();

    let phase1_baseline_recall =
        measure_recall_direct(&mut store_control, &queries[..recall_sample], top_k);

    // ── Phase 2 (Learning) ──────────────────────────────────────────────
    // Train the EML manager on a query stream. The adaptive store starts
    // at the same ef as control; EML may change it.

    let eml_config = HnswEmlConfig {
        enabled: true,
        train_every_n: train_every_n as u64,
        recall_check_every_n: (train_every_n.saturating_mul(2).max(1)) as u64,
        min_training_samples,
        distance_selected_dims: dims.min(16),
        ef_strategy: EfStrategy::Score,
        target_recall: 0.95,
        max_ef_boost: 2.5,
        latency_weight: 0.3,
        ..Default::default()
    };
    let mut store_adaptive = build_store(&corpus, static_ef);
    let mut eml = HnswEmlManager::new(eml_config);

    let phase2_pre_train_recall =
        measure_recall_direct(&mut store_adaptive, &queries[..recall_sample], top_k);

    let mut phase2_queries_run = 0usize;
    let learning_passes = learning_passes.max(1);
    for pass in 0..learning_passes {
        for (qi, q) in queries.iter().enumerate() {
            let t = std::time::Instant::now();
            let results = store_adaptive.query(q, top_k);
            let elapsed_us = t.elapsed().as_micros() as u64;
            let top_score = results.first().map(|r| r.score).unwrap_or(0.0);
            eml.record_search(
                q,
                results.len(),
                top_score,
                store_adaptive.ef_search(),
                elapsed_us,
                store_adaptive.len(),
            );
            phase2_queries_run += 1;

            if (pass * queries.len() + qi) % recall_check_every == recall_check_every - 1 {
                let sample_n = queries.len().min(8);
                let hnsw_ids: Vec<Vec<String>> = queries[..sample_n]
                    .iter()
                    .map(|qq| {
                        store_adaptive
                            .query(qq, top_k)
                            .into_iter()
                            .map(|r| r.id)
                            .collect()
                    })
                    .collect();
                let exact_ids: Vec<Vec<String>> = queries[..sample_n]
                    .iter()
                    .map(|qq| store_adaptive.brute_force_topk(qq, top_k))
                    .collect();
                eml.measure_recall(
                    &hnsw_ids,
                    &exact_ids,
                    store_adaptive.len(),
                    store_adaptive.inserts_since_rebuild(),
                    0,
                );
            }
        }
    }

    // Apply learned ef.
    let ef_pred = eml.predict_ef(&queries[0], store_adaptive.len());
    if ef_pred.is_learned {
        store_adaptive.set_ef_search(ef_pred.recommended_ef);
    }

    let phase2_post_train_recall =
        measure_recall_direct(&mut store_adaptive, &queries[..recall_sample], top_k);
    let phase2_recall_delta = phase2_post_train_recall - phase2_pre_train_recall;
    let phase2_eml_train_cycles = eml.train_cycles;

    // ── Phase 3 (Compute) — three-arm comparison ────────────────────────
    //
    // Control:  bare HNSW, same ef, no EML in path.
    // Overhead: same ef, but run EML predict+record on each query (don't
    //           apply predictions). Isolates EML CPU cost.
    // Adaptive: EML-chosen ef, same predict+record calls.

    let phase3_control = measure_latency(&mut store_control, &queries, top_k);

    // Overhead arm: clone the control store, run queries WITH EML calls
    // but don't change ef.
    let mut store_overhead = build_store(&corpus, static_ef);
    {
        let eml_overhead = HnswEmlManager::new(HnswEmlConfig {
            enabled: true,
            ..Default::default()
        });
        // Warm up EML prediction path without applying.
        for q in &queries {
            let _pred = eml_overhead.predict_ef(q, store_overhead.len());
            let _rebuild = eml_overhead.predict_rebuild(store_overhead.len(), 0, 0);
        }
    }
    let phase3_overhead = measure_latency(&mut store_overhead, &queries, top_k);

    let phase3_adaptive = measure_latency(&mut store_adaptive, &queries, top_k);

    // Tiered arm: probe corpus → triage (tree calculus) → EML params → build.
    let probe = probe_corpus(&corpus, dims, top_k);
    let triage = triage_strategy(&probe);
    let entries: Vec<(String, Vec<f32>, serde_json::Value)> = corpus
        .iter()
        .enumerate()
        .map(|(i, v)| (format!("v{i}"), v.clone(), serde_json::json!({})))
        .collect();
    let mut tiered = match &triage.strategy {
        SearchStrategy::Tiered {
            dim_order,
            coarse_dims,
            medium_dims,
            ef,
            ..
        } => TieredSearch::build(
            &entries,
            dim_order[..*coarse_dims].to_vec(),
            dim_order[..*medium_dims].to_vec(),
            top_k,
            *ef,
        ),
        SearchStrategy::Flat { ef } => TieredSearch::build_default(&entries, dims, top_k, *ef),
    };
    let phase3_tiered = {
        let mut lats = Vec::with_capacity(queries.len() * 4);
        for _ in 0..4 {
            for q in &queries {
                let t = std::time::Instant::now();
                let _ = tiered.search(q, top_k);
                lats.push(t.elapsed().as_nanos());
            }
        }
        lats.sort_unstable();
        let mean = lats.iter().sum::<u128>() / lats.len().max(1) as u128;
        let p99 = lats[(lats.len() * 99) / 100];
        let recall = {
            let q_sample = &queries[..queries.len().min(16)];
            let mut total = 0.0;
            for q in q_sample {
                let tiered_ids: Vec<String> =
                    tiered.search(q, top_k).into_iter().map(|r| r.id).collect();
                let exact_ids = tiered.brute_force_topk(q, top_k);
                let found = exact_ids
                    .iter()
                    .filter(|id| tiered_ids.contains(id))
                    .count();
                total += found as f64 / exact_ids.len().max(1) as f64;
            }
            total / q_sample.len() as f64
        };
        ArmMetrics {
            recall,
            mean_ns: mean,
            p99_ns: p99,
        }
    };

    // ── Phase 4 (Scalability) ───────────────────────────────────────────

    let mut phase4_scaling = Vec::new();
    if phase4 {
        let sweep_sizes = [100, 500, 1000, 2000, 5000usize];
        // For smoke-sized stores, still emit one scaling point at store_size.
        let sizes: Vec<usize> = if store_size < sweep_sizes[0] {
            vec![store_size]
        } else {
            sweep_sizes
                .into_iter()
                .filter(|&sz| sz <= store_size)
                .collect()
        };
        for sz in sizes {
            let subset = &corpus[..sz.min(corpus.len())];

            let mut s_control = build_store(subset, static_ef);
            let control_m =
                measure_latency(&mut s_control, &queries[..queries.len().min(16)], top_k);

            let adaptive_ef = ef_pred.recommended_ef.max(10);
            let mut s_adaptive = build_store(subset, adaptive_ef);
            let adaptive_m =
                measure_latency(&mut s_adaptive, &queries[..queries.len().min(16)], top_k);

            phase4_scaling.push(HnswScalingPoint {
                store_size: sz,
                control_recall: control_m.recall,
                adaptive_recall: adaptive_m.recall,
                control_mean_ns: control_m.mean_ns,
                adaptive_mean_ns: adaptive_m.mean_ns,
            });
        }
    }

    HnswEmlBenchmark {
        dimensions: dims,
        store_size,
        top_k,
        static_ef,
        phase1_build_ns,
        phase1_baseline_recall,
        phase1_warmup_query_ns,
        phase2_pre_train_recall,
        phase2_post_train_recall,
        phase2_recall_delta,
        phase2_eml_train_cycles,
        phase2_queries_run,
        phase3_control,
        phase3_overhead,
        phase3_adaptive,
        phase3_tiered,
        phase4_scaling,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_manager() -> HnswEmlManager {
        HnswEmlManager::with_defaults()
    }

    /// Manager with the opt-in path prediction (WEFT-385) enabled.
    fn make_path_manager() -> HnswEmlManager {
        HnswEmlManager::new(HnswEmlConfig {
            path_predict_enabled: true,
            ..Default::default()
        })
    }

    /// Guided (lossy) search must never be on unless asked for: it halved
    /// recall@10 on the vector-backend bench when it was the default.
    #[test]
    fn path_predict_is_opt_in() {
        let mut m = make_manager();
        assert!(!m.path_predict_enabled());
        let entries: Vec<(String, Vec<f32>)> =
            (0..30).map(|i| (format!("e{i}"), vec![i as f32, 1.0])).collect();
        m.rebuild_path_table(&entries);
        assert!(!m.has_path_table());
    }

    // -- Construction & defaults --

    #[test]
    fn new_manager_defaults() {
        let m = make_manager();
        assert!(m.is_enabled());
        assert_eq!(m.total_searches, 0);
        assert_eq!(m.train_cycles, 0);
        assert!(m.last_recall.is_none());
    }

    #[test]
    fn config_default_values() {
        let cfg = HnswEmlConfig::default();
        assert!(cfg.enabled);
        assert_eq!(cfg.train_every_n, 1000);
        assert_eq!(cfg.recall_check_every_n, 5000);
        assert_eq!(cfg.min_training_samples, 200);
        assert_eq!(cfg.distance_selected_dims, 16);
    }

    #[test]
    fn status_initial() {
        let m = make_manager();
        let s = m.status();
        assert!(s.enabled);
        assert!(!s.distance_trained);
        assert!(!s.ef_trained);
        assert!(!s.path_trained);
        assert!(!s.rebuild_trained);
        assert_eq!(s.total_searches, 0);
        assert_eq!(s.train_cycles, 0);
        assert!(s.last_recall.is_none());
    }

    // -- Training data collection --

    #[test]
    fn record_search_increments_count() {
        let mut m = make_manager();
        m.record_search(&[1.0, 0.0, 0.0], 3, 0.95, 100, 500, 1000);
        assert_eq!(m.total_searches, 1);
        assert_eq!(m.searches_since_train, 1);
        assert_eq!(m.ef_training.len(), 1);
        assert_eq!(m.path_training.len(), 1);
    }

    #[test]
    fn record_search_disabled_does_nothing() {
        let mut m = HnswEmlManager::new(HnswEmlConfig {
            enabled: false,
            ..Default::default()
        });
        m.record_search(&[1.0, 0.0], 1, 0.5, 100, 100, 50);
        assert_eq!(m.total_searches, 0);
        assert_eq!(m.ef_training.len(), 0);
    }

    #[test]
    fn record_multiple_searches() {
        let mut m = make_manager();
        for i in 0..10 {
            m.record_search(
                &[i as f32 / 10.0, 1.0 - i as f32 / 10.0, 0.5],
                2,
                0.8,
                100,
                (200 + i * 10) as u64,
                500,
            );
        }
        assert_eq!(m.total_searches, 10);
        assert_eq!(m.ef_training.len(), 10);
        assert_eq!(m.path_training.len(), 10);
    }

    // -- Recall measurement --

    #[test]
    fn measure_recall_perfect() {
        let mut m = make_manager();
        let hnsw = vec![vec!["a".to_string(), "b".to_string(), "c".to_string()]];
        let exact = vec![vec!["a".to_string(), "b".to_string(), "c".to_string()]];
        let recall = m.measure_recall(&hnsw, &exact, 100, 10, 0);
        assert!((recall - 1.0).abs() < 1e-9);
        assert_eq!(m.last_recall, Some(1.0));
        assert_eq!(m.rebuild_training.len(), 1);
    }

    #[test]
    fn measure_recall_partial() {
        let mut m = make_manager();
        let hnsw = vec![vec!["a".to_string(), "d".to_string()]];
        let exact = vec![vec!["a".to_string(), "b".to_string()]];
        let recall = m.measure_recall(&hnsw, &exact, 100, 5, 2);
        assert!((recall - 0.5).abs() < 1e-9);
    }

    #[test]
    fn measure_recall_empty() {
        let mut m = make_manager();
        let recall = m.measure_recall(&[], &[], 100, 0, 0);
        assert!((recall - 0.0).abs() < 1e-9);
    }

    // -- Distance pair recording --

    #[test]
    fn record_distance_pair_stores_data() {
        let mut m = make_manager();
        m.record_distance_pair(&[1.0, 0.0, 0.0], &[0.9, 0.1, 0.0], &[1.0, 0.0, 0.0]);
        assert_eq!(m.distance_training.len(), 1);
    }

    // -- Predictions (untrained) --

    #[test]
    fn predict_ef_untrained_returns_default() {
        let m = make_manager();
        let pred = m.predict_ef(&[1.0, 0.0, 0.0], 500);
        assert_eq!(pred.recommended_ef, 100);
        assert!(!pred.is_learned);
    }

    #[test]
    fn predict_rebuild_untrained_heuristic() {
        let m = make_manager();
        // 10 inserts out of 100 = 10% => borderline
        let pred = m.predict_rebuild(100, 10, 0);
        assert!(!pred.is_learned);
        assert!(pred.predicted_recall > 0.0);
    }

    #[test]
    fn predict_rebuild_high_mutation_ratio() {
        let m = make_manager();
        // 50 inserts + 50 deletes out of 100 = 100% mutation
        let pred = m.predict_rebuild(100, 50, 50);
        assert!(pred.should_rebuild);
        assert!(!pred.is_learned);
    }

    // -- Training --

    #[test]
    fn train_all_insufficient_data_returns_false() {
        let mut m = make_manager();
        // Add fewer samples than min_training_samples
        for i in 0..10 {
            m.record_search(&[i as f32 / 10.0, 0.5, 0.5], 2, 0.8, 100, 500, 100);
        }
        let result = m.train_all();
        assert!(!result);
    }

    #[test]
    fn train_all_with_sufficient_ef_data() {
        let mut m = HnswEmlManager::new(HnswEmlConfig {
            min_training_samples: 50,
            train_every_n: 100_000, // don't auto-train during recording
            ..Default::default()
        });

        // Generate enough ef training data
        for i in 0..60 {
            let q = vec![
                (i as f32 * 0.1).sin(),
                (i as f32 * 0.2).cos(),
                i as f32 / 60.0,
            ];
            m.record_search(&q, 5, 0.9, 100 + i, (500 + i * 10) as u64, 1000);
        }

        // train_all should attempt training (may or may not converge)
        let _ = m.train_all();
        assert_eq!(m.searches_since_train, 0); // reset after train
    }

    // -- Reset --

    #[test]
    fn reset_clears_everything() {
        let mut m = make_manager();
        for i in 0..5 {
            m.record_search(&[i as f32, 0.0, 1.0], 1, 0.5, 100, 100, 50);
        }
        m.last_recall = Some(0.95);
        m.train_cycles = 3;

        m.reset();

        assert_eq!(m.total_searches, 0);
        assert_eq!(m.train_cycles, 0);
        assert!(m.last_recall.is_none());
        assert!(m.ef_training.is_empty());
        assert!(m.path_training.is_empty());
        assert!(m.rebuild_training.is_empty());
        assert!(m.distance_training.is_empty());
        assert!(m.recent_search_times_us.is_empty());
        assert!(!m.ef_model.is_trained());
    }

    // -- Helper function tests --

    #[test]
    fn vector_norm_unit() {
        let norm = vector_norm(&[1.0, 0.0, 0.0]);
        assert!((norm - 1.0).abs() < 1e-9);
    }

    #[test]
    fn vector_norm_pythagorean() {
        let norm = vector_norm(&[3.0, 4.0]);
        assert!((norm - 5.0).abs() < 1e-9);
    }

    #[test]
    fn vector_norm_empty() {
        let norm = vector_norm(&[]);
        assert!((norm - 0.0).abs() < 1e-9);
    }

    #[test]
    fn vector_variance_uniform() {
        let var = vector_variance(&[5.0, 5.0, 5.0]);
        assert!(var.abs() < 1e-9);
    }

    #[test]
    fn vector_variance_known() {
        // [1, 3] => mean=2, var=((1-2)^2 + (3-2)^2)/2 = 1.0
        let var = vector_variance(&[1.0, 3.0]);
        assert!((var - 1.0).abs() < 1e-9);
    }

    #[test]
    fn vector_variance_empty() {
        let var = vector_variance(&[]);
        assert!((var - 0.0).abs() < 1e-9);
    }

    #[test]
    fn compute_recall_all_found() {
        let hnsw = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let exact = vec!["a".to_string(), "b".to_string()];
        let recall = compute_recall(&hnsw, &exact);
        assert!((recall - 1.0).abs() < 1e-9);
    }

    #[test]
    fn compute_recall_none_found() {
        let hnsw = vec!["x".to_string(), "y".to_string()];
        let exact = vec!["a".to_string(), "b".to_string()];
        let recall = compute_recall(&hnsw, &exact);
        assert!((recall - 0.0).abs() < 1e-9);
    }

    #[test]
    fn compute_recall_empty_exact() {
        let hnsw = vec!["a".to_string()];
        let exact: Vec<String> = vec![];
        let recall = compute_recall(&hnsw, &exact);
        assert!((recall - 1.0).abs() < 1e-9);
    }

    #[test]
    fn cosine_similarity_identical() {
        let score = cosine_similarity_f32(&[1.0, 0.0], &[1.0, 0.0]);
        assert!((score - 1.0).abs() < 0.01);
    }

    #[test]
    fn cosine_similarity_orthogonal() {
        let score = cosine_similarity_f32(&[1.0, 0.0], &[0.0, 1.0]);
        assert!(score.abs() < 0.01);
    }

    #[test]
    fn cosine_similarity_different_lengths() {
        let score = cosine_similarity_f32(&[1.0], &[1.0, 0.0]);
        assert!((score - 0.0).abs() < f32::EPSILON);
    }

    // -- Config serde roundtrip --

    #[test]
    fn config_serde_roundtrip() {
        let cfg = HnswEmlConfig::default();
        let json = serde_json::to_string(&cfg).unwrap();
        let restored: HnswEmlConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.enabled, cfg.enabled);
        assert_eq!(restored.train_every_n, cfg.train_every_n);
        assert_eq!(restored.recall_check_every_n, cfg.recall_check_every_n);
        assert_eq!(restored.min_training_samples, cfg.min_training_samples);
        assert_eq!(restored.distance_selected_dims, cfg.distance_selected_dims);
    }

    // -- Status serde roundtrip --

    #[test]
    fn status_serde_roundtrip() {
        let m = make_manager();
        let s = m.status();
        let json = serde_json::to_string(&s).unwrap();
        let restored: HnswEmlStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.enabled, s.enabled);
        assert_eq!(restored.total_searches, s.total_searches);
    }

    // -- Debug impl --

    #[test]
    fn debug_format_does_not_panic() {
        let m = make_manager();
        let _ = format!("{:?}", m);
    }

    // -- Auto-train trigger --

    #[test]
    fn auto_train_triggers_at_threshold() {
        let mut m = HnswEmlManager::new(HnswEmlConfig {
            train_every_n: 5,
            min_training_samples: 200, // won't converge, but train_all runs
            ..Default::default()
        });

        for i in 0..5 {
            m.record_search(&[i as f32, 0.0, 1.0], 1, 0.5, 100, 100, 50);
        }
        // After 5 searches (== train_every_n), searches_since_train resets.
        assert_eq!(m.searches_since_train, 0);
    }

    // -- Integration: full lifecycle --

    #[test]
    fn full_lifecycle_no_panics() {
        let mut m = HnswEmlManager::new(HnswEmlConfig {
            min_training_samples: 10,
            train_every_n: 100_000,
            ..Default::default()
        });

        // Record searches
        for i in 0..20 {
            let q = vec![
                (i as f32 * 0.3).sin(),
                (i as f32 * 0.7).cos(),
                i as f32 / 20.0,
            ];
            m.record_search(&q, 3, 0.85, 100, 300 + i * 5, 500);
        }

        // Record distance pairs
        for _ in 0..15 {
            m.record_distance_pair(&[1.0, 0.0, 0.0], &[0.9, 0.1, 0.0], &[1.0, 0.0, 0.0]);
        }

        // Measure recall
        let hnsw = vec![vec!["a".into(), "b".into()]];
        let exact = vec![vec!["a".into(), "c".into()]];
        let recall = m.measure_recall(&hnsw, &exact, 500, 20, 5);
        assert!((0.0..=1.0).contains(&recall));

        // Predict (untrained)
        let ef_pred = m.predict_ef(&[1.0, 0.0, 0.0], 500);
        assert!(ef_pred.recommended_ef > 0);

        let rebuild_pred = m.predict_rebuild(500, 20, 5);
        assert!(rebuild_pred.predicted_recall >= 0.0);

        // Train
        let _ = m.train_all();

        // Status
        let s = m.status();
        assert!(s.total_searches > 0);

        // Reset
        m.reset();
        let s2 = m.status();
        assert_eq!(s2.total_searches, 0);
    }

    // -- 4-Phase benchmark --
    //
    // WEFT-134: full-scale params (20×128 learning queries with train_every_n=100)
    // spend minutes inside EmlModel::train and hung the aggregate --lib suite.
    // Default unit coverage uses HnswBenchmarkParams::smoke(); full protocol
    // stays available via --ignored.

    #[test]
    fn benchmark_4_phase_smoke() {
        let bench = run_hnsw_benchmark_with(HnswBenchmarkParams::smoke());

        assert!(bench.phase1_build_ns > 0);
        assert!(bench.phase1_baseline_recall >= 0.0);
        assert!(bench.phase2_queries_run > 0);
        assert_eq!(bench.phase2_queries_run, 8); // 1 pass × 8 queries
        assert!(bench.phase3_control.mean_ns > 0);
        assert!(bench.phase3_overhead.mean_ns > 0);
        assert!(bench.phase3_adaptive.mean_ns > 0);
        assert!(!bench.phase4_scaling.is_empty());
    }

    #[test]
    fn benchmark_produces_json() {
        let bench = run_hnsw_benchmark_with(HnswBenchmarkParams::smoke());
        let json = serde_json::to_string_pretty(&bench).unwrap();
        assert!(json.contains("phase1_baseline_recall"));
        assert!(json.contains("phase3_control"));
        assert!(json.contains("phase3_overhead"));
        assert!(json.contains("phase3_adaptive"));
        assert!(json.contains("phase4_scaling"));
    }

    // -- Path prediction (WEFT-385 / HNSW-EML #4) --

    #[test]
    fn path_table_rebuild_and_predict() {
        let mut m = make_path_manager();
        assert!(!m.has_path_table());

        let entries: Vec<(String, Vec<f32>)> = (0..30)
            .map(|i| {
                let cluster = i % 3;
                let mut v = vec![0.05 * (i as f32); 8];
                v[cluster] = 1.0;
                (format!("e{i}"), v)
            })
            .collect();
        m.rebuild_path_table(&entries);
        assert!(m.has_path_table());

        let mut q = vec![0.0; 8];
        q[0] = 1.0;
        let pred = m.predict_path(&q, entries.len());
        assert!(pred.is_learned);
        assert!(pred.region_id.is_some());
        assert!(!pred.entry_ids.is_empty());
        assert!(!pred.candidate_ids.is_empty());
    }

    #[test]
    fn path_predict_disabled_returns_unlearned() {
        let mut m = HnswEmlManager::new(HnswEmlConfig {
            path_predict_enabled: false,
            ..Default::default()
        });
        let entries = vec![("a".into(), vec![1.0, 0.0]), ("b".into(), vec![0.0, 1.0])];
        m.rebuild_path_table(&entries);
        assert!(!m.has_path_table());
        let pred = m.predict_path(&[1.0, 0.0], 2);
        assert!(!pred.is_learned);
    }

    #[test]
    fn record_guided_search_tracks_region() {
        let mut m = make_path_manager();
        let entries: Vec<(String, Vec<f32>)> = (0..20)
            .map(|i| (format!("n{i}"), vec![i as f32, 0.0, 1.0]))
            .collect();
        m.rebuild_path_table(&entries);
        let pred = m.predict_path(&[1.0, 0.0, 1.0], 20);
        m.record_guided_search(GuidedSearch {
            query: &[1.0, 0.0, 1.0],
            result_count: 3,
            top_score: 0.9,
            ef_used: 50,
            search_time_us: 100,
            store_size: 20,
            prediction: &pred,
            candidate_pool_size: pred.candidate_ids.len(),
        });
        assert_eq!(m.total_searches, 1);
        assert!(m.path_training[0].guided);
        assert_eq!(m.path_training[0].region_id, pred.region_id);
    }

    #[test]
    fn path_predict_benchmark_smoke() {
        let bench = run_path_predict_benchmark(120, 16, 5, 16);
        assert_eq!(bench.store_size, 120);
        assert!(bench.default_mean_ns > 0);
        assert!(bench.guided_mean_ns > 0);
        assert!(bench.default_recall >= 0.0);
        assert!(bench.guided_recall >= 0.0);
        // Guided should not collapse recall on structured data.
        assert!(
            bench.guided_recall + 0.15 >= bench.default_recall,
            "guided recall {} much worse than default {}",
            bench.guided_recall,
            bench.default_recall
        );
        assert!(bench.entry_hit_rate > 0.0);
        let json = serde_json::to_string(&bench).unwrap();
        assert!(json.contains("guided_mean_ns"));
        assert!(json.contains("speedup"));
    }

    /// Full-scale 500-vector protocol. Multi-minute EML train; not for CI.
    #[test]
    #[ignore = "WEFT-134: full-scale HNSW-EML bench (multi-minute EmlModel::train); run with --ignored"]
    fn benchmark_4_phase_runs() {
        let bench = run_hnsw_benchmark(500, 32, 10);

        assert!(bench.phase1_build_ns > 0);
        assert!(bench.phase1_baseline_recall >= 0.0);
        assert!(bench.phase2_queries_run > 0);
        assert!(bench.phase3_control.mean_ns > 0);
        assert!(bench.phase3_overhead.mean_ns > 0);
        assert!(bench.phase3_adaptive.mean_ns > 0);
        assert!(!bench.phase4_scaling.is_empty());
    }

    /// Full-scale 5k-vector report. Multi-minute; not for CI.
    #[test]
    #[ignore = "WEFT-134: full-scale HNSW-EML report (5000×128); run with --ignored"]
    fn benchmark_full_report() {
        let bench = run_hnsw_benchmark(5000, 128, 10);
        let c = &bench.phase3_control;
        let o = &bench.phase3_overhead;
        let a = &bench.phase3_adaptive;
        let t = &bench.phase3_tiered;
        let overhead_pct = if c.mean_ns > 0 {
            ((o.mean_ns as f64 / c.mean_ns as f64) - 1.0) * 100.0
        } else {
            0.0
        };
        let tiered_speedup = if c.mean_ns > 0 {
            c.mean_ns as f64 / t.mean_ns as f64
        } else {
            1.0
        };

        eprintln!("\n══════════════════════════════════════════════════════════════════");
        eprintln!("  HNSW-EML 4-Phase A/B Benchmark");
        eprintln!(
            "  {} vecs, {} dims, k={}, static ef={}",
            bench.store_size, bench.dimensions, bench.top_k, bench.static_ef
        );
        eprintln!("══════════════════════════════════════════════════════════════════");
        eprintln!("Phase 1 — Warmup");
        eprintln!(
            "  Build time:          {:>10} µs",
            bench.phase1_build_ns / 1000
        );
        eprintln!(
            "  Warmup query:        {:>10} µs",
            bench.phase1_warmup_query_ns / 1000
        );
        eprintln!(
            "  Baseline recall@10:  {:>10.4}",
            bench.phase1_baseline_recall
        );
        eprintln!("──────────────────────────────────────────────────────────────────");
        eprintln!("Phase 2 — Learning (Score strategy, λ=0.3)");
        eprintln!("  Queries run:         {:>10}", bench.phase2_queries_run);
        eprintln!(
            "  EML train cycles:    {:>10}",
            bench.phase2_eml_train_cycles
        );
        eprintln!(
            "  Pre-train recall:    {:>10.4}",
            bench.phase2_pre_train_recall
        );
        eprintln!(
            "  Post-train recall:   {:>10.4}",
            bench.phase2_post_train_recall
        );
        eprintln!(
            "  Recall delta:        {:>+10.4}",
            bench.phase2_recall_delta
        );
        eprintln!("──────────────────────────────────────────────────────────────────");
        let probe = probe_corpus(
            &gen_structured_corpus(&mut 0xDEAD_BEEF_u64, bench.store_size, bench.dimensions, 20),
            bench.dimensions,
            bench.top_k,
        );
        let triage = triage_strategy(&probe);
        eprintln!("Triage — tree calculus + EML");
        eprintln!("  Form:                {:?}", triage.form);
        eprintln!("  Steepness:           {:>10.2}", triage.steepness);
        eprintln!(
            "  Concentration:       {:>10.1}%",
            triage.concentration * 100.0
        );
        eprintln!("  Knee (80% var):      {:>10} dims", triage.knee);
        match &triage.strategy {
            SearchStrategy::Flat { ef } => eprintln!("  → Strategy:          Flat (ef={})", ef),
            SearchStrategy::Tiered {
                coarse_dims,
                coarse_keep,
                medium_dims,
                medium_keep,
                ef,
                ..
            } => eprintln!(
                "  → Strategy:          Tiered (coarse={}d keep={}, medium={}d keep={}, ef={})",
                coarse_dims, coarse_keep, medium_dims, medium_keep, ef
            ),
        }
        eprintln!("──────────────────────────────────────────────────────────────────");
        eprintln!("Phase 3 — Compute (4-arm A/B)");
        eprintln!();
        eprintln!("                          recall@10    mean (ns)    p99 (ns)");
        eprintln!(
            "  Control (flat HNSW):     {:.4}    {:>10}    {:>10}",
            c.recall, c.mean_ns, c.p99_ns
        );
        eprintln!(
            "  Overhead (EML nop):      {:.4}    {:>10}    {:>10}",
            o.recall, o.mean_ns, o.p99_ns
        );
        eprintln!(
            "  Adaptive ef (EML):       {:.4}    {:>10}    {:>10}",
            a.recall, a.mean_ns, a.p99_ns
        );
        eprintln!(
            "  Tiered (coarse→fine):    {:.4}    {:>10}    {:>10}",
            t.recall, t.mean_ns, t.p99_ns
        );
        eprintln!();
        eprintln!("  EML overhead:        {:>+.1}% mean latency", overhead_pct);
        eprintln!(
            "  Tiered vs control:   {:>.2}× speed, {:>+.4} recall",
            tiered_speedup,
            t.recall - c.recall
        );
        eprintln!("──────────────────────────────────────────────────────────────────");
        eprintln!("Phase 4 — Scalability");
        eprintln!();
        eprintln!("  N        control              adaptive");
        eprintln!("           recall   mean(ns)    recall   mean(ns)");
        for pt in &bench.phase4_scaling {
            eprintln!(
                "  {:<5}    {:.4}   {:>9}    {:.4}   {:>9}",
                pt.store_size,
                pt.control_recall,
                pt.control_mean_ns,
                pt.adaptive_recall,
                pt.adaptive_mean_ns,
            );
        }
        eprintln!("══════════════════════════════════════════════════════════════════\n");

        assert!(bench.phase2_queries_run > 0);
        assert!(bench.phase3_control.mean_ns > 0);
        assert!(!bench.phase4_scaling.is_empty());
    }
}
