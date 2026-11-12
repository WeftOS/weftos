//! Graph-aware re-ranking of vector / HNSW hits (WEFT-376 / LightRAG P4).
//!
//! After HNSW (or any primary scorer) returns nearest neighbors, re-rank
//! using knowledge-graph topology so nodes that are both semantically
//! similar **and** structurally connected rank higher than pure-similarity
//! isolates.
//!
//! Features (all normalized to roughly `[0, 1]`):
//! - **vector** — primary score (cosine / keyword / fusion prior)
//! - **degree** — log-normalized connectivity (centrality proxy)
//! - **community** — shares community with top seed hits
//! - **hop** — inverse hop distance to nearest seed (neighbors, multi-hop)
//! - **relation** — max incident edge confidence weight toward the seed set
//!
//! Pure graphify logic (no kernel dependency). The kernel bridge wraps
//! HNSW search + this re-ranker; the CLI / MCP query paths can feed any
//! scored hit list through the same API.

use crate::analyze::node_community_map;
use crate::entity::EntityId;
use crate::model::KnowledgeGraph;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// Tunable weights and knobs for graph-aware re-ranking.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphRerankConfig {
    /// Weight for the primary (vector / keyword) score.
    pub w_vector: f64,
    /// Weight for log-normalized degree centrality.
    pub w_degree: f64,
    /// Weight for community co-membership with seeds.
    pub w_community: f64,
    /// Weight for hop-distance proximity to seeds.
    pub w_hop: f64,
    /// Weight for relation-confidence edges toward the seed set.
    pub w_relation: f64,
    /// How many top primary hits become topology seeds (default 3).
    pub seed_k: usize,
    /// Max BFS depth when measuring hop distance (default 3).
    pub max_hops: usize,
}

impl Default for GraphRerankConfig {
    fn default() -> Self {
        Self {
            // Vector still dominates; topology breaks ties and lifts hubs
            // that sit near the semantic match cluster.
            w_vector: 0.45,
            w_degree: 0.15,
            w_community: 0.15,
            w_hop: 0.15,
            w_relation: 0.10,
            seed_k: 3,
            max_hops: 3,
        }
    }
}

impl GraphRerankConfig {
    /// All mass on the primary score (identity re-rank).
    pub fn vector_only() -> Self {
        Self {
            w_vector: 1.0,
            w_degree: 0.0,
            w_community: 0.0,
            w_hop: 0.0,
            w_relation: 0.0,
            seed_k: 3,
            max_hops: 3,
        }
    }

    /// Emphasize graph structure (useful when vector scores are flat).
    pub fn topology_heavy() -> Self {
        Self {
            w_vector: 0.30,
            w_degree: 0.20,
            w_community: 0.20,
            w_hop: 0.20,
            w_relation: 0.10,
            seed_k: 3,
            max_hops: 3,
        }
    }
}

// ---------------------------------------------------------------------------
// Hits
// ---------------------------------------------------------------------------

/// One primary (vector / keyword) hit before graph re-ranking.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VectorHit {
    /// Entity id (hex when coming from HNSW, or any stable string key).
    pub id: String,
    /// Primary score — higher is better (cosine similarity, keyword score, …).
    pub score: f64,
    /// Optional free-form metadata (HNSW payload, labels, etc.).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

impl VectorHit {
    /// Construct a hit with no metadata.
    pub fn new(id: impl Into<String>, score: f64) -> Self {
        Self {
            id: id.into(),
            score,
            metadata: None,
        }
    }

    /// Construct a hit with metadata.
    pub fn with_metadata(id: impl Into<String>, score: f64, metadata: serde_json::Value) -> Self {
        Self {
            id: id.into(),
            score,
            metadata: Some(metadata),
        }
    }
}

/// Per-feature breakdown for a re-ranked hit (debugging / UX explanations).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RerankFeatures {
    pub vector: f64,
    pub degree: f64,
    pub community: f64,
    pub hop: f64,
    pub relation: f64,
}

/// One hit after graph-aware re-ranking.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RerankedHit {
    /// Entity id (same key as the input hit).
    pub id: String,
    /// Final fused score (higher is better).
    pub score: f64,
    /// Primary score before re-ranking.
    pub primary_score: f64,
    /// Feature breakdown used for the fused score.
    pub features: RerankFeatures,
    /// Hop distance to nearest seed (`None` if unreachable within max_hops).
    pub hop_distance: Option<usize>,
    /// Degree in the knowledge graph (0 if entity not found).
    pub degree: usize,
    /// Community id when known.
    pub community_id: Option<usize>,
    /// Optional metadata carried from the primary hit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Re-rank primary hits using graph topology.
