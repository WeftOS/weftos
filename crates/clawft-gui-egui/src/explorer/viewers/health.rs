//! `HealthViewer` — renders a `HealthReport` shape as a compact
//! key/value board with a colored signal-strength chip and an inline
//! breadcrumb back to the parent node.
//!
//! READS-ONLY tier per `.planning/sensors/EXPLORER-MANAGEMENT-SURFACE.md`
//! affordances #1, #2 (WEFT-268). Pairs with the `HealthReport`
//! ObjectType (priority 12) so when the substrate publishes
//! `substrate/<node>/health` we get a typed render instead of the
//! generic JSON badge.

use super::SubstrateViewer;
use crate::ontology::ObjectType;
use serde_json::Value;

pub struct HealthViewer;

impl SubstrateViewer for HealthViewer {
    fn matches(value: &Value) -> u32 {
        // Reuse the ObjectType's classifier so the viewer fires under
        // exactly the same conditions the type does. Priority 12 mirrors
        // the type's priority — any specialised payload viewer above 12
        // (currently none in this band) wins.
        crate::ontology::types::health_report::HealthReport::matches(value).min(12)
    }

    fn paint(ui: &mut egui::Ui, path: &str, value: &Value) {
        let Some(obj) = value.as_object() else {
            return;
        };

        ui.label(
            egui::RichText::new(format!("health · {path}"))
                .color(crate::theming::palette::TEXT_SUBTLE)
                .small(),
        );
        ui.add_space(4.0);

        // Signal strength chip from RSSI when present.
        if let Some(rssi) = obj.get("rssi").and_then(Value::as_i64) {
            paint_rssi_chip(ui, rssi);
            ui.add_space(4.0);
        }

        // Breadcrumb back to the publishing node — strips the
        // `/health` suffix. WEFT-272: the link posts a navigation
        // intent via `request_navigation`, which the Explorer drains
        // on its next paint and runs through `on_select`. No direct
        // mutation of the Explorer from here — viewers stay stateless.
        if let Some(node_path) = parent_node_path(path) {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("⤴ node:")
                        .small()
                        .color(crate::theming::palette::TEXT_LABEL_FAINT),
                );
                let link = ui
                    .link(
                        egui::RichText::new(node_path)
                            .monospace()
                            .small()
                            .color(crate::theming::palette::ACCENT_STEEL),
                    )
                    .on_hover_text("Navigate to the publishing node.");
                if link.clicked() {
                    crate::explorer::request_navigation(ui.ctx(), node_path.to_string());
                }
            });
            ui.add_space(4.0);
        }

        // Key/value board for known scalar fields.
        let known = [
            ("rssi", "dBm"),
            ("free_heap", "B"),
            ("uptime_s", "s"),
            ("cpu_pct", "%"),
            ("temp_c", "°C"),
            ("tick", ""),
        ];
        egui::Grid::new(("health_kvs", path))
            .num_columns(3)
            .spacing([12.0, 2.0])
            .show(ui, |ui| {
                for (k, unit) in known {
                    if let Some(v) = obj.get(k) {
                        ui.label(egui::RichText::new(k).weak());
                        ui.label(egui::RichText::new(format_scalar(v)).monospace());
                        ui.label(
                            egui::RichText::new(unit)
                                .small()
                                .color(crate::theming::palette::TEXT_LABEL_FAINT),
                        );
                        ui.end_row();
                    }
                }
            });

        // WEFT-271: inline sparklines for the known numeric scalars.
        // Reuses TimeSeriesViewer's per-path history ring keyed on a
        // synthetic `path/<field>` so each scalar tracks independently
        // and the next time the user lands on this HealthReport they
        // see a continuous trace, not a reset.
        ui.add_space(4.0);
        for (k, _unit) in known {
            if let Some(v) = obj.get(k)
                && v.as_f64().is_some()
            {
                let child_path = format!("{path}/{k}");
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(k).small().weak());
                    super::time_series::embed_sparkline(ui, &child_path, v, 28.0);
                });
            }
        }

        // WEFT-276: render the declared Action surface as a passive
        // bullet list. No buttons until the Action pipeline lands; this
        // is the read-only affordance the audit calls for.
        let caps = crate::ontology::types::health_report::HealthReport::capabilities();
        if !caps.applicable_actions.is_empty() {
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new("actions")
                    .small()
                    .color(crate::theming::palette::TEXT_LABEL_FAINT),
            );
            for a in caps.applicable_actions {
                ui.label(egui::RichText::new(format!("  • {a}")).monospace().small());
            }
        }
    }
}

