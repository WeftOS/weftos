//! WeftOS theming for egui — the base-layer style token set.
//!
//! Not a brand color system (the foundation doc rejects color-as-identity
//! at the base). Colors here are neutral + a single warm accent sampled
//! from the logo, used *only* to mark active/focused/selected state.
//! Everything else is dark-on-dark with thin strokes and crisp rounding —
//! the "good Linux system, no fluff" direction from the user brief.

use eframe::egui;

/// Apply the WeftOS egui style to the given context. Idempotent — call
/// once during app creation.
pub fn apply(ctx: &egui::Context) {
    let tokens = Tokens::default();
    ctx.set_visuals(tokens.visuals());
    ctx.set_global_style(tokens.style());
}

/// Neutral design tokens. No brand color at the base; `accent` is the
/// single warm gold used to signal active/focused state only.
pub struct Tokens {
    // Surfaces
    pub bg_app: egui::Color32,     // app root
    pub bg_panel: egui::Color32,   // side panels, dock, tray
    pub bg_sidebar: egui::Color32, // desktop sidebar (lifted neutral charcoal — DESIGN.md §2.1)
    pub bg_surface: egui::Color32, // cards / windows / inset frames
    pub bg_hover: egui::Color32,   // hover state
    pub bg_active: egui::Color32,  // pressed / selected
    // Strokes
    pub stroke_hair: egui::Color32, // 1px separators
    pub stroke_soft: egui::Color32, // 1px panel edges
    // Text
    pub text_primary: egui::Color32,
    pub text_secondary: egui::Color32,
    pub text_dim: egui::Color32,
    // Accent (single, used sparingly)
    pub accent: egui::Color32,
    pub accent_dim: egui::Color32,
    // State
    pub ok: egui::Color32,
    pub warn: egui::Color32,
    pub crit: egui::Color32,
    // Shape
    pub rounding: f32,
    pub rounding_tight: f32,
    pub spacing: egui::Vec2,
    pub button_padding: egui::Vec2,
}

impl Default for Tokens {
    fn default() -> Self {
        Self {
            bg_app: egui::Color32::from_rgb(8, 8, 10),
            bg_panel: egui::Color32::from_rgb(14, 14, 18),
            // Lifted neutral charcoal — pure desaturated grey, no hue cast.
            // Phase 1 of the 0.8.0 desktop wave (DESIGN.md §2.1, §5).
            bg_sidebar: egui::Color32::from_rgb(0x2A, 0x2A, 0x30),
            bg_surface: egui::Color32::from_rgb(22, 22, 28),
            bg_hover: egui::Color32::from_rgb(32, 32, 40),
            bg_active: egui::Color32::from_rgb(44, 44, 54),
            stroke_hair: egui::Color32::from_rgba_unmultiplied(255, 255, 255, 14),
            stroke_soft: egui::Color32::from_rgba_unmultiplied(255, 255, 255, 24),
            text_primary: egui::Color32::from_rgb(224, 222, 232),
            text_secondary: egui::Color32::from_rgb(170, 168, 180),
            text_dim: egui::Color32::from_rgb(112, 110, 122),
            // Warm gold, sampled to echo the logo. Kept muted.
            accent: egui::Color32::from_rgb(196, 162, 92),
            accent_dim: egui::Color32::from_rgba_unmultiplied(196, 162, 92, 80),
            ok: egui::Color32::from_rgb(110, 200, 150),
            warn: egui::Color32::from_rgb(220, 175, 85),
            crit: egui::Color32::from_rgb(220, 95, 95),
            rounding: 4.0,
            rounding_tight: 3.0,
            spacing: egui::vec2(6.0, 6.0),
            button_padding: egui::vec2(10.0, 5.0),
        }
    }
}

