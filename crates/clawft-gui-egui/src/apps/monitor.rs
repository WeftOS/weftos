//! Monitor — system dashboard. DESIGN.md §9 sidebar 7, archetype
//! `tile-grid`. Graduated under WEFT-585.
//!
//! Tile schema (0.7.0 cut):
//! - Kernel — uptime / load (process count) / memory proxy from
//!   `snap.status`. KPI numbers + a small sparkline if a rolling
//!   buffer is available (none today; placeholder).
//! - Mesh — total / healthy node counts from `snap.mesh_status`.
//! - Chain — height + latency from `snap.chain_status`.
//! - Sensors — one tile per present sensor field on `Snapshot`
//!   (`audio_mic`, `tof_depth`, `network_battery`).
//!
//! Tiles are 220x140 with 12 px gap, wrapping. When *no* tile would
//! have data (everything in `snap` is None), the empty state is
//! drawn instead. When at least one source is publishing, every
//! tile is rendered — sources without data show "—" so the user
//! sees the schema even before all adapters are wired.

use std::sync::Arc;

use eframe::egui;

use crate::live::{Live, Snapshot};
use crate::shell::desktop::Desktop;
use crate::theming::Tokens;

const TILE_W: f32 = 220.0;
const TILE_H: f32 = 140.0;
const GAP: f32 = 12.0;
const PAD: f32 = 16.0;

pub fn show(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    _desk: &mut Desktop,
    _live: &Arc<Live>,
    snap: &Snapshot,
) {
    super::paint_heading(ui, rect, "Monitor");
    let body = egui::Rect::from_min_max(egui::pos2(rect.left(), rect.top() + 64.0), rect.max);

    // Empty state: only when *nothing at all* is publishing. As soon
    // as a single source has data we paint the full grid (other tiles
    // simply show "—") so the user can see the schema.
    let any_data = snap.status.is_some()
        || snap.mesh_status.is_some()
        || snap.chain_status.is_some()
        || snap.audio_mic.is_some()
        || snap.tof_depth.is_some()
        || snap.network_battery.is_some();
    if !any_data {
        super::state::render_if_needed(
            ui,
            body,
            snap,
            false,
            "No sensor adapters publishing",
            Some("Install one with `weft adapter install sensors-host`."),
        );
        return;
    }

    let tokens = Tokens::default();
    let tiles = build_tiles(snap);
    paint_tile_grid(ui, body, &tokens, &tiles);
}

/// One tile in the dashboard grid. The rendering pass reads `kpi`
/// for the headline number and `sub` for the label/unit/footer line.
struct Tile {
    title: &'static str,
    kpi: String,
    sub: String,
    /// Optional rolling-window samples for a small sparkline. None
    /// today across the board (no historical buffers are published
    /// yet); kept on the struct so the renderer is ready when
    /// adapters start emitting series.
    spark: Option<Vec<f32>>,
}

fn build_tiles(snap: &Snapshot) -> Vec<Tile> {
    let mut out = Vec::new();

    // Kernel.
    out.push(kernel_tile(snap));
    // Mesh.
    out.push(mesh_tile(snap));
    // Chain.
    out.push(chain_tile(snap));
    // Sensors — only emit tiles for fields actually present so the
    // grid stays compact when only one sensor is wired.
    if snap.audio_mic.is_some() {
        out.push(audio_tile(snap));
    }
    if snap.tof_depth.is_some() {
        out.push(tof_tile(snap));
    }
    if snap.network_battery.is_some() {
        out.push(battery_tile(snap));
    }
    out
}

fn kernel_tile(snap: &Snapshot) -> Tile {
    let (kpi, sub) = match &snap.status {
        Some(s) => {
            let state = s
                .get("state")
                .and_then(|v| v.as_str())
                .unwrap_or("—")
                .to_string();
            let uptime = s.get("uptime_secs").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let procs = s.get("process_count").and_then(|v| v.as_u64()).unwrap_or(0);
            let svcs = s.get("service_count").and_then(|v| v.as_u64()).unwrap_or(0);
            (
                state,
                format!("up {} · {procs}p / {svcs}s", fmt_duration(uptime)),
            )
        }
        None => ("—".to_string(), "no kernel data".to_string()),
    };
    Tile {
        title: "Kernel",
        kpi,
        sub,
        spark: None,
    }
}

fn mesh_tile(snap: &Snapshot) -> Tile {
    let (kpi, sub) = match &snap.mesh_status {
        Some(v) => {
            let total = v.get("total_nodes").and_then(|n| n.as_u64()).unwrap_or(0);
            let healthy = v.get("healthy_nodes").and_then(|n| n.as_u64()).unwrap_or(0);
            (
                format!("{healthy}/{total}"),
                if total == 0 {
                    "no peers".to_string()
                } else {
                    "healthy / total".to_string()
                },
            )
        }
        None => ("—".to_string(), "mesh adapter offline".to_string()),
    };
    Tile {
        title: "Mesh",
        kpi,
        sub,
        spark: None,
    }
}

