//! Checklist view: title, `3/7` progress with a bar and the next open
//! item in the head band; tappable rows with check boxes in the left
//! column. Rows are 64 px, the touch minimum.

use weftos_leaf_scene::{Layer, Node, NodeId, SceneOp};

use crate::guide::{
    head_input, head_lines, head_title, head_counter, row_box, row_id, row_input, row_of,
    rows_visible, Checklist, Drawn, GuideAnswer, RowScroll, Swipe, HEAD_COLS, HEAD_X, ROW_COLS,
    ROW_TEXT_X,
};
use crate::prim::{line, rect_fill, rect_stroke, text};
use crate::text::wrap;
use crate::theme::{ids, BODY, DIM, GREEN, GRID, GUIDE_HEAD, GUIDE_SIDE, LINE_PITCH, PAD, SMALL, TOUCH_MIN};

const ROW_H: i32 = TOUCH_MIN;

#[derive(Debug, Clone)]
pub struct ChecklistView {
    list: Checklist,
    scroll: RowScroll,
    swipe: Swipe,
    drawn: Drawn,
}

impl ChecklistView {
    pub fn new(list: Checklist) -> Self {
        Self {
            list,
            scroll: RowScroll::default(),
            swipe: Swipe::default(),
            drawn: Drawn::default(),
        }
    }

    pub fn list(&self) -> &Checklist {
        &self.list
    }

    pub fn done_count(&self) -> usize {
        self.list.items.iter().filter(|i| i.done).count()
    }

    fn max_scroll(&self) -> usize {
        self.list.items.len().saturating_sub(rows_visible(ROW_H))
    }

    /// Draw the view from nothing (the layout was just cleared).
    pub fn open_ops(&mut self) -> Vec<SceneOp> {
        self.drawn = Drawn::default();
        let mut ops = vec![SceneOp::Insert(head_input())];
        self.drawn.sync(self.nodes(), &mut ops);
        ops
    }

    fn nodes(&self) -> Vec<Node> {
        let total = self.list.items.len();
        let done = self.done_count();
        let mut v = vec![
            head_title(&self.list.title, 5),
            head_counter(&format!("{done}/{total}")),
        ];
        // Progress bar under the title.
        let bar_x = HEAD_X;
        let bar_w = GUIDE_HEAD.w - 2 * PAD - 4;
        let bar_y = GUIDE_HEAD.y + 54;
        v.push(rect_fill(ids::GUIDE_BAR_TRACK, Layer::Widget, bar_x, bar_y, bar_w, 3, GRID));
        if total > 0 && done > 0 {
            let w = (bar_w as i64 * done as i64 / total as i64) as i32;
            v.push(rect_fill(ids::GUIDE_BAR_FILL, Layer::Widget, bar_x, bar_y, w.max(2), 3, GREEN));
        }
        v.push(text(ids::GUIDE_HINT, &SMALL, HEAD_X, GUIDE_HEAD.y + 70, "tap an item to toggle it", DIM));
        let next = self.list.items.iter().find(|i| !i.done);
        let next_text = match next {
            Some(i) => format!("next: {}", i.text),
            None => "all done".to_string(),
        };
        v.extend(head_lines(&next_text, &BODY, HEAD_COLS, GUIDE_HEAD.y + 92, LINE_PITCH, 3, GREEN));

        for slot in 0..rows_visible(ROW_H) {
            let Some(item) = self.list.items.get(self.scroll.offset + slot) else { break };
            let b = row_box(slot, ROW_H);
            let color = if item.done { DIM } else { GREEN };
            v.push(row_input(slot, b.y, b.h));
            v.push(rect_stroke(row_id(slot, 1), Layer::Widget, GUIDE_SIDE.x + 16, b.y + 20, 24, 24, color));
            if item.done {
                v.push(line(row_id(slot, 2), GUIDE_SIDE.x + 21, b.y + 32, GUIDE_SIDE.x + 27, b.y + 38, GREEN));
                v.push(line(row_id(slot, 3), GUIDE_SIDE.x + 27, b.y + 38, GUIDE_SIDE.x + 37, b.y + 26, GREEN));
            }
            for (k, l) in wrap(&item.text, ROW_COLS).into_iter().take(2).enumerate() {
                v.push(text(row_id(slot, 4 + k as u32), &BODY, GUIDE_SIDE.x + ROW_TEXT_X, b.y + 10 + k as i32 * LINE_PITCH, l, color));
            }
            v.push(line(row_id(slot, 6), GUIDE_SIDE.x + 8, b.bottom() - 1, GUIDE_SIDE.right() - 8, b.bottom() - 1, GRID));
        }
        v
    }

    fn resync(&mut self) -> Vec<SceneOp> {
        let mut ops = Vec::new();
        self.drawn.sync(self.nodes(), &mut ops);
        ops
    }

    /// The agent (or the host) sets an item; returns the update ops.
    pub fn set_item(&mut self, item_id: &str, done: bool) -> Vec<SceneOp> {
        if let Some(i) = self.list.items.iter_mut().find(|i| i.id == item_id) {
            i.done = done;
        }
        self.resync()
    }

    pub fn on_tap(&mut self, node: NodeId) -> (Vec<SceneOp>, Option<GuideAnswer>) {
        let Some((slot, 0)) = row_of(node) else { return (Vec::new(), None) };
        let idx = self.scroll.offset + slot;
        let Some(item) = self.list.items.get_mut(idx) else { return (Vec::new(), None) };
        item.done = !item.done;
        let answer = GuideAnswer::Toggle {
            checklist_id: self.list.id.clone(),
            item_id: item.id.clone(),
            done: item.done,
        };
        (self.resync(), Some(answer))
    }

    pub fn on_drag(&mut self, node: NodeId, dx: i32, dy: i32) -> (Vec<SceneOp>, bool) {
        if node.path_hash() == ids::GUIDE_HEAD_INPUT {
            return (Vec::new(), self.swipe.feed(dx, dy));
        }
        if row_of(node).is_some() && self.scroll.drag(dy, ROW_H, self.max_scroll()) {
            return (self.resync(), false);
        }
        (Vec::new(), false)
    }

    pub fn release(&mut self) {
        self.swipe.reset();
        self.scroll.reset_px();
    }

    pub fn live_ids(&self) -> Vec<u32> {
        self.drawn.ids()
    }
}
