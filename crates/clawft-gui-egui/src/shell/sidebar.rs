//! Canonical desktop sidebar — the launcher and connection indicator
//! for the WeftOS desktop shell.
//!
//! This is the **frozen canonical block** specified in
//! [`docs/DESIGN.md`](../../../../../docs/DESIGN.md) §5. It is byte-
//! identical on every screen — width, fill, header, divider, item
//! order, footer. Per-screen state is limited to which row is
//! highlighted and which group (if any) is expanded.
//!
//! Any drift from the spec is a bug. Snapshot tests in
//! `tests/sidebar_snapshot.rs` (Phase 2a follow-up) compare painted
//! output against `docs/design/mockups/desktop-0.8.0.png`.
//!
//! Implementation notes:
//! - The sidebar reserves 220 px (or 48 px when collapsed). The caller
//!   is responsible for placing the body to the right of the returned
//!   width via [`Sidebar::reserved_width`].
//! - State that survives daemon restart belongs at
//!   `substrate/desktop/sidebar/*`. The first cut keeps it in-process;
//!   persistence ships when Settings (Phase 3) wires `config.set`.
//! - The active row is a `bg_active` lift plus a 3 px `text_primary`
//!   stripe (neutral; NO chromatic accent, DESIGN.md §2.1/§5); hover is a
//!   plain `bg_hover` lift with no stripe, so the two read as different.

use std::collections::BTreeSet;

use eframe::egui;

use crate::live::{Connection, Snapshot};
use crate::theming::Tokens;

// ── Layout constants ────────────────────────────────────────────────

pub const SIDEBAR_WIDTH_EXPANDED: f32 = 220.0;
pub const SIDEBAR_WIDTH_COLLAPSED: f32 = 48.0;
pub const HEADER_HEIGHT: f32 = 110.0;
pub const ROW_HEIGHT: f32 = 32.0;
pub const SUBROW_HEIGHT: f32 = 28.0;
pub const ROW_PADDING_LEFT: f32 = 12.0;
pub const SUBROW_INDENT: f32 = 16.0;
pub const ICON_BOX: f32 = 16.0;
pub const ICON_GAP: f32 = 8.0;
pub const FOOTER_HEIGHT: f32 = 32.0;

// ── Public types ────────────────────────────────────────────────────

/// Persistent sidebar state. Owned by `Desktop`.
#[derive(Clone, Debug)]
pub struct Sidebar {
    pub collapsed: bool,
    pub hidden: bool,
    pub expanded: BTreeSet<&'static str>,
    pub active: SidebarTarget,
}

impl Default for Sidebar {
    fn default() -> Self {
        Self {
            collapsed: false,
            hidden: false,
            expanded: BTreeSet::new(),
            active: SidebarTarget::Files,
        }
    }
}

impl Sidebar {
    /// Width currently reserved on the left edge.
    pub fn reserved_width(&self) -> f32 {
        if self.hidden {
            6.0 // edge-handle pin
        } else if self.collapsed {
            SIDEBAR_WIDTH_COLLAPSED
        } else {
            SIDEBAR_WIDTH_EXPANDED
        }
    }

    /// Apply an action to internal state.
    pub fn apply(&mut self, action: SidebarAction) {
        match action {
            SidebarAction::Open(t) => self.active = t,
            SidebarAction::ToggleGroup(id) => {
                if !self.expanded.remove(id) {
                    self.expanded.insert(id);
                }
            }
            SidebarAction::ToggleCollapsed => self.collapsed = !self.collapsed,
            SidebarAction::ToggleHidden => self.hidden = !self.hidden,
        }
    }

    /// Whether this leaf or sub-leaf is the active one.
    fn is_active(&self, target: SidebarTarget) -> bool {
        self.active == target
    }

    /// Whether this group is the *parent* of the active sub-leaf.
    fn group_holds_active(&self, group_id: &str) -> bool {
        matches!(
            (group_id, self.active),
            ("network", SidebarTarget::Network(_))
                | ("logs", SidebarTarget::Logs(_))
                | ("apps", SidebarTarget::Apps(_))
        )
    }
}

