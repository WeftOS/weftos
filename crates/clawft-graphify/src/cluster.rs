//! Community detection via label propagation or SASE, oversized community
//! splitting, cohesion scoring, and auto-labeling.
//!
//! Ported from Python `graphify/cluster.py`. Default path is label propagation
//! (deterministic, matches WeftOS `causal.rs`). SASE (k-order graph convolution
//! with RFF and k-means; WEFT-516 / arXiv:2408.05765) is available via
//! [`ClusterMethod::Sase`] / [`cluster_with`]. Enable Cargo feature
//! `sase-cluster` to make SASE the default for [`cluster`].

use crate::cluster_sase;
use crate::eml_models::ClusterThresholdModel;
use crate::entity::EntityId;
use crate::model::KnowledgeGraph;
use std::collections::HashMap;

// Re-export SASE config for callers / benches.
pub use crate::cluster_sase::SaseConfig;

/// Communities larger than 25% of the graph get split.
pub const MAX_COMMUNITY_FRACTION: f64 = 0.25;
/// Only split if community has at least this many nodes.
pub const MIN_SPLIT_SIZE: usize = 10;

/// Community detection algorithm selection (WEFT-516).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ClusterMethod {
    /// Classic label propagation (default when `sase-cluster` is off).
    #[default]
    LabelPropagation,
    /// SASE: adaptive k-order SGC + Random Fourier Features + k-means.
    Sase,
}

/// Resolve the default clustering method.
///
/// - Without Cargo feature `sase-cluster`: [`ClusterMethod::LabelPropagation`]
/// - With `sase-cluster`: [`ClusterMethod::Sase`] (opt-in default migration)
pub fn default_cluster_method() -> ClusterMethod {
    #[cfg(feature = "sase-cluster")]
    {
        ClusterMethod::Sase
    }
    #[cfg(not(feature = "sase-cluster"))]
    {
        ClusterMethod::LabelPropagation
    }
}

/// Run community detection on the knowledge graph using [`default_cluster_method`].
///
/// Returns `{community_id: [entity_ids]}` where community 0 is the largest.
/// - Empty graph returns `{}`
/// - Edgeless graph: each node is its own community
/// - Oversized communities (>25% of graph, >=10 nodes) are recursively split
/// - Communities are re-indexed by size descending
pub fn cluster(kg: &KnowledgeGraph) -> HashMap<usize, Vec<EntityId>> {
    cluster_with(kg, default_cluster_method())
}

/// Run community detection with an explicit [`ClusterMethod`].
pub fn cluster_with(kg: &KnowledgeGraph, method: ClusterMethod) -> HashMap<usize, Vec<EntityId>> {
    cluster_with_config(
        kg,
        method,
        &SaseConfig::default(),
        MAX_COMMUNITY_FRACTION,
        MIN_SPLIT_SIZE,
    )
}

/// Run community detection with an optional EML threshold model.
///
/// When `eml_model` is `Some` and trained, uses learned thresholds
/// for max community fraction, min split size, and cohesion.
/// Pass `None` to use the original hardcoded constants.
/// Uses [`default_cluster_method`] for the partition algorithm.
pub fn cluster_eml(
    kg: &KnowledgeGraph,
    eml_model: Option<&ClusterThresholdModel>,
) -> HashMap<usize, Vec<EntityId>> {
    cluster_eml_with(kg, eml_model, default_cluster_method())
}

/// EML thresholds + explicit partition method.
pub fn cluster_eml_with(
    kg: &KnowledgeGraph,
    eml_model: Option<&ClusterThresholdModel>,
    method: ClusterMethod,
) -> HashMap<usize, Vec<EntityId>> {
    let (max_fraction, min_split) = match eml_model {
        Some(model) if model.is_trained() => {
            let node_count = kg.node_count() as f64;
            let edge_density = if kg.node_count() > 1 {
                kg.edge_count() as f64 / (kg.node_count() as f64 * (kg.node_count() as f64 - 1.0))
            } else {
                0.0
            };
            let (frac, split, _cohesion) = model.predict(node_count, edge_density, 0.0);
            (frac, split as usize)
        }
        _ => (MAX_COMMUNITY_FRACTION, MIN_SPLIT_SIZE),
    };
    cluster_with_config(kg, method, &SaseConfig::default(), max_fraction, min_split)
}

