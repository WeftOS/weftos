//! SASE community detection (WEFT-516 / KG-004 research path).
//!
//! Scalable and Adaptive Spectral Embedding (Liu et al., CIKM 2024,
//! arXiv:2408.05765) adapted for structural knowledge graphs without
//! dense node attributes:
//!
//! 1. **Initial features** — deterministic Gaussian RFF-style features
//! 2. **k-order simple graph convolution** — row-stochastic `S = D⁻¹A`
//!    applied adaptively until the embedding stabilizes
//! 3. **Random Fourier Features** — map convolved features into a
//!    higher-dimensional approximate spectral space
//! 4. **k-means** — cluster the embeddings (k chosen by modularity on
//!    small graphs, √n heuristic on large ones)
//!
//! Linear time/space in edges for the convolution + RFF steps. Parameter
//! free for callers: defaults in [`SaseConfig`] are fixed for determinism.

use crate::entity::EntityId;
use crate::model::KnowledgeGraph;
use std::collections::HashMap;

/// Default embedding feature dimension before convolution.
pub const SASE_FEATURE_DIM: usize = 32;
/// Default RFF output dimension.
pub const SASE_RFF_DIM: usize = 32;
/// Maximum simple-graph-convolution order.
pub const SASE_MAX_ORDER: usize = 5;
/// Cap on k-means cluster count search.
pub const SASE_MAX_KMEANS_K: usize = 16;
/// k-means assignment iterations.
pub const SASE_KMEANS_ITERS: usize = 20;
/// Relative Frobenius change below which adaptive order stops.
const ORDER_STABILITY_EPS: f64 = 1e-3;
/// Below this node count, search k by Newman modularity.
const MODULARITY_SEARCH_MAX_N: usize = 500;

/// Tunable SASE knobs. Defaults are parameter-free for production callers.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SaseConfig {
    /// Dimensionality of the initial feature matrix.
    pub feature_dim: usize,
    /// Dimensionality of the RFF map.
    pub rff_dim: usize,
    /// Maximum k-order convolution steps.
    pub max_order: usize,
    /// Upper bound on k for k-means.
    pub max_kmeans_k: usize,
    /// k-means iteration budget.
    pub kmeans_iters: usize,
    /// LCG seed for RFF weights and k-means tie-breaks.
    pub seed: u64,
}

impl Default for SaseConfig {
    fn default() -> Self {
        Self {
            feature_dim: SASE_FEATURE_DIM,
            rff_dim: SASE_RFF_DIM,
            max_order: SASE_MAX_ORDER,
            max_kmeans_k: SASE_MAX_KMEANS_K,
            kmeans_iters: SASE_KMEANS_ITERS,
            seed: 0x5A5E_C1A5_5160_DEAD,
        }
    }
}