fn chain_tile(snap: &Snapshot) -> Tile {
    let (kpi, sub) = match &snap.chain_status {
        Some(v) => {
            let available = v
                .get("available")
                .and_then(|b| b.as_bool())
                .unwrap_or(false);
            if !available {
                let reason = v
                    .get("reason")
                    .and_then(|s| s.as_str())
                    .unwrap_or("unavailable");
                ("—".to_string(), reason.to_string())
            } else {
                let height = v
                    .get("sequence")
                    .and_then(|n| n.as_u64())
                    .or_else(|| v.get("event_count").and_then(|n| n.as_u64()))
                    .unwrap_or(0);
                let chain_id = v
                    .get("chain_id")
                    .and_then(|s| s.as_str())
                    .unwrap_or("local");
                (format!("#{height}"), format!("chain {chain_id}"))
            }
        }
        None => ("—".to_string(), "chain adapter offline".to_string()),
    };
    Tile {
        title: "Chain",
        kpi,
        sub,
        spark: None,
    }
}

fn audio_tile(snap: &Snapshot) -> Tile {
    let (kpi, sub) = match &snap.audio_mic {
        Some(v) => {
            let avail = v
                .get("available")
                .and_then(|b| b.as_bool())
                .unwrap_or(false);
            if !avail {
                ("—".to_string(), "mic offline".to_string())
            } else {
                let rms = v.get("rms_db").and_then(|n| n.as_f64()).unwrap_or(0.0);
                let sr = v.get("sample_rate").and_then(|n| n.as_u64()).unwrap_or(0);
                (format!("{rms:.1} dB"), format!("sample rate {sr} Hz"))
            }
        }
        None => ("—".to_string(), "no mic".to_string()),
    };
    Tile {
        title: "Mic",
        kpi,
        sub,
        spark: None,
    }
}

fn tof_tile(snap: &Snapshot) -> Tile {
    let (kpi, sub) = match &snap.tof_depth {
        Some(v) => {
            let avail = v
                .get("available")
                .and_then(|b| b.as_bool())
                .unwrap_or(false);
            if !avail {
                ("—".to_string(), "tof offline".to_string())
            } else {
                let w = v.get("width").and_then(|n| n.as_u64()).unwrap_or(0);
                let h = v.get("height").and_then(|n| n.as_u64()).unwrap_or(0);
                let min = v
                    .get("min_mm")
                    .and_then(|n| n.as_u64())
                    .map(|m| format!("min {m} mm"))
                    .unwrap_or_else(|| "min —".into());
                (format!("{w}×{h}"), min)
            }
        }
        None => ("—".to_string(), "no tof".to_string()),
    };
    Tile {
        title: "ToF",
        kpi,
        sub,
        spark: None,
    }
}

fn battery_tile(snap: &Snapshot) -> Tile {
    let (kpi, sub) = match &snap.network_battery {
        Some(v) => {
            let present = v.get("present").and_then(|b| b.as_bool()).unwrap_or(false);
            if !present {
                ("—".to_string(), "no battery".to_string())
            } else {
                let pct = v.get("percent").and_then(|n| n.as_u64()).unwrap_or(0);
                let charging = v.get("charging").and_then(|b| b.as_bool()).unwrap_or(false);
                (
                    format!("{pct}%"),
                    if charging {
                        "charging".to_string()
                    } else {
                        "on battery".to_string()
                    },
                )
            }
        }
        None => ("—".to_string(), "no battery field".to_string()),
    };
    Tile {
        title: "Battery",
        kpi,
        sub,
        spark: None,
    }
}

fn fmt_duration(secs: f64) -> String {
    let s = secs as u64;
    let h = s / 3600;
    let m = (s % 3600) / 60;
    let sec = s % 60;
    if h > 0 {
        format!("{h}h{m}m")
    } else if m > 0 {
        format!("{m}m{sec}s")
    } else {
        format!("{sec}s")
    }
}

fn paint_tile_grid(ui: &egui::Ui, rect: egui::Rect, tokens: &Tokens, tiles: &[Tile]) {
    let inner = rect.shrink2(egui::vec2(PAD, PAD));
    let mut x = inner.left();
    let mut y = inner.top();
    for tile in tiles {
        if x + TILE_W > inner.right() {
            x = inner.left();
            y += TILE_H + GAP;
        }
        if y + TILE_H > inner.bottom() {
            break; // overflow — out of room. acceptable for 0.7.0
        }
        let tile_rect = egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(TILE_W, TILE_H));
        paint_tile(ui, tile_rect, tokens, tile);
        x += TILE_W + GAP;
    }
}

