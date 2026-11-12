//! Files — substrate browser. DESIGN.md §9 sidebar 1, archetype
//! `app-window` (DESIGN.md §4.1 list-detail). WEFT-579.
//!
//! In WeftOS the substrate IS the filesystem. The wasm panel can't
//! see the host's POSIX filesystem (browser sandbox), but every
//! resource the daemon publishes — kernel state, mesh peers, chain
//! events, RVF manifests, agent manifests, sensor topics — lives at
//! a path under `substrate/`. The Files app surfaces that tree
//! directly: left pane is a collapsible folder/leaf hierarchy built
//! from the live snapshot's topic paths; right pane shows the
//! selected node's JSON value (or, for branches, a child summary).
//!
//! When a real POSIX/RVF mount adapter ships, it'll publish more
//! paths under `substrate/fs/...` and they'll just appear in the
//! tree alongside everything else. No UI surgery needed — that's
//! the whole point of unifying through the substrate.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use eframe::egui;

use crate::live::{Live, Snapshot};
use crate::shell::desktop::Desktop;
use crate::theming::Tokens;

const TOOLBAR_H: f32 = 32.0;
const LEFT_PANE_W: f32 = 260.0;
const HEADER_H: f32 = 64.0;

/// Files panel state — persisted on `Desktop` so navigation +
/// expansion survive sidebar moves.
#[derive(Default)]
pub struct FilesState {
    /// Set of expanded folder paths (e.g. `"substrate"`,
    /// `"substrate/kernel"`).
    pub expanded: BTreeSet<String>,
    /// Currently-selected path, if any. Drives the right pane.
    pub selected: Option<String>,
}

pub fn show(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    desk: &mut Desktop,
    live: &Arc<Live>,
    _snap: &Snapshot,
) {
    super::paint_heading(ui, rect, "Files · substrate/");

    let body = egui::Rect::from_min_max(egui::pos2(rect.left(), rect.top() + HEADER_H), rect.max);
    let inset = body.shrink2(egui::vec2(24.0, 8.0));

    // Snapshot the substrate once per frame; the tree builds from
    // its topic paths.
    let snapshot = live.substrate_snapshot();
    let tree = build_tree(&snapshot);

    let toolbar_rect = egui::Rect::from_min_max(
        inset.min,
        egui::pos2(inset.right(), inset.top() + TOOLBAR_H),
    );
    paint_toolbar(ui, toolbar_rect, &mut desk.files_state, &tree);

    let panes_top = toolbar_rect.bottom() + 8.0;
    if panes_top >= inset.bottom() {
        return;
    }
    let left_rect = egui::Rect::from_min_max(
        egui::pos2(inset.left(), panes_top),
        egui::pos2(inset.left() + LEFT_PANE_W, inset.bottom()),
    );
    let right_rect = egui::Rect::from_min_max(
        egui::pos2(left_rect.right() + 8.0, panes_top),
        egui::pos2(inset.right(), inset.bottom()),
    );

    paint_left_pane(ui, left_rect, &tree, &mut desk.files_state);
    paint_right_pane(ui, right_rect, &snapshot, &desk.files_state);
}

fn paint_toolbar(ui: &mut egui::Ui, rect: egui::Rect, state: &mut FilesState, tree: &TreeNode) {
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    if child.button("Expand all").clicked() {
        state.expanded.clear();
        collect_branches(tree, &mut state.expanded);
    }
    if child.button("Collapse all").clicked() {
        state.expanded.clear();
    }
    child.add_space(12.0);
    child.label(
        egui::RichText::new(format!("{} topics", count_leaves(tree)))
            .small()
            .color(Tokens::default().text_dim),
    );
}

fn paint_left_pane(ui: &mut egui::Ui, rect: egui::Rect, tree: &TreeNode, state: &mut FilesState) {
    let tokens = Tokens::default();
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, egui::CornerRadius::same(4), tokens.bg_surface);
    painter.rect_stroke(
        rect,
        egui::CornerRadius::same(4),
        egui::Stroke::new(1.0, tokens.stroke_hair),
        egui::epaint::StrokeKind::Inside,
    );

    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect.shrink2(egui::vec2(8.0, 8.0)))
            .layout(egui::Layout::top_down(egui::Align::Min)),
    );

    egui::ScrollArea::vertical()
        .id_salt("files_tree_scroll")
        .auto_shrink([false, false])
        .show(&mut child, |ui| {
            if tree.children.is_empty() {
                ui.label(
                    egui::RichText::new("(no substrate topics yet)")
                        .italics()
                        .color(tokens.text_dim),
                );
                return;
            }
            for (name, sub) in &tree.children {
                paint_tree_node(ui, name, sub, "", state, &tokens);
            }
        });
}

