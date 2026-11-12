//! Left-hand tree widget for the Explorer.
//!
//! Walks the substrate namespace by calling `substrate.list` with a
//! `{prefix, depth: 1}` request whenever the user expands a node.
//! Children are cached in [`Explorer::tree_children`](super::Explorer)
//! so navigation stays instant after the first fetch; the owning
//! Explorer also re-polls expanded prefixes on a slow tick so newly
//! appearing paths show up without the user re-clicking.

use eframe::egui;
use serde_json::Value;

use super::{ACTIVITY_WINDOW, Explorer, TreeNode};

/// The substrate root — children at depth-1 below this are
/// **node-ids** (32-hex `SHA-256(pubkey)[..16]` ids per the node-identity
/// gate). The Explorer's tree starts here; there is no synthetic
/// header above it. The `ui.heading("Substrate")` painted by
/// [`Explorer::show`](crate::explorer::Explorer::show) already
/// frames the panel.
pub const ROOT_PREFIX: &str = "substrate";

/// Tree filter chip row state. Persists within a session so the user's
/// triage choice survives panel toggle / re-mount. WEFT-270.
///
/// Filters are AND-combined: a row is shown if it passes every active
/// filter. The default state is "show everything" (no filters active)
/// so the tree behaves identically to the pre-filter pane until the
/// user opts in.
#[derive(Debug, Clone, Default)]
pub struct TreeFilters {
    /// Substring match against the path's last segment (case-
    /// insensitive). Empty = inactive.
    pub name_query: String,
    /// Show only paths that have published a value within
    /// [`crate::explorer::ACTIVITY_WINDOW`]. Default off.
    pub active_only: bool,
    /// Show only the well-known sensor sub-tree (any path containing
    /// `/sensor/`). Default off — `false` means "no sensor filter".
    pub sensors_only: bool,
    /// Hide leaves (`has_value && child_count == 0`). Useful when
    /// scanning the structural shape of the substrate. Default off.
    pub hide_leaves: bool,
}

impl TreeFilters {
    /// True if any filter is active. Used to render a small "filtered"
    /// hint next to the chip row.
    pub fn any_active(&self) -> bool {
        !self.name_query.is_empty() || self.active_only || self.sensors_only || self.hide_leaves
    }

    /// Decide whether `child` passes the active filters. The active-
    /// only filter consults the parent Explorer's activity map, which
    /// is why this lives on a method that takes `&Explorer`.
    fn passes(&self, ex: &Explorer, child: &TreeNode) -> bool {
        let last = last_segment(&child.path);
        if !self.name_query.is_empty() {
            let q = self.name_query.to_ascii_lowercase();
            if !last.to_ascii_lowercase().contains(&q) {
                return false;
            }
        }
        if self.sensors_only && !child.path.contains("/sensor/") && !child.path.ends_with("/sensor")
        {
            return false;
        }
        if self.hide_leaves && child.has_value && child.child_count == 0 {
            return false;
        }
        if self.active_only {
            // Activity dot lives on the path itself — a row is "active"
            // if its full path has a recent activity stamp.
            let active = ex
                .activity
                .get(&child.path)
                .map(|t| t.elapsed() <= ACTIVITY_WINDOW)
                .unwrap_or(false);
            if !active {
                return false;
            }
        }
        true
    }
}