fn paint_tile(ui: &egui::Ui, rect: egui::Rect, tokens: &Tokens, tile: &Tile) {
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, tokens.rounding, tokens.bg_panel);
    painter.rect_stroke(
        rect,
        egui::CornerRadius::same(tokens.rounding.round() as u8),
        egui::Stroke::new(1.0, tokens.stroke_soft),
        egui::epaint::StrokeKind::Inside,
    );

    // Title.
    painter.text(
        egui::pos2(rect.left() + 14.0, rect.top() + 14.0),
        egui::Align2::LEFT_TOP,
        tile.title,
        egui::FontId::proportional(11.0),
        tokens.text_dim,
    );
    // KPI.
    painter.text(
        egui::pos2(rect.left() + 14.0, rect.top() + 36.0),
        egui::Align2::LEFT_TOP,
        &tile.kpi,
        egui::FontId::monospace(22.0),
        tokens.text_primary,
    );
    // Footer / sub-label.
    painter.text(
        egui::pos2(rect.left() + 14.0, rect.bottom() - 14.0),
        egui::Align2::LEFT_BOTTOM,
        &tile.sub,
        egui::FontId::proportional(11.0),
        tokens.text_secondary,
    );

    // Optional sparkline. None today — published series will land
    // in M1.6+ once adapters start buffering history.
    if let Some(samples) = &tile.spark
        && samples.len() >= 2
    {
        paint_sparkline(&painter, rect, tokens, samples);
    }
}

fn paint_sparkline(painter: &egui::Painter, rect: egui::Rect, tokens: &Tokens, samples: &[f32]) {
    // Polyline along the bottom-right quadrant of the tile.
    let plot_rect = egui::Rect::from_min_max(
        egui::pos2(rect.right() - 90.0, rect.top() + 36.0),
        egui::pos2(rect.right() - 10.0, rect.top() + 86.0),
    );
    let (min, max) = samples
        .iter()
        .fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), s| {
            (lo.min(*s), hi.max(*s))
        });
    let span = (max - min).max(1e-3);
    let step = plot_rect.width() / (samples.len() as f32 - 1.0).max(1.0);
    for i in 1..samples.len() {
        let p0 = egui::pos2(
            plot_rect.left() + step * (i - 1) as f32,
            plot_rect.bottom() - (samples[i - 1] - min) / span * plot_rect.height(),
        );
        let p1 = egui::pos2(
            plot_rect.left() + step * i as f32,
            plot_rect.bottom() - (samples[i] - min) / span * plot_rect.height(),
        );
        painter.line_segment([p0, p1], egui::Stroke::new(1.0, tokens.accent));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live::Connection;

    #[test]
    fn renders_default_desktop_without_panic() {
        let ctx = egui::Context::default();
        let live = Live::spawn();
        let mut desk = Desktop::default();
        let snap = Snapshot {
            connection: Connection::Connected,
            ..Default::default()
        };
        let _ = ctx.run_ui(Default::default(), |ui| {
            egui::CentralPanel::default().show_inside(ui, |ui| {
                let rect = ui.max_rect();
                show(ui, rect, &mut desk, &live, &snap);
            });
        });
    }

    #[test]
    fn renders_with_status_present() {
        let ctx = egui::Context::default();
        let live = Live::spawn();
        let mut desk = Desktop::default();
        let snap = Snapshot {
            connection: Connection::Connected,
            status: Some(serde_json::json!({
                "state": "running",
                "uptime_secs": 1234,
                "process_count": 3,
                "service_count": 2,
            })),
            mesh_status: Some(serde_json::json!({
                "total_nodes": 4,
                "healthy_nodes": 3,
            })),
            ..Default::default()
        };
        let _ = ctx.run_ui(Default::default(), |ui| {
            egui::CentralPanel::default().show_inside(ui, |ui| {
                let rect = ui.max_rect();
                show(ui, rect, &mut desk, &live, &snap);
            });
        });
    }

    #[test]
    fn build_tiles_includes_core_three_when_status_present() {
        let snap = Snapshot {
            status: Some(serde_json::json!({"state":"running"})),
            ..Default::default()
        };
        let tiles = build_tiles(&snap);
        // Kernel + Mesh + Chain are always emitted; sensor tiles only
        // when their snapshot field is present.
        assert!(tiles.iter().any(|t| t.title == "Kernel"));
        assert!(tiles.iter().any(|t| t.title == "Mesh"));
        assert!(tiles.iter().any(|t| t.title == "Chain"));
        assert!(!tiles.iter().any(|t| t.title == "Mic"));
    }
}