fn paint_tree_node(
    ui: &mut egui::Ui,
    name: &str,
    node: &TreeNode,
    parent_path: &str,
    state: &mut FilesState,
    tokens: &Tokens,
) {
    let path = if parent_path.is_empty() {
        name.to_string()
    } else {
        format!("{parent_path}/{name}")
    };
    let is_branch = !node.children.is_empty();
    let is_open = state.expanded.contains(&path);
    let is_selected = state.selected.as_deref() == Some(&path);

    let glyph = if is_branch {
        if is_open { "▼" } else { "▶" }
    } else {
        "·"
    };
    let depth = parent_path.matches('/').count() + if parent_path.is_empty() { 0 } else { 1 };
    let indent = (depth as f32) * 12.0;

    ui.horizontal(|ui| {
        ui.add_space(indent);
        let label = format!("{glyph}  {name}");
        let text = if is_selected {
            egui::RichText::new(label)
                .monospace()
                .color(tokens.text_primary)
        } else {
            egui::RichText::new(label)
                .monospace()
                .color(tokens.text_secondary)
        };
        if ui.selectable_label(is_selected, text).clicked() {
            state.selected = Some(path.clone());
            if is_branch {
                if is_open {
                    state.expanded.remove(&path);
                } else {
                    state.expanded.insert(path.clone());
                }
            }
        }
    });

    if is_branch && is_open {
        for (child_name, child_node) in &node.children {
            paint_tree_node(ui, child_name, child_node, &path, state, tokens);
        }
    }
}

fn paint_right_pane(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    snapshot: &clawft_substrate::OntologySnapshot,
    state: &FilesState,
) {
    let tokens = Tokens::default();
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, egui::CornerRadius::same(4), tokens.bg_surface);
    painter.rect_stroke(
        rect,
        egui::CornerRadius::same(4),
        egui::Stroke::new(1.0, tokens.stroke_hair),
        egui::epaint::StrokeKind::Inside,
    );

    let inner = rect.shrink2(egui::vec2(8.0, 8.0));
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(inner)
            .layout(egui::Layout::top_down(egui::Align::Min)),
    );

    let Some(path) = state.selected.as_ref() else {
        child.label(
            egui::RichText::new("Select a node from the tree to see its value.")
                .italics()
                .color(tokens.text_dim),
        );
        return;
    };

    child.label(egui::RichText::new(path).monospace());
    child.add_space(4.0);
    child.separator();

    // Direct topic hit?
    let value = snapshot.read(path);
    let Some(value) = value else {
        // No direct value — likely a branch path. Show children.
        let prefix = format!("{path}/");
        let kids: Vec<&String> = snapshot
            .iter()
            .map(|(k, _)| k)
            .filter(|k| k.starts_with(&prefix) || k.as_str() == path)
            .collect();
        if kids.is_empty() {
            child.label(
                egui::RichText::new("(no value at this path)")
                    .italics()
                    .color(tokens.text_dim),
            );
            return;
        }
        child.label(
            egui::RichText::new(format!("{} child topic(s) under this folder:", kids.len()))
                .small()
                .color(tokens.text_dim),
        );
        child.add_space(4.0);
        egui::ScrollArea::vertical()
            .id_salt("files_kids_scroll")
            .auto_shrink([false, false])
            .show(&mut child, |ui| {
                for k in kids {
                    ui.monospace(k);
                }
            });
        return;
    };

    let pretty = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
    egui::ScrollArea::both()
        .id_salt("files_value_scroll")
        .auto_shrink([false, false])
        .show(&mut child, |ui| {
            ui.add(
                egui::TextEdit::multiline(&mut pretty.as_str())
                    .font(egui::TextStyle::Monospace)
                    .desired_width(f32::INFINITY)
                    .code_editor()
                    .interactive(false),
            );
        });
}

/// Tree built from the substrate's topic paths. A leaf is a path that
/// exactly matches a topic; a branch has children. A path can be both
/// (a topic with sub-topics under it) — the value lookup happens via
/// `snapshot.read(path)` regardless.
#[derive(Default)]
struct TreeNode {
    children: BTreeMap<String, TreeNode>,
}

fn build_tree(snapshot: &clawft_substrate::OntologySnapshot) -> TreeNode {
    let mut root = TreeNode::default();
    for (path, _) in snapshot.iter() {
        let mut cur = &mut root;
        for seg in path.split('/').filter(|s| !s.is_empty()) {
            cur = cur.children.entry(seg.to_string()).or_default();
        }
    }
    root
}

