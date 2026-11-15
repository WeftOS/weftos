//! `TimeSeriesViewer` — renders a raw numeric scalar (`f64`, `i64`,
//! `u64`) as a current-value readout plus a sparkline of the recent
//! history for that substrate path.
//!
//! Priority **5**: intentionally low. Any specialised shape (audio
//! meter, mesh counters, chain tail, etc.) wins over this catch-all,
//! but for a bare numeric tick — `substrate/.../tick`,
//! `substrate/.../counter`, etc. — this beats the JSON fallback's `1`
//! and shows "something's moving" rather than a static `42`.
//!
//! # Design tension — state in a stateless viewer
//!
//! [`SubstrateViewer`] is a pure-function trait: `matches` and
//! `paint` both take `&Value` and no `&mut self`. That's fine for
//! every other viewer — they render what you give them and keep no
//! history. A time-series sparkline needs *history*, which has to
//! come from somewhere.
//!
//! Two obvious places:
//!
//! 1. **Promote the trait to carry per-viewer state** — invasive,
//!    affects every other viewer, and contradicts the "pure
//!    dispatch" design in `explorer::viewers::dispatch`.
//! 2. **Store history in a process-global map keyed by substrate
//!    path.** What we do here. It's a heuristic — the viewer side-
//!    channels state out-of-band — but it preserves the trait and
//!    the dispatch contract, and the only consumer is the Explorer's
//!    right-hand pane which paints one path at a time.
//!
//! The buffer is bounded (`MAX_HISTORY = 240`, ~one minute at 4 Hz)
//! and keyed on `path`, so it self-GCs on path reuse. If a later
//! iteration wants proper viewer state this module can flip to
//! option 1 without changing any call site — `paint` will still be
//! `paint(ui, path, value)` from the caller's side.

use super::SubstrateViewer;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Mutex;

/// Max samples kept per path. One minute at 4 Hz is plenty to spot
/// a trend, and keeps the memory bill for idle paths trivial.
const MAX_HISTORY: usize = 240;

/// Process-global per-path history. `Mutex` rather than `RwLock`
/// because paints touch one key at a time and contention is zero
/// (paint is single-threaded inside the egui pass).
static HISTORY: Mutex<Option<HashMap<String, Vec<f64>>>> = Mutex::new(None);

/// Append a sample to the per-path history ring and return the
/// current snapshot. Public so other viewers (HealthViewer,
/// SensorViewer) can embed an inline sparkline for a named scalar
/// field without re-dispatching through the viewer registry.
/// WEFT-271.
pub fn record_sample(path: &str, v: f64) -> Vec<f64> {
    push_sample(path, v)
}

/// Paint a compact inline sparkline for `path` at `height` pixels
/// tall. Width fills the available space (clamped to a sane band).
/// Returns silently if `value` is non-numeric so callers can fan this
/// out across a list of optional fields without pre-checking each.
/// WEFT-271.
pub fn embed_sparkline(ui: &mut egui::Ui, path: &str, value: &Value, height: f32) {
    let Some(current) = value.as_f64() else {
        return;
    };
    let history = push_sample(path, current);
    let w = ui.available_width().clamp(120.0, 480.0);
    let (rect, _resp) = ui.allocate_exact_size(egui::vec2(w, height), egui::Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 2.0, crate::theming::palette::PLOT_BG);
    if history.len() < 2 {
        return;
    }
    let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
    for v in &history {
        if v.is_finite() {
            if *v < lo {
                lo = *v;
            }
            if *v > hi {
                hi = *v;
            }
        }
    }
    if !lo.is_finite() || !hi.is_finite() {
        lo = 0.0;
        hi = 1.0;
    }
    if (hi - lo).abs() < f64::EPSILON {
        lo -= 0.5;
        hi += 0.5;
    }
    let span = hi - lo;
    let denom = (history.len() - 1).max(1) as f32;
    let mut points = Vec::with_capacity(history.len());
    for (i, v) in history.iter().enumerate() {
        let t = i as f32 / denom;
        let x = rect.left() + t * rect.width();
        let n = ((v - lo) / span).clamp(0.0, 1.0) as f32;
        let y = rect.bottom() - n * rect.height();
        points.push(egui::pos2(x, y));
    }
    painter.add(egui::epaint::PathShape::line(
        points,
        egui::Stroke::new(1.2, crate::theming::palette::ACCENT_CYAN),
    ));
}

fn push_sample(path: &str, v: f64) -> Vec<f64> {
    let mut guard = HISTORY.lock().unwrap();
    let map = guard.get_or_insert_with(HashMap::new);
    let buf = map.entry(path.to_string()).or_default();
    // Unconditional push — repeated identical values render as flat
    // runs in the sparkline so a stuck signal is visible as a
    // horizontal line rather than a frozen-looking history.
    buf.push(v);
    if buf.len() > MAX_HISTORY {
        let excess = buf.len() - MAX_HISTORY;
        buf.drain(..excess);
    }
    buf.clone()
}

pub struct TimeSeriesViewer;

impl SubstrateViewer for TimeSeriesViewer {
    fn matches(value: &Value) -> u32 {
        // Only raw numeric scalars — not an object-with-a-numeric-
        // field, since those are somebody else's shape.
        if value.as_f64().is_some() { 5 } else { 0 }
    }