fn cluster_with_config(
    kg: &KnowledgeGraph,
    method: ClusterMethod,
    sase: &SaseConfig,
    max_fraction: f64,
    min_split: usize,
) -> HashMap<usize, Vec<EntityId>> {
    if kg.node_count() == 0 {
        return HashMap::new();
    }

    if kg.edge_count() == 0 {
        let mut result = HashMap::new();
        let mut ids: Vec<EntityId> = kg.entity_ids().cloned().collect();
        ids.sort_by_key(|a| a.0);
        for (i, id) in ids.into_iter().enumerate() {
            result.insert(i, vec![id]);
        }
        return result;
    }

    let isolates: Vec<EntityId> = kg
        .entity_ids()
        .filter(|id| kg.degree(id) == 0)
        .cloned()
        .collect();
    let connected: Vec<EntityId> = kg
        .entity_ids()
        .filter(|id| kg.degree(id) > 0)
        .cloned()
        .collect();

    let mut raw: HashMap<usize, Vec<EntityId>> = HashMap::new();
    if !connected.is_empty() {
        let partition = partition_connected(kg, &connected, method, sase);
        for (node, cid) in partition {
            raw.entry(cid).or_default().push(node);
        }
    }

    let first_isolate_cid = raw.keys().copied().max().unwrap_or(0) + 1;
    for (cid, node) in (first_isolate_cid..).zip(isolates) {
        raw.insert(cid, vec![node]);
    }

    let max_size = std::cmp::max(min_split, (kg.node_count() as f64 * max_fraction) as usize);

    let mut final_communities: Vec<Vec<EntityId>> = Vec::new();
    for nodes in raw.into_values() {
        if nodes.len() > max_size {
            final_communities.extend(split_community(kg, &nodes, method, sase));
        } else {
            final_communities.push(nodes);
        }
    }

    final_communities.sort_by_key(|c| std::cmp::Reverse(c.len()));
    final_communities
        .into_iter()
        .enumerate()
        .map(|(i, mut nodes)| {
            nodes.sort_by_key(|a| a.0);
            (i, nodes)
        })
        .collect()
}

fn partition_connected(
    kg: &KnowledgeGraph,
    connected: &[EntityId],
    method: ClusterMethod,
    sase: &SaseConfig,
) -> HashMap<EntityId, usize> {
    match method {
        ClusterMethod::LabelPropagation => label_propagation(kg, connected),
        ClusterMethod::Sase => cluster_sase::sase_partition(kg, connected, sase),
    }
}

