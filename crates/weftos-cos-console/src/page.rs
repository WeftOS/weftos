//! Page mode stub (C2): the visual panel expands to the full content
//! area and shows a title and plain wrapped paragraphs. Drag to
//! scroll; swipe right (or `hide_page`) to return to the split.
//! Nodes 300–999: chrome at 300..400, lines recycled from 400..1000.

use weftos_leaf_scene::{Layer, NodeId, SceneOp};

use crate::pool::LinePool;
use crate::prim::{hit_rect, line, text};
use crate::text::{truncate, wrap};
use crate::theme::{
    ids, nid, BODY, DIM, GREEN, LINE_PITCH, PAGE, PAGE_COLS, PAGE_MARGIN, PAGE_TEXT_BOTTOM,
    PAGE_TEXT_TOP, PAGE_VISIBLE, SMALL,
};

/// Horizontal travel that counts as a swipe back to the split.
pub const SWIPE_PX: i32 = 120;

/// A page for the visual panel: title plus plain paragraphs.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PageDoc {
    pub title: String,
    pub paragraphs: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct PageView {
    title: String,
    lines: Vec<String>,
    /// Lines scrolled past the top.
    scroll: usize,
    pool: LinePool,
    slots: Vec<Option<(NodeId, usize)>>,
    px_accum: i32,
    swipe: (i32, i32),
    thumb: Option<(i32, i32)>,
}

impl PageView {
    pub fn new(doc: &PageDoc) -> Self {
        let mut lines = Vec::new();
        for (i, p) in doc.paragraphs.iter().enumerate() {
            if i > 0 {
                lines.push(String::new());
            }
            lines.extend(wrap(p, PAGE_COLS));
        }
        Self {
            title: doc.title.clone(),
            lines,
            scroll: 0,
            pool: LinePool::new(ids::PAGE_LINES),
            slots: vec![None; PAGE_VISIBLE],
            px_accum: 0,
            swipe: (0, 0),
            thumb: None,
        }
    }

    pub fn scroll(&self) -> usize {
        self.scroll
    }

    pub fn max_scroll(&self) -> usize {
        self.lines.len().saturating_sub(PAGE_VISIBLE)
    }

    pub fn total_lines(&self) -> usize {
        self.lines.len()
    }

    pub fn live_ids(&self) -> Vec<NodeId> {
        self.slots.iter().flatten().map(|(id, _)| *id).collect()
    }

    /// Chrome plus the first window of lines.
    pub fn open_ops(&mut self) -> Vec<SceneOp> {
        let x0 = PAGE.x + PAGE_MARGIN;
        let x1 = PAGE.right() - PAGE_MARGIN;
        let title_cols = BODY.cols(x1 - x0);
        let mut ops = vec![
            SceneOp::Insert(hit_rect(ids::PAGE_INPUT, Layer::Bg, PAGE.x, PAGE.y, PAGE.w, PAGE.h)),
            SceneOp::Insert(text(ids::PAGE_TITLE, &BODY, x0, PAGE.y + 14, truncate(&self.title, title_cols), GREEN)),
            SceneOp::Insert(line(ids::PAGE_RULE, x0, PAGE.y + 42, x1, PAGE.y + 42, DIM)),
            SceneOp::Insert(text(
                ids::PAGE_HINT,
                &SMALL,
                x1 - 6 * 24,
                PAGE.bottom() - 14,
                "swipe right to return  >",
                DIM,
            )),
        ];
        self.render(&mut ops);
        ops
    }

    fn render(&mut self, ops: &mut Vec<SceneOp>) {
        let x = PAGE.x + PAGE_MARGIN;
        for i in 0..PAGE_VISIBLE {
            let idx = self.scroll + i;
            let want = self.lines.get(idx).map(|_| idx);
            match (want, self.slots[i]) {
                (None, None) => {}
                (None, Some((id, _))) => {
                    ops.push(SceneOp::Remove(id));
                    self.pool.free(id);
                    self.slots[i] = None;
                }
                (Some(idx), Some((_, cur))) if cur == idx => {}
                (Some(idx), old) => {
                    if let Some((id, _)) = old {
                        ops.push(SceneOp::Remove(id));
                        self.pool.free(id);
                    }
                    let id = self.pool.alloc();
                    let y = PAGE_TEXT_TOP + i as i32 * LINE_PITCH;
                    ops.push(SceneOp::Insert(text(id.path_hash(), &BODY, x, y, self.lines[idx].clone(), GREEN)));
                    self.slots[i] = Some((id, idx));
                }
            }
        }
        self.render_thumb(ops);
    }

    fn render_thumb(&mut self, ops: &mut Vec<SceneOp>) {
        if self.lines.len() <= PAGE_VISIBLE {
            return;
        }
        let track = PAGE_TEXT_BOTTOM - PAGE_TEXT_TOP;
        let total = self.lines.len() as f32;
        let y0 = PAGE_TEXT_TOP + (self.scroll as f32 / total * track as f32) as i32;
        let h = ((PAGE_VISIBLE as f32 / total * track as f32) as i32).max(8);
        if self.thumb == Some((y0, h)) {
            return;
        }
        self.thumb = Some((y0, h));
        let x = PAGE.right() - 6;
        ops.push(SceneOp::Update(line(ids::PAGE_THUMB, x, y0, x, y0 + h, DIM)));
    }

    /// A drag on the page: scroll by `dy`, and close on a clear
    /// rightward swipe. Returns `(ops, close)`.
    pub fn drag(&mut self, dx: i32, dy: i32) -> (Vec<SceneOp>, bool) {
        self.swipe.0 += dx;
        self.swipe.1 += dy;
        if self.swipe.0 >= SWIPE_PX && self.swipe.0 > 2 * self.swipe.1.abs() {
            self.swipe = (0, 0);
            return (Vec::new(), true);
        }
        let mut ops = Vec::new();
        self.px_accum += dy;
        let lines = self.px_accum / LINE_PITCH;
        self.px_accum -= lines * LINE_PITCH;
        if lines != 0 {
            let target = self.scroll as i64 - lines as i64;
            let next = target.clamp(0, self.max_scroll() as i64) as usize;
            if next != self.scroll {
                self.scroll = next;
                self.render(&mut ops);
            }
        }
        (ops, false)
    }

    /// Scroll by whole lines (positive goes further down the page).
    pub fn scroll_lines(&mut self, delta: i32) -> Vec<SceneOp> {
        let mut ops = Vec::new();
        let target = self.scroll as i64 + delta as i64;
        let next = target.clamp(0, self.max_scroll() as i64) as usize;
        if next != self.scroll {
            self.scroll = next;
            self.render(&mut ops);
        }
        ops
    }

    /// End of a touch gesture.
    pub fn release(&mut self) {
        self.swipe = (0, 0);
        self.px_accum = 0;
    }

    /// Remove every page node.
    pub fn close_ops(&mut self) -> Vec<SceneOp> {
        let mut ops: Vec<SceneOp> = [ids::PAGE_INPUT, ids::PAGE_TITLE, ids::PAGE_RULE, ids::PAGE_HINT]
            .into_iter()
            .map(|n| SceneOp::Remove(nid(n)))
            .collect();
        if self.thumb.take().is_some() {
            ops.push(SceneOp::Remove(nid(ids::PAGE_THUMB)));
        }
        for s in self.slots.iter_mut() {
            if let Some((id, _)) = s.take() {
                ops.push(SceneOp::Remove(id));
            }
        }
        self.pool.clear();
        ops
    }
}