/// Render the left tree pane. Returns the path newly selected this
/// frame (if any) so the caller can swap subscriptions atomically.
///
/// Layout: each top-level row is one node in the mesh; expanding a
/// node reveals its substrate subtree (`sensor/`, `health/`, `meta/`,
/// `kernel/` for kernel-class nodes, etc.). Mesh-canonical paths
/// under `substrate/_derived/...` appear as a sibling top-level row
/// when populated.
pub fn paint(ui: &mut egui::Ui, ex: &mut Explorer) -> Option<String> {
    let mut newly_selected: Option<String> = None;
    let mut to_request: Vec<String> = Vec::new();

    // Auto-request the root listing if we don't have a cache entry
    // yet. Also keep the root marked expanded so the slow-tick
    // re-list fires for newly-arrived nodes.
    if !ex.tree_children.contains_key(ROOT_PREFIX) {
        to_request.push(ROOT_PREFIX.to_string());
    }
    ex.expanded.insert(ROOT_PREFIX.to_string());

    // WEFT-270: chip row above the tree — type/status filters that
    // narrow what gets rendered. Painted outside the ScrollArea so it
    // stays pinned even when the tree scrolls.
    paint_filter_chips(ui, &mut ex.tree_filters);
    ui.separator();

    egui::ScrollArea::both()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            if let Some(msg) = &ex.backend_hint {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new("⚠ backend")
                            .small()
                            .color(crate::theming::palette::ACCENT_AMBER),
                    )
                    .on_hover_text(msg);
                });
                ui.separator();
            }

            match ex.tree_children.get(ROOT_PREFIX).cloned() {
                Some(kids) if kids.is_empty() => {
                    ui.label(
                        egui::RichText::new("(no nodes registered yet)")
                            .italics()
                            .small()
                            .color(crate::theming::palette::TEXT_LABEL_FAINT),
                    );
                }
                Some(kids) => {
                    let filters = ex.tree_filters.clone();
                    let mut shown = 0_usize;
                    for child in kids.iter() {
                        if !filters.passes(ex, child) {
                            continue;
                        }
                        shown += 1;
                        let label = last_segment(&child.path);
                        let is_leaf = child.has_value && child.child_count == 0;
                        render_node(
                            ui,
                            ex,
                            &child.path,
                            label,
                            0,
                            is_leaf,
                            &mut newly_selected,
                            &mut to_request,
                        );
                    }
                    if shown == 0 && filters.any_active() {
                        ui.label(
                            egui::RichText::new("(no rows match the current filter)")
                                .italics()
                                .small()
                                .color(crate::theming::palette::TEXT_LABEL_FAINT),
                        );
                    }
                }
                None => {
                    ui.label(
                        egui::RichText::new("loading…")
                            .italics()
                            .small()
                            .color(crate::theming::palette::TEXT_LABEL_FAINT),
                    );
                }
            }
        });

    // After the borrow of ex is released, enqueue any list requests
    // gathered during the walk. We can't do this inside the walk
    // because `to_request.push(...)` needs `&mut Vec` while we're
    // holding `&mut Explorer`.
    for prefix in to_request {
        ex.queue_list(prefix);
    }

    newly_selected
}

