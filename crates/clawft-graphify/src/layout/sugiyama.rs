//! Sugiyama hierarchical (layered digraph) layout.
//!
//! Implements the classic four-phase algorithm for directed graphs:
//! 1. **Cycle removal** — reverse a feedback arc set so the working graph is a DAG
//! 2. **Layer assignment** — longest-path ranking (sources at layer 0)
//! 3. **Crossing reduction** — barycenter heuristic with multi-pass sweeps
//! 4. **Coordinate assignment** — discrete x within layers, y by layer index
//!
//! Unlike tree layout (Reingold–Tilford on `Contains` only), this consumes *all*
//! directed edges and places nodes on discrete ranks so edges predominantly
//! flow downward.

use crate::entity::EntityId;
use std::collections::{HashMap, HashSet, VecDeque};

/// Configuration for the Sugiyama layered layout.
#[derive(Debug, Clone)]
pub struct LayeredConfig {
    /// Horizontal gap between adjacent nodes in the same layer.
    pub node_spacing: f64,
    /// Vertical gap between successive layers.
    pub layer_spacing: f64,
    /// Number of up/down barycenter sweeps for crossing reduction.
    pub crossing_iterations: usize,
    /// Outer padding when fitting into the viewport.
    pub padding: f64,
}

impl Default for LayeredConfig {
    fn default() -> Self {
        Self {
            node_spacing: 80.0,
            layer_spacing: 100.0,
            crossing_iterations: 24,
            padding: 40.0,
        }
    }
}

/// Compute Sugiyama layered positions for a directed graph.
///
/// `node_ids`: entities to position.
/// `edges`: directed pairs of `(source_idx, target_idx)` into `node_ids`.
///
/// Returns a map of `EntityId → (x, y)`. Coordinates are scaled to fit
/// `[padding, width - padding] × [padding, height - padding]` when possible.
pub fn layout(
    node_ids: &[EntityId],
    edges: &[(usize, usize)],
    width: f64,
    height: f64,
    config: &LayeredConfig,
) -> HashMap<EntityId, (f64, f64)> {
    let n = node_ids.len();
    if n == 0 {
        return HashMap::new();
    }
    if n == 1 {
        let mut result = HashMap::new();
        result.insert(node_ids[0].clone(), (width / 2.0, height / 2.0));
        return result;
    }

    // Build adjacency from unique directed edges (skip self-loops).
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut rev: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut edge_set: HashSet<(usize, usize)> = HashSet::new();
    for &(s, t) in edges {
        if s >= n || t >= n || s == t {
            continue;
        }
        if edge_set.insert((s, t)) {
            adj[s].push(t);
            rev[t].push(s);
        }
    }

    // Phase 1: cycle removal via DFS feedback arc set (reverse back-edges).
    let dag_edges = remove_cycles(n, &adj);

    // Rebuild adjacency for the acyclic working graph.
    let mut dag_adj: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut dag_rev: Vec<Vec<usize>> = vec![Vec::new(); n];
    for &(s, t) in &dag_edges {
        dag_adj[s].push(t);
        dag_rev[t].push(s);
    }

    // Phase 2: longest-path layer assignment (sources → sinks).
    let layers = assign_layers(n, &dag_adj, &dag_rev);

    // Group nodes by layer index.
    let max_layer = layers.iter().copied().max().unwrap_or(0);
    let mut by_layer: Vec<Vec<usize>> = vec![Vec::new(); max_layer + 1];
    for (node, &layer) in layers.iter().enumerate() {
        by_layer[layer].push(node);
    }
    // Deterministic initial order within each layer (by original index).
    for layer_nodes in &mut by_layer {
        layer_nodes.sort_unstable();
    }

    // Phase 3: barycenter crossing reduction.
    reduce_crossings(
        &mut by_layer,
        &dag_adj,
        &dag_rev,
        config.crossing_iterations,
    );

    // Phase 4: coordinate assignment.
    let mut positions = assign_coordinates(&by_layer, config);

    // Fit into the requested viewport.
    fit_to_viewport(&mut positions, width, height, config.padding);

    node_ids
        .iter()
        .enumerate()
        .map(|(i, id)| (id.clone(), positions[i]))
        .collect()
}

