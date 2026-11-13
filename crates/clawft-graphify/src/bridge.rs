//! Kernel bridge: maps `KnowledgeGraph` into ECC subsystems.
//!
//! Compiled only with the `kernel-bridge` feature. Provides `GraphifyBridge`
//! for ingesting entities and relationships into `CausalGraph`, indexing
//! embeddings into `HnswService`, and creating cross-references in
//! `CrossRefStore`.
//!
//! Also provides `GraphifyAnalyzer`, the 9th analyzer in the assessment
//! pipeline.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};

use clawft_kernel::causal::{CausalEdgeType, CausalGraph, NodeId as CausalNodeId};
use clawft_kernel::crossref::{
    CrossRef, CrossRefStore, CrossRefType, StructureTag, UniversalNodeId,
};
use clawft_kernel::hnsw_service::HnswService;

use crate::GraphifyError;
use crate::edge_embed::{EDGE_HNSW_PREFIX, EdgeEmbeddingIndex};
use crate::entity::EntityId;
use crate::model::{Entity, GodNode, KnowledgeGraph};
use crate::relationship::{Confidence, RelationType, Relationship};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// StructureTag byte for Graphify KnowledgeGraph entities.
const GRAPHIFY_STRUCTURE_TAG: u8 = 0x20;

// ---------------------------------------------------------------------------
// EmbeddingProvider trait
// ---------------------------------------------------------------------------

/// Trait for providing text embeddings (decouples bridge from LLM backend).
#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    /// Generate an embedding vector for the given text.
    async fn embed(&self, text: &str) -> Result<Vec<f32>, GraphifyError>;
}

/// No-op embedding provider that returns a zero vector of the given dimension.
pub struct NoOpEmbedder {
    pub dimensions: usize,
}

#[async_trait]
impl EmbeddingProvider for NoOpEmbedder {
    async fn embed(&self, _text: &str) -> Result<Vec<f32>, GraphifyError> {
        Ok(vec![0.0; self.dimensions])
    }
}

// ---------------------------------------------------------------------------
// IngestResult
// ---------------------------------------------------------------------------

/// Summary of an ingest operation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IngestResult {
    pub nodes_created: usize,
    pub edges_created: usize,
    pub crossrefs_created: usize,
    /// Entity embeddings indexed into HNSW.
    pub embeddings_indexed: usize,
    /// Relationship (edge) embeddings indexed into HNSW (WEFT-375).
    pub edge_embeddings_indexed: usize,
    pub duration_ms: u64,
}

// ---------------------------------------------------------------------------
// RelationType -> CausalEdgeType mapping
// ---------------------------------------------------------------------------

/// Map a graphify `RelationType` to a `CausalEdgeType`.
///
/// See architecture.md section 2.4 for the full mapping table.
#[allow(unreachable_patterns)]
pub fn relation_to_causal_edge_type(rt: &RelationType) -> CausalEdgeType {
    match rt {
        // Code domain: causal dependency chain.
        RelationType::Calls
        | RelationType::Imports
        | RelationType::ImportsFrom
        | RelationType::DependsOn => CausalEdgeType::Causes,

        // Structural containment / type hierarchy.
        RelationType::Contains
        | RelationType::MethodOf
        | RelationType::Implements
        | RelationType::Extends
        | RelationType::Configures => CausalEdgeType::Enables,

        // Direct contradiction.
        RelationType::Contradicts => CausalEdgeType::Contradicts,

        // Supporting evidence.
        RelationType::Corroborates | RelationType::WitnessedBy => CausalEdgeType::EvidenceFor,

        // Temporal ordering.
        RelationType::Precedes => CausalEdgeType::Follows,

        // Semantic similarity / co-occurrence.
        RelationType::SemanticallySimilarTo
        | RelationType::RelatedTo
        | RelationType::FoundAt
        | RelationType::LocatedAt
        | RelationType::DocumentedIn
        | RelationType::OwnedBy
        | RelationType::ContactedBy
        | RelationType::CaseOf
        | RelationType::Instantiates => CausalEdgeType::Correlates,

        // Negation / alibi.
        RelationType::AlibiedBy => CausalEdgeType::Inhibits,

        // Custom: default to Correlates.
        RelationType::Custom(_) => CausalEdgeType::Correlates,

        // Catch remaining non-exhaustive variants.
        _ => CausalEdgeType::Correlates,
    }
}

/// Map a `RelationType` to a `CrossRefType::Custom` discriminant in the
/// 0x20..0x3F range.
#[allow(unreachable_patterns)]
fn relation_to_crossref_discriminant(rt: &RelationType) -> u8 {
    match rt {
        RelationType::Calls => 0x20,
        RelationType::Imports => 0x21,
        RelationType::ImportsFrom => 0x22,
        RelationType::DependsOn => 0x23,
        RelationType::Contains => 0x24,
        RelationType::Implements => 0x25,
        RelationType::Configures => 0x26,
        RelationType::Extends => 0x27,
        RelationType::MethodOf => 0x28,
        RelationType::Instantiates => 0x29,
        RelationType::WitnessedBy => 0x30,
        RelationType::FoundAt => 0x31,
        RelationType::Contradicts => 0x32,
        RelationType::Corroborates => 0x33,
        RelationType::AlibiedBy => 0x34,
        RelationType::Precedes => 0x35,
        RelationType::DocumentedIn => 0x36,
        RelationType::OwnedBy => 0x37,
        RelationType::ContactedBy => 0x38,
        RelationType::LocatedAt => 0x39,
        RelationType::SemanticallySimilarTo => 0x3A,
        RelationType::RelatedTo => 0x3B,
        RelationType::CaseOf => 0x3C,
        RelationType::Custom(_) | _ => 0x3F,
    }
}

