//! `WaveformViewer` — renders an object shaped like
//! `{ samples: [f32; N], sample_rate: f64 }` as a line plot.
//!
//! Priority **15**, deliberately above [`AudioMeterViewer`]'s 10 so a
//! payload that carries both a full sample buffer and the rms/peak
//! scalars promotes to the full waveform rather than the dB bars.
//!
//! The `ui://waveform` primitive in `surface_host::compose` renders a
//! nearly identical line plot, but its [`render_waveform`] helper is
//! tied to `SurfaceNode` + `OntologySnapshot` bindings and isn't
//! exposed as a reusable function. Rather than introduce a circular
//! dep from the explorer back into the composer, this viewer inlines
//! the minimal line-plot renderer. If the composer's path ever gets
//! lifted into a shared primitives module the shapes are
//! interchangeable.

use super::SubstrateViewer;
use serde_json::Value;

pub struct WaveformViewer;

impl SubstrateViewer for WaveformViewer {
    fn matches(value: &Value) -> u32 {
        let Some(obj) = value.as_object() else {
            return 0;
        };
        let Some(arr) = obj.get("samples").and_then(Value::as_array) else {
            return 0;
        };
        if arr.is_empty() {
            return 0;
        }
        // Every sample must be numeric — otherwise the fallback
        // should handle it.
        if !arr.iter().all(|v| v.as_f64().is_some()) {
            return 0;
        }
        if obj.get("sample_rate").and_then(Value::as_f64).is_none() {
            return 0;
        }
        15
    }

