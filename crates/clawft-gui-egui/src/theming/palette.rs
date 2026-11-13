//! Named colour palette for explorer / chat / graph surfaces.
//!
//! Every value here is the exact RGBA that used to be a raw
//! `Color32::from_rgb` literal at its call site (WEFT design-drift
//! token pass, 2026-11-11); rendered colours are unchanged. Each entry
//! is pinned by `palette_contract.rs`. Where a role
//! is later folded into a [`super::Tokens`] field, change it here once.

use eframe::egui::Color32;

/// Selected list-row / tab fill.
pub const ROW_SELECTED_FILL: Color32 = Color32::from_rgb(40, 55, 75);
/// Idle list-row / tab fill.
pub const ROW_IDLE_FILL: Color32 = Color32::from_rgb(28, 28, 34);
/// Selected list-row / tab outline.
pub const ROW_SELECTED_STROKE: Color32 = Color32::from_rgb(90, 140, 200);
/// Idle list-row / tab outline.
pub const ROW_IDLE_STROKE: Color32 = Color32::from_rgb(50, 50, 58);
/// Selected list-row / tab text.
pub const ROW_SELECTED_TEXT: Color32 = Color32::from_rgb(220, 230, 245);
/// Idle list-row / tab text.
pub const ROW_IDLE_TEXT: Color32 = Color32::from_rgb(170, 175, 185);
/// Toggle chip fill, on.
pub const CHIP_ON_FILL: Color32 = Color32::from_rgb(55, 90, 130);
/// Toggle chip fill, off.
pub const CHIP_OFF_FILL: Color32 = Color32::from_rgb(40, 42, 50);
/// Toggle chip outline, on.
pub const CHIP_ON_STROKE: Color32 = Color32::from_rgb(120, 170, 220);
/// Toggle chip outline, off.
pub const CHIP_OFF_STROKE: Color32 = Color32::from_rgb(70, 72, 80);
/// Toggle chip text, on.
pub const CHIP_ON_TEXT: Color32 = Color32::from_rgb(230, 235, 245);
/// Toggle chip text, off.
pub const CHIP_OFF_TEXT: Color32 = Color32::from_rgb(180, 185, 195);
/// Muted small section heading.
pub const TEXT_HEADING_MUTED: Color32 = Color32::from_rgb(180, 180, 190);
/// Primary small action label (cool tint).
pub const TEXT_ACTION_BLUE: Color32 = Color32::from_rgb(200, 220, 240);
/// Live-conversation marker dot.
pub const TEXT_LIVE_DOT: Color32 = Color32::from_rgb(100, 190, 230);
/// Selected-conversation marker.
pub const TEXT_SELECTED_MARK: Color32 = Color32::from_rgb(160, 190, 230);
/// Faint metadata (age, timestamps).
pub const TEXT_META_FAINT: Color32 = Color32::from_rgb(130, 140, 150);
/// Conversation snippet text.
pub const TEXT_SNIPPET: Color32 = Color32::from_rgb(150, 155, 165);
/// Cool-tinted section label.
pub const TEXT_SECTION_BLUE: Color32 = Color32::from_rgb(140, 170, 210);
/// Subtle body / title text.
pub const TEXT_SUBTLE: Color32 = Color32::from_rgb(160, 160, 170);
/// Monospace session-id label.
pub const TEXT_SESSION_ID: Color32 = Color32::from_rgb(130, 140, 155);
/// Faint field label.
pub const TEXT_LABEL_FAINT: Color32 = Color32::from_rgb(140, 140, 150);
/// Streaming cursor glyph.
pub const TEXT_CURSOR: Color32 = Color32::from_rgb(120, 180, 200);
/// Faint italic hint (loading, age).
pub const TEXT_HINT_FAINT: Color32 = Color32::from_rgb(140, 150, 160);
/// Muted warm warning text.
pub const TEXT_WARN_MUTED: Color32 = Color32::from_rgb(180, 140, 120);
/// Collapsing-header label.
pub const TEXT_DISCLOSURE: Color32 = Color32::from_rgb(170, 170, 180);
/// Status line text.
pub const TEXT_STATUS: Color32 = Color32::from_rgb(180, 200, 220);
/// Progress hint text.
pub const TEXT_PROGRESS: Color32 = Color32::from_rgb(150, 160, 170);
/// System-message text.
pub const TEXT_SYSTEM_MSG: Color32 = Color32::from_rgb(150, 150, 160);
/// Identity-drift banner fill.
pub const DRIFT_BANNER_FILL: Color32 = Color32::from_rgb(70, 50, 20);
/// Identity-drift banner title.
pub const DRIFT_TITLE: Color32 = Color32::from_rgb(240, 200, 140);
/// Identity-drift banner source line.
pub const DRIFT_SOURCE: Color32 = Color32::from_rgb(230, 220, 200);
/// Identity-drift banner body.
pub const DRIFT_BODY: Color32 = Color32::from_rgb(220, 210, 190);
/// User chat bubble fill.
pub const BUBBLE_USER_FILL: Color32 = Color32::from_rgb(50, 60, 80);
/// User chat bubble text.
pub const BUBBLE_USER_TEXT: Color32 = Color32::from_rgb(225, 230, 240);
/// Assistant chat bubble fill.
pub const BUBBLE_ASSISTANT_FILL: Color32 = Color32::from_rgb(40, 50, 50);
/// Error chat bubble fill.
pub const BUBBLE_ERROR_FILL: Color32 = Color32::from_rgb(70, 30, 30);
/// Error chat bubble text.
pub const BUBBLE_ERROR_TEXT: Color32 = Color32::from_rgb(240, 170, 170);
/// VT100 terminal error text.
pub const TERM_ERROR_TEXT: Color32 = Color32::from_rgb(220, 120, 120);
/// VT100 terminal exited-session text.
pub const TERM_EXIT_TEXT: Color32 = Color32::from_rgb(160, 120, 120);
/// VT100 terminal success hint.
pub const TERM_OK_HINT: Color32 = Color32::from_rgb(140, 200, 160);
/// VT100 terminal selection overlay (unmultiplied).
pub const TERM_SELECTION_OVERLAY: Color32 =
    Color32::from_rgba_unmultiplied_const(120, 160, 220, 80);
