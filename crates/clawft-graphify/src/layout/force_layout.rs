//! Barnes-Hut force-directed layout.
//!
//! Velocity-damped integration with **quadtree-accelerated repulsion**
//! (Barnes–Hut O(n log n)), spring attraction on edges, center gravity,
//! and pairwise collision resolution for moderate graph sizes.
//!
//! The approximation criterion uses `ForceConfig::theta`: a quadtree cell of
//! side length `s` at distance `d` from a body is treated as a single
//! multipole when `s / d < theta`. Setting `theta = 0.0` forces exact
//! leaf-level evaluation (still via the tree).

use crate::entity::EntityId;
use std::collections::HashMap;

/// Force layout configuration — can be tuned by EML LayoutModel.
#[derive(Debug, Clone)]
pub struct ForceConfig {
    pub repulsion: f64,
    pub spring_strength: f64,
    pub spring_length: f64,
    pub damping: f64,
    pub center_gravity: f64,
    pub iterations: usize,
    /// Barnes–Hut opening angle (typical 0.5–0.9). Lower = more accurate / slower.
    pub theta: f64,
    /// Run exact O(n²) pairwise repulsion when `n <= exact_threshold`.
    /// Above this, use Barnes–Hut. Default 32.
    pub exact_threshold: usize,
    /// Minimum node spacing for collision resolution. 0 disables.
    pub min_distance: f64,
    /// Skip collision resolution when `n` exceeds this (repulsion handles spacing).
    pub collision_threshold: usize,
}

impl Default for ForceConfig {
    fn default() -> Self {
        Self {
            repulsion: 400.0,
            spring_strength: 0.08,
            spring_length: 120.0,
            damping: 0.4,
            center_gravity: 0.005,
            iterations: 300,
            theta: 0.8,
            exact_threshold: 32,
            min_distance: 80.0,
            collision_threshold: 256,
        }
    }
}

struct Body {
    id: EntityId,
    x: f64,
    y: f64,
    vx: f64,
    vy: f64,
}

struct Edge {
    source: usize,
    target: usize,
}

// ---------------------------------------------------------------------------
// Barnes–Hut quadtree
// ---------------------------------------------------------------------------

/// Axis-aligned square cell for the Barnes–Hut tree.
struct Quad {
    /// Lower-left corner.
    x0: f64,
    y0: f64,
    /// Side length.
    size: f64,
}

impl Quad {
    fn child(&self, index: usize) -> Quad {
        let half = self.size * 0.5;
        let (ox, oy) = match index {
            0 => (0.0, 0.0),   // SW
            1 => (half, 0.0),  // SE
            2 => (0.0, half),  // NW
            3 => (half, half), // NE
            _ => unreachable!(),
        };
        Quad {
            x0: self.x0 + ox,
            y0: self.y0 + oy,
            size: half,
        }
    }

    fn quadrant_of(&self, x: f64, y: f64) -> usize {
        let mid_x = self.x0 + self.size * 0.5;
        let mid_y = self.y0 + self.size * 0.5;
        let east = if x >= mid_x { 1 } else { 0 };
        let north = if y >= mid_y { 2 } else { 0 };
        east | north
    }
}

/// Internal Barnes–Hut node stored in a flat arena.
///
/// - Leaf with a body: `body_idx = Some(i)`, children empty
/// - Empty leaf: mass == 0, no body, no children
/// - Internal: `children` populated, mass/COM aggregated
struct BhNode {
    bounds: Quad,
    /// Total mass (= body count for unit-mass particles).
    mass: f64,
    com_x: f64,
    com_y: f64,
    /// Single body when this is a leaf that holds one particle.
    body_idx: Option<usize>,
    /// Child indices into the arena (SW, SE, NW, NE). Empty = leaf.
    children: [Option<usize>; 4],
}

impl BhNode {
    fn empty(bounds: Quad) -> Self {
        Self {
            bounds,
            mass: 0.0,
            com_x: 0.0,
            com_y: 0.0,
            body_idx: None,
            children: [None, None, None, None],
        }
    }

    fn is_leaf(&self) -> bool {
        self.children.iter().all(|c| c.is_none())
    }
}

struct BarnesHutTree {
    nodes: Vec<BhNode>,
}