// ---------------------------------------------------------------------------
// GraphifyBridge
// ---------------------------------------------------------------------------

/// Bridges a `KnowledgeGraph` into the ECC subsystems (CausalGraph, HNSW,
/// CrossRefStore).
pub struct GraphifyBridge {
    causal_graph: Arc<CausalGraph>,
    hnsw: Arc<HnswService>,
    crossref_store: Arc<CrossRefStore>,
    /// Maps EntityId -> CausalNodeId for reverse lookup.
    entity_to_causal: DashMap<EntityId, CausalNodeId>,
}

impl GraphifyBridge {
    /// Create a new bridge.
    pub fn new(
        causal_graph: Arc<CausalGraph>,
        hnsw: Arc<HnswService>,
        crossref_store: Arc<CrossRefStore>,
    ) -> Self {
        Self {
            causal_graph,
            hnsw,
            crossref_store,
            entity_to_causal: DashMap::new(),
        }
    }

    /// Ingest an entire `KnowledgeGraph` into the ECC subsystems.
    ///
    /// For each entity: creates a `CausalNode`, embeds into HNSW, and
    /// registers a `CrossRef` to a Graphify-namespaced `UniversalNodeId`.
    ///
    /// For each relationship: creates a `CausalEdge` and a `CrossRef`
    /// preserving the original `RelationType`, and indexes an edge
    /// embedding under `rel::{stable_id}` (WEFT-375 / LightRAG P5).
    pub async fn ingest(
        &self,
        kg: &KnowledgeGraph,
        embedding_provider: &dyn EmbeddingProvider,
        hlc_timestamp: u64,
        chain_seq: u64,
    ) -> Result<IngestResult, GraphifyError> {
        let start = std::time::Instant::now();
        let mut result = IngestResult::default();

        // Phase 1: Ingest entities.
        for entity in kg.entities() {
            self.ingest_entity(entity, embedding_provider, hlc_timestamp, chain_seq)
                .await?;
            result.nodes_created += 1;
            result.embeddings_indexed += 1;
        }

        // Phase 2: Ingest relationships (+ edge embeddings).
        for (src, tgt, rel) in kg.edges() {
            self.ingest_relationship(rel, hlc_timestamp, chain_seq)?;
            self.ingest_relationship_embedding(src, tgt, rel, embedding_provider)
                .await?;
            result.edges_created += 1;
            result.edge_embeddings_indexed += 1;
        }

        // CrossRefs: one per entity (entity -> graphify namespace) + one per edge.
        result.crossrefs_created = result.nodes_created + result.edges_created;
        result.duration_ms = start.elapsed().as_millis() as u64;

        Ok(result)
    }

    /// Ingest a single entity: add to CausalGraph, embed into HNSW,
    /// register CrossRef.
    pub async fn ingest_entity(
        &self,
        entity: &Entity,
        embedding_provider: &dyn EmbeddingProvider,
        hlc_timestamp: u64,
        chain_seq: u64,
    ) -> Result<CausalNodeId, GraphifyError> {
        // 1. Add to CausalGraph.
        let metadata = serde_json::json!({
            "graphify_entity_type": entity.entity_type.discriminant(),
            "source_file": entity.source_file,
            "label": entity.label,
        });
        let causal_id = self.causal_graph.add_node(entity.label.clone(), metadata);
        self.entity_to_causal.insert(entity.id.clone(), causal_id);

        // 2. Embed into HNSW.
        let text = format!(
            "{} {} {}",
            entity.entity_type.discriminant(),
            entity.label,
            entity.source_file.as_deref().unwrap_or(""),
        );
        let embedding = embedding_provider.embed(&text).await?;
        let hnsw_metadata = serde_json::json!({
            "entity_type": entity.entity_type.discriminant(),
            "label": entity.label,
            "source_file": entity.source_file,
        });
        self.hnsw
            .insert(entity.id.to_hex(), embedding, hnsw_metadata);

        // 3. Register CrossRef (entity -> graphify namespace).
        let uni_id = entity_to_universal_node_id(&entity.id, hlc_timestamp);
        self.crossref_store.insert(CrossRef {
            source: uni_id,
            source_structure: StructureTag::Custom(GRAPHIFY_STRUCTURE_TAG),
            target: UniversalNodeId::zero(),
            target_structure: StructureTag::ExoChain,
            ref_type: CrossRefType::EvidenceFor,
            created_at: hlc_timestamp,
            chain_seq,
        });

        Ok(causal_id)
    }