///
/// - Preserves the input set (no invent / no drop of ids present in `hits`).
/// - Stable on equal scores (primary order wins ties).
/// - Hits whose id cannot be resolved to an [`EntityId`] still participate
///   with zero topology features (vector-only contribution).
///
/// `communities` is optional; when `None` and the KG has `communities`
/// populated, those are used. When both are absent, community score is 0.
pub fn graph_rerank(
    hits: &[VectorHit],
    kg: &KnowledgeGraph,
    communities: Option<&HashMap<usize, Vec<EntityId>>>,
    config: &GraphRerankConfig,
) -> Vec<RerankedHit> {
    if hits.is_empty() {
        return Vec::new();
    }

    // Resolve string ids → EntityId (hex preferred; fall back to legacy hash).
    let resolved: Vec<(usize, Option<EntityId>)> = hits
        .iter()
        .enumerate()
        .map(|(i, h)| (i, resolve_entity_id(&h.id, kg)))
        .collect();

    let node_comm = build_node_community_map(kg, communities);

    // Seeds: top-K by primary score (stable on ties via original index).
    let mut seed_order: Vec<usize> = (0..hits.len()).collect();
    seed_order.sort_by(|&a, &b| {
        hits[b]
            .score
            .partial_cmp(&hits[a].score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.cmp(&b))
    });
    let seed_k = config.seed_k.max(1).min(hits.len());
    let seed_indices: Vec<usize> = seed_order.into_iter().take(seed_k).collect();
    let seed_entities: HashSet<EntityId> = seed_indices
        .iter()
        .filter_map(|&i| resolved[i].1.clone())
        .collect();
    let seed_communities: HashSet<usize> = seed_entities
        .iter()
        .filter_map(|id| node_comm.get(id).copied())
        .collect();

    // Max primary score for normalization.
    let max_primary = hits
        .iter()
        .map(|h| h.score)
        .fold(0.0_f64, f64::max)
        .max(1e-12);

    // Max degree among candidates (and a floor so isolated graphs stay 0).
    let degrees: Vec<usize> = resolved
        .iter()
        .map(|(_, eid)| eid.as_ref().map(|id| kg.degree(id)).unwrap_or(0))
        .collect();
    let max_degree = degrees.iter().copied().max().unwrap_or(0).max(1);

    // Hop distances from the seed set (BFS undirected).
    let hop_map = bfs_hops(kg, &seed_entities, config.max_hops);

    // Relation weight toward seed set (1-hop max confidence).
    let relation_map = seed_relation_weights(kg, &seed_entities);

    let mut out: Vec<RerankedHit> = hits
        .iter()
        .enumerate()
        .map(|(i, hit)| {
            let eid = resolved[i].1.as_ref();
            let degree = degrees[i];
            let degree_score = (1.0 + degree as f64).ln() / (1.0 + max_degree as f64).ln();

            let community_score = match eid.and_then(|id| node_comm.get(id).copied()) {
                Some(cid) if seed_communities.contains(&cid) => 1.0,
                _ => 0.0,
            };

            let hop_distance = eid.and_then(|id| hop_map.get(id).copied());
            let hop_score = hop_to_score(hop_distance, config.max_hops);

            let relation_score = eid
                .and_then(|id| relation_map.get(id).copied())
                .unwrap_or(0.0);

            let vector_score = (hit.score / max_primary).clamp(0.0, 1.0);

            let features = RerankFeatures {
                vector: vector_score,
                degree: degree_score,
                community: community_score,
                hop: hop_score,
                relation: relation_score,
            };

            let score = config.w_vector * features.vector
                + config.w_degree * features.degree
                + config.w_community * features.community
                + config.w_hop * features.hop
                + config.w_relation * features.relation;

            RerankedHit {
                id: hit.id.clone(),
                score,
                primary_score: hit.score,
                features,
                hop_distance,
                degree,
                community_id: eid.and_then(|id| node_comm.get(id).copied()),
                metadata: hit.metadata.clone(),
            }
        })
        .collect();

    // Sort by fused score desc; stable on ties via original primary order.
    out.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                b.primary_score
                    .partial_cmp(&a.primary_score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
    });

    out
}

