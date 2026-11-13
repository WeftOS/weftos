//! Interaction state — scrub, zoom/pan, select, filters (G3 / G4 scaffold).

use super::ViewTime;
use super::model::{GNode, GraphModel};
use eframe::egui::{self, Key};
use std::collections::HashSet;

/// Mutable UI interaction state for the conversation graph canvas.
#[derive(Debug, Clone)]
pub struct InteractState {
    pub selected: Option<String>,
    /// Classification / edge kinds that are **dimmed** (shown but faded).
    pub dim_edge_kinds: HashSet<String>,
    pub dim_intents: HashSet<String>,
    /// Visible time window pan offset (ms), G3.
    pub pan_ms: i64,
    /// Zoom scale on the time axis (1.0 = default).
    pub zoom: f32,
    /// When true, G5 cluster hulls paint.
    pub show_clusters: bool,
    /// LOD: collapse labels/badges.
    pub lod_collapse: bool,
    /// G4: when false, show history-incomplete hint under scrub.
    pub full_history: bool,
}

impl Default for InteractState {
    fn default() -> Self {
        Self {
            selected: None,
            dim_edge_kinds: HashSet::new(),
            dim_intents: HashSet::new(),
            pan_ms: 0,
            zoom: 1.0,
            show_clusters: true,
            lod_collapse: false,
            full_history: false,
        }
    }
}

impl InteractState {
    pub fn select(&mut self, id: Option<String>) {
        self.selected = id;
    }

    pub fn toggle_edge_filter(&mut self, kind: &str) {
        if !self.dim_edge_kinds.remove(kind) {
            self.dim_edge_kinds.insert(kind.to_string());
        }
    }

    pub fn edge_dimmed(&self, kind: &str) -> bool {
        self.dim_edge_kinds.contains(kind)
    }

    pub fn node_dimmed(&self, n: &GNode) -> bool {
        if let Some(intent) = n.classification.intent.as_deref()
            && self.dim_intents.contains(intent)
        {
            return true;
        }
        false
    }

    /// Clamp pan/zoom to sane ranges (G3 unit contract).
    pub fn clamp_view(&mut self) {
        self.zoom = self.zoom.clamp(0.25, 8.0);
        // pan unbounded in theory; soft clamp for tests / runaway
        self.pan_ms = self.pan_ms.clamp(-86_400_000, 86_400_000);
    }

    /// Handle keyboard step for scrubber (G4) and basic toggles.
    pub fn handle_input(&mut self, ui: &egui::Ui, view_time: &mut ViewTime) {
        let input = ui.input(|i| i.clone());
        if input.key_pressed(Key::ArrowLeft) {
            if let ViewTime::Pinned(t) = *view_time {
                *view_time = ViewTime::Pinned(t.saturating_sub(1_000));
            } else {
                *view_time = ViewTime::Pinned(0);
            }
        }
        if input.key_pressed(Key::ArrowRight) {
            match *view_time {
                ViewTime::Pinned(t) => *view_time = ViewTime::Pinned(t.saturating_add(1_000)),
                ViewTime::Live => {}
            }
        }
        if input.key_pressed(Key::L) {
            *view_time = ViewTime::Live;
        }
        self.clamp_view();
    }

    /// Resolve selected node for the inspect panel (G3).
    pub fn selected_node<'a>(&self, model: &'a GraphModel) -> Option<&'a GNode> {
        let id = self.selected.as_ref()?;
        model.nodes.iter().find(|n| n.id == *id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_toggle_and_selection() {
        let mut s = InteractState::default();
        s.select(Some("n1".into()));
        assert_eq!(s.selected.as_deref(), Some("n1"));
        s.toggle_edge_filter("Follows");
        assert!(s.edge_dimmed("Follows"));
        s.toggle_edge_filter("Follows");
        assert!(!s.edge_dimmed("Follows"));
    }

    #[test]
    fn clamp_view_bounds() {
        let mut s = InteractState {
            zoom: 100.0,
            pan_ms: i64::MAX / 2,
            ..Default::default()
        };
        s.clamp_view();
        assert!(s.zoom <= 8.0);
        assert!(s.pan_ms <= 86_400_000);
    }

    #[test]
    fn selected_node_from_fixture() {
        let m = GraphModel::fixture();
        let mut s = InteractState::default();
        s.select(Some("n2".into()));
        let n = s.selected_node(&m).unwrap();
        assert_eq!(n.id, "n2");
    }
}