/// Where the sidebar can navigate to. Matches DESIGN.md §9 OOB manifest.
///
/// `Workspace` is the ADR-073 Agent Workspace mode (0.9+ freeform stage).
/// It is intentionally optional and does not replace stock desktop apps.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum SidebarTarget {
    Files,
    Processes,
    Services,
    Network(NetworkTab),
    Settings,
    Scheduler,
    Monitor,
    Logs(LogsTab),
    Terminal,
    Chat,
    Admin,
    Explorer,
    /// Freeform multi-agent stage (WEFT-686 / WEFT-687).
    Workspace,
    Apps(AppsTab),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum NetworkTab {
    Mesh,
    WiFi,
    Bluetooth,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum LogsTab {
    System,
    WitnessChain,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum AppsTab {
    BuiltIn,
    Installed,
    Developer,
}

/// User actions surfaced from one paint pass.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SidebarAction {
    Open(SidebarTarget),
    ToggleGroup(&'static str),
    ToggleCollapsed,
    ToggleHidden,
}

// ── Render ──────────────────────────────────────────────────────────

/// Paint the sidebar inside `rect`. Returns the user's last action, if
/// any, for the desktop to dispatch.
pub fn paint(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    state: &Sidebar,
    snap: &Snapshot,
) -> Option<SidebarAction> {
    let tokens = Tokens::default();
    let painter = ui.painter_at(rect);

    // Region fill (DESIGN.md §2.1 + §5)
    painter.rect_filled(rect, 0.0, tokens.bg_sidebar);
    // Right-edge separator stroke (`stroke_soft`)
    painter.line_segment(
        [
            egui::pos2(rect.right(), rect.top()),
            egui::pos2(rect.right(), rect.bottom()),
        ],
        egui::Stroke::new(1.0, tokens.stroke_soft),
    );

    let mut action: Option<SidebarAction> = None;
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect)
            .layout(egui::Layout::top_down(egui::Align::LEFT)),
    );

    // Header (identity + Kernel chip + divider)
    paint_header(&mut child, rect, snap, &tokens, state.collapsed);

    // Menu items
    let menu_top = rect.top() + HEADER_HEIGHT;
    let menu_rect = egui::Rect::from_min_max(
        egui::pos2(rect.left(), menu_top),
        egui::pos2(rect.right(), rect.bottom() - FOOTER_HEIGHT),
    );
    // The body scrolls when the expanded groups outgrow the space between
    // the header and the pinned footer; the scroll area clips its rows to
    // `menu_rect` so they can never paint over the footer.
    let mut menu_ui = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(menu_rect)
            .layout(egui::Layout::top_down(egui::Align::LEFT)),
    );
    menu_ui.set_clip_rect(menu_rect.intersect(ui.clip_rect()));
    let menu_out = egui::ScrollArea::vertical()
        .id_salt("sidebar-menu")
        .auto_shrink([false, false])
        .show(&mut menu_ui, |ui| paint_menu(ui, state, &tokens));
    if let Some(a) = menu_out.inner {
        action = Some(a);
    }

    // Footer collapse handle
    let footer_rect = egui::Rect::from_min_max(
        egui::pos2(rect.left(), rect.bottom() - FOOTER_HEIGHT),
        egui::pos2(rect.right(), rect.bottom()),
    );
    if let Some(a) = paint_footer(ui, footer_rect, &tokens, state.collapsed) {
        action = Some(a);
    }

    action
}

fn paint_header(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    snap: &Snapshot,
    tokens: &Tokens,
    collapsed: bool,
) {
    let painter = ui.painter_at(rect);
    let header_rect = egui::Rect::from_min_max(
        egui::pos2(rect.left(), rect.top()),
        egui::pos2(rect.right(), rect.top() + HEADER_HEIGHT),
    );

    if !collapsed {
        // Line 1 — version
        painter.text(
            egui::pos2(rect.left() + ROW_PADDING_LEFT, rect.top() + 14.0),
            egui::Align2::LEFT_TOP,
            format!("WeftOS v{}", env!("CARGO_PKG_VERSION")),
            egui::FontId::monospace(12.5),
            tokens.text_secondary,
        );
        // Line 2 — instance name (working-directory basename for now)
        painter.text(
            egui::pos2(rect.left() + ROW_PADDING_LEFT, rect.top() + 32.0),
            egui::Align2::LEFT_TOP,
            instance_name(),
            egui::FontId::monospace(12.5),
            tokens.text_secondary,
        );
    }

    // Kernel chip — connection indicator (DESIGN.md §5 connection status rule)
    let chip_top = rect.top() + 56.0;
    let chip_h = 28.0;
    let chip_rect = egui::Rect::from_min_max(
        egui::pos2(rect.left() + ROW_PADDING_LEFT, chip_top),
        egui::pos2(rect.right() - ROW_PADDING_LEFT, chip_top + chip_h),
    );
    paint_kernel_chip(ui, chip_rect, snap.connection, tokens, collapsed);

    // Divider — between hair and soft; reuse the closest existing
    // token (`stroke_hair`) rather than introducing a new alpha.
    painter.line_segment(
        [
            egui::pos2(rect.left(), header_rect.bottom()),
            egui::pos2(rect.right(), header_rect.bottom()),
        ],
        egui::Stroke::new(1.0, tokens.stroke_hair),
    );
}