/// Reverse a feedback arc set so the working graph becomes a DAG.
///
/// Uses a DFS coloring approach: when a back-edge to a grey (in-stack) node is
/// found it is reversed rather than dropped, preserving connectivity for
/// ranking.
fn remove_cycles(n: usize, adj: &[Vec<usize>]) -> Vec<(usize, usize)> {
    let mut result: HashSet<(usize, usize)> = HashSet::new();
    for (s, targets) in adj.iter().enumerate() {
        for &t in targets {
            result.insert((s, t));
        }
    }

    // 0 = white, 1 = grey, 2 = black
    let mut color = vec![0u8; n];
    let mut stack: Vec<(usize, usize)> = Vec::new(); // (node, next_child_idx)

    for start in 0..n {
        if color[start] != 0 {
            continue;
        }
        color[start] = 1;
        stack.push((start, 0));

        while let Some((u, child_i)) = stack.pop() {
            let neighbors = &adj[u];
            if child_i < neighbors.len() {
                stack.push((u, child_i + 1));
                let v = neighbors[child_i];
                match color[v] {
                    0 => {
                        color[v] = 1;
                        stack.push((v, 0));
                    }
                    1 => {
                        // Back-edge u → v: reverse it in the working set.
                        result.remove(&(u, v));
                        // Avoid creating a self-loop on reverse of multi-edges
                        // that already exist the other way — still insert reverse
                        // so the edge participates in ranking if needed.
                        if u != v {
                            result.insert((v, u));
                        }
                    }
                    _ => {}
                }
            } else {
                color[u] = 2;
            }
        }
    }

    result.into_iter().collect()
}

/// Longest-path layering: layer(v) = 0 if no predecessors, else
/// 1 + max(layer(u) for u in preds). Processed in topological order.
fn assign_layers(n: usize, adj: &[Vec<usize>], rev: &[Vec<usize>]) -> Vec<usize> {
    let mut indegree = vec![0usize; n];
    for targets in adj {
        for &t in targets {
            indegree[t] += 1;
        }
    }

    let mut queue: VecDeque<usize> = VecDeque::new();
    for (i, &deg) in indegree.iter().enumerate() {
        if deg == 0 {
            queue.push_back(i);
        }
    }

    let mut order = Vec::with_capacity(n);
    let mut seen = vec![false; n];
    while let Some(u) = queue.pop_front() {
        if seen[u] {
            continue;
        }
        seen[u] = true;
        order.push(u);
        for &v in &adj[u] {
            indegree[v] = indegree[v].saturating_sub(1);
            if indegree[v] == 0 {
                queue.push_back(v);
            }
        }
    }

    // Any remaining nodes (shouldn't happen for a true DAG after cycle
    // removal, but be defensive) go at the end in index order.
    for (i, &was_seen) in seen.iter().enumerate() {
        if !was_seen {
            order.push(i);
        }
    }

    let mut layer = vec![0usize; n];
    for &u in &order {
        let mut best = 0usize;
        let mut has_pred = false;
        for &p in &rev[u] {
            // Only count predecessors that appear earlier in topo order
            // (i.e. edges that still point forward after cycle removal).
            has_pred = true;
            best = best.max(layer[p] + 1);
        }
        layer[u] = if has_pred { best } else { 0 };
    }

    // Normalize so the minimum layer is 0 (handles disconnected sources).
    let min_layer = layer.iter().copied().min().unwrap_or(0);
    if min_layer > 0 {
        for l in &mut layer {
            *l -= min_layer;
        }
    }

    layer
}

