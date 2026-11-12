//! Synthetic fixtures for graphify benches (no tree-sitter / LLM required).
//!
//! CI and unit tests use these generators so `scripts/build.sh test
//! clawft-graphify` never needs grammars, models, or multi-minute corpus runs.
//! Criterion benches re-use the same builders at larger N.

use crate::entity::{DomainTag, EntityId, EntityType, FileType};
use crate::model::{Entity, ExtractionResult, KnowledgeGraph};
use crate::relationship::{Confidence, RelationType, Relationship};

/// Build a deterministic code-domain entity for benches.
pub fn make_entity(name: &str, source_file: &str) -> Entity {
    Entity {
        id: EntityId::new(&DomainTag::Code, &EntityType::Function, name, source_file),
        entity_type: EntityType::Function,
        label: name.to_owned(),
        iri: None,
        source_file: Some(source_file.to_owned()),
        source_location: None,
        file_type: FileType::Code,
        metadata: serde_json::json!({}),
        legacy_id: None,
    }
}

/// Build a directed CALLS relationship between two entities.
pub fn make_calls(src: &Entity, tgt: &Entity) -> Relationship {
    Relationship {
        source: src.id.clone(),
        target: tgt.id.clone(),
        relation_type: RelationType::Calls,
        confidence: Confidence::Extracted,
        weight: 1.0,
        source_file: src.source_file.clone(),
        source_location: None,
        metadata: serde_json::json!({}),
        embedding: None,
    }
}

/// Synthetic extraction for one "file": `entities_per_file` functions plus
/// chain CALLS edges (f0→f1→…→fN-1).
pub fn synthetic_extraction(file_idx: usize, entities_per_file: usize) -> ExtractionResult {
    let source_file = format!("bench/file_{file_idx:05}.rs");
    let mut entities = Vec::with_capacity(entities_per_file);
    for e in 0..entities_per_file {
        let name = format!("fn_{file_idx}_{e}");
        entities.push(make_entity(&name, &source_file));
    }
    let mut relationships = Vec::new();
    for e in 1..entities.len() {
        relationships.push(make_calls(&entities[e - 1], &entities[e]));
    }
    ExtractionResult {
        source_file,
        entities,
        relationships,
        hyperedges: Vec::new(),
        input_tokens: (entities_per_file as u64).saturating_mul(40),
        output_tokens: (entities_per_file as u64).saturating_mul(8),
        errors: Vec::new(),
    }
}

/// Generate `n_files` synthetic extractions (default shape for throughput runs).
pub fn synthetic_extractions(n_files: usize, entities_per_file: usize) -> Vec<ExtractionResult> {
    (0..n_files)
        .map(|i| synthetic_extraction(i, entities_per_file))
        .collect()
}

/// Build a synthetic knowledge graph with `n_nodes` entities and a ring of
/// CALLS edges (degree 2) plus every `skip`-th long-range edge for density.
///
/// Designed for graph-ops benches (neighbor scan, cluster, god nodes, …)
/// without loading real corpora.
pub fn synthetic_graph(n_nodes: usize) -> KnowledgeGraph {
    synthetic_graph_with_density(n_nodes, 1)
}

/// Like [`synthetic_graph`], but also adds long-range edges every `stride`
/// nodes (`stride == 0` means ring only).
pub fn synthetic_graph_with_density(n_nodes: usize, stride: usize) -> KnowledgeGraph {
    if n_nodes == 0 {
        return KnowledgeGraph::new();
    }

    let mut entities = Vec::with_capacity(n_nodes);
    for i in 0..n_nodes {
        let file = format!("mod_{}.rs", i / 50);
        let name = format!("n{i}");
        entities.push(make_entity(&name, &file));
    }

    let mut rels = Vec::with_capacity(n_nodes.saturating_mul(2));
    for i in 0..n_nodes {
        let next = (i + 1) % n_nodes;
        rels.push(make_calls(&entities[i], &entities[next]));
        if stride > 0 && i % stride == 0 {
            let far = (i + stride.saturating_mul(7) + 3) % n_nodes;
            if far != i && far != next {
                rels.push(make_calls(&entities[i], &entities[far]));
            }
        }
    }

    KnowledgeGraph::from_parts(entities, rels, vec![])
}

/// Rough structural memory estimate for a graph of `n_nodes` / `n_edges`.
///
/// Not RSS — a conservative lower-bound style model used for GRAPH-041's
/// "50K entity graph in < 500 MB" soft check without process probing.
pub fn estimate_graph_bytes(n_nodes: usize, n_edges: usize) -> u64 {
    // EntityId [32] + label/path heap (~64) + metadata/json (~48) + petgraph node (~48)
    const PER_NODE: u64 = 32 + 64 + 48 + 48 + 64;
    // Relationship + edge index overhead
    const PER_EDGE: u64 = 64 + 32 + 48 + 48;
    // HashMap index / communities slack
    const OVERHEAD: u64 = 1024 * 1024; // 1 MiB base
    OVERHEAD + (n_nodes as u64).saturating_mul(PER_NODE) + (n_edges as u64).saturating_mul(PER_EDGE)
}

/// Token-reduction helper: corpus tokens (sum of extraction input tokens)
/// vs a graph-query token estimate (labels of neighbors of a hub).
pub fn token_reduction_ratio(corpus_tokens: u64, graph_query_tokens: u64) -> f64 {
    if corpus_tokens == 0 {
        return 0.0;
    }
    1.0 - (graph_query_tokens as f64 / corpus_tokens as f64)
}

/// Estimate query tokens for a neighbor walk result (words ≈ tokens).
pub fn estimate_query_tokens(labels: &[String]) -> u64 {
    labels
        .iter()
        .map(|l| l.split_whitespace().count().max(1) as u64)
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthetic_extraction_counts() {
        let ext = synthetic_extraction(0, 5);
        assert_eq!(ext.entities.len(), 5);
        assert_eq!(ext.relationships.len(), 4);
        assert!(ext.source_file.contains("file_00000"));
    }

    #[test]
    fn synthetic_graph_ring() {
        let kg = synthetic_graph(10);
        assert_eq!(kg.entity_count(), 10);
        assert!(kg.relationship_count() >= 10);
    }

    #[test]
    fn estimate_50k_under_500mb() {
        let bytes = estimate_graph_bytes(50_000, 100_000);
        assert!(
            bytes < 500 * 1024 * 1024,
            "estimate {bytes} exceeds 500 MiB soft budget"
        );
    }
}
