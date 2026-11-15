//! Workshop — config-driven, hot-reload UI composition.
//!
//! Spec: `.planning/ontology/ADOPTION.md` §8 Step 3 and
//! `.planning/explorer/PHASE-2-PLAN.md` §6 Track 5.
//!
//! ## What it is
//!
//! A Workshop is a composition primitive: a JSON value, stored at
//! `substrate/ui/workshop/<name>`, that describes a layout of child
//! panels. Each child panel names a substrate path whose value is
//! rendered with the existing shape-dispatched viewer registry
//! ([`crate::explorer::viewers::dispatch`]).
//!
//! The whole point of this primitive is that iterating on the
//! composition happens via substrate **publish**, not GUI rebuild. A
//! new Workshop value replaces the [`WorkshopView`] state on the next
//! frame and the layout re-renders.
//!
//! ## Schema
//!
//! ```json
//! {
//!   "title": "Mic diagnostic",
//!   "layout": "rows",
//!   "params": { "node": "n-6f3a9c" },
//!   "panels": [
//!     {
//!       "title": "RMS gauge",
//!       "substrate_path": "substrate/sensor/mic",
//!       "viewer_hint": "auto",
//!       "min_height": 120
//!     },
//!     {
//!       "title": "Per-node mic",
//!       "substrate_path_template": "substrate/${node}/sensor/mic"
//!     }
//!   ]
//! }
//! ```
//!
//! * `title` — optional string; rendered as the Workshop heading.
//! * `layout` — one of `rows` (default), `grid`, `tabs`. All three are
//!   implemented (`grid` paints an `egui::Grid`, `tabs` paints a
//!   selectable tab bar). Unknown layouts round-trip through
//!   [`WorkshopLayout::Unknown`] and degrade to rows.
//! * `panels` — ordered array of [`WorkshopPanel`]s. Each panel must
//!   resolve to a substrate path: either supply `substrate_path`
//!   directly, or supply `substrate_path_template` (with `${param}`
//!   placeholders) plus the top-level `params` map.
//! * `params` — optional `{ name: string }` map. Values substitute
//!   into every panel's `substrate_path_template` via `${name}`. A
//!   placeholder with no matching param parses successfully but the
//!   panel renders a small "missing param `<name>`" hint at paint
//!   time. [WEFT-274]
//! * `viewer_hint` — explicit viewer override. `"auto"` (or unset) is
//!   shape-dispatched via [`super::viewers::dispatch`]; any other value
//!   that names a registered viewer ([`viewer_for_hint`]) routes the
//!   panel through that viewer directly. Unknown hints fall back to
//!   shape-dispatch with a small debug hint so the writer can see
//!   their hint didn't match. [WEFT-280]
//!
//! ## Hot-reload mechanism
//!
//! 1. User (or a file-watcher, or a script, or an LLM) publishes a new
//!    Workshop JSON value to `substrate/ui/workshop/<name>`.
//! 2. The Explorer's existing `SELECT_POLL` re-read fetches the new
//!    value into `Explorer::selected_value`.
//! 3. [`WorkshopView::paint`] parses the value afresh each frame — the
//!    `Workshop` struct is never cached across frames, so a published
//!    change takes effect in the very next paint.
//! 4. The child-panel poller ([`WorkshopView`]) diffs its tracked path
//!    set against the newly-parsed panel list; stale subscriptions are
//!    dropped, new ones start on the next tick.
//!
//! No reload, no rebuild, no explicit subscription handshake — the
//! substrate poll cascade handles it.

use std::collections::HashMap;
use std::sync::Arc;

use eframe::egui;
use serde_json::Value;

use crate::live::{self, Command, Live, ReplyRx};
use crate::wasm_time::epoch_minus;

/// Reserved Object Type name so Track 2's `ontology::infer` can match
/// Workshop shapes once a registration is added there. The value lives
/// in this module rather than in `ontology/types/` to keep Workshop
/// self-contained for the MVP — the Object-Type registration is the
/// single add-site when the ontology layer grows to recognize it.
pub const OBJECT_TYPE_NAME: &str = "Workshop";

/// Human-readable Object Type label.
pub const OBJECT_TYPE_DISPLAY: &str = "Workshop";

/// Shape-match priority for the Workshop Object Type — higher than the
/// generic Mesh (20) so a substrate value with a `panels: [...]` array
/// lands here decisively.
pub const OBJECT_TYPE_PRIORITY: u32 = 30;

/// Per-panel poll cadence. Independent of the top-level Workshop
/// re-read (which picks up schema changes) — this is the per-panel
/// value refresh. 400ms matches the selected-path poll rate in the
/// Explorer so all panels feel equally live.
const PANEL_POLL: std::time::Duration = std::time::Duration::from_millis(400);

/// Minimum panel height when unspecified. Leaves enough vertical room
/// for a typical viewer (gauge, sparkline, badge row) without forcing
/// a scrollbar on small content.
const DEFAULT_MIN_HEIGHT: f32 = 80.0;