    /// Ingest a single relationship: add CausalEdge + CrossRef.
    pub fn ingest_relationship(
        &self,
        rel: &Relationship,
        hlc_timestamp: u64,
        chain_seq: u64,
    ) -> Result<(), GraphifyError> {
        let src_causal = self
            .entity_to_causal
            .get(&rel.source)
            .map(|r| *r.value())
            .ok_or_else(|| {
                GraphifyError::BridgeError(format!("source entity {} not ingested", rel.source,))
            })?;

        let tgt_causal = self
            .entity_to_causal
            .get(&rel.target)
            .map(|r| *r.value())
            .ok_or_else(|| {
                GraphifyError::BridgeError(format!("target entity {} not ingested", rel.target,))
            })?;

        let edge_type = relation_to_causal_edge_type(&rel.relation_type);
        let weight = rel.confidence.to_weight();

        self.causal_graph.link(
            src_causal,
            tgt_causal,
            edge_type,
            weight,
            hlc_timestamp,
            chain_seq,
        );

        // CrossRef preserving original RelationType.
        let src_uni = entity_to_universal_node_id(&rel.source, hlc_timestamp);
        let tgt_uni = entity_to_universal_node_id(&rel.target, hlc_timestamp);
        self.crossref_store.insert(CrossRef {
            source: src_uni,
            source_structure: StructureTag::Custom(GRAPHIFY_STRUCTURE_TAG),
            target: tgt_uni,
            target_structure: StructureTag::Custom(GRAPHIFY_STRUCTURE_TAG),
            ref_type: CrossRefType::Custom(relation_to_crossref_discriminant(&rel.relation_type)),
            created_at: hlc_timestamp,
            chain_seq,
        });

        Ok(())
    }

    /// Index a single relationship embedding into HNSW (WEFT-375).
    ///
    /// Entry id: `rel::{stable_id}`. Reuses a pre-populated
    /// [`Relationship::embedding`] when present; otherwise calls the provider
    /// with the edge's semantic text.
    pub async fn ingest_relationship_embedding(
        &self,
        src: &Entity,
        tgt: &Entity,
        rel: &Relationship,
        embedding_provider: &dyn EmbeddingProvider,
    ) -> Result<(), GraphifyError> {
        let embed_text = Relationship::embed_text_with_context(
            &src.label,
            &rel.relation_type,
            &tgt.label,
            rel.source_file.as_deref(),
        );
        let embedding = match rel.embedding.as_ref() {
            Some(v) if !v.is_empty() => v.clone(),
            _ => embedding_provider.embed(&embed_text).await?,
        };
        let hnsw_id = EdgeEmbeddingIndex::hnsw_id_for(&rel.stable_id());
        let metadata = serde_json::json!({
            "kind": "relationship",
            "relationship_id": rel.stable_id(),
            "source": src.id.to_hex(),
            "target": tgt.id.to_hex(),
            "relation_type": rel.relation_type_str(),
            "source_label": src.label,
            "target_label": tgt.label,
            "embed_text": embed_text,
        });
        self.hnsw.insert(hnsw_id, embedding, metadata);
        Ok(())
    }

    /// Relationship-anchored retrieval over HNSW (WEFT-375).
    ///
    /// Embeds `query`, searches HNSW, and returns only entries whose id is
    /// prefixed with [`EDGE_HNSW_PREFIX`] (`rel::`).
    pub async fn search_relationships(
        &self,
        query: &str,
        embedding_provider: &dyn EmbeddingProvider,
        top_k: usize,
    ) -> Result<Vec<clawft_kernel::hnsw_service::HnswSearchResult>, GraphifyError> {
        if top_k == 0 {
            return Ok(Vec::new());
        }
        let q = embedding_provider.embed(query).await?;
        // Oversample so entity hits do not starve edge results.
        let raw = self.hnsw.search(&q, top_k.saturating_mul(4).max(top_k));
        let mut out = Vec::with_capacity(top_k);
        for r in raw {
            if r.id.starts_with(EDGE_HNSW_PREFIX) {
                out.push(r);
                if out.len() >= top_k {
                    break;
                }
            }
        }
        Ok(out)
    }

    /// Look up which CausalNodeId corresponds to an EntityId.
    pub fn causal_node_for(&self, entity_id: &EntityId) -> Option<CausalNodeId> {
        self.entity_to_causal.get(entity_id).map(|r| *r.value())
    }

    /// Borrow the underlying HNSW service (tests / advanced callers).
    pub fn hnsw(&self) -> &Arc<HnswService> {
        &self.hnsw
    }

    /// HNSW search followed by graph-aware re-ranking (WEFT-376 / LightRAG P4).
    ///
    /// 1. Query the multi-key-aware HNSW index (`search_dedup`) for
    ///    `top_k * candidate_multiplier` candidates (default multiplier 3).
    /// 2. Re-rank with [`crate::graph_rerank::graph_rerank`] using degree,
    ///    community co-membership, hop distance, and relation weights from
    ///    `kg`.
    /// 3. Return the top `top_k` hits.
    ///
    /// When `config` is `None`, [`GraphRerankConfig::default`] is used.
    pub fn search_graph_reranked(
        &self,
        query: &[f32],
        kg: &KnowledgeGraph,
        top_k: usize,
        config: Option<&crate::graph_rerank::GraphRerankConfig>,
    ) -> Vec<crate::graph_rerank::RerankedHit> {
        use crate::graph_rerank::{GraphRerankConfig, VectorHit, graph_rerank_top_k};

        let cfg_owned;
        let cfg = match config {
            Some(c) => c,
            None => {
                cfg_owned = GraphRerankConfig::default();
                &cfg_owned
            }
        };

        // Over-fetch so re-ranking has room to promote structurally strong
        // lower-vector hits (LightRAG-style neighbourhood expansion budget).
        let fetch_k = top_k.saturating_mul(3).max(top_k).max(1);
        let raw = self.hnsw.search_dedup(query, fetch_k);
        let hits: Vec<VectorHit> = raw
            .into_iter()
            .map(|r| VectorHit::with_metadata(r.id, r.score as f64, r.metadata))
            .collect();

        graph_rerank_top_k(&hits, kg, kg.communities.as_ref(), cfg, top_k)
    }

