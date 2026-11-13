//! `ConnectionBadgeViewer` — renders an object that carries a `state`
//! field whose string value names a connection lifecycle state.
//!
//! Triggers on any of: `connected`, `disconnected`, `connecting`,
//! `idle`, `error`, `unknown`. Anything else (e.g. `"paused"`) is
//! declined so the JSON fallback handles it.
//!
//! Layout: a coloured dot, the state label, then any remaining scalar
//! fields as a small two-column key/value table.

use super::SubstrateViewer;
use serde_json::Value;

pub struct ConnectionBadgeViewer;

/// Known connection states — matching against this list is how we
/// avoid false positives on arbitrary "state" fields.
const KNOWN_STATES: &[&str] = &[
    "connected",
    "disconnected",
    "connecting",
    "idle",
    "error",
    "unknown",
];

impl SubstrateViewer for ConnectionBadgeViewer {
    fn matches(value: &Value) -> u32 {
        let Some(obj) = value.as_object() else {
            return 0;
        };
        let Some(state) = obj.get("state").and_then(Value::as_str) else {
            return 0;
        };
        if KNOWN_STATES.contains(&state) { 10 } else { 0 }
    }

    fn paint(ui: &mut egui::Ui, path: &str, value: &Value) {
        let obj = match value.as_object() {
            Some(o) => o,
            None => return,
        };
        let state = obj
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let color = state_color(state);

        ui.label(
            egui::RichText::new(format!("connection · {path}"))
                .color(crate::theming::palette::TEXT_SUBTLE)
                .small(),
        );
        ui.add_space(4.0);

        ui.horizontal(|ui| {
            // Coloured dot.
            let dot_size = egui::vec2(14.0, 14.0);
            let (rect, _resp) = ui.allocate_exact_size(dot_size, egui::Sense::hover());
            ui.painter().circle_filled(rect.center(), 6.0, color);
            ui.label(egui::RichText::new(state).strong().color(color));
        });

        ui.add_space(6.0);

        // Remaining scalar fields in a tidy key/value table.
        let mut kvs: Vec<(&str, String)> = Vec::new();
        for (k, v) in obj {
            if k == "state" {
                continue;
            }
            if let Some(s) = scalar_to_string(v) {
                kvs.push((k.as_str(), s));
            }
        }
        if !kvs.is_empty() {
            egui::Grid::new(("connection_badge_kvs", path))
                .num_columns(2)
                .spacing([12.0, 2.0])
                .show(ui, |ui| {
                    for (k, v) in kvs {
                        ui.label(egui::RichText::new(k).weak());
                        ui.label(egui::RichText::new(v).monospace());
                        ui.end_row();
                    }
                });
        }
    }
}

fn state_color(state: &str) -> egui::Color32 {
    match state {
        "connected" => crate::theming::palette::STATUS_OK,
        "connecting" | "idle" => crate::theming::palette::STATUS_WARN,
        "disconnected" | "error" => crate::theming::palette::STATUS_ERR,
        _ => crate::theming::palette::TEXT_LABEL_FAINT,
    }
}

fn scalar_to_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn matches_connected() {
        let v = json!({ "state": "connected", "ssid": "home-wifi", "rssi": -47 });
        assert_eq!(ConnectionBadgeViewer::matches(&v), 10);
    }

    #[test]
    fn matches_all_known_states() {
        for s in KNOWN_STATES {
            let v = json!({ "state": *s });
            assert_eq!(
                ConnectionBadgeViewer::matches(&v),
                10,
                "state {s} should match",
            );
        }
    }

    #[test]
    fn rejects_unknown_state_string() {
        let v = json!({ "state": "paused" });
        assert_eq!(ConnectionBadgeViewer::matches(&v), 0);
    }

    #[test]
    fn rejects_state_as_number() {
        let v = json!({ "state": 1 });
        assert_eq!(ConnectionBadgeViewer::matches(&v), 0);
    }

    #[test]
    fn rejects_missing_state() {
        let v = json!({ "ssid": "home-wifi" });
        assert_eq!(ConnectionBadgeViewer::matches(&v), 0);
    }

    #[test]
    fn rejects_empty_object() {
        let v = json!({});
        assert_eq!(ConnectionBadgeViewer::matches(&v), 0);
    }

    #[test]
    fn rejects_null() {
        assert_eq!(ConnectionBadgeViewer::matches(&Value::Null), 0);
    }

    #[test]
    fn rejects_string_root() {
        let v = json!("connected");
        assert_eq!(ConnectionBadgeViewer::matches(&v), 0);
    }
}