/// Partition connected nodes via SASE. Returns `entity → community_id`
/// with contiguous community ids starting at 0.
pub fn sase_partition(
    kg: &KnowledgeGraph,
    nodes: &[EntityId],
    config: &SaseConfig,
) -> HashMap<EntityId, usize> {
    let n = nodes.len();
    if n == 0 {
        return HashMap::new();
    }
    if n == 1 {
        let mut out = HashMap::new();
        out.insert(nodes[0].clone(), 0);
        return out;
    }

    // Deterministic node order.
    let mut sorted: Vec<EntityId> = nodes.to_vec();
    sorted.sort_by_key(|a| a.0);
    let index: HashMap<&EntityId, usize> =
        sorted.iter().enumerate().map(|(i, id)| (id, i)).collect();

    // Sparse undirected adjacency (unique neighbors).
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut degree = vec![0.0f64; n];
    for (i, id) in sorted.iter().enumerate() {
        let mut seen = std::collections::HashSet::new();
        for neigh in kg.neighbors(id) {
            if let Some(&j) = index.get(&neigh.id)
                && i != j
                && seen.insert(j)
            {
                adj[i].push(j);
                degree[i] += 1.0;
            }
        }
        adj[i].sort_unstable();
    }

    let mut rng = Lcg::new(config.seed);

    // Step 1: initial Gaussian features (n × d).
    let d = config.feature_dim.max(1);
    let mut x = vec![0.0f64; n * d];
    for i in 0..n {
        for j in 0..d {
            x[i * d + j] = rng.next_gaussian();
        }
        // Project out constant mean per feature column for stability.
    }
    for j in 0..d {
        let mean: f64 = (0..n).map(|i| x[i * d + j]).sum::<f64>() / n as f64;
        for i in 0..n {
            x[i * d + j] -= mean;
        }
    }
    row_l2_normalize(&mut x, n, d);

    // Step 2: adaptive k-order simple graph convolution X ← S X.
    let max_k = config.max_order.max(1);
    let mut best_x = x.clone();
    for _order in 1..=max_k {
        let prev = x.clone();
        x = sparse_sgc_mul(&adj, &degree, &prev, n, d);
        row_l2_normalize(&mut x, n, d);
        let change = relative_frobenius(&prev, &x);
        best_x = x.clone();
        if change < ORDER_STABILITY_EPS {
            break;
        }
    }
    let x = best_x;

    // Step 3: Random Fourier Features map.
    let m = config.rff_dim.max(1);
    let z = rff_map(&x, n, d, m, &mut rng);

    // Step 4: choose k and run k-means.
    let k_hi = config.max_kmeans_k.min(n).max(1);
    let labels = if n <= MODULARITY_SEARCH_MAX_N && k_hi >= 2 {
        let sweep = KmeansSweep {
            n,
            dim: m,
            k_hi,
            iters: config.kmeans_iters,
        };
        pick_kmeans_by_modularity(kg, &sorted, &z, sweep, &mut rng)
    } else {
        let k = heuristic_k(n, k_hi);
        kmeans(&z, n, m, k, config.kmeans_iters, &mut rng)
    };

    // Remap to contiguous ids in sorted label order for determinism.
    let mut unique: Vec<usize> = labels.clone();
    unique.sort_unstable();
    unique.dedup();
    let remap: HashMap<usize, usize> = unique
        .into_iter()
        .enumerate()
        .map(|(cid, lab)| (lab, cid))
        .collect();

    sorted
        .into_iter()
        .enumerate()
        .map(|(i, id)| (id, remap[&labels[i]]))
        .collect()
}

/// √n / 2 style cluster count, clamped to [1, max_k].
fn heuristic_k(n: usize, max_k: usize) -> usize {
    let est = ((n as f64).sqrt() / 2.0).round() as usize;
    est.clamp(1, max_k.max(1))
}

/// Shape of the embedded point set and the k-means search bounds.
#[derive(Clone, Copy)]
struct KmeansSweep {
    n: usize,
    dim: usize,
    k_hi: usize,
    iters: usize,
}