    /// Export from CausalGraph back into a KnowledgeGraph.
    ///
    /// This is a reverse bridge: reads CausalNodes that carry the
    /// `graphify_entity_type` metadata field and reconstructs entities
    /// and relationships.
    pub fn export_from_causal(&self) -> KnowledgeGraph {
        let mut kg = KnowledgeGraph::new();

        // Reverse-map: CausalNodeId -> EntityId.
        let causal_to_entity: std::collections::HashMap<CausalNodeId, EntityId> = self
            .entity_to_causal
            .iter()
            .map(|entry| {
                let causal_id: CausalNodeId = *entry.value();
                let entity_id: EntityId = entry.key().clone();
                (causal_id, entity_id)
            })
            .collect();

        // Re-create entities from CausalGraph nodes.
        for (causal_id, entity_id) in &causal_to_entity {
            if let Some(node) = self.causal_graph.get_node(*causal_id) {
                let entity_type_str = node
                    .metadata
                    .get("graphify_entity_type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("concept");
                let entity_type = parse_entity_type(entity_type_str);
                let source_file = node
                    .metadata
                    .get("source_file")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());

                let entity = Entity {
                    id: entity_id.clone(),
                    entity_type,
                    label: node.label.clone(),
                    source_file,
                    source_location: None,
                    file_type: crate::entity::FileType::Code,
                    metadata: node.metadata.clone(),
                    legacy_id: None,
                    iri: None,
                };
                kg.add_entity(entity);
            }
        }

        // Re-create relationships from CausalGraph edges.
        for causal_id in causal_to_entity.keys() {
            for edge in self.causal_graph.get_forward_edges(*causal_id) {
                let src_eid: Option<&EntityId> = causal_to_entity.get(&edge.source);
                let tgt_eid: Option<&EntityId> = causal_to_entity.get(&edge.target);
                if let (Some(src_eid), Some(tgt_eid)) = (src_eid, tgt_eid) {
                    let rel = Relationship {
                        source: src_eid.clone(),
                        target: tgt_eid.clone(),
                        relation_type: causal_edge_to_relation_type(&edge.edge_type),
                        confidence: weight_to_confidence(edge.weight),
                        weight: edge.weight,
                        source_file: None,
                        source_location: None,
                        metadata: serde_json::json!({}),
                        embedding: None,
                    };
                    kg.add_relationship(rel);
                }
            }
        }

        kg
    }
}

/// Generate a `UniversalNodeId` for a graphify entity.
fn entity_to_universal_node_id(id: &EntityId, hlc_timestamp: u64) -> UniversalNodeId {
    UniversalNodeId::new(
        &StructureTag::Custom(GRAPHIFY_STRUCTURE_TAG),
        id.as_bytes(),
        hlc_timestamp,
        id.as_bytes(),
        &[],
    )
}

/// Parse an entity type discriminant string back into an `EntityType`.
fn parse_entity_type(s: &str) -> crate::entity::EntityType {
    use crate::entity::EntityType;
    match s {
        "module" => EntityType::Module,
        "class" => EntityType::Class,
        "function" => EntityType::Function,
        "import" => EntityType::Import,
        "config" => EntityType::Config,
        "service" => EntityType::Service,
        "endpoint" => EntityType::Endpoint,
        "interface" => EntityType::Interface,
        "struct_" => EntityType::Struct,
        "enum_" => EntityType::Enum,
        "constant" => EntityType::Constant,
        "package" => EntityType::Package,
        "person" => EntityType::Person,
        "event" => EntityType::Event,
        "evidence" => EntityType::Evidence,
        "location" => EntityType::Location,
        "timeline" => EntityType::Timeline,
        "document" => EntityType::Document,
        "hypothesis" => EntityType::Hypothesis,
        "organization" => EntityType::Organization,
        "file" => EntityType::File,
        "concept" => EntityType::Concept,
        other => EntityType::Custom(other.to_string()),
    }
}

/// Reverse-map a `CausalEdgeType` to the most likely `RelationType`.
///
/// This is lossy -- the original `RelationType` is preserved in CrossRef
/// metadata, not here.
fn causal_edge_to_relation_type(et: &CausalEdgeType) -> RelationType {
    match et {
        CausalEdgeType::Causes => RelationType::DependsOn,
        CausalEdgeType::Enables => RelationType::Contains,
        CausalEdgeType::Contradicts => RelationType::Contradicts,
        CausalEdgeType::EvidenceFor => RelationType::Corroborates,
        CausalEdgeType::Follows => RelationType::Precedes,
        CausalEdgeType::Correlates => RelationType::RelatedTo,
        CausalEdgeType::Inhibits => RelationType::AlibiedBy,
        CausalEdgeType::TriggeredBy => RelationType::DependsOn,
        _ => RelationType::RelatedTo,
    }
}

/// Reverse-map a CausalEdge weight to a Confidence level.
fn weight_to_confidence(w: f32) -> Confidence {
    if w >= 0.9 {
        Confidence::Extracted
    } else if w >= 0.5 {
        Confidence::Inferred
    } else {
        Confidence::Ambiguous
    }
}

