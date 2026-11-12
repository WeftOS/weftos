//! `clawft-graphify` -- Knowledge graph builder for WeftOS.
//!
//! Extracts entities and relationships from source code (via tree-sitter AST)
//! and documents (via LLM semantic extraction), clusters them into communities,
//! analyzes structural patterns, and exports interactive visualizations.
//!
//! # Domains
//!
//! - **Code Assessment:** Maps modules, classes, functions, imports, call
//!   graphs, and inferred dependencies into a queryable knowledge graph.
//!
//! - **Forensic Analysis:** Extracts persons, events, evidence, locations,
//!   timelines and their relationships from investigative documents.

// Re-export core types at the crate root for convenience.
pub mod alignment;
pub mod analyze;
// WEFT-370 / GRAPH-041: extraction + graph_ops benchmark harness.
pub mod bench;
pub mod build;
pub mod cache;
pub mod cluster;
/// WEFT-516: SASE (k-order SGC + RFF) spectral clustering internals.
pub mod cluster_sase;
pub mod conversation;
pub mod domain;
// WEFT-375 / LightRAG P5: edge embeddings for relationship-level queries.
pub mod edge_embed;
// WEFT-517 / LightRAG: dual-level keyword retrieval (Guo et al. 2410.05779).
pub mod eml_models;
pub mod entity;
pub mod export;
pub mod extract;
pub mod lightrag;
// WEFT-376 / LightRAG P4: graph-aware re-ranking of HNSW/vector hits.
pub mod graph_rerank;
pub mod hooks;
/// WEFT-377: motif-based multi-node hyperedge discovery.
pub mod hyperedge;
pub mod ingest;
#[cfg(feature = "rdf-ingest")]
pub use ingest::rdf;
pub mod layout;
pub mod model;
pub mod pipeline;
pub mod relationship;
pub mod report;
pub mod summary;
pub mod topology;
pub mod topology_infer;
pub mod validation;
pub mod vault;
pub mod watch;

// WEFT-369: MCP server exposing graphify query/ingest/export/diff tools.
#[cfg(feature = "mcp")]
pub mod mcp;

#[cfg(feature = "kernel-bridge")]
pub mod bridge;

#[cfg(feature = "semantic-extract")]
pub mod semantic_extract;

#[cfg(feature = "vision-extract")]
pub mod vision_extract;

pub use alignment::{
    AlignmentConfig, AlignmentMethod, AlignmentRefiner, ApplyResult, EaAgent, EmbeddingLookup,
    EntityAlignment, HeuristicRefiner, MapEmbeddingLookup, MergeProposal, PreferredSide,
    ProposalStatus, ScoreBreakdown, align_entities, propose_merges,
};
pub use build::MergeStats;
pub use edge_embed::{
    DEFAULT_EDGE_EMBED_DIMS, EDGE_HNSW_PREFIX, EdgeEmbeddingIndex, EdgeSearchResult,
    populate_edge_embeddings, query_relationships, query_relationships_dims,
    relationship_search_keys,
};
pub use entity::{DomainTag, EntityId, EntityType, FileType};
pub use graph_rerank::{
    GraphRerankConfig, RerankFeatures, RerankedHit, VectorHit, graph_rerank, graph_rerank_top_k,
};
pub use hyperedge::{
    HyperedgeDiscoveryConfig, apply_discovered_hyperedges, discover_hyperedges,
    discover_hyperedges_with_config, merge_hyperedges,
};
pub use lightrag::{
    DualLevelHit, DualLevelResult, TokenCostEstimate, dual_level_retrieve, split_levels,
    token_cost_estimate, tokenize_keywords,
};
pub use model::{
    DetectionResult, Entity, ExtractionResult, ExtractionStats, GodNode, GraphDiff, Hyperedge,
    KnowledgeGraph, SuggestedQuestion, SurprisingConnection,
};
pub use relationship::{Confidence, RelationType, Relationship};
// WEFT-378: vault domain hyperedges + SUGGEST → ratify → CRDT pipeline.
pub use vault::{
    ApplyStats, CrdtDelta, CrdtEntry, CrdtError, CrdtHyperedgeStore, HyperedgeKind,
    HyperedgeProposal, HyperedgeSuggestConfig, MergeStats as CrdtMergeStats, ProposalLifecycle,
    RatifyView, VAULT_DOMAIN, VaultHyperedgePipeline, suggest_hyperedges, suggest_ratify_apply,
    vault_entity_id,
};

// ---------------------------------------------------------------------------
// GraphifyError
// ---------------------------------------------------------------------------

/// Top-level error type for the graphify crate.
#[derive(Debug, thiserror::Error)]
pub enum GraphifyError {
    /// AST or semantic extraction failed for a file.
    #[error("extraction failed: {0}")]
    ExtractionFailed(String),

    /// A tree-sitter grammar is not available (feature not enabled).
    #[error("grammar not available: {0}")]
    GrammarNotAvailable(String),

    /// LLM call failed during semantic extraction.
    #[error("LLM error: {0}")]
    LlmError(String),

    /// Cache read/write/GC failure.
    #[error("cache error: {0}")]
    CacheError(String),

    /// Graph assembly (build) failure.
    #[error("build error: {0}")]
    BuildError(String),

    /// Kernel bridge error (CausalGraph/HNSW/CrossRef).
    #[error("bridge error: {0}")]
    BridgeError(String),

    /// File detection / classification error.
    #[error("detection error: {0}")]
    DetectionError(String),

    /// Export serialization or I/O error.
    #[error("export error: {0}")]
    ExportError(String),

    /// Schema validation error.
    #[error("validation error: {0}")]
    ValidationError(String),

    /// URL ingestion error.
    #[error("ingest error: {0}")]
    IngestError(String),

    /// File watcher error.
    #[error("watch error: {0}")]
    WatchError(String),

    /// Git hook installation/uninstallation error.
    #[error("hook error: {0}")]
    HookError(String),

    /// Pipeline orchestration error.
    #[error("pipeline error: {0}")]
    Pipeline(String),
}

impl From<std::io::Error> for GraphifyError {
    fn from(err: std::io::Error) -> Self {
        Self::CacheError(err.to_string())
    }
}

impl From<serde_json::Error> for GraphifyError {
    fn from(err: serde_json::Error) -> Self {
        Self::ValidationError(err.to_string())
    }
}