/// Parent node-path: drop the trailing `/health` (or `/<anything>` last
/// segment if the input doesn't end in `/health`). Returns `None` when
/// the path has no `/` at all.
fn parent_node_path(path: &str) -> Option<&str> {
    let (parent, _) = path.rsplit_once('/')?;
    if parent.is_empty() {
        return None;
    }
    Some(parent)
}

fn format_scalar(v: &Value) -> String {
    match v {
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Null => "—".into(),
        _ => v.to_string(),
    }
}

/// Render an RSSI chip with a colour mapped to the typical Wi-Fi
/// signal-quality bands. The bands match what most consumer access-
/// point UIs use:
///   ≥ -50   excellent (green)
///   ≥ -65   good      (lime)
///   ≥ -75   fair      (amber)
///   else    poor      (red)
fn paint_rssi_chip(ui: &mut egui::Ui, rssi: i64) {
    let (label, color) = if rssi >= -50 {
        ("excellent", crate::theming::palette::STATUS_OK)
    } else if rssi >= -65 {
        ("good", crate::theming::palette::HEALTH_LIME)
    } else if rssi >= -75 {
        ("fair", crate::theming::palette::STATUS_WARN)
    } else {
        ("poor", crate::theming::palette::STATUS_ERR)
    };
    ui.horizontal(|ui| {
        let dot_size = egui::vec2(12.0, 12.0);
        let (rect, _resp) = ui.allocate_exact_size(dot_size, egui::Sense::hover());
        ui.painter().circle_filled(rect.center(), 5.0, color);
        ui.label(
            egui::RichText::new(format!("{rssi} dBm · {label}"))
                .strong()
                .color(color),
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn matches_kind_health() {
        let v = json!({ "kind": "health" });
        assert_eq!(HealthViewer::matches(&v), 12);
    }

    #[test]
    fn matches_two_scalars() {
        let v = json!({ "rssi": -47, "free_heap": 184_000_u64 });
        assert_eq!(HealthViewer::matches(&v), 12);
    }

    #[test]
    fn rejects_single_scalar() {
        let v = json!({ "tick": 1_u64 });
        assert_eq!(HealthViewer::matches(&v), 0);
    }

    #[test]
    fn rejects_array() {
        assert_eq!(HealthViewer::matches(&json!([1, 2, 3])), 0);
    }

    #[test]
    fn parent_node_path_strips_health_suffix() {
        assert_eq!(
            parent_node_path("substrate/n-bfc4cd/health"),
            Some("substrate/n-bfc4cd"),
        );
    }

    #[test]
    fn parent_node_path_returns_none_for_root() {
        assert_eq!(parent_node_path("health"), None);
    }

    #[test]
    fn format_scalar_handles_null() {
        assert_eq!(format_scalar(&Value::Null), "—");
    }

    #[test]
    fn paint_does_not_panic() {
        let ctx = egui::Context::default();
        let raw_input = egui::RawInput::default();
        let v = json!({
            "rssi": -47,
            "free_heap": 184_000_u64,
            "uptime_s": 12_345_u64,
            "tick": 42_u64,
        });
        let _ = ctx.run_ui(raw_input, |ui| {
            egui::CentralPanel::default().show_inside(ui, |ui| {
                HealthViewer::paint(ui, "substrate/n-bfc4cd/health", &v);
            });
        });
    }
}