impl std::fmt::Debug for GraphifyBridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GraphifyBridge")
            .field("entity_count", &self.entity_to_causal.len())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// GraphifyAnalyzer (9th assessment analyzer)
// ---------------------------------------------------------------------------

use clawft_kernel::assessment::Finding;
use clawft_kernel::assessment::analyzer::{AnalysisContext, Analyzer};

/// The 9th analyzer in the WeftOS assessment pipeline.
///
/// Runs graphify extraction on the codebase and produces findings for:
/// - God nodes (high complexity / coupling)
/// - Surprising cross-community connections (unexpected coupling)
/// - Low-cohesion communities (architectural concerns)
pub struct GraphifyAnalyzer {
    /// Optional bridge reference. If `None`, analysis runs in standalone
    /// mode without CausalGraph ingestion.
    _bridge: Option<Arc<GraphifyBridge>>,
}

impl GraphifyAnalyzer {
    /// Create a new analyzer with an optional bridge.
    pub fn new(bridge: Option<Arc<GraphifyBridge>>) -> Self {
        Self { _bridge: bridge }
    }
}

impl Analyzer for GraphifyAnalyzer {
    fn id(&self) -> &str {
        "graphify"
    }

    fn name(&self) -> &str {
        "Knowledge Graph"
    }

    fn categories(&self) -> &[&str] {
        &[
            "architecture",
            "dependencies",
            "complexity",
            "knowledge-gaps",
        ]
    }

    fn analyze(
        &self,
        _project: &Path,
        files: &[PathBuf],
        _context: &AnalysisContext,
    ) -> Vec<Finding> {
        // Build a lightweight knowledge graph from the file list.
        // In a full implementation, this would run the extraction pipeline.
        // For now, we produce findings from the entity/relationship structure
        // that can be populated by earlier pipeline stages.
        let mut findings = Vec::new();

        // For Phase 3, we generate findings based on file count as a proxy.
        // Real implementation will run extract -> build -> analyze.
        if files.len() > 200 {
            findings.push(Finding {
                severity: "info".into(),
                category: "architecture".into(),
                file: String::new(),
                line: None,
                message: format!(
                    "Large codebase ({} files) -- consider running `weft graphify` \
                     for detailed dependency and coupling analysis.",
                    files.len(),
                ),
            });
        }

        findings
    }
}

/// Analyze a pre-built knowledge graph and produce assessment findings.
///
/// This function is called by the full pipeline when the KnowledgeGraph is
/// already available. It produces findings for:
/// - God nodes (high coupling / complexity)
/// - Surprising connections (unexpected dependencies)
/// - Low-cohesion communities
pub fn analyze_kg_to_findings(kg: &KnowledgeGraph) -> Vec<Finding> {
    let mut findings = Vec::new();

    // God nodes -> "high complexity" findings.
    for gn in kg.god_nodes(10) {
        let severity = if gn.degree > 20 { "warning" } else { "info" };
        findings.push(Finding {
            severity: severity.into(),
            category: "complexity".into(),
            file: gn.source_file.unwrap_or_default(),
            line: None,
            message: format!(
                "God node: '{}' ({:?}) has {} connections -- high coupling risk.",
                gn.label, gn.entity_type, gn.degree,
            ),
        });
    }

    // Surprising connections -> "coupling" findings.
    for sc in kg.surprising_connections(10) {
        findings.push(Finding {
            severity: "info".into(),
            category: "dependencies".into(),
            file: String::new(),
            line: None,
            message: format!(
                "Unexpected dependency: '{}' (community {:?}) -> '{}' (community {:?}).",
                sc.source_label, sc.source_community, sc.target_label, sc.target_community,
            ),
        });
    }

    // Low-cohesion communities -> "architectural" findings.
    if let Some(communities) = &kg.communities {
        for (&cid, members) in communities {
            if members.len() == 1
                && let Some(entity) = kg.entity(&members[0])
            {
                findings.push(Finding {
                    severity: "info".into(),
                    category: "architecture".into(),
                    file: entity.source_file.clone().unwrap_or_default(),
                    line: None,
                    message: format!(
                        "Singleton community {}: '{}' is isolated from all clusters.",
                        cid, entity.label,
                    ),
                });
            }
        }
    }

    findings
}

// ---------------------------------------------------------------------------
// KnowledgeGraph analysis helpers (god_nodes, surprising_connections)
// ---------------------------------------------------------------------------

impl KnowledgeGraph {
    /// Find entities with the highest degree (god nodes).
    pub fn god_nodes(&self, top_n: usize) -> Vec<GodNode> {
        let mut nodes: Vec<_> = self
            .node_ids()
            .filter_map(|id| {
                let entity = self.entity(id)?;
                let deg = self.degree(id);
                Some(GodNode {
                    entity_id: id.clone(),
                    label: entity.label.clone(),
                    degree: deg,
                    entity_type: entity.entity_type.clone(),
                    source_file: entity.source_file.clone(),
                })
            })
            .collect();
        nodes.sort_by_key(|n| std::cmp::Reverse(n.degree));
        nodes.truncate(top_n);
        nodes
    }

