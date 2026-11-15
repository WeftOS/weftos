//! Control-intent toggle viewer.
//!
//! Renders a one-button enable/disable affordance for substrate
//! values that match the daemon's control-plane shape:
//!
//! ```json
//! {
//!   "enabled":   true,
//!   "kind":      "service" | "sensor",
//!   "target":    "<slug>",
//!   "label":     "<human readable>",
//!   "updated_at_ms": 1700000000000
//! }
//! ```
//!
//! Click → fires `control.set_enabled` over the existing Live RPC
//! transport. The substrate value updates, the subscription
//! re-renders this view, the button label flips. No local state
//! kept here — the substrate value IS the state.

use std::sync::Arc;

use eframe::egui;
use serde_json::Value;

use crate::live::{self, Command, Live};

/// Priority for shape-match. Higher than `JsonFallbackViewer`'s 1.
/// We don't go through the standard SubstrateViewer trait because
/// we need [`Live`] to fire RPCs — see [`paint`] below.
const PRIORITY: u32 = 25;

/// Shape predicate. Returns [`PRIORITY`] when `value` looks like a
/// control intent, `0` otherwise. Strict on key presence + types so
/// arbitrary booleans-named-`enabled` don't false-match.
pub fn matches(value: &Value) -> u32 {
    let Some(obj) = value.as_object() else {
        return 0;
    };
    let has_enabled = obj.get("enabled").and_then(Value::as_bool).is_some();
    let kind_ok = obj
        .get("kind")
        .and_then(Value::as_str)
        .map(|s| s == "service" || s == "sensor")
        .unwrap_or(false);
    let has_target = obj.get("target").and_then(Value::as_str).is_some();
    if has_enabled && kind_ok && has_target {
        PRIORITY
    } else {
        0
    }
}

/// Render the toggle. `path` is the substrate path of the intent
/// (used only for display + debugging — the RPC keys off `kind` +
/// `target` from the value, so renaming the path doesn't break the
/// affordance).
pub fn paint(ui: &mut egui::Ui, path: &str, value: &Value, live: &Arc<Live>) {
    // We checked these in `matches`; unwrapping after a failed
    // match would be a bug, but defensively re-extract so a
    // direct paint call (e.g. in a future reuse) doesn't panic.
    let Some(obj) = value.as_object() else {
        ui.label("(not a control intent)");
        return;
    };
    let enabled = obj.get("enabled").and_then(Value::as_bool).unwrap_or(false);
    let kind = obj.get("kind").and_then(Value::as_str).unwrap_or("");
    let target = obj.get("target").and_then(Value::as_str).unwrap_or("");
    let label = obj
        .get("label")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(target);

    ui.label(
        egui::RichText::new(format!("control · {kind}"))
            .color(crate::theming::palette::TEXT_SUBTLE)
            .small(),
    );
    ui.add_space(2.0);
    ui.heading(label);
    ui.add_space(8.0);

    ui.horizontal(|ui| {
        let (status_text, status_color) = if enabled {
            ("● enabled", crate::theming::palette::STATUS_OK)
        } else {
            ("○ disabled", crate::theming::palette::TEXT_ERROR_SOFT)
        };
        ui.label(
            egui::RichText::new(status_text)
                .strong()
                .color(status_color),
        );

        ui.add_space(12.0);

        let btn_label = if enabled { "Disable" } else { "Enable" };
        if ui.button(btn_label).clicked() {
            // Fire-and-forget: we don't need the reply because the
            // substrate subscription will surface the new value
            // within one poll. Errors land on the daemon log.
            live.submit(Command::Raw {
                method: "control.set_enabled".into(),
                params: serde_json::json!({
                    "kind":   kind,
                    "target": target,
                    "enabled": !enabled,
                    "label":  label,
                }),
                reply: Some(live::reply_channel().0),
            });
        }
    });

    ui.add_space(8.0);
    ui.separator();
    ui.label(
        egui::RichText::new(format!("path: {path}"))
            .small()
            .monospace()
            .color(crate::theming::palette::TEXT_LABEL_FAINT),
    );
    ui.label(
        egui::RichText::new(format!("target: {target}"))
            .small()
            .monospace()
            .color(crate::theming::palette::TEXT_LABEL_FAINT),
    );
    if let Some(ts) = obj.get("updated_at_ms").and_then(Value::as_u64) {
        ui.label(
            egui::RichText::new(format!("updated_at_ms: {ts}"))
                .small()
                .monospace()
                .color(crate::theming::palette::TEXT_LABEL_FAINT),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn matches_full_service_intent() {
        let v = json!({
            "enabled": true,
            "kind": "service",
            "target": "whisper",
            "label": "Whisper STT",
            "updated_at_ms": 1700000000000_u64,
        });
        assert_eq!(matches(&v), PRIORITY);
    }

    #[test]
    fn matches_sensor_intent() {
        let v = json!({
            "enabled": false,
            "kind": "sensor",
            "target": "n-bfc4cd/mic/pcm_chunk",
            "label": "",
            "updated_at_ms": 0,
        });
        assert_eq!(matches(&v), PRIORITY);
    }

    #[test]
    fn rejects_unknown_kind() {
        let v = json!({"enabled": true, "kind": "ghost", "target": "x"});
        assert_eq!(matches(&v), 0);
    }

    #[test]
    fn rejects_missing_enabled() {
        let v = json!({"kind": "service", "target": "x"});
        assert_eq!(matches(&v), 0);
    }

    #[test]
    fn rejects_missing_target() {
        let v = json!({"enabled": true, "kind": "service"});
        assert_eq!(matches(&v), 0);
    }

    #[test]
    fn rejects_string_enabled() {
        let v = json!({"enabled": "yes", "kind": "service", "target": "x"});
        assert_eq!(matches(&v), 0);
    }

    #[test]
    fn rejects_non_object() {
        assert_eq!(matches(&Value::Null), 0);
        assert_eq!(matches(&json!([1, 2, 3])), 0);
        assert_eq!(matches(&json!(42)), 0);
    }

    #[test]
    fn priority_beats_json_fallback() {
        // Sanity that the priority sits above the json_fallback's
        // catch-all (1).
        const { assert!(PRIORITY > 1) };
    }
}
