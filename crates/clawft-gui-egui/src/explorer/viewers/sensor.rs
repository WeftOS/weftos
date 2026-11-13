//! `SensorViewer` — renders a Sensor envelope with a raw/summary
//! child-pane switcher chip row.
//!
//! WEFT-269 acceptance: a `{kind: <sensor>, raw, summary}` envelope
//! published by sensor services (mic, tof, camera, …) renders with a
//! header chip + pane toggle so the user can flip between the raw
//! payload and the down-sampled summary without leaving the panel.
//!
//! Pane state is persisted via `egui::Id` memory keyed on the substrate
//! path, so flipping between panels and coming back keeps the user's
//! last choice. Default pane: summary (matches the management-surface
//! "show the cheap thing first" guidance).

use super::SubstrateViewer;
use crate::ontology::ObjectType;
use serde_json::Value;

pub struct SensorViewer;

#[derive(Copy, Clone, PartialEq, Eq, Default)]
enum Pane {
    #[default]
    Summary,
    Raw,
    Both,
}

impl Pane {
    fn label(&self) -> &'static str {
        match self {
            Pane::Summary => "summary",
            Pane::Raw => "raw",
            Pane::Both => "both",
        }
    }
}

impl SubstrateViewer for SensorViewer {
    fn matches(value: &Value) -> u32 {
        // Reuse the ObjectType classifier so the envelope and the
        // viewer fire under the same conditions.
        crate::ontology::types::sensor::Sensor::matches(value).min(8)
    }

    fn paint(ui: &mut egui::Ui, path: &str, value: &Value) {
        let Some(obj) = value.as_object() else {
            return;
        };
        let kind = obj.get("kind").and_then(Value::as_str).unwrap_or("sensor");
        let has_raw = obj.contains_key("raw");
        let has_summary = obj.contains_key("summary");

        ui.label(
            egui::RichText::new(format!("sensor · {kind} · {path}"))
                .color(crate::theming::palette::TEXT_SUBTLE)
                .small(),
        );

        // WEFT-272 breadcrumb back to the publishing node. A sensor
        // path looks like `substrate/<node>/sensor/<kind>` — climb two
        // segments to land on the node root.
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
        }
        ui.add_space(2.0);

        // Pane state lives in egui memory keyed on the substrate path
        // so navigation back to the same sensor remembers the choice.
        let id = egui::Id::new(("weft-sensor-pane", path));
        let mut pane = ui
            .ctx()
            .data_mut(|d| d.get_temp::<Pane>(id).unwrap_or(Pane::default()));

        // Switcher chip row. Disable the chips for missing panes so a
        // sensor that only publishes a summary doesn't offer a dead
        // "raw" toggle.
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("view:")
                    .small()
                    .color(crate::theming::palette::TEXT_LABEL_FAINT),
            );
            for option in [Pane::Summary, Pane::Raw, Pane::Both] {
                let avail = match option {
                    Pane::Summary => has_summary,
                    Pane::Raw => has_raw,
                    Pane::Both => has_raw && has_summary,
                };
                let resp = ui.add_enabled(
                    avail,
                    egui::Button::selectable(pane == option, option.label()),
                );
                if resp.clicked() && avail {
                    pane = option;
                }
            }
        });
        ui.ctx().data_mut(|d| d.insert_temp(id, pane));
        ui.add_space(4.0);

        // If the user picked an unavailable pane, fall back to whatever
        // is present. Default to summary; if no summary, raw; if neither
        // (shouldn't happen for a valid envelope), bail.
        let effective = match pane {
            Pane::Summary if has_summary => Pane::Summary,
            Pane::Raw if has_raw => Pane::Raw,
            Pane::Both if has_raw && has_summary => Pane::Both,
            _ if has_summary => Pane::Summary,
            _ if has_raw => Pane::Raw,
            _ => {
                ui.label(
                    egui::RichText::new("(no raw or summary payload)")
                        .italics()
                        .color(crate::theming::palette::TEXT_LABEL_FAINT),
                );
                return;
            }
        };

        match effective {
            Pane::Summary => paint_subtree(ui, path, "summary", obj.get("summary")),
            Pane::Raw => paint_subtree(ui, path, "raw", obj.get("raw")),
            Pane::Both => {
                paint_subtree(ui, path, "summary", obj.get("summary"));
                ui.separator();
                paint_subtree(ui, path, "raw", obj.get("raw"));
            }
        }

        // WEFT-276 passive Action surface for Sensor.
        let caps = crate::ontology::types::sensor::Sensor::capabilities();
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

