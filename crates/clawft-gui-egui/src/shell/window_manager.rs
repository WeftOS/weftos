//! Freeform window manager v1 — multi-pane stage (ADR-073 Phase B / WEFT-686).
//!
//! Owns hit-test concerns until graduated into surface primitives:
//! z-order, drag, resize, edge snap, and versioned layout persistence.
//!
//! Coordinates are **normalized** to the stage rect (0..1). Painting
//! converts to absolute egui rects each frame.
//!
//! # WASM degradation
//! Layout persistence writes to disk on native only. On
//! `wasm32-unknown-unknown`, `load_layout` / `save_layout` are no-ops
//! (in-memory layout still works for the session). Documented residual:
//! OPFS-backed persist can land later without schema change.

use std::path::{Path, PathBuf};

use eframe::egui;
use serde::{Deserialize, Serialize};

use super::window_intent::{ArrangeMode, PaneId, PaneKind};

/// Schema version for layout files. Bump when `LayoutFile` fields change
/// incompatibly; loaders reject unknown major versions.
pub const LAYOUT_SCHEMA_VERSION: u32 = 1;

/// Edge-snap threshold in absolute pixels.
pub const SNAP_PX: f32 = 16.0;
/// Minimum pane size in normalized units.
pub const MIN_NORM_W: f32 = 0.12;
pub const MIN_NORM_H: f32 = 0.12;
/// Title bar height (px).
pub const TITLE_BAR_H: f32 = 28.0;
/// Resize handle thickness (px).
pub const RESIZE_HANDLE: f32 = 8.0;

/// Persistent geometry in stage-normalized space.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NormRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl NormRect {
    pub fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }

    pub fn clamp_min(&mut self) {
        self.w = self.w.max(MIN_NORM_W);
        self.h = self.h.max(MIN_NORM_H);
        self.x = self.x.clamp(0.0, 1.0 - self.w);
        self.y = self.y.clamp(0.0, 1.0 - self.h);
    }

    pub fn to_abs(&self, stage: egui::Rect) -> egui::Rect {
        let min = egui::pos2(
            stage.left() + self.x * stage.width(),
            stage.top() + self.y * stage.height(),
        );
        let max = egui::pos2(
            stage.left() + (self.x + self.w) * stage.width(),
            stage.top() + (self.y + self.h) * stage.height(),
        );
        egui::Rect::from_min_max(min, max)
    }

    pub fn from_abs(abs: egui::Rect, stage: egui::Rect) -> Self {
        let sw = stage.width().max(1.0);
        let sh = stage.height().max(1.0);
        let mut r = Self {
            x: (abs.left() - stage.left()) / sw,
            y: (abs.top() - stage.top()) / sh,
            w: abs.width() / sw,
            h: abs.height() / sh,
        };
        r.clamp_min();
        r
    }
}

/// Visual attention state for a pane (ADR-073 attention bus — visual first).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionState {
    #[default]
    Calm,
    NeedsHuman,
}

/// One freeform pane on the stage.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Pane {
    pub id: PaneId,
    pub kind: PaneKind,
    pub rect: NormRect,
    /// Higher draws / hit-tests later (on top).
    pub z: u32,
    #[serde(default)]
    pub attention: AttentionState,
    /// Rolling text buffer for agent output (summarize target).
    #[serde(default)]
    pub body_text: String,
    /// Last summary produced by `WindowIntent::Summarize`.
    #[serde(default)]
    pub summary: Option<String>,
}

/// Versioned layout file on disk.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LayoutFile {
    pub schema_version: u32,
    pub panes: Vec<Pane>,
    pub focused: Option<PaneId>,
    pub next_z: u32,
    pub next_seq: u64,
}

/// Freeform multi-pane stage state.
pub struct WindowManager {
    pub panes: Vec<Pane>,
    pub focused: Option<PaneId>,
    next_z: u32,
    next_seq: u64,
    /// In-flight drag (pane id + grab offset in absolute px).
    drag: Option<DragState>,
    /// In-flight resize.
    resize: Option<ResizeState>,
    /// Dirty flag — layout should be persisted.
    pub dirty: bool,
    /// Path used for native persistence (None = memory only / wasm).
    layout_path: Option<PathBuf>,
}