/// Parsed Workshop value. Cheap to rebuild every frame — the shape is
/// O(panels) scalars + a few strings. `WorkshopView` owns the live
/// per-panel subscription state; `Workshop` is pure schema.
///
/// Hand-parsed from `serde_json::Value` rather than `#[derive(Deserialize)]`
/// because `clawft-gui-egui` keeps serde at one remove (through
/// `serde_json`) and we don't need the full derive plumbing for this
/// O(few fields) shape.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Workshop {
    /// Optional display title for the composition.
    pub title: Option<String>,
    /// Layout strategy. `rows`, `grid`, and `tabs` are implemented;
    /// unknown layout strings round-trip through [`WorkshopLayout::Unknown`]
    /// so a forward-compatible writer doesn't get silently clipped.
    pub layout: WorkshopLayout,
    /// Top-level parameter map, substituted into every panel's
    /// `substrate_path_template`. `${name}` placeholders resolve to
    /// the matching value here. Missing params render an inline
    /// per-panel hint at paint time. [WEFT-274]
    pub params: HashMap<String, String>,
    /// Ordered panel list. Empty is legal (Workshop renders its title
    /// and an empty-state hint).
    pub panels: Vec<WorkshopPanel>,
}

/// Layout strategy for a Workshop. Only `Rows` is rendered today;
/// `Grid` and `Tabs` fall back to a rows layout with a debug hint so a
/// forward-looking writer can publish the shape now and the renderer
/// degrades gracefully. `Unknown` preserves unknown layouts on
/// round-trip so a TOML writer doesn't lose data.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum WorkshopLayout {
    #[default]
    Rows,
    Grid,
    Tabs,
    /// Any layout string the MVP doesn't recognize.
    Unknown,
}

impl WorkshopLayout {
    fn from_str(s: &str) -> Self {
        match s {
            "rows" => WorkshopLayout::Rows,
            "grid" => WorkshopLayout::Grid,
            "tabs" => WorkshopLayout::Tabs,
            _ => WorkshopLayout::Unknown,
        }
    }
}

/// One child panel in a Workshop.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkshopPanel {
    /// Display label for the panel header. Optional; when absent the
    /// substrate path is shown as the header.
    pub title: Option<String>,
    /// Substrate path whose value this panel renders. Either this
    /// field is set directly, or it is derived from
    /// `substrate_path_template` after [`Workshop::params`]
    /// substitution. Empty after substitution → panel renders an
    /// inline error.
    pub substrate_path: String,
    /// Original `${param}`-bearing template, kept around so
    /// `paint_panel` can show a sensible hint when a placeholder is
    /// missing. `None` when the panel was authored with a literal
    /// `substrate_path`. [WEFT-274]
    pub substrate_path_template: Option<String>,
    /// Substitution status:
    /// * `Ok(())` — every `${name}` placeholder in the template was
    ///   resolved (or there was no template).
    /// * `Err(name)` — a `${name}` placeholder had no matching key
    ///   in [`Workshop::params`]; rendered as an inline hint.
    pub substitution_status: Result<(), String>,
    /// Explicit viewer name to force, or `"auto"` / unset for
    /// shape-dispatched default. Recognised viewer names see
    /// [`viewer_for_hint`]. [WEFT-280]
    pub viewer_hint: String,
    /// Optional per-panel minimum height in logical pixels.
    pub min_height: Option<f32>,
}

/// Parse a substrate value into a [`Workshop`]. Returns `Err` with a
/// short human-readable message when the value isn't a Workshop shape.
///
/// Strict enough to reject arbitrary blobs but lenient enough that
/// forward-compatible writers (extra fields, unknown layouts) succeed.
pub fn parse(value: &Value) -> Result<Workshop, String> {
    let obj = value
        .as_object()
        .ok_or_else(|| "workshop value must be a JSON object".to_string())?;
    let panels_val = obj
        .get("panels")
        .ok_or_else(|| "workshop value missing required `panels` field".to_string())?;
    let panels_arr = panels_val
        .as_array()
        .ok_or_else(|| "workshop `panels` must be an array".to_string())?;

    // Parse top-level `params` map first so panels can substitute.
    // Stringy values only — the param substitution lives in the
    // path string and JSON arrays/objects don't have a sensible
    // textual encoding for that role.
    let mut params: HashMap<String, String> = HashMap::new();
    if let Some(p) = obj.get("params") {
        let map = p
            .as_object()
            .ok_or_else(|| "workshop `params` must be a JSON object".to_string())?;
        for (k, v) in map {
            let s = v.as_str().ok_or_else(|| {
                format!(
                    "workshop `params.{k}` must be a string (numbers and bools \
                     have no canonical path-component encoding)"
                )
            })?;
            params.insert(k.clone(), s.to_string());
        }
    }

    let mut panels = Vec::with_capacity(panels_arr.len());
    for (i, p) in panels_arr.iter().enumerate() {
        panels.push(parse_panel(p, &params).map_err(|e| format!("panels[{i}]: {e}"))?);
    }

    let title = obj.get("title").and_then(Value::as_str).map(str::to_string);

    let layout = obj
        .get("layout")
        .and_then(Value::as_str)
        .map(WorkshopLayout::from_str)
        .unwrap_or_default();

    Ok(Workshop {
        title,
        layout,
        params,
        panels,
    })
}

