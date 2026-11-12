//! `AudioMeterViewer` — renders an object with `rms_db` + `peak_db`
//! numeric fields as a pair of dB bars plus numeric readouts.
//!
//! **dB range: -65 … -5 dBFS.** This is sized against the INMP441
//! MEMS mic (noise floor ~-57 dBFS, speech peaks ~-10 dBFS) that
//! replaced the piezo in `substrate/sensor/mic`. The old piezo range
//! (-43…-16) is obsolete — do not resurrect it.
//!
//! Bar colour thresholds (in dBFS):
//! - **green** above -30 (loud: speech peaks, clap)
//! - **amber** -45 … -30 (room tone, speech midrange)
//! - **red**   below -45 (quiet, near noise floor)
//!
//! The viewer is stateless; no scrolling history is kept. A future
//! `WaveformViewer` will cover that niche.

use super::SubstrateViewer;
use serde_json::Value;

/// dBFS bottom end of the meter. Anything quieter is clamped to 0 %.
const DB_MIN: f64 = -65.0;
/// dBFS top end of the meter. Anything louder is clamped to 100 %.
const DB_MAX: f64 = -5.0;

pub struct AudioMeterViewer;

impl SubstrateViewer for AudioMeterViewer {
    fn matches(value: &Value) -> u32 {
        let Some(obj) = value.as_object() else {
            return 0;
        };
        let has_rms = obj.get("rms_db").and_then(Value::as_f64).is_some();
        let has_peak = obj.get("peak_db").and_then(Value::as_f64).is_some();
        if has_rms && has_peak { 10 } else { 0 }
    }

    fn paint(ui: &mut egui::Ui, path: &str, value: &Value) {
        let obj = match value.as_object() {
            Some(o) => o,
            None => return,
        };
        let rms = obj.get("rms_db").and_then(Value::as_f64).unwrap_or(DB_MIN);
        let peak = obj.get("peak_db").and_then(Value::as_f64).unwrap_or(DB_MIN);

        ui.label(
            egui::RichText::new(format!("audio meter · {path}"))
                .color(crate::theming::palette::TEXT_SUBTLE)
                .small(),
        );
        ui.add_space(4.0);

        paint_bar(ui, "rms_db ", rms);
        paint_bar(ui, "peak_db", peak);

        ui.add_space(6.0);

        // Optional scalars — shown as plain text if present.
        if let Some(available) = obj.get("available").and_then(Value::as_bool) {
            let (txt, col) = if available {
                ("available", crate::theming::palette::STATUS_OK)
            } else {
                ("unavailable", crate::theming::palette::TEXT_ERROR_SOFT)
            };
            ui.label(egui::RichText::new(txt).color(col).strong());
        }
        if let Some(sr) = obj.get("sample_rate").and_then(Value::as_f64) {
            ui.label(format!("sample_rate: {} Hz", sr as i64));
        }
        if let Some(tick) = obj.get("tick").and_then(Value::as_f64) {
            ui.label(format!("tick: {}", tick as i64));
        }
    }
}

fn paint_bar(ui: &mut egui::Ui, label: &str, db: f64) {
    let frac = ((db - DB_MIN) / (DB_MAX - DB_MIN)).clamp(0.0, 1.0) as f32;
    let color = level_color(db);

    let desired = egui::vec2(ui.available_width().min(320.0), 16.0);
    let (rect, _resp) = ui.allocate_exact_size(desired, egui::Sense::hover());
    let painter = ui.painter_at(rect);

    // Background track.
    painter.rect_filled(rect, 2.0, crate::theming::palette::METER_BG);

    // Filled portion.
    let mut fill = rect;
    fill.set_width(rect.width() * frac);
    painter.rect_filled(fill, 2.0, color);

    // Readout overlay: "<label>: <db:6.1> dBFS".
    let readout = format!("{label}: {db:6.1} dBFS");
    painter.text(
        egui::pos2(rect.left() + 6.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        readout,
        egui::FontId::monospace(11.0),
        crate::theming::palette::METER_TEXT,
    );
}

fn level_color(db: f64) -> egui::Color32 {
    if db >= -30.0 {
        crate::theming::palette::STATUS_OK // green
    } else if db >= -45.0 {
        crate::theming::palette::STATUS_WARN // amber
    } else {
        crate::theming::palette::STATUS_ERR // red
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn matches_full_shape() {
        let v = json!({
            "rms_db": -41.2,
            "peak_db": -17.1,
            "available": true,
            "sample_rate": 16000,
            "tick": 214
        });
        assert_eq!(AudioMeterViewer::matches(&v), 10);
    }

    #[test]
    fn matches_minimal_numeric_pair() {
        let v = json!({ "rms_db": -50.0, "peak_db": -30.0 });
        assert_eq!(AudioMeterViewer::matches(&v), 10);
    }

    #[test]
    fn rejects_only_rms() {
        let v = json!({ "rms_db": -41.2 });
        assert_eq!(AudioMeterViewer::matches(&v), 0);
    }

    #[test]
    fn rejects_only_peak() {
        let v = json!({ "peak_db": -17.1 });
        assert_eq!(AudioMeterViewer::matches(&v), 0);
    }

    #[test]
    fn rejects_string_values() {
        let v = json!({ "rms_db": "-41.2", "peak_db": "-17.1" });
        assert_eq!(AudioMeterViewer::matches(&v), 0);
    }

    #[test]
    fn rejects_null_peak() {
        let v = json!({ "rms_db": -41.2, "peak_db": null });
        assert_eq!(AudioMeterViewer::matches(&v), 0);
    }

    #[test]
    fn rejects_empty_object() {
        let v = json!({});
        assert_eq!(AudioMeterViewer::matches(&v), 0);
    }

    #[test]
    fn rejects_null_value() {
        assert_eq!(AudioMeterViewer::matches(&Value::Null), 0);
    }

    #[test]
    fn rejects_array() {
        let v = json!([-41.2, -17.1]);
        assert_eq!(AudioMeterViewer::matches(&v), 0);
    }
}