impl Tokens {
    fn visuals(&self) -> egui::Visuals {
        let mut v = egui::Visuals::dark();

        v.window_fill = self.bg_surface;
        v.panel_fill = self.bg_panel;
        v.extreme_bg_color = self.bg_app;
        v.faint_bg_color = self.bg_app;
        v.code_bg_color = ansi::DEFAULT_BG;

        v.window_stroke = egui::Stroke::new(1.0, self.stroke_soft);
        // egui 0.34 renamed `*_rounding` → `*_corner_radius` and switched
        // CornerRadius from f32 to u8.
        v.window_corner_radius = egui::CornerRadius::same((self.rounding * 2.0).round() as u8);
        v.menu_corner_radius = egui::CornerRadius::same(self.rounding.round() as u8);
        v.popup_shadow = egui::Shadow {
            offset: [0, 4],
            blur: 16,
            spread: 0,
            color: egui::Color32::from_rgba_unmultiplied(0, 0, 0, 160),
        };
        v.window_shadow = v.popup_shadow;

        v.override_text_color = Some(self.text_primary);
        v.hyperlink_color = self.accent;
        v.warn_fg_color = self.warn;
        v.error_fg_color = self.crit;

        v.selection.bg_fill = self.accent_dim;
        v.selection.stroke = egui::Stroke::new(1.0, self.accent);

        // Widget state tints
        let wv = &mut v.widgets;
        wv.noninteractive.bg_fill = self.bg_panel;
        wv.noninteractive.weak_bg_fill = self.bg_panel;
        wv.noninteractive.bg_stroke = egui::Stroke::new(1.0, self.stroke_hair);
        wv.noninteractive.fg_stroke = egui::Stroke::new(1.0, self.text_secondary);
        wv.noninteractive.corner_radius = egui::CornerRadius::same(self.rounding.round() as u8);

        wv.inactive.bg_fill = self.bg_surface;
        wv.inactive.weak_bg_fill = self.bg_panel;
        wv.inactive.bg_stroke = egui::Stroke::new(1.0, self.stroke_hair);
        wv.inactive.fg_stroke = egui::Stroke::new(1.0, self.text_primary);
        wv.inactive.corner_radius = egui::CornerRadius::same(self.rounding.round() as u8);

        wv.hovered.bg_fill = self.bg_hover;
        wv.hovered.weak_bg_fill = self.bg_hover;
        wv.hovered.bg_stroke = egui::Stroke::new(1.0, self.stroke_soft);
        wv.hovered.fg_stroke = egui::Stroke::new(1.0, self.text_primary);
        wv.hovered.corner_radius = egui::CornerRadius::same(self.rounding.round() as u8);

        wv.active.bg_fill = self.bg_active;
        wv.active.weak_bg_fill = self.bg_active;
        wv.active.bg_stroke = egui::Stroke::new(1.0, self.accent);
        wv.active.fg_stroke = egui::Stroke::new(1.0, self.accent);
        wv.active.corner_radius = egui::CornerRadius::same(self.rounding.round() as u8);

        wv.open.bg_fill = self.bg_surface;
        wv.open.weak_bg_fill = self.bg_surface;
        wv.open.bg_stroke = egui::Stroke::new(1.0, self.stroke_soft);
        wv.open.fg_stroke = egui::Stroke::new(1.0, self.text_primary);
        wv.open.corner_radius = egui::CornerRadius::same(self.rounding.round() as u8);

        v
    }

    fn style(&self) -> egui::Style {
        let mut s = egui::Style::default();
        s.spacing.item_spacing = self.spacing;
        s.spacing.button_padding = self.button_padding;
        s.spacing.menu_margin = egui::Margin::symmetric(6, 4);
        s.spacing.indent = 14.0;
        s.spacing.interact_size.y = 22.0;
        s.spacing.icon_width = 14.0;
        s.spacing.icon_width_inner = 8.0;
        s.spacing.icon_spacing = 4.0;
        s.spacing.scroll.bar_width = 8.0;
        s.spacing.tooltip_width = 360.0;

        // Slight reduction in heading weight to keep the shell calm.
        if let Some(h) = s.text_styles.get_mut(&egui::TextStyle::Heading) {
            *h = egui::FontId::new(18.0, egui::FontFamily::Proportional);
        }
        if let Some(b) = s.text_styles.get_mut(&egui::TextStyle::Body) {
            *b = egui::FontId::new(13.0, egui::FontFamily::Proportional);
        }
        if let Some(sm) = s.text_styles.get_mut(&egui::TextStyle::Small) {
            *sm = egui::FontId::new(11.0, egui::FontFamily::Proportional);
        }
        if let Some(m) = s.text_styles.get_mut(&egui::TextStyle::Monospace) {
            *m = egui::FontId::new(12.5, egui::FontFamily::Monospace);
        }
        if let Some(bt) = s.text_styles.get_mut(&egui::TextStyle::Button) {
            *bt = egui::FontId::new(13.0, egui::FontFamily::Proportional);
        }

        s
    }
}