fn paint_kernel_chip(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    conn: Connection,
    tokens: &Tokens,
    collapsed: bool,
) {
    let painter = ui.painter_at(rect);
    // Chip outline (`stroke_soft`)
    painter.rect_stroke(
        rect,
        egui::CornerRadius::same(4),
        egui::Stroke::new(1.0, tokens.stroke_soft),
        egui::epaint::StrokeKind::Inside,
    );

    let dot_color = match conn {
        Connection::Connected => tokens.ok,
        Connection::Connecting => tokens.warn,
        Connection::Disconnected => tokens.crit,
    };
    let dot_x = rect.left() + 12.0;
    let dot_y = rect.center().y;
    painter.circle_filled(egui::pos2(dot_x, dot_y), 4.0, dot_color);

    if collapsed {
        return;
    }

    // Disconnected case is the only one whose label text turns red
    // (DESIGN.md §5 connection-status rule).
    let label_color = match conn {
        Connection::Disconnected => tokens.crit,
        _ => tokens.text_secondary,
    };
    let state_text = match conn {
        Connection::Connected => "connected",
        Connection::Connecting => "connecting",
        Connection::Disconnected => "disconnected",
    };
    painter.text(
        egui::pos2(dot_x + 12.0, dot_y),
        egui::Align2::LEFT_CENTER,
        format!("Kernel · {state_text}"),
        egui::FontId::proportional(12.0),
        label_color,
    );
}