fn count_leaves(node: &TreeNode) -> usize {
    if node.children.is_empty() {
        1
    } else {
        node.children.values().map(count_leaves).sum()
    }
}

fn collect_branches(node: &TreeNode, out: &mut BTreeSet<String>) {
    fn walk(node: &TreeNode, prefix: &str, out: &mut BTreeSet<String>) {
        for (name, sub) in &node.children {
            let path = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            if !sub.children.is_empty() {
                out.insert(path.clone());
                walk(sub, &path, out);
            }
        }
    }
    walk(node, "", out);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live::Connection;
    use serde_json::Value;

    fn run_show(snap: Snapshot) {
        let ctx = egui::Context::default();
        let mut desk = Desktop::default();
        let live = Live::spawn();
        let _ = ctx.run_ui(Default::default(), |ui| {
            egui::CentralPanel::default().show_inside(ui, |ui| {
                let rect = ui.max_rect();
                show(ui, rect, &mut desk, &live, &snap);
            });
        });
    }

    #[test]
    fn show_does_not_panic_with_default_snapshot() {
        run_show(Snapshot::default());
    }

    #[test]
    fn show_does_not_panic_when_connected() {
        let snap = Snapshot {
            connection: Connection::Connected,
            ..Default::default()
        };
        run_show(snap);
    }

    #[test]
    fn build_tree_groups_paths_by_segment() {
        let snap = clawft_substrate::OntologySnapshot::empty()
            .with("substrate/kernel/status", Value::String("ok".into()))
            .with("substrate/kernel/processes", Value::Array(vec![]))
            .with("substrate/mesh/status", Value::Object(Default::default()));
        let tree = build_tree(&snap);
        let substrate = tree.children.get("substrate").expect("substrate branch");
        assert!(substrate.children.contains_key("kernel"));
        assert!(substrate.children.contains_key("mesh"));
        let kernel = substrate.children.get("kernel").unwrap();
        assert!(kernel.children.contains_key("status"));
        assert!(kernel.children.contains_key("processes"));
    }

    #[test]
    fn count_leaves_matches_topic_count() {
        let snap = clawft_substrate::OntologySnapshot::empty()
            .with("a/b/c", Value::Null)
            .with("a/b/d", Value::Null)
            .with("a/e", Value::Null);
        let tree = build_tree(&snap);
        assert_eq!(count_leaves(&tree), 3);
    }

    #[test]
    fn collect_branches_gathers_intermediate_folders() {
        let snap = clawft_substrate::OntologySnapshot::empty()
            .with("a/b/c", Value::Null)
            .with("a/d", Value::Null);
        let tree = build_tree(&snap);
        let mut out = BTreeSet::new();
        collect_branches(&tree, &mut out);
        assert!(out.contains("a"));
        assert!(out.contains("a/b"));
        assert!(!out.contains("a/d")); // leaf, not branch
    }

    /// Both panes build a `ScrollArea` inside a `new_child` of the same parent
    /// Ui. Without an explicit `id_salt` they get the same id, and egui draws
    /// "First use of ScrollArea ID ..." over the panes (debug builds) and
    /// shares one scroll offset between the tree and the detail pane. The
    /// right pane only builds its scroll area once a node is selected.
    #[test]
    fn tree_and_detail_panes_do_not_share_a_scroll_area_id() {
        let ctx = egui::Context::default();
        let mut desk = Desktop::default();
        let live = Live::spawn();
        desk.files_state.expanded.insert("substrate".into());
        desk.files_state.selected = Some("substrate".into());
        let snap = Snapshot::default();
        let mut text = String::new();
        // A few frames: egui reports the clash on the frame after first use.
        for _ in 0..3 {
            let out = ctx.run_ui(Default::default(), |ui| {
                egui::CentralPanel::default().show_inside(ui, |ui| {
                    let rect = ui.max_rect();
                    show(ui, rect, &mut desk, &live, &snap);
                });
            });
            for clipped in &out.shapes {
                collect_text(&clipped.shape, &mut text);
            }
        }
        assert!(
            !text.contains("ScrollArea ID"),
            "egui flagged a duplicate ScrollArea id: {text}"
        );
    }

    fn collect_text(shape: &egui::epaint::Shape, out: &mut String) {
        match shape {
            egui::epaint::Shape::Text(t) => out.push_str(t.galley.text()),
            egui::epaint::Shape::Vec(v) => v.iter().for_each(|s| collect_text(s, out)),
            _ => {}
        }
    }
}
