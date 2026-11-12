//! Painter — nodes, edges, optional cluster hulls + scrub sweep (G1/G5).
//!
//! Custom egui painter only — no graph library (ADR-067 D1).

use super::encode::{self, NodeShape};
use super::interact::InteractState;
use super::layout::{position_map, PlacedNode};
use super::model::GraphModel;
use super::ViewTime;
use eframe::egui::{self, Color32, Pos2, Rect, Sense, Stroke, Vec2};

const NODE_R: f32 = 14.0;

/// Paint the graph into the available UI rect.
pub fn paint_graph(
    ui: &mut egui::Ui,
    model: &GraphModel,
    placed: &[PlacedNode],
    interact: &InteractState,
    view_time: ViewTime,
) {
    let desired = Vec2::new(ui.available_width(), ui.available_height().max(220.0));
    let (rect, _response) = ui.allocate_exact_size(desired, Sense::hover());
    let painter = ui.painter_at(rect);
    let origin = rect.min;
    let pos = position_map(placed);

    // Edges first.
    for e in &model.edges {
        let Some(&(x0, y0)) = pos.get(&e.source) else {
            continue;
        };
        let Some(&(x1, y1)) = pos.get(&e.target) else {
            continue;
        };
        let style = encode::style_for_edge_kind(&e.kind);
        let dim = interact.edge_dimmed(&e.kind);
        let mut c = style.color;
        if dim {
            c = crate::theming::palette::data_rgba(c.r(), c.g(), c.b(), 40);
        }
        let p0 = origin + Vec2::new(x0, y0);
        let p1 = origin + Vec2::new(x1, y1);
        if style.dashed {
            paint_dashed_line(&painter, p0, p1, Stroke::new(style.width, c));
        } else {
            painter.line_segment([p0, p1], Stroke::new(style.width, c));
        }
    }

    // G5 scaffold: topic hulls when enabled (convex AABB approximation).
    if interact.show_clusters {
        paint_topic_hulls(ui, &painter, origin, model, &pos, interact.lod_collapse);
    }

    // Nodes.
    for n in &model.nodes {
        let Some(&(x, y)) = pos.get(&n.id) else {
            continue;
        };
        let center = origin + Vec2::new(x, y);
        let mut style = encode::style_for_state(n.state);
        if interact.node_dimmed(n) {
            style.opacity *= 0.25;
        }
        let fill = with_opacity(style.fill, style.opacity);
        let outline = with_opacity(style.outline, style.opacity.max(0.5));
        let shape = encode::shape_for_role(n.role);
        match shape {
            NodeShape::Circle | NodeShape::Ring => {
                painter.circle_filled(center, NODE_R, fill);
                painter.circle_stroke(
                    center,
                    NODE_R,
                    Stroke::new(style.outline_width, outline),
                );
            }
            NodeShape::RoundedSquare => {
                let r = Rect::from_center_size(center, Vec2::splat(NODE_R * 1.8));
                painter.rect_filled(r, 4.0, fill);
                painter.rect_stroke(
                    r,
                    4.0,
                    Stroke::new(style.outline_width, outline),
                    egui::epaint::StrokeKind::Inside,
                );
            }
            NodeShape::Diamond => {
                let pts = [
                    center + Vec2::new(0.0, -NODE_R),
                    center + Vec2::new(NODE_R, 0.0),
                    center + Vec2::new(0.0, NODE_R),
                    center + Vec2::new(-NODE_R, 0.0),
                ];
                painter.add(egui::Shape::convex_polygon(
                    pts.to_vec(),
                    fill,
                    Stroke::new(style.outline_width, outline),
                ));
            }
        }
        if style.tombstone {
            painter.line_segment(
                [
                    center + Vec2::new(-NODE_R * 0.6, -NODE_R * 0.6),
                    center + Vec2::new(NODE_R * 0.6, NODE_R * 0.6),
                ],
                Stroke::new(1.5, outline),
            );
        }
        // Selection ring (G3).
        if interact.selected.as_deref() == Some(n.id.as_str()) {
            painter.circle_stroke(center, NODE_R + 4.0, Stroke::new(2.0, Color32::WHITE));
        }
        // Label (LOD: hide when collapsed).
        if !interact.lod_collapse {
            let label = truncate(&n.text, 24);
            painter.text(
                center + Vec2::new(0.0, NODE_R + 10.0),
                egui::Align2::CENTER_TOP,
                label,
                egui::FontId::proportional(11.0),
                Color32::LIGHT_GRAY,
            );
        }
    }

    // G4: scrub sweep line when pinned.
    if let ViewTime::Pinned(t) = view_time {
        // Map t into x using model ts range if possible.
        let xs: Vec<u64> = model.nodes.iter().map(|n| n.ts_ms).collect();
        if let (Some(&min_ts), Some(&max_ts)) = (xs.iter().min(), xs.iter().max()) {
            let span = (max_ts.saturating_sub(min_ts)).max(1) as f32;
            let frac = (t.saturating_sub(min_ts)) as f32 / span;
            let x = rect.left() + frac.clamp(0.0, 1.0) * rect.width();
            painter.vline(
                x,
                rect.y_range(),
                Stroke::new(1.5, crate::theming::palette::CG_HIGHLIGHT),
            );
        }
    }
}