fn paint_menu(ui: &mut egui::Ui, state: &Sidebar, tokens: &Tokens) -> Option<SidebarAction> {
    // Rows are placed by hand from the scroll area's origin; the content
    // height is reported back with `allocate_exact_size` at the end.
    let origin = ui.cursor().min;
    let rect = egui::Rect::from_min_size(origin, egui::vec2(ui.available_width(), 0.0));
    let mut cursor_y = rect.top() + 4.0;
    let mut action: Option<SidebarAction> = None;

    // Leaf rows (canonical order — DESIGN.md §5). Agent Workspace
    // (ADR-073) is appended after Explorer so the 0.8 stock order is
    // preserved; Workspace is an optional 0.9+ mode, not a reorder.
    // Canonical icon set per DESIGN.md §5. The Geometric Shapes /
    // Math / Block Elements / Arrows glyphs (▢ ≣ ↯ ◯ ◷ ▥ ≡ ▌ ▦)
    // are covered by the DejaVuSans subset registered as a fallback
    // in `app::install_symbol_font`; the rest (⚙ ✱ ⛨ ⌖) come from
    // egui's default Ubuntu-Light + NotoEmoji. Workspace uses ❖ (U+2756,
    // in DejaVuSans-WeftSymbols).
    let items: [MenuItem; 14] = [
        MenuItem::Leaf("Files", "▢", SidebarTarget::Files),
        MenuItem::Leaf("Processes", "≣", SidebarTarget::Processes),
        MenuItem::Leaf("Services", "↯", SidebarTarget::Services),
        MenuItem::Group(
            "Network",
            "◯",
            "network",
            &[
                ("Mesh", SidebarTarget::Network(NetworkTab::Mesh)),
                ("Wi-Fi", SidebarTarget::Network(NetworkTab::WiFi)),
                ("Bluetooth", SidebarTarget::Network(NetworkTab::Bluetooth)),
            ],
        ),
        MenuItem::Leaf("Settings", "⚙", SidebarTarget::Settings),
        MenuItem::Leaf("Scheduler", "◷", SidebarTarget::Scheduler),
        MenuItem::Leaf("Monitor", "▥", SidebarTarget::Monitor),
        MenuItem::Group(
            "Logs",
            "≡",
            "logs",
            &[
                ("System", SidebarTarget::Logs(LogsTab::System)),
                ("Witness chain", SidebarTarget::Logs(LogsTab::WitnessChain)),
            ],
        ),
        MenuItem::Leaf("Terminal", "▌", SidebarTarget::Terminal),
        MenuItem::Leaf("Chat", "✱", SidebarTarget::Chat),
        MenuItem::Leaf("Admin", "⛨", SidebarTarget::Admin),
        MenuItem::Leaf("Explorer", "⌖", SidebarTarget::Explorer),
        MenuItem::Leaf("Workspace", "❖", SidebarTarget::Workspace),
        MenuItem::Group(
            "Apps",
            "▦",
            "apps",
            &[
                ("Built-in", SidebarTarget::Apps(AppsTab::BuiltIn)),
                ("Installed", SidebarTarget::Apps(AppsTab::Installed)),
                ("Developer", SidebarTarget::Apps(AppsTab::Developer)),
            ],
        ),
    ];

    for item in items.iter() {
        match item {
            MenuItem::Leaf(label, icon, target) => {
                let row_rect = egui::Rect::from_min_max(
                    egui::pos2(rect.left(), cursor_y),
                    egui::pos2(rect.right(), cursor_y + ROW_HEIGHT),
                );
                if paint_row(
                    ui,
                    row_rect,
                    label,
                    icon,
                    None,
                    state.is_active(*target),
                    state.collapsed,
                    tokens,
                ) {
                    action = Some(SidebarAction::Open(*target));
                }
                cursor_y += ROW_HEIGHT;
            }
            MenuItem::Group(label, icon, group_id, subs) => {
                let row_rect = egui::Rect::from_min_max(
                    egui::pos2(rect.left(), cursor_y),
                    egui::pos2(rect.right(), cursor_y + ROW_HEIGHT),
                );
                let expanded = state.expanded.contains(group_id);
                let chevron = Some(if expanded { "▾" } else { "▸" });
                let parent_active = state.group_holds_active(group_id);
                if paint_row(
                    ui,
                    row_rect,
                    label,
                    icon,
                    chevron,
                    parent_active && !expanded,
                    state.collapsed,
                    tokens,
                ) {
                    action = Some(SidebarAction::ToggleGroup(group_id));
                }
                cursor_y += ROW_HEIGHT;

                if expanded && !state.collapsed {
                    for (sub_label, sub_target) in subs.iter() {
                        let sub_rect = egui::Rect::from_min_max(
                            egui::pos2(rect.left(), cursor_y),
                            egui::pos2(rect.right(), cursor_y + SUBROW_HEIGHT),
                        );
                        if paint_subrow(
                            ui,
                            sub_rect,
                            sub_label,
                            state.is_active(*sub_target),
                            tokens,
                        ) {
                            action = Some(SidebarAction::Open(*sub_target));
                        }
                        cursor_y += SUBROW_HEIGHT;
                    }
                }
            }
        }
    }

    let total_h = cursor_y - origin.y;
    ui.allocate_exact_size(egui::vec2(rect.width(), total_h), egui::Sense::hover());

    action
}

/// Width of the active-row stripe, px.
const ACTIVE_STRIPE_W: f32 = 3.0;

enum MenuItem {
    Leaf(&'static str, &'static str, SidebarTarget),
    Group(
        &'static str,
        &'static str,
        &'static str,
        &'static [(&'static str, SidebarTarget)],
    ),
}

