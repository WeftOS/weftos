//! Editable Phase 3+ patch UI for `ui://graph` (WEFT-281).
//!
//! Builds on [`super::graph_adapter`] — pure model + ops — and paints an
//! interactive node-link editor with:
//! - click-to-select nodes
//! - drag-to-reposition
//! - inspector (label / kind)
//! - add / remove node
//! - connect mode (pick source then target)
//!
//! Backend is [`GraphEditBackend::CustomPainter`]. The reserved
//! `EguiNodeGraph` variant documents the migration path without
//! introducing the crate (egui pin + wasm cost).
//!
//! Local edits stay in egui temp memory until graph-edit substrate
//! verbs exist; the inspector exposes the serialized patch via
//! [`GraphDocument::to_value`] for future write-back.

use super::graph_adapter::{
    GraphDocument, GraphEdge, GraphEditBackend, GraphNode, GraphPatchEditor,
};
use serde_json::Value;
use std::collections::HashMap;

/// Node visual radius in points.
const NODE_RADIUS: f32 = 18.0;
const CANVAS_HEIGHT: f32 = 320.0;
const MIN_CANVAS_WIDTH: f32 = 240.0;

/// Interaction mode for the canvas.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
enum InteractMode {
    #[default]
    Select,
    /// First click = source, second = target → add edge.
    Connect,
}

/// Per-path edit session held in egui temp data.
#[derive(Clone, Debug)]
struct EditSession {
    doc: GraphDocument,
    /// Fingerprint of the value last ingested (when not dirty).
    source_fp: u64,
    mode: InteractMode,
    /// Pending connect source while in Connect mode.
    connect_from: Option<String>,
    /// Label buffer for the selected-node inspector (avoids fighting focus).
    label_buf: String,
    kind_buf: String,
    backend: GraphEditBackend,
}

impl EditSession {
    fn from_value(value: &Value) -> Self {
        let doc = GraphDocument::from_value(value);
        let source_fp = GraphDocument::source_fingerprint(value);
        Self {
            doc,
            source_fp,
            mode: InteractMode::Select,
            connect_from: None,
            label_buf: String::new(),
            kind_buf: String::new(),
            backend: GraphEditBackend::CustomPainter,
        }
    }

    fn sync_inspector_from_selection(&mut self) {
        match self
            .doc
            .selected
            .as_deref()
            .and_then(|id| self.doc.node(id))
        {
            Some(n) => {
                self.label_buf = n.label.clone();
                self.kind_buf = n.kind.clone().unwrap_or_default();
            }
            None => {
                self.label_buf.clear();
                self.kind_buf.clear();
            }
        }
    }

    fn rebind_if_source_changed(&mut self, value: &Value) {
        let fp = GraphDocument::source_fingerprint(value);
        if fp == self.source_fp {
            return;
        }
        // External republish: adopt new value only when local edits
        // are clean, so we never clobber in-progress patch work.
        if !self.doc.dirty {
            let selected = self.doc.selected.clone();
            self.doc = GraphDocument::from_value(value);
            self.doc.selected = selected.filter(|id| self.doc.nodes.iter().any(|n| n.id == *id));
            self.source_fp = fp;
            self.sync_inspector_from_selection();
        }
    }
}

/// Paint the editable patch UI for `value` at `path`.
pub fn paint(ui: &mut egui::Ui, path: &str, value: &Value) {
    let session_id = egui::Id::new(("weft-graph-edit-session", path));
    let mut session = ui.ctx().data_mut(|d| {
        d.get_temp::<EditSession>(session_id)
            .unwrap_or_else(|| EditSession::from_value(value))
    });
    session.rebind_if_source_changed(value);

    paint_toolbar(ui, &mut session, value);
    ui.add_space(4.0);

    // Canvas + inspector side-by-side when wide enough.
    let wide = ui.available_width() > 420.0;
    if wide {
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                paint_canvas(ui, path, &mut session);
            });
            ui.separator();
            ui.vertical(|ui| {
                ui.set_min_width(160.0);
                ui.set_max_width(200.0);
                paint_inspector(ui, &mut session);
            });
        });
    } else {
        paint_canvas(ui, path, &mut session);
        ui.add_space(4.0);
        paint_inspector(ui, &mut session);
    }

    ui.ctx().data_mut(|d| d.insert_temp(session_id, session));
}