/// Named colour palette for explorer / chat / graph surfaces.
///
/// Every value here is the exact RGBA that used to be a raw
/// `Color32::from_rgb` literal at its call site (WEFT design-drift
/// token pass, 2026-11-11); rendered colours are unchanged. Each entry
/// is pinned by `palette_matches_legacy_literals` below. Where a role
/// is later folded into a [`Tokens`] field, change it here once.
pub mod palette {
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
    pub const TERM_SELECTION_OVERLAY: Color32 = Color32::from_rgba_unmultiplied_const(120, 160, 220, 80);
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
}

/// ANSI / xterm-256 terminal palette. Terminal colours are data
/// (Solarized-dark-ish, matching the native terminal), not UI chrome.
pub mod ansi {
    use eframe::egui::Color32;

    /// Default terminal foreground.
    pub const DEFAULT_FG: Color32 = Color32::from_rgb(220, 220, 225);
    /// Default terminal background (also the code-block background).
    pub const DEFAULT_BG: Color32 = Color32::from_rgb(12, 12, 16);

    /// The 16 base ANSI colours, indices 0..=15.
    pub const PALETTE_16: [Color32; 16] = [
        Color32::from_rgb(7, 54, 66),
        Color32::from_rgb(220, 50, 47),
        Color32::from_rgb(133, 153, 0),
        Color32::from_rgb(181, 137, 0),
        Color32::from_rgb(38, 139, 210),
        Color32::from_rgb(211, 54, 130),
        Color32::from_rgb(42, 161, 152),
        Color32::from_rgb(238, 232, 213),
        Color32::from_rgb(88, 110, 117),
        Color32::from_rgb(255, 90, 87),
        Color32::from_rgb(180, 200, 0),
        Color32::from_rgb(220, 180, 30),
        Color32::from_rgb(100, 180, 240),
        Color32::from_rgb(240, 90, 170),
        Color32::from_rgb(80, 200, 190),
        Color32::from_rgb(253, 246, 227),
    ];

    /// xterm-256 lookup: 0..16 base palette, 16..232 6x6x6 cube,
    /// 232..256 greyscale ramp.
    pub fn indexed(i: u8) -> Color32 {
        if i < 16 {
            return PALETTE_16[usize::from(i)];
        }
        if i < 232 {
            let i = i - 16;
            let conv = |v: u8| -> u8 {
                if v == 0 {
                    0
                } else {
                    55 + v * 40
                }
            };
            return super::palette::data_rgb(conv(i / 36), conv((i / 6) % 6), conv(i % 6));
        }
        let v = 8 + (i - 232) * 10;
        super::palette::data_rgb(v, v, v)
    }
}

// ── Token contract test ─────────────────────────────────────────────
//
// Every value below mirrors `docs/DESIGN.md` §2.1 + §2.3 and
// `.claude/skills/weftos-design/references/tokens.md`. If you change a
// token in `Tokens::default`, update this test, DESIGN.md, AND
// `tokens.md` in the same commit. A diff in only one of the three is a
// drift bug that the design audit (§10) is designed to catch.
//
// This is a Phase 1 (0.8.0) gate — see
// `docs/plans/desktop-implementation-0.8.0.md`.

#[cfg(test)]
mod token_contract {
    use super::Tokens;
    use eframe::egui::Color32;

    #[track_caller]
    fn assert_token(actual: Color32, expected: Color32, name: &str) {
        assert_eq!(
            actual, expected,
            "token `{name}` drifted from DESIGN.md §2.1 / tokens.md"
        );
    }

