//! Conversation graph view — ADR-067 G1–G5 scaffold (WEFT-630).
//!
//! Timeline-scrubbed causal graph for one conversation. Design:
//! `.planning/gui/conversation-graph-view/design.md`. Phase plan:
//! `docs/plans/adr-067-g1-g5-phase-plan.md`.
//!
//! ## Scaffold status
//!
//! Modules exist and compile; behaviour is intentionally incomplete until
//! each G-phase is claimed. Query [`phase_status`] for the table.

use eframe::egui;

pub mod encode;
pub mod feed;
pub mod interact;
pub mod layout;
pub mod model;
pub mod paint;

pub use model::{GEdge, GNode, GraphModel, NodeRole, NodeStateTag};

/// Follow-live vs scrubbed history (ADR-067 D4).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum ViewTime {
    /// Pin scrubber to now; poll head.
    #[default]
    Live,
    /// Rebuild graph-state-at-T (ms wall time or mapped chain_seq).
    Pinned(u64),
}

/// Implementation status for one GUI phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhaseStatus {
    /// Module hooks present; behaviour incomplete.
    Scaffolded,
    InProgress,
    Done,
}

/// One row of the G1–G5 status table.
#[derive(Debug, Clone, Copy)]
pub struct PhaseInfo {
    pub id: &'static str,
    pub name: &'static str,
    pub status: PhaseStatus,
    pub primary_modules: &'static [&'static str],
}

/// Canonical G1–G5 scaffold status (kept in sync with
/// `docs/plans/adr-067-g1-g5-phase-plan.md`).
pub fn phase_status() -> &'static [PhaseInfo] {
    &[
        PhaseInfo {
            id: "G1",
            name: "View scaffold + static fixture render",
            status: PhaseStatus::Scaffolded,
            primary_modules: &["mod", "model", "encode", "layout", "paint"],
        },
        PhaseInfo {
            id: "G2",
            name: "Live feed (follow-live)",
            status: PhaseStatus::Scaffolded,
            primary_modules: &["feed", "model"],
        },
        PhaseInfo {
            id: "G3",
            name: "Interaction: zoom/pan, inspect, filters",
            status: PhaseStatus::Scaffolded,
            primary_modules: &["interact"],
        },
        PhaseInfo {
            id: "G4",
            name: "Time scrubber + history replay",
            status: PhaseStatus::Scaffolded,
            primary_modules: &["interact", "feed"],
        },
        PhaseInfo {
            id: "G5",
            name: "Classification visual polish + clusters",
            status: PhaseStatus::Scaffolded,
            primary_modules: &["paint", "encode"],
        },
    ]
}

/// Honest empty-state when ECC conversation loop is not enabled (ADR-067 D7).
pub const EMPTY_STATE_ECC_OFF: &str = "ECC conversation loop not enabled";

/// Degraded G4 scrub hint until P1-graph snapshots land.
pub const HISTORY_INCOMPLETE_HINT: &str = "history-incomplete past this point";

/// Explorer panel shell for the conversation graph (G1 scaffold).
///
/// Renders fixture (or empty model) with timeline layout. Live feed, scrub,
/// and full interaction land in G2–G4.
#[derive(Debug, Default)]
pub struct ConversationGraphPanel {
    pub model: GraphModel,
    pub view_time: ViewTime,
    pub interact: interact::InteractState,
    /// When false, show [`EMPTY_STATE_ECC_OFF`] instead of the canvas.
    pub ecc_enabled: bool,
    /// Use the built-in fixture when no live data is present (G1 path).
    pub use_fixture: bool,
}

impl ConversationGraphPanel {
    /// Fresh panel with fixture preloaded for G1 review.
    pub fn with_fixture() -> Self {
        Self {
            model: GraphModel::fixture(),
            view_time: ViewTime::Live,
            interact: interact::InteractState::default(),
            ecc_enabled: true,
            use_fixture: true,
        }
    }

    /// Paint the graph (or empty-state) into `ui`.
    pub fn show(&mut self, ui: &mut egui::Ui) {
        if !self.ecc_enabled && !self.use_fixture {
            ui.centered_and_justified(|ui| {
                ui.label(egui::RichText::new(EMPTY_STATE_ECC_OFF).weak());
            });
            return;
        }

        if self.use_fixture && self.model.nodes.is_empty() {
            self.model = GraphModel::fixture();
        }

        // G3/G4: interact hooks (no-ops beyond state until fully wired).
        self.interact.handle_input(ui, &mut self.view_time);

        let placed = layout::layout_timeline(&self.model, ui.available_size());
        paint::paint_graph(ui, &self.model, &placed, &self.interact, self.view_time);

        if matches!(self.view_time, ViewTime::Pinned(_)) && !self.interact.full_history {
            ui.small(HISTORY_INCOMPLETE_HINT);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_table_covers_g1_through_g5() {
        let rows = phase_status();
        assert_eq!(rows.len(), 5);
        assert_eq!(rows[0].id, "G1");
        assert_eq!(rows[4].id, "G5");
        for r in rows {
            assert_eq!(r.status, PhaseStatus::Scaffolded);
        }
    }

    #[test]
    fn fixture_panel_has_nodes() {
        let p = ConversationGraphPanel::with_fixture();
        assert!(p.model.nodes.len() >= 4);
        assert!(!p.model.edges.is_empty());
    }
}