/// Render a single tree row and, if `expanded`, its children.
///
/// * `prefix` — the prefix we asked the backend to list (`""` for root).
/// * `display` — the label to show for this row.
/// * `depth` — indentation depth.
/// * `is_leaf` — true when `substrate.list` reported this child with
///   `has_value: true && child_count == 0`. Leaves render without an
///   expand caret because expanding them would fire a
///   `substrate.list { prefix: <leaf-path> }` whose kernel-side
///   contract returns the leaf itself as a child (see
///   `substrate_service::list` and the test
///   `list_leaf_prefix_returns_itself`). Recursing into
///   `tree_children["<leaf>"] == [{path:"<leaf>",...}]` would render
///   the same row inside itself indefinitely and overflow the stack
///   on WASM.
#[allow(clippy::too_many_arguments)]
fn render_node(
    ui: &mut egui::Ui,
    ex: &mut Explorer,
    prefix: &str,
    display: &str,
    depth: usize,
    is_leaf: bool,
    newly_selected: &mut Option<String>,
    to_request: &mut Vec<String>,
) {
    let is_expanded = ex.expanded.contains(prefix);
    let is_selected = ex.selected.as_deref() == Some(prefix);
    let has_activity = path_is_active(ex, prefix);

    // Indent by depth.
    let indent = (depth as f32) * 14.0;

    ui.horizontal(|ui| {
        ui.add_space(indent);

        if is_leaf {
            // Pad to the same column width the caret button would
            // occupy so leaf rows stay aligned with their siblings.
            // The caret button is rendered with `frame(false)` so its
            // width is just the glyph; ~14 px matches "▸" in the
            // current theme.
            ui.add_space(14.0);
        } else {
            // Expand/collapse arrow. Clicking fires a list request if
            // we haven't cached children for this prefix yet.
            let arrow = if is_expanded { "▾" } else { "▸" };
            let arrow_resp =
                ui.add(egui::Button::new(egui::RichText::new(arrow).monospace()).frame(false));
            if arrow_resp.clicked() {
                if is_expanded {
                    ex.expanded.remove(prefix);
                } else {
                    ex.expanded.insert(prefix.to_string());
                    // Always re-request on expand — children may have
                    // changed since last time.
                    to_request.push(prefix.to_string());
                }
            }
        }

        // Activity dot (● if updated in the last ACTIVITY_WINDOW, ○ otherwise).
        let (dot, dot_color) = if has_activity {
            ("●", crate::theming::palette::SHELL_OK)
        } else {
            ("○", crate::theming::palette::TREE_OFFLINE_DOT)
        };
        ui.label(
            egui::RichText::new(dot)
                .monospace()
                .small()
                .color(dot_color),
        );

        // Label / selectable. Top-level rows (depth 0 = node-ids)
        // render slightly bolder so the eye anchors on the node
        // boundary; deeper rows are plain monospace.
        let label_text = if depth == 0 {
            egui::RichText::new(display).monospace().strong()
        } else {
            egui::RichText::new(display).monospace()
        };
        let resp = ui.selectable_label(is_selected, label_text);
        if resp.clicked() {
            if !is_selected {
                *newly_selected = Some(prefix.to_string());
            }
            ex.selected = Some(prefix.to_string());
        }
    });

    if !is_expanded {
        return;
    }

    // Render cached children, if any. If we have no cache entry yet,
    // a list request has already been queued above.
    let children = ex.tree_children.get(prefix).cloned();
    match children {
        Some(kids) if kids.is_empty() => {
            ui.horizontal(|ui| {
                ui.add_space(indent + 24.0);
                ui.label(
                    egui::RichText::new("(empty)")
                        .italics()
                        .small()
                        .color(crate::theming::palette::TEXT_LABEL_FAINT),
                );
            });
        }
        Some(kids) => {
            for child in kids {
                // Defense in depth against the kernel's "list of a leaf
                // returns the leaf itself as a child" contract: skip any
                // child whose path equals our own prefix. Without this
                // guard the row would re-render itself inside itself.
                // The is_leaf branch in the parent's caret rendering
                // suppresses the expand affordance that triggers this
                // case in the first place; this is the second line.
                if child.path == prefix {
                    continue;
                }
                let child_label = last_segment(&child.path);
                let is_leaf = child.has_value && child.child_count == 0;
                render_node(
                    ui,
                    ex,
                    &child.path,
                    child_label,
                    depth + 1,
                    is_leaf,
                    newly_selected,
                    to_request,
                );
            }
        }
        None => {
            ui.horizontal(|ui| {
                ui.add_space(indent + 24.0);
                ui.label(
                    egui::RichText::new("loading…")
                        .italics()
                        .small()
                        .color(crate::theming::palette::TEXT_LABEL_FAINT),
                );
            });
        }
    }
}

/// Is this path currently "live"? (delta within the last window.)
fn path_is_active(ex: &Explorer, path: &str) -> bool {
    let Some(t) = ex.activity.get(path) else {
        return false;
    };
    t.elapsed() <= ACTIVITY_WINDOW
}