/// Label propagation community detection.
///
/// Each node starts with a unique label. In each iteration, each node adopts
/// the most frequent label among its neighbors (ties broken by smallest label
/// for determinism). Converges when no labels change.
fn label_propagation(kg: &KnowledgeGraph, nodes: &[EntityId]) -> HashMap<EntityId, usize> {
    let node_set: std::collections::HashSet<&EntityId> = nodes.iter().collect();

    // Initialize: each node gets its own label (index-based for determinism)
    let mut sorted_nodes: Vec<&EntityId> = nodes.iter().collect();
    sorted_nodes.sort_by_key(|a| a.0);

    let _node_to_idx: HashMap<&EntityId, usize> = sorted_nodes
        .iter()
        .enumerate()
        .map(|(i, id)| (*id, i))
        .collect();

    let mut labels: HashMap<&EntityId, usize> = sorted_nodes
        .iter()
        .enumerate()
        .map(|(i, id)| (*id, i))
        .collect();

    // Iterate until convergence (max 50 iterations as safety valve)
    for _ in 0..50 {
        let mut changed = false;

        // Process nodes in deterministic order
        for &node in &sorted_nodes {
            let neighbors = kg.neighbors(node);
            let neighbor_labels: Vec<usize> = neighbors
                .iter()
                .filter(|n| node_set.contains(&n.id))
                .filter_map(|n| labels.get(&n.id).copied())
                .collect();

            if neighbor_labels.is_empty() {
                continue;
            }

            // Count label frequencies
            let mut freq: HashMap<usize, usize> = HashMap::new();
            for l in &neighbor_labels {
                *freq.entry(*l).or_insert(0) += 1;
            }

            // Pick the most frequent label (ties broken by smallest label ID)
            let max_count = *freq.values().max().unwrap();
            let best_label = freq
                .iter()
                .filter(|(_, count)| **count == max_count)
                .map(|(label, _)| *label)
                .min()
                .unwrap();

            if labels[node] != best_label {
                labels.insert(node, best_label);
                changed = true;
            }
        }

        if !changed {
            break;
        }
    }

    // Remap labels to contiguous community IDs.
    // Deterministic ordering via sorted+deduped labels.
    let mut unique_labels: Vec<usize> = labels.values().copied().collect();
    unique_labels.sort();
    unique_labels.dedup();
    let label_to_cid: HashMap<usize, usize> = unique_labels
        .into_iter()
        .enumerate()
        .map(|(cid, label)| (label, cid))
        .collect();

    labels
        .into_iter()
        .map(|(id, label)| (id.clone(), label_to_cid[&label]))
        .collect()
}

/// Split an oversized community by re-running the chosen partition method
/// on its subgraph.
fn split_community(
    kg: &KnowledgeGraph,
    nodes: &[EntityId],
    method: ClusterMethod,
    sase: &SaseConfig,
) -> Vec<Vec<EntityId>> {
    let sub = kg.subgraph(nodes);
    if sub.edge_count() == 0 {
        // No edges: each node is its own community
        return nodes.iter().map(|n| vec![n.clone()]).collect();
    }

    let connected: Vec<EntityId> = nodes
        .iter()
        .filter(|id| sub.degree(id) > 0)
        .cloned()
        .collect();

    if connected.is_empty() {
        return vec![nodes.to_vec()];
    }

    let partition = partition_connected(&sub, &connected, method, sase);
    let mut sub_communities: HashMap<usize, Vec<EntityId>> = HashMap::new();
    for (node, cid) in partition {
        sub_communities.entry(cid).or_default().push(node);
    }

    if sub_communities.len() <= 1 {
        return vec![nodes.to_vec()];
    }

    sub_communities
        .into_values()
        .map(|mut v| {
            v.sort_by_key(|a| a.0);
            v
        })
        .collect()
}

/// Cohesion score: ratio of actual intra-community edges to maximum possible.
///
/// - Complete subgraph = 1.0
/// - Disconnected nodes = 0.0
/// - Single node = 1.0 by convention
pub fn cohesion_score(kg: &KnowledgeGraph, community_nodes: &[EntityId]) -> f64 {
    let n = community_nodes.len();
    if n <= 1 {
        return 1.0;
    }

    let sub = kg.subgraph(community_nodes);
    let actual = sub.edge_count() as f64;
    let possible = n as f64 * (n as f64 - 1.0) / 2.0;

    if possible <= 0.0 {
        return 0.0;
    }

    let score = actual / possible;
    (score * 100.0).round() / 100.0
}

