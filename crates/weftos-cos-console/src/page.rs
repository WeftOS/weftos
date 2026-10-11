//! The page layout: one full 800×456 region with a title and plain
//! blocks (headings, paragraphs, bullet lists, rules). Drag to scroll;
//! swipe right (or `hide_page`) returns to the split. Nodes 300–499:
//! chrome at 300..400, lines recycled from 400..500. Rendered markdown
//! and SVG come in C5; this is the C2 example page.

use weftos_leaf_scene::{Layer, Node, NodeId, SceneOp};

use crate::pool::LinePool;
use crate::prim::{hit_rect, line, text};
use crate::text::{truncate, wrap, wrap_prefixed};
use crate::theme::{
    ids, nid, BODY, DIM, GREEN, LINE_PITCH, PAGE, PAGE_COLS, PAGE_MARGIN, PAGE_TEXT_BOTTOM,
    PAGE_TEXT_TOP, PAGE_VISIBLE, SMALL,
};

/// Horizontal travel that counts as a swipe back to the split.
pub const SWIPE_PX: i32 = 120;

/// One block of a page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PageBlock {
    Heading(String),
    Paragraph(String),
    Bullets(Vec<String>),
    Rule,
}

/// A page for the page layout: a title and blocks.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PageDoc {
    pub title: String,
    pub blocks: Vec<PageBlock>,
}

impl PageDoc {
    /// Title plus plain paragraphs.
    pub fn paragraphs(title: impl Into<String>, paragraphs: Vec<String>) -> Self {
        Self {
            title: title.into(),
            blocks: paragraphs.into_iter().map(PageBlock::Paragraph).collect(),
        }
    }

    /// The C2 example page: headings, paragraphs, a bullet list and a
    /// rule, so every block kind is on screen.
    pub fn example() -> Self {
        use PageBlock::*;
        Self {
            title: "Plan for Friday 10 October".into(),
            blocks: vec![
                Heading("Morning".into()),
                Paragraph(
                    "Review the display-client session API first; it unblocks the console merge and \
                     takes fifteen minutes if the trait matches the contract."
                        .into(),
                ),
                Bullets(vec![
                    "09:30  display-client review (lane A)".into(),
                    "10:00  licence relay: two one-file cards, a developer lane on each".into(),
                    "11:00  PG cog host: read the two restart traces from 03:10".into(),
                ]),
                Rule,
                Heading("Afternoon".into()),
                Paragraph(
                    "Clear. Standup notes and the board summary are drafted and waiting for your \
                     word before anything is posted."
                        .into(),
                ),
                Paragraph("Say \"close\" or swipe right to return to the conversation.".into()),
            ],
        }
    }
}

/// A laid-out page line.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PageLine {
    Text { text: String, heading: bool },
    Rule,
}

#[derive(Debug, Clone)]
pub struct PageView {
    title: String,
    lines: Vec<PageLine>,
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
        let mut lines: Vec<PageLine> = Vec::new();
        let blank = |lines: &mut Vec<PageLine>| {
            if !lines.is_empty() {
                lines.push(PageLine::Text { text: String::new(), heading: false });
            }
        };
        for block in &doc.blocks {
            match block {
                PageBlock::Heading(h) => {
                    blank(&mut lines);
                    for t in wrap(&h.to_uppercase(), PAGE_COLS) {
                        lines.push(PageLine::Text { text: t, heading: true });
                    }
                }
                PageBlock::Paragraph(p) => {
                    blank(&mut lines);
                    for t in wrap(p, PAGE_COLS) {
                        lines.push(PageLine::Text { text: t, heading: false });
                    }
                }
                PageBlock::Bullets(items) => {
                    blank(&mut lines);
                    for item in items {
                        for t in wrap_prefixed(item, PAGE_COLS, "- ", "  ") {
                            lines.push(PageLine::Text { text: t, heading: false });
                        }
                    }
                }
                PageBlock::Rule => {
                    blank(&mut lines);
                    lines.push(PageLine::Rule);
                }
            }
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

    fn line_node(&self, id: NodeId, slot: usize, idx: usize) -> Node {
        let x = PAGE.x + PAGE_MARGIN;
        let y = PAGE_TEXT_TOP + slot as i32 * LINE_PITCH;
        match &self.lines[idx] {
            // Headings are upper-cased at layout time and sit after a
            // blank line; one green keeps the page calm.
            PageLine::Text { text: t, .. } => text(id.path_hash(), &BODY, x, y, t.clone(), GREEN),
            PageLine::Rule => line(id.path_hash(), x, y + 10, PAGE.right() - PAGE_MARGIN, y + 10, DIM),
        }
    }

    fn render(&mut self, ops: &mut Vec<SceneOp>) {
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
                    ops.push(SceneOp::Insert(self.line_node(id, i, idx)));
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
        self.px_accum += dy;
        let lines = self.px_accum / LINE_PITCH;
        self.px_accum -= lines * LINE_PITCH;
        (self.scroll_lines(-lines), false)
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

    /// End of a touch gesture (or a pause in dragging).
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
