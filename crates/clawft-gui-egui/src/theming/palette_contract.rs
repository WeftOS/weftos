//! Contract tests pinning every `palette` / `ansi` entry to the exact
//! literal it replaced (design-drift token pass).

use super::{ansi, palette};
use eframe::egui::Color32;

/// Each palette entry must equal the literal it replaced.
#[test]
fn palette_matches_legacy_literals() {
    let rows: &[(&str, Color32, Color32)] = &[
        (
            "ROW_SELECTED_FILL",
            palette::ROW_SELECTED_FILL,
            Color32::from_rgb(40, 55, 75),
        ),
        (
            "ROW_IDLE_FILL",
            palette::ROW_IDLE_FILL,
            Color32::from_rgb(28, 28, 34),
        ),
        (
            "ROW_SELECTED_STROKE",
            palette::ROW_SELECTED_STROKE,
            Color32::from_rgb(90, 140, 200),
        ),
        (
            "ROW_IDLE_STROKE",
            palette::ROW_IDLE_STROKE,
            Color32::from_rgb(50, 50, 58),
        ),
        (
            "ROW_SELECTED_TEXT",
            palette::ROW_SELECTED_TEXT,
            Color32::from_rgb(220, 230, 245),
        ),
        (
            "ROW_IDLE_TEXT",
            palette::ROW_IDLE_TEXT,
            Color32::from_rgb(170, 175, 185),
        ),
        (
            "CHIP_ON_FILL",
            palette::CHIP_ON_FILL,
            Color32::from_rgb(55, 90, 130),
        ),
        (
            "CHIP_OFF_FILL",
            palette::CHIP_OFF_FILL,
            Color32::from_rgb(40, 42, 50),
        ),
        (
            "CHIP_ON_STROKE",
            palette::CHIP_ON_STROKE,
            Color32::from_rgb(120, 170, 220),
        ),
        (
            "CHIP_OFF_STROKE",
            palette::CHIP_OFF_STROKE,
            Color32::from_rgb(70, 72, 80),
        ),
        (
            "CHIP_ON_TEXT",
            palette::CHIP_ON_TEXT,
            Color32::from_rgb(230, 235, 245),
        ),
        (
            "CHIP_OFF_TEXT",
            palette::CHIP_OFF_TEXT,
            Color32::from_rgb(180, 185, 195),
        ),
        (
            "TEXT_HEADING_MUTED",
            palette::TEXT_HEADING_MUTED,
            Color32::from_rgb(180, 180, 190),
        ),
        (
            "TEXT_ACTION_BLUE",
            palette::TEXT_ACTION_BLUE,
            Color32::from_rgb(200, 220, 240),
        ),
        (
            "TEXT_LIVE_DOT",
            palette::TEXT_LIVE_DOT,
            Color32::from_rgb(100, 190, 230),
        ),
        (
            "TEXT_SELECTED_MARK",
            palette::TEXT_SELECTED_MARK,
            Color32::from_rgb(160, 190, 230),
        ),
        (
            "TEXT_META_FAINT",
            palette::TEXT_META_FAINT,
            Color32::from_rgb(130, 140, 150),
        ),
        (
            "TEXT_SNIPPET",
            palette::TEXT_SNIPPET,
            Color32::from_rgb(150, 155, 165),
        ),
        (
            "TEXT_SECTION_BLUE",
            palette::TEXT_SECTION_BLUE,
            Color32::from_rgb(140, 170, 210),
        ),
        (
            "TEXT_SUBTLE",
            palette::TEXT_SUBTLE,
            Color32::from_rgb(160, 160, 170),
        ),
        (
            "TEXT_SESSION_ID",
            palette::TEXT_SESSION_ID,
            Color32::from_rgb(130, 140, 155),
        ),
        (
            "TEXT_LABEL_FAINT",
            palette::TEXT_LABEL_FAINT,
            Color32::from_rgb(140, 140, 150),
        ),
        (
            "TEXT_CURSOR",
            palette::TEXT_CURSOR,
            Color32::from_rgb(120, 180, 200),
        ),
        (
            "TEXT_HINT_FAINT",
            palette::TEXT_HINT_FAINT,
            Color32::from_rgb(140, 150, 160),
        ),
        (
            "TEXT_WARN_MUTED",
            palette::TEXT_WARN_MUTED,
            Color32::from_rgb(180, 140, 120),
        ),
        (
            "TEXT_DISCLOSURE",
            palette::TEXT_DISCLOSURE,
            Color32::from_rgb(170, 170, 180),
        ),
        (
            "TEXT_STATUS",
            palette::TEXT_STATUS,
            Color32::from_rgb(180, 200, 220),
        ),
        (
            "TEXT_PROGRESS",
            palette::TEXT_PROGRESS,
            Color32::from_rgb(150, 160, 170),
        ),
        (
            "TEXT_SYSTEM_MSG",
            palette::TEXT_SYSTEM_MSG,
            Color32::from_rgb(150, 150, 160),
        ),
        (
            "DRIFT_BANNER_FILL",
            palette::DRIFT_BANNER_FILL,
            Color32::from_rgb(70, 50, 20),
        ),
        (
            "DRIFT_TITLE",
            palette::DRIFT_TITLE,
            Color32::from_rgb(240, 200, 140),
        ),
        (
            "DRIFT_SOURCE",
            palette::DRIFT_SOURCE,
            Color32::from_rgb(230, 220, 200),
        ),
        (
            "DRIFT_BODY",
            palette::DRIFT_BODY,
            Color32::from_rgb(220, 210, 190),
        ),
        (
            "BUBBLE_USER_FILL",
            palette::BUBBLE_USER_FILL,
            Color32::from_rgb(50, 60, 80),
        ),
        (
            "BUBBLE_USER_TEXT",
            palette::BUBBLE_USER_TEXT,
            Color32::from_rgb(225, 230, 240),
        ),
        (
            "BUBBLE_ASSISTANT_FILL",
            palette::BUBBLE_ASSISTANT_FILL,
            Color32::from_rgb(40, 50, 50),
        ),
        (
            "BUBBLE_ERROR_FILL",
            palette::BUBBLE_ERROR_FILL,
            Color32::from_rgb(70, 30, 30),
        ),
        (
            "BUBBLE_ERROR_TEXT",
            palette::BUBBLE_ERROR_TEXT,
            Color32::from_rgb(240, 170, 170),
        ),
        (
            "TERM_ERROR_TEXT",
            palette::TERM_ERROR_TEXT,
            Color32::from_rgb(220, 120, 120),
        ),
        (
            "TERM_EXIT_TEXT",
            palette::TERM_EXIT_TEXT,
            Color32::from_rgb(160, 120, 120),
        ),
        (
            "TERM_OK_HINT",
            palette::TERM_OK_HINT,
            Color32::from_rgb(140, 200, 160),
        ),
        (
            "TERM_SELECTION_OVERLAY",
            palette::TERM_SELECTION_OVERLAY,
            Color32::from_rgba_unmultiplied(120, 160, 220, 80),
        ),
        (
            "TERM_LABEL",
            palette::TERM_LABEL,
            Color32::from_rgb(150, 170, 200),
        ),
        (
            "GRAPH_EDIT_HINT",
            palette::GRAPH_EDIT_HINT,
            Color32::from_rgb(100, 110, 130),
        ),
        (
            "GRAPH_EDIT_STATUS_OK",
            palette::GRAPH_EDIT_STATUS_OK,
            Color32::from_rgb(180, 200, 120),
        ),
        (
            "GRAPH_EDIT_CANVAS_BG",
            palette::GRAPH_EDIT_CANVAS_BG,
            Color32::from_rgb(20, 20, 28),
        ),
        (
            "GRAPH_EDIT_EDGE",
            palette::GRAPH_EDIT_EDGE,
            Color32::from_rgb(110, 120, 140),
        ),
        (
            "GRAPH_EDIT_NODE_SELECTED",
            palette::GRAPH_EDIT_NODE_SELECTED,
            Color32::from_rgb(255, 210, 90),
        ),
        (
            "GRAPH_EDIT_NODE_CONNECT_SRC",
            palette::GRAPH_EDIT_NODE_CONNECT_SRC,
            Color32::from_rgb(120, 220, 140),
        ),
        (
            "GRAPH_EDIT_NODE_OUTLINE",
            palette::GRAPH_EDIT_NODE_OUTLINE,
            Color32::from_rgb(200, 200, 215),
        ),
        (
            "GRAPH_EDIT_NODE_LABEL",
            palette::GRAPH_EDIT_NODE_LABEL,
            Color32::from_rgb(235, 235, 240),
        ),
        (
            "GRAPH_EDIT_KIND_NONE",
            palette::GRAPH_EDIT_KIND_NONE,
            Color32::from_rgb(60, 70, 100),
        ),
        (
            "CG_SPECULATIVE_FILL",
            palette::CG_SPECULATIVE_FILL,
            Color32::from_rgb(180, 180, 200),
        ),
        (
            "CG_SPECULATIVE_OUTLINE",
            palette::CG_SPECULATIVE_OUTLINE,
            Color32::from_rgb(120, 120, 160),
        ),
        (
            "CG_FRONTIER_FILL",
            palette::CG_FRONTIER_FILL,
            Color32::from_rgb(80, 160, 255),
        ),
        (
            "CG_FRONTIER_OUTLINE",
            palette::CG_FRONTIER_OUTLINE,
            Color32::from_rgb(40, 100, 220),
        ),
        (
            "CG_COMMITTED_FILL",
            palette::CG_COMMITTED_FILL,
            Color32::from_rgb(90, 140, 110),
        ),
        (
            "CG_COMMITTED_OUTLINE",
            palette::CG_COMMITTED_OUTLINE,
            Color32::from_rgb(60, 90, 70),
        ),
        (
            "CG_STALE_FILL",
            palette::CG_STALE_FILL,
            Color32::from_rgb(140, 140, 140),
        ),
        (
            "CG_STALE_OUTLINE",
            palette::CG_STALE_OUTLINE,
            Color32::from_rgb(100, 100, 100),
        ),
        (
            "CG_PRUNED_FILL",
            palette::CG_PRUNED_FILL,
            Color32::from_rgb(60, 60, 60),
        ),
        (
            "CG_PRUNED_OUTLINE",
            palette::CG_PRUNED_OUTLINE,
            Color32::from_rgb(120, 80, 80),
        ),
        (
            "CG_EDGE_CONTRADICTS",
            palette::CG_EDGE_CONTRADICTS,
            Color32::from_rgb(220, 60, 60),
        ),
        (
            "CG_EDGE_SUPPORTS",
            palette::CG_EDGE_SUPPORTS,
            Color32::from_rgb(100, 160, 220),
        ),
        (
            "CG_EDGE_SPEAKER",
            palette::CG_EDGE_SPEAKER,
            Color32::from_rgb(140, 140, 160),
        ),
        (
            "CG_EDGE_DEFAULT",
            palette::CG_EDGE_DEFAULT,
            Color32::from_rgb(180, 180, 190),
        ),
        (
            "STATUS_OK",
            palette::STATUS_OK,
            Color32::from_rgb(110, 200, 150),
        ),
        (
            "STATUS_WARN",
            palette::STATUS_WARN,
            Color32::from_rgb(220, 180, 80),
        ),
        (
            "STATUS_ERR",
            palette::STATUS_ERR,
            Color32::from_rgb(200, 90, 90),
        ),
        (
            "STATUS_INFO",
            palette::STATUS_INFO,
            Color32::from_rgb(160, 180, 220),
        ),
        (
            "STATUS_NEUTRAL",
            palette::STATUS_NEUTRAL,
            Color32::from_rgb(150, 150, 160),
        ),
        (
            "TEXT_ERROR_SOFT",
            palette::TEXT_ERROR_SOFT,
            Color32::from_rgb(220, 120, 120),
        ),
        (
            "ACCENT_CYAN",
            palette::ACCENT_CYAN,
            Color32::from_rgb(110, 200, 240),
        ),
        (
            "ACCENT_SKY",
            palette::ACCENT_SKY,
            Color32::from_rgb(160, 200, 230),
        ),
        (
            "ACCENT_STEEL",
            palette::ACCENT_STEEL,
            Color32::from_rgb(140, 175, 220),
        ),
        (
            "ACCENT_STEEL_LIGHT",
            palette::ACCENT_STEEL_LIGHT,
            Color32::from_rgb(180, 200, 230),
        ),
        (
            "ACCENT_SAND",
            palette::ACCENT_SAND,
            Color32::from_rgb(200, 180, 120),
        ),
        (
            "ACCENT_SAND_DEEP",
            palette::ACCENT_SAND_DEEP,
            Color32::from_rgb(200, 170, 120),
        ),
        (
            "ACCENT_PEACH",
            palette::ACCENT_PEACH,
            Color32::from_rgb(220, 170, 120),
        ),
        (
            "ACCENT_AMBER",
            palette::ACCENT_AMBER,
            Color32::from_rgb(220, 170, 90),
        ),
        (
            "JSON_BOOL",
            palette::JSON_BOOL,
            Color32::from_rgb(220, 160, 100),
        ),
        (
            "JSON_STR",
            palette::JSON_STR,
            Color32::from_rgb(150, 210, 150),
        ),
        (
            "JSON_ARRAY",
            palette::JSON_ARRAY,
            Color32::from_rgb(200, 170, 220),
        ),
        (
            "JSON_OBJECT",
            palette::JSON_OBJECT,
            Color32::from_rgb(220, 190, 130),
        ),
        (
            "CHAIN_TS_TEXT",
            palette::CHAIN_TS_TEXT,
            Color32::from_rgb(170, 170, 200),
        ),
        (
            "CHAIN_SUMMARY_TEXT",
            palette::CHAIN_SUMMARY_TEXT,
            Color32::from_rgb(200, 200, 210),
        ),
        (
            "CHAIN_DETAIL_TEXT",
            palette::CHAIN_DETAIL_TEXT,
            Color32::from_rgb(200, 220, 200),
        ),
        (
            "PROC_PID_TEXT",
            palette::PROC_PID_TEXT,
            Color32::from_rgb(200, 200, 220),
        ),
        (
            "HEALTH_LIME",
            palette::HEALTH_LIME,
            Color32::from_rgb(170, 200, 110),
        ),
        (
            "TEXT_DIM",
            palette::TEXT_DIM,
            Color32::from_rgb(130, 130, 140),
        ),
        (
            "TEXT_DIMMER",
            palette::TEXT_DIMMER,
            Color32::from_rgb(120, 120, 130),
        ),
        ("PLOT_BG", palette::PLOT_BG, Color32::from_rgb(18, 18, 24)),
        ("METER_BG", palette::METER_BG, Color32::from_rgb(24, 24, 32)),
        (
            "PLOT_GRID",
            palette::PLOT_GRID,
            Color32::from_rgb(60, 60, 72),
        ),
        (
            "METER_TEXT",
            palette::METER_TEXT,
            Color32::from_rgb(230, 230, 235),
        ),
        (
            "DEPTH_MISSING",
            palette::DEPTH_MISSING,
            Color32::from_rgb(64, 64, 72),
        ),
        (
            "DEPTH_FAR",
            palette::DEPTH_FAR,
            Color32::from_rgb(220, 70, 60),
        ),
        (
            "SCOPE_LINE",
            palette::SCOPE_LINE,
            Color32::from_rgb(120, 220, 160),
        ),
        (
            "TREE_OFFLINE_DOT",
            palette::TREE_OFFLINE_DOT,
            Color32::from_rgb(90, 90, 100),
        ),
        (
            "SHELL_OK",
            palette::SHELL_OK,
            Color32::from_rgb(110, 210, 160),
        ),
        (
            "SHELL_CONNECTING",
            palette::SHELL_CONNECTING,
            Color32::from_rgb(240, 200, 110),
        ),
        (
            "SHELL_ERR_SOFT",
            palette::SHELL_ERR_SOFT,
            Color32::from_rgb(240, 150, 150),
        ),
        (
            "SHELL_ERR_TEXT",
            palette::SHELL_ERR_TEXT,
            Color32::from_rgb(220, 140, 140),
        ),
        (
            "SHELL_AMBER_A",
            palette::SHELL_AMBER_A,
            Color32::from_rgb(220, 160, 60),
        ),
        (
            "SHELL_AMBER_B",
            palette::SHELL_AMBER_B,
            Color32::from_rgb(220, 180, 60),
        ),
        (
            "SHELL_POPUP_FILL",
            palette::SHELL_POPUP_FILL,
            Color32::from_rgba_unmultiplied(18, 18, 24, 235),
        ),
        (
            "SHELL_POPUP_STROKE",
            palette::SHELL_POPUP_STROKE,
            Color32::from_rgba_unmultiplied(255, 255, 255, 25),
        ),
        (
            "TRAY_PANEL_FILL",
            palette::TRAY_PANEL_FILL,
            Color32::from_rgba_unmultiplied(14, 14, 20, 215),
        ),
        (
            "TRAY_HAIRLINE",
            palette::TRAY_HAIRLINE,
            Color32::from_rgba_unmultiplied(255, 255, 255, 20),
        ),
        (
            "TRAY_TEXT_DIM",
            palette::TRAY_TEXT_DIM,
            Color32::from_rgba_unmultiplied(200, 200, 220, 180),
        ),
        (
            "TRAY_TEXT",
            palette::TRAY_TEXT,
            Color32::from_rgba_unmultiplied(220, 220, 235, 200),
        ),
        (
            "TRAY_TEXT_BRIGHT",
            palette::TRAY_TEXT_BRIGHT,
            Color32::from_rgba_unmultiplied(230, 230, 240, 220),
        ),
        (
            "TRAY_CHIP_HOVER_FILL",
            palette::TRAY_CHIP_HOVER_FILL,
            Color32::from_rgba_unmultiplied(52, 52, 68, 220),
        ),
        (
            "TRAY_CHIP_FILL",
            palette::TRAY_CHIP_FILL,
            Color32::from_rgba_unmultiplied(28, 28, 38, 180),
        ),
        ("GRID_BG", palette::GRID_BG, Color32::from_rgb(4, 4, 6)),
        (
            "GRID_TINT_ROSE",
            palette::GRID_TINT_ROSE,
            Color32::from_rgba_unmultiplied(80, 30, 48, 22),
        ),
        (
            "GRID_TINT_SLATE",
            palette::GRID_TINT_SLATE,
            Color32::from_rgba_unmultiplied(55, 55, 72, 30),
        ),
        (
            "GRID_TINT_BLUE",
            palette::GRID_TINT_BLUE,
            Color32::from_rgba_unmultiplied(34, 56, 96, 22),
        ),
        (
            "GRID_VIGNETTE",
            palette::GRID_VIGNETTE,
            Color32::from_rgba_unmultiplied(0, 0, 0, 140),
        ),
        (
            "WORKSPACE_SCRIM",
            palette::WORKSPACE_SCRIM,
            Color32::from_rgba_unmultiplied(8, 8, 12, 180),
        ),
        (
            "CG_HIGHLIGHT",
            palette::CG_HIGHLIGHT,
            Color32::from_rgb(255, 200, 80),
        ),
        (
            "DEMO_BLOCK_A",
            palette::DEMO_BLOCK_A,
            Color32::from_rgb(60, 80, 120),
        ),
        (
            "DEMO_BLOCK_B",
            palette::DEMO_BLOCK_B,
            Color32::from_rgb(40, 60, 90),
        ),
        (
            "DEMO_BLOCK_C",
            palette::DEMO_BLOCK_C,
            Color32::from_rgb(80, 60, 120),
        ),
        (
            "DEMO_HAIRLINE",
            palette::DEMO_HAIRLINE,
            Color32::from_rgba_unmultiplied(255, 255, 255, 36),
        ),
        (
            "BLOCK_BAR_OK",
            palette::BLOCK_BAR_OK,
            Color32::from_rgb(60, 160, 90),
        ),
        (
            "BLOCK_BAR_WARN",
            palette::BLOCK_BAR_WARN,
            Color32::from_rgb(220, 160, 40),
        ),
        (
            "BLOCK_BAR_ERR",
            palette::BLOCK_BAR_ERR,
            Color32::from_rgb(220, 70, 70),
        ),
        (
            "BLOCK_TEXT_OK",
            palette::BLOCK_TEXT_OK,
            Color32::from_rgb(110, 210, 140),
        ),
        (
            "BLOCK_TEXT_WARN",
            palette::BLOCK_TEXT_WARN,
            Color32::from_rgb(255, 205, 90),
        ),
        (
            "BLOCK_TEXT_ERR",
            palette::BLOCK_TEXT_ERR,
            Color32::from_rgb(255, 140, 140),
        ),
        (
            "BLOCK_TERM_BG",
            palette::BLOCK_TERM_BG,
            Color32::from_rgb(8, 10, 14),
        ),
        (
            "BLOCK_TERM_INPUT",
            palette::BLOCK_TERM_INPUT,
            Color32::from_rgb(120, 200, 255),
        ),
        (
            "BLOCK_TERM_OUTPUT",
            palette::BLOCK_TERM_OUTPUT,
            Color32::from_rgb(210, 210, 210),
        ),
        (
            "BLOCK_TERM_ERROR",
            palette::BLOCK_TERM_ERROR,
            Color32::from_rgb(240, 120, 120),
        ),
    ];
    for (name, got, want) in rows {
        assert_eq!(got, want, "palette::{name} drifted from its legacy literal");
    }
}