    #[test]
    fn palette_matches_design_md() {
        let t = Tokens::default();
        // Surfaces (DESIGN.md §2.1 palette table)
        assert_token(t.bg_app, Color32::from_rgb(0x08, 0x08, 0x0A), "bg_app");
        assert_token(t.bg_panel, Color32::from_rgb(0x0E, 0x0E, 0x12), "bg_panel");
        assert_token(
            t.bg_sidebar,
            Color32::from_rgb(0x2A, 0x2A, 0x30),
            "bg_sidebar",
        );
        assert_token(
            t.bg_surface,
            Color32::from_rgb(0x16, 0x16, 0x1C),
            "bg_surface",
        );
        assert_token(t.bg_hover, Color32::from_rgb(0x20, 0x20, 0x28), "bg_hover");
        assert_token(
            t.bg_active,
            Color32::from_rgb(0x2C, 0x2C, 0x36),
            "bg_active",
        );
        // Strokes (white @ low alpha; egui stores premultiplied)
        assert_token(
            t.stroke_hair,
            Color32::from_rgba_unmultiplied(255, 255, 255, 14),
            "stroke_hair",
        );
        assert_token(
            t.stroke_soft,
            Color32::from_rgba_unmultiplied(255, 255, 255, 24),
            "stroke_soft",
        );
        // Text
        assert_token(
            t.text_primary,
            Color32::from_rgb(0xE0, 0xDE, 0xE8),
            "text_primary",
        );
        assert_token(
            t.text_secondary,
            Color32::from_rgb(0xAA, 0xA8, 0xB4),
            "text_secondary",
        );
        assert_token(t.text_dim, Color32::from_rgb(0x70, 0x6E, 0x7A), "text_dim");
        // Accent
        assert_token(t.accent, Color32::from_rgb(0xC4, 0xA2, 0x5C), "accent");
        assert_token(
            t.accent_dim,
            Color32::from_rgba_unmultiplied(0xC4, 0xA2, 0x5C, 80),
            "accent_dim",
        );
        // State
        assert_token(t.ok, Color32::from_rgb(0x6E, 0xC8, 0x96), "ok");
        assert_token(t.warn, Color32::from_rgb(0xDC, 0xAF, 0x55), "warn");
        assert_token(t.crit, Color32::from_rgb(0xDC, 0x5F, 0x5F), "crit");
    }

    #[test]
    fn shape_tokens_match_design_md() {
        let t = Tokens::default();
        // DESIGN.md §2.3 — Spacing & radius
        assert_eq!(t.rounding, 4.0, "rounding");
        assert_eq!(t.rounding_tight, 3.0, "rounding_tight");
        assert_eq!(t.spacing.x, 6.0, "spacing.x");
        assert_eq!(t.spacing.y, 6.0, "spacing.y");
        assert_eq!(t.button_padding.x, 10.0, "button_padding.x");
        assert_eq!(t.button_padding.y, 5.0, "button_padding.y");
    }
}

#[cfg(test)]
mod palette_contract {
    use super::{ansi, palette};
    use eframe::egui::Color32;