    fn paint(ui: &mut egui::Ui, path: &str, value: &Value) {
        let obj = match value.as_object() {
            Some(o) => o,
            None => return,
        };
        let samples: Vec<f64> = obj
            .get("samples")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_f64).collect())
            .unwrap_or_default();
        let sample_rate = obj
            .get("sample_rate")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);

        let duration_ms = if sample_rate > 0.0 {
            (samples.len() as f64 / sample_rate) * 1000.0
        } else {
            0.0
        };

        ui.label(
            egui::RichText::new(format!(
                "waveform · {path}  ({n} samples @ {sr:.0} Hz, {dur:.1} ms)",
                n = samples.len(),
                sr = sample_rate,
                dur = duration_ms,
            ))
            .color(crate::theming::palette::TEXT_SUBTLE)
            .small(),
        );
        ui.add_space(4.0);

        let height = 120.0_f32;
        let width = ui.available_width().clamp(200.0, 480.0);
        let (rect, _resp) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());
        let painter = ui.painter_at(rect);

        painter.rect_filled(rect, 2.0, crate::theming::palette::PLOT_BG);

        if samples.len() < 2 {
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "no samples",
                egui::FontId::proportional(11.0),
                crate::theming::palette::TEXT_DIM,
            );
            return;
        }

        // Auto-scale y to observed min/max, with a floor so a flat-line
        // signal doesn't divide by zero.
        let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
        let (mut peak_idx, mut peak_val) = (0usize, f64::NEG_INFINITY);
        let (mut trough_idx, mut trough_val) = (0usize, f64::INFINITY);
        for (i, s) in samples.iter().enumerate() {
            if !s.is_finite() {
                continue;
            }
            if *s < lo {
                lo = *s;
            }
            if *s > hi {
                hi = *s;
            }
            if *s > peak_val {
                peak_val = *s;
                peak_idx = i;
            }
            if *s < trough_val {
                trough_val = *s;
                trough_idx = i;
            }
        }
        if !lo.is_finite() || !hi.is_finite() {
            lo = -1.0;
            hi = 1.0;
        }
        if (hi - lo).abs() < f64::EPSILON {
            lo -= 0.5;
            hi += 0.5;
        }
        let span = hi - lo;

        // Centre axis line (y = 0) if 0 falls inside the range.
        if lo < 0.0 && hi > 0.0 {
            let y_zero = rect.bottom() - (((-lo) / span) as f32) * rect.height();
            painter.line_segment(
                [
                    egui::pos2(rect.left(), y_zero),
                    egui::pos2(rect.right(), y_zero),
                ],
                egui::Stroke::new(1.0, crate::theming::palette::PLOT_GRID),
            );
        }

        let denom = (samples.len() - 1).max(1) as f32;
        let mut points = Vec::with_capacity(samples.len());
        for (i, s) in samples.iter().enumerate() {
            let t = i as f32 / denom;
            let x = rect.left() + t * rect.width();
            let n = ((s - lo) / span).clamp(0.0, 1.0) as f32;
            let y = rect.bottom() - n * rect.height();
            points.push(egui::pos2(x, y));
        }
        painter.add(egui::epaint::PathShape::line(
            points,
            egui::Stroke::new(1.5, crate::theming::palette::STATUS_OK),
        ));

        // Peak/trough markers — little dots at the extrema.
        if peak_val.is_finite() {
            let t = peak_idx as f32 / denom;
            let x = rect.left() + t * rect.width();
            let n = ((peak_val - lo) / span).clamp(0.0, 1.0) as f32;
            let y = rect.bottom() - n * rect.height();
            painter.circle_filled(egui::pos2(x, y), 3.0, crate::theming::palette::STATUS_WARN);
        }
        if trough_val.is_finite() {
            let t = trough_idx as f32 / denom;
            let x = rect.left() + t * rect.width();
            let n = ((trough_val - lo) / span).clamp(0.0, 1.0) as f32;
            let y = rect.bottom() - n * rect.height();
            painter.circle_filled(egui::pos2(x, y), 3.0, crate::theming::palette::STATUS_ERR);
        }

        ui.add_space(4.0);
        ui.label(
            egui::RichText::new(format!(
                "y: {lo:.3} … {hi:.3}   peak: {peak_val:.3}   trough: {trough_val:.3}"
            ))
            .small()
            .color(crate::theming::palette::TEXT_SYSTEM_MSG),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sine_fixture(n: usize, sr: f64) -> Value {
        let samples: Vec<f64> = (0..n)
            .map(|i| {
                let t = i as f64 / sr;
                (t * 2.0 * std::f64::consts::PI * 440.0).sin()
            })
            .collect();
        json!({
            "samples": samples,
            "sample_rate": sr,
        })
    }

    #[test]
    fn matches_well_formed_waveform() {
        assert_eq!(WaveformViewer::matches(&sine_fixture(64, 16000.0)), 15);
    }

    #[test]
    fn matches_integer_samples() {
        let v = json!({
            "samples": [0, 1, -1, 2, -2],
            "sample_rate": 8000,
        });
        assert_eq!(WaveformViewer::matches(&v), 15);
    }

    #[test]
    fn matches_priority_beats_audio_meter() {
        // 15 > 10 so a combined-shape payload promotes to WaveformViewer.
        assert!(WaveformViewer::matches(&sine_fixture(16, 16000.0)) > 10);
    }

    #[test]
    fn rejects_missing_samples() {
        let v = json!({ "sample_rate": 16000 });
        assert_eq!(WaveformViewer::matches(&v), 0);
    }

    #[test]
    fn rejects_missing_sample_rate() {
        let v = json!({ "samples": [0.0, 0.1] });
        assert_eq!(WaveformViewer::matches(&v), 0);
    }

    #[test]
    fn rejects_empty_samples() {
        let v = json!({ "samples": [], "sample_rate": 16000 });
        assert_eq!(WaveformViewer::matches(&v), 0);
    }

    #[test]
    fn rejects_string_samples() {
        let v = json!({
            "samples": ["0.1", "0.2"],
            "sample_rate": 16000,
        });
        assert_eq!(WaveformViewer::matches(&v), 0);
    }

    #[test]
    fn rejects_mixed_samples() {
        let v = json!({
            "samples": [0.0, "nope"],
            "sample_rate": 16000,
        });
        assert_eq!(WaveformViewer::matches(&v), 0);
    }

    #[test]
    fn rejects_non_object() {
        assert_eq!(WaveformViewer::matches(&Value::Null), 0);
        assert_eq!(WaveformViewer::matches(&json!([1, 2, 3])), 0);
        assert_eq!(WaveformViewer::matches(&json!(42)), 0);
    }

    #[test]
    fn paint_does_not_panic_on_realistic_fixture() {
        let ctx = egui::Context::default();
        let raw_input = egui::RawInput::default();
        let v = sine_fixture(128, 16000.0);
        let _ = ctx.run_ui(raw_input, |ui| {
            egui::CentralPanel::default().show_inside(ui, |ui| {
                WaveformViewer::paint(ui, "substrate/sensor/mic/waveform", &v);
            });
        });
    }

    #[test]
    fn paint_handles_flat_line() {
        let ctx = egui::Context::default();
        let raw_input = egui::RawInput::default();
        let v = json!({
            "samples": [0.0, 0.0, 0.0, 0.0],
            "sample_rate": 8000,
        });
        let _ = ctx.run_ui(raw_input, |ui| {
            egui::CentralPanel::default().show_inside(ui, |ui| {
                WaveformViewer::paint(ui, "test/flat", &v);
            });
        });
    }
}