/// VT100 terminal tab-bar label.
pub const TERM_LABEL: Color32 = Color32::from_rgb(150, 170, 200);
/// Graph editor placeholder / hint text.
pub const GRAPH_EDIT_HINT: Color32 = Color32::from_rgb(100, 110, 130);
/// Graph editor positive status text.
pub const GRAPH_EDIT_STATUS_OK: Color32 = Color32::from_rgb(180, 200, 120);
/// Graph editor canvas background.
pub const GRAPH_EDIT_CANVAS_BG: Color32 = Color32::from_rgb(20, 20, 28);
/// Graph editor edge stroke.
pub const GRAPH_EDIT_EDGE: Color32 = Color32::from_rgb(110, 120, 140);
/// Graph editor selected-node outline.
pub const GRAPH_EDIT_NODE_SELECTED: Color32 = Color32::from_rgb(255, 210, 90);
/// Graph editor connect-source node outline.
pub const GRAPH_EDIT_NODE_CONNECT_SRC: Color32 = Color32::from_rgb(120, 220, 140);
/// Graph editor default node outline.
pub const GRAPH_EDIT_NODE_OUTLINE: Color32 = Color32::from_rgb(200, 200, 215);
/// Graph editor node label.
pub const GRAPH_EDIT_NODE_LABEL: Color32 = Color32::from_rgb(235, 235, 240);
/// Graph editor fill for untyped nodes.
pub const GRAPH_EDIT_KIND_NONE: Color32 = Color32::from_rgb(60, 70, 100);
/// Conversation graph: speculative node fill.
pub const CG_SPECULATIVE_FILL: Color32 = Color32::from_rgb(180, 180, 200);
/// Conversation graph: speculative node outline.
pub const CG_SPECULATIVE_OUTLINE: Color32 = Color32::from_rgb(120, 120, 160);
/// Conversation graph: frontier node fill.
pub const CG_FRONTIER_FILL: Color32 = Color32::from_rgb(80, 160, 255);
/// Conversation graph: frontier node outline.
pub const CG_FRONTIER_OUTLINE: Color32 = Color32::from_rgb(40, 100, 220);
/// Conversation graph: committed node fill.
pub const CG_COMMITTED_FILL: Color32 = Color32::from_rgb(90, 140, 110);
/// Conversation graph: committed node outline.
pub const CG_COMMITTED_OUTLINE: Color32 = Color32::from_rgb(60, 90, 70);
/// Conversation graph: stale node fill.
pub const CG_STALE_FILL: Color32 = Color32::from_rgb(140, 140, 140);
/// Conversation graph: stale node outline.
pub const CG_STALE_OUTLINE: Color32 = Color32::from_rgb(100, 100, 100);
/// Conversation graph: pruned node fill.
pub const CG_PRUNED_FILL: Color32 = Color32::from_rgb(60, 60, 60);
/// Conversation graph: pruned node outline.
pub const CG_PRUNED_OUTLINE: Color32 = Color32::from_rgb(120, 80, 80);
/// Conversation graph: contradicts edge.
pub const CG_EDGE_CONTRADICTS: Color32 = Color32::from_rgb(220, 60, 60);
/// Conversation graph: enables / evidence edge.
pub const CG_EDGE_SUPPORTS: Color32 = Color32::from_rgb(100, 160, 220);
/// Conversation graph: speaker-type edge.
pub const CG_EDGE_SPEAKER: Color32 = Color32::from_rgb(140, 140, 160);
/// Conversation graph: default causal edge.
pub const CG_EDGE_DEFAULT: Color32 = Color32::from_rgb(180, 180, 190);

