//! Chat — first-class sidebar app (WEFT-588). Concierge-bot agent
//! stream graduated from `crates/clawft-gui-egui/src/explorer/chat.rs`.
//! DESIGN.md §9 sidebar 10.
//!
//! All the real work — multi-conversation sidebar (WEFT-254), markdown
//! rendering, system-prompt editor, heartbeat label, identity-drift
//! warning, `agent.chat_stream` RPC plumbing (WEFT-253 progressive
//! frames) — lives in `explorer::chat`. This module is a thin host: it
//! owns nothing, paints the canonical heading, and delegates the body
//! to the standalone [`ChatPanel`](crate::explorer::chat::ChatPanel)
//! instance on [`Desktop`].
//!
//! State-lifting note: `desk.chat` is independent from
//! `desk.explorer.chat_view` (which backs the substrate-sentinel
//! dispatch path inside the Explorer detail pane). Two multi-session
//! panels by design — the sidebar app is the user's persistent
//! concierge-bot surface; the substrate-sentinel chat is whatever the
//! substrate topology surfaces under a `{kind:"chat"}` value.
//!
//! Wire shape: `chat::paint` takes a substrate path string and a
//! sentinel value. We synthesise both — the sidebar app has no
//! substrate path of its own (it's a permanent UI surface, not a
//! topology-mounted panel) and we hardcode the model name. Both
//! arguments are cosmetic only inside `chat::paint` — the path is
//! used solely for the muted footer hint and the value is queried
//! only for the model display string. Neither drives any RPC.

use std::sync::Arc;

use eframe::egui;
use serde_json::json;

use crate::live::{Live, Snapshot};
use crate::shell::desktop::Desktop;

const HEADING_BAND_H: f32 = 64.0;

/// Cosmetic substrate path shown in the chat panel's muted footer.
/// Not a real substrate mount — the sidebar Chat app is a permanent
/// surface, not topology-driven.
const SIDEBAR_CHAT_PATH: &str = "ui://sidebar/chat";

pub fn show(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    desk: &mut Desktop,
    live: &Arc<Live>,
    _snap: &Snapshot,
) {
    super::paint_heading(ui, rect, "Chat · concierge-bot");
    let body = super::body_rect(rect, HEADING_BAND_H);

    // Synthesised sentinel — seeds the WEFT-256 model chip strip until
    // `llm.models` lands. Cosmetic path only; RPC selection is owned
    // by `ChatView` once the catalog arrives.
    let sentinel = json!({ "kind": "chat", "model": "local" });

    ui.scope_builder(egui::UiBuilder::new().max_rect(body), |ui| {
        crate::explorer::chat::paint(ui, SIDEBAR_CHAT_PATH, &sentinel, &mut desk.chat, live);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apps::layout_test::run_frames;

    /// Sidebar width used by the desktop shell; the app rect starts here.
    const SIDEBAR_W: f32 = 220.0;

    fn layout(w: f32, h: f32) -> (egui::Rect, Vec<crate::apps::layout_test::TextRun>) {
        let mut desk = Desktop::default();
        let live = Live::spawn();
        let snap = Snapshot::default();
        let mut app_rect = egui::Rect::NOTHING;
        let runs = run_frames(
            egui::vec2(w, h),
            |_| vec![],
            |ui, screen| {
                app_rect = egui::Rect::from_min_max(egui::pos2(SIDEBAR_W, 0.0), screen.max);
                show(ui, app_rect, &mut desk, &live, &snap);
            },
        );
        (app_rect, runs)
    }

    /// The chat body must keep the heading's inset from the sidebar edge (it
    /// used to start flush against it), at 1280x800 and at a narrower 1024x700.
    #[test]
    fn body_is_inset_from_the_sidebar_edge() {
        for (w, h) in [(1280.0, 800.0), (1024.0, 700.0)] {
            let (rect, runs) = layout(w, h);
            let min_left = runs
                .iter()
                .map(|r| r.bounds.left())
                .fold(f32::INFINITY, f32::min);
            assert!(
                min_left >= rect.left() + crate::apps::BODY_INSET_X - 1.0,
                "{w}x{h}: text starts at x={min_left}, flush against the sidebar edge at {}",
                rect.left()
            );
        }
    }

    /// The input row and the Send button must fit inside the window (the
    /// Send button used to run off the bottom), at 1280x800 and 1024x700.
    #[test]
    fn input_row_and_send_button_fit_the_window() {
        for (w, h) in [(1280.0, 800.0), (1024.0, 700.0)] {
            let (_, runs) = layout(w, h);
            let send = runs
                .iter()
                .find(|r| r.text == "Send")
                .unwrap_or_else(|| panic!("{w}x{h}: no Send button painted"));
            assert!(
                send.bounds.bottom() <= h - crate::apps::BODY_INSET_Y + 1.0
                    && send.bounds.right() <= w,
                "{w}x{h}: Send button runs off the window: {:?}",
                send.bounds
            );
        }
    }
}