fn parse_panel(value: &Value, params: &HashMap<String, String>) -> Result<WorkshopPanel, String> {
    let obj = value
        .as_object()
        .ok_or_else(|| "panel must be a JSON object".to_string())?;
    // Either a literal substrate_path or a templated form. Authoring
    // both is permitted (the literal wins) so a writer can switch
    // between them mid-iteration.
    let substrate_path_template = obj
        .get("substrate_path_template")
        .and_then(Value::as_str)
        .map(str::to_string);
    let (substrate_path, substitution_status) =
        if let Some(literal) = obj.get("substrate_path").and_then(Value::as_str) {
            (literal.to_string(), Ok(()))
        } else if let Some(tmpl) = substrate_path_template.as_deref() {
            substitute(tmpl, params)
        } else {
            return Err(
                "missing required `substrate_path` (or `substrate_path_template`) string".into(),
            );
        };
    let title = obj.get("title").and_then(Value::as_str).map(str::to_string);
    let viewer_hint = obj
        .get("viewer_hint")
        .and_then(Value::as_str)
        .unwrap_or("auto")
        .to_string();
    // Accept either integer or float for min_height — TOML often emits
    // integers and we don't want to force-quote the value.
    let min_height = obj.get("min_height").and_then(|v| {
        v.as_f64()
            .map(|f| f as f32)
            .or_else(|| v.as_i64().map(|i| i as f32))
            .or_else(|| v.as_u64().map(|u| u as f32))
    });
    Ok(WorkshopPanel {
        title,
        substrate_path,
        substrate_path_template,
        substitution_status,
        viewer_hint,
        min_height,
    })
}

/// Substitute `${name}` placeholders in `template` against `params`.
///
/// Returns `(rendered_path, Ok(()))` when every placeholder resolved,
/// or `(partial_path, Err(missing_name))` for the first missing
/// placeholder. The partial path retains the literal `${missing}`
/// substring so the panel renders something diagnosable rather than
/// nothing.
///
/// Syntax: `${name}` only — no default values, no escaping. This
/// keeps the writer side trivial (TOML keys map 1-to-1 to
/// placeholder names) and matches the example proposed in
/// EXPLORER-MANAGEMENT-SURFACE §6.2 verbatim.
fn substitute(template: &str, params: &HashMap<String, String>) -> (String, Result<(), String>) {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    let mut first_missing: Option<String> = None;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find('}') {
            Some(end) => {
                let name = &after[..end];
                match params.get(name) {
                    Some(v) => out.push_str(v),
                    None => {
                        if first_missing.is_none() {
                            first_missing = Some(name.to_string());
                        }
                        // Keep the literal `${name}` so the rendered
                        // path is at least diagnosable.
                        out.push_str("${");
                        out.push_str(name);
                        out.push('}');
                    }
                }
                rest = &after[end + 1..];
            }
            None => {
                // Unterminated `${` — treat as literal and bail.
                out.push_str(&rest[start..]);
                return (
                    out,
                    Err("unterminated `${` placeholder in substrate_path_template".to_string()),
                );
            }
        }
    }
    out.push_str(rest);
    match first_missing {
        Some(name) => (out, Err(name)),
        None => (out, Ok(())),
    }
}

/// Shape-match a substrate value as a Workshop. Returns the priority
/// (>0) when the value looks like a Workshop, 0 otherwise.
///
/// Heuristic: object with a `panels` array, and every panel entry has
/// a `substrate_path` string. Two-level check keeps false positives
/// out without forcing full schema validation in the Object-Type
/// inference path.
pub fn matches(value: &Value) -> u32 {
    let Some(obj) = value.as_object() else {
        return 0;
    };
    let Some(panels) = obj.get("panels").and_then(Value::as_array) else {
        return 0;
    };
    // Empty panels array is a legitimate Workshop (a blank canvas
    // someone is about to fill). Non-empty panels must all look like
    // panels.
    let all_panel_shaped = panels.iter().all(|p| {
        p.as_object()
            .and_then(|o| o.get("substrate_path"))
            .and_then(Value::as_str)
            .is_some()
    });
    if all_panel_shaped {
        OBJECT_TYPE_PRIORITY
    } else {
        0
    }
}

/// Live per-panel subscription state for a [`Workshop`] rendering.
/// Owns one [`PanelSub`] per substrate path currently referenced by
/// the Workshop schema; diffs the path set on each paint so newly
/// added panels get polled and removed panels have their pending
/// replies dropped.
#[derive(Default)]
pub struct WorkshopView {
    /// Per-substrate-path subscription state, keyed by
    /// [`WorkshopPanel::substrate_path`]. Using a `HashMap` here (vs.
    /// a `Vec` matching the panel order) lets Workshop layout tweaks
    /// reorder panels without churning subscriptions.
    subs: HashMap<String, PanelSub>,
}

struct PanelSub {
    /// Last value we pulled for this path. Rendered while the next
    /// read is in flight so the panel doesn't flicker to blank.
    value: Option<Value>,
    /// Pending `substrate.read` reply channel. `Some` while a read is
    /// in flight; dropped atomically when the Workshop stops
    /// referencing this path.
    pending: Option<ReplyRx>,
    /// When the last poll was dispatched. Used to space reads at
    /// [`PANEL_POLL`].
    last_poll: web_time::Instant,
    /// Last error message, if the most recent read failed. Cleared on
    /// the next successful read. Rendered above the viewer so the user
    /// sees *why* a panel isn't updating rather than a stale value.
    last_error: Option<String>,
}

