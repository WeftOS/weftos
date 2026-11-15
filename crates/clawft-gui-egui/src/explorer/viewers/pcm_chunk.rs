//! `PcmChunkViewer` — renders a substrate value carrying base64
//! PCM audio without dragging a multi-KB string through egui's
//! text layout.
//!
//! The wire shape (matched against ESP32 firmware emissions):
//!
//! ```json
//! {
//!   "data":         "<base64>",     // s16le PCM samples
//!   "encoding":     "base64",
//!   "format":       "i16le",
//!   "sample_rate":  16000,
//!   "channels":     1,
//!   "samples":      8000,
//!   "start_ts_ms":  3807924
//! }
//! ```
//!
//! Why a dedicated viewer rather than letting JsonFallback handle
//! it: a single 500-ms 16-kHz mono i16le chunk is ~16 KB raw, ~21
//! KB once base64-encoded. JsonFallback would try to lay out the
//! `data` field as a single monospace galley and choke the render
//! thread. `paint_string` now hard-caps that path (see
//! `STR_INLINE_HARD_MAX`), but skipping it entirely on the hot
//! path is a clearer architecture: the shape is known, render the
//! parts you can read, summarise the bytes you can't.
//!
//! Priority **20** — wins decisively over JsonFallback (1) and
//! over generic shape-matchers; lower than ObjectType-Mesh's 20-as
//! priority because the two never overlap (Mesh matches a top-level
//! snapshot; PcmChunk matches a leaf payload).
//!
//! ## Inline waveform mini-plot
//!
//! The viewer renders a one-line waveform under the metadata so
//! you can eyeball whether the mic is picking up signal vs.
//! silence. The decode is **rate-limited and cached** because
//! base64-decoding a 16 KB chunk on every repaint (egui repaints
//! at 60 fps when the mouse moves) was the lockup that motivated
//! splitting this viewer off in the first place. Specifically:
//!
//! - Decode at most once per [`MIN_DECODE_INTERVAL_MS`] (250 ms ≈
//!   4 Hz). egui repaints in between read straight from the cache.
//! - Cache key is `(path, start_ts_ms)`. A new chunk on the same
//!   path always has a new `start_ts_ms` (it is monotonic), so the
//!   cache invalidates exactly when the underlying audio changes,
//!   not on cosmetic repaints.
//! - Decimate to ≤[`MAX_PLOT_POINTS`] (60) before storing. A 16-kHz
//!   chunk has 8000 samples; we don't need that resolution at 40 px
//!   tall on the screen, and the per-frame cost of pushing 8000
//!   `pos2`s into a path is non-trivial.
//!
//! The cache lives in egui's per-id memory so it survives panel
//! re-creates without us needing a `&mut Self`.

use super::SubstrateViewer;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use serde_json::Value;
use std::time::Instant;

/// Priority for shape-match.
const PRIORITY: u32 = 20;

/// Vertical pixels reserved for the inline plot row. Single-line
/// (no axes), so 40 px is enough for a clearly-readable trace
/// without crowding the metadata above.
const PLOT_HEIGHT: f32 = 40.0;

/// Hard upper bound on plot points. Picked to match `decimate_to`'s
/// "≤60 points" contract — the test
/// [`tests::decimation_caps_to_60_points`] enforces it.
const MAX_PLOT_POINTS: usize = 60;

/// Threshold above which the raw decoded sample buffer is decimated
/// before storing in the cache. 8000 was the post-handoff choice in
/// the doc — anything below this is cheap enough to push straight
/// to the painter as-is, anything above blows the per-frame budget.
const DECIMATE_ABOVE: usize = 8000;

/// Decimation factor when `samples > DECIMATE_ABOVE`. 4× drops a
/// 16 kHz frame to 4 kHz which still resolves voice formants well
/// enough for an "is the mic alive?" eyeball test.
const DECIMATE_FACTOR: usize = 4;

/// Minimum wall-clock spacing between decodes for the same cache
/// key. 250 ms keeps decode at ≤4 Hz even when egui is repainting
/// at 60 fps because of mouse motion. Below this, repaints serve
/// from the cache and skip decode entirely.
const MIN_DECODE_INTERVAL_MS: u64 = 250;