/// Try k ∈ 2..=k_hi (and 1), pick labels with highest Newman modularity.
fn pick_kmeans_by_modularity(
    kg: &KnowledgeGraph,
    nodes: &[EntityId],
    z: &[f64],
    sweep: KmeansSweep,
    rng: &mut Lcg,
) -> Vec<usize> {
    let KmeansSweep {
        n,
        dim,
        k_hi,
        iters,
    } = sweep;
    let mut best_labels = vec![0usize; n];
    let mut best_q = f64::NEG_INFINITY;

    // Always consider k=1 (single community).
    {
        let labels = vec![0usize; n];
        let q = modularity_from_labels(kg, nodes, &labels);
        if q > best_q {
            best_q = q;
            best_labels = labels;
        }
    }

    for k in 2..=k_hi {
        // Fresh sub-stream so k trials are independent of each other order.
        let mut sub = Lcg::new(rng.next_u64() ^ (k as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let labels = kmeans(z, n, dim, k, iters, &mut sub);
        let q = modularity_from_labels(kg, nodes, &labels);
        // Prefer higher Q; on ties keep the earlier (smaller) k.
        if q > best_q + 1e-12 {
            best_q = q;
            best_labels = labels;
        }
    }
    let _ = best_q;
    best_labels
}

fn modularity_from_labels(kg: &KnowledgeGraph, nodes: &[EntityId], labels: &[usize]) -> f64 {
    let mut communities: HashMap<usize, Vec<EntityId>> = HashMap::new();
    for (i, id) in nodes.iter().enumerate() {
        communities.entry(labels[i]).or_default().push(id.clone());
    }
    crate::cluster::newman_modularity(kg, &communities)
}

/// Row-stochastic simple graph convolution: `(SX)[i] = Σ_j A_ij X_j / deg(i)`.
fn sparse_sgc_mul(adj: &[Vec<usize>], degree: &[f64], x: &[f64], n: usize, d: usize) -> Vec<f64> {
    let mut out = vec![0.0f64; n * d];
    for i in 0..n {
        let deg = degree[i];
        if deg <= 0.0 {
            // Isolate in the connected set: keep features.
            for j in 0..d {
                out[i * d + j] = x[i * d + j];
            }
            continue;
        }
        let inv = 1.0 / deg;
        for &nb in &adj[i] {
            for j in 0..d {
                out[i * d + j] += inv * x[nb * d + j];
            }
        }
    }
    out
}

fn row_l2_normalize(x: &mut [f64], n: usize, d: usize) {
    for i in 0..n {
        let mut norm_sq = 0.0;
        for j in 0..d {
            norm_sq += x[i * d + j] * x[i * d + j];
        }
        let norm = norm_sq.sqrt();
        if norm > 1e-12 {
            for j in 0..d {
                x[i * d + j] /= norm;
            }
        }
    }
}

fn relative_frobenius(a: &[f64], b: &[f64]) -> f64 {
    let mut num = 0.0;
    let mut den = 0.0;
    for (ai, bi) in a.iter().zip(b.iter()) {
        let d = ai - bi;
        num += d * d;
        den += ai * ai;
    }
    if den < 1e-18 {
        return num.sqrt();
    }
    (num / den).sqrt()
}

/// φ(x)_j = √(2/m) cos(w_j · x + b_j) with w ~ N(0,I), b ~ U(0,2π).
fn rff_map(x: &[f64], n: usize, d: usize, m: usize, rng: &mut Lcg) -> Vec<f64> {
    let mut w = vec![0.0f64; m * d];
    let mut b = vec![0.0f64; m];
    for j in 0..m {
        for k in 0..d {
            w[j * d + k] = rng.next_gaussian();
        }
        b[j] = rng.next_uniform() * 2.0 * std::f64::consts::PI;
    }
    let scale = (2.0 / m as f64).sqrt();
    let mut z = vec![0.0f64; n * m];
    for i in 0..n {
        for j in 0..m {
            let mut dot = b[j];
            for k in 0..d {
                dot += w[j * d + k] * x[i * d + k];
            }
            z[i * m + j] = scale * dot.cos();
        }
    }
    z
}

/// Deterministic k-means (farthest-point init + iterative assignment).
fn kmeans(
    z: &[f64],
    n: usize,
    dim: usize,
    k: usize,
    max_iters: usize,
    rng: &mut Lcg,
) -> Vec<usize> {
    let k = k.clamp(1, n);
    if k == 1 {
        return vec![0; n];
    }

    // Farthest-point centroid initialization for determinism + spread.
    let mut centroids = vec![0.0f64; k * dim];
    // First centroid: node 0 (sorted order).
    centroids[..dim].copy_from_slice(&z[..dim]);
    let mut chosen = vec![0usize];
    for c in 1..k {
        let mut best_i = 0usize;
        let mut best_dist = -1.0f64;
        for i in 0..n {
            if chosen.contains(&i) {
                continue;
            }
            let mut min_d = f64::INFINITY;
            for &cj in &chosen {
                min_d = min_d.min(sq_dist(
                    &z[i * dim..(i + 1) * dim],
                    &z[cj * dim..(cj + 1) * dim],
                ));
            }
            // Prefer larger min distance; break ties by smaller index.
            if min_d > best_dist + 1e-15 || ((min_d - best_dist).abs() <= 1e-15 && i < best_i) {
                best_dist = min_d;
                best_i = i;
            }
        }
        // Fallback if all remaining equidistant.
        if best_dist < 0.0 {
            best_i = (0..n).find(|i| !chosen.contains(i)).unwrap_or(0);
        }
        for d in 0..dim {
            centroids[c * dim + d] = z[best_i * dim + d];
        }
        chosen.push(best_i);
    }

    let mut labels = vec![0usize; n];
    for _ in 0..max_iters {
        let mut changed = false;
        // Assign.
        for i in 0..n {
            let mut best_c = 0usize;
            let mut best_d = f64::INFINITY;
            for c in 0..k {
                let dist = sq_dist(
                    &z[i * dim..(i + 1) * dim],
                    &centroids[c * dim..(c + 1) * dim],
                );
                if dist < best_d - 1e-15 || ((dist - best_d).abs() <= 1e-15 && c < best_c) {
                    best_d = dist;
                    best_c = c;
                }
            }
            if labels[i] != best_c {
                labels[i] = best_c;
                changed = true;
            }
        }
        // Update centroids.
        let mut sums = vec![0.0f64; k * dim];
        let mut counts = vec![0usize; k];
        for i in 0..n {
            let c = labels[i];
            counts[c] += 1;
            for d in 0..dim {
                sums[c * dim + d] += z[i * dim + d];
            }
        }
        for c in 0..k {
            if counts[c] == 0 {
                // Re-seed empty cluster at a random point (deterministic LCG).
                let i = (rng.next_u64() as usize) % n;
                for d in 0..dim {
                    centroids[c * dim + d] = z[i * dim + d];
                }
            } else {
                let inv = 1.0 / counts[c] as f64;
                for d in 0..dim {
                    centroids[c * dim + d] = sums[c * dim + d] * inv;
                }
            }
        }
        if !changed {
            break;
        }
    }
    labels
}

fn sq_dist(a: &[f64], b: &[f64]) -> f64 {
    a.iter()
        .zip(b.iter())
        .map(|(x, y)| {
            let d = x - y;
            d * d
        })
        .sum()
}

/// Minimal LCG + Box-Muller — no `rand` dependency.
struct Lcg {
    state: u64,
    spare_gaussian: Option<f64>,
}

impl Lcg {
    fn new(seed: u64) -> Self {
        Self {
            state: seed | 1, // avoid zero state
            spare_gaussian: None,
        }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self
            .state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.state
    }

    fn next_uniform(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    fn next_gaussian(&mut self) -> f64 {
        if let Some(s) = self.spare_gaussian.take() {
            return s;
        }
        let u1 = self.next_uniform().max(1e-15);
        let u2 = self.next_uniform();
        let r = (-2.0 * u1.ln()).sqrt();
        let theta = 2.0 * std::f64::consts::PI * u2;
        self.spare_gaussian = Some(r * theta.sin());
        r * theta.cos()
    }
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
    fn sase_two_cliques_separates() {
        // Two triangles with no bridge: SASE should find ≥2 communities
        // or at least cover all nodes with high within-clique cohesion.
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
        let nodes: Vec<EntityId> = kg.entity_ids().cloned().collect();
        let part = sase_partition(&kg, &nodes, &SaseConfig::default());
        assert_eq!(part.len(), 6);
        let n_comms = part
            .values()
            .copied()
            .collect::<std::collections::HashSet<_>>()
            .len();
        assert!(
            n_comms >= 2,
            "two disconnected cliques should yield ≥2 communities, got {n_comms}"
        );

        // Same-file nodes should share a community more often than not.
        let id_a1 = EntityId::new(&DomainTag::Code, &EntityType::Function, "a1", "f.py");
        let id_a2 = EntityId::new(&DomainTag::Code, &EntityType::Function, "a2", "f.py");
        let id_b1 = EntityId::new(&DomainTag::Code, &EntityType::Function, "b1", "g.py");
        assert_eq!(part[&id_a1], part[&id_a2]);
        assert_ne!(part[&id_a1], part[&id_b1]);
    }

    #[test]
    fn sase_deterministic() {
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
        let nodes: Vec<EntityId> = kg.entity_ids().cloned().collect();
        let cfg = SaseConfig::default();
        let p1 = sase_partition(&kg, &nodes, &cfg);
        let p2 = sase_partition(&kg, &nodes, &cfg);
        assert_eq!(p1, p2);
    }

    #[test]
    fn sase_single_node() {
        let entities = vec![make_entity("a", "f.py")];
        let kg = KnowledgeGraph::from_parts(entities, vec![], vec![]);
        let nodes: Vec<EntityId> = kg.entity_ids().cloned().collect();
        let part = sase_partition(&kg, &nodes, &SaseConfig::default());
        assert_eq!(part.len(), 1);
        assert_eq!(*part.values().next().unwrap(), 0);
    }

    #[test]
    fn heuristic_k_clamped() {
        assert_eq!(heuristic_k(1, 16), 1);
        assert_eq!(heuristic_k(100, 16), 5); // sqrt(100)/2 = 5
        assert_eq!(heuristic_k(10_000, 16), 16); // clamped to max
    }
}