fn paint_toolbar(ui: &mut egui::Ui, session: &mut EditSession, value: &Value) {
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new(format!(
                "patch · {} nodes · {} edges{}",
                session.doc.nodes.len(),
                session.doc.edges.len(),
                if session.doc.dirty { " · dirty" } else { "" }
            ))
            .color(crate::theming::palette::TEXT_SUBTLE)
            .small(),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(
                egui::RichText::new(match session.backend {
                    GraphEditBackend::CustomPainter => "backend: custom",
                    GraphEditBackend::EguiNodeGraph => "backend: egui_node_graph",
                })
                .small()
                .color(crate::theming::palette::GRAPH_EDIT_HINT),
            );
        });
    });

    ui.horizontal(|ui| {
        let select_sel = session.mode == InteractMode::Select;
        if ui.selectable_label(select_sel, "Select").clicked() {
            session.mode = InteractMode::Select;
            session.connect_from = None;
        }
        let connect_sel = session.mode == InteractMode::Connect;
        if ui.selectable_label(connect_sel, "Connect").clicked() {
            session.mode = InteractMode::Connect;
            session.connect_from = None;
        }

        if ui.button("+ Node").clicked() {
            session.doc.add_node("node".into(), None);
            session.sync_inspector_from_selection();
        }

        let can_del = session.doc.selected.is_some();
        if ui
            .add_enabled(can_del, egui::Button::new("Delete"))
            .clicked()
            && let Some(id) = session.doc.selected.clone()
        {
            session.doc.remove_node(&id);
            session.sync_inspector_from_selection();
        }

        if ui
            .add_enabled(session.doc.dirty, egui::Button::new("Reset"))
            .on_hover_text("Discard local edits and re-load substrate value")
            .clicked()
        {
            *session = EditSession::from_value(value);
        }
    });

    if session.mode == InteractMode::Connect {
        let hint = match session.connect_from.as_deref() {
            Some(s) => format!("Connect: pick target for `{s}`"),
            None => "Connect: pick source node".into(),
        };
        ui.label(
            egui::RichText::new(hint)
                .small()
                .color(crate::theming::palette::GRAPH_EDIT_STATUS_OK),
        );
    }
}

fn paint_inspector(ui: &mut egui::Ui, session: &mut EditSession) {
    ui.label(egui::RichText::new("Inspector").strong().small());
    ui.add_space(2.0);

    let Some(sel) = session.doc.selected.clone() else {
        ui.label(
            egui::RichText::new("no selection")
                .italics()
                .small()
                .color(crate::theming::palette::TEXT_LABEL_FAINT),
        );
        return;
    };

    ui.label(
        egui::RichText::new(format!("id · {sel}"))
            .small()
            .monospace()
            .color(crate::theming::palette::TEXT_SUBTLE),
    );

    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("label").small());
        let resp = ui.add(
            egui::TextEdit::singleline(&mut session.label_buf)
                .desired_width(120.0)
                .font(egui::TextStyle::Monospace),
        );
        if resp.changed() {
            session.doc.set_node_label(&sel, session.label_buf.clone());
        }
    });

    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("kind").small());
        let resp = ui.add(
            egui::TextEdit::singleline(&mut session.kind_buf)
                .desired_width(120.0)
                .font(egui::TextStyle::Monospace),
        );
        if resp.changed() {
            let kind = if session.kind_buf.trim().is_empty() {
                None
            } else {
                Some(session.kind_buf.clone())
            };
            session.doc.set_node_kind(&sel, kind);
        }
    });

    // Outgoing edges from selection.
    let outs: Vec<&GraphEdge> = session
        .doc
        .edges
        .iter()
        .filter(|e| e.source == sel)
        .collect();
    if !outs.is_empty() {
        ui.add_space(4.0);
        ui.label(egui::RichText::new("out edges").small().weak());
        let mut to_remove: Option<(String, String)> = None;
        for e in outs {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(format!("→ {}", e.target))
                        .small()
                        .monospace(),
                );
                if ui.small_button("×").clicked() {
                    to_remove = Some((e.source.clone(), e.target.clone()));
                }
            });
        }
        if let Some((s, t)) = to_remove {
            session.doc.remove_edge(&s, &t);
        }
    }
}