/// Last path segment used as a tree label (`substrate/sensor/mic` → `mic`).
fn last_segment(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Paint the WEFT-270 chip row above the substrate tree. Mutates the
/// passed `TreeFilters` directly — the caller stores the state on the
/// owning [`Explorer`] so changes persist within the session.
///
/// Layout: a single horizontal row with a search field, three toggle
/// chips, and a "clear" pill that appears only when at least one
/// filter is active. The row deliberately renders quietly when no
/// filters are set so users who don't need it don't see chrome.
fn paint_filter_chips(ui: &mut egui::Ui, filters: &mut TreeFilters) {
    ui.horizontal_wrapped(|ui| {
        ui.label(
            egui::RichText::new("filter:")
                .small()
                .color(crate::theming::palette::TEXT_LABEL_FAINT),
        );

        // Name search: small TextEdit. ~14ch wide so the row still fits
        // a typical 220 px tree column.
        let name_resp = ui.add(
            egui::TextEdit::singleline(&mut filters.name_query)
                .hint_text("name…")
                .desired_width(96.0),
        );
        name_resp.on_hover_text("Filter rows by substring of the last path segment.");

        // Toggle chips. `Button::selectable` gives us the inverted
        // background that reads as "this filter is on".
        if ui
            .add(egui::Button::selectable(filters.active_only, "active"))
            .on_hover_text("Show only paths that have published recently.")
            .clicked()
        {
            filters.active_only = !filters.active_only;
        }
        if ui
            .add(egui::Button::selectable(filters.sensors_only, "sensors"))
            .on_hover_text("Show only paths under .../sensor/...")
            .clicked()
        {
            filters.sensors_only = !filters.sensors_only;
        }
        if ui
            .add(egui::Button::selectable(filters.hide_leaves, "no leaves"))
            .on_hover_text("Hide leaf paths to scan structure only.")
            .clicked()
        {
            filters.hide_leaves = !filters.hide_leaves;
        }

        if filters.any_active()
            && ui
                .small_button("clear")
                .on_hover_text("Clear all active filters.")
                .clicked()
        {
            *filters = TreeFilters::default();
        }
    });
}

/// Parse a `substrate.list` response into a Vec of children. Tolerant
/// of shape drift — returns an empty Vec when the response doesn't
/// match the expected envelope.
pub fn parse_list_response(v: &Value) -> Vec<TreeNode> {
    let Some(children) = v.get("children").and_then(|c| c.as_array()) else {
        return Vec::new();
    };
    children
        .iter()
        .filter_map(|c| {
            let path = c.get("path")?.as_str()?.to_string();
            let has_value = c
                .get("has_value")
                .and_then(|b| b.as_bool())
                .unwrap_or(false);
            let child_count = c.get("child_count").and_then(|n| n.as_u64()).unwrap_or(0);
            Some(TreeNode {
                path,
                has_value,
                child_count,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_list_response_happy_path() {
        let v = json!({
            "children": [
                { "path": "substrate/sensor/mic", "has_value": true, "child_count": 0 },
                { "path": "substrate/sensor/tof", "has_value": true, "child_count": 0 },
            ]
        });
        let kids = parse_list_response(&v);
        assert_eq!(kids.len(), 2);
        assert_eq!(kids[0].path, "substrate/sensor/mic");
        assert!(kids[0].has_value);
    }

    #[test]
    fn parse_list_response_empty() {
        let v = json!({ "children": [] });
        assert!(parse_list_response(&v).is_empty());
    }

    #[test]
    fn parse_list_response_garbage_returns_empty() {
        let v = json!({ "oops": 1 });
        assert!(parse_list_response(&v).is_empty());
    }

    #[test]
    fn last_segment_basic() {
        assert_eq!(last_segment("substrate/sensor/mic"), "mic");
        assert_eq!(last_segment("root"), "root");
        assert_eq!(last_segment(""), "");
    }

    #[test]
    fn tree_filters_default_passes_everything() {
        // WEFT-270 acceptance: a fresh TreeFilters has no filters set,
        // so the tree behaves identically to the pre-filter pane.
        let f = TreeFilters::default();
        assert!(!f.any_active());
        // We can't easily build an Explorer here without paint side
        // effects, so the integration of `passes` is exercised via the
        // dedicated tests below using a minimal Explorer.
    }

    #[test]
    fn tree_filters_name_query_substring() {
        let f = TreeFilters {
            name_query: "MIC".into(),
            ..Default::default()
        };
        assert!(f.any_active());
        // Build a minimal Explorer with no activity to drive `passes`.
        let ex = Explorer::default();
        let mic = TreeNode {
            path: "substrate/n-bfc4cd/sensor/mic".into(),
            has_value: true,
            child_count: 0,
        };
        let tof = TreeNode {
            path: "substrate/n-bfc4cd/sensor/tof".into(),
            has_value: true,
            child_count: 0,
        };
        // Search is case-insensitive — "MIC" matches "mic".
        assert!(f.passes(&ex, &mic));
        assert!(!f.passes(&ex, &tof));
    }

    #[test]
    fn tree_filters_sensors_only() {
        let f = TreeFilters {
            sensors_only: true,
            ..Default::default()
        };
        let ex = Explorer::default();
        let mic = TreeNode {
            path: "substrate/n-bfc4cd/sensor/mic".into(),
            has_value: true,
            child_count: 0,
        };
        let health = TreeNode {
            path: "substrate/n-bfc4cd/health".into(),
            has_value: true,
            child_count: 0,
        };
        assert!(f.passes(&ex, &mic));
        assert!(!f.passes(&ex, &health));
    }

    #[test]
    fn tree_filters_hide_leaves() {
        let f = TreeFilters {
            hide_leaves: true,
            ..Default::default()
        };
        let ex = Explorer::default();
        let leaf = TreeNode {
            path: "substrate/n-bfc4cd/health".into(),
            has_value: true,
            child_count: 0,
        };
        let inner = TreeNode {
            path: "substrate/n-bfc4cd".into(),
            has_value: false,
            child_count: 4,
        };
        assert!(!f.passes(&ex, &leaf));
        assert!(f.passes(&ex, &inner));
    }

    #[test]
    fn tree_filters_active_only_blocks_quiet_paths() {
        let f = TreeFilters {
            active_only: true,
            ..Default::default()
        };
        let ex = Explorer::default();
        // Nothing's been recorded as active — the "active only" filter
        // should hide every row.
        let mic = TreeNode {
            path: "substrate/n-bfc4cd/sensor/mic".into(),
            has_value: true,
            child_count: 0,
        };
        assert!(!f.passes(&ex, &mic));
    }

    #[test]
    fn tree_filters_active_only_admits_recently_active_path() {
        use ::web_time::Instant;
        let f = TreeFilters {
            active_only: true,
            ..Default::default()
        };
        let mut ex = Explorer::default();
        let path = "substrate/n-bfc4cd/sensor/mic".to_string();
        ex.activity.insert(path.clone(), Instant::now());
        let mic = TreeNode {
            path,
            has_value: true,
            child_count: 0,
        };
        assert!(f.passes(&ex, &mic));
    }

    #[test]
    fn parse_list_response_preserves_leaf_self_reference() {
        // The kernel's `substrate.list` returns the prefix itself as the
        // sole child when the prefix is a leaf with a value (see
        // `substrate_service::list` and the test `list_leaf_prefix_returns_itself`
        // in clawft-kernel). The parser MUST preserve that shape — the
        // recursion guard lives in `render_node`, not here. If we filtered
        // self-references at parse time, callers that use `list` to ask
        // "is this a leaf?" would silently lose their answer.
        let v = json!({
            "children": [
                { "path": "substrate/n-bfc4cd/sensor/mic/pcm_chunk",
                  "has_value": true, "child_count": 0 }
            ]
        });
        let kids = parse_list_response(&v);
        assert_eq!(kids.len(), 1);
        assert_eq!(kids[0].path, "substrate/n-bfc4cd/sensor/mic/pcm_chunk");
        assert!(kids[0].has_value);
        assert_eq!(kids[0].child_count, 0);
    }

    /// An off filter toggle must still read as a control: it paints a frame
    /// with a visible stroke.
    #[test]
    fn off_filter_toggle_paints_a_frame() {
        let mut filters = TreeFilters::default();
        let size = egui::vec2(600.0, 80.0);
        let shapes = crate::apps::layout_test::run_shapes(size, |_| vec![], |ui, _| {
            paint_filter_chips(ui, &mut filters);
        });
        // Locate the "sensors" label, then a stroked rect that encloses it.
        let mut label = None;
        for s in &shapes {
            if let egui::Shape::Text(t) = s {
                if t.galley.text() == "sensors" {
                    label = Some(egui::Rect::from_min_size(t.pos, t.galley.size()).center());
                }
            }
        }
        let label = label.expect("sensors toggle painted");
        let framed = shapes.iter().any(|s| match s {
            egui::Shape::Rect(r) => {
                r.rect.contains(label)
                    && r.rect.width() < 120.0
                    && r.stroke.width > 0.0
                    && r.stroke.color.a() > 0
            }
            _ => false,
        });
        assert!(framed, "an off toggle must paint a stroked frame");
    }
}