impl BarnesHutTree {
    /// Build a Barnes–Hut tree over all bodies.
    fn build(bodies: &[Body]) -> Self {
        let n = bodies.len();
        if n == 0 {
            return Self { nodes: vec![] };
        }

        // Bounding square with padding so edge points fall strictly inside.
        let mut min_x = f64::MAX;
        let mut min_y = f64::MAX;
        let mut max_x = f64::MIN;
        let mut max_y = f64::MIN;
        for b in bodies {
            min_x = min_x.min(b.x);
            min_y = min_y.min(b.y);
            max_x = max_x.max(b.x);
            max_y = max_y.max(b.y);
        }
        let cx = (min_x + max_x) * 0.5;
        let cy = (min_y + max_y) * 0.5;
        let extent = (max_x - min_x).max(max_y - min_y).max(1.0) * 1.05 + 1.0;
        let half = extent * 0.5;
        let root_bounds = Quad {
            x0: cx - half,
            y0: cy - half,
            size: extent,
        };

        let mut tree = Self {
            // Reserve ~2n nodes; worst case is denser but rare.
            nodes: Vec::with_capacity(n * 2 + 8),
        };
        tree.nodes.push(BhNode::empty(root_bounds));

        for i in 0..n {
            tree.insert(0, i, bodies, 0);
        }
        tree
    }

    fn insert(&mut self, node_idx: usize, body_idx: usize, bodies: &[Body], depth: usize) {
        // Guard against degenerate co-located points.
        if depth > 64 {
            // Absorb into this leaf without subdividing further.
            let body = &bodies[body_idx];
            let node = &mut self.nodes[node_idx];
            let new_mass = node.mass + 1.0;
            node.com_x = (node.com_x * node.mass + body.x) / new_mass;
            node.com_y = (node.com_y * node.mass + body.y) / new_mass;
            node.mass = new_mass;
            return;
        }

        let body = &bodies[body_idx];
        let (bx, by) = (body.x, body.y);

        // Snapshot leaf state before mutation.
        let is_leaf = self.nodes[node_idx].is_leaf();
        let existing = self.nodes[node_idx].body_idx;
        let mass = self.nodes[node_idx].mass;

        if is_leaf && mass == 0.0 {
            // Empty leaf → place body.
            let node = &mut self.nodes[node_idx];
            node.body_idx = Some(body_idx);
            node.mass = 1.0;
            node.com_x = bx;
            node.com_y = by;
            return;
        }

        if is_leaf {
            // Occupied leaf → subdivide, reinsert existing, then insert new.
            if let Some(old_idx) = existing {
                self.subdivide(node_idx);
                self.insert(node_idx, old_idx, bodies, depth + 1);
                self.insert(node_idx, body_idx, bodies, depth + 1);
            } else {
                // Mass > 0 but no body_idx: aggregated deep leaf — just update COM.
                let node = &mut self.nodes[node_idx];
                let new_mass = node.mass + 1.0;
                node.com_x = (node.com_x * node.mass + bx) / new_mass;
                node.com_y = (node.com_y * node.mass + by) / new_mass;
                node.mass = new_mass;
            }
            return;
        }

        // Internal node: update COM then recurse into the correct child.
        {
            let node = &mut self.nodes[node_idx];
            let new_mass = node.mass + 1.0;
            node.com_x = (node.com_x * node.mass + bx) / new_mass;
            node.com_y = (node.com_y * node.mass + by) / new_mass;
            node.mass = new_mass;
            node.body_idx = None;
        }

        let q = self.nodes[node_idx].bounds.quadrant_of(bx, by);
        let child = self.nodes[node_idx].children[q].expect("internal node missing child");
        self.insert(child, body_idx, bodies, depth + 1);
    }

    fn subdivide(&mut self, node_idx: usize) {
        let bounds = {
            let node = &self.nodes[node_idx];
            Quad {
                x0: node.bounds.x0,
                y0: node.bounds.y0,
                size: node.bounds.size,
            }
        };
        let mut children = [None; 4];
        for (i, slot) in children.iter_mut().enumerate() {
            let child_bounds = bounds.child(i);
            let idx = self.nodes.len();
            self.nodes.push(BhNode::empty(child_bounds));
            *slot = Some(idx);
        }
        let node = &mut self.nodes[node_idx];
        node.children = children;
        node.body_idx = None;
        // mass/COM preserved for re-aggregation during reinsert; reset so
        // reinsert rebuilds them cleanly.
        node.mass = 0.0;
        node.com_x = 0.0;
        node.com_y = 0.0;
    }