// ── Explorer viewers, blocks and shell (D-TK01 second pass) ──────────

/// Status: healthy / ok / running (explorer viewers).
pub const STATUS_OK: Color32 = Color32::from_rgb(110, 200, 150);
/// Status: degraded / warning / paused (explorer viewers).
pub const STATUS_WARN: Color32 = Color32::from_rgb(220, 180, 80);
/// Status: failed / down / error (explorer viewers).
pub const STATUS_ERR: Color32 = Color32::from_rgb(200, 90, 90);
/// Status: informational / sleeping / total count.
pub const STATUS_INFO: Color32 = Color32::from_rgb(160, 180, 220);
/// Status: unknown / unclassified state label.
pub const STATUS_NEUTRAL: Color32 = Color32::from_rgb(150, 150, 160);
/// Soft error text outside the terminal.
pub const TEXT_ERROR_SOFT: Color32 = Color32::from_rgb(220, 120, 120);
/// Cyan accent: numeric values, series lines.
pub const ACCENT_CYAN: Color32 = Color32::from_rgb(110, 200, 240);
/// Sky accent: info-kind rows, series labels.
pub const ACCENT_SKY: Color32 = Color32::from_rgb(160, 200, 230);
/// Steel-blue accent: section / link text.
pub const ACCENT_STEEL: Color32 = Color32::from_rgb(140, 175, 220);
/// Light steel-blue accent (sensor).
pub const ACCENT_STEEL_LIGHT: Color32 = Color32::from_rgb(180, 200, 230);
/// Sand accent (lineage marker).
pub const ACCENT_SAND: Color32 = Color32::from_rgb(200, 180, 120);
/// Deeper sand accent (workshop headings).
pub const ACCENT_SAND_DEEP: Color32 = Color32::from_rgb(200, 170, 120);
/// Peach accent (workshop pending marker).
pub const ACCENT_PEACH: Color32 = Color32::from_rgb(220, 170, 120);
/// Amber accent (tree folder label).
pub const ACCENT_AMBER: Color32 = Color32::from_rgb(220, 170, 90);
/// JSON fallback viewer: boolean.
pub const JSON_BOOL: Color32 = Color32::from_rgb(220, 160, 100);
/// JSON fallback viewer: string.
pub const JSON_STR: Color32 = Color32::from_rgb(150, 210, 150);
/// JSON fallback viewer: array.
pub const JSON_ARRAY: Color32 = Color32::from_rgb(200, 170, 220);
/// JSON fallback viewer: object.
pub const JSON_OBJECT: Color32 = Color32::from_rgb(220, 190, 130);
/// Chain tail: timestamp text.
pub const CHAIN_TS_TEXT: Color32 = Color32::from_rgb(170, 170, 200);
/// Chain tail: summary text.
pub const CHAIN_SUMMARY_TEXT: Color32 = Color32::from_rgb(200, 200, 210);
/// Chain tail: detail text.
pub const CHAIN_DETAIL_TEXT: Color32 = Color32::from_rgb(200, 220, 200);
/// Process table: pid / name text.
pub const PROC_PID_TEXT: Color32 = Color32::from_rgb(200, 200, 220);
/// Health viewer: lime marker.
pub const HEALTH_LIME: Color32 = Color32::from_rgb(170, 200, 110);
/// Dim plot / axis text.
pub const TEXT_DIM: Color32 = Color32::from_rgb(130, 130, 140);
/// Dimmer plot axis text.
pub const TEXT_DIMMER: Color32 = Color32::from_rgb(120, 120, 130);
/// Plot / waveform canvas background.
pub const PLOT_BG: Color32 = Color32::from_rgb(18, 18, 24);
/// Meter / mesh-bar canvas background.
pub const METER_BG: Color32 = Color32::from_rgb(24, 24, 32);
/// Waveform grid line.
pub const PLOT_GRID: Color32 = Color32::from_rgb(60, 60, 72);
/// Audio meter value text.
pub const METER_TEXT: Color32 = Color32::from_rgb(230, 230, 235);
/// Depth map: pixel with no depth.
pub const DEPTH_MISSING: Color32 = Color32::from_rgb(64, 64, 72);
/// Depth map: out-of-range / far fallback.
pub const DEPTH_FAR: Color32 = Color32::from_rgb(220, 70, 60);
/// Oscilloscope trace line.
pub const SCOPE_LINE: Color32 = Color32::from_rgb(120, 220, 160);
/// Explorer tree: offline marker dot.
pub const TREE_OFFLINE_DOT: Color32 = Color32::from_rgb(90, 90, 100);
/// Shell: connected / on indicator.
pub const SHELL_OK: Color32 = Color32::from_rgb(110, 210, 160);
/// Shell: connecting indicator.
pub const SHELL_CONNECTING: Color32 = Color32::from_rgb(240, 200, 110);
/// Shell / block: disconnected / error text (soft).
pub const SHELL_ERR_SOFT: Color32 = Color32::from_rgb(240, 150, 150);
/// Shell: error body text.
pub const SHELL_ERR_TEXT: Color32 = Color32::from_rgb(220, 140, 140);
/// Shell: amber marker A (desktop).
pub const SHELL_AMBER_A: Color32 = Color32::from_rgb(220, 160, 60);
/// Shell: amber marker B (desktop).
pub const SHELL_AMBER_B: Color32 = Color32::from_rgb(220, 180, 60);
/// Shell: popup panel fill.
pub const SHELL_POPUP_FILL: Color32 = Color32::from_rgba_unmultiplied_const(18, 18, 24, 235);
/// Shell: popup panel hairline.
pub const SHELL_POPUP_STROKE: Color32 = Color32::from_rgba_unmultiplied_const(255, 255, 255, 25);
/// Tray: bar fill.
pub const TRAY_PANEL_FILL: Color32 = Color32::from_rgba_unmultiplied_const(14, 14, 20, 215);
/// Tray: hairline.
pub const TRAY_HAIRLINE: Color32 = Color32::from_rgba_unmultiplied_const(255, 255, 255, 20);
/// Tray: dim text.
pub const TRAY_TEXT_DIM: Color32 = Color32::from_rgba_unmultiplied_const(200, 200, 220, 180);
/// Tray: text.
pub const TRAY_TEXT: Color32 = Color32::from_rgba_unmultiplied_const(220, 220, 235, 200);
/// Tray: bright text.
pub const TRAY_TEXT_BRIGHT: Color32 = Color32::from_rgba_unmultiplied_const(230, 230, 240, 220);
/// Tray: chip hover fill.
pub const TRAY_CHIP_HOVER_FILL: Color32 = Color32::from_rgba_unmultiplied_const(52, 52, 68, 220);
/// Tray: chip fill.
pub const TRAY_CHIP_FILL: Color32 = Color32::from_rgba_unmultiplied_const(28, 28, 38, 180);
/// Shell grid: backdrop.
pub const GRID_BG: Color32 = Color32::from_rgb(4, 4, 6);
/// Shell grid: rose glow tint.
pub const GRID_TINT_ROSE: Color32 = Color32::from_rgba_unmultiplied_const(80, 30, 48, 22);
/// Shell grid: slate glow tint.
pub const GRID_TINT_SLATE: Color32 = Color32::from_rgba_unmultiplied_const(55, 55, 72, 30);
/// Shell grid: blue glow tint.
pub const GRID_TINT_BLUE: Color32 = Color32::from_rgba_unmultiplied_const(34, 56, 96, 22);
/// Shell grid: edge vignette.
pub const GRID_VIGNETTE: Color32 = Color32::from_rgba_unmultiplied_const(0, 0, 0, 140);
/// Workspace: backdrop scrim.
pub const WORKSPACE_SCRIM: Color32 = Color32::from_rgba_unmultiplied_const(8, 8, 12, 180);
/// Conversation graph: highlight ring.
pub const CG_HIGHLIGHT: Color32 = Color32::from_rgb(255, 200, 80);
/// Canon demo: placeholder block A.
pub const DEMO_BLOCK_A: Color32 = Color32::from_rgb(60, 80, 120);
/// Canon demo: placeholder block B.
pub const DEMO_BLOCK_B: Color32 = Color32::from_rgb(40, 60, 90);
/// Canon demo: placeholder block C.
pub const DEMO_BLOCK_C: Color32 = Color32::from_rgb(80, 60, 120);
/// Demo lab: swatch outline.
pub const DEMO_HAIRLINE: Color32 = Color32::from_rgba_unmultiplied_const(255, 255, 255, 36);
/// Block: bar ok.
pub const BLOCK_BAR_OK: Color32 = Color32::from_rgb(60, 160, 90);
/// Block: bar warn.
pub const BLOCK_BAR_WARN: Color32 = Color32::from_rgb(220, 160, 40);
/// Block: bar error.
pub const BLOCK_BAR_ERR: Color32 = Color32::from_rgb(220, 70, 70);
/// Block: text ok.
pub const BLOCK_TEXT_OK: Color32 = Color32::from_rgb(110, 210, 140);
/// Block: text warn.
pub const BLOCK_TEXT_WARN: Color32 = Color32::from_rgb(255, 205, 90);
/// Block: text error.
pub const BLOCK_TEXT_ERR: Color32 = Color32::from_rgb(255, 140, 140);
/// Terminal block: background.
pub const BLOCK_TERM_BG: Color32 = Color32::from_rgb(8, 10, 14);
/// Terminal block: input line.
pub const BLOCK_TERM_INPUT: Color32 = Color32::from_rgb(120, 200, 255);
/// Terminal block: output line.
pub const BLOCK_TERM_OUTPUT: Color32 = Color32::from_rgb(210, 210, 210);
/// Terminal block: error line.
pub const BLOCK_TERM_ERROR: Color32 = Color32::from_rgb(240, 120, 120);