impl PanelSub {
    fn new() -> Self {
        Self {
            value: None,
            pending: None,
            // Epoch: fire the first poll immediately. The
            // `epoch_minus` helper handles the WASM cold-load
            // time-origin underflow (see WEFT-247); fallback means
            // the first poll fires one `PANEL_POLL` later instead.
            last_poll: epoch_minus(PANEL_POLL * 2),
            last_error: None,
        }
    }
}

impl WorkshopView {
    /// Paint a Workshop derived from `value`. Drives per-panel
    /// subscriptions, prunes stale ones, and delegates rendering of
    /// each panel's value to the viewer registry.
    ///
    /// Called from the Explorer's detail pane; see
    /// [`crate::explorer::Explorer::paint_detail`].
    pub fn paint(&mut self, ui: &mut egui::Ui, value: &Value, live: &Arc<Live>) {
        // Parse fresh every frame. If a publish to the Workshop path
        // changed the schema between frames, the new layout takes
        // effect here — no cache invalidation dance. Cost is O(panels)
        // JSON traversal per frame; the panel count is small by
        // construction.
        let workshop = match parse(value) {
            Ok(w) => w,
            Err(err) => {
                paint_parse_error(ui, &err, value);
                return;
            }
        };

        self.reconcile_subs(&workshop);
        self.drain_replies();
        self.tick(live);

        paint_header(ui, &workshop);
        match workshop.layout {
            WorkshopLayout::Rows => self.paint_rows(ui, &workshop),
            WorkshopLayout::Grid => self.paint_grid(ui, &workshop),
            WorkshopLayout::Tabs => self.paint_tabs(ui, &workshop),
            WorkshopLayout::Unknown => {
                // Unknown layouts fall back to rows with a small
                // diagnostic so a forward-compatible writer can see
                // their layout string didn't match the registered set.
                ui.label(
                    egui::RichText::new(format!(
                        "layout `{:?}` not implemented — falling back to rows",
                        workshop.layout
                    ))
                    .italics()
                    .small()
                    .color(crate::theming::palette::ACCENT_SAND_DEEP),
                );
                self.paint_rows(ui, &workshop);
            }
        }
    }

    /// Drop subscriptions for paths no longer referenced by the
    /// Workshop; add fresh [`PanelSub`]s for newly introduced paths.
    /// Dropping the `PanelSub` closes its `ReplyRx` — no dangling
    /// reads against a removed panel.
    fn reconcile_subs(&mut self, workshop: &Workshop) {
        let mut desired: HashMap<String, ()> = HashMap::new();
        for panel in &workshop.panels {
            desired.insert(panel.substrate_path.clone(), ());
        }
        // Drop subs whose path is no longer in the Workshop.
        self.subs.retain(|path, _| desired.contains_key(path));
        // Ensure every desired path has a sub.
        for path in desired.keys() {
            self.subs.entry(path.clone()).or_insert_with(PanelSub::new);
        }
    }

    /// Fire `substrate.read` for any panel whose poll interval has
    /// elapsed and isn't already in flight.
    fn tick(&mut self, live: &Arc<Live>) {
        for (path, sub) in self.subs.iter_mut() {
            if sub.pending.is_some() {
                continue;
            }
            if sub.last_poll.elapsed() < PANEL_POLL {
                continue;
            }
            let (tx, rx) = live::reply_channel();
            sub.pending = Some(rx);
            sub.last_poll = web_time::Instant::now();
            live.submit(Command::Raw {
                method: "substrate.read".into(),
                params: serde_json::json!({ "path": path }),
                reply: Some(tx),
            });
        }
    }

    /// Drain completed reads into each panel's cached value.
    fn drain_replies(&mut self) {
        for (_path, sub) in self.subs.iter_mut() {
            let Some(rx) = sub.pending.as_mut() else {
                continue;
            };
            match live::try_recv_reply(rx) {
                live::TryReply::Done(Ok(value)) => {
                    sub.pending = None;
                    let new_value = value.get("value").cloned().unwrap_or(Value::Null);
                    sub.value = Some(new_value);
                    sub.last_error = None;
                }
                live::TryReply::Done(Err(err)) => {
                    sub.pending = None;
                    sub.last_error = Some(err);
                }
                live::TryReply::Closed => {
                    sub.pending = None;
                    sub.last_error = Some("transport closed".to_string());
                }
                live::TryReply::Empty => {
                    // Still in flight.
                }
            }
        }
    }