    /// Accumulate repulsive force on body `i` from the subtree at `node_idx`.
    fn accumulate_repulsion(
        &self,
        node_idx: usize,
        i: usize,
        ctx: &RepulsionCtx<'_>,
        fx: &mut f64,
        fy: &mut f64,
    ) {
        let RepulsionCtx {
            bodies,
            theta,
            repulsion,
            scale,
        } = *ctx;
        let node = &self.nodes[node_idx];
        if node.mass == 0.0 {
            return;
        }

        // Leaf with this body alone → no self-force.
        if node.is_leaf() {
            if let Some(j) = node.body_idx {
                if j == i {
                    return;
                }
                let (dfx, dfy) = pairwise_repulsion(
                    bodies[i].x,
                    bodies[i].y,
                    bodies[j].x,
                    bodies[j].y,
                    1.0,
                    repulsion,
                    scale,
                );
                *fx += dfx;
                *fy += dfy;
                return;
            }
            // Aggregated deep leaf (co-located bodies) — treat as multipole,
            // but skip if COM is essentially this body (self).
            let dx = node.com_x - bodies[i].x;
            let dy = node.com_y - bodies[i].y;
            if dx * dx + dy * dy < 1e-12 {
                return;
            }
            let (dfx, dfy) = pairwise_repulsion(
                bodies[i].x,
                bodies[i].y,
                node.com_x,
                node.com_y,
                node.mass,
                repulsion,
                scale,
            );
            *fx += dfx;
            *fy += dfy;
            return;
        }

        // Internal: Barnes–Hut criterion s/d < theta → approximate.
        let dx = node.com_x - bodies[i].x;
        let dy = node.com_y - bodies[i].y;
        let dist_sq = dx * dx + dy * dy;
        let dist = dist_sq.sqrt();
        if dist < 1e-12 {
            // Body coincides with COM of a cluster — recurse to avoid NaN.
            for child in node.children.iter().flatten() {
                self.accumulate_repulsion(*child, i, ctx, fx, fy);
            }
            return;
        }

        if node.bounds.size / dist < theta {
            let (dfx, dfy) = pairwise_repulsion(
                bodies[i].x,
                bodies[i].y,
                node.com_x,
                node.com_y,
                node.mass,
                repulsion,
                scale,
            );
            *fx += dfx;
            *fy += dfy;
            return;
        }

        for child in node.children.iter().flatten() {
            self.accumulate_repulsion(*child, i, ctx, fx, fy);
        }
    }
}

/// Read-only inputs shared by every step of one repulsion traversal.
#[derive(Clone, Copy)]
struct RepulsionCtx<'a> {
    bodies: &'a [Body],
    theta: f64,
    repulsion: f64,
    scale: f64,
}

/// Repulsive force on body A from a mass at B (Coulomb-like 1/r²).
/// Returns (fx, fy) applied to A (away from B).
#[inline]
fn pairwise_repulsion(
    ax: f64,
    ay: f64,
    bx: f64,
    by: f64,
    mass: f64,
    repulsion: f64,
    scale: f64,
) -> (f64, f64) {
    let dx = ax - bx;
    let dy = ay - by;
    let dist_sq = dx * dx + dy * dy + 1.0; // soften
    let dist = dist_sq.sqrt();
    let force = repulsion * scale * mass / dist_sq;
    (force * dx / dist, force * dy / dist)
}

// ---------------------------------------------------------------------------
// Public layout API
// ---------------------------------------------------------------------------

