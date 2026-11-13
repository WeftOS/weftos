//! Lowest-priority viewer — always wins when no specialized viewer
//! matches. Renders `value` as pretty JSON with type badges, monospace
//! values, and collapsible sections for nested objects/arrays.

use super::SubstrateViewer;
use serde_json::Value;

/// Max string length shown inline before we clip + offer an expand-to-full
/// toggle. Mirrors what the substrate's logs pane does so long blobs
/// don't blow up the right-hand pane.
const STR_CLIP_LEN: usize = 200;

/// Hard upper bound — beyond this we never inline-render the full
/// content, regardless of expand state. Otherwise a multi-KB blob
/// (the pcm_chunk's `data` field is ~21 KB of base64) re-lays out
/// a giant galley every frame and locks up the GUI on Windows.
/// Above this threshold we show only a clipped preview + size +
/// copy-to-clipboard, no expand button.
const STR_INLINE_HARD_MAX: usize = 4_096;

pub struct JsonFallbackViewer;

impl SubstrateViewer for JsonFallbackViewer {
    /// Priority 1 — per plan §3.3, this is the catch-all. Every other
    /// viewer returns 0 when it doesn't match, so 1 always loses to a
    /// real match but beats the `0` no-match signal.
    fn matches(_value: &Value) -> u32 {
        1
    }

    fn paint(ui: &mut egui::Ui, path: &str, value: &Value) {
        ui.horizontal(|ui| {
            ui.monospace(path);
            ui.separator();
            ui.label(type_badge_text(value));
        });
        ui.separator();

        egui::ScrollArea::both()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                paint_value(ui, path, value, 0);
            });
    }
}

/// Render a single JSON value. `depth` is the nesting depth used to
/// stabilise CollapsingHeader ids across rebuilds — egui needs unique
/// ids for each header in a frame.
fn paint_value(ui: &mut egui::Ui, id_prefix: &str, value: &Value, depth: usize) {
    match value {
        Value::Null => {
            ui.horizontal(|ui| {
                badge(ui, "null", NULL_COLOR);
                ui.monospace("null");
            });
        }
        Value::Bool(b) => {
            ui.horizontal(|ui| {
                badge(ui, "bool", BOOL_COLOR);
                ui.monospace(b.to_string());
            });
        }
        Value::Number(n) => {
            ui.horizontal(|ui| {
                badge(ui, "num", NUM_COLOR);
                ui.monospace(n.to_string());
            });
        }
        Value::String(s) => {
            paint_string(ui, id_prefix, s, depth);
        }
        Value::Array(arr) => {
            paint_array(ui, id_prefix, arr, depth);
        }
        Value::Object(map) => {
            paint_object(ui, id_prefix, map, depth);
        }
    }
}

/// Render a string value with clipping + expand-to-full when it runs
/// past [`STR_CLIP_LEN`].
///
/// Strings longer than [`STR_INLINE_HARD_MAX`] are *never* inline-
/// rendered in full — only a clipped preview plus a size badge plus
/// a "copy" button. This prevents the GUI from locking up on
/// pathological values (raw base64 audio chunks at ~21 KB, etc.)
/// where re-laying out a giant monospace galley every frame
/// chokes the render thread.
fn paint_string(ui: &mut egui::Ui, id_prefix: &str, s: &str, _depth: usize) {
    let len = s.len();

    // Pathological-blob branch: never inline-render in full, no
    // expand affordance. This is the lockup guard.
    if len > STR_INLINE_HARD_MAX {
        let (display, _) = clip_string(s, STR_CLIP_LEN);
        ui.horizontal_wrapped(|ui| {
            badge(ui, "str", STR_COLOR);
            ui.monospace(format!("\"{display}…\""));
            ui.label(
                egui::RichText::new(format!("[{} bytes]", len))
                    .small()
                    .color(crate::theming::palette::TEXT_LABEL_FAINT),
            );
            if ui.small_button("copy").clicked() {
                ui.ctx().copy_text(s.to_string());
            }
        });
        return;
    }

    // Persist expand state per (path, length) using egui's Id
    // memory. Keying on the string length keeps the bool sticky across
    // frames as long as the value's length is the same; resets if the
    // shape changes under us (subscription updates).
    let id = egui::Id::new(("weft-explorer-string-expand", id_prefix, len));
    let mut expanded = ui
        .ctx()
        .data_mut(|d| d.get_temp::<bool>(id).unwrap_or(false));

    let (display, truncated) = clip_string(s, STR_CLIP_LEN);
    ui.horizontal_wrapped(|ui| {
        badge(ui, "str", STR_COLOR);
        if truncated && !expanded {
            ui.monospace(format!("\"{display}…\""));
            if ui.small_button("expand").clicked() {
                expanded = true;
            }
        } else if truncated {
            // Collapse button when fully shown.
            if ui.small_button("collapse").clicked() {
                expanded = false;
            }
            ui.monospace(format!("\"{s}\""));
        } else {
            ui.monospace(format!("\"{s}\""));
        }
    });

    ui.ctx().data_mut(|d| d.insert_temp(id, expanded));
}

