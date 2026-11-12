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

pub mod ansi;
pub mod palette;

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
mod palette_contract;

#[cfg(test)]
impl Tokens {
    /// The `Visuals` that `Tokens::visuals` builds. `apply` installs them and then
    /// replaces them with egui's defaults (`set_global_style` of a `Style` that
    /// carries default visuals), so colour-snapshot tests that want the widget
    /// tints observable set them again with this.
    pub(crate) fn visuals_for_test(&self) -> egui::Visuals {
        self.visuals()
    }
}