/// Compute force-directed layout positions.
///
/// `node_ids`: entities to position.
/// `edges`: pairs of (source_idx, target_idx) into node_ids.
///
/// Uses Barnes–Hut O(n log n) repulsion when `n > config.exact_threshold`,
/// otherwise exact pairwise O(n²) repulsion (faster for tiny graphs).
pub fn layout(
    node_ids: &[EntityId],
    edges: &[(usize, usize)],
    width: f64,
    height: f64,
    config: &ForceConfig,
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

    // Initialize positions in a circle.
    let cx = width / 2.0;
    let cy = height / 2.0;
    let radius = (width.min(height) / 4.0).max(50.0);

    let mut bodies: Vec<Body> = node_ids
        .iter()
        .enumerate()
        .map(|(i, id)| {
            let angle = 2.0 * std::f64::consts::PI * (i as f64) / (n as f64);
            Body {
                id: id.clone(),
                x: cx + radius * angle.cos(),
                y: cy + radius * angle.sin(),
                vx: 0.0,
                vy: 0.0,
            }
        })
        .collect();

    let sim_edges: Vec<Edge> = edges
        .iter()
        .map(|&(s, t)| Edge {
            source: s,
            target: t,
        })
        .collect();

    let use_barnes_hut = n > config.exact_threshold;
    let theta = config.theta.max(0.0);

    for iter in 0..config.iterations {
        let alpha = 1.0 - (iter as f64 / config.iterations as f64);
        let alpha_decay = alpha * alpha; // quadratic cooldown

        // --- Repulsion ---
        if use_barnes_hut {
            let tree = BarnesHutTree::build(&bodies);
            if !tree.nodes.is_empty() {
                for i in 0..n {
                    let mut fx = 0.0;
                    let mut fy = 0.0;
                    let ctx = RepulsionCtx {
                        bodies: &bodies,
                        theta,
                        repulsion: config.repulsion,
                        scale: alpha_decay,
                    };
                    tree.accumulate_repulsion(0, i, &ctx, &mut fx, &mut fy);
                    bodies[i].vx += fx;
                    bodies[i].vy += fy;
                }
            }
        } else {
            // Exact pairwise for small n.
            for i in 0..n {
                for j in (i + 1)..n {
                    let dx = bodies[j].x - bodies[i].x;
                    let dy = bodies[j].y - bodies[i].y;
                    let dist_sq = dx * dx + dy * dy + 1.0;
                    let dist = dist_sq.sqrt();
                    let force = config.repulsion * alpha_decay / dist_sq;
                    let fx = force * dx / dist;
                    let fy = force * dy / dist;
                    bodies[i].vx -= fx;
                    bodies[i].vy -= fy;
                    bodies[j].vx += fx;
                    bodies[j].vy += fy;
                }
            }
        }

        // --- Spring attraction on edges ---
        for edge in &sim_edges {
            let s = edge.source;
            let t = edge.target;
            if s >= n || t >= n || s == t {
                continue;
            }
            let dx = bodies[t].x - bodies[s].x;
            let dy = bodies[t].y - bodies[s].y;
            let dist = (dx * dx + dy * dy).sqrt().max(1.0);
            let displacement = dist - config.spring_length;
            let force = config.spring_strength * displacement * alpha_decay;
            let fx = force * dx / dist;
            let fy = force * dy / dist;
            bodies[s].vx += fx;
            bodies[s].vy += fy;
            bodies[t].vx -= fx;
            bodies[t].vy -= fy;
        }

        // --- Center gravity ---
        for body in &mut bodies {
            body.vx += (cx - body.x) * config.center_gravity * alpha_decay;
            body.vy += (cy - body.y) * config.center_gravity * alpha_decay;
        }

        // --- Collision resolution (moderate n only) ---
        if config.min_distance > 0.0 && n <= config.collision_threshold {
            let min_dist = config.min_distance;
            for i in 0..n {
                for j in (i + 1)..n {
                    let dx = bodies[j].x - bodies[i].x;
                    let dy = bodies[j].y - bodies[i].y;
                    let dist = (dx * dx + dy * dy).sqrt().max(0.1);
                    if dist < min_dist {
                        let push = (min_dist - dist) * 0.5;
                        let px = push * dx / dist;
                        let py = push * dy / dist;
                        bodies[i].x -= px;
                        bodies[i].y -= py;
                        bodies[j].x += px;
                        bodies[j].y += py;
                    }
                }
            }
        }

        // --- Integrate with damping ---
        for body in &mut bodies {
            body.vx *= config.damping;
            body.vy *= config.damping;
            let max_v = 50.0;
            body.vx = body.vx.clamp(-max_v, max_v);
            body.vy = body.vy.clamp(-max_v, max_v);
            body.x += body.vx;
            body.y += body.vy;
        }
    }

    bodies.into_iter().map(|b| (b.id, (b.x, b.y))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::{DomainTag, EntityType};

    fn eid(name: &str) -> EntityId {
        EntityId::new(&DomainTag::Code, &EntityType::Module, name, "test")
    }

    fn many_ids(n: usize) -> Vec<EntityId> {
        (0..n).map(|i| eid(&format!("n{i}"))).collect()
    }

    #[test]
    fn two_connected_nodes_near_spring_length() {
        let ids = vec![eid("a"), eid("b")];
        let edges = vec![(0, 1)];
        let positions = layout(&ids, &edges, 600.0, 400.0, &ForceConfig::default());

        let (ax, ay) = positions[&ids[0]];
        let (bx, by) = positions[&ids[1]];
        let dist = ((bx - ax).powi(2) + (by - ay).powi(2)).sqrt();

        // Should settle near spring_length.
        assert!(dist > 60.0 && dist < 250.0, "dist={dist}");
        // Both should be near center.
        assert!(ax > 100.0 && ax < 500.0);
        assert!(ay > 50.0 && ay < 350.0);
    }

    #[test]
    fn disconnected_nodes_repel() {
        let ids = vec![eid("a"), eid("b"), eid("c")];
        let edges: Vec<(usize, usize)> = vec![];
        let positions = layout(&ids, &edges, 600.0, 400.0, &ForceConfig::default());

        // All three should be spread apart.
        let pts: Vec<(f64, f64)> = ids.iter().map(|id| positions[id]).collect();
        for i in 0..3 {
            for j in (i + 1)..3 {
                let dist = ((pts[j].0 - pts[i].0).powi(2) + (pts[j].1 - pts[i].1).powi(2)).sqrt();
                assert!(dist > 30.0, "nodes {i} and {j} too close: {dist}");
            }
        }
    }

    #[test]
    fn single_node_at_center() {
        let ids = vec![eid("solo")];
        let positions = layout(&ids, &[], 600.0, 400.0, &ForceConfig::default());
        let (x, y) = positions[&ids[0]];
        assert!((x - 300.0).abs() < 1.0);
        assert!((y - 200.0).abs() < 1.0);
    }

    #[test]
    fn barnes_hut_path_produces_finite_positions() {
        // Force the Barnes–Hut path (n > exact_threshold).
        let n = 64;
        let ids = many_ids(n);
        let mut edges = Vec::new();
        for i in 0..n - 1 {
            edges.push((i, i + 1));
        }
        // A few long-range links.
        edges.push((0, n / 2));
        edges.push((n / 4, 3 * n / 4));

        let cfg = ForceConfig {
            exact_threshold: 8,
            iterations: 80,
            theta: 0.8,
            ..ForceConfig::default()
        };

        let positions = layout(&ids, &edges, 1000.0, 800.0, &cfg);
        assert_eq!(positions.len(), n);
        for id in &ids {
            let (x, y) = positions[id];
            assert!(
                x.is_finite() && y.is_finite(),
                "non-finite pos for {id:?}: ({x},{y})"
            );
        }
    }

    #[test]
    fn barnes_hut_theta_zero_matches_exact_pairwise() {
        // With theta=0 and forced BH path, leaf evaluation should match exact.
        let n = 20;
        let ids = many_ids(n);
        let edges: Vec<(usize, usize)> = (0..n - 1).map(|i| (i, i + 1)).collect();

        let exact_cfg = ForceConfig {
            exact_threshold: 10_000, // always exact
            iterations: 40,
            min_distance: 0.0, // disable collision for clean comparison
            collision_threshold: 0,
            ..ForceConfig::default()
        };

        let mut bh_cfg = exact_cfg.clone();
        bh_cfg.exact_threshold = 0; // always BH
        bh_cfg.theta = 0.0; // exact leaf evaluation

        let exact = layout(&ids, &edges, 800.0, 600.0, &exact_cfg);
        let bh = layout(&ids, &edges, 800.0, 600.0, &bh_cfg);

        let mut max_err = 0.0_f64;
        for id in &ids {
            let (ex, ey) = exact[id];
            let (bx, by) = bh[id];
            max_err = max_err.max((ex - bx).abs()).max((ey - by).abs());
        }
        // Numerical noise only — same forces, same integration.
        assert!(
            max_err < 1e-6,
            "theta=0 BH should match exact pairwise, max_err={max_err}"
        );
    }

    #[test]
    fn barnes_hut_repels_large_disconnected_set() {
        let n = 48;
        let ids = many_ids(n);
        let edges: Vec<(usize, usize)> = vec![];

        let cfg = ForceConfig {
            exact_threshold: 4,
            iterations: 100,
            min_distance: 0.0,
            ..ForceConfig::default()
        };

        let positions = layout(&ids, &edges, 1200.0, 900.0, &cfg);

        // Sample pairwise distances — disconnected nodes should spread out.
        let mut close_pairs = 0usize;
        let mut total = 0usize;
        for i in 0..n {
            for j in (i + 1)..n {
                let (ax, ay) = positions[&ids[i]];
                let (bx, by) = positions[&ids[j]];
                let dist = ((bx - ax).powi(2) + (by - ay).powi(2)).sqrt();
                total += 1;
                if dist < 5.0 {
                    close_pairs += 1;
                }
            }
        }
        // Allow a few collisions from random stacking but not mass collapse.
        assert!(
            close_pairs < total / 20,
            "too many collapsed pairs: {close_pairs}/{total}"
        );
    }

    #[test]
    fn empty_input_returns_empty() {
        let positions = layout(&[], &[], 600.0, 400.0, &ForceConfig::default());
        assert!(positions.is_empty());
    }
}