#[derive(Clone, Debug)]
struct DragState {
    id: PaneId,
    grab_offset: egui::Vec2,
}

#[derive(Clone, Debug)]
struct ResizeState {
    id: PaneId,
    /// Which edges are being dragged.
    left: bool,
    right: bool,
    top: bool,
    bottom: bool,
}

impl Default for WindowManager {
    fn default() -> Self {
        Self::new()
    }
}

impl WindowManager {
    pub fn new() -> Self {
        Self {
            panes: Vec::new(),
            focused: None,
            next_z: 1,
            next_seq: 1,
            drag: None,
            resize: None,
            dirty: false,
            layout_path: None,
        }
    }

    /// Attach a native layout path and attempt load.
    pub fn with_layout_path(mut self, path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        self.layout_path = Some(path.clone());
        if let Err(e) = self.load_from_path(&path) {
            log::debug!("workspace layout load skipped: {e}");
        }
        self
    }

    /// Default layout path: `$XDG_DATA_HOME/weftos/workspace-layout.json`
    /// or `~/.weftos/workspace-layout.json`.
    pub fn default_layout_path() -> Option<PathBuf> {
        if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
            let p = PathBuf::from(xdg);
            if !p.as_os_str().is_empty() {
                return Some(p.join("weftos").join("workspace-layout.json"));
            }
        }
        if let Some(home) = std::env::var_os("HOME") {
            return Some(
                PathBuf::from(home)
                    .join(".weftos")
                    .join("workspace-layout.json"),
            );
        }
        None
    }

    pub fn pane_count(&self) -> usize {
        self.panes.len()
    }

    pub fn get(&self, id: &PaneId) -> Option<&Pane> {
        self.panes.iter().find(|p| &p.id == id)
    }

    pub fn get_mut(&mut self, id: &PaneId) -> Option<&mut Pane> {
        self.panes.iter_mut().find(|p| &p.id == id)
    }

    /// Spawn a pane. Assigns id/z if needed. Returns the pane id.
    /// Background agent spawns (`PaneKind::Agent { background: true }`)
    /// do **not** open a pane (explicit opt-in to invisible).
    pub fn spawn(&mut self, kind: PaneKind, id: Option<PaneId>) -> Option<PaneId> {
        if let PaneKind::Agent {
            background: true, ..
        } = &kind
        {
            return None;
        }
        let id = id.unwrap_or_else(|| {
            let seq = self.next_seq;
            self.next_seq += 1;
            PaneId::new(format!("pane-{seq}"))
        });
        // Offset cascade for new panes so they don't fully overlap.
        let n = self.panes.len() as f32;
        let offset = (n * 0.04) % 0.35;
        let rect = NormRect::new(0.08 + offset, 0.08 + offset, 0.42, 0.40);
        let z = self.alloc_z();
        // Fill empty session_id for agents.
        let kind = match kind {
            PaneKind::Agent {
                session_id,
                role,
                title,
                background,
            } if session_id.is_empty() => PaneKind::Agent {
                session_id: format!("sess-{}", self.next_seq.saturating_sub(1)),
                role,
                title,
                background,
            },
            other => other,
        };
        let body_text = match &kind {
            PaneKind::Agent { role, title, .. } => {
                format!("Agent `{title}` ({role}) ready.\nAwaiting work…\n")
            }
            PaneKind::App { app } => format!("App surface: {app}\n"),
            PaneKind::Tool { tool, .. } => format!("Tool: {tool}\n"),
        };
        self.panes.push(Pane {
            id: id.clone(),
            kind,
            rect,
            z,
            attention: AttentionState::Calm,
            body_text,
            summary: None,
        });
        self.focused = Some(id.clone());
        self.dirty = true;
        Some(id)
    }

    pub fn focus(&mut self, id: &PaneId) -> bool {
        if !self.panes.iter().any(|p| &p.id == id) {
            return false;
        }
        self.focused = Some(id.clone());
        // Raise z.
        let z = self.alloc_z();
        if let Some(p) = self.get_mut(id) {
            p.z = z;
        }
        self.dirty = true;
        true
    }

    pub fn close(&mut self, id: &PaneId) -> bool {
        let before = self.panes.len();
        self.panes.retain(|p| &p.id != id);
        if self.panes.len() == before {
            return false;
        }
        if self.focused.as_ref() == Some(id) {
            self.focused = self.panes.iter().max_by_key(|p| p.z).map(|p| p.id.clone());
        }
        self.dirty = true;
        true
    }

    pub fn arrange(&mut self, mode: ArrangeMode) {
        let n = self.panes.len();
        if n == 0 {
            return;
        }
        // Sort by z so arrange is stable visually.
        self.panes.sort_by_key(|p| p.z);
        match mode {
            ArrangeMode::Grid => {
                let cols = (n as f32).sqrt().ceil() as usize;
                let rows = n.div_ceil(cols);
                let cell_w = 1.0 / cols as f32;
                let cell_h = 1.0 / rows as f32;
                let margin = 0.01;
                for (i, p) in self.panes.iter_mut().enumerate() {
                    let col = i % cols;
                    let row = i / cols;
                    p.rect = NormRect::new(
                        col as f32 * cell_w + margin,
                        row as f32 * cell_h + margin,
                        (cell_w - 2.0 * margin).max(MIN_NORM_W),
                        (cell_h - 2.0 * margin).max(MIN_NORM_H),
                    );
                    p.rect.clamp_min();
                }
            }
            ArrangeMode::Cascade => {
                for (i, p) in self.panes.iter_mut().enumerate() {
                    let o = (i as f32) * 0.05;
                    p.rect = NormRect::new(0.05 + o, 0.05 + o, 0.50, 0.45);
                    p.rect.clamp_min();
                }
            }
            ArrangeMode::Stack => {
                // All centered, slightly offset, full focus stack.
                for (i, p) in self.panes.iter_mut().enumerate() {
                    let o = (i as f32) * 0.01;
                    p.rect = NormRect::new(0.15 + o, 0.12 + o, 0.70, 0.70);
                    p.rect.clamp_min();
                }
            }
        }
        self.dirty = true;
    }

    /// Attach tool pane to agent: updates tool's `attached_to`.
    pub fn attach(&mut self, tool_id: &PaneId, agent_id: &PaneId) -> bool {
        if !self.panes.iter().any(|p| &p.id == agent_id) {
            return false;
        }
        if let Some(p) = self.get_mut(tool_id)
            && let PaneKind::Tool {
                tool,
                attached_to: _,
            } = &p.kind
        {
            let tool = tool.clone();
            p.kind = PaneKind::Tool {
                tool,
                attached_to: Some(agent_id.0.clone()),
            };
            self.dirty = true;
            return true;
        }
        false
    }

    /// Summarize pane body into short text (UI first; TTS later).
    pub fn summarize(&mut self, id: &PaneId, max_chars: usize) -> Option<String> {
        let p = self.get_mut(id)?;
        let raw = p.body_text.trim();
        if raw.is_empty() {
            p.summary = Some("(empty)".into());
            return p.summary.clone();
        }
        // Prefer last non-empty lines — agent walls grow at the end.
        let lines: Vec<&str> = raw.lines().filter(|l| !l.trim().is_empty()).collect();
        let tail = lines.iter().rev().take(6).cloned().collect::<Vec<_>>();
        let mut joined = tail.into_iter().rev().collect::<Vec<_>>().join(" · ");
        if joined.chars().count() > max_chars {
            joined = joined.chars().take(max_chars.saturating_sub(1)).collect();
            joined.push('…');
        }
        p.summary = Some(joined.clone());
        self.dirty = true;
        Some(joined)
    }

    /// Append agent output (used by demo / future substrate feed).
    pub fn append_body(&mut self, id: &PaneId, text: &str) {
        if let Some(p) = self.get_mut(id) {
            p.body_text.push_str(text);
            if !text.ends_with('\n') {
                p.body_text.push('\n');
            }
        }
    }

    pub fn set_attention(&mut self, id: &PaneId, state: AttentionState) {
        if let Some(p) = self.get_mut(id) {
            p.attention = state;
        }
    }

    fn alloc_z(&mut self) -> u32 {
        let z = self.next_z;
        self.next_z = self.next_z.saturating_add(1);
        z
    }

    // ── Layout persistence ──────────────────────────────────────────

    pub fn to_layout_file(&self) -> LayoutFile {
        LayoutFile {
            schema_version: LAYOUT_SCHEMA_VERSION,
            panes: self.panes.clone(),
            focused: self.focused.clone(),
            next_z: self.next_z,
            next_seq: self.next_seq,
        }
    }

    pub fn apply_layout_file(&mut self, file: LayoutFile) -> Result<(), String> {
        if file.schema_version != LAYOUT_SCHEMA_VERSION {
            return Err(format!(
                "unsupported layout schema_version {} (want {LAYOUT_SCHEMA_VERSION})",
                file.schema_version
            ));
        }
        self.panes = file.panes;
        self.focused = file.focused;
        self.next_z = file.next_z.max(1);
        self.next_seq = file.next_seq.max(1);
        self.dirty = false;
        Ok(())
    }

    pub fn load_from_path(&mut self, path: &Path) -> Result<(), String> {
        #[cfg(target_arch = "wasm32")]
        {
            let _ = path;
            return Err("layout persistence disabled on wasm".into());
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
            let file: LayoutFile = serde_json::from_str(&text).map_err(|e| e.to_string())?;
            self.apply_layout_file(file)
        }
    }

    pub fn save_if_dirty(&mut self) {
        if !self.dirty {
            return;
        }
        if let Some(path) = self.layout_path.clone() {
            if let Err(e) = self.save_to_path(&path) {
                log::warn!("workspace layout save failed: {e}");
            } else {
                self.dirty = false;
            }
        } else {
            // Memory-only: clear dirty so we don't spam.
            self.dirty = false;
        }
    }

    pub fn save_to_path(&self, path: &Path) -> Result<(), String> {
        #[cfg(target_arch = "wasm32")]
        {
            let _ = path;
            return Err("layout persistence disabled on wasm".into());
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            let file = self.to_layout_file();
            let text = serde_json::to_string_pretty(&file).map_err(|e| e.to_string())?;
            std::fs::write(path, text).map_err(|e| e.to_string())
        }
    }

    // ── Geometry helpers ────────────────────────────────────────────

    /// Snap a rect to stage edges (and optionally other panes) in abs space.
    pub fn snap_abs(stage: egui::Rect, mut abs: egui::Rect, others: &[egui::Rect]) -> egui::Rect {
        // Stage edges.
        if (abs.left() - stage.left()).abs() < SNAP_PX {
            abs = abs.translate(egui::vec2(stage.left() - abs.left(), 0.0));
        }
        if (abs.right() - stage.right()).abs() < SNAP_PX {
            abs = abs.translate(egui::vec2(stage.right() - abs.right(), 0.0));
        }
        if (abs.top() - stage.top()).abs() < SNAP_PX {
            abs = abs.translate(egui::vec2(0.0, stage.top() - abs.top()));
        }
        if (abs.bottom() - stage.bottom()).abs() < SNAP_PX {
            abs = abs.translate(egui::vec2(0.0, stage.bottom() - abs.bottom()));
        }
        // Other pane edges.
        for o in others {
            if (abs.left() - o.right()).abs() < SNAP_PX {
                abs = abs.translate(egui::vec2(o.right() - abs.left(), 0.0));
            }
            if (abs.right() - o.left()).abs() < SNAP_PX {
                abs = abs.translate(egui::vec2(o.left() - abs.right(), 0.0));
            }
            if (abs.top() - o.bottom()).abs() < SNAP_PX {
                abs = abs.translate(egui::vec2(0.0, o.bottom() - abs.top()));
            }
            if (abs.bottom() - o.top()).abs() < SNAP_PX {
                abs = abs.translate(egui::vec2(0.0, o.top() - abs.bottom()));
            }
            if (abs.left() - o.left()).abs() < SNAP_PX {
                abs = abs.translate(egui::vec2(o.left() - abs.left(), 0.0));
            }
            if (abs.top() - o.top()).abs() < SNAP_PX {
                abs = abs.translate(egui::vec2(0.0, o.top() - abs.top()));
            }
        }
        // Keep fully inside stage.
        let dx = if abs.left() < stage.left() {
            stage.left() - abs.left()
        } else if abs.right() > stage.right() {
            stage.right() - abs.right()
        } else {
            0.0
        };
        let dy = if abs.top() < stage.top() {
            stage.top() - abs.top()
        } else if abs.bottom() > stage.bottom() {
            stage.bottom() - abs.bottom()
        } else {
            0.0
        };
        abs.translate(egui::vec2(dx, dy))
    }

    /// Sorted pane indices by z ascending (paint back-to-front).
    pub fn paint_order(&self) -> Vec<usize> {
        let mut idx: Vec<usize> = (0..self.panes.len()).collect();
        idx.sort_by_key(|&i| self.panes[i].z);
        idx
    }

    /// Hit-test topmost pane under pointer.
    pub fn hit_test(&self, stage: egui::Rect, pos: egui::Pos2) -> Option<PaneId> {
        let mut best: Option<(u32, PaneId)> = None;
        for p in &self.panes {
            let abs = p.rect.to_abs(stage);
            if abs.contains(pos) {
                match &best {
                    Some((z, _)) if *z >= p.z => {}
                    _ => best = Some((p.z, p.id.clone())),
                }
            }
        }
        best.map(|(_, id)| id)
    }

    /// Title string for chrome.
    pub fn pane_title(pane: &Pane) -> String {
        match &pane.kind {
            PaneKind::Agent { title, role, .. } => {
                if title.is_empty() {
                    format!("Agent · {role}")
                } else {
                    format!("{title} · {role}")
                }
            }
            PaneKind::App { app } => format!("App · {app}"),
            PaneKind::Tool { tool, attached_to } => match attached_to {
                Some(a) => format!("Tool · {tool} → {a}"),
                None => format!("Tool · {tool}"),
            },
        }
    }

    // ── Interaction (called from paint) ─────────────────────────────

    /// Begin drag on title bar.
    pub fn begin_drag(&mut self, id: PaneId, grab_offset: egui::Vec2) {
        self.focus(&id);
        self.drag = Some(DragState { id, grab_offset });
    }

    pub fn begin_resize(&mut self, id: PaneId, left: bool, right: bool, top: bool, bottom: bool) {
        self.focus(&id);
        self.resize = Some(ResizeState {
            id,
            left,
            right,
            top,
            bottom,
        });
    }

    pub fn end_interaction(&mut self, stage: egui::Rect) {
        // Apply edge snap on release.
        if let Some(drag) = self.drag.take() {
            self.snap_pane(&drag.id, stage);
        }
        if let Some(resize) = self.resize.take() {
            self.snap_pane(&resize.id, stage);
        }
    }

    fn snap_pane(&mut self, id: &PaneId, stage: egui::Rect) {
        let others: Vec<egui::Rect> = self
            .panes
            .iter()
            .filter(|p| &p.id != id)
            .map(|p| p.rect.to_abs(stage))
            .collect();
        if let Some(p) = self.get_mut(id) {
            let abs = p.rect.to_abs(stage);
            let snapped = Self::snap_abs(stage, abs, &others);
            p.rect = NormRect::from_abs(snapped, stage);
            self.dirty = true;
        }
    }

    /// Drive drag/resize from pointer position while primary is down.
    pub fn interact_pointer(&mut self, stage: egui::Rect, pointer: Option<egui::Pos2>, down: bool) {
        if !down {
            if self.drag.is_some() || self.resize.is_some() {
                self.end_interaction(stage);
            }
            return;
        }
        let Some(pos) = pointer else { return };

        if let Some(drag) = self.drag.clone()
            && let Some(p) = self.get_mut(&drag.id)
        {
            let abs = p.rect.to_abs(stage);
            let new_min = pos - drag.grab_offset;
            let mut moved = egui::Rect::from_min_size(new_min, abs.size());
            // Clamp inside stage during drag (soft — hard snap on release).
            if moved.left() < stage.left() {
                moved = moved.translate(egui::vec2(stage.left() - moved.left(), 0.0));
            }
            if moved.top() < stage.top() {
                moved = moved.translate(egui::vec2(0.0, stage.top() - moved.top()));
            }
            if moved.right() > stage.right() {
                moved = moved.translate(egui::vec2(stage.right() - moved.right(), 0.0));
            }
            if moved.bottom() > stage.bottom() {
                moved = moved.translate(egui::vec2(0.0, stage.bottom() - moved.bottom()));
            }
            p.rect = NormRect::from_abs(moved, stage);
            self.dirty = true;
        }

        if let Some(rs) = self.resize.clone()
            && let Some(p) = self.get_mut(&rs.id)
        {
            let mut abs = p.rect.to_abs(stage);
            let min_w = MIN_NORM_W * stage.width();
            let min_h = MIN_NORM_H * stage.height();
            if rs.left {
                let new_left = pos.x.clamp(stage.left(), abs.right() - min_w);
                abs.min.x = new_left;
            }
            if rs.right {
                let new_right = pos.x.clamp(abs.left() + min_w, stage.right());
                abs.max.x = new_right;
            }
            if rs.top {
                let new_top = pos.y.clamp(stage.top(), abs.bottom() - min_h);
                abs.min.y = new_top;
            }
            if rs.bottom {
                let new_bottom = pos.y.clamp(abs.top() + min_h, stage.bottom());
                abs.max.y = new_bottom;
            }
            p.rect = NormRect::from_abs(abs, stage);
            self.dirty = true;
        }
    }

    pub fn is_dragging(&self) -> bool {
        self.drag.is_some() || self.resize.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_agent_opens_pane() {
        let mut wm = WindowManager::new();
        let id = wm
            .spawn(
                PaneKind::Agent {
                    session_id: "s1".into(),
                    role: "coder".into(),
                    title: "Coder".into(),
                    background: false,
                },
                None,
            )
            .expect("pane");
        assert_eq!(wm.pane_count(), 1);
        assert_eq!(wm.focused.as_ref(), Some(&id));
    }

    #[test]
    fn background_spawn_skips_pane() {
        let mut wm = WindowManager::new();
        let id = wm.spawn(
            PaneKind::Agent {
                session_id: "s1".into(),
                role: "coder".into(),
                title: "Bg".into(),
                background: true,
            },
            None,
        );
        assert!(id.is_none());
        assert_eq!(wm.pane_count(), 0);
    }

    #[test]
    fn concurrent_apps_and_agent() {
        let mut wm = WindowManager::new();
        wm.spawn(
            PaneKind::App {
                app: "terminal".into(),
            },
            None,
        );
        wm.spawn(PaneKind::App { app: "chat".into() }, None);
        wm.spawn(
            PaneKind::Agent {
                session_id: "a1".into(),
                role: "agent".into(),
                title: "A1".into(),
                background: false,
            },
            None,
        );
        assert_eq!(wm.pane_count(), 3);
    }

    #[test]
    fn z_order_focus_raises() {
        let mut wm = WindowManager::new();
        let a = wm.spawn(PaneKind::App { app: "a".into() }, None).unwrap();
        let b = wm.spawn(PaneKind::App { app: "b".into() }, None).unwrap();
        let za = wm.get(&a).unwrap().z;
        let zb = wm.get(&b).unwrap().z;
        assert!(zb > za);
        wm.focus(&a);
        assert!(wm.get(&a).unwrap().z > zb);
    }

    #[test]
    fn arrange_grid() {
        let mut wm = WindowManager::new();
        for i in 0..4 {
            wm.spawn(
                PaneKind::App {
                    app: format!("app{i}"),
                },
                None,
            );
        }
        wm.arrange(ArrangeMode::Grid);
        // 2x2 grid cells should not all share the same origin.
        let xs: Vec<f32> = wm.panes.iter().map(|p| p.rect.x).collect();
        assert!(xs.iter().any(|&x| x > 0.2));
    }

    #[test]
    fn layout_roundtrip() {
        let mut wm = WindowManager::new();
        wm.spawn(
            PaneKind::Agent {
                session_id: "s".into(),
                role: "r".into(),
                title: "T".into(),
                background: false,
            },
            Some(PaneId::new("fixed")),
        );
        let file = wm.to_layout_file();
        assert_eq!(file.schema_version, LAYOUT_SCHEMA_VERSION);
        let json = serde_json::to_string(&file).unwrap();
        let back: LayoutFile = serde_json::from_str(&json).unwrap();
        let mut wm2 = WindowManager::new();
        wm2.apply_layout_file(back).unwrap();
        assert_eq!(wm2.pane_count(), 1);
        assert_eq!(wm2.panes[0].id.as_str(), "fixed");
    }

    #[test]
    fn layout_version_reject() {
        let mut wm = WindowManager::new();
        let bad = LayoutFile {
            schema_version: 99,
            panes: vec![],
            focused: None,
            next_z: 1,
            next_seq: 1,
        };
        assert!(wm.apply_layout_file(bad).is_err());
    }

    #[test]
    fn snap_to_left_edge() {
        let stage = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1000.0, 800.0));
        let abs = egui::Rect::from_min_size(egui::pos2(10.0, 100.0), egui::vec2(200.0, 150.0));
        let snapped = WindowManager::snap_abs(stage, abs, &[]);
        assert!((snapped.left() - 0.0).abs() < 0.01);
    }

    #[test]
    fn summarize_truncates() {
        let mut wm = WindowManager::new();
        let id = wm
            .spawn(
                PaneKind::Agent {
                    session_id: "s".into(),
                    role: "r".into(),
                    title: "T".into(),
                    background: false,
                },
                None,
            )
            .unwrap();
        wm.append_body(&id, &"line of agent output\n".repeat(20));
        let s = wm.summarize(&id, 40).unwrap();
        assert!(s.chars().count() <= 40);
    }

    #[test]
    fn attach_tool_to_agent() {
        let mut wm = WindowManager::new();
        let agent = wm
            .spawn(
                PaneKind::Agent {
                    session_id: "s".into(),
                    role: "coder".into(),
                    title: "C".into(),
                    background: false,
                },
                Some(PaneId::new("agent")),
            )
            .unwrap();
        let tool = wm
            .spawn(
                PaneKind::Tool {
                    tool: "terminal".into(),
                    attached_to: None,
                },
                Some(PaneId::new("tool")),
            )
            .unwrap();
        assert!(wm.attach(&tool, &agent));
        match &wm.get(&tool).unwrap().kind {
            PaneKind::Tool {
                attached_to: Some(a),
                ..
            } => assert_eq!(a, "agent"),
            _ => panic!("expected attach"),
        }
    }

    #[test]
    fn native_layout_file_roundtrip() {
        let dir = std::env::temp_dir().join(format!("weftos-wm-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("layout.json");
        let mut wm = WindowManager::new();
        wm.spawn(
            PaneKind::App {
                app: "terminal".into(),
            },
            None,
        );
        wm.spawn(PaneKind::App { app: "chat".into() }, None);
        wm.save_to_path(&path).unwrap();
        let mut wm2 = WindowManager::new();
        wm2.load_from_path(&path).unwrap();
        assert_eq!(wm2.pane_count(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