/// Climb to the publishing node from a sensor path.
///
/// `substrate/<node>/sensor/<kind>` → `substrate/<node>`. If the path
/// ends in `/sensor` (sensor sub-tree root) we climb one segment; if
/// it ends with anything else under `/sensor/...` we climb two. Falls
/// back to the immediate parent in any other shape so the breadcrumb
/// at least lands somewhere useful instead of a broken link.
fn parent_node_path(path: &str) -> Option<&str> {
    if path.ends_with("/sensor") {
        let (parent, _) = path.rsplit_once('/')?;
        if parent.is_empty() {
            None
        } else {
            Some(parent)
        }
    } else if let Some(idx) = path.rfind("/sensor/") {
        Some(&path[..idx])
    } else {
        let (parent, _) = path.rsplit_once('/')?;
        if parent.is_empty() {
            None
        } else {
            Some(parent)
        }
    }
}

/// Render a child sub-tree by dispatching it back through the viewer
/// registry. Lets a `{summary: { rms_db: -41.2, peak_db: -17.1 }}`
/// envelope re-pick the AudioMeterViewer for its summary pane while
/// the raw pane (a PCM chunk, say) renders with PcmChunkViewer.
fn paint_subtree(ui: &mut egui::Ui, parent_path: &str, label: &'static str, sub: Option<&Value>) {
    ui.label(
        egui::RichText::new(label)
            .strong()
            .small()
            .color(crate::theming::palette::ACCENT_STEEL_LIGHT),
    );
    match sub {
        Some(v) => {
            // Re-dispatch through the viewer registry. Passing a
            // synthetic child path keeps state-keyed viewers (like
            // TimeSeriesViewer's per-path history) from colliding with
            // the parent. We don't recurse into SensorViewer because
            // that would loop on a `{raw: {kind:"mic", ...}}` payload
            // — guarded by the priority-cascade ordering plus this
            // explicit suppression below the dispatch entry.
            let child_path = format!("{parent_path}/{label}");
            super::dispatch(ui, &child_path, v);
        }
        None => {
            ui.label(
                egui::RichText::new("(missing)")
                    .italics()
                    .small()
                    .color(crate::theming::palette::TEXT_LABEL_FAINT),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn matches_kind_mic() {
        let v = json!({ "kind": "mic", "rms_db": -41.2 });
        assert_eq!(SensorViewer::matches(&v), 8);
    }

    #[test]
    fn matches_paired_raw_summary() {
        let v = json!({
            "raw": { "frame": 1 },
            "summary": { "rms_db": -41.2 },
        });
        assert_eq!(SensorViewer::matches(&v), 8);
    }

    #[test]
    fn rejects_unknown_kind_no_split() {
        let v = json!({ "kind": "biscuit" });
        assert_eq!(SensorViewer::matches(&v), 0);
    }

    #[test]
    fn parent_node_path_strips_sensor_kind() {
        assert_eq!(
            parent_node_path("substrate/n-bfc4cd/sensor/mic"),
            Some("substrate/n-bfc4cd"),
        );
    }

    #[test]
    fn parent_node_path_strips_sensor_root() {
        assert_eq!(
            parent_node_path("substrate/n-bfc4cd/sensor"),
            Some("substrate/n-bfc4cd"),
        );
    }

    #[test]
    fn parent_node_path_falls_back_to_immediate_parent() {
        assert_eq!(
            parent_node_path("substrate/n-bfc4cd/health"),
            Some("substrate/n-bfc4cd"),
        );
    }

    #[test]
    fn parent_node_path_returns_none_for_root() {
        assert_eq!(parent_node_path("sensor"), None);
    }

    #[test]
    fn paint_does_not_panic_on_summary_only() {
        let ctx = egui::Context::default();
        let raw_input = egui::RawInput::default();
        let v = json!({
            "kind": "mic",
            "summary": { "rms_db": -41.2, "peak_db": -17.1 },
        });
        let _ = ctx.run_ui(raw_input, |ui| {
            egui::CentralPanel::default().show_inside(ui, |ui| {
                SensorViewer::paint(ui, "substrate/n-bfc4cd/sensor/mic", &v);
            });
        });
    }

    #[test]
    fn paint_does_not_panic_on_full_envelope() {
        let ctx = egui::Context::default();
        let raw_input = egui::RawInput::default();
        let v = json!({
            "kind": "tof",
            "raw": { "ranges_mm": [120, 130, 140] },
            "summary": { "min_mm": 120, "max_mm": 140 },
        });
        let _ = ctx.run_ui(raw_input, |ui| {
            egui::CentralPanel::default().show_inside(ui, |ui| {
                SensorViewer::paint(ui, "substrate/n-bfc4cd/sensor/tof", &v);
            });
        });
    }
}
