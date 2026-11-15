//! `MeshNodesViewer` — renders mesh membership / health data shaped
//! like `{ total_nodes, healthy_nodes, nodes?: [...] }`.
//!
//! Priority 12 — above the JSON fallback (1) and the ConnectionBadge
//! (10) catchalls, below the richer domain-specific viewers
//! (WaveformViewer at 15).
//!
//! Once this viewer ships, the existing Mesh chip panel becomes a
//! candidate for replacement: select the mesh substrate path in the
//! Explorer and the counters + node list render without going through
//! the chip layer.

use super::SubstrateViewer;
use serde_json::Value;

pub struct MeshNodesViewer;

impl SubstrateViewer for MeshNodesViewer {
    fn matches(value: &Value) -> u32 {
        let Some(obj) = value.as_object() else {
            return 0;
        };
        let total = obj.get("total_nodes").and_then(Value::as_u64);
        let healthy = obj.get("healthy_nodes").and_then(Value::as_u64);
        if total.is_none() || healthy.is_none() {
            return 0;
        }
        12
    }

    fn paint(ui: &mut egui::Ui, path: &str, value: &Value) {
        let obj = match value.as_object() {
            Some(o) => o,
            None => return,
        };
        let total = obj.get("total_nodes").and_then(Value::as_u64).unwrap_or(0);
        let healthy = obj
            .get("healthy_nodes")
            .and_then(Value::as_u64)
            .unwrap_or(0);

        ui.label(
            egui::RichText::new(format!("mesh · {path}"))
                .color(crate::theming::palette::TEXT_SUBTLE)
                .small(),
        );
        ui.add_space(4.0);

        // Big counters.
        ui.horizontal(|ui| {
            counter(ui, "total", total, crate::theming::palette::STATUS_INFO);
            ui.add_space(12.0);
            let health_color = health_color(healthy, total);
            counter(ui, "healthy", healthy, health_color);
        });

        ui.add_space(6.0);

        // Health ratio bar.
        let frac = if total == 0 {
            0.0
        } else {
            (healthy as f32 / total as f32).clamp(0.0, 1.0)
        };
        let bar_w = ui.available_width().min(320.0);
        let (rect, _resp) = ui.allocate_exact_size(egui::vec2(bar_w, 6.0), egui::Sense::hover());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 2.0, crate::theming::palette::METER_BG);
        let mut fill = rect;
        fill.set_width(rect.width() * frac);
        painter.rect_filled(fill, 2.0, health_color(healthy, total));

        // Optional `nodes` array.
        if let Some(nodes) = obj.get("nodes").and_then(Value::as_array) {
            ui.add_space(8.0);
            ui.label(
                egui::RichText::new(format!("nodes ({})", nodes.len()))
                    .small()
                    .color(crate::theming::palette::TEXT_SYSTEM_MSG),
            );
            ui.add_space(2.0);

            egui::ScrollArea::vertical()
                .max_height(220.0)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    egui::Grid::new(("mesh_nodes_grid", path))
                        .num_columns(3)
                        .spacing([12.0, 2.0])
                        .striped(true)
                        .show(ui, |ui| {
                            ui.label(egui::RichText::new("id").weak().small());
                            ui.label(egui::RichText::new("status").weak().small());
                            ui.label(egui::RichText::new("age").weak().small());
                            ui.end_row();

                            for node in nodes {
                                let Some(n) = node.as_object() else {
                                    continue;
                                };
                                let id = n
                                    .get("id")
                                    .and_then(Value::as_str)
                                    .map(|s| s.to_string())
                                    .or_else(|| {
                                        n.get("id").and_then(Value::as_u64).map(|u| u.to_string())
                                    })
                                    .unwrap_or_else(|| "?".to_string());
                                let status =
                                    n.get("status").and_then(Value::as_str).unwrap_or("unknown");
                                let age = n
                                    .get("age_ms")
                                    .and_then(Value::as_u64)
                                    .map(|ms| format!("{ms} ms"))
                                    .or_else(|| {
                                        n.get("age")
                                            .and_then(Value::as_u64)
                                            .map(|s| format!("{s} s"))
                                    })
                                    .or_else(|| {
                                        n.get("age").and_then(Value::as_str).map(String::from)
                                    })
                                    .unwrap_or_default();

                                ui.monospace(id);
                                ui.label(
                                    egui::RichText::new(status)
                                        .color(status_color(status))
                                        .strong(),
                                );
                                ui.monospace(age);
                                ui.end_row();
                            }
                        });
                });
        }
    }
}

