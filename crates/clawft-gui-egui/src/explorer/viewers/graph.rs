//! `GraphViewer` — the `ui://graph` primitive (Vertex analog).
//!
//! Renders a substrate value shaped as `{ nodes: [...], edges: [...] }`
//! as a 2-D node-link diagram. Read-only by default per ADOPTION §9: the
//! same primitive must eventually serve the Explorer's graph-view
//! toggle AND vector-synth's ⊃μBus patch UI.
//!
//! ## Phase 3+ edit (WEFT-281)
//!
//! Toggle **Edit** in the header (or set `"editable": true` on the
//! value) to open the interactive patch UI in [`super::graph_edit`].
//! Model + ops live in [`super::graph_adapter`] — the migration seam
//! for a future `egui_node_graph` backend (not linked today: egui pin
//! + wasm budget).
//!
//! ## Library choice
//!
//! Rolled our own on top of egui's 2-D painter. Tradeoffs:
//! - `egui_node_graph` is editor-grade and opinionated (typed ports,
//!   live evaluation, drag-to-connect). Good for the patch editor;
//!   overkill for the read-only Explorer primitive and carries an
//!   egui-version pin.
//! - `egui_graphs` is visualisation-grade but pulls in `petgraph`
//!   and has its own egui-version coupling.
//! - Rolling our own costs ~250 lines of painter code, zero new
//!   deps, no WASM bloat, and keeps the surface stable when we do
//!   graduate to `egui_node_graph` for the editable Phase 3+ patch UI
//!   — the adapter boundary (JSON `Value` → node/edge lists) is the
//!   migration seam.
//!
//! ## Shape tolerance (matches)
//!
//! `nodes`: either
//!   - `[{ "id": "n1", "label"?: "...", "kind"?: "...", "pos"?: [x, y] }, ...]`
//!   - `[ "n1", "n2", ... ]` (bare-id array)
//!
//! `edges`: either
//!   - `[{ "source": "n1", "target": "n2", "kind"?, "label"? }, ...]`
//!   - `[[ "n1", "n2" ], ...]` (pair array)
//!   - endpoints may themselves be `[node_id, port_index]` to match
//!     the vector-synth `Cable { from: (NodeId, u8), to: (NodeId, u8) }`
//!     shape — we render using the node-id half and ignore the port
//!     index for the MVP.
//!
//! ## Layout fallback
//!
//! If every node has a `pos`, we use those. Otherwise we lay nodes
//! out on a circle sized to the viewer width — deterministic, cheap,
//! no force iteration, no state. For dense graphs the user sees
//! overlapping edges — acceptable for MVP.

use super::SubstrateViewer;
use serde_json::Value;
use std::collections::HashMap;

pub struct GraphViewer;

/// Priority 14: above ChainTail / MeshNodes (12), below Waveform (15).
/// Per PHASE-2-PLAN Track 3 acceptance.
const PRIORITY: u32 = 14;

/// Node visual radius in px (canvas units are egui points).
const NODE_RADIUS: f32 = 18.0;
/// Desired canvas height in the detail pane.
const CANVAS_HEIGHT: f32 = 320.0;
/// Minimum canvas width — on very narrow panels we still want room
/// for a handful of nodes.
const MIN_CANVAS_WIDTH: f32 = 240.0;

impl SubstrateViewer for GraphViewer {
    fn matches(value: &Value) -> u32 {
        let Some(obj) = value.as_object() else {
            return 0;
        };
        let Some(nodes) = obj.get("nodes").and_then(Value::as_array) else {
            return 0;
        };
        let Some(edges) = obj.get("edges").and_then(Value::as_array) else {
            return 0;
        };
        // Empty graphs are still valid `ui://graph` values (e.g. a
        // freshly-initialised patch). Require both arrays to exist but
        // not to be non-empty.
        if !nodes.iter().all(is_valid_node_shape) {
            return 0;
        }
        if !edges.iter().all(is_valid_edge_shape) {
            return 0;
        }
        PRIORITY
    }