    fn paint(ui: &mut egui::Ui, path: &str, value: &Value) {
        let Some(current) = value.as_f64() else {
            return;
        };
        let history = push_sample(path, current);

        ui.label(
            egui::RichText::new(format!("time series · {path}"))
                .color(crate::theming::palette::TEXT_SUBTLE)
                .small(),
        );
        ui.add_space(4.0);

        // Big current-value readout.
        ui.label(
            egui::RichText::new(format!("{current}"))
                .heading()
                .color(crate::theming::palette::ACCENT_SKY)
                .strong(),
        );

        ui.add_space(4.0);

        let w = ui.available_width().clamp(200.0, 480.0);
        let h = 64.0_f32;
        let (rect, _resp) = ui.allocate_exact_size(egui::vec2(w, h), egui::Sense::hover());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 2.0, crate::theming::palette::PLOT_BG);

        if history.len() < 2 {
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "collecting…",
                egui::FontId::proportional(11.0),
                crate::theming::palette::TEXT_DIM,
            );
            return;
        }

        let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
        for v in &history {
            if v.is_finite() {
                if *v < lo {
                    lo = *v;
                }
                if *v > hi {
                    hi = *v;
                }
            }
        }
        if !lo.is_finite() || !hi.is_finite() {
            lo = 0.0;
            hi = 1.0;
        }
        if (hi - lo).abs() < f64::EPSILON {
            lo -= 0.5;
            hi += 0.5;
        }
        let span = hi - lo;

        let denom = (history.len() - 1).max(1) as f32;
        let mut points = Vec::with_capacity(history.len());
        for (i, v) in history.iter().enumerate() {
            let t = i as f32 / denom;
            let x = rect.left() + t * rect.width();
            let n = ((v - lo) / span).clamp(0.0, 1.0) as f32;
            let y = rect.bottom() - n * rect.height();
            points.push(egui::pos2(x, y));
        }
        painter.add(egui::epaint::PathShape::line(
            points,
            egui::Stroke::new(1.5, crate::theming::palette::ACCENT_CYAN),
        ));

        ui.add_space(2.0);
        ui.label(
            egui::RichText::new(format!(
                "range: {lo:.3} … {hi:.3}   samples: {}",
                history.len()
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

    #[test]
    fn matches_integers() {
        assert_eq!(TimeSeriesViewer::matches(&json!(42)), 5);
        assert_eq!(TimeSeriesViewer::matches(&json!(-7)), 5);
    }

    #[test]
    fn matches_floats() {
        assert_eq!(TimeSeriesViewer::matches(&json!(1.5)), 5);
        assert_eq!(TimeSeriesViewer::matches(&json!(-0.5)), 5);
    }

    #[test]
    fn rejects_string() {
        assert_eq!(TimeSeriesViewer::matches(&json!("42")), 0);
    }

    #[test]
    fn rejects_bool() {
        assert_eq!(TimeSeriesViewer::matches(&json!(true)), 0);
    }

    #[test]
    fn rejects_null() {
        assert_eq!(TimeSeriesViewer::matches(&Value::Null), 0);
    }

    #[test]
    fn rejects_object() {
        assert_eq!(TimeSeriesViewer::matches(&json!({"v": 1})), 0);
    }

    #[test]
    fn rejects_array() {
        assert_eq!(TimeSeriesViewer::matches(&json!([1, 2, 3])), 0);
    }

    #[test]
    fn push_sample_bounds_history() {
        let path = "test/ring/bounded";
        // Fresh path should start empty. Push 2× cap and confirm the
        // buffer stays at cap.
        for i in 0..(MAX_HISTORY * 2) {
            let h = push_sample(path, i as f64);
            assert!(h.len() <= MAX_HISTORY);
        }
        let final_h = push_sample(path, 999.0);
        assert!(final_h.len() <= MAX_HISTORY);
        // Most recent value is always at the end.
        assert_eq!(final_h.last().copied(), Some(999.0));
    }

    #[test]
    fn paint_does_not_panic_on_scalar() {
        let ctx = egui::Context::default();
        let raw_input = egui::RawInput::default();
        let v = json!(1.5);
        let _ = ctx.run_ui(raw_input, |ui| {
            egui::CentralPanel::default().show_inside(ui, |ui| {
                TimeSeriesViewer::paint(ui, "test/scalar/tick", &v);
            });
        });
    }

    #[test]
    fn paint_collects_history_across_calls() {
        let ctx = egui::Context::default();
        let path = "test/scalar/history";
        // Seed with a handful of paints at varying values.
        for v in [1.0, 2.0, 3.0, 4.0, 5.0] {
            let raw_input = egui::RawInput::default();
            let _ = ctx.run_ui(raw_input, |ui| {
                egui::CentralPanel::default().show_inside(ui, |ui| {
                    TimeSeriesViewer::paint(ui, path, &json!(v));
                });
            });
        }
        // Final history should contain at least 5 points for this path.
        let guard = HISTORY.lock().unwrap();
        let map = guard.as_ref().expect("map initialised");
        assert!(map.get(path).map(|h| h.len() >= 5).unwrap_or(false));
    }
}
