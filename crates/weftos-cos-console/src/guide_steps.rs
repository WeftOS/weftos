//! Steps view: numbered steps in the left column with the current one
//! framed; title, `Step 2 of 5`, the current step's full text and the
//! back / next buttons in the head band.

use weftos_leaf_scene::{Layer, Node, NodeId, SceneOp};

use crate::guide::{
    back_button, head_counter, head_input, head_lines, head_title, next_button, row_box, row_id,
    row_input, row_of, rows_visible, Drawn, GuideAnswer, RowScroll, StepDir, Steps, Swipe,
    HEAD_COLS, ROW_COLS, ROW_TEXT_X,
};
use crate::prim::{circle_fill, circle_stroke, line, rect_stroke, text};
use crate::text::wrap;
use crate::theme::{ids, BLACK, BODY, DIM, GREEN, GRID, GUIDE_HEAD, GUIDE_SIDE, LINE_PITCH, TOUCH_MIN};

const ROW_H: i32 = TOUCH_MIN;

#[derive(Debug, Clone)]
pub struct StepsView {
    steps: Steps,
    scroll: RowScroll,
    swipe: Swipe,
    drawn: Drawn,
}

impl StepsView {
    pub fn new(mut steps: Steps) -> Self {
        if !steps.steps.is_empty() {
            steps.current = steps.current.min(steps.steps.len() - 1);
        }
        let mut scroll = RowScroll::default();
        scroll.reveal(steps.current, rows_visible(ROW_H));
        Self {
            steps,
            scroll,
            swipe: Swipe::default(),
            drawn: Drawn::default(),
        }
    }

    pub fn steps(&self) -> &Steps {
        &self.steps
    }

    pub fn current(&self) -> usize {
        self.steps.current
    }

    fn max_scroll(&self) -> usize {
        self.steps.steps.len().saturating_sub(rows_visible(ROW_H))
    }

    /// Draw the view from nothing (the layout was just cleared).
    pub fn open_ops(&mut self) -> Vec<SceneOp> {
        self.drawn = Drawn::default();
        let mut ops = vec![SceneOp::Insert(head_input())];
        self.drawn.sync(self.nodes(), &mut ops);
        ops
    }

    fn nodes(&self) -> Vec<Node> {
        let total = self.steps.steps.len();
        let cur = self.steps.current;
        let counter = format!("Step {} of {}", cur + 1, total);
        let mut v = vec![head_title(&self.steps.title, counter.len()), head_counter(&counter)];
        if let Some(s) = self.steps.steps.get(cur) {
            v.extend(head_lines(s, &BODY, HEAD_COLS, GUIDE_HEAD.y + 52, LINE_PITCH, 3, GREEN));
        }
        v.extend(back_button("< back", cur > 0));
        v.extend(next_button("next >", cur + 1 < total));

        for slot in 0..rows_visible(ROW_H) {
            let idx = self.scroll.offset + slot;
            let Some(s) = self.steps.steps.get(idx) else { break };
            let b = row_box(slot, ROW_H);
            let color = if idx == cur { GREEN } else { DIM };
            v.push(row_input(slot, b.y, b.h));
            let (cx, cy) = (GUIDE_SIDE.x + 30, b.y + 32);
            if idx < cur {
                v.push(circle_fill(row_id(slot, 1), cx, cy, 16, DIM));
                v.push(text(row_id(slot, 2), &BODY, cx - 5, cy - 10, format!("{}", idx + 1), BLACK));
            } else {
                v.push(circle_stroke(row_id(slot, 1), cx, cy, 16, color));
                v.push(text(row_id(slot, 2), &BODY, cx - 5, cy - 10, format!("{}", idx + 1), color));
            }
            for (k, l) in wrap(s, ROW_COLS).into_iter().take(2).enumerate() {
                v.push(text(row_id(slot, 4 + k as u32), &BODY, GUIDE_SIDE.x + ROW_TEXT_X, b.y + 10 + k as i32 * LINE_PITCH, l, color));
            }
            if idx == cur {
                v.push(rect_stroke(row_id(slot, 3), Layer::Widget, GUIDE_SIDE.x + 4, b.y + 2, GUIDE_SIDE.w - 8, b.h - 4, GREEN));
            } else {
                v.push(line(row_id(slot, 6), GUIDE_SIDE.x + 8, b.bottom() - 1, GUIDE_SIDE.right() - 8, b.bottom() - 1, GRID));
            }
        }
        v
    }

    fn resync(&mut self) -> Vec<SceneOp> {
        let mut ops = Vec::new();
        self.drawn.sync(self.nodes(), &mut ops);
        ops
    }

    /// Move the current step; returns the update ops.
    pub fn go(&mut self, dir: StepDir) -> Vec<SceneOp> {
        let total = self.steps.steps.len();
        let next = match dir {
            StepDir::Next => (self.steps.current + 1).min(total.saturating_sub(1)),
            StepDir::Back => self.steps.current.saturating_sub(1),
        };
        self.steps.current = next;
        self.scroll.reveal(next, rows_visible(ROW_H));
        self.resync()
    }

    pub fn on_tap(&mut self, node: NodeId) -> (Vec<SceneOp>, Option<GuideAnswer>) {
        let dir = match node.path_hash() {
            n if n == ids::GUIDE_NEXT => StepDir::Next,
            n if n == ids::GUIDE_BACK => StepDir::Back,
            _ => return (Vec::new(), None),
        };
        let total = self.steps.steps.len();
        let movable = match dir {
            StepDir::Next => self.steps.current + 1 < total,
            StepDir::Back => self.steps.current > 0,
        };
        if !movable {
            return (Vec::new(), None);
        }
        let ops = self.go(dir);
        (ops, Some(GuideAnswer::Step { steps_id: self.steps.id.clone(), dir }))
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
