//! `DepthMapViewer` — renders a VL53L5CX/L7CX-style depth frame.
//!
//! Matches an object with `depths_mm` (array of numbers), `width`, and
//! `height` (both numeric). Paints a `width` × `height` grid of cells
//! coloured by depth.
//!
//! The palette here mirrors `surface_host::compose::heatmap_color`
//! (the 5-stop viridis-ish ramp introduced in commit 613b58a). The
//! existing `render_heatmap` in `surface_host/compose.rs` is tied to
//! `SurfaceNode`/`OntologySnapshot`, so we inline a minimal grid
//! renderer rather than introduce a circular dependency. If/when that
//! palette gets lifted into a shared module it can be re-imported
//! here — the viewer will keep working unchanged.
//!
//! `0xFFFF` (65535) is the sentinel for "no valid reading" and renders
//! grey, matching the shell.

use super::SubstrateViewer;
use serde_json::Value;

pub struct DepthMapViewer;

impl SubstrateViewer for DepthMapViewer {
    fn matches(value: &Value) -> u32 {
        let Some(obj) = value.as_object() else {
            return 0;
        };
        // `width` and `height` must be numeric.
        let Some(_w) = obj.get("width").and_then(Value::as_u64) else {
            return 0;
        };
        let Some(_h) = obj.get("height").and_then(Value::as_u64) else {
            return 0;
        };
        // `depths_mm` must be an array of numbers (at least one, and
        // every element must be numeric — otherwise the fallback
        // should handle it).
        let Some(arr) = obj.get("depths_mm").and_then(Value::as_array) else {
            return 0;
        };
        if arr.is_empty() {
            return 0;
        }
        if !arr
            .iter()
            .all(|v| v.as_u64().is_some() || v.as_f64().is_some())
        {
            return 0;
        }
        10
    }

    fn paint(ui: &mut egui::Ui, path: &str, value: &Value) {
        let obj = match value.as_object() {
            Some(o) => o,
            None => return,
        };
        let width = obj.get("width").and_then(Value::as_u64).unwrap_or(0) as usize;
        let height = obj.get("height").and_then(Value::as_u64).unwrap_or(0) as usize;
        let data: Vec<u16> = obj
            .get("depths_mm")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .map(|v| v.as_u64().unwrap_or(65535) as u16)
                    .collect()
            })
            .unwrap_or_default();
        let min_mm = obj.get("min_mm").and_then(Value::as_u64).map(|x| x as u16);
        let max_mm = obj.get("max_mm").and_then(Value::as_u64).map(|x| x as u16);

        ui.label(
            egui::RichText::new(format!("depth map · {path}  ({width}×{height})"))
                .color(crate::theming::palette::TEXT_SUBTLE)
                .small(),
        );
        ui.add_space(4.0);

        if width == 0 || height == 0 || data.len() != width * height {
            ui.label(
                egui::RichText::new(format!(
                    "depth_map: no/invalid data (w={width} h={height} len={})",
                    data.len()
                ))
                .color(crate::theming::palette::TEXT_SUBTLE)
                .italics(),
            );
            return;
        }

        let (lo, hi) = match (min_mm, max_mm) {
            (Some(a), Some(b)) if b > a => (a, b),
            _ => {
                let mut mn = u16::MAX;
                let mut mx = 0u16;
                for d in &data {
                    if *d == 65535 {
                        continue;
                    }
                    if *d < mn {
                        mn = *d;
                    }
                    if *d > mx {
                        mx = *d;
                    }
                }
                if mn == u16::MAX {
                    (0, 1)
                } else if mn == mx {
                    (mn, mn.saturating_add(1))
                } else {
                    (mn, mx)
                }
            }
        };

        // Fit the grid into the available width, with sane upper bound.
        let avail = ui.available_width().clamp(120.0, 360.0);
        let gap: f32 = 2.0;
        let cell = ((avail - gap * (width as f32 - 1.0)) / width as f32).clamp(8.0, 32.0);
        let total_w = width as f32 * cell + (width as f32 - 1.0) * gap;
        let total_h = height as f32 * cell + (height as f32 - 1.0) * gap;
        let (rect, _resp) =
            ui.allocate_exact_size(egui::vec2(total_w, total_h), egui::Sense::hover());
        let painter = ui.painter_at(rect);

        for row in 0..height {
            for col in 0..width {
                let idx = row * width + col;
                let raw_px = data[idx];
                let color = heatmap_color(raw_px, lo, hi);
                let x0 = rect.left() + col as f32 * (cell + gap);
                let y0 = rect.top() + row as f32 * (cell + gap);
                let cell_rect =
                    egui::Rect::from_min_size(egui::pos2(x0, y0), egui::vec2(cell, cell));
                painter.rect_filled(cell_rect, 2.0, color);
            }
        }

        ui.add_space(4.0);
        ui.label(
            egui::RichText::new(format!("range: {lo} … {hi} mm"))
                .small()
                .color(crate::theming::palette::TEXT_SYSTEM_MSG),
        );
    }
}