#[allow(clippy::too_many_arguments)]
fn paint_row(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    label: &str,
    icon: &str,
    chevron: Option<&str>,
    active: bool,
    collapsed: bool,
    tokens: &Tokens,
) -> bool {
    let response = ui.interact(
        rect,
        egui::Id::new(("sidebar-row", label)),
        egui::Sense::click(),
    );
    let painter = ui.painter_at(rect);

    if active {
        painter.rect_filled(rect, 0.0, tokens.bg_active);
        // 3px left edge stripe in the brightest neutral (`text_primary`).
        // Selection stays achromatic (DESIGN.md §5); a hovered row is a
        // plain lift with no stripe, so the two states cannot be confused
        // (bg_hover is darker than bg_sidebar, bg_active only marginally
        // lighter).
        painter.rect_filled(
            egui::Rect::from_min_max(
                egui::pos2(rect.left(), rect.top()),
                egui::pos2(rect.left() + ACTIVE_STRIPE_W, rect.bottom()),
            ),
            0.0,
            tokens.text_primary,
        );
    } else if response.hovered() {
        painter.rect_filled(rect, 0.0, tokens.bg_hover);
    }

    let icon_x = rect.left() + ROW_PADDING_LEFT;
    let label_color = if active {
        tokens.text_primary
    } else {
        tokens.text_secondary
    };
    painter.text(
        egui::pos2(icon_x, rect.center().y),
        egui::Align2::LEFT_CENTER,
        icon,
        egui::FontId::proportional(13.0),
        label_color,
    );

    if !collapsed {
        painter.text(
            egui::pos2(icon_x + ICON_BOX + ICON_GAP, rect.center().y),
            egui::Align2::LEFT_CENTER,
            label,
            egui::FontId::proportional(13.0),
            label_color,
        );
        if let Some(ch) = chevron {
            painter.text(
                egui::pos2(rect.right() - ROW_PADDING_LEFT, rect.center().y),
                egui::Align2::RIGHT_CENTER,
                ch,
                egui::FontId::proportional(13.0),
                tokens.text_dim,
            );
        }
    }

    response.clicked()
}

fn paint_subrow(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    label: &str,
    active: bool,
    tokens: &Tokens,
) -> bool {
    let response = ui.interact(
        rect,
        egui::Id::new(("sidebar-subrow", label)),
        egui::Sense::click(),
    );
    let painter = ui.painter_at(rect);

    if active {
        painter.rect_filled(rect, 0.0, tokens.bg_active);
        painter.rect_filled(
            egui::Rect::from_min_max(
                egui::pos2(rect.left(), rect.top()),
                egui::pos2(rect.left() + ACTIVE_STRIPE_W, rect.bottom()),
            ),
            0.0,
            tokens.text_primary,
        );
    } else if response.hovered() {
        painter.rect_filled(rect, 0.0, tokens.bg_hover);
    }

    let label_color = if active {
        tokens.text_primary
    } else {
        tokens.text_dim
    };
    painter.text(
        egui::pos2(
            rect.left() + ROW_PADDING_LEFT + SUBROW_INDENT,
            rect.center().y,
        ),
        egui::Align2::LEFT_CENTER,
        format!("· {label}"),
        egui::FontId::proportional(12.0),
        label_color,
    );

    response.clicked()
}

fn paint_footer(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    tokens: &Tokens,
    collapsed: bool,
) -> Option<SidebarAction> {
    let painter = ui.painter_at(rect);
    // 1px divider at top of footer (`stroke_hair`)
    painter.line_segment(
        [
            egui::pos2(rect.left(), rect.top()),
            egui::pos2(rect.right(), rect.top()),
        ],
        egui::Stroke::new(1.0, tokens.stroke_hair),
    );

    let response = ui.interact(
        rect,
        egui::Id::new("sidebar-collapse"),
        egui::Sense::click(),
    );
    if response.hovered() {
        ui.painter_at(rect).rect_filled(rect, 0.0, tokens.bg_hover);
    }

    let glyph = if collapsed { "▶" } else { "◀" };
    let painter = ui.painter_at(rect);
    painter.text(
        egui::pos2(rect.left() + ROW_PADDING_LEFT, rect.center().y),
        egui::Align2::LEFT_CENTER,
        glyph,
        egui::FontId::proportional(13.0),
        tokens.text_secondary,
    );
    if !collapsed {
        painter.text(
            egui::pos2(
                rect.left() + ROW_PADDING_LEFT + ICON_BOX + ICON_GAP,
                rect.center().y,
            ),
            egui::Align2::LEFT_CENTER,
            "collapse",
            egui::FontId::proportional(12.0),
            tokens.text_secondary,
        );
    }

    if response.clicked() {
        Some(SidebarAction::ToggleCollapsed)
    } else {
        None
    }
}

// ── Helpers ─────────────────────────────────────────────────────────