    fn paint(ui: &mut egui::Ui, path: &str, value: &Value) {
        let obj = match value.as_object() {
            Some(o) => o,
            None => return,
        };
        let nodes_raw = obj.get("nodes").and_then(Value::as_array);
        let edges_raw = obj.get("edges").and_then(Value::as_array);

        let nodes: Vec<GraphNode> = nodes_raw
            .map(|a| a.iter().filter_map(GraphNode::from_value).collect())
            .unwrap_or_default();
        let edges: Vec<GraphEdge> = edges_raw
            .map(|a| a.iter().filter_map(GraphEdge::from_value).collect())
            .unwrap_or_default();

        // WEFT-281: Edit toggle (or substrate `"editable": true`) routes
        // to the Phase 3+ patch UI. Default remains read-only so existing
        // fixtures and Explorer detail panes are unchanged.
        let force_edit = obj
            .get("editable")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let edit_id = egui::Id::new(("weft-graph-edit-mode", path));
        let mut edit_mode = force_edit
            || ui
                .ctx()
                .data_mut(|d| d.get_temp::<bool>(edit_id).unwrap_or(false));

        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(format!(
                    "graph · {path}  ({} nodes, {} edges)",
                    nodes.len(),
                    edges.len()
                ))
                .color(crate::theming::palette::TEXT_SUBTLE)
                .small(),
            );
            if !force_edit {
                if ui
                    .toggle_value(&mut edit_mode, "Edit")
                    .on_hover_text("Phase 3+ patch UI — select, edit, connect nodes")
                    .changed()
                {
                    ui.ctx().data_mut(|d| d.insert_temp(edit_id, edit_mode));
                }
            } else {
                ui.label(
                    egui::RichText::new("editable")
                        .small()
                        .color(crate::theming::palette::GRAPH_EDIT_STATUS_OK),
                );
            }
        });
        ui.add_space(4.0);

        if edit_mode {
            super::graph_edit::paint(ui, path, value);
            return;
        }

        if nodes.is_empty() {
            ui.label(
                egui::RichText::new("graph: no nodes")
                    .color(crate::theming::palette::TEXT_SUBTLE)
                    .italics(),
            );
            return;
        }

        let canvas_w = ui.available_width().max(MIN_CANVAS_WIDTH);
        let desired = egui::vec2(canvas_w, CANVAS_HEIGHT);
        let (rect, _resp) = ui.allocate_exact_size(desired, egui::Sense::hover());
        let painter = ui.painter_at(rect);

        // Background.
        painter.rect_filled(rect, 3.0, crate::theming::palette::GRAPH_EDIT_CANVAS_BG);

        // Position every node in canvas space.
        let positions = layout(&nodes, rect);

        // Draw edges first so nodes paint on top.
        for edge in &edges {
            let (Some(src), Some(tgt)) = (positions.get(&edge.source), positions.get(&edge.target))
            else {
                continue;
            };
            draw_edge(&painter, *src, *tgt, edge.kind.as_deref());
        }

        // Draw nodes.
        for node in &nodes {
            let Some(pos) = positions.get(&node.id) else {
                continue;
            };
            draw_node(&painter, *pos, node);
        }
    }
}

// ─── shape predicates ────────────────────────────────────────────────

fn is_valid_node_shape(v: &Value) -> bool {
    // Bare id: string, number, or bool — anything Value::as_str-able or
    // coercible to a stable id. We accept string, u64, i64.
    match v {
        Value::String(_) => true,
        Value::Number(_) => true,
        Value::Object(obj) => {
            // Must have at least one of: id, label.
            obj.get("id").is_some() || obj.get("label").is_some()
        }
        _ => false,
    }
}

fn is_valid_edge_shape(v: &Value) -> bool {
    match v {
        Value::Array(a) => {
            a.len() == 2 && endpoint_id(&a[0]).is_some() && endpoint_id(&a[1]).is_some()
        }
        Value::Object(obj) => {
            let has_src = obj
                .get("source")
                .map(endpoint_id)
                .is_some_and(|o| o.is_some())
                || obj
                    .get("from")
                    .map(endpoint_id)
                    .is_some_and(|o| o.is_some());
            let has_tgt = obj
                .get("target")
                .map(endpoint_id)
                .is_some_and(|o| o.is_some())
                || obj.get("to").map(endpoint_id).is_some_and(|o| o.is_some());
            has_src && has_tgt
        }
        _ => false,
    }
}

/// Extract a stable id from an endpoint. Accepts:
/// - `"n1"` or `42` (bare id)
/// - `["n1", 0]` or `[42, 3]` (node-id + port index — we drop the port
///   for the MVP and key on the node id only)
fn endpoint_id(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Array(a) => a.first().and_then(endpoint_id),
        _ => None,
    }
}

// ─── model ──────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
struct GraphNode {
    id: String,
    label: String,
    kind: Option<String>,
    pos: Option<(f32, f32)>,
}

