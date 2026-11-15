//! Timeline layout — ADR-067 D3 (time on X, lane on Y).
//!
//! Deterministic and stateless per frame given `(nodes, edges, size)`.

use super::model::{GraphModel, NodeRole, NodeStateTag};
use eframe::egui::Vec2;
use std::collections::HashMap;

/// Placed node in canvas coordinates (relative to canvas origin).
#[derive(Debug, Clone)]
pub struct PlacedNode {
    pub id: String,
    pub x: f32,
    pub y: f32,
    pub lane: Lane,
}

/// Causal / thread lane (design §1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    Main,
    Spec,
    Fork,
    Speaker,
}

/// Padding and spacing constants (points).
const PAD_X: f32 = 48.0;
const PAD_Y: f32 = 40.0;
const LANE_GAP: f32 = 56.0;

/// Assign lanes and map timestamps to X within `size`.
pub fn layout_timeline(model: &GraphModel, size: Vec2) -> Vec<PlacedNode> {
    if model.nodes.is_empty() {
        return Vec::new();
    }

    let min_ts = model.nodes.iter().map(|n| n.ts_ms).min().unwrap_or(0);
    let max_ts = model.nodes.iter().map(|n| n.ts_ms).max().unwrap_or(min_ts);
    let span = (max_ts.saturating_sub(min_ts)).max(1) as f32;
    let usable_w = (size.x - 2.0 * PAD_X).max(1.0);

    let mut placed = Vec::with_capacity(model.nodes.len());
    for n in &model.nodes {
        let lane = assign_lane(n.role, n.state);
        let t = (n.ts_ms.saturating_sub(min_ts)) as f32 / span;
        let x = PAD_X + t * usable_w;
        let y = PAD_Y + lane_index(lane) as f32 * LANE_GAP;
        placed.push(PlacedNode {
            id: n.id.clone(),
            x,
            y,
            lane,
        });
    }
    placed
}

fn assign_lane(role: NodeRole, state: NodeStateTag) -> Lane {
    if role == NodeRole::Speaker {
        return Lane::Speaker;
    }
    if state == NodeStateTag::Speculative {
        return Lane::Spec;
    }
    Lane::Main
}

fn lane_index(lane: Lane) -> i32 {
    match lane {
        Lane::Spec => 0,
        Lane::Main => 1,
        Lane::Fork => 2,
        Lane::Speaker => 3,
    }
}

/// Lookup map id → position for edge drawing.
pub fn position_map(placed: &[PlacedNode]) -> HashMap<String, (f32, f32)> {
    placed.iter().map(|p| (p.id.clone(), (p.x, p.y))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_main_lane_ordered_by_time() {
        let m = GraphModel::fixture();
        let placed = layout_timeline(&m, Vec2::new(800.0, 400.0));
        assert!(!placed.is_empty());

        let mut main: Vec<_> = placed.iter().filter(|p| p.lane == Lane::Main).collect();
        main.sort_by(|a, b| a.x.partial_cmp(&b.x).unwrap());
        // xs strictly non-decreasing for main turns with increasing ts
        for w in main.windows(2) {
            assert!(w[0].x <= w[1].x);
        }

        // Speaker off main lane
        assert!(placed.iter().any(|p| p.lane == Lane::Speaker));
    }

    #[test]
    fn empty_model_empty_layout() {
        let m = GraphModel::default();
        assert!(layout_timeline(&m, Vec2::new(100.0, 100.0)).is_empty());
    }
}