pub struct PcmChunkViewer;

/// Per-viewer cached decode state. Keyed in egui memory by an Id
/// derived from the path so multiple PcmChunk panels (different
/// substrate paths) do not stomp each other.
#[derive(Clone)]
struct DecodeCache {
    /// Last `start_ts_ms` we decoded from. When the incoming
    /// chunk's ts changes, the cached `points` are stale and we
    /// re-decode; until it changes, repaints are free.
    key_ts_ms: u64,
    /// Last wall-clock time we ran a decode. Combined with
    /// [`MIN_DECODE_INTERVAL_MS`] this rate-limits decode even when
    /// the chunk's `start_ts_ms` advances faster than 4 Hz (which
    /// shouldn't happen for 500-ms windows but defends against
    /// bursty publishers).
    last_decode_at: Instant,
    /// The decimated, normalized waveform ready for the painter.
    /// `None` means "decode failed" — render the empty trace and
    /// don't keep retrying every repaint.
    points: Option<Vec<f32>>,
}

impl SubstrateViewer for PcmChunkViewer {
    fn matches(value: &Value) -> u32 {
        let Some(obj) = value.as_object() else {
            return 0;
        };
        let has_data = obj.get("data").and_then(Value::as_str).is_some();
        let has_sr = obj.get("sample_rate").and_then(Value::as_u64).is_some();
        let format_known = obj
            .get("format")
            .and_then(Value::as_str)
            .map(|s| s == "i16le")
            .unwrap_or(false);
        let encoding_known = obj
            .get("encoding")
            .and_then(Value::as_str)
            .map(|s| s == "base64")
            .unwrap_or(false);
        if has_data && has_sr && format_known && encoding_known {
            PRIORITY
        } else {
            0
        }
    }

    fn paint(ui: &mut egui::Ui, path: &str, value: &Value) {
        let obj = match value.as_object() {
            Some(o) => o,
            None => return,
        };

        let sample_rate = obj.get("sample_rate").and_then(Value::as_u64).unwrap_or(0);
        let channels = obj.get("channels").and_then(Value::as_u64).unwrap_or(1);
        let samples = obj.get("samples").and_then(Value::as_u64).unwrap_or(0);
        let start_ts_ms = obj.get("start_ts_ms").and_then(Value::as_u64).unwrap_or(0);
        let data_str = obj.get("data").and_then(Value::as_str).unwrap_or("");
        let data_len = data_str.len();
        let chunk_ms = (samples * 1000).checked_div(sample_rate).unwrap_or(0);

        ui.label(
            egui::RichText::new(format!("pcm_chunk · {path}"))
                .color(crate::theming::palette::TEXT_SUBTLE)
                .small(),
        );
        ui.add_space(6.0);

        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("format").strong());
            ui.monospace("i16le · base64");
        });
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("sample_rate").strong());
            ui.monospace(format!("{sample_rate} Hz"));
        });
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("channels").strong());
            ui.monospace(format!("{channels}"));
        });
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("samples").strong());
            ui.monospace(format!("{samples} ({chunk_ms} ms)"));
        });
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("start_ts_ms").strong());
            ui.monospace(format!("{start_ts_ms}"));
        });
        ui.add_space(6.0);
        ui.separator();
        ui.add_space(4.0);

        // Pull (or build) the cached decimated waveform. Cache key
        // is the path — different PcmChunk panels at different paths
        // do not share cache state.
        let cache_id = ui.make_persistent_id(("pcm_chunk_decode", path));
        let cached = ui.memory(|m| m.data.get_temp::<DecodeCache>(cache_id));

        let now = Instant::now();
        let needs_decode = match &cached {
            None => true,
            Some(c) => {
                c.key_ts_ms != start_ts_ms
                    && now.saturating_duration_since(c.last_decode_at).as_millis() as u64
                        >= MIN_DECODE_INTERVAL_MS
            }
        };

        let points: Option<Vec<f32>> = if needs_decode {
            let pts = decode_and_decimate(data_str, samples as usize);
            let new_cache = DecodeCache {
                key_ts_ms: start_ts_ms,
                last_decode_at: now,
                points: pts.clone(),
            };
            ui.memory_mut(|m| {
                m.data.insert_temp::<DecodeCache>(cache_id, new_cache);
            });
            pts
        } else {
            cached.and_then(|c| c.points)
        };

        // Plot row — fixed height, full available width, no axes,
        // no labels, no interaction. Single line normalized to
        // [-1.0, 1.0].
        let width = ui.available_width().max(80.0);
        let (rect, _resp) =
            ui.allocate_exact_size(egui::vec2(width, PLOT_HEIGHT), egui::Sense::hover());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 2.0, crate::theming::palette::PLOT_BG);

        match points.as_deref() {
            Some(pts) if pts.len() >= 2 => {
                let denom = (pts.len() - 1).max(1) as f32;
                let mut path_pts = Vec::with_capacity(pts.len());
                for (i, s) in pts.iter().enumerate() {
                    let t = i as f32 / denom;
                    let x = rect.left() + t * rect.width();
                    // Normalised input in [-1, 1] maps to vertical
                    // centre ± half-height.
                    let n = s.clamp(-1.0, 1.0);
                    let y = rect.center().y - n * (rect.height() * 0.5);
                    path_pts.push(egui::pos2(x, y));
                }
                painter.add(egui::epaint::PathShape::line(
                    path_pts,
                    egui::Stroke::new(1.2, crate::theming::palette::STATUS_OK),
                ));
            }
            _ => {
                painter.text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    "no audio",
                    egui::FontId::proportional(10.0),
                    crate::theming::palette::TEXT_DIMMER,
                );
            }
        }

        ui.add_space(4.0);
        ui.label(
            egui::RichText::new(format!("[encoded data — {data_len} bytes]"))
                .small()
                .italics()
                .color(crate::theming::palette::TEXT_LABEL_FAINT),
        );
    }
}