fn paint_canvas(ui: &mut egui::Ui, path: &str, session: &mut EditSession) {
    let _ = path;
    if session.doc.nodes.is_empty() {
        ui.label(
            egui::RichText::new("graph: no nodes — use + Node")
                .color(crate::theming::palette::TEXT_SUBTLE)
                .italics(),
        );
        return;
    }

    let canvas_w = ui.available_width().clamp(MIN_CANVAS_WIDTH, 520.0);
    let desired = egui::vec2(canvas_w, CANVAS_HEIGHT);
    let (rect, resp) = ui.allocate_exact_size(desired, egui::Sense::click_and_drag());
    let painter = ui.painter_at(rect);

    painter.rect_filled(rect, 3.0, crate::theming::palette::GRAPH_EDIT_CANVAS_BG);

    let mut positions = layout_session(&session.doc, rect);

    // Drag selected node.
    if resp.dragged()
        && let Some(id) = session.doc.selected.clone()
        && let Some(&old) = positions.get(&id)
    {
        let mut pos = old + resp.drag_delta();
        // Clamp inside canvas with inset.
        let inset = NODE_RADIUS + 4.0;
        pos.x = pos.x.clamp(rect.min.x + inset, rect.max.x - inset);
        pos.y = pos.y.clamp(rect.min.y + inset, rect.max.y - inset);
        // Store as canvas-local coords so layout reuses them.
        let local = pos - rect.min;
        session.doc.set_node_pos(&id, (local.x, local.y));
        positions.insert(id, pos);
    }

    // Edges under nodes.
    for edge in &session.doc.edges {
        let (Some(src), Some(tgt)) = (positions.get(&edge.source), positions.get(&edge.target))
        else {
            continue;
        };
        draw_edge(&painter, *src, *tgt);
    }

    // Hit-test + draw nodes.
    let pointer = resp.interact_pointer_pos();
    let mut clicked_id: Option<String> = None;
    if resp.clicked()
        && let Some(p) = pointer
    {
        clicked_id = hit_test(&session.doc.nodes, &positions, p);
    }

    for node in &session.doc.nodes {
        let Some(pos) = positions.get(&node.id) else {
            continue;
        };
        let selected = session.doc.selected.as_deref() == Some(node.id.as_str());
        let connect_src = session.connect_from.as_deref() == Some(node.id.as_str());
        draw_node(&painter, *pos, node, selected, connect_src);
    }

    if let Some(id) = clicked_id {
        match session.mode {
            InteractMode::Select => {
                session.doc.select_node(Some(id));
                session.sync_inspector_from_selection();
            }
            InteractMode::Connect => {
                if let Some(from) = session.connect_from.take() {
                    if from != id {
                        session.doc.add_edge(&from, &id, None);
                    }
                    session.mode = InteractMode::Select;
                } else {
                    session.connect_from = Some(id);
                }
            }
        }
    } else if resp.clicked() && session.mode == InteractMode::Select {
        // Click empty canvas clears selection.
        session.doc.select_node(None);
        session.sync_inspector_from_selection();
    }
}