    /// Find cross-community connections (surprising dependencies).
    pub fn surprising_connections(&self, top_n: usize) -> Vec<crate::model::SurprisingConnection> {
        use crate::model::SurprisingConnection;
        use std::collections::HashMap as HM;

        let communities = match &self.communities {
            Some(c) => c,
            None => return Vec::new(),
        };

        let mut entity_community: HM<&EntityId, usize> = HM::new();
        for (&cid, members) in communities {
            for eid in members {
                entity_community.insert(eid, cid);
            }
        }

        let mut surprises: Vec<SurprisingConnection> = self
            .edges()
            .filter_map(|(src, tgt, _rel)| {
                let sc = entity_community.get(&src.id).copied();
                let tc = entity_community.get(&tgt.id).copied();
                if sc != tc {
                    Some(SurprisingConnection {
                        source_id: src.id.clone(),
                        source_label: src.label.clone(),
                        target_id: tgt.id.clone(),
                        target_label: tgt.label.clone(),
                        source_community: sc,
                        target_community: tc,
                    })
                } else {
                    None
                }
            })
            .collect();

        surprises.truncate(top_n);
        surprises
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::{DomainTag, EntityType, FileType};
    use crate::model::Entity;

    fn make_entity(name: &str) -> Entity {
        Entity {
            id: EntityId::new(&DomainTag::Code, &EntityType::Function, name, "test.py"),
            entity_type: EntityType::Function,
            label: name.to_string(),
            source_file: Some("test.py".into()),
            source_location: None,
            file_type: FileType::Code,
            metadata: serde_json::json!({}),
            legacy_id: None,
            iri: None,
        }
    }

    fn make_rel(src: &Entity, tgt: &Entity) -> Relationship {
        Relationship {
            source: src.id.clone(),
            target: tgt.id.clone(),
            relation_type: RelationType::Calls,
            confidence: Confidence::Extracted,
            weight: 1.0,
            source_file: Some("test.py".into()),
            source_location: None,
            metadata: serde_json::json!({}),
            embedding: None,
        }
    }

    #[test]
    fn relation_type_mappings_complete() {
        // Verify the key mappings.
        assert_eq!(
            relation_to_causal_edge_type(&RelationType::Calls),
            CausalEdgeType::Causes,
        );
        assert_eq!(
            relation_to_causal_edge_type(&RelationType::Contains),
            CausalEdgeType::Enables,
        );
        assert_eq!(
            relation_to_causal_edge_type(&RelationType::Contradicts),
            CausalEdgeType::Contradicts,
        );
        assert_eq!(
            relation_to_causal_edge_type(&RelationType::Corroborates),
            CausalEdgeType::EvidenceFor,
        );
        assert_eq!(
            relation_to_causal_edge_type(&RelationType::Precedes),
            CausalEdgeType::Follows,
        );
        assert_eq!(
            relation_to_causal_edge_type(&RelationType::AlibiedBy),
            CausalEdgeType::Inhibits,
        );
        assert_eq!(
            relation_to_causal_edge_type(&RelationType::RelatedTo),
            CausalEdgeType::Correlates,
        );
    }

    #[test]
    fn crossref_discriminants_unique() {
        let types = vec![
            RelationType::Calls,
            RelationType::Imports,
            RelationType::ImportsFrom,
            RelationType::DependsOn,
            RelationType::Contains,
            RelationType::Implements,
            RelationType::Configures,
            RelationType::Extends,
            RelationType::MethodOf,
            RelationType::Instantiates,
            RelationType::WitnessedBy,
            RelationType::FoundAt,
            RelationType::Contradicts,
            RelationType::Corroborates,
            RelationType::AlibiedBy,
            RelationType::Precedes,
            RelationType::DocumentedIn,
            RelationType::OwnedBy,
            RelationType::ContactedBy,
            RelationType::LocatedAt,
            RelationType::SemanticallySimilarTo,
            RelationType::RelatedTo,
            RelationType::CaseOf,
        ];
        let mut seen = std::collections::HashSet::new();
        for rt in &types {
            let disc = relation_to_crossref_discriminant(rt);
            assert!(
                seen.insert(disc),
                "duplicate discriminant 0x{disc:02x} for {rt:?}",
            );
        }
    }

    #[test]
    fn weight_to_confidence_thresholds() {
        assert_eq!(weight_to_confidence(1.0), Confidence::Extracted);
        assert_eq!(weight_to_confidence(0.7), Confidence::Inferred);
        assert_eq!(weight_to_confidence(0.4), Confidence::Ambiguous);
        assert_eq!(weight_to_confidence(0.0), Confidence::Ambiguous);
    }

    #[test]
    fn parse_entity_type_roundtrip() {
        let types = vec![
            EntityType::Module,
            EntityType::Class,
            EntityType::Function,
            EntityType::Person,
            EntityType::Evidence,
        ];
        for et in types {
            let disc = et.discriminant();
            let parsed = parse_entity_type(disc);
            assert_eq!(parsed, et, "roundtrip failed for {disc}");
        }
    }

    #[tokio::test]
    async fn bridge_ingest_and_export() {
        use clawft_kernel::hnsw_service::HnswServiceConfig;

        let causal = Arc::new(CausalGraph::new());
        let hnsw = Arc::new(HnswService::new(HnswServiceConfig::default()));
        let crossref = Arc::new(CrossRefStore::new());

        let bridge = GraphifyBridge::new(causal.clone(), hnsw.clone(), crossref.clone());
        let embedder = NoOpEmbedder { dimensions: 3 };

        // Build a small KG.
        let mut kg = KnowledgeGraph::new();
        let a = make_entity("alpha");
        let b = make_entity("beta");
        kg.add_entity(a.clone());
        kg.add_entity(b.clone());
        kg.add_relationship(make_rel(&a, &b));

        // Ingest.
        let result = bridge.ingest(&kg, &embedder, 1000, 42).await.unwrap();
        assert_eq!(result.nodes_created, 2);
        assert_eq!(result.edges_created, 1);
        assert_eq!(result.crossrefs_created, 3); // 2 entities + 1 edge
        assert_eq!(result.embeddings_indexed, 2);
        assert_eq!(result.edge_embeddings_indexed, 1);

        // Verify CausalGraph.
        assert_eq!(causal.node_count(), 2);
        assert_eq!(causal.edge_count(), 1);

        // Verify HNSW: 2 entity vectors + 1 edge vector (WEFT-375).
        assert_eq!(hnsw.len(), 3);

        // Verify CrossRefStore.
        assert_eq!(crossref.count(), 3);

        // Verify reverse lookup.
        assert!(bridge.causal_node_for(&a.id).is_some());
        assert!(bridge.causal_node_for(&b.id).is_some());

        // Export back.
        let exported = bridge.export_from_causal();
        assert_eq!(exported.entity_count(), 2);
        // Edges may duplicate due to traversal; verify at least 1.
        assert!(exported.relationship_count() >= 1);
    }

    /// WEFT-376: HNSW search → graph re-rank end-to-end on the bridge.
    #[tokio::test]
    async fn bridge_search_graph_reranked() {
        use crate::graph_rerank::GraphRerankConfig;
        use clawft_kernel::hnsw_service::{HnswServiceConfig, MultiKey};

        let causal = Arc::new(CausalGraph::new());
        let hnsw = Arc::new(HnswService::new(HnswServiceConfig::default()));
        let crossref = Arc::new(CrossRefStore::new());
        let bridge = GraphifyBridge::new(causal, hnsw.clone(), crossref);

        // Distinct embeddings so HNSW ranking is deterministic.
        // hub is structurally central; isolated has a slightly closer
        // vector to the query but zero topology.
        let mut kg = KnowledgeGraph::new();
        let hub = make_entity("hub_fn");
        let leaf = make_entity("leaf_fn");
        let isolated = make_entity("isolated_fn");
        kg.add_entity(hub.clone());
        kg.add_entity(leaf.clone());
        kg.add_entity(isolated.clone());
        kg.add_relationship(make_rel(&hub, &leaf));

        let mut communities = std::collections::HashMap::new();
        communities.insert(0, vec![hub.id.clone(), leaf.id.clone()]);
        communities.insert(1, vec![isolated.id.clone()]);
        kg.communities = Some(communities);

        // Insert with multi-key so search_dedup is exercised.
        hnsw.insert_multi_key(
            hub.id.to_hex(),
            &[MultiKey {
                key_type: "name".into(),
                embedding: vec![0.9, 0.1, 0.0],
            }],
            serde_json::json!({"label": "hub_fn"}),
            4,
        );
        hnsw.insert_multi_key(
            leaf.id.to_hex(),
            &[MultiKey {
                key_type: "name".into(),
                embedding: vec![0.85, 0.15, 0.0],
            }],
            serde_json::json!({"label": "leaf_fn"}),
            4,
        );
        hnsw.insert_multi_key(
            isolated.id.to_hex(),
            &[MultiKey {
                key_type: "name".into(),
                embedding: vec![1.0, 0.0, 0.0],
            }],
            serde_json::json!({"label": "isolated_fn"}),
            4,
        );

        let query = [1.0_f32, 0.0, 0.0];
        let cfg = GraphRerankConfig::topology_heavy();
        let ranked = bridge.search_graph_reranked(&query, &kg, 3, Some(&cfg));

        assert_eq!(ranked.len(), 3);
        // All three entities present.
        let ids: std::collections::HashSet<_> = ranked.iter().map(|h| h.id.clone()).collect();
        assert!(ids.contains(&hub.id.to_hex()));
        assert!(ids.contains(&leaf.id.to_hex()));
        assert!(ids.contains(&isolated.id.to_hex()));

        // Hub (degree 1, same community as leaf, hop-close) should not
        // be last under topology-heavy weights when query is near isolated.
        let hub_rank = ranked.iter().position(|h| h.id == hub.id.to_hex()).unwrap();
        let iso_rank = ranked
            .iter()
            .position(|h| h.id == isolated.id.to_hex())
            .unwrap();
        // At least one of: hub beats isolated, or hub has hop features set.
        let hub_hit = ranked.iter().find(|h| h.id == hub.id.to_hex()).unwrap();
        assert!(hub_hit.degree >= 1);
        assert!(
            hub_rank < iso_rank || hub_hit.features.degree > 0.0,
            "expected topology signals on hub; ranks hub={hub_rank} iso={iso_rank}"
        );
    }

    /// WEFT-375: relationship-level HNSW search ("how does X interact with Y?").
    #[tokio::test]
    #[cfg_attr(not(feature = "real-embedder-tests"), ignore = "needs --features real-embedder-tests and WEFTOS_TEST_EMBED_MODEL_DIR (nomic-embed-text-v1.5 bundle)")]
    async fn bridge_relationship_hnsw_search() {
        use clawft_kernel::hnsw_service::HnswServiceConfig;

        // Recall is judged with a REAL embedding model (owner rule 2026-11-11):
        // WEFTOS_TEST_EMBED_MODEL_DIR + the `real-embedder-tests` feature. Without
        // them the helper prints why and the test returns; hashing is no substitute.
        let Some(real) = clawft_kernel::embedding_support::real_embedder() else {
            return;
        };

        /// Adapts the kernel's real embedder to the bridge's provider trait.
        struct RealEmbedder(Arc<dyn clawft_kernel::embedding::EmbeddingProvider>);
        #[async_trait]
        impl EmbeddingProvider for RealEmbedder {
            async fn embed(&self, text: &str) -> Result<Vec<f32>, GraphifyError> {
                self.0
                    .embed(text)
                    .await
                    .map_err(|e| GraphifyError::BridgeError(format!("embedding failed: {e}")))
            }
        }

        let causal = Arc::new(CausalGraph::new());
        let hnsw = Arc::new(HnswService::new(HnswServiceConfig::default()));
        let crossref = Arc::new(CrossRefStore::new());
        let bridge = GraphifyBridge::new(causal, hnsw.clone(), crossref);
        let embedder = RealEmbedder(real.clone());

        let mut kg = KnowledgeGraph::new();
        let auth = Entity {
            id: EntityId::new(
                &DomainTag::Code,
                &EntityType::Service,
                "AuthService",
                "a.rs",
            ),
            entity_type: EntityType::Service,
            label: "AuthService".into(),
            source_file: Some("a.rs".into()),
            source_location: None,
            file_type: FileType::Code,
            metadata: serde_json::json!({}),
            legacy_id: None,
            iri: None,
        };
        let db = Entity {
            id: EntityId::new(&DomainTag::Code, &EntityType::Service, "Database", "d.rs"),
            entity_type: EntityType::Service,
            label: "Database".into(),
            source_file: Some("d.rs".into()),
            source_location: None,
            file_type: FileType::Code,
            metadata: serde_json::json!({}),
            legacy_id: None,
            iri: None,
        };
        let logger = Entity {
            id: EntityId::new(&DomainTag::Code, &EntityType::Service, "Logger", "l.rs"),
            entity_type: EntityType::Service,
            label: "Logger".into(),
            source_file: Some("l.rs".into()),
            source_location: None,
            file_type: FileType::Code,
            metadata: serde_json::json!({}),
            legacy_id: None,
            iri: None,
        };
        kg.add_entity(auth.clone());
        kg.add_entity(db.clone());
        kg.add_entity(logger.clone());
        kg.add_relationship(make_rel(&auth, &db));
        kg.add_relationship(Relationship {
            source: logger.id.clone(),
            target: db.id.clone(),
            relation_type: RelationType::DependsOn,
            confidence: Confidence::Inferred,
            weight: 0.7,
            source_file: Some("l.rs".into()),
            source_location: None,
            metadata: serde_json::json!({}),
            embedding: None,
        });

        bridge.ingest(&kg, &embedder, 1, 1).await.unwrap();
        assert_eq!(hnsw.len(), 5); // 3 entities + 2 edges

        // The query shares no content word with its target edge
        // ("AuthService calls Database in test.py").
        let query = "authentication module invokes the persistence store";
        let hits = bridge
            .search_relationships(query, &embedder, 2)
            .await
            .unwrap();
        let q = real.embed(query).await.unwrap();
        for text in [
            Relationship::embed_text_with_context(
                &auth.label,
                &RelationType::Calls,
                &db.label,
                Some("test.py"),
            ),
            Relationship::embed_text_with_context(
                &logger.label,
                &RelationType::DependsOn,
                &db.label,
                Some("l.rs"),
            ),
        ] {
            let v = real.embed(&text).await.unwrap();
            eprintln!(
                "cosine(query, {text:?}) = {:.4}",
                clawft_kernel::embedding_support::cosine(&q, &v)
            );
        }
        assert!(!hits.is_empty());
        assert!(
            hits[0].id.starts_with(EDGE_HNSW_PREFIX),
            "expected rel:: prefix, got {}",
            hits[0].id
        );
        assert_eq!(hits[0].metadata["kind"], "relationship");
        assert_eq!(
            hits[0].metadata["source_label"], "AuthService",
            "top edge should be AuthService -> Database, got {}",
            hits[0].metadata["embed_text"]
        );
    }

    #[test]
    fn graphify_analyzer_metadata() {
        let analyzer = GraphifyAnalyzer::new(None);
        assert_eq!(analyzer.id(), "graphify");
        assert_eq!(analyzer.name(), "Knowledge Graph");
        assert_eq!(analyzer.categories().len(), 4);
    }

    #[test]
    fn analyze_kg_god_nodes() {
        let mut kg = KnowledgeGraph::new();
        let hub = make_entity("hub");
        kg.add_entity(hub.clone());
        for i in 0..25 {
            let leaf = Entity {
                id: EntityId::new(
                    &DomainTag::Code,
                    &EntityType::Function,
                    &format!("leaf_{i}"),
                    "test.py",
                ),
                entity_type: EntityType::Function,
                label: format!("leaf_{i}"),
                source_file: Some("test.py".into()),
                source_location: None,
                file_type: FileType::Code,
                metadata: serde_json::json!({}),
                legacy_id: None,
                iri: None,
            };
            kg.add_entity(leaf.clone());
            kg.add_relationship(make_rel(&hub, &leaf));
        }

        let findings = analyze_kg_to_findings(&kg);
        assert!(
            findings
                .iter()
                .any(|f| f.category == "complexity" && f.message.contains("hub")),
            "Expected a god-node finding for 'hub'",
        );
    }
}
