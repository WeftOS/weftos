//! Scene side of the chat log: the visible window as recycled text
//! nodes (contract range 1000–4095), the scroll thumb, and the
//! "new messages below" marker.

use weftos_leaf_scene::{Layer, Node, NodeId, Rgba, SceneOp};

use crate::chat_log::{ChatLog, Line};
use crate::pool::LinePool;
use crate::prim::{hit_rect, line, rect_fill, text};
use crate::text::center;
use crate::theme::{
    chat_visible, ids, nid, Box, BLACK, BODY, CHAT, CHAT_COLS, CHAT_LABEL_H, DIM, GREEN,
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
    area: Box,
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
            log: ChatLog::new(CHAT_COLS, chat_visible(CHAT.h)),
            area: CHAT,
            pool: LinePool::new(ids::CHAT_LINES),
            slots: vec![None; chat_visible(CHAT.h)],
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

    pub fn area(&self) -> Box {
        self.area
    }

    /// Re-home the log in `area`. Call after a `Clear`; the next
    /// `render` draws every visible line afresh.
    pub fn set_area(&mut self, area: Box) {
        self.area = area;
        let visible = chat_visible(area.h);
        self.log.set_visible(visible);
        self.slots = vec![None; visible];
        self.pool.clear();
        self.marker_shown = false;
        self.thumb = None;
    }

    fn text_top(&self) -> i32 {
        self.area.y + CHAT_LABEL_H
    }

    fn text_bottom(&self) -> i32 {
        self.area.bottom() - 10
    }

    /// Y of slot `i`'s top edge.
    fn slot_y(&self, i: usize) -> i32 {
        self.text_top() + i as i32 * LINE_PITCH
    }

    fn color_for(l: &Line) -> Rgba {
        if l.bright {
            GREEN
        } else {
            DIM
        }
    }

    fn line_node(&self, id: NodeId, i: usize, s: &str, color: Rgba) -> Node {
        text(id.path_hash(), &BODY, self.area.x + PAD, self.slot_y(i), s, color)
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
                        ops.push(SceneOp::Update(self.line_node(old.id, i, &l.text, color)));
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
                    ops.push(SceneOp::Insert(self.line_node(id, i, &l.text, color)));
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
        let track_h = self.text_bottom() - self.text_top();
        let (start, len) = self.log.thumb();
        let y0 = self.text_top() + (start * track_h as f32) as i32;
        let h = ((len * track_h as f32) as i32).max(8).min(track_h - (y0 - self.text_top()));
        if self.thumb == Some((y0, h)) {
            return;
        }
        self.thumb = Some((y0, h));
        let x = self.area.right() - 6;
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
            ops.push(SceneOp::Remove(nid(ids::CHAT_MARKER_BG)));
            ops.push(SceneOp::Remove(nid(ids::CHAT_MARKER_RULE)));
            ops.push(SceneOp::Remove(nid(ids::CHAT_MARKER_TEXT)));
            return;
        }
        // The marker takes the last line's row exactly, so the line
        // under it is hidden whole rather than cut mid-glyph.
        let x = self.area.x + PAD;
        let w = self.area.w - 2 * PAD - 8;
        let y = self.slot_y(self.slots.len() - 1);
        let h = self.area.bottom() - y;
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

    /// Node ids currently on screen (for the range tests).
    pub fn live_ids(&self) -> Vec<NodeId> {
        self.slots.iter().flatten().map(|s| s.id).collect()
    }
}