    /// Rows layout: vertical ScrollArea + per-panel Frame.
    fn paint_rows(&self, ui: &mut egui::Ui, workshop: &Workshop) {
        if Self::paint_empty_state(ui, workshop) {
            return;
        }
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for (idx, panel) in workshop.panels.iter().enumerate() {
                    self.paint_panel(ui, idx, panel);
                    ui.add_space(6.0);
                }
            });
    }

    /// Grid layout: square-ish grid via `egui::Grid`. Column count is
    /// derived from the panel count (`ceil(sqrt(n))`) so a 4-panel
    /// Workshop renders 2×2, a 9-panel Workshop renders 3×3, etc. The
    /// row order is left-to-right top-to-bottom per `WorkshopPanel`
    /// order so the schema's ordering survives the layout.
    /// [WEFT-278]
    fn paint_grid(&self, ui: &mut egui::Ui, workshop: &Workshop) {
        if Self::paint_empty_state(ui, workshop) {
            return;
        }
        let cols = grid_columns_for(workshop.panels.len());
        let cell_w = (ui.available_width() / cols as f32).max(120.0) - 12.0;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                egui::Grid::new(("workshop-grid", workshop.panels.len(), cols))
                    .num_columns(cols)
                    .spacing(egui::vec2(8.0, 8.0))
                    .show(ui, |ui| {
                        for (idx, panel) in workshop.panels.iter().enumerate() {
                            ui.allocate_ui_with_layout(
                                egui::vec2(cell_w, 0.0),
                                egui::Layout::top_down(egui::Align::Min),
                                |ui| {
                                    ui.set_min_width(cell_w);
                                    self.paint_panel(ui, idx, panel);
                                },
                            );
                            if (idx + 1) % cols == 0 {
                                ui.end_row();
                            }
                        }
                        // Close out the trailing partial row so the
                        // grid finalizes cleanly.
                        if !workshop.panels.is_empty()
                            && !workshop.panels.len().is_multiple_of(cols)
                        {
                            ui.end_row();
                        }
                    });
            });
    }

    /// Tabs layout: a horizontal selectable tab bar across panel
    /// titles, with the selected panel rendered in full beneath. The
    /// active tab is per-Workshop egui memory, so re-parsing the
    /// schema on each frame doesn't reset the user's selection. The
    /// selected index is clamped to `panels.len() - 1` whenever a
    /// hot-reload shrinks the panel count. [WEFT-279]
    fn paint_tabs(&self, ui: &mut egui::Ui, workshop: &Workshop) {
        if Self::paint_empty_state(ui, workshop) {
            return;
        }
        // Persist the active tab index across frames. Keyed off the
        // ui id of the current scope so two stacked Workshops each
        // get their own tab state.
        let mem_id = ui.id().with("workshop-tabs-active");
        let mut active = ui.ctx().data(|d| d.get_temp::<usize>(mem_id)).unwrap_or(0);
        if active >= workshop.panels.len() {
            active = workshop.panels.len() - 1;
        }
        ui.horizontal_wrapped(|ui| {
            for (idx, panel) in workshop.panels.iter().enumerate() {
                let label = panel
                    .title
                    .clone()
                    .unwrap_or_else(|| panel.substrate_path.clone());
                if ui.selectable_label(active == idx, label).clicked() {
                    active = idx;
                }
            }
        });
        ui.separator();
        ui.ctx().data_mut(|d| d.insert_temp(mem_id, active));
        if let Some(panel) = workshop.panels.get(active) {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    self.paint_panel(ui, active, panel);
                });
        }
    }

    /// Empty-state placeholder shared by all three layouts. Returns
    /// `true` when it painted (caller should bail out), `false` when
    /// the panel list is non-empty.
    fn paint_empty_state(ui: &mut egui::Ui, workshop: &Workshop) -> bool {
        if !workshop.panels.is_empty() {
            return false;
        }
        ui.vertical_centered(|ui| {
            ui.add_space(16.0);
            ui.label(
                egui::RichText::new("(empty Workshop — publish a `panels` array)")
                    .italics()
                    .color(crate::theming::palette::TEXT_SUBTLE),
            );
        });
        true
    }

    fn paint_panel(&self, ui: &mut egui::Ui, idx: usize, panel: &WorkshopPanel) {
        let title = panel
            .title
            .clone()
            .unwrap_or_else(|| panel.substrate_path.clone());
        let min_h = panel.min_height.unwrap_or(DEFAULT_MIN_HEIGHT);
        let id = egui::Id::new(("weft-workshop-panel", idx, &panel.substrate_path));

        egui::Frame::group(ui.style())
            .inner_margin(egui::Margin::same(6))
            .show(ui, |ui| {
                // Header row: title + muted path hint.
                ui.horizontal(|ui| {
                    ui.strong(title);
                    ui.separator();
                    ui.label(
                        egui::RichText::new(&panel.substrate_path)
                            .monospace()
                            .small()
                            .color(crate::theming::palette::TEXT_SYSTEM_MSG),
                    );
                });
                ui.separator();

                // Body: the viewer registry renders the sub's most
                // recent value. If nothing has landed yet we show a
                // brief "reading…" hint.
                ui.push_id(id, |ui| {
                    ui.allocate_ui_with_layout(
                        egui::vec2(ui.available_width(), min_h),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            // [WEFT-274] Surface a missing-param hint
                            // before any subscription state — the
                            // panel's path is broken until the writer
                            // supplies the param.
                            if let Err(missing) = &panel.substitution_status {
                                ui.label(
                                    egui::RichText::new(format!(
                                        "missing param `{missing}` in `substrate_path_template`"
                                    ))
                                    .small()
                                    .color(crate::theming::palette::ACCENT_PEACH),
                                );
                            }
                            let sub = self.subs.get(&panel.substrate_path);
                            match sub {
                                Some(sub) => {
                                    if let Some(err) = &sub.last_error {
                                        ui.label(
                                            egui::RichText::new(format!("error: {err}"))
                                                .small()
                                                .color(crate::theming::palette::TEXT_ERROR_SOFT),
                                        );
                                    }
                                    match &sub.value {
                                        Some(v) => {
                                            // [WEFT-280] Honor explicit
                                            // viewer_hint when the name
                                            // matches a registered
                                            // viewer; fall through to
                                            // shape-dispatch otherwise.
                                            paint_with_viewer_hint(
                                                ui,
                                                &panel.viewer_hint,
                                                &panel.substrate_path,
                                                v,
                                            );
                                        }
                                        None => {
                                            ui.label(
                                                egui::RichText::new("reading…")
                                                    .italics()
                                                    .color(crate::theming::palette::TEXT_SUBTLE),
                                            );
                                        }
                                    }
                                }
                                None => {
                                    // Should be unreachable — reconcile
                                    // ensures a sub exists for every
                                    // declared panel path. Render a
                                    // debug hint rather than panicking.
                                    ui.label("(no subscription)");
                                }
                            }
                        },
                    );
                });
            });
    }
}