/// Colour derived from data (a hash, a truecolor escape) rather than
/// chosen by a designer. Not a palette entry; kept here so callers
/// never spell a raw `Color32` constructor.
pub const fn data_rgb(r: u8, g: u8, b: u8) -> Color32 {
    Color32::from_rgb(r, g, b)
}

/// Chat heartbeat dot; the green channel pulses with `pulse`.
pub const fn heartbeat_dot(pulse: u8) -> Color32 {
    Color32::from_rgb(80, pulse, 200)
}

/// Data-derived colour with alpha (a hue from a hash, a node colour
/// dimmed to a fixed alpha). Same contract as [`data_rgb`].
pub const fn data_rgba(r: u8, g: u8, b: u8, a: u8) -> Color32 {
    Color32::from_rgba_unmultiplied_const(r, g, b, a)
}

/// Boot splash halo; `alpha` is animated by the caller.
pub const fn boot_halo(alpha: u8) -> Color32 {
    Color32::from_rgba_unmultiplied_const(80, 60, 20, alpha)
}

/// Boot splash subtitle; `alpha` is animated by the caller.
pub const fn boot_subtitle(alpha: u8) -> Color32 {
    Color32::from_rgba_unmultiplied_const(210, 195, 160, alpha)
}

/// Boot splash logo tint (white); `alpha` is animated by the caller.
pub const fn boot_tint(alpha: u8) -> Color32 {
    Color32::from_rgba_unmultiplied_const(255, 255, 255, alpha)
}