/// Convenience: re-rank and return only the top `k` hits.
pub fn graph_rerank_top_k(
    hits: &[VectorHit],
    kg: &KnowledgeGraph,
    communities: Option<&HashMap<usize, Vec<EntityId>>>,
    config: &GraphRerankConfig,
    k: usize,
) -> Vec<RerankedHit> {
    let mut ranked = graph_rerank(hits, kg, communities, config);
    ranked.truncate(k);
    ranked
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

fn build_node_community_map(
    kg: &KnowledgeGraph,
    communities: Option<&HashMap<usize, Vec<EntityId>>>,
) -> HashMap<EntityId, usize> {
    if let Some(c) = communities {
        return node_community_map(c);
    }
    if let Some(c) = kg.communities.as_ref() {
        return node_community_map(c);
    }
    HashMap::new()
}

/// Resolve a hit id string to an entity present in the KG.
///
/// Prefer hex decode of BLAKE3 ids (HNSW stores `entity.id.to_hex()`).
/// Fall back to legacy-string hashing when the entity exists under that key.
fn resolve_entity_id(id: &str, kg: &KnowledgeGraph) -> Option<EntityId> {
    if let Some(eid) = EntityId::from_hex(id)
        && kg.entity(&eid).is_some()
    {
        return Some(eid);
    }
    // Legacy / label-style keys used in tests.
    let legacy = EntityId::from_legacy_string(id);
    if kg.entity(&legacy).is_some() {
        return Some(legacy);
    }
    // Direct scan: match hex of any entity (handles casing).
    let lower = id.to_ascii_lowercase();
    for eid in kg.entity_ids() {
        if eid.to_hex() == lower {
            return Some(eid.clone());
        }
    }
    None
}

/// Undirected BFS hop distances from the seed set, capped at `max_hops`.
fn bfs_hops(
    kg: &KnowledgeGraph,
    seeds: &HashSet<EntityId>,
    max_hops: usize,
) -> HashMap<EntityId, usize> {
    let mut dist: HashMap<EntityId, usize> = HashMap::new();
    let mut q: VecDeque<(EntityId, usize)> = VecDeque::new();

    for s in seeds {
        if kg.entity(s).is_some() {
            dist.insert(s.clone(), 0);
            q.push_back((s.clone(), 0));
        }
    }

    while let Some((node, d)) = q.pop_front() {
        if d >= max_hops {
            continue;
        }
        for neighbor in kg.neighbors(&node) {
            if let std::collections::hash_map::Entry::Vacant(e) = dist.entry(neighbor.id.clone()) {
                e.insert(d + 1);
                q.push_back((neighbor.id.clone(), d + 1));
            }
        }
    }
    dist
}

/// Map hop distance → score in `[0, 1]`. Unreachable → 0.
fn hop_to_score(hop: Option<usize>, max_hops: usize) -> f64 {
    match hop {
        Some(0) => 1.0,
        Some(h) if h <= max_hops => {
            // Linear decay: hop 1 → ~0.75 at max_hops=3, hop 3 → ~0.25.
            1.0 - (h as f64) / ((max_hops + 1) as f64)
        }
        _ => 0.0,
    }
}

/// For each non-seed neighbor of the seed set, max incident confidence weight.
/// Seeds themselves get 1.0.
fn seed_relation_weights(kg: &KnowledgeGraph, seeds: &HashSet<EntityId>) -> HashMap<EntityId, f64> {
    let mut map: HashMap<EntityId, f64> = HashMap::new();
    for s in seeds {
        map.insert(s.clone(), 1.0);
        for rel in kg.edges_of(s) {
            let other = if seeds.contains(&rel.source) {
                &rel.target
            } else {
                &rel.source
            };
            if seeds.contains(other) {
                continue;
            }
            let w = rel.confidence.to_weight() as f64;
            let entry = map.entry(other.clone()).or_insert(0.0);
            if w > *entry {
                *entry = w;
            }
        }
    }
    map
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::{DomainTag, EntityType, FileType};
    use crate::model::Entity;
    use crate::relationship::{Confidence, RelationType, Relationship};

    fn make_entity(name: &str, source: &str) -> Entity {
        Entity {
            id: EntityId::new(&DomainTag::Code, &EntityType::Function, name, source),
            entity_type: EntityType::Function,
            label: name.to_string(),
            source_file: Some(source.into()),
            source_location: None,
            file_type: FileType::Code,
            metadata: serde_json::json!({}),
            legacy_id: None,
            iri: None,
        }
    }

    fn make_rel(src: &Entity, tgt: &Entity, conf: Confidence) -> Relationship {
        Relationship {
            source: src.id.clone(),
            target: tgt.id.clone(),
            relation_type: RelationType::Calls,
            confidence: conf,
            weight: conf.to_weight(),
            source_file: src.source_file.clone(),
            source_location: None,
            metadata: serde_json::json!({}),
            embedding: None,
        }
    }

    /// Graph:
    ///   hub --calls--> a
    ///   hub --calls--> b
    ///   isolated (no edges)
    /// Primary vector scores: isolated highest, then a, then hub, then b.
    /// After graph re-rank with topology weights, hub should rise because
    /// of degree + hop proximity to seed a.
    fn sample_kg() -> (KnowledgeGraph, Entity, Entity, Entity, Entity) {
        let mut kg = KnowledgeGraph::new();
        let hub = make_entity("hub", "core.rs");
        let a = make_entity("alpha", "a.rs");
        let b = make_entity("beta", "b.rs");
        let isolated = make_entity("isolated", "iso.rs");
        kg.add_entity(hub.clone());
        kg.add_entity(a.clone());
        kg.add_entity(b.clone());
        kg.add_entity(isolated.clone());
        kg.add_relationship(make_rel(&hub, &a, Confidence::Extracted));
        kg.add_relationship(make_rel(&hub, &b, Confidence::Extracted));

        // Communities: hub+a+b together, isolated alone.
        let mut communities = HashMap::new();
        communities.insert(0, vec![hub.id.clone(), a.id.clone(), b.id.clone()]);
        communities.insert(1, vec![isolated.id.clone()]);
        kg.communities = Some(communities);

        (kg, hub, a, b, isolated)
    }

    #[test]
    fn empty_hits_returns_empty() {
        let (kg, ..) = sample_kg();
        let out = graph_rerank(&[], &kg, None, &GraphRerankConfig::default());
        assert!(out.is_empty());
    }

    #[test]
    fn vector_only_preserves_primary_order() {
        let (kg, hub, a, _b, isolated) = sample_kg();
        let hits = vec![
            VectorHit::new(isolated.id.to_hex(), 0.99),
            VectorHit::new(a.id.to_hex(), 0.90),
            VectorHit::new(hub.id.to_hex(), 0.50),
        ];
        let out = graph_rerank(&hits, &kg, None, &GraphRerankConfig::vector_only());
        assert_eq!(out[0].id, isolated.id.to_hex());
        assert_eq!(out[1].id, a.id.to_hex());
        assert_eq!(out[2].id, hub.id.to_hex());
    }

    #[test]
    fn graph_topology_lifts_connected_hub_over_isolated() {
        let (kg, hub, a, b, isolated) = sample_kg();
        // Isolated has best vector score but zero topology.
        // Hub has weaker vector but high degree + same community as seed `a`.
        // seed_k=1 → only the top primary hit is a seed. Use `a` as the
        // sole seed so hop-distance from hub is measurable (hub is not a seed).
        let hits = vec![
            VectorHit::new(a.id.to_hex(), 0.99), // sole seed
            VectorHit::new(isolated.id.to_hex(), 0.95),
            VectorHit::new(hub.id.to_hex(), 0.55),
            VectorHit::new(b.id.to_hex(), 0.40),
        ];
        let cfg = GraphRerankConfig {
            seed_k: 1,
            ..GraphRerankConfig::topology_heavy()
        };
        let out = graph_rerank(&hits, &kg, None, &cfg);

        let rank = |id: &EntityId| {
            out.iter()
                .position(|h| h.id == id.to_hex())
                .expect("hit present")
        };

        // Hub must outrank the isolated high-vector node once topology is on.
        assert!(
            rank(&hub.id) < rank(&isolated.id),
            "hub rank {} should be better than isolated rank {}; scores: {:?}",
            rank(&hub.id),
            rank(&isolated.id),
            out.iter()
                .map(|h| (h.id.chars().take(8).collect::<String>(), h.score))
                .collect::<Vec<_>>()
        );

        // Seed `a` should stay near the top.
        assert!(
            rank(&a.id) <= 1,
            "seed a should be top-2, got rank {}",
            rank(&a.id)
        );

        // Hop features populated for connected nodes (hub is 1 hop from seed a).
        let hub_hit = out.iter().find(|h| h.id == hub.id.to_hex()).unwrap();
        assert_eq!(hub_hit.hop_distance, Some(1));
        assert!(hub_hit.features.degree > 0.0);
        assert_eq!(hub_hit.features.community, 1.0);
        assert!(hub_hit.features.relation > 0.0);

        let iso_hit = out.iter().find(|h| h.id == isolated.id.to_hex()).unwrap();
        assert_eq!(iso_hit.degree, 0);
        assert!(iso_hit.hop_distance.is_none());
        assert_eq!(iso_hit.features.community, 0.0);
        assert_eq!(iso_hit.features.relation, 0.0);
    }

    #[test]
    fn hop_score_decays_with_distance() {
        assert!((hop_to_score(Some(0), 3) - 1.0).abs() < 1e-9);
        assert!(hop_to_score(Some(1), 3) > hop_to_score(Some(2), 3));
        assert!(hop_to_score(Some(2), 3) > hop_to_score(Some(3), 3));
        assert_eq!(hop_to_score(None, 3), 0.0);
        assert_eq!(hop_to_score(Some(99), 3), 0.0);
    }

    #[test]
    fn relation_weight_prefers_extracted_over_ambiguous() {
        let mut kg = KnowledgeGraph::new();
        let seed = make_entity("seed", "s.rs");
        let strong = make_entity("strong", "st.rs");
        let weak = make_entity("weak", "w.rs");
        kg.add_entity(seed.clone());
        kg.add_entity(strong.clone());
        kg.add_entity(weak.clone());
        kg.add_relationship(make_rel(&seed, &strong, Confidence::Extracted));
        kg.add_relationship(make_rel(&seed, &weak, Confidence::Ambiguous));

        let hits = vec![
            VectorHit::new(seed.id.to_hex(), 1.0),
            VectorHit::new(strong.id.to_hex(), 0.5),
            VectorHit::new(weak.id.to_hex(), 0.5),
        ];
        let cfg = GraphRerankConfig {
            w_vector: 0.2,
            w_degree: 0.0,
            w_community: 0.0,
            w_hop: 0.0,
            w_relation: 0.8,
            seed_k: 1,
            max_hops: 2,
        };
        let out = graph_rerank(&hits, &kg, None, &cfg);
        let strong_s = out
            .iter()
            .find(|h| h.id == strong.id.to_hex())
            .unwrap()
            .score;
        let weak_s = out.iter().find(|h| h.id == weak.id.to_hex()).unwrap().score;
        assert!(
            strong_s > weak_s,
            "extracted edge should outrank ambiguous: {strong_s} vs {weak_s}"
        );
    }

    #[test]
    fn top_k_truncates() {
        let (kg, hub, a, b, isolated) = sample_kg();
        let hits = vec![
            VectorHit::new(isolated.id.to_hex(), 0.9),
            VectorHit::new(a.id.to_hex(), 0.8),
            VectorHit::new(hub.id.to_hex(), 0.7),
            VectorHit::new(b.id.to_hex(), 0.6),
        ];
        let out = graph_rerank_top_k(&hits, &kg, None, &GraphRerankConfig::default(), 2);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn unknown_ids_keep_vector_contribution() {
        let (kg, ..) = sample_kg();
        let hits = vec![
            VectorHit::new("not-a-real-entity-id", 0.8),
            VectorHit::new("also-missing", 0.6),
        ];
        let out = graph_rerank(&hits, &kg, None, &GraphRerankConfig::default());
        assert_eq!(out.len(), 2);
        assert!(out[0].score > 0.0);
        assert_eq!(out[0].degree, 0);
        assert!(out[0].hop_distance.is_none());
    }

    #[test]
    fn config_defaults_sum_to_one() {
        let c = GraphRerankConfig::default();
        let sum = c.w_vector + c.w_degree + c.w_community + c.w_hop + c.w_relation;
        assert!((sum - 1.0).abs() < 1e-9);
    }
}