/// Multi-pass barycenter crossing reduction.
fn reduce_crossings(
    by_layer: &mut [Vec<usize>],
    adj: &[Vec<usize>],
    rev: &[Vec<usize>],
    iterations: usize,
) {
    if by_layer.len() < 2 {
        return;
    }

    for iter in 0..iterations {
        if iter % 2 == 0 {
            // Sweep top → bottom: order layer i by barycenter of parents in i-1.
            for i in 1..by_layer.len() {
                let prev = &by_layer[i - 1];
                let pos: HashMap<usize, f64> = prev
                    .iter()
                    .enumerate()
                    .map(|(p, &node)| (node, p as f64))
                    .collect();
                barycenter_sort(&mut by_layer[i], |node| {
                    let preds = &rev[node];
                    if preds.is_empty() {
                        return None;
                    }
                    let sum: f64 = preds.iter().filter_map(|p| pos.get(p).copied()).sum();
                    let count = preds.iter().filter(|p| pos.contains_key(p)).count();
                    if count == 0 {
                        None
                    } else {
                        Some(sum / count as f64)
                    }
                });
            }
        } else {
            // Sweep bottom → top: order layer i by barycenter of children in i+1.
            for i in (0..by_layer.len() - 1).rev() {
                let next = &by_layer[i + 1];
                let pos: HashMap<usize, f64> = next
                    .iter()
                    .enumerate()
                    .map(|(p, &node)| (node, p as f64))
                    .collect();
                barycenter_sort(&mut by_layer[i], |node| {
                    let succs = &adj[node];
                    if succs.is_empty() {
                        return None;
                    }
                    let sum: f64 = succs.iter().filter_map(|s| pos.get(s).copied()).sum();
                    let count = succs.iter().filter(|s| pos.contains_key(s)).count();
                    if count == 0 {
                        None
                    } else {
                        Some(sum / count as f64)
                    }
                });
            }
        }
    }
}

/// Stable sort of a layer by barycenter; nodes with no median keep relative order
/// at the end of the computed positions (use previous index as fallback).
fn barycenter_sort(layer: &mut [usize], bary: impl Fn(usize) -> Option<f64>) {
    let indexed: Vec<(usize, usize, f64)> = layer
        .iter()
        .enumerate()
        .map(|(ord, &node)| {
            let key = bary(node).unwrap_or(ord as f64);
            (ord, node, key)
        })
        .collect();

    let mut sorted = indexed;
    sorted.sort_by(|a, b| {
        a.2.partial_cmp(&b.2)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });

    for (i, &(_, node, _)) in sorted.iter().enumerate() {
        layer[i] = node;
    }
}

/// Place nodes: x by order within layer (centered), y by layer index.
fn assign_coordinates(by_layer: &[Vec<usize>], config: &LayeredConfig) -> Vec<(f64, f64)> {
    let n: usize = by_layer.iter().map(|l| l.len()).sum();
    let mut positions = vec![(0.0, 0.0); n];

    let max_width = by_layer.iter().map(|l| l.len()).max().unwrap_or(1).max(1);

    // Total width used by the widest layer (for centering narrower layers).
    let widest_span = (max_width.saturating_sub(1)) as f64 * config.node_spacing;

    for (layer_idx, nodes) in by_layer.iter().enumerate() {
        let y = layer_idx as f64 * config.layer_spacing;
        let layer_span = (nodes.len().saturating_sub(1)) as f64 * config.node_spacing;
        let x_offset = (widest_span - layer_span) / 2.0;

        for (i, &node) in nodes.iter().enumerate() {
            let x = x_offset + i as f64 * config.node_spacing;
            positions[node] = (x, y);
        }
    }

    positions
}

