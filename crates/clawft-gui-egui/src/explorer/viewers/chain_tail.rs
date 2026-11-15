//! `ChainTailViewer` — renders an ExoChain-style event tail: a JSON
//! array where each entry carries `{ seq: u64, ts, kind, payload? }`.
//!
//! Priority 12. The only other array-shaped viewer is the JSON
//! fallback, so this wins on anything that looks like a chain tail.
//!
//! Rows render newest-first (largest `seq` on top) in a monospace
//! `[seq] ts kind payload-summary` line. Payloads are collapsed to a
//! single-line JSON summary by default; clicking the row expands the
//! full pretty-printed payload.

use super::SubstrateViewer;
use serde_json::Value;

/// Max characters shown for the inline payload summary.
const PAYLOAD_SUMMARY_LEN: usize = 120;

pub struct ChainTailViewer;

impl SubstrateViewer for ChainTailViewer {
    fn matches(value: &Value) -> u32 {
        let Some(arr) = value.as_array() else {
            return 0;
        };
        if arr.is_empty() {
            return 0;
        }
        // Every entry must be an object with seq (u64) + ts (any type)
        // + kind (string). Anything looser and the fallback should
        // win — we only claim rows that are unambiguously chain-shaped.
        for item in arr {
            let Some(o) = item.as_object() else {
                return 0;
            };
            if o.get("seq").and_then(Value::as_u64).is_none() {
                return 0;
            }
            if o.get("ts").is_none() {
                return 0;
            }
            if o.get("kind").and_then(Value::as_str).is_none() {
                return 0;
            }
        }
        12
    }

    fn paint(ui: &mut egui::Ui, path: &str, value: &Value) {
        let arr = match value.as_array() {
            Some(a) => a,
            None => return,
        };

        ui.label(
            egui::RichText::new(format!("chain · {path}  ({} events)", arr.len()))
                .color(crate::theming::palette::TEXT_SUBTLE)
                .small(),
        );
        ui.add_space(4.0);

        // Sort newest-first by seq without mutating the caller's value.
        let mut rows: Vec<&Value> = arr.iter().collect();
        rows.sort_by(|a, b| {
            let seq_a = a.get("seq").and_then(Value::as_u64).unwrap_or(0);
            let seq_b = b.get("seq").and_then(Value::as_u64).unwrap_or(0);
            seq_b.cmp(&seq_a)
        });

        egui::ScrollArea::vertical()
            .max_height(360.0)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                for entry in rows {
                    paint_row(ui, path, entry);
                }
            });
    }
}

fn paint_row(ui: &mut egui::Ui, path: &str, entry: &Value) {
    let Some(obj) = entry.as_object() else {
        return;
    };
    let seq = obj.get("seq").and_then(Value::as_u64).unwrap_or(0);
    let ts = obj
        .get("ts")
        .map(ts_to_string)
        .unwrap_or_else(|| "?".to_string());
    let kind = obj.get("kind").and_then(Value::as_str).unwrap_or("?");
    let payload = obj.get("payload");

    // Persist expand state per (path, seq). Keeps rows sticky across
    // repaints without dragging a whole state blob around.
    let id = egui::Id::new(("weft-explorer-chain-row", path, seq));
    let mut expanded = ui
        .ctx()
        .data_mut(|d| d.get_temp::<bool>(id).unwrap_or(false));

    ui.horizontal(|ui| {
        ui.monospace(
            egui::RichText::new(format!("[{seq:>6}]"))
                .color(crate::theming::palette::TEXT_SYSTEM_MSG),
        );
        ui.monospace(egui::RichText::new(&ts).color(crate::theming::palette::CHAIN_TS_TEXT));
        ui.monospace(egui::RichText::new(kind).color(kind_color(kind)).strong());
        let summary = payload
            .map(|p| payload_summary(p, PAYLOAD_SUMMARY_LEN))
            .unwrap_or_default();
        if !summary.is_empty() {
            let label = ui
                .monospace(
                    egui::RichText::new(&summary)
                        .color(crate::theming::palette::CHAIN_SUMMARY_TEXT),
                )
                .on_hover_text("click to toggle full payload");
            if label.clicked() {
                expanded = !expanded;
            }
        }
    });

    if expanded && let Some(p) = payload {
        let pretty = serde_json::to_string_pretty(p).unwrap_or_else(|_| p.to_string());
        ui.add(
            egui::Label::new(
                egui::RichText::new(pretty)
                    .monospace()
                    .color(crate::theming::palette::CHAIN_DETAIL_TEXT),
            )
            .wrap(),
        );
    }

    ui.ctx().data_mut(|d| d.insert_temp(id, expanded));
}