/// Newman modularity score Q for a community partition.
///
/// Measures the quality of a partition of the graph into communities.
/// ```text
/// Q = (1/2m) * sum_ij [A_ij - k_i*k_j/(2m)] * delta(c_i, c_j)
/// ```
/// where:
/// - `m` = total number of edges
/// - `A_ij` = 1 if edge exists between i and j, 0 otherwise
/// - `k_i` = degree of node i
/// - `delta(c_i, c_j)` = 1 if i and j are in the same community
///
/// Q ranges from -0.5 to 1.0. Higher values indicate better partitions.
/// A value above 0.3 typically indicates significant community structure.
///
/// Returns 0.0 for graphs with no edges.
pub fn newman_modularity(kg: &KnowledgeGraph, communities: &HashMap<usize, Vec<EntityId>>) -> f64 {
    let m = kg.edge_count();
    if m == 0 {
        return 0.0;
    }

    let two_m = 2.0 * m as f64;

    // Build a reverse mapping: entity_id -> community_id.
    let mut node_community: HashMap<&EntityId, usize> = HashMap::new();
    for (&cid, members) in communities {
        for id in members {
            node_community.insert(id, cid);
        }
    }

    // For each community, sum: (internal edges) - (sum_k_i)^2 / (2m)
    // This avoids the O(n^2) pairwise iteration.
    //
    // Q = sum_c [ L_c/m - (D_c/(2m))^2 ]
    //   where L_c = number of edges with both endpoints in community c
    //         D_c = sum of degrees of nodes in community c
    let mut q = 0.0;

    for members in communities.values() {
        // Compute L_c: count edges within this community.
        let member_set: std::collections::HashSet<&EntityId> = members.iter().collect();

        let mut internal_edges = 0usize;
        let mut degree_sum = 0usize;

        for id in members {
            degree_sum += kg.degree(id);
        }

        // Count edges where both endpoints are in this community.
        // We iterate over graph edges and check membership.
        for (src, tgt, _rel) in kg.edges() {
            if member_set.contains(&src.id) && member_set.contains(&tgt.id) {
                internal_edges += 1;
            }
        }

        // In the undirected Newman formula, each internal edge contributes
        // 2 to the adjacency sum (A_ij + A_ji). Since kg.edges() yields
        // directed edges (each stored once), multiply by 2.
        let l_c = 2.0 * internal_edges as f64;
        let d_c = degree_sum as f64;

        q += l_c / two_m - (d_c / two_m).powi(2);
    }

    // Clamp to theoretical range for numerical stability.
    q.clamp(-0.5, 1.0)
}

/// Batch cohesion scoring for all communities.
pub fn score_all(
    kg: &KnowledgeGraph,
    communities: &HashMap<usize, Vec<EntityId>>,
) -> HashMap<usize, f64> {
    communities
        .iter()
        .map(|(&cid, nodes)| (cid, cohesion_score(kg, nodes)))
        .collect()
}

/// Auto-label a community: use the most common source file stem, or the
/// highest-degree node's label if no common file.
pub fn auto_label(kg: &KnowledgeGraph, community: &[EntityId]) -> String {
    // Count source file stems
    let mut stem_counts: HashMap<String, usize> = HashMap::new();
    for id in community {
        if let Some(entity) = kg.entity(id)
            && let Some(ref source) = entity.source_file
            && !source.is_empty()
        {
            let stem = std::path::Path::new(source.as_str())
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or(source.as_str())
                .to_owned();
            *stem_counts.entry(stem).or_insert(0) += 1;
        }
    }

    // Pick the most common stem
    if let Some((stem, _)) = stem_counts.iter().max_by_key(|(_, count)| *count) {
        return stem.clone();
    }

    // Fallback: highest-degree node label
    community
        .iter()
        .max_by_key(|id| kg.degree(id))
        .and_then(|id| kg.entity(id))
        .map(|e| e.label.clone())
        .unwrap_or_else(|| format!("Community ({})", community.len()))
}