fn paint_topic_hulls(
    _ui: &egui::Ui,
    painter: &egui::Painter,
    origin: Pos2,
    model: &GraphModel,
    pos: &std::collections::HashMap<String, (f32, f32)>,
    lod_collapse: bool,
) {
    if lod_collapse {
        return;
    }
    let mut by_topic: std::collections::HashMap<String, Vec<Pos2>> =
        std::collections::HashMap::new();
    for n in &model.nodes {
        if let Some(topic) = n.classification.topic.as_ref()
            && let Some(&(x, y)) = pos.get(&n.id) {
                by_topic
                    .entry(topic.clone())
                    .or_default()
                    .push(origin + Vec2::new(x, y));
            }
    }
    for (topic, pts) in by_topic {
        if pts.len() < 2 {
            continue;
        }
        let min_x = pts.iter().map(|p| p.x).fold(f32::INFINITY, f32::min);
        let max_x = pts.iter().map(|p| p.x).fold(f32::NEG_INFINITY, f32::max);
        let min_y = pts.iter().map(|p| p.y).fold(f32::INFINITY, f32::min);
        let max_y = pts.iter().map(|p| p.y).fold(f32::NEG_INFINITY, f32::max);
        let pad = 18.0;
        let r = Rect::from_min_max(
            Pos2::new(min_x - pad, min_y - pad),
            Pos2::new(max_x + pad, max_y + pad),
        );
        let hue = encode::topic_hue(&topic);
        let fill = crate::theming::palette::data_rgba(hue.r(), hue.g(), hue.b(), 28);
        painter.rect_filled(r, 8.0, fill);
        painter.rect_stroke(
            r,
            8.0,
            Stroke::new(1.0, hue),
            egui::epaint::StrokeKind::Inside,
        );
    }
}

fn paint_dashed_line(painter: &egui::Painter, a: Pos2, b: Pos2, stroke: Stroke) {
    let delta = b - a;
    let len = delta.length();
    if len < 1.0 {
        return;
    }
    let dir = delta / len;
    let mut t = 0.0;
    let dash = 6.0;
    let gap = 4.0;
    while t < len {
        let t1 = (t + dash).min(len);
        painter.line_segment([a + dir * t, a + dir * t1], stroke);
        t = t1 + gap;
    }
}

fn with_opacity(c: Color32, opacity: f32) -> Color32 {
    let a = (opacity.clamp(0.0, 1.0) * 255.0) as u8;
    crate::theming::palette::data_rgba(c.r(), c.g(), c.b(), a)
}

fn truncate(s: &str, max: usize) -> String {
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i >= max {
            out.push('…');
            break;
        }
        out.push(ch);
    }
    out
}