/// Instance name. For now the working-directory basename per
/// DESIGN.md §5 fallback; Settings will let the user override via
/// `config/identity/instance_name` once Phase 3 ships.
fn instance_name() -> String {
    std::env::current_dir()
        .ok()
        .and_then(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "weftos".to_string())
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_files_active_collapsed_off() {
        let s = Sidebar::default();
        assert!(!s.collapsed);
        assert!(!s.hidden);
        assert!(s.expanded.is_empty());
        assert_eq!(s.active, SidebarTarget::Files);
        assert_eq!(s.reserved_width(), SIDEBAR_WIDTH_EXPANDED);
    }

    #[test]
    fn collapse_changes_reserved_width() {
        let mut s = Sidebar::default();
        s.apply(SidebarAction::ToggleCollapsed);
        assert!(s.collapsed);
        assert_eq!(s.reserved_width(), SIDEBAR_WIDTH_COLLAPSED);
        s.apply(SidebarAction::ToggleCollapsed);
        assert!(!s.collapsed);
        assert_eq!(s.reserved_width(), SIDEBAR_WIDTH_EXPANDED);
    }

    #[test]
    fn hidden_pins_an_edge_handle() {
        let mut s = Sidebar::default();
        s.apply(SidebarAction::ToggleHidden);
        assert!(s.hidden);
        assert_eq!(s.reserved_width(), 6.0);
    }

    #[test]
    fn group_toggle_expands_and_collapses() {
        let mut s = Sidebar::default();
        assert!(!s.expanded.contains("network"));
        s.apply(SidebarAction::ToggleGroup("network"));
        assert!(s.expanded.contains("network"));
        s.apply(SidebarAction::ToggleGroup("network"));
        assert!(!s.expanded.contains("network"));
    }

    #[test]
    fn open_sets_active_target() {
        let mut s = Sidebar::default();
        s.apply(SidebarAction::Open(SidebarTarget::Settings));
        assert_eq!(s.active, SidebarTarget::Settings);
        s.apply(SidebarAction::Open(SidebarTarget::Logs(
            LogsTab::WitnessChain,
        )));
        assert_eq!(s.active, SidebarTarget::Logs(LogsTab::WitnessChain));
        assert!(s.group_holds_active("logs"));
        assert!(!s.group_holds_active("network"));
    }

    #[test]
    fn group_holds_active_recognises_each_group() {
        let mut s = Sidebar::default();
        for (target, group) in [
            (SidebarTarget::Network(NetworkTab::Mesh), "network"),
            (SidebarTarget::Logs(LogsTab::System), "logs"),
            (SidebarTarget::Apps(AppsTab::BuiltIn), "apps"),
        ] {
            s.active = target;
            assert!(
                s.group_holds_active(group),
                "group {group} should hold {target:?}"
            );
        }
    }

    #[test]
    fn instance_name_is_non_empty() {
        let n = instance_name();
        assert!(!n.is_empty());
    }

    use crate::apps::layout_test::run_frames;

    fn expanded_all() -> Sidebar {
        let mut s = Sidebar::default();
        for g in ["network", "logs", "apps"] {
            s.apply(SidebarAction::ToggleGroup(g));
        }
        s
    }

    /// Paint the sidebar for a few frames at `h` points high, with `events`
    /// injected on every frame after the first, and return the text runs.
    fn paint_sidebar(
        h: f32,
        state: &Sidebar,
        events: impl Fn() -> Vec<egui::Event>,
    ) -> Vec<crate::apps::layout_test::TextRun> {
        let snap = Snapshot::default();
        run_frames(
            egui::vec2(SIDEBAR_WIDTH_EXPANDED, h),
            |frame| if frame == 0 { vec![] } else { events() },
            |ui, screen| {
                let _ = paint(ui, screen, state, &snap);
            },
        )
    }

    fn footer_top(h: f32) -> f32 {
        h - FOOTER_HEIGHT
    }

    /// With Network, Logs and Apps expanded the menu is taller than the
    /// window. No menu row may be visible over the pinned footer, and the
    /// footer must still be there.
    #[test]
    fn expanded_menu_does_not_overprint_the_footer() {
        for h in [800.0, 700.0] {
            let runs = paint_sidebar(h, &expanded_all(), Vec::new);
            let footer = runs
                .iter()
                .find(|r| r.text == "collapse")
                .expect("footer label painted");
            assert!(
                footer.bounds.top() >= footer_top(h),
                "footer moved at h={h}"
            );
            let footer_texts = ["collapse", "◀", "▶"];
            for r in runs
                .iter()
                .filter(|r| !footer_texts.contains(&r.text.as_str()))
            {
                let v = r.visible();
                let overlaps_footer = v.is_positive() && v.bottom() > footer_top(h) + 0.5;
                assert!(
                    !overlaps_footer || r.bounds.top() < 0.0,
                    "h={h}: {:?} is visible over the footer (visible {:?}, footer top {})",
                    r.text,
                    v,
                    footer_top(h)
                );
            }
        }
    }

    /// The last row ("Developer") is out of reach unless the body scrolls:
    /// scrolling the body must bring it fully above the footer.
    #[test]
    fn menu_scrolls_to_the_last_row() {
        let h = 700.0;
        let wheel = || {
            vec![
                egui::Event::PointerMoved(egui::pos2(60.0, 300.0)),
                egui::Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Point,
                    phase: egui::TouchPhase::Move,
                    delta: egui::vec2(0.0, -2000.0),
                    modifiers: egui::Modifiers::NONE,
                },
            ]
        };
        let runs = paint_sidebar(h, &expanded_all(), wheel);
        let dev = runs
            .iter()
            .find(|r| r.text == "· Developer")
            .expect("Developer row painted");
        assert!(
            dev.visible().height() >= dev.bounds.height() - 0.5
                && dev.bounds.bottom() <= footer_top(h),
            "Developer not fully visible above the footer after scrolling: {:?}, footer top {}",
            dev.bounds,
            footer_top(h)
        );
    }

    /// Fills and the left-edge stripe painted for the row whose label is
    /// `label`: (full-width tile fill, stripe colour and width).
    fn row_paint(
        shapes: &[egui::Shape],
        label: &str,
        pos: egui::Pos2,
    ) -> (Option<egui::Color32>, Option<(egui::Color32, f32)>) {
        let _ = label;
        let mut tile = None;
        let mut stripe = None;
        for s in shapes {
            if let egui::Shape::Rect(r) = s {
                if r.rect.contains(pos) && r.rect.width() > 100.0 && r.rect.height() < 40.0 {
                    tile = Some(r.fill);
                }
                if r.rect.width() <= 4.0
                    && r.rect.top() <= pos.y
                    && r.rect.bottom() >= pos.y
                    && r.rect.left() < 4.0
                {
                    stripe = Some((r.fill, r.rect.width()));
                }
            }
        }
        (tile, stripe)
    }

    fn label_pos(label: &str) -> egui::Pos2 {
        let runs = paint_sidebar(800.0, &Sidebar::default(), Vec::new);
        let r = runs.iter().find(|r| r.text == label).expect("label");
        r.bounds.center()
    }

    /// Hover and selected tiles must be told apart at token level: the
    /// selected row carries a neutral `text_primary` stripe (not accent, not
    /// the old dim `text_dim`), the hovered row carries none, fills differ.
    #[test]
    fn hover_and_selected_tiles_are_clearly_distinct() {
        let t = Tokens::default();
        let size = egui::vec2(SIDEBAR_WIDTH_EXPANDED, 800.0);
        let snap = Snapshot::default();
        let sel_pos = label_pos("Files");
        let hov_pos = label_pos("Processes");
        let paint_it = |ui: &mut egui::Ui, screen: egui::Rect| {
            let _ = paint(ui, screen, &Sidebar::default(), &snap);
        };
        let hover = |frame: usize| {
            if frame == 0 {
                vec![]
            } else {
                vec![egui::Event::PointerMoved(hov_pos)]
            }
        };
        let shapes = crate::apps::layout_test::run_shapes(size, hover, paint_it);
        let (sel_fill, sel_stripe) = row_paint(&shapes, "Files", sel_pos);
        let (hov_fill, hov_stripe) = row_paint(&shapes, "Processes", hov_pos);
        assert_eq!(sel_fill, Some(t.bg_active), "selected tile fill");
        assert_eq!(hov_fill, Some(t.bg_hover), "hover tile fill");
        assert!(hov_stripe.is_none(), "hover must not paint a stripe");
        let (c, w) = sel_stripe.expect("selected row paints a stripe");
        assert_eq!(
            c, t.text_primary,
            "selected stripe is the neutral text_primary token"
        );
        assert_ne!(c, t.accent, "selection must not use the chromatic accent");
        assert_ne!(
            c, t.text_dim,
            "stripe must be brighter than the old dim grey"
        );
        assert_ne!(sel_fill, hov_fill, "selected and hover fills differ");
        assert!(w >= 3.0, "selected stripe is at least 3 px, got {w}");
    }
}
