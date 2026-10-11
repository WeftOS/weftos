//! Scene side of the chat log: the visible window as recycled text
//! nodes (contract range 1000–4095), the scroll thumb, and the
//! "new messages below" marker.

use weftos_leaf_scene::{Layer, Node, NodeId, Rgba, SceneOp};

use crate::chat_log::{ChatLog, Line, Role};
use crate::pool::LinePool;
use crate::prim::{hit_rect, line, rect_fill, text};
use crate::text::center;
use crate::theme::{
    ids, BLACK, BODY, CHAT, CHAT_COLS, CHAT_TEXT_BOTTOM, CHAT_TEXT_TOP, CHAT_VISIBLE, DIM, GREEN,
    LINE_PITCH, PAD, SMALL,
};

const MARKER_H: i32 = 20;

#[derive(Debug, Clone)]
struct Slot {
    id: NodeId,
    key: (u64, u32),
    text: String,
    color: Rgba,
}

#[derive(Debug, Clone)]
pub struct ChatView {
    log: ChatLog,
    pool: LinePool,
    slots: Vec<Option<Slot>>,
    marker_shown: bool,
    thumb: Option<(i32, i32)>,
}

impl Default for ChatView {
    fn default() -> Self {
        Self::new()
    }
}

impl ChatView {
    pub fn new() -> Self {
        Self {
            log: ChatLog::new(CHAT_COLS, CHAT_VISIBLE),
            pool: LinePool::new(ids::CHAT_LINES),
            slots: vec![None; CHAT_VISIBLE],
            marker_shown: false,
            thumb: None,
        }
    }

    pub fn log(&self) -> &ChatLog {
        &self.log
    }

    pub fn log_mut(&mut self) -> &mut ChatLog {
        &mut self.log
    }

    pub fn pool(&self) -> &LinePool {
        &self.pool
    }

    /// Y of slot `i`'s top edge.
    fn slot_y(i: usize) -> i32 {
        CHAT_TEXT_TOP + i as i32 * LINE_PITCH
    }

    fn color_for(l: &Line) -> Rgba {
        match (l.role, l.bright) {
            (_, true) => GREEN,
            (_, false) => DIM,
        }
    }

    fn line_node(id: NodeId, i: usize, s: &str, color: Rgba) -> Node {
        text(id.path_hash(), &BODY, CHAT.x + PAD, Self::slot_y(i), s, color)
    }

    /// Emit the ops that bring the scene up to date with the log:
    /// changed slots are updated in place, lines that left the window
    /// are removed and their ids recycled.
    pub fn render(&mut self, ops: &mut Vec<SceneOp>) {
        let window = self.log.window().to_vec();
        let n = window.len();
        let visible = self.slots.len();
        let offset = visible - n; // bottom-align when the log is short
        for i in 0..visible {
            let want = if i >= offset { Some(&window[i - offset]) } else { None };
            match (want, self.slots[i].take()) {
                (None, None) => {}
                (None, Some(old)) => {
                    ops.push(SceneOp::Remove(old.id));
                    self.pool.free(old.id);
                }
                (Some(l), Some(old)) if old.key == l.key() => {
                    let color = Self::color_for(l);
                    if old.text != l.text || old.color != color {
                        ops.push(SceneOp::Update(Self::line_node(old.id, i, &l.text, color)));
                    }
                    self.slots[i] = Some(Slot {
                        text: l.text.clone(),
                        color,
                        ..old
                    });
                }
                (Some(l), old) => {
                    if let Some(old) = old {
                        ops.push(SceneOp::Remove(old.id));
                        self.pool.free(old.id);
                    }
                    let id = self.pool.alloc();
                    let color = Self::color_for(l);
                    ops.push(SceneOp::Insert(Self::line_node(id, i, &l.text, color)));
                    self.slots[i] = Some(Slot {
                        id,
                        key: l.key(),
                        text: l.text.clone(),
                        color,
                    });
                }
            }
        }
        self.render_thumb(ops);
        self.render_marker(ops);
    }

    fn render_thumb(&mut self, ops: &mut Vec<SceneOp>) {
        let track_h = CHAT_TEXT_BOTTOM - CHAT_TEXT_TOP;
        let (start, len) = self.log.thumb();
        let y0 = CHAT_TEXT_TOP + (start * track_h as f32) as i32;
        let h = ((len * track_h as f32) as i32).max(8).min(track_h - (y0 - CHAT_TEXT_TOP));
        if self.thumb == Some((y0, h)) {
            return;
        }
        self.thumb = Some((y0, h));
        let x = CHAT.right() - 6;
        let color = if self.log.is_at_bottom() { DIM } else { GREEN };
        ops.push(SceneOp::Update(line(ids::CHAT_THUMB, x, y0, x, y0 + h, color)));
    }

    fn render_marker(&mut self, ops: &mut Vec<SceneOp>) {
        let want = self.log.unread_below();
        if want == self.marker_shown {
            return;
        }
        self.marker_shown = want;
        if !want {
            ops.push(SceneOp::Remove(crate::theme::nid(ids::CHAT_MARKER_BG)));
            ops.push(SceneOp::Remove(crate::theme::nid(ids::CHAT_MARKER_RULE)));
            ops.push(SceneOp::Remove(crate::theme::nid(ids::CHAT_MARKER_TEXT)));
            return;
        }
        // The marker takes the last line's row exactly, so the line
        // under it is hidden whole rather than cut mid-glyph.
        let x = CHAT.x + PAD;
        let w = CHAT.w - 2 * PAD - 8;
        let y = Self::slot_y(CHAT_VISIBLE - 1);
        let h = CHAT.bottom() - y;
        let mut bg = rect_fill(ids::CHAT_MARKER_BG, Layer::Alert, x, y, w, h, BLACK);
        bg.input = hit_rect(ids::CHAT_MARKER_BG, Layer::Alert, x, y, w, h).input;
        ops.push(SceneOp::Insert(bg));
        let mut rule = line(ids::CHAT_MARKER_RULE, x, y, x + w, y, GREEN);
        rule.layer = Layer::Alert;
        ops.push(SceneOp::Insert(rule));
        let label = center("v  new messages below  v", SMALL.cols(w));
        let mut t = text(ids::CHAT_MARKER_TEXT, &SMALL, x, y + (h - MARKER_H) / 2 + 5, label, GREEN);
        t.layer = Layer::Alert;
        ops.push(SceneOp::Insert(t));
    }

    /// Remove every chat node (page mode); the pool is reset.
    pub fn clear(&mut self, ops: &mut Vec<SceneOp>) {
        for s in self.slots.iter_mut() {
            if let Some(old) = s.take() {
                ops.push(SceneOp::Remove(old.id));
            }
        }
        self.pool.clear();
        if self.marker_shown {
            self.marker_shown = false;
            ops.push(SceneOp::Remove(crate::theme::nid(ids::CHAT_MARKER_BG)));
            ops.push(SceneOp::Remove(crate::theme::nid(ids::CHAT_MARKER_RULE)));
            ops.push(SceneOp::Remove(crate::theme::nid(ids::CHAT_MARKER_TEXT)));
        }
        if self.thumb.take().is_some() {
            ops.push(SceneOp::Remove(crate::theme::nid(ids::CHAT_THUMB)));
        }
    }

    /// Node ids currently on screen (for the range tests).
    pub fn live_ids(&self) -> Vec<NodeId> {
        self.slots.iter().flatten().map(|s| s.id).collect()
    }

    pub fn is_user_line(l: &Line) -> bool {
        l.role == Role::User
    }
}