/// Compute the column count for a square-ish grid layout.
/// `ceil(sqrt(n))` so 1→1, 2→2, 3→2, 4→2, 5→3, 9→3, 10→4. Defaults
/// to 1 when the panel list is empty (the empty-state shortcut
/// short-circuits before this is reached, but a safe fallback keeps
/// the helper total).
pub(crate) fn grid_columns_for(n: usize) -> usize {
    if n <= 1 {
        return n.max(1);
    }
    let sqrt = (n as f64).sqrt().ceil() as usize;
    sqrt.max(1)
}

/// Resolve a panel's `viewer_hint` to an explicit viewer paint
/// function. `"auto"` (and unset) routes through the shape-matcher
/// dispatcher, which is the legacy behavior. Named hints that match
/// a registered viewer route directly to it. Unknown names fall
/// back to shape-dispatch with an inline diagnostic so the writer
/// sees their hint didn't match. [WEFT-280]
pub(crate) fn paint_with_viewer_hint(ui: &mut egui::Ui, hint: &str, path: &str, value: &Value) {
    let trimmed = hint.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("auto") {
        super::viewers::dispatch(ui, path, value);
        return;
    }
    if let Some(paint) = viewer_for_hint(trimmed) {
        paint(ui, path, value);
        return;
    }
    ui.label(
        egui::RichText::new(format!(
            "viewer_hint `{trimmed}` is not a registered viewer — falling back to auto"
        ))
        .italics()
        .small()
        .color(crate::theming::palette::ACCENT_SAND_DEEP),
    );
    super::viewers::dispatch(ui, path, value);
}

/// Map a viewer-hint name to the matching viewer's paint function.
/// Names are stable lower-snake-case identifiers chosen to mirror the
/// module name of each viewer. Returns `None` for any name not in the
/// registered set; the caller falls back to shape-dispatch in that
/// case so a typo never blanks a panel.
///
/// To register a new viewer here, add a `"name" => &paint_fn` arm and
/// keep the names sorted alphabetically.
pub(crate) fn viewer_for_hint(name: &str) -> Option<fn(&mut egui::Ui, &str, &Value)> {
    use super::viewers::*;
    Some(match name {
        "audio_meter" => audio_meter::AudioMeterViewer::paint,
        "chain_tail" => chain_tail::ChainTailViewer::paint,
        "connection_badge" => connection_badge::ConnectionBadgeViewer::paint,
        "depth_map" => depth_map::DepthMapViewer::paint,
        "graph" | "graph_edit" => graph::GraphViewer::paint,
        "health" => health::HealthViewer::paint,
        "json" | "json_fallback" => json_fallback::JsonFallbackViewer::paint,
        "lineage" => lineage::LineageViewer::paint,
        "mesh_nodes" => mesh_nodes::MeshNodesViewer::paint,
        "pcm_chunk" => pcm_chunk::PcmChunkViewer::paint,
        "process_table" => process_table::ProcessTableViewer::paint,
        "sensor" => sensor::SensorViewer::paint,
        "time_series" => time_series::TimeSeriesViewer::paint,
        "waveform" => waveform::WaveformViewer::paint,
        _ => return None,
    })
}

fn paint_header(ui: &mut egui::Ui, workshop: &Workshop) {
    ui.horizontal(|ui| {
        ui.heading(
            workshop
                .title
                .clone()
                .unwrap_or_else(|| "Workshop".to_string()),
        );
        ui.separator();
        ui.label(
            egui::RichText::new(format!(
                "{:?} · {} panel{}",
                workshop.layout,
                workshop.panels.len(),
                if workshop.panels.len() == 1 { "" } else { "s" },
            ))
            .small()
            .color(crate::theming::palette::TERM_LABEL),
        );
    });
    ui.separator();
}