fn layout_session(doc: &GraphDocument, rect: egui::Rect) -> HashMap<String, egui::Pos2> {
    let mut out = HashMap::with_capacity(doc.nodes.len());
    let center = rect.center();
    let inset = NODE_RADIUS + 8.0;
    let inner_w = (rect.width() - 2.0 * inset).max(1.0);
    let inner_h = (rect.height() - 2.0 * inset).max(1.0);

    // Prefer explicit pos as canvas-local (origin = rect.min) when set
    // by drag; otherwise ring layout.
    let all_positioned = !doc.nodes.is_empty() && doc.nodes.iter().all(|n| n.pos.is_some());
    if all_positioned {
        // If positions look like canvas-local (within rect size * 2),
        // treat as local; else fit bounding box (substrate-normalised).
        let xs: Vec<f32> = doc
            .nodes
            .iter()
            .filter_map(|n| n.pos.map(|p| p.0))
            .collect();
        let ys: Vec<f32> = doc
            .nodes
            .iter()
            .filter_map(|n| n.pos.map(|p| p.1))
            .collect();
        let max_x = xs.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        let max_y = ys.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        let local_ish = max_x <= rect.width() * 2.0 && max_y <= rect.height() * 2.0 && max_x >= 0.0;

        if local_ish {
            for node in &doc.nodes {
                let (x, y) = node.pos.unwrap_or((0.0, 0.0));
                out.insert(node.id.clone(), rect.min + egui::vec2(x, y));
            }
            return out;
        }

        let min_x = xs.iter().copied().fold(f32::INFINITY, f32::min);
        let min_y = ys.iter().copied().fold(f32::INFINITY, f32::min);
        let span_x = (max_x - min_x).max(1e-6);
        let span_y = (max_y - min_y).max(1e-6);
        for node in &doc.nodes {
            let (x, y) = node.pos.unwrap_or((0.0, 0.0));
            let nx = rect.min.x + inset + ((x - min_x) / span_x) * inner_w;
            let ny = rect.min.y + inset + ((y - min_y) / span_y) * inner_h;
            out.insert(node.id.clone(), egui::pos2(nx, ny));
        }
        return out;
    }

    let n = doc.nodes.len().max(1);
    let radius = (inner_w.min(inner_h) * 0.5 - NODE_RADIUS).max(NODE_RADIUS);
    for (i, node) in doc.nodes.iter().enumerate() {
        let pos = if let Some((x, y)) = node.pos {
            // Canvas-local if plausible, else absolute.
            if x >= 0.0 && y >= 0.0 && x <= rect.width() * 2.0 && y <= rect.height() * 2.0 {
                rect.min + egui::vec2(x, y)
            } else {
                egui::pos2(x, y)
            }
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

fn hit_test(
    nodes: &[GraphNode],
    positions: &HashMap<String, egui::Pos2>,
    pointer: egui::Pos2,
) -> Option<String> {
    // Top-most (last drawn) wins — reverse iterate.
    for node in nodes.iter().rev() {
        let Some(pos) = positions.get(&node.id) else {
            continue;
        };
        let rect =
            egui::Rect::from_center_size(*pos, egui::vec2(NODE_RADIUS * 2.0, NODE_RADIUS * 1.4));
        if rect.expand(2.0).contains(pointer) {
            return Some(node.id.clone());
        }
    }
    None
}

fn draw_edge(painter: &egui::Painter, src: egui::Pos2, tgt: egui::Pos2) {
    let mid = egui::pos2((src.x + tgt.x) * 0.5, (src.y + tgt.y) * 0.5);
    let stroke = egui::Stroke::new(1.5, crate::theming::palette::GRAPH_EDIT_EDGE);
    painter.line_segment([src, mid], stroke);
    painter.line_segment([mid, tgt], stroke);
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

fn draw_node(
    painter: &egui::Painter,
    pos: egui::Pos2,
    node: &GraphNode,
    selected: bool,
    connect_src: bool,
) {
    let fill = kind_color(node.kind.as_deref());
    let rect = egui::Rect::from_center_size(pos, egui::vec2(NODE_RADIUS * 2.0, NODE_RADIUS * 1.4));
    painter.rect_filled(rect, 4.0, fill);
    let stroke_color = if selected {
        crate::theming::palette::GRAPH_EDIT_NODE_SELECTED
    } else if connect_src {
        crate::theming::palette::GRAPH_EDIT_NODE_CONNECT_SRC
    } else {
        crate::theming::palette::GRAPH_EDIT_NODE_OUTLINE
    };
    let stroke_w = if selected || connect_src { 2.0 } else { 1.0 };
    painter.rect_stroke(
        rect,
        4.0,
        egui::Stroke::new(stroke_w, stroke_color),
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

    #[test]
    fn session_from_value_select_edit_roundtrip() {
        let v = json!({
            "nodes": [
                { "id": "n1", "label": "A" },
                { "id": "n2", "label": "B" }
            ],
            "edges": [{ "source": "n1", "target": "n2" }]
        });
        let mut session = EditSession::from_value(&v);
        assert!(!session.doc.dirty);
        session.doc.select_node(Some("n1".into()));
        session.sync_inspector_from_selection();
        assert_eq!(session.label_buf, "A");
        session.label_buf = "Alpha".into();
        session.doc.set_node_label("n1", session.label_buf.clone());
        assert!(session.doc.dirty);
        let out = session.doc.to_value();
        assert_eq!(out["nodes"][0]["label"], "Alpha");
    }

    #[test]
    fn rebind_skips_when_dirty() {
        let v = json!({
            "nodes": [{ "id": "n1", "label": "A" }],
            "edges": []
        });
        let mut session = EditSession::from_value(&v);
        session.doc.set_node_label("n1", "edited".into());
        assert!(session.doc.dirty);

        let v2 = json!({
            "nodes": [{ "id": "n1", "label": "external" }],
            "edges": []
        });
        session.rebind_if_source_changed(&v2);
        // Local edit preserved.
        assert_eq!(session.doc.node("n1").unwrap().label, "edited");
    }

    #[test]
    fn rebind_adopts_when_clean() {
        let v = json!({
            "nodes": [{ "id": "n1", "label": "A" }],
            "edges": []
        });
        let mut session = EditSession::from_value(&v);
        let v2 = json!({
            "nodes": [
                { "id": "n1", "label": "A" },
                { "id": "n2", "label": "B" }
            ],
            "edges": []
        });
        session.rebind_if_source_changed(&v2);
        assert_eq!(session.doc.nodes.len(), 2);
        assert!(!session.doc.dirty);
    }

    #[test]
    fn connect_mode_adds_edge_via_ops() {
        // Mirrors canvas connect: select source then target.
        let v = json!({
            "nodes": ["a", "b", "c"],
            "edges": []
        });
        let mut session = EditSession::from_value(&v);
        session.mode = InteractMode::Connect;
        session.connect_from = Some("a".into());
        // second pick
        let from = session.connect_from.take().unwrap();
        assert!(session.doc.add_edge(&from, "b", None));
        assert_eq!(session.doc.edges.len(), 1);
        assert_eq!(session.doc.edges[0].source, "a");
        assert_eq!(session.doc.edges[0].target, "b");
    }

    #[test]
    fn hit_test_finds_node() {
        let nodes = vec![GraphNode {
            id: "x".into(),
            label: "x".into(),
            kind: None,
            pos: None,
        }];
        let mut positions = HashMap::new();
        positions.insert("x".into(), egui::pos2(100.0, 100.0));
        assert_eq!(
            hit_test(&nodes, &positions, egui::pos2(100.0, 100.0)),
            Some("x".into())
        );
        assert_eq!(hit_test(&nodes, &positions, egui::pos2(0.0, 0.0)), None);
    }
}