#[test]
fn data_helpers_match_legacy_literals() {
    assert_eq!(
        palette::data_rgba(1, 2, 3, 4),
        Color32::from_rgba_unmultiplied(1, 2, 3, 4)
    );
    assert_eq!(
        palette::boot_halo(9),
        Color32::from_rgba_unmultiplied(80, 60, 20, 9)
    );
    assert_eq!(
        palette::boot_subtitle(9),
        Color32::from_rgba_unmultiplied(210, 195, 160, 9)
    );
    assert_eq!(
        palette::boot_tint(9),
        Color32::from_rgba_unmultiplied(255, 255, 255, 9)
    );
    assert_eq!(palette::heartbeat_dot(120), Color32::from_rgb(80, 120, 200));
    assert_eq!(palette::data_rgb(1, 2, 3), Color32::from_rgb(1, 2, 3));
}

#[test]
fn ansi_defaults_match_legacy_literals() {
    assert_eq!(ansi::DEFAULT_FG, Color32::from_rgb(220, 220, 225));
    assert_eq!(ansi::DEFAULT_BG, Color32::from_rgb(12, 12, 16));
    assert_eq!(ansi::BRIGHT_FG, Color32::from_rgb(245, 245, 250));
}

/// Round-trip: every base index maps to the legacy table entry, and
/// the cube / greyscale arithmetic is unchanged.
#[test]
fn ansi_palette_round_trips() {
    let legacy: [(u8, u8, u8); 16] = [
        (7, 54, 66),
        (220, 50, 47),
        (133, 153, 0),
        (181, 137, 0),
        (38, 139, 210),
        (211, 54, 130),
        (42, 161, 152),
        (238, 232, 213),
        (88, 110, 117),
        (255, 90, 87),
        (180, 200, 0),
        (220, 180, 30),
        (100, 180, 240),
        (240, 90, 170),
        (80, 200, 190),
        (253, 246, 227),
    ];
    for (i, (r, g, b)) in legacy.iter().enumerate() {
        assert_eq!(
            ansi::indexed(i as u8),
            Color32::from_rgb(*r, *g, *b),
            "ansi {i}"
        );
        assert_eq!(ansi::PALETTE_16[i], ansi::indexed(i as u8));
    }
    assert_eq!(ansi::indexed(16), Color32::from_rgb(0, 0, 0));
    assert_eq!(ansi::indexed(231), Color32::from_rgb(255, 255, 255));
    assert_eq!(ansi::indexed(196), Color32::from_rgb(255, 0, 0));
    assert_eq!(ansi::indexed(232), Color32::from_rgb(8, 8, 8));
    assert_eq!(ansi::indexed(255), Color32::from_rgb(238, 238, 238));
}