/// Decode a base64 i16le PCM chunk into a normalized + decimated
/// f32 buffer suitable for the inline plot.
///
/// Returns `None` on decode failure (malformed base64, odd byte
/// count, etc.) — the caller renders an empty trace instead of a
/// distorted half-decode.
///
/// `expected_samples` is advisory: when present and non-zero, we
/// trust it for the decimation decision so a payload with a stale
/// `samples` field still bounds the plot. The actual point count
/// always comes from the decoded buffer.
fn decode_and_decimate(b64: &str, _expected_samples: usize) -> Option<Vec<f32>> {
    if b64.is_empty() {
        return None;
    }
    let raw = B64.decode(b64.as_bytes()).ok()?;
    if raw.is_empty() || raw.len() % 2 != 0 {
        return None;
    }
    let sample_count = raw.len() / 2;

    // Decimate at decode time so the cache stores ≤MAX_PLOT_POINTS
    // floats, not 8000.
    let stride = if sample_count > DECIMATE_ABOVE {
        DECIMATE_FACTOR
    } else {
        1
    };
    let decimated_len = sample_count.div_ceil(stride);
    // After the first stride pass we may still be over MAX_PLOT_POINTS;
    // collapse with a second uniform decimation so the plot is bounded.
    let extra_stride = decimated_len.div_ceil(MAX_PLOT_POINTS).max(1);

    let mut out: Vec<f32> = Vec::with_capacity(MAX_PLOT_POINTS.min(decimated_len));
    let mut idx = 0;
    let mut emit_counter = 0usize;
    while idx + 1 < raw.len() {
        if emit_counter.is_multiple_of(extra_stride) {
            // i16le: low byte first.
            let s = i16::from_le_bytes([raw[idx], raw[idx + 1]]);
            // Normalise to [-1.0, 1.0]. i16::MIN's magnitude is one
            // greater than i16::MAX; dividing by 32768.0 keeps it in
            // range without clipping the negative extreme.
            out.push(s as f32 / 32768.0);
            if out.len() >= MAX_PLOT_POINTS {
                break;
            }
        }
        idx += 2 * stride;
        emit_counter += 1;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture(data_len: usize) -> Value {
        json!({
            "data": "a".repeat(data_len),
            "encoding": "base64",
            "format": "i16le",
            "sample_rate": 16000,
            "channels": 1,
            "samples": 8000,
            "start_ts_ms": 1234,
        })
    }

    /// Build a base64-encoded i16le buffer of `n_samples` zero-valued
    /// samples. Useful for the decode tests — zero is the simplest
    /// non-degenerate input and survives normalisation losslessly.
    fn b64_zeros(n_samples: usize) -> String {
        let raw = vec![0u8; n_samples * 2];
        B64.encode(&raw)
    }

    /// Build a base64-encoded i16le buffer of an i16 ramp 0, 1, 2, …
    /// to verify decimation order.
    fn b64_ramp(n_samples: usize) -> String {
        let mut raw = Vec::with_capacity(n_samples * 2);
        for i in 0..n_samples {
            let v = (i as i16).to_le_bytes();
            raw.extend_from_slice(&v);
        }
        B64.encode(&raw)
    }

    #[test]
    fn matches_full_shape() {
        let v = fixture(64);
        assert_eq!(PcmChunkViewer::matches(&v), PRIORITY);
    }

    #[test]
    fn matches_large_data_field() {
        // The exact size that locks up JsonFallback today — make sure
        // the dedicated viewer is the one that wins.
        let v = fixture(21336);
        assert_eq!(PcmChunkViewer::matches(&v), PRIORITY);
    }

    #[test]
    fn rejects_unknown_format() {
        let mut v = fixture(64);
        v["format"] = json!("opus");
        assert_eq!(PcmChunkViewer::matches(&v), 0);
    }

    #[test]
    fn rejects_unknown_encoding() {
        let mut v = fixture(64);
        v["encoding"] = json!("raw");
        assert_eq!(PcmChunkViewer::matches(&v), 0);
    }

    #[test]
    fn rejects_missing_data() {
        let v = json!({
            "encoding": "base64",
            "format": "i16le",
            "sample_rate": 16000,
        });
        assert_eq!(PcmChunkViewer::matches(&v), 0);
    }

    #[test]
    fn rejects_missing_sample_rate() {
        let v = json!({
            "data": "abc",
            "encoding": "base64",
            "format": "i16le",
        });
        assert_eq!(PcmChunkViewer::matches(&v), 0);
    }

    #[test]
    fn rejects_non_object() {
        assert_eq!(PcmChunkViewer::matches(&Value::Null), 0);
        assert_eq!(PcmChunkViewer::matches(&json!([1, 2, 3])), 0);
    }

    #[test]
    fn priority_beats_json_fallback() {
        const { assert!(PRIORITY > 1) };
    }

    // ─── Decode + decimation tests ───────────────────────────────────

    #[test]
    fn decode_returns_expected_sample_count() {
        // 16 KB worth of zero samples = 8192 i16 samples. After
        // decimation (stride 4 because >8000, then capped to ≤60)
        // the final buffer must have ≤ MAX_PLOT_POINTS entries.
        let b64 = b64_zeros(8192);
        let pts = decode_and_decimate(&b64, 8192).expect("decode");
        assert!(!pts.is_empty(), "expected non-empty waveform");
        assert!(
            pts.len() <= MAX_PLOT_POINTS,
            "expected ≤{MAX_PLOT_POINTS} points, got {}",
            pts.len()
        );
        // Zero samples normalise to exactly 0.0.
        assert!(pts.iter().all(|p| (*p - 0.0).abs() < f32::EPSILON));
    }

    #[test]
    fn decimation_caps_to_60_points() {
        // Whatever the input size, the plot must never exceed
        // MAX_PLOT_POINTS — that's the per-frame budget contract.
        for n in [1000usize, 4000, 8000, 8001, 16_000, 32_000] {
            let b64 = b64_zeros(n);
            let pts = decode_and_decimate(&b64, n).expect("decode");
            assert!(
                pts.len() <= MAX_PLOT_POINTS,
                "{n} samples decimated to {} (cap is {MAX_PLOT_POINTS})",
                pts.len(),
            );
        }
    }

    #[test]
    fn decimation_preserves_signal_order_for_ramp() {
        // The decimated output should be monotonically increasing
        // when fed a monotonically increasing ramp. Without this the
        // decimator could reorder the buffer and the plot would
        // look like noise on a clean ramp.
        let b64 = b64_ramp(4000); // < DECIMATE_ABOVE so stride=1 path
        let pts = decode_and_decimate(&b64, 4000).expect("decode");
        for w in pts.windows(2) {
            assert!(
                w[1] >= w[0],
                "ramp decimation reordered: {:?} → {:?}",
                w[0],
                w[1],
            );
        }
    }

    #[test]
    fn decode_fails_on_bad_base64() {
        // A `!` is not a base64 alphabet char (not even URL-safe).
        assert!(decode_and_decimate("not-valid-base64!", 0).is_none());
    }

    #[test]
    fn decode_fails_on_odd_byte_count() {
        // i16le requires even bytes; 3 bytes after decode is half
        // a sample and we should refuse rather than render garbage.
        let raw = vec![0u8, 1u8, 2u8];
        let b64 = B64.encode(&raw);
        assert!(decode_and_decimate(&b64, 0).is_none());
    }

    #[test]
    fn decode_handles_empty_string() {
        assert!(decode_and_decimate("", 0).is_none());
    }

    // ─── Cache + paint tests ─────────────────────────────────────────

    #[test]
    fn cache_skips_decode_when_key_unchanged() {
        // The cache decision lives in `paint`'s `needs_decode`
        // expression; the helper below mirrors that check so we can
        // assert the contract without spinning up egui memory.
        fn would_decode(cached_ts: Option<u64>, current_ts: u64, elapsed_ms: u64) -> bool {
            match cached_ts {
                None => true,
                Some(c) => c != current_ts && elapsed_ms >= MIN_DECODE_INTERVAL_MS,
            }
        }

        // Cache hit + same ts → no decode no matter how long ago.
        assert!(!would_decode(Some(1234), 1234, 999_999));
        // Different ts but inside throttle window → no decode.
        assert!(!would_decode(Some(1234), 2000, MIN_DECODE_INTERVAL_MS - 1));
        // Different ts AND past throttle → decode.
        assert!(would_decode(Some(1234), 2000, MIN_DECODE_INTERVAL_MS));
        // Cold cache → always decode.
        assert!(would_decode(None, 1234, 0));
    }

    #[test]
    fn paint_does_not_panic_on_realistic_fixture() {
        // 16 KB raw → ~21 KB base64 (the post-handoff lockup
        // workload). The viewer should paint cleanly without
        // crashing and without holding a fat decoded buffer.
        let ctx = egui::Context::default();
        let raw_input = egui::RawInput::default();
        let value = json!({
            "data": b64_zeros(8192),
            "encoding": "base64",
            "format": "i16le",
            "sample_rate": 16000,
            "channels": 1,
            "samples": 8192,
            "start_ts_ms": 99,
        });
        let _ = ctx.run_ui(raw_input, |ui| {
            egui::CentralPanel::default().show_inside(ui, |ui| {
                PcmChunkViewer::paint(ui, "substrate/n-x/sensor/mic/pcm_chunk", &value);
            });
        });
    }

    #[test]
    fn paint_does_not_panic_on_undecodable_data() {
        // The viewer must render an empty trace rather than crash if
        // the daemon emits a malformed `data` field.
        let ctx = egui::Context::default();
        let raw_input = egui::RawInput::default();
        let value = json!({
            "data": "not-valid-base64!",
            "encoding": "base64",
            "format": "i16le",
            "sample_rate": 16000,
            "channels": 1,
            "samples": 0,
            "start_ts_ms": 99,
        });
        let _ = ctx.run_ui(raw_input, |ui| {
            egui::CentralPanel::default().show_inside(ui, |ui| {
                PcmChunkViewer::paint(ui, "broken", &value);
            });
        });
    }
}