impl GraphNode {
    fn from_value(v: &Value) -> Option<Self> {
        match v {
            Value::String(s) => Some(Self {
                id: s.clone(),
                label: s.clone(),
                kind: None,
                pos: None,
            }),
            Value::Number(n) => Some(Self {
                id: n.to_string(),
                label: n.to_string(),
                kind: None,
                pos: None,
            }),
            Value::Object(obj) => {
                let id = obj
                    .get("id")
                    .and_then(|x| match x {
                        Value::String(s) => Some(s.clone()),
                        Value::Number(n) => Some(n.to_string()),
                        _ => None,
                    })
                    .or_else(|| obj.get("label").and_then(Value::as_str).map(str::to_owned))?;
                let label = obj
                    .get("label")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_else(|| id.clone());
                let kind = obj.get("kind").and_then(Value::as_str).map(str::to_owned);
                let pos = obj.get("pos").and_then(|p| p.as_array()).and_then(|a| {
                    if a.len() != 2 {
                        return None;
                    }
                    let x = a[0].as_f64()? as f32;
                    let y = a[1].as_f64()? as f32;
                    Some((x, y))
                });
                Some(Self {
                    id,
                    label,
                    kind,
                    pos,
                })
            }
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
struct GraphEdge {
    source: String,
    target: String,
    kind: Option<String>,
}

impl GraphEdge {
    fn from_value(v: &Value) -> Option<Self> {
        match v {
            Value::Array(a) if a.len() == 2 => {
                let source = endpoint_id(&a[0])?;
                let target = endpoint_id(&a[1])?;
                Some(Self {
                    source,
                    target,
                    kind: None,
                })
            }
            Value::Object(obj) => {
                let source = obj
                    .get("source")
                    .or_else(|| obj.get("from"))
                    .and_then(endpoint_id)?;
                let target = obj
                    .get("target")
                    .or_else(|| obj.get("to"))
                    .and_then(endpoint_id)?;
                let kind = obj.get("kind").and_then(Value::as_str).map(str::to_owned);
                Some(Self {
                    source,
                    target,
                    kind,
                })
            }
            _ => None,
        }
    }
}

// ─── layout ─────────────────────────────────────────────────────────

/// Returns a map of `node.id -> canvas position`. Uses explicit `pos`
/// for nodes that supply one; falls back to evenly-spaced circular
/// layout for the rest. Mixed graphs (some positioned, some not) lay
/// the unpositioned ones around the centre.
fn layout(nodes: &[GraphNode], rect: egui::Rect) -> HashMap<String, egui::Pos2> {
    let mut out = HashMap::with_capacity(nodes.len());
    let center = rect.center();
    let inset = NODE_RADIUS + 8.0;
    let inner_w = (rect.width() - 2.0 * inset).max(1.0);
    let inner_h = (rect.height() - 2.0 * inset).max(1.0);

    // If every node has a `pos`, treat the `pos` values as already-
    // normalised coordinates and just re-scale into canvas space via
    // a bounding-box fit.
    let all_positioned = !nodes.is_empty() && nodes.iter().all(|n| n.pos.is_some());
    if all_positioned {
        let xs: Vec<f32> = nodes.iter().filter_map(|n| n.pos.map(|p| p.0)).collect();
        let ys: Vec<f32> = nodes.iter().filter_map(|n| n.pos.map(|p| p.1)).collect();
        let (min_x, max_x) = (min_f32(&xs), max_f32(&xs));
        let (min_y, max_y) = (min_f32(&ys), max_f32(&ys));
        let span_x = (max_x - min_x).max(1e-6);
        let span_y = (max_y - min_y).max(1e-6);
        for node in nodes {
            let (x, y) = node.pos.unwrap_or((0.0, 0.0));
            let nx = rect.min.x + inset + ((x - min_x) / span_x) * inner_w;
            let ny = rect.min.y + inset + ((y - min_y) / span_y) * inner_h;
            out.insert(node.id.clone(), egui::pos2(nx, ny));
        }
        return out;
    }

    // Fallback: circular layout. Cheap, deterministic, no force sim.
    let n = nodes.len().max(1);
    let radius = (inner_w.min(inner_h) * 0.5 - NODE_RADIUS).max(NODE_RADIUS);
    for (i, node) in nodes.iter().enumerate() {
        // Honour explicit pos if given, otherwise slot on the ring.
        let pos = if let Some((x, y)) = node.pos {
            egui::pos2(x, y)
        } else {
            let theta =
                (i as f32) / (n as f32) * std::f32::consts::TAU - std::f32::consts::FRAC_PI_2;
            egui::pos2(
                center.x + radius * theta.cos(),
                center.y + radius * theta.sin(),
            )
        };
        out.insert(node.id.clone(), pos);
    }
    out
}

fn min_f32(xs: &[f32]) -> f32 {
    xs.iter().copied().fold(f32::INFINITY, f32::min)
}

fn max_f32(xs: &[f32]) -> f32 {
    xs.iter().copied().fold(f32::NEG_INFINITY, f32::max)
}

// ─── drawing ────────────────────────────────────────────────────────

fn draw_edge(painter: &egui::Painter, src: egui::Pos2, tgt: egui::Pos2, _kind: Option<&str>) {
    // Straight line with a midpoint control point offset — gives a
    // gentle bezier feel without a full cubic calc. Good enough for
    // the MVP; when we graduate to a real graph lib we inherit its
    // routing.
    let mid = egui::pos2((src.x + tgt.x) * 0.5, (src.y + tgt.y) * 0.5);
    let stroke = egui::Stroke::new(1.5, crate::theming::palette::GRAPH_EDIT_EDGE);
    painter.line_segment([src, mid], stroke);
    painter.line_segment([mid, tgt], stroke);

    // Small arrowhead at target.
    let dir = (tgt - src).normalized();
    if dir.length_sq() > 0.0 {
        let perp = egui::vec2(-dir.y, dir.x);
        let head_size = 6.0;
        let tail = tgt - dir * (NODE_RADIUS + 1.0);
        let a = tail - dir * head_size + perp * (head_size * 0.5);
        let b = tail - dir * head_size - perp * (head_size * 0.5);
        painter.line_segment([tail, a], stroke);
        painter.line_segment([tail, b], stroke);
    }
}

fn draw_node(painter: &egui::Painter, pos: egui::Pos2, node: &GraphNode) {
    let fill = kind_color(node.kind.as_deref());
    let rect = egui::Rect::from_center_size(pos, egui::vec2(NODE_RADIUS * 2.0, NODE_RADIUS * 1.4));
    painter.rect_filled(rect, 4.0, fill);
    painter.rect_stroke(
        rect,
        4.0,
        egui::Stroke::new(1.0, crate::theming::palette::GRAPH_EDIT_NODE_OUTLINE),
        egui::StrokeKind::Inside,
    );
    painter.text(
        pos,
        egui::Align2::CENTER_CENTER,
        truncate(&node.label, 10),
        egui::FontId::monospace(10.0),
        crate::theming::palette::GRAPH_EDIT_NODE_LABEL,
    );
}

fn kind_color(kind: Option<&str>) -> egui::Color32 {
    // Stable hash-ish mapping — keep deterministic so the same kind
    // always gets the same colour across renders.
    match kind {
        None => crate::theming::palette::GRAPH_EDIT_KIND_NONE,
        Some(k) => {
            let h: u32 = k.bytes().fold(2166136261u32, |acc, b| {
                acc.wrapping_mul(16777619).wrapping_add(b as u32)
            });
            let r = 60 + ((h & 0x3F) as u8);
            let g = 70 + (((h >> 6) & 0x3F) as u8);
            let b = 100 + (((h >> 12) & 0x3F) as u8);
            crate::theming::palette::data_rgb(r, g, b)
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        let truncated: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{truncated}…")
    }
}

// ─── tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ─ matches: positive ─────────────────────────────────────────

    #[test]
    fn matches_full_typed_shape() {
        let v = json!({
            "nodes": [
                { "id": "n1", "label": "Oscillator", "kind": "osc", "pos": [0.0, 0.0] },
                { "id": "n2", "label": "Filter",     "kind": "vcf", "pos": [1.0, 1.0] }
            ],
            "edges": [
                { "source": "n1", "target": "n2", "kind": "audio" }
            ]
        });
        assert_eq!(GraphViewer::matches(&v), PRIORITY);
    }

    #[test]
    fn matches_bare_id_nodes_and_pair_edges() {
        let v = json!({
            "nodes": ["n1", "n2", "n3"],
            "edges": [["n1", "n2"], ["n2", "n3"]]
        });
        assert_eq!(GraphViewer::matches(&v), PRIORITY);
    }

    #[test]
    fn matches_numeric_ids() {
        // Vector-synth NodeId is u32; bare numeric ids must match.
        let v = json!({
            "nodes": [0, 1, 2],
            "edges": [[0, 1], [1, 2]]
        });
        assert_eq!(GraphViewer::matches(&v), PRIORITY);
    }

    #[test]
    fn matches_vector_synth_cable_shape() {
        // `from: [node_id, port]`, `to: [node_id, port]` — matches
        // vector-synth's Cable { from: (NodeId, u8), to: (NodeId, u8) }
        // once serialised.
        let v = json!({
            "nodes": [0, 1],
            "edges": [
                { "from": [0, 0], "to": [1, 2] }
            ]
        });
        assert_eq!(GraphViewer::matches(&v), PRIORITY);
    }

    #[test]
    fn matches_empty_arrays() {
        let v = json!({ "nodes": [], "edges": [] });
        assert_eq!(GraphViewer::matches(&v), PRIORITY);
    }

    #[test]
    fn matches_mixed_node_forms() {
        let v = json!({
            "nodes": [
                "n1",
                { "id": "n2", "label": "B" }
            ],
            "edges": [["n1", "n2"]]
        });
        assert_eq!(GraphViewer::matches(&v), PRIORITY);
    }

    // ─ matches: negative ─────────────────────────────────────────

    #[test]
    fn rejects_missing_edges() {
        let v = json!({ "nodes": ["n1", "n2"] });
        assert_eq!(GraphViewer::matches(&v), 0);
    }

    #[test]
    fn rejects_missing_nodes() {
        let v = json!({ "edges": [["n1", "n2"]] });
        assert_eq!(GraphViewer::matches(&v), 0);
    }

    #[test]
    fn rejects_nodes_not_array() {
        let v = json!({ "nodes": "not-an-array", "edges": [] });
        assert_eq!(GraphViewer::matches(&v), 0);
    }

    #[test]
    fn rejects_edges_not_array() {
        let v = json!({ "nodes": [], "edges": {} });
        assert_eq!(GraphViewer::matches(&v), 0);
    }

    #[test]
    fn rejects_invalid_node_element() {
        // nested array in node position is nonsensical
        let v = json!({
            "nodes": [["a", "b"]], // array-of-arrays in nodes
            "edges": []
        });
        assert_eq!(GraphViewer::matches(&v), 0);
    }

    #[test]
    fn rejects_object_node_without_id_or_label() {
        let v = json!({
            "nodes": [{ "kind": "osc" }],
            "edges": []
        });
        assert_eq!(GraphViewer::matches(&v), 0);
    }

    #[test]
    fn rejects_malformed_edge_pair() {
        // three-element array is not a pair
        let v = json!({
            "nodes": ["a", "b", "c"],
            "edges": [["a", "b", "c"]]
        });
        assert_eq!(GraphViewer::matches(&v), 0);
    }

    #[test]
    fn rejects_edge_with_only_source() {
        let v = json!({
            "nodes": ["a", "b"],
            "edges": [{ "source": "a" }]
        });
        assert_eq!(GraphViewer::matches(&v), 0);
    }

    #[test]
    fn rejects_non_object() {
        assert_eq!(GraphViewer::matches(&Value::Null), 0);
        assert_eq!(GraphViewer::matches(&json!([])), 0);
        assert_eq!(GraphViewer::matches(&json!("hello")), 0);
    }

    #[test]
    fn rejects_audio_meter_shape() {
        // Sanity check vs neighbouring viewer — AudioMeter shape must
        // not collide with ours.
        let v = json!({ "rms_db": -41.2, "peak_db": -17.1 });
        assert_eq!(GraphViewer::matches(&v), 0);
    }

    // ─ endpoint_id helper ────────────────────────────────────────

    #[test]
    fn endpoint_id_string() {
        assert_eq!(endpoint_id(&json!("n1")), Some("n1".into()));
    }

    #[test]
    fn endpoint_id_number() {
        assert_eq!(endpoint_id(&json!(42)), Some("42".into()));
    }

    #[test]
    fn endpoint_id_array_takes_first() {
        assert_eq!(endpoint_id(&json!(["n1", 3])), Some("n1".into()));
        assert_eq!(endpoint_id(&json!([7, 0])), Some("7".into()));
    }

    #[test]
    fn endpoint_id_rejects_bool_and_null() {
        assert_eq!(endpoint_id(&Value::Null), None);
        assert_eq!(endpoint_id(&json!(true)), None);
    }

    // ─ GraphNode / GraphEdge parsing ─────────────────────────────

    #[test]
    fn graph_node_from_bare_string() {
        let n = GraphNode::from_value(&json!("osc1")).unwrap();
        assert_eq!(n.id, "osc1");
        assert_eq!(n.label, "osc1");
        assert!(n.kind.is_none());
        assert!(n.pos.is_none());
    }

    #[test]
    fn graph_node_from_typed_object() {
        let n = GraphNode::from_value(&json!({
            "id": "n7",
            "label": "LFO",
            "kind": "lfo",
            "pos": [12.5, -3.0]
        }))
        .unwrap();
        assert_eq!(n.id, "n7");
        assert_eq!(n.label, "LFO");
        assert_eq!(n.kind.as_deref(), Some("lfo"));
        assert_eq!(n.pos, Some((12.5, -3.0)));
    }

    #[test]
    fn graph_node_label_only_falls_back_to_label_as_id() {
        let n = GraphNode::from_value(&json!({ "label": "named" })).unwrap();
        assert_eq!(n.id, "named");
        assert_eq!(n.label, "named");
    }

    #[test]
    fn graph_edge_pair_form() {
        let e = GraphEdge::from_value(&json!(["a", "b"])).unwrap();
        assert_eq!(e.source, "a");
        assert_eq!(e.target, "b");
        assert!(e.kind.is_none());
    }

    #[test]
    fn graph_edge_object_form_with_kind() {
        let e = GraphEdge::from_value(&json!({
            "source": "a",
            "target": "b",
            "kind": "cv"
        }))
        .unwrap();
        assert_eq!(e.source, "a");
        assert_eq!(e.target, "b");
        assert_eq!(e.kind.as_deref(), Some("cv"));
    }

    #[test]
    fn graph_edge_from_to_alias() {
        let e = GraphEdge::from_value(&json!({
            "from": [0, 0],
            "to": [1, 2]
        }))
        .unwrap();
        assert_eq!(e.source, "0");
        assert_eq!(e.target, "1");
    }

    // ─ layout ────────────────────────────────────────────────────

    #[test]
    fn layout_positions_all_nodes() {
        let nodes = vec![
            GraphNode {
                id: "a".into(),
                label: "a".into(),
                kind: None,
                pos: None,
            },
            GraphNode {
                id: "b".into(),
                label: "b".into(),
                kind: None,
                pos: None,
            },
            GraphNode {
                id: "c".into(),
                label: "c".into(),
                kind: None,
                pos: None,
            },
        ];
        let rect = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(400.0, 320.0));
        let positions = layout(&nodes, rect);
        assert_eq!(positions.len(), 3);
        for id in ["a", "b", "c"] {
            let p = positions.get(id).unwrap();
            assert!(rect.contains(*p), "node {id} position {p:?} outside rect");
        }
    }

    #[test]
    fn layout_honours_supplied_pos_when_all_positioned() {
        let nodes = vec![
            GraphNode {
                id: "a".into(),
                label: "a".into(),
                kind: None,
                pos: Some((0.0, 0.0)),
            },
            GraphNode {
                id: "b".into(),
                label: "b".into(),
                kind: None,
                pos: Some((1.0, 1.0)),
            },
        ];
        let rect = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(400.0, 320.0));
        let positions = layout(&nodes, rect);
        let a = positions.get("a").unwrap();
        let b = positions.get("b").unwrap();
        // `a` mapped to min-corner, `b` mapped to max-corner after inset.
        assert!(a.x < b.x);
        assert!(a.y < b.y);
    }

    #[test]
    fn layout_empty_nodes_is_empty() {
        let rect = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(400.0, 320.0));
        let positions = layout(&[], rect);
        assert!(positions.is_empty());
    }

    // ─ truncate helper ───────────────────────────────────────────

    #[test]
    fn truncate_short_unchanged() {
        assert_eq!(truncate("osc", 10), "osc");
    }

    #[test]
    fn truncate_long_gets_ellipsis() {
        let t = truncate("superduperlongname", 10);
        assert!(t.ends_with('…'));
        assert!(t.chars().count() <= 10);
    }

    // ─ WEFT-281 edit flag (matches still succeeds) ───────────────

    #[test]
    fn matches_with_editable_flag() {
        // `"editable": true` is advisory for paint routing; shape match
        // still requires nodes + edges arrays.
        let v = json!({
            "nodes": ["a", "b"],
            "edges": [["a", "b"]],
            "editable": true
        });
        assert_eq!(GraphViewer::matches(&v), PRIORITY);
    }
}
