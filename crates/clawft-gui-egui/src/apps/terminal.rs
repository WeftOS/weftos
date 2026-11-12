//! Terminal — first-class sidebar app (WEFT-587). Real PTY-backed
//! terminal emulator graduated from
//! `crates/clawft-gui-egui/src/explorer/terminal.rs`. DESIGN.md §9
//! sidebar 9.
//!
//! The renderer + RPC machinery (alacritty grid, vte parser, mouse
//! selection, scrollback wheel handler, multi-tab panel, etc.) live
//! in `explorer::terminal`. This module is a thin host: it owns
//! nothing itself, paints the canonical heading, and delegates the
//! body rect to the standalone
//! [`TerminalPanel`](crate::explorer::terminal::TerminalPanel)
//! instance kept on the [`Desktop`] (WEFT-263 multi-tab).
//!
//! State-lifting note: `desk.terminal` is independent from
//! `desk.explorer.terminal_view` (which still backs the
//! substrate-sentinel dispatch path inside the Explorer detail pane).
//! Two panels by design — the sidebar app is the user's "open a shell"
//! affordance; the substrate-sentinel terminal is whatever the
//! substrate topology decides to expose.
//!
//! Wasm builds get the real vt100-backed renderer (WEFT-264) from
//! `explorer::terminal` / `terminal_vt100` — same multi-tab panel API,
//! daemon RPC surface, and grid paint path as native (alacritty).

use std::sync::Arc;

use eframe::egui;

use crate::live::{Live, Snapshot};
use crate::shell::desktop::Desktop;

/// Heading band height. Matches the convention shared with
/// `apps/explorer.rs` and `apps/chat.rs` — `paint_heading` writes its
/// glyphs at `rect.left() + 24, rect.top() + 24`, so the body must
/// start below that.
const HEADING_BAND_H: f32 = 64.0;

pub fn show(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    desk: &mut Desktop,
    live: &Arc<Live>,
    _snap: &Snapshot,
) {
    super::paint_heading(ui, rect, "Terminal");
    let body = egui::Rect::from_min_max(
        egui::pos2(rect.left(), rect.top() + HEADING_BAND_H),
        rect.max,
    );
    // Confine the terminal body to the rect carved out below the
    // heading. `scope_builder` (vs. raw `new_child`) wraps the child
    // Ui so its widget entry is finalised against actual bounds —
    // important for the focus + click-and-drag plumbing inside
    // `TerminalPanel::paint` / `Terminal::paint_ui`.
    ui.scope_builder(egui::UiBuilder::new().max_rect(body), |ui| {
        desk.terminal.paint(ui, live);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apps::layout_test::run_frames;

    /// The terminal body must keep the heading's inset from the sidebar edge
    /// (it used to start flush against it), at 1280x800 and 1024x700.
    #[test]
    fn body_is_inset_from_the_sidebar_edge() {
        for (w, h) in [(1280.0, 800.0), (1024.0, 700.0)] {
            let mut desk = Desktop::default();
            let live = Live::spawn();
            let snap = Snapshot::default();
            let sidebar_w = 220.0;
            let runs = run_frames(
                egui::vec2(w, h),
                |_| vec![],
                |ui, screen| {
                    let rect = egui::Rect::from_min_max(egui::pos2(sidebar_w, 0.0), screen.max);
                    show(ui, rect, &mut desk, &live, &snap);
                },
            );
            assert!(!runs.is_empty(), "{w}x{h}: nothing painted");
            let min_left = runs
                .iter()
                .map(|r| r.bounds.left())
                .fold(f32::INFINITY, f32::min);
            assert!(
                min_left >= sidebar_w + crate::apps::BODY_INSET_X - 1.0,
                "{w}x{h}: terminal text starts at x={min_left}, flush against the sidebar edge"
            );
        }
    }
}