/// Generate labels for all communities.
pub fn auto_label_all(
    kg: &KnowledgeGraph,
    communities: &HashMap<usize, Vec<EntityId>>,
) -> HashMap<usize, String> {
    communities
        .iter()
        .map(|(&cid, nodes)| (cid, auto_label(kg, nodes)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::{DomainTag, EntityType, FileType};
    use crate::model::Entity;
    use crate::relationship::{Confidence, RelationType, Relationship};

    fn make_entity(name: &str, source_file: &str) -> Entity {
        Entity {
            id: EntityId::new(&DomainTag::Code, &EntityType::Function, name, source_file),
            entity_type: EntityType::Function,
            label: name.to_owned(),
            source_file: Some(source_file.to_owned()),
            source_location: None,
            file_type: FileType::Code,
            metadata: serde_json::json!({}),
            legacy_id: None,
            iri: None,
        }
    }

    fn make_rel(src_name: &str, src_file: &str, tgt_name: &str, tgt_file: &str) -> Relationship {
        Relationship {
            source: EntityId::new(&DomainTag::Code, &EntityType::Function, src_name, src_file),
            target: EntityId::new(&DomainTag::Code, &EntityType::Function, tgt_name, tgt_file),
            relation_type: RelationType::Calls,
            confidence: Confidence::Extracted,
            weight: 1.0,
            source_file: None,
            source_location: None,
            metadata: serde_json::json!({}),
            embedding: None,
        }
    }

    #[test]
    fn empty_graph_returns_empty() {
        let kg = KnowledgeGraph::new();
        let c = cluster(&kg);
        assert!(c.is_empty());
    }

    #[test]
    fn edgeless_graph_each_node_own_community() {
        let entities = vec![make_entity("a", "a.py"), make_entity("b", "b.py")];
        let kg = KnowledgeGraph::from_parts(entities, vec![], vec![]);
        let c = cluster(&kg);
        assert_eq!(c.len(), 2);
        for nodes in c.values() {
            assert_eq!(nodes.len(), 1);
        }
    }

    #[test]
    fn cluster_covers_all_nodes() {
        let entities = vec![
            make_entity("a", "f.py"),
            make_entity("b", "f.py"),
            make_entity("c", "f.py"),
            make_entity("d", "g.py"),
        ];
        let rels = vec![
            make_rel("a", "f.py", "b", "f.py"),
            make_rel("b", "f.py", "c", "f.py"),
            make_rel("a", "f.py", "c", "f.py"),
        ];
        let kg = KnowledgeGraph::from_parts(entities, rels, vec![]);
        let c = cluster(&kg);
        let all_nodes: Vec<&EntityId> = c.values().flat_map(|v| v.iter()).collect();
        assert_eq!(all_nodes.len(), 4);
    }

    #[test]
    fn cohesion_complete_subgraph_is_one() {
        let entities = vec![
            make_entity("a", "f.py"),
            make_entity("b", "f.py"),
            make_entity("c", "f.py"),
        ];
        let rels = vec![
            make_rel("a", "f.py", "b", "f.py"),
            make_rel("b", "f.py", "c", "f.py"),
            make_rel("a", "f.py", "c", "f.py"),
        ];
        let kg = KnowledgeGraph::from_parts(entities, rels, vec![]);
        let ids: Vec<EntityId> = kg.entity_ids().cloned().collect();
        let score = cohesion_score(&kg, &ids);
        assert!((score - 1.0).abs() < 0.01);
    }

    #[test]
    fn cohesion_disconnected_is_zero() {
        let entities = vec![
            make_entity("a", "f.py"),
            make_entity("b", "f.py"),
            make_entity("c", "f.py"),
        ];
        let kg = KnowledgeGraph::from_parts(entities, vec![], vec![]);
        let ids: Vec<EntityId> = kg.entity_ids().cloned().collect();
        let score = cohesion_score(&kg, &ids);
        assert!((score - 0.0).abs() < 0.01);
    }

    #[test]
    fn cohesion_single_node_is_one() {
        let entities = vec![make_entity("a", "f.py")];
        let kg = KnowledgeGraph::from_parts(entities, vec![], vec![]);
        let ids: Vec<EntityId> = kg.entity_ids().cloned().collect();
        let score = cohesion_score(&kg, &ids);
        assert!((score - 1.0).abs() < 0.01);
    }

    #[test]
    fn score_all_keys_match_communities() {
        let entities = vec![make_entity("a", "f.py"), make_entity("b", "f.py")];
        let rels = vec![make_rel("a", "f.py", "b", "f.py")];
        let kg = KnowledgeGraph::from_parts(entities, rels, vec![]);
        let communities = cluster(&kg);
        let scores = score_all(&kg, &communities);
        assert_eq!(scores.len(), communities.len());
        for key in communities.keys() {
            assert!(scores.contains_key(key));
        }
    }

    #[test]
    fn newman_modularity_no_edges_is_zero() {
        let entities = vec![make_entity("a", "f.py"), make_entity("b", "f.py")];
        let kg = KnowledgeGraph::from_parts(entities, vec![], vec![]);
        let mut communities = HashMap::new();
        communities.insert(0, kg.entity_ids().cloned().collect());
        let q = newman_modularity(&kg, &communities);
        assert!((q - 0.0).abs() < 1e-6);
    }

    #[test]
    fn newman_modularity_single_community() {
        // All nodes in one community: Q should be 0 (no better than random).
        let entities = vec![
            make_entity("a", "f.py"),
            make_entity("b", "f.py"),
            make_entity("c", "f.py"),
        ];
        let rels = vec![
            make_rel("a", "f.py", "b", "f.py"),
            make_rel("b", "f.py", "c", "f.py"),
        ];
        let kg = KnowledgeGraph::from_parts(entities, rels, vec![]);
        let ids: Vec<EntityId> = kg.entity_ids().cloned().collect();
        let mut communities = HashMap::new();
        communities.insert(0, ids);
        let q = newman_modularity(&kg, &communities);
        // Single community => Q = 1 - (sum_degrees/2m)^2 = 1 - 1 = 0
        assert!(q.abs() < 0.01, "single community Q should be ~0, got {q}");
    }

    #[test]
    fn newman_modularity_perfect_partition() {
        // Two disconnected cliques should give high modularity.
        let entities = vec![
            make_entity("a1", "f.py"),
            make_entity("a2", "f.py"),
            make_entity("a3", "f.py"),
            make_entity("b1", "g.py"),
            make_entity("b2", "g.py"),
            make_entity("b3", "g.py"),
        ];
        let rels = vec![
            // Clique A
            make_rel("a1", "f.py", "a2", "f.py"),
            make_rel("a2", "f.py", "a3", "f.py"),
            make_rel("a1", "f.py", "a3", "f.py"),
            // Clique B
            make_rel("b1", "g.py", "b2", "g.py"),
            make_rel("b2", "g.py", "b3", "g.py"),
            make_rel("b1", "g.py", "b3", "g.py"),
        ];
        let kg = KnowledgeGraph::from_parts(entities, rels, vec![]);

        let id_a1 = EntityId::new(&DomainTag::Code, &EntityType::Function, "a1", "f.py");
        let id_a2 = EntityId::new(&DomainTag::Code, &EntityType::Function, "a2", "f.py");
        let id_a3 = EntityId::new(&DomainTag::Code, &EntityType::Function, "a3", "f.py");
        let id_b1 = EntityId::new(&DomainTag::Code, &EntityType::Function, "b1", "g.py");
        let id_b2 = EntityId::new(&DomainTag::Code, &EntityType::Function, "b2", "g.py");
        let id_b3 = EntityId::new(&DomainTag::Code, &EntityType::Function, "b3", "g.py");

        let mut communities = HashMap::new();
        communities.insert(0, vec![id_a1, id_a2, id_a3]);
        communities.insert(1, vec![id_b1, id_b2, id_b3]);

        let q = newman_modularity(&kg, &communities);
        // Two equal disconnected cliques: Q = 0.5
        assert!(q > 0.3, "perfect partition should have Q > 0.3, got {q}");
    }

    #[test]
    fn newman_modularity_bad_partition() {
        // Put connected nodes in different communities.
        let entities = vec![make_entity("a", "f.py"), make_entity("b", "f.py")];
        let rels = vec![make_rel("a", "f.py", "b", "f.py")];
        let kg = KnowledgeGraph::from_parts(entities, rels, vec![]);

        let id_a = EntityId::new(&DomainTag::Code, &EntityType::Function, "a", "f.py");
        let id_b = EntityId::new(&DomainTag::Code, &EntityType::Function, "b", "f.py");

        let mut communities = HashMap::new();
        communities.insert(0, vec![id_a]);
        communities.insert(1, vec![id_b]);

        let q = newman_modularity(&kg, &communities);
        // Splitting a single edge: Q should be negative or near zero.
        assert!(q <= 0.01, "bad partition Q should be <= 0, got {q}");
    }

    #[test]
    fn newman_modularity_in_range() {
        let entities = vec![
            make_entity("a", "f.py"),
            make_entity("b", "f.py"),
            make_entity("c", "f.py"),
            make_entity("d", "g.py"),
        ];
        let rels = vec![
            make_rel("a", "f.py", "b", "f.py"),
            make_rel("b", "f.py", "c", "f.py"),
            make_rel("a", "f.py", "c", "f.py"),
            make_rel("c", "f.py", "d", "g.py"),
        ];
        let kg = KnowledgeGraph::from_parts(entities, rels, vec![]);
        let communities = cluster(&kg);
        let q = newman_modularity(&kg, &communities);
        assert!(
            (-0.5..=1.0).contains(&q),
            "Q must be in [-0.5, 1.0], got {q}"
        );
    }

    #[test]
    fn auto_label_uses_file_stem() {
        let entities = vec![
            make_entity("a", "auth.py"),
            make_entity("b", "auth.py"),
            make_entity("c", "models.py"),
        ];
        let kg = KnowledgeGraph::from_parts(entities, vec![], vec![]);
        let ids: Vec<EntityId> = kg.entity_ids().cloned().collect();
        let label = auto_label(&kg, &ids);
        assert_eq!(label, "auth");
    }

    #[test]
    fn default_method_is_label_prop_without_feature() {
        #[cfg(not(feature = "sase-cluster"))]
        assert_eq!(default_cluster_method(), ClusterMethod::LabelPropagation);
        #[cfg(feature = "sase-cluster")]
        assert_eq!(default_cluster_method(), ClusterMethod::Sase);
    }

    #[test]
    fn sase_covers_all_nodes() {
        let entities = vec![
            make_entity("a", "f.py"),
            make_entity("b", "f.py"),
            make_entity("c", "f.py"),
            make_entity("d", "g.py"),
        ];
        let rels = vec![
            make_rel("a", "f.py", "b", "f.py"),
            make_rel("b", "f.py", "c", "f.py"),
            make_rel("a", "f.py", "c", "f.py"),
        ];
        let kg = KnowledgeGraph::from_parts(entities, rels, vec![]);
        let c = cluster_with(&kg, ClusterMethod::Sase);
        let all_nodes: Vec<&EntityId> = c.values().flat_map(|v| v.iter()).collect();
        assert_eq!(all_nodes.len(), 4);
        // Isolate `d` is its own community.
        assert!(c.values().any(|v| v.len() == 1));
    }

    #[test]
    fn sase_empty_and_edgeless() {
        let empty = KnowledgeGraph::new();
        assert!(cluster_with(&empty, ClusterMethod::Sase).is_empty());

        let entities = vec![make_entity("a", "a.py"), make_entity("b", "b.py")];
        let kg = KnowledgeGraph::from_parts(entities, vec![], vec![]);
        let c = cluster_with(&kg, ClusterMethod::Sase);
        assert_eq!(c.len(), 2);
    }

    #[test]
    fn sase_two_cliques_modularity_positive() {
        let entities = vec![
            make_entity("a1", "f.py"),
            make_entity("a2", "f.py"),
            make_entity("a3", "f.py"),
            make_entity("b1", "g.py"),
            make_entity("b2", "g.py"),
            make_entity("b3", "g.py"),
        ];
        let rels = vec![
            make_rel("a1", "f.py", "a2", "f.py"),
            make_rel("a2", "f.py", "a3", "f.py"),
            make_rel("a1", "f.py", "a3", "f.py"),
            make_rel("b1", "g.py", "b2", "g.py"),
            make_rel("b2", "g.py", "b3", "g.py"),
            make_rel("b1", "g.py", "b3", "g.py"),
        ];
        let kg = KnowledgeGraph::from_parts(entities, rels, vec![]);
        let communities = cluster_with(&kg, ClusterMethod::Sase);
        assert_eq!(communities.values().map(|v| v.len()).sum::<usize>(), 6);
        assert!(
            communities.len() >= 2,
            "expected ≥2 communities, got {}",
            communities.len()
        );
        let q = newman_modularity(&kg, &communities);
        assert!(
            q > 0.2,
            "SASE on two cliques should have solid modularity, got {q}"
        );
    }

    #[test]
    fn sase_deterministic_full_pipeline() {
        let entities = vec![
            make_entity("a", "f.py"),
            make_entity("b", "f.py"),
            make_entity("c", "f.py"),
            make_entity("d", "g.py"),
        ];
        let rels = vec![
            make_rel("a", "f.py", "b", "f.py"),
            make_rel("b", "f.py", "c", "f.py"),
            make_rel("a", "f.py", "c", "f.py"),
            make_rel("c", "f.py", "d", "g.py"),
        ];
        let kg = KnowledgeGraph::from_parts(entities, rels, vec![]);
        let c1 = cluster_with(&kg, ClusterMethod::Sase);
        let c2 = cluster_with(&kg, ClusterMethod::Sase);
        assert_eq!(c1, c2);
    }

    #[test]
    fn label_prop_and_sase_both_cover() {
        let entities = vec![
            make_entity("a", "f.py"),
            make_entity("b", "f.py"),
            make_entity("c", "f.py"),
            make_entity("d", "g.py"),
            make_entity("e", "g.py"),
        ];
        let rels = vec![
            make_rel("a", "f.py", "b", "f.py"),
            make_rel("b", "f.py", "c", "f.py"),
            make_rel("d", "g.py", "e", "g.py"),
        ];
        let kg = KnowledgeGraph::from_parts(entities, rels, vec![]);
        let lp = cluster_with(&kg, ClusterMethod::LabelPropagation);
        let sase = cluster_with(&kg, ClusterMethod::Sase);
        let count = |c: &HashMap<usize, Vec<EntityId>>| c.values().map(|v| v.len()).sum::<usize>();
        assert_eq!(count(&lp), 5);
        assert_eq!(count(&sase), 5);
    }

    /// Lightweight microbench (not criterion): times LP vs SASE on a 200-node
    /// ring+skip synthetic graph. Prints durations; asserts both finish and
    /// cover all nodes. Full criterion: `cargo bench -p clawft-graphify
    /// --bench graph_ops -- graph_ops_cluster_method`.
    #[test]
    fn microbench_lp_vs_sase_covers() {
        use crate::bench::fixtures::synthetic_graph;
        use std::time::Instant;

        let kg = synthetic_graph(200);
        let n = kg.node_count();

        let t0 = Instant::now();
        let lp = cluster_with(&kg, ClusterMethod::LabelPropagation);
        let lp_ms = t0.elapsed().as_secs_f64() * 1000.0;

        let t1 = Instant::now();
        let sase = cluster_with(&kg, ClusterMethod::Sase);
        let sase_ms = t1.elapsed().as_secs_f64() * 1000.0;

        let cover = |c: &HashMap<usize, Vec<EntityId>>| c.values().map(|v| v.len()).sum::<usize>();
        assert_eq!(cover(&lp), n);
        assert_eq!(cover(&sase), n);

        let lp_q = newman_modularity(&kg, &lp);
        let sase_q = newman_modularity(&kg, &sase);
        eprintln!(
            "WEFT-516 microbench n={n}: LP {lp_ms:.2}ms Q={lp_q:.3} | SASE {sase_ms:.2}ms Q={sase_q:.3}"
        );
        // Both should produce a valid modularity in range (quality comparison is informational).
        assert!((-0.5..=1.0).contains(&lp_q));
        assert!((-0.5..=1.0).contains(&sase_q));
    }
}