/// Paint a JSON array with per-element rows. 100+ element arrays stay
/// responsive because the outer `ScrollArea::both` clips off-screen
/// rows — egui only lays out what's visible when paired with
/// `CollapsingHeader` collapsed by default.
fn paint_array(ui: &mut egui::Ui, id_prefix: &str, arr: &[Value], depth: usize) {
    let n = arr.len();
    let label = format!("array[{n}]");
    let id = egui::Id::new(("weft-explorer-arr", id_prefix, depth));
    // Default-closed above a small threshold so a 100-element sensor
    // frame doesn't dump 100 rows on first select.
    let default_open = n <= 8;
    egui::CollapsingHeader::new(label)
        .id_salt(id)
        .default_open(default_open)
        .show(ui, |ui| {
            for (i, v) in arr.iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.monospace(format!("[{i}]"));
                    let child_id = format!("{id_prefix}[{i}]");
                    paint_value(ui, &child_id, v, depth + 1);
                });
            }
        });
}

/// Paint a JSON object with per-key rows. For wide objects (100+
/// keys) we stay in a CollapsingHeader closed-by-default so the tree
/// stays walkable — the top-level dispatch already opens the root.
fn paint_object(
    ui: &mut egui::Ui,
    id_prefix: &str,
    map: &serde_json::Map<String, Value>,
    depth: usize,
) {
    let k = map.len();
    let id = egui::Id::new(("weft-explorer-obj", id_prefix, depth));

    // Top level (depth == 0) is always flat — the detail pane owns the
    // header already. Nested objects get a collapsing section.
    if depth == 0 {
        paint_object_body(ui, id_prefix, map, depth);
        return;
    }

    let default_open = k <= 8;
    egui::CollapsingHeader::new(format!("object{{{k}}}"))
        .id_salt(id)
        .default_open(default_open)
        .show(ui, |ui| {
            paint_object_body(ui, id_prefix, map, depth);
        });
}

fn paint_object_body(
    ui: &mut egui::Ui,
    id_prefix: &str,
    map: &serde_json::Map<String, Value>,
    depth: usize,
) {
    for (key, v) in map {
        match v {
            // Scalars render inline on one line: `key: [badge] value`.
            Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {
                ui.horizontal_wrapped(|ui| {
                    ui.strong(format!("{key}:"));
                    let child_id = format!("{id_prefix}/{key}");
                    paint_value(ui, &child_id, v, depth + 1);
                });
            }
            // Composite values get their own collapsing section labelled
            // by the key — keeps the outline readable for large shapes.
            Value::Array(_) | Value::Object(_) => {
                let child_id = format!("{id_prefix}/{key}");
                let id = egui::Id::new(("weft-explorer-obj-field", &child_id, depth));
                let type_label = match v {
                    Value::Array(a) => format!("array[{}]", a.len()),
                    Value::Object(o) => format!("object{{{}}}", o.len()),
                    _ => String::new(),
                };
                // Default-closed for sensor-sized shapes so selecting a
                // path with a 100-key payload doesn't inflate on entry.
                let size = match v {
                    Value::Array(a) => a.len(),
                    Value::Object(o) => o.len(),
                    _ => 0,
                };
                let default_open = size <= 8;
                egui::CollapsingHeader::new(format!("{key}  ({type_label})"))
                    .id_salt(id)
                    .default_open(default_open)
                    .show(ui, |ui| {
                        paint_value(ui, &child_id, v, depth + 1);
                    });
            }
        }
    }
}