/// 5-stop viridis-ish colormap. Kept in sync with
/// `surface_host::compose::heatmap_color` — 0xFFFF renders mid-grey.
fn heatmap_color(mm: u16, min: u16, max: u16) -> egui::Color32 {
    if mm == 65535 {
        return crate::theming::palette::DEPTH_MISSING;
    }
    let span = (max.saturating_sub(min)).max(1) as f32;
    let clamped = mm.clamp(min, max);
    let t = ((clamped - min) as f32 / span).clamp(0.0, 1.0);
    let stops = [
        (0.00_f32, [38u8, 18, 110]),
        (0.25, [30, 120, 200]),
        (0.50, [50, 200, 120]),
        (0.75, [220, 200, 60]),
        (1.00, [220, 70, 60]),
    ];
    for i in 0..stops.len() - 1 {
        let (t0, c0) = stops[i];
        let (t1, c1) = stops[i + 1];
        if t <= t1 {
            let local = ((t - t0) / (t1 - t0)).clamp(0.0, 1.0);
            let r = (c0[0] as f32 + (c1[0] as f32 - c0[0] as f32) * local) as u8;
            let g = (c0[1] as f32 + (c1[1] as f32 - c0[1] as f32) * local) as u8;
            let b = (c0[2] as f32 + (c1[2] as f32 - c0[2] as f32) * local) as u8;
            return crate::theming::palette::data_rgb(r, g, b);
        }
    }
    crate::theming::palette::DEPTH_FAR
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_frame_8x8() -> Value {
        let depths: Vec<u64> = (0..64).map(|i| 200 + i * 10).collect();
        json!({
            "width": 8,
            "height": 8,
            "depths_mm": depths,
        })
    }

    #[test]
    fn matches_well_formed_frame() {
        assert_eq!(DepthMapViewer::matches(&sample_frame_8x8()), 10);
    }

    #[test]
    fn matches_frame_with_sentinels() {
        let v = json!({
            "width": 2,
            "height": 2,
            "depths_mm": [200, 65535, 300, 65535],
        });
        assert_eq!(DepthMapViewer::matches(&v), 10);
    }

    #[test]
    fn rejects_missing_width() {
        let v = json!({ "height": 8, "depths_mm": [0, 1] });
        assert_eq!(DepthMapViewer::matches(&v), 0);
    }

    #[test]
    fn rejects_width_as_string() {
        let v = json!({ "width": "8", "height": 8, "depths_mm": [0, 1] });
        assert_eq!(DepthMapViewer::matches(&v), 0);
    }

    #[test]
    fn rejects_depths_as_strings() {
        let v = json!({
            "width": 2,
            "height": 1,
            "depths_mm": ["200", "300"],
        });
        assert_eq!(DepthMapViewer::matches(&v), 0);
    }

    #[test]
    fn rejects_depths_missing() {
        let v = json!({ "width": 8, "height": 8 });
        assert_eq!(DepthMapViewer::matches(&v), 0);
    }

    #[test]
    fn rejects_depths_empty_array() {
        let v = json!({ "width": 0, "height": 0, "depths_mm": [] });
        assert_eq!(DepthMapViewer::matches(&v), 0);
    }

    #[test]
    fn rejects_depths_mixed_types() {
        let v = json!({
            "width": 2,
            "height": 1,
            "depths_mm": [200, "not a number"],
        });
        assert_eq!(DepthMapViewer::matches(&v), 0);
    }

    #[test]
    fn rejects_empty_object() {
        let v = json!({});
        assert_eq!(DepthMapViewer::matches(&v), 0);
    }

    #[test]
    fn rejects_null() {
        assert_eq!(DepthMapViewer::matches(&Value::Null), 0);
    }

    #[test]
    fn rejects_array_root() {
        let v = json!([0, 1, 2]);
        assert_eq!(DepthMapViewer::matches(&v), 0);
    }
}