fn counter(ui: &mut egui::Ui, label: &str, n: u64, color: egui::Color32) {
    ui.vertical(|ui| {
        ui.label(
            egui::RichText::new(label)
                .small()
                .color(crate::theming::palette::TEXT_SYSTEM_MSG),
        );
        ui.label(
            egui::RichText::new(n.to_string())
                .heading()
                .color(color)
                .strong(),
        );
    });
}

fn health_color(healthy: u64, total: u64) -> egui::Color32 {
    if total == 0 {
        return crate::theming::palette::TEXT_LABEL_FAINT;
    }
    let ratio = healthy as f32 / total as f32;
    if ratio >= 0.85 {
        crate::theming::palette::STATUS_OK // green
    } else if ratio >= 0.5 {
        crate::theming::palette::STATUS_WARN // amber
    } else {
        crate::theming::palette::STATUS_ERR // red
    }
}

fn status_color(status: &str) -> egui::Color32 {
    match status {
        "healthy" | "online" | "up" | "connected" => crate::theming::palette::STATUS_OK,
        "degraded" | "slow" | "lagging" => crate::theming::palette::STATUS_WARN,
        "down" | "offline" | "error" | "disconnected" => crate::theming::palette::STATUS_ERR,
        _ => crate::theming::palette::STATUS_NEUTRAL,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn matches_minimal_counters() {
        let v = json!({ "total_nodes": 4, "healthy_nodes": 3 });
        assert_eq!(MeshNodesViewer::matches(&v), 12);
    }

    #[test]
    fn matches_with_nodes_list() {
        let v = json!({
            "total_nodes": 2,
            "healthy_nodes": 1,
            "nodes": [
                { "id": "node-a", "status": "healthy", "age_ms": 1200 },
                { "id": "node-b", "status": "down", "age_ms": 0 },
            ],
        });
        assert_eq!(MeshNodesViewer::matches(&v), 12);
    }

    #[test]
    fn rejects_missing_healthy() {
        let v = json!({ "total_nodes": 4 });
        assert_eq!(MeshNodesViewer::matches(&v), 0);
    }

    #[test]
    fn rejects_missing_total() {
        let v = json!({ "healthy_nodes": 3 });
        assert_eq!(MeshNodesViewer::matches(&v), 0);
    }

    #[test]
    fn rejects_counters_as_strings() {
        let v = json!({ "total_nodes": "4", "healthy_nodes": "3" });
        assert_eq!(MeshNodesViewer::matches(&v), 0);
    }

    #[test]
    fn rejects_non_object() {
        assert_eq!(MeshNodesViewer::matches(&Value::Null), 0);
        assert_eq!(MeshNodesViewer::matches(&json!([1, 2])), 0);
    }

    #[test]
    fn health_color_buckets() {
        // Full health = green.
        assert_eq!(health_color(10, 10), crate::theming::palette::STATUS_OK);
        // Two-thirds = amber.
        assert_eq!(health_color(6, 10), crate::theming::palette::STATUS_WARN);
        // Quarter = red.
        assert_eq!(health_color(2, 10), crate::theming::palette::STATUS_ERR);
        // Empty mesh = neutral.
        assert_eq!(
            health_color(0, 0),
            crate::theming::palette::TEXT_LABEL_FAINT
        );
    }

    #[test]
    fn paint_does_not_panic_on_realistic_fixture() {
        let ctx = egui::Context::default();
        let raw_input = egui::RawInput::default();
        let v = json!({
            "total_nodes": 3,
            "healthy_nodes": 2,
            "nodes": [
                { "id": "alpha", "status": "healthy", "age_ms": 1500 },
                { "id": "beta",  "status": "degraded", "age_ms": 4500 },
                { "id": "gamma", "status": "down", "age": "expired" },
            ],
        });
        let _ = ctx.run_ui(raw_input, |ui| {
            egui::CentralPanel::default().show_inside(ui, |ui| {
                MeshNodesViewer::paint(ui, "substrate/mesh", &v);
            });
        });
    }

    #[test]
    fn paint_handles_counters_only() {
        let ctx = egui::Context::default();
        let raw_input = egui::RawInput::default();
        let v = json!({ "total_nodes": 0, "healthy_nodes": 0 });
        let _ = ctx.run_ui(raw_input, |ui| {
            egui::CentralPanel::default().show_inside(ui, |ui| {
                MeshNodesViewer::paint(ui, "mesh/empty", &v);
            });
        });
    }
}