    /// Each palette entry must equal the literal it replaced.
    #[test]
    fn palette_matches_legacy_literals() {
        let rows: &[(&str, Color32, Color32)] = &[
            ("ROW_SELECTED_FILL", palette::ROW_SELECTED_FILL, Color32::from_rgb(40, 55, 75)),
            ("ROW_IDLE_FILL", palette::ROW_IDLE_FILL, Color32::from_rgb(28, 28, 34)),
            ("ROW_SELECTED_STROKE", palette::ROW_SELECTED_STROKE, Color32::from_rgb(90, 140, 200)),
            ("ROW_IDLE_STROKE", palette::ROW_IDLE_STROKE, Color32::from_rgb(50, 50, 58)),
            ("ROW_SELECTED_TEXT", palette::ROW_SELECTED_TEXT, Color32::from_rgb(220, 230, 245)),
            ("ROW_IDLE_TEXT", palette::ROW_IDLE_TEXT, Color32::from_rgb(170, 175, 185)),
            ("CHIP_ON_FILL", palette::CHIP_ON_FILL, Color32::from_rgb(55, 90, 130)),
            ("CHIP_OFF_FILL", palette::CHIP_OFF_FILL, Color32::from_rgb(40, 42, 50)),
            ("CHIP_ON_STROKE", palette::CHIP_ON_STROKE, Color32::from_rgb(120, 170, 220)),
            ("CHIP_OFF_STROKE", palette::CHIP_OFF_STROKE, Color32::from_rgb(70, 72, 80)),
            ("CHIP_ON_TEXT", palette::CHIP_ON_TEXT, Color32::from_rgb(230, 235, 245)),
            ("CHIP_OFF_TEXT", palette::CHIP_OFF_TEXT, Color32::from_rgb(180, 185, 195)),
            ("TEXT_HEADING_MUTED", palette::TEXT_HEADING_MUTED, Color32::from_rgb(180, 180, 190)),
            ("TEXT_ACTION_BLUE", palette::TEXT_ACTION_BLUE, Color32::from_rgb(200, 220, 240)),
            ("TEXT_LIVE_DOT", palette::TEXT_LIVE_DOT, Color32::from_rgb(100, 190, 230)),
            ("TEXT_SELECTED_MARK", palette::TEXT_SELECTED_MARK, Color32::from_rgb(160, 190, 230)),
            ("TEXT_META_FAINT", palette::TEXT_META_FAINT, Color32::from_rgb(130, 140, 150)),
            ("TEXT_SNIPPET", palette::TEXT_SNIPPET, Color32::from_rgb(150, 155, 165)),
            ("TEXT_SECTION_BLUE", palette::TEXT_SECTION_BLUE, Color32::from_rgb(140, 170, 210)),
            ("TEXT_SUBTLE", palette::TEXT_SUBTLE, Color32::from_rgb(160, 160, 170)),
            ("TEXT_SESSION_ID", palette::TEXT_SESSION_ID, Color32::from_rgb(130, 140, 155)),
            ("TEXT_LABEL_FAINT", palette::TEXT_LABEL_FAINT, Color32::from_rgb(140, 140, 150)),
            ("TEXT_CURSOR", palette::TEXT_CURSOR, Color32::from_rgb(120, 180, 200)),
            ("TEXT_HINT_FAINT", palette::TEXT_HINT_FAINT, Color32::from_rgb(140, 150, 160)),
            ("TEXT_WARN_MUTED", palette::TEXT_WARN_MUTED, Color32::from_rgb(180, 140, 120)),
            ("TEXT_DISCLOSURE", palette::TEXT_DISCLOSURE, Color32::from_rgb(170, 170, 180)),
            ("TEXT_STATUS", palette::TEXT_STATUS, Color32::from_rgb(180, 200, 220)),
            ("TEXT_PROGRESS", palette::TEXT_PROGRESS, Color32::from_rgb(150, 160, 170)),
            ("TEXT_SYSTEM_MSG", palette::TEXT_SYSTEM_MSG, Color32::from_rgb(150, 150, 160)),
            ("DRIFT_BANNER_FILL", palette::DRIFT_BANNER_FILL, Color32::from_rgb(70, 50, 20)),
            ("DRIFT_TITLE", palette::DRIFT_TITLE, Color32::from_rgb(240, 200, 140)),
            ("DRIFT_SOURCE", palette::DRIFT_SOURCE, Color32::from_rgb(230, 220, 200)),
            ("DRIFT_BODY", palette::DRIFT_BODY, Color32::from_rgb(220, 210, 190)),
            ("BUBBLE_USER_FILL", palette::BUBBLE_USER_FILL, Color32::from_rgb(50, 60, 80)),
            ("BUBBLE_USER_TEXT", palette::BUBBLE_USER_TEXT, Color32::from_rgb(225, 230, 240)),
            ("BUBBLE_ASSISTANT_FILL", palette::BUBBLE_ASSISTANT_FILL, Color32::from_rgb(40, 50, 50)),
            ("BUBBLE_ERROR_FILL", palette::BUBBLE_ERROR_FILL, Color32::from_rgb(70, 30, 30)),
            ("BUBBLE_ERROR_TEXT", palette::BUBBLE_ERROR_TEXT, Color32::from_rgb(240, 170, 170)),
            ("TERM_ERROR_TEXT", palette::TERM_ERROR_TEXT, Color32::from_rgb(220, 120, 120)),
            ("TERM_EXIT_TEXT", palette::TERM_EXIT_TEXT, Color32::from_rgb(160, 120, 120)),
            ("TERM_OK_HINT", palette::TERM_OK_HINT, Color32::from_rgb(140, 200, 160)),
            ("TERM_SELECTION_OVERLAY", palette::TERM_SELECTION_OVERLAY, Color32::from_rgba_unmultiplied(120, 160, 220, 80)),
            ("TERM_LABEL", palette::TERM_LABEL, Color32::from_rgb(150, 170, 200)),
            ("GRAPH_EDIT_HINT", palette::GRAPH_EDIT_HINT, Color32::from_rgb(100, 110, 130)),
            ("GRAPH_EDIT_STATUS_OK", palette::GRAPH_EDIT_STATUS_OK, Color32::from_rgb(180, 200, 120)),
            ("GRAPH_EDIT_CANVAS_BG", palette::GRAPH_EDIT_CANVAS_BG, Color32::from_rgb(20, 20, 28)),
            ("GRAPH_EDIT_EDGE", palette::GRAPH_EDIT_EDGE, Color32::from_rgb(110, 120, 140)),
            ("GRAPH_EDIT_NODE_SELECTED", palette::GRAPH_EDIT_NODE_SELECTED, Color32::from_rgb(255, 210, 90)),
            ("GRAPH_EDIT_NODE_CONNECT_SRC", palette::GRAPH_EDIT_NODE_CONNECT_SRC, Color32::from_rgb(120, 220, 140)),
            ("GRAPH_EDIT_NODE_OUTLINE", palette::GRAPH_EDIT_NODE_OUTLINE, Color32::from_rgb(200, 200, 215)),
            ("GRAPH_EDIT_NODE_LABEL", palette::GRAPH_EDIT_NODE_LABEL, Color32::from_rgb(235, 235, 240)),
            ("GRAPH_EDIT_KIND_NONE", palette::GRAPH_EDIT_KIND_NONE, Color32::from_rgb(60, 70, 100)),
            ("CG_SPECULATIVE_FILL", palette::CG_SPECULATIVE_FILL, Color32::from_rgb(180, 180, 200)),
            ("CG_SPECULATIVE_OUTLINE", palette::CG_SPECULATIVE_OUTLINE, Color32::from_rgb(120, 120, 160)),
            ("CG_FRONTIER_FILL", palette::CG_FRONTIER_FILL, Color32::from_rgb(80, 160, 255)),
            ("CG_FRONTIER_OUTLINE", palette::CG_FRONTIER_OUTLINE, Color32::from_rgb(40, 100, 220)),
            ("CG_COMMITTED_FILL", palette::CG_COMMITTED_FILL, Color32::from_rgb(90, 140, 110)),
            ("CG_COMMITTED_OUTLINE", palette::CG_COMMITTED_OUTLINE, Color32::from_rgb(60, 90, 70)),
            ("CG_STALE_FILL", palette::CG_STALE_FILL, Color32::from_rgb(140, 140, 140)),
            ("CG_STALE_OUTLINE", palette::CG_STALE_OUTLINE, Color32::from_rgb(100, 100, 100)),
            ("CG_PRUNED_FILL", palette::CG_PRUNED_FILL, Color32::from_rgb(60, 60, 60)),
            ("CG_PRUNED_OUTLINE", palette::CG_PRUNED_OUTLINE, Color32::from_rgb(120, 80, 80)),
            ("CG_EDGE_CONTRADICTS", palette::CG_EDGE_CONTRADICTS, Color32::from_rgb(220, 60, 60)),
            ("CG_EDGE_SUPPORTS", palette::CG_EDGE_SUPPORTS, Color32::from_rgb(100, 160, 220)),
            ("CG_EDGE_SPEAKER", palette::CG_EDGE_SPEAKER, Color32::from_rgb(140, 140, 160)),
            ("CG_EDGE_DEFAULT", palette::CG_EDGE_DEFAULT, Color32::from_rgb(180, 180, 190)),
        ];
        for (name, got, want) in rows {
            assert_eq!(got, want, "palette::{name} drifted from its legacy literal");
        }
    }

    #[test]
    fn data_helpers_match_legacy_literals() {
        assert_eq!(palette::heartbeat_dot(120), Color32::from_rgb(80, 120, 200));
        assert_eq!(palette::data_rgb(1, 2, 3), Color32::from_rgb(1, 2, 3));
    }

    #[test]
    fn ansi_defaults_match_legacy_literals() {
        assert_eq!(ansi::DEFAULT_FG, Color32::from_rgb(220, 220, 225));
        assert_eq!(ansi::DEFAULT_BG, Color32::from_rgb(12, 12, 16));
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
            assert_eq!(ansi::indexed(i as u8), Color32::from_rgb(*r, *g, *b), "ansi {i}");
            assert_eq!(ansi::PALETTE_16[i], ansi::indexed(i as u8));
        }
        assert_eq!(ansi::indexed(16), Color32::from_rgb(0, 0, 0));
        assert_eq!(ansi::indexed(231), Color32::from_rgb(255, 255, 255));
        assert_eq!(ansi::indexed(196), Color32::from_rgb(255, 0, 0));
        assert_eq!(ansi::indexed(232), Color32::from_rgb(8, 8, 8));
        assert_eq!(ansi::indexed(255), Color32::from_rgb(238, 238, 238));
    }
}