/// One-line badge with the JSON type of `value`, used in the header.
fn type_badge_text(value: &Value) -> egui::RichText {
    let (text, color) = match value {
        Value::Null => ("null", NULL_COLOR),
        Value::Bool(_) => ("bool", BOOL_COLOR),
        Value::Number(_) => ("num", NUM_COLOR),
        Value::String(_) => ("str", STR_COLOR),
        Value::Array(a) => {
            return egui::RichText::new(format!("array[{}]", a.len()))
                .monospace()
                .small()
                .color(ARRAY_COLOR);
        }
        Value::Object(o) => {
            return egui::RichText::new(format!("object{{{}}}", o.len()))
                .monospace()
                .small()
                .color(OBJECT_COLOR);
        }
    };
    egui::RichText::new(text).monospace().small().color(color)
}

/// Small colored pill-badge. Passing through `Label` with a background
/// stroke would be closer to a real pill, but this is the cheapest
/// thing that visually distinguishes the type at a glance.
fn badge(ui: &mut egui::Ui, text: &str, color: egui::Color32) {
    ui.label(egui::RichText::new(text).monospace().small().color(color));
}

/// Clip `s` to at most `max` chars (by char boundary, not byte boundary —
/// important for multibyte codepoints). Returns `(clipped, was_clipped)`.
fn clip_string(s: &str, max: usize) -> (String, bool) {
    let mut char_indices = s.char_indices();
    if let Some((byte_idx, _)) = char_indices.nth(max) {
        (s[..byte_idx].to_string(), true)
    } else {
        (s.to_string(), false)
    }
}

// ── Colors ──────────────────────────────────────────────────────────

const NULL_COLOR: egui::Color32 = crate::theming::palette::TEXT_LABEL_FAINT;
const BOOL_COLOR: egui::Color32 = crate::theming::palette::JSON_BOOL;
const NUM_COLOR: egui::Color32 = crate::theming::palette::ACCENT_CYAN;
const STR_COLOR: egui::Color32 = crate::theming::palette::JSON_STR;
const ARRAY_COLOR: egui::Color32 = crate::theming::palette::JSON_ARRAY;
const OBJECT_COLOR: egui::Color32 = crate::theming::palette::JSON_OBJECT;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn matches_always_returns_one() {
        assert_eq!(JsonFallbackViewer::matches(&Value::Null), 1);
        assert_eq!(JsonFallbackViewer::matches(&json!(42)), 1);
        assert_eq!(JsonFallbackViewer::matches(&json!({"x": 1})), 1);
    }

    #[test]
    fn clip_string_under_limit_is_untouched() {
        let (s, clipped) = clip_string("hello", 200);
        assert_eq!(s, "hello");
        assert!(!clipped);
    }

    #[test]
    fn clip_string_over_limit_is_truncated() {
        let long = "x".repeat(300);
        let (s, clipped) = clip_string(&long, 200);
        assert_eq!(s.chars().count(), 200);
        assert!(clipped);
    }

    #[test]
    fn clip_string_respects_multibyte() {
        // 3 codepoints, each 3 bytes in UTF-8
        let s = "日本語";
        let (out, clipped) = clip_string(s, 2);
        assert_eq!(out.chars().count(), 2);
        assert!(clipped);
    }
}