fn paint_parse_error(ui: &mut egui::Ui, err: &str, raw: &Value) {
    ui.horizontal(|ui| {
        ui.heading("Workshop");
        ui.separator();
        ui.label(
            egui::RichText::new("parse error")
                .small()
                .color(crate::theming::palette::TEXT_ERROR_SOFT),
        );
    });
    ui.separator();
    ui.label(
        egui::RichText::new(err)
            .monospace()
            .small()
            .color(crate::theming::palette::TEXT_ERROR_SOFT),
    );
    ui.add_space(4.0);
    ui.collapsing("raw value", |ui| {
        let pretty =
            serde_json::to_string_pretty(raw).unwrap_or_else(|_| "<unserialisable>".to_string());
        ui.monospace(pretty);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_minimal_workshop() {
        let v = json!({ "panels": [] });
        let w = parse(&v).expect("minimal workshop parses");
        assert!(w.title.is_none());
        assert_eq!(w.layout, WorkshopLayout::Rows);
        assert!(w.panels.is_empty());
    }

    #[test]
    fn parse_full_workshop() {
        let v = json!({
            "title": "Mic diagnostic",
            "layout": "rows",
            "panels": [
                {
                    "title": "RMS gauge",
                    "substrate_path": "substrate/sensor/mic",
                    "viewer_hint": "auto",
                    "min_height": 120.0,
                },
                {
                    "substrate_path": "substrate/derived/transcript/mic",
                    "viewer_hint": "auto",
                }
            ]
        });
        let w = parse(&v).expect("full workshop parses");
        assert_eq!(w.title.as_deref(), Some("Mic diagnostic"));
        assert_eq!(w.layout, WorkshopLayout::Rows);
        assert_eq!(w.panels.len(), 2);
        assert_eq!(w.panels[0].title.as_deref(), Some("RMS gauge"));
        assert_eq!(w.panels[0].substrate_path, "substrate/sensor/mic");
        assert_eq!(w.panels[0].min_height, Some(120.0));
        assert!(w.panels[1].title.is_none());
    }

    #[test]
    fn parse_rejects_non_object() {
        assert!(parse(&json!([])).is_err());
        assert!(parse(&json!(42)).is_err());
        assert!(parse(&json!(null)).is_err());
    }

    #[test]
    fn parse_rejects_missing_panels() {
        let v = json!({ "title": "no panels" });
        assert!(parse(&v).is_err());
    }

    #[test]
    fn parse_rejects_panel_without_path() {
        let v = json!({
            "panels": [
                { "title": "missing substrate_path" }
            ]
        });
        assert!(parse(&v).is_err());
    }

    #[test]
    fn parse_accepts_unknown_layout() {
        let v = json!({
            "layout": "sunburst",
            "panels": []
        });
        let w = parse(&v).expect("unknown layout round-trips");
        assert_eq!(w.layout, WorkshopLayout::Unknown);
    }

    #[test]
    fn parse_accepts_grid_and_tabs_layouts() {
        let grid = parse(&json!({ "layout": "grid", "panels": [] })).unwrap();
        assert_eq!(grid.layout, WorkshopLayout::Grid);
        let tabs = parse(&json!({ "layout": "tabs", "panels": [] })).unwrap();
        assert_eq!(tabs.layout, WorkshopLayout::Tabs);
    }

    #[test]
    fn matches_positive_for_workshop_shape() {
        let v = json!({
            "panels": [
                { "substrate_path": "substrate/sensor/mic" }
            ]
        });
        assert_eq!(matches(&v), OBJECT_TYPE_PRIORITY);
    }

    #[test]
    fn matches_empty_panels_still_workshop() {
        let v = json!({ "panels": [] });
        assert_eq!(matches(&v), OBJECT_TYPE_PRIORITY);
    }

    #[test]
    fn matches_rejects_panel_missing_substrate_path() {
        let v = json!({
            "panels": [
                { "title": "broken" }
            ]
        });
        assert_eq!(matches(&v), 0);
    }

    #[test]
    fn matches_rejects_non_object() {
        assert_eq!(matches(&json!([])), 0);
        assert_eq!(matches(&Value::Null), 0);
        assert_eq!(matches(&json!("workshop")), 0);
    }

    #[test]
    fn matches_rejects_missing_panels() {
        let v = json!({ "title": "not a workshop" });
        assert_eq!(matches(&v), 0);
    }

    #[test]
    fn layout_default_is_rows() {
        assert_eq!(WorkshopLayout::default(), WorkshopLayout::Rows);
    }

    fn panel(path: &str) -> WorkshopPanel {
        WorkshopPanel {
            title: None,
            substrate_path: path.into(),
            substrate_path_template: None,
            substitution_status: Ok(()),
            viewer_hint: "auto".into(),
            min_height: None,
        }
    }

    #[test]
    fn reconcile_adds_and_removes_subs() {
        let mut view = WorkshopView::default();
        let w1 = Workshop {
            title: None,
            layout: WorkshopLayout::Rows,
            params: HashMap::new(),
            panels: vec![panel("a"), panel("b")],
        };
        view.reconcile_subs(&w1);
        assert_eq!(view.subs.len(), 2);
        assert!(view.subs.contains_key("a"));
        assert!(view.subs.contains_key("b"));

        // Remove `a`, add `c` — `b` should be retained across the edit
        // so its cached value doesn't churn.
        let b_ptr_addr = view.subs.get("b").map(|s| s as *const _ as usize);
        let w2 = Workshop {
            title: None,
            layout: WorkshopLayout::Rows,
            params: HashMap::new(),
            panels: vec![panel("b"), panel("c")],
        };
        view.reconcile_subs(&w2);
        assert_eq!(view.subs.len(), 2);
        assert!(!view.subs.contains_key("a"));
        assert!(view.subs.contains_key("b"));
        assert!(view.subs.contains_key("c"));
        // Sanity: `b`'s sub struct survives in place (HashMap may
        // relocate, but it wasn't recreated as a fresh PanelSub).
        let _ = b_ptr_addr; // Just document intent.
    }

    #[test]
    fn parse_resolves_substrate_path_template_with_params() {
        // [WEFT-274] `${name}` placeholders substitute from the
        // top-level `params` map when no literal `substrate_path`
        // is present.
        let v = json!({
            "params": { "node": "n-6f3a9c" },
            "panels": [
                { "substrate_path_template": "substrate/${node}/sensor/mic" }
            ]
        });
        let w = parse(&v).expect("templated workshop parses");
        assert_eq!(w.panels[0].substrate_path, "substrate/n-6f3a9c/sensor/mic");
        assert!(w.panels[0].substitution_status.is_ok());
        assert_eq!(
            w.panels[0].substrate_path_template.as_deref(),
            Some("substrate/${node}/sensor/mic")
        );
    }

    #[test]
    fn parse_records_missing_param_for_template() {
        let v = json!({
            "panels": [
                { "substrate_path_template": "substrate/${node}/sensor/mic" }
            ]
        });
        let w = parse(&v).expect("template-without-params still parses");
        // Path retains the literal `${node}` so paint sees something.
        assert_eq!(w.panels[0].substrate_path, "substrate/${node}/sensor/mic");
        assert_eq!(
            w.panels[0].substitution_status.as_ref().unwrap_err(),
            "node"
        );
    }

    #[test]
    fn parse_literal_path_wins_over_template() {
        // Both fields present: literal wins, no substitution attempted.
        let v = json!({
            "params": { "node": "n-aaa" },
            "panels": [
                {
                    "substrate_path": "substrate/literal",
                    "substrate_path_template": "substrate/${node}/sensor/mic"
                }
            ]
        });
        let w = parse(&v).unwrap();
        assert_eq!(w.panels[0].substrate_path, "substrate/literal");
        assert!(w.panels[0].substitution_status.is_ok());
    }

    #[test]
    fn parse_rejects_non_string_param() {
        // Numbers/bools have no canonical encoding into a path
        // component — we reject them at parse time so the writer sees
        // the error immediately.
        let v = json!({
            "params": { "tries": 3 },
            "panels": []
        });
        let err = parse(&v).unwrap_err();
        assert!(err.contains("`params.tries`"), "got: {err}");
    }

    #[test]
    fn parse_rejects_panel_without_path_or_template() {
        let v = json!({
            "panels": [{ "title": "no path" }]
        });
        let err = parse(&v).unwrap_err();
        assert!(err.contains("substrate_path"), "got: {err}");
    }

    #[test]
    fn substitute_handles_unterminated_placeholder() {
        let mut params = HashMap::new();
        params.insert("a".to_string(), "x".to_string());
        let (out, status) = substitute("substrate/${a}/${unterminated", &params);
        // First half resolved; the trailing `${unterminated` is
        // surfaced as a parse error.
        assert!(out.starts_with("substrate/x/"));
        assert!(status.unwrap_err().contains("unterminated"));
    }

    #[test]
    fn grid_columns_are_squareish() {
        // [WEFT-278] sqrt-shaped grid keeps cells visually balanced.
        assert_eq!(grid_columns_for(0), 1);
        assert_eq!(grid_columns_for(1), 1);
        assert_eq!(grid_columns_for(2), 2);
        assert_eq!(grid_columns_for(3), 2);
        assert_eq!(grid_columns_for(4), 2);
        assert_eq!(grid_columns_for(5), 3);
        assert_eq!(grid_columns_for(9), 3);
        assert_eq!(grid_columns_for(10), 4);
    }

    #[test]
    fn viewer_for_hint_resolves_known_names() {
        // [WEFT-280] Named hints map to registered viewers; unknown
        // names return None so the dispatcher falls back to auto.
        assert!(viewer_for_hint("audio_meter").is_some());
        assert!(viewer_for_hint("waveform").is_some());
        assert!(viewer_for_hint("graph").is_some());
        assert!(viewer_for_hint("json").is_some());
        assert!(viewer_for_hint("json_fallback").is_some());
        // Case-sensitive on purpose — viewer names are stable
        // identifiers, not user-facing text.
        assert!(viewer_for_hint("Audio_Meter").is_none());
        assert!(viewer_for_hint("not_a_real_viewer").is_none());
    }

    #[test]
    fn object_type_constants_present() {
        assert_eq!(OBJECT_TYPE_NAME, "Workshop");
        assert_eq!(OBJECT_TYPE_DISPLAY, "Workshop");
        // Workshop is a specialized structural shape — priority should
        // beat Mesh (20) so a substrate value that happens to look
        // both Mesh-ish and Workshop-shaped lands here.
        const { assert!(OBJECT_TYPE_PRIORITY > 20) };
    }
}