/// Scale and translate positions into the viewport with padding.
fn fit_to_viewport(positions: &mut [(f64, f64)], width: f64, height: f64, padding: f64) {
    if positions.is_empty() {
        return;
    }

    let mut min_x = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_y = f64::NEG_INFINITY;
    for &(x, y) in positions.iter() {
        min_x = min_x.min(x);
        max_x = max_x.max(x);
        min_y = min_y.min(y);
        max_y = max_y.max(y);
    }

    let span_x = (max_x - min_x).max(1.0);
    let span_y = (max_y - min_y).max(1.0);
    let inner_w = (width - 2.0 * padding).max(1.0);
    let inner_h = (height - 2.0 * padding).max(1.0);

    // Uniform scale so aspect ratio of the layered structure is preserved,
    // then center inside the viewport.
    let scale = (inner_w / span_x).min(inner_h / span_y);
    let used_w = span_x * scale;
    let used_h = span_y * scale;
    let ox = padding + (inner_w - used_w) / 2.0;
    let oy = padding + (inner_h - used_h) / 2.0;

    for p in positions.iter_mut() {
        p.0 = ox + (p.0 - min_x) * scale;
        p.1 = oy + (p.1 - min_y) * scale;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::{DomainTag, EntityType};

    fn eid(name: &str) -> EntityId {
        EntityId::new(&DomainTag::Code, &EntityType::Function, name, "test")
    }

    #[test]
    fn empty_graph() {
        let positions = layout(&[], &[], 800.0, 600.0, &LayeredConfig::default());
        assert!(positions.is_empty());
    }

    #[test]
    fn single_node_centered() {
        let a = eid("a");
        let positions = layout(
            std::slice::from_ref(&a),
            &[],
            800.0,
            600.0,
            &LayeredConfig::default(),
        );
        assert_eq!(positions.len(), 1);
        let (x, y) = positions[&a];
        assert!((x - 400.0).abs() < 0.01);
        assert!((y - 300.0).abs() < 0.01);
    }

    #[test]
    fn chain_is_strictly_layered() {
        // a → b → c must occupy three distinct y-ranks with increasing y.
        let a = eid("a");
        let b = eid("b");
        let c = eid("c");
        let nodes = vec![a.clone(), b.clone(), c.clone()];
        let edges = vec![(0, 1), (1, 2)];
        let positions = layout(&nodes, &edges, 800.0, 600.0, &LayeredConfig::default());

        let (_, ay) = positions[&a];
        let (_, by) = positions[&b];
        let (_, cy) = positions[&c];
        assert!(ay < by, "source should be above mid: {ay} < {by}");
        assert!(by < cy, "mid should be above sink: {by} < {cy}");
    }

    #[test]
    fn diamond_dag_two_middles_same_layer() {
        //     a
        //    / \
        //   b   c
        //    \ /
        //     d
        let a = eid("a");
        let b = eid("b");
        let c = eid("c");
        let d = eid("d");
        let nodes = vec![a.clone(), b.clone(), c.clone(), d.clone()];
        let edges = vec![(0, 1), (0, 2), (1, 3), (2, 3)];
        let positions = layout(&nodes, &edges, 800.0, 600.0, &LayeredConfig::default());

        let (_, ay) = positions[&a];
        let (bx, by) = positions[&b];
        let (cx, cy) = positions[&c];
        let (_, dy) = positions[&d];

        // b and c share a layer (same y within epsilon after fit).
        assert!(
            (by - cy).abs() < 1.0,
            "b and c should share a layer: by={by} cy={cy}"
        );
        assert!(ay < by && by < dy, "layers top→bottom: a < b/c < d");
        // b and c must be separated horizontally.
        assert!((bx - cx).abs() > 1.0, "siblings must not overlap in x");
    }

    #[test]
    fn cyclic_graph_still_produces_finite_positions() {
        // a → b → c → a
        let a = eid("a");
        let b = eid("b");
        let c = eid("c");
        let nodes = vec![a.clone(), b.clone(), c.clone()];
        let edges = vec![(0, 1), (1, 2), (2, 0)];
        let positions = layout(&nodes, &edges, 800.0, 600.0, &LayeredConfig::default());
        assert_eq!(positions.len(), 3);
        for id in &nodes {
            let (x, y) = positions[id];
            assert!(x.is_finite() && y.is_finite());
            assert!((0.0..=800.0).contains(&x));
            assert!((0.0..=600.0).contains(&y));
        }
    }

    #[test]
    fn disconnected_components_all_positioned() {
        let a = eid("a");
        let b = eid("b");
        let c = eid("c");
        let nodes = vec![a.clone(), b.clone(), c.clone()];
        // only a→b; c is isolated
        let edges = vec![(0, 1)];
        let positions = layout(&nodes, &edges, 800.0, 600.0, &LayeredConfig::default());
        assert_eq!(positions.len(), 3);
        assert!(positions.contains_key(&c));
    }

    #[test]
    fn layout_is_deterministic() {
        let nodes: Vec<_> = (0..6).map(|i| eid(&format!("n{i}"))).collect();
        let edges = vec![(0, 1), (0, 2), (1, 3), (2, 3), (3, 4), (2, 5)];
        let p1 = layout(&nodes, &edges, 1000.0, 800.0, &LayeredConfig::default());
        let p2 = layout(&nodes, &edges, 1000.0, 800.0, &LayeredConfig::default());
        for id in &nodes {
            let (x1, y1) = p1[id];
            let (x2, y2) = p2[id];
            assert!((x1 - x2).abs() < 1e-9 && (y1 - y2).abs() < 1e-9);
        }
    }
}
