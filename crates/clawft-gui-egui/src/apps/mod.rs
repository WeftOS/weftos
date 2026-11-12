//! WeftOS apps — first-class surfaces driven from the desktop sidebar.
//!
//! Phase 3 of the 0.8.0 desktop wave (see
//! `docs/plans/desktop-implementation-0.8.0.md`). Each module under
//! `apps::` corresponds to one entry in DESIGN.md §9 OOB manifest.
//!
//! The first cut ships **stubs** — heading + empty/loading/offline
//! state via [`state::render_if_needed`] against the bound substrate
//! paths. Each app's real content (table, tree, plot, surface
//! composer, …) is filled in by the swarm under follow-up Plane
//! tickets. The contract is: every app is one of the five archetypes
//! from DESIGN.md §4 and uses only the `blocks/` library + the
//! surface composer for rendering.

pub mod admin;
pub mod chat;
pub mod explorer;
pub mod files;
pub mod launcher;
pub mod logs;
pub mod monitor;
pub mod network;
pub mod processes;
pub mod scheduler;
pub mod services;
pub mod settings;
pub mod state;
pub mod terminal;

use std::sync::Arc;

use eframe::egui;

use crate::live::{Live, Snapshot};
use crate::shell::desktop::Desktop;
use crate::shell::sidebar::SidebarTarget;

/// Dispatch to the active app. Called from `desktop::show()` after
/// painting the sidebar + wallpaper.
///
/// Apps receive `&mut Desktop` so they can read/write their own state
/// (Explorer expansion set, blocks demo state, app registry, etc.) and
/// `&Arc<Live>` so they can submit RPC commands through the live bridge.
///
/// Lifecycle hygiene (WEFT-590): before dispatching the active app,
/// the previous active target is compared against the current one.
/// On a transition AWAY from an app that needs cleanup (today: only
/// Explorer, which holds substrate.list / substrate.read polls), the
/// per-app `close()` runs so background polls don't keep firing
/// against a hidden panel. `prev_active` is then refreshed.
///
/// Terminal/Chat sidebar apps intentionally do NOT close on
/// nav-away — the user might be mid-conversation or mid-shell-command
/// and expects to come back to the running session. Their state
/// survives across hides; only Explorer's RPC polls leak budget when
/// nobody's watching.
pub fn dispatch(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    desk: &mut Desktop,
    live: &Arc<Live>,
    snap: &Snapshot,
) {
    let target = desk.sidebar.active;
    if desk.prev_active != target {
        // Transition: run lifecycle cleanup on the app we're leaving.
        // Today only Explorer needs this.
        if let SidebarTarget::Explorer = desk.prev_active {
            desk.explorer.close(live);
        }
        desk.prev_active = target;
    }
    match target {
        SidebarTarget::Files => files::show(ui, rect, desk, live, snap),
        SidebarTarget::Processes => processes::show(ui, rect, desk, live, snap),
        SidebarTarget::Services => services::show(ui, rect, desk, live, snap),
        SidebarTarget::Network(tab) => network::show(ui, rect, desk, live, snap, tab),
        SidebarTarget::Settings => settings::show(ui, rect, desk, live, snap),
        SidebarTarget::Scheduler => scheduler::show(ui, rect, desk, live, snap),
        SidebarTarget::Monitor => monitor::show(ui, rect, desk, live, snap),
        SidebarTarget::Logs(tab) => logs::show(ui, rect, desk, live, snap, tab),
        SidebarTarget::Terminal => terminal::show(ui, rect, desk, live, snap),
        SidebarTarget::Chat => chat::show(ui, rect, desk, live, snap),
        SidebarTarget::Admin => admin::show(ui, rect, desk, live, snap),
        SidebarTarget::Explorer => explorer::show(ui, rect, desk, live, snap),
        // Agent Workspace freeform stage (ADR-073 / WEFT-686..688).
        // Stock desktop apps remain available on other sidebar targets.
        SidebarTarget::Workspace => {
            crate::shell::workspace::show(ui, rect, &mut desk.workspace);
        }
        SidebarTarget::Apps(tab) => launcher::show(ui, rect, desk, live, snap, tab),
    }
}

/// Horizontal inset of an app body from the app rect's edges. Matches the
/// x offset `paint_heading` uses, so heading and body share a left edge.
pub(crate) const BODY_INSET_X: f32 = 24.0;
/// Vertical inset between the body and the bottom edge of the app rect.
pub(crate) const BODY_INSET_Y: f32 = 8.0;

/// Body rect under a heading band of `band_h`, inset from the sidebar edge,
/// the right edge and the bottom edge of the window.
pub(crate) fn body_rect(rect: egui::Rect, band_h: f32) -> egui::Rect {
    egui::Rect::from_min_max(egui::pos2(rect.left(), rect.top() + band_h), rect.max)
        .shrink2(egui::vec2(BODY_INSET_X, BODY_INSET_Y))
}

/// Common header — heading text rendered top-left of the app rect.
pub(crate) fn paint_heading(ui: &egui::Ui, rect: egui::Rect, heading: &str) {
    use crate::theming::Tokens;
    let tokens = Tokens::default();
    let painter = ui.painter_at(rect);
    painter.text(
        egui::pos2(rect.left() + 24.0, rect.top() + 24.0),
        egui::Align2::LEFT_TOP,
        heading,
        egui::FontId::proportional(18.0),
        tokens.text_primary,
    );
}

/// Headless layout helpers shared by the app layout tests.
#[cfg(test)]
pub(crate) mod layout_test {
    use eframe::egui;

    /// One painted text run: its string, its bounding rect and the clip
    /// rect it was painted under.
    pub struct TextRun {
        pub text: String,
        pub bounds: egui::Rect,
        pub clip: egui::Rect,
    }

    impl TextRun {
        /// The part of the text that is actually visible (bounds ∩ clip).
        pub fn visible(&self) -> egui::Rect {
            self.bounds.intersect(self.clip)
        }
    }

    fn collect(shape: &egui::epaint::Shape, clip: egui::Rect, out: &mut Vec<TextRun>) {
        match shape {
            egui::epaint::Shape::Text(t) => out.push(TextRun {
                text: t.galley.text().to_string(),
                bounds: shape.visual_bounding_rect(),
                clip,
            }),
            egui::epaint::Shape::Vec(v) => v.iter().for_each(|s| collect(s, clip, out)),
            _ => {}
        }
    }

    /// Run `f` for a few frames in a headless context of `size` points
    /// (egui needs a frame or two to settle layout) and return the text
    /// runs painted by the last frame.
    pub fn run_frames(
        size: egui::Vec2,
        mut input: impl FnMut(usize) -> Vec<egui::Event>,
        mut f: impl FnMut(&mut egui::Ui, egui::Rect),
    ) -> Vec<TextRun> {
        let ctx = egui::Context::default();
        // The app's own style (text sizes and spacing) decides the layout.
        crate::theming::apply(&ctx);
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, size);
        let mut runs = Vec::new();
        for frame in 0..4 {
            let raw = egui::RawInput {
                screen_rect: Some(screen),
                events: input(frame),
                ..Default::default()
            };
            let out = ctx.run_ui(raw, |ui| {
                egui::CentralPanel::default()
                    .frame(egui::Frame::NONE)
                    .show_inside(ui, |ui| f(ui, screen));
            });
            runs.clear();
            for clipped in &out.shapes {
                collect(&clipped.shape, clipped.clip_rect, &mut runs);
            }
        }
        runs
    }
}