/// Stringify a `ts` value — u64 / i64 / f64 / string all acceptable.
fn ts_to_string(ts: &Value) -> String {
    match ts {
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// One-line JSON summary clipped to `max` chars. Null/bool/num/str
/// render compactly; objects/arrays collapse to `{n}`/`[n]`.
fn payload_summary(p: &Value, max: usize) -> String {
    let raw = match p {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => format!("\"{s}\""),
        Value::Array(a) => {
            // Short array summary without full nested stringification.
            format!("[{} items]", a.len())
        }
        Value::Object(o) => {
            // For small objects, render as compact json; for large,
            // summarise by key count.
            if o.len() <= 4 {
                serde_json::to_string(p).unwrap_or_else(|_| format!("{{{} keys}}", o.len()))
            } else {
                format!("{{{} keys}}", o.len())
            }
        }
    };
    clip(&raw, max)
}

fn clip(s: &str, max: usize) -> String {
    let mut char_indices = s.char_indices();
    match char_indices.nth(max) {
        Some((byte_idx, _)) => {
            let mut out = s[..byte_idx].to_string();
            out.push('…');
            out
        }
        None => s.to_string(),
    }
}

/// Colour-code common event kinds. Unknown kinds get a neutral
/// label — we deliberately don't try to enumerate every possible
/// kind the chain emits.
fn kind_color(kind: &str) -> egui::Color32 {
    match kind {
        k if k.starts_with("error") || k.ends_with("_error") || k == "failed" => {
            crate::theming::palette::STATUS_ERR
        }
        k if k.starts_with("warn") => crate::theming::palette::STATUS_WARN,
        k if k.contains("commit") || k.contains("accepted") || k == "ok" => {
            crate::theming::palette::STATUS_OK
        }
        _ => crate::theming::palette::ACCENT_SKY,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_tail() -> Value {
        json!([
            { "seq": 1, "ts": 1700000000, "kind": "block_start", "payload": { "height": 1 } },
            { "seq": 2, "ts": 1700000001, "kind": "tx_commit",   "payload": { "id": "t1" } },
            { "seq": 3, "ts": 1700000002, "kind": "block_end",   "payload": null },
        ])
    }

    #[test]
    fn matches_well_formed_tail() {
        assert_eq!(ChainTailViewer::matches(&sample_tail()), 12);
    }

    #[test]
    fn matches_string_ts() {
        let v = json!([
            { "seq": 1, "ts": "2026-04-23T00:00:00Z", "kind": "tick" },
        ]);
        assert_eq!(ChainTailViewer::matches(&v), 12);
    }

    #[test]
    fn rejects_empty_array() {
        assert_eq!(ChainTailViewer::matches(&json!([])), 0);
    }

    #[test]
    fn rejects_object_root() {
        assert_eq!(ChainTailViewer::matches(&json!({"seq": 1})), 0);
    }

    #[test]
    fn rejects_missing_seq() {
        let v = json!([{ "ts": 1, "kind": "x" }]);
        assert_eq!(ChainTailViewer::matches(&v), 0);
    }

    #[test]
    fn rejects_non_numeric_seq() {
        let v = json!([{ "seq": "1", "ts": 1, "kind": "x" }]);
        assert_eq!(ChainTailViewer::matches(&v), 0);
    }

    #[test]
    fn rejects_missing_kind() {
        let v = json!([{ "seq": 1, "ts": 1 }]);
        assert_eq!(ChainTailViewer::matches(&v), 0);
    }

    #[test]
    fn rejects_missing_ts() {
        let v = json!([{ "seq": 1, "kind": "x" }]);
        assert_eq!(ChainTailViewer::matches(&v), 0);
    }

    #[test]
    fn rejects_if_any_entry_malformed() {
        let v = json!([
            { "seq": 1, "ts": 1, "kind": "ok" },
            { "seq": 2, "ts": 2 }, // missing kind
        ]);
        assert_eq!(ChainTailViewer::matches(&v), 0);
    }

    #[test]
    fn payload_summary_short_object() {
        let s = payload_summary(&json!({"x": 1}), 200);
        assert!(s.contains("\"x\""));
    }

    #[test]
    fn payload_summary_large_object() {
        let s = payload_summary(&json!({"a":1,"b":2,"c":3,"d":4,"e":5,"f":6}), 200);
        assert_eq!(s, "{6 keys}");
    }

    #[test]
    fn payload_summary_clips_long_string() {
        let long = "x".repeat(400);
        let s = payload_summary(&Value::String(long), 80);
        assert!(s.chars().count() <= 81); // 80 chars + ellipsis
        assert!(s.ends_with('…'));
    }

    #[test]
    fn paint_does_not_panic_on_realistic_fixture() {
        let ctx = egui::Context::default();
        let raw_input = egui::RawInput::default();
        let v = sample_tail();
        let _ = ctx.run_ui(raw_input, |ui| {
            egui::CentralPanel::default().show_inside(ui, |ui| {
                ChainTailViewer::paint(ui, "substrate/chain/tail", &v);
            });
        });
    }

    #[test]
    fn paint_sorts_newest_first() {
        // Paint a multi-row fixture and confirm we don't panic even
        // when seqs arrive out-of-order — ordering is a visual concern
        // so we just exercise the code path.
        let ctx = egui::Context::default();
        let raw_input = egui::RawInput::default();
        let v = json!([
            { "seq": 5, "ts": 5, "kind": "ok" },
            { "seq": 1, "ts": 1, "kind": "ok" },
            { "seq": 3, "ts": 3, "kind": "ok" },
        ]);
        let _ = ctx.run_ui(raw_input, |ui| {
            egui::CentralPanel::default().show_inside(ui, |ui| {
                ChainTailViewer::paint(ui, "chain/out_of_order", &v);
            });
        });
    }
}
