//! Survey view: one question at a time. Question and explanation in
//! the head band with back and skip; 2–6 large round numbered buttons
//! with captions down the left column. A tap fills the button and
//! answers.

use weftos_leaf_scene::{Node, NodeId, SceneOp};

use crate::guide::{
    back_button, head_counter, head_input, head_title, next_button, row_box, row_id,
    row_input, row_of, Drawn, GuideAnswer, Survey, Swipe, HEAD_COLS, HEAD_X,
};
use crate::prim::{circle_fill, circle_stroke, text};
use crate::text::wrap;
use crate::theme::{ids, BLACK, BODY, DIM, GREEN, GUIDE_HEAD, GUIDE_SIDE, LINE_PITCH, SMALL};

/// Row pitch for the round buttons (64 px button plus breathing room).
const ROW_H: i32 = 68;
const BUTTON_R: i32 = 30;
const CAPTION_X: i32 = 90;
const CAPTION_COLS: usize = BODY.cols(GUIDE_SIDE.w - CAPTION_X - 8);

#[derive(Debug, Clone)]
pub struct SurveyView {
    survey: Survey,
    answered: Option<u8>,
    swipe: Swipe,
    drawn: Drawn,
}

impl SurveyView {
    pub fn new(mut survey: Survey) -> Self {
        survey.choices.truncate(6);
        Self {
            survey,
            answered: None,
            swipe: Swipe::default(),
            drawn: Drawn::default(),
        }
    }

    pub fn survey(&self) -> &Survey {
        &self.survey
    }

    pub fn answered(&self) -> Option<u8> {
        self.answered
    }

    /// Draw the view from nothing (the layout was just cleared).
    pub fn open_ops(&mut self) -> Vec<SceneOp> {
        self.drawn = Drawn::default();
        let mut ops = vec![SceneOp::Insert(head_input())];
        self.drawn.sync(self.nodes(), &mut ops);
        ops
    }

    fn nodes(&self) -> Vec<Node> {
        let counter = format!("Q{}/{}", self.survey.index, self.survey.total);
        let mut v = vec![head_counter(&counter)];
        // The question takes the title row and up to two more lines.
        let q = wrap(&self.survey.question, HEAD_COLS.saturating_sub(counter.len() + 2));
        if let Some(first) = q.first() {
            v.push(head_title(first, counter.len()));
        }
        for (i, l) in q.iter().skip(1).take(2).enumerate() {
            v.push(text(ids::GUIDE_TEXT + i as u32, &BODY, HEAD_X, GUIDE_HEAD.y + 42 + i as i32 * LINE_PITCH, l.clone(), GREEN));
        }
        // The explanation follows the question's last line.
        let q_lines = q.len().clamp(1, 3) as i32;
        let explain_y = GUIDE_HEAD.y + 20 + q_lines * LINE_PITCH + 8;
        let explain_cols = SMALL.cols(GUIDE_HEAD.w - 2 * crate::theme::PAD - 4);
        for (i, l) in wrap(&self.survey.explain, explain_cols).into_iter().take(2).enumerate() {
            v.push(text(ids::GUIDE_TEXT + 3 + i as u32, &SMALL, HEAD_X, explain_y + i as i32 * 12, l, DIM));
        }
        v.extend(back_button("< back", self.survey.index > 1));
        v.extend(next_button("skip >", self.answered.is_none()));

        for (slot, c) in self.survey.choices.iter().enumerate() {
            let b = row_box(slot, ROW_H);
            let (cx, cy) = (GUIDE_SIDE.x + 46, b.y + ROW_H / 2);
            let filled = self.answered == Some(c.n);
            let dimmed = self.answered.is_some() && !filled;
            let color = if dimmed { DIM } else { GREEN };
            v.push(row_input(slot, b.y, b.h));
            if filled {
                v.push(circle_fill(row_id(slot, 1), cx, cy, BUTTON_R, GREEN));
                v.push(text(row_id(slot, 2), &BODY, cx - 5, cy - 10, format!("{}", c.n), BLACK));
            } else {
                v.push(circle_stroke(row_id(slot, 1), cx, cy, BUTTON_R, color));
                v.push(text(row_id(slot, 2), &BODY, cx - 5, cy - 10, format!("{}", c.n), color));
            }
            let lines = wrap(&c.caption, CAPTION_COLS);
            let n = lines.len().min(2) as i32;
            let y0 = cy - (n * LINE_PITCH - 2) / 2;
            for (k, l) in lines.into_iter().take(2).enumerate() {
                v.push(text(row_id(slot, 4 + k as u32), &BODY, GUIDE_SIDE.x + CAPTION_X, y0 + k as i32 * LINE_PITCH, l, color));
            }
        }
        v
    }

    fn resync(&mut self) -> Vec<SceneOp> {
        let mut ops = Vec::new();
        self.drawn.sync(self.nodes(), &mut ops);
        ops
    }

    pub fn on_tap(&mut self, node: NodeId) -> (Vec<SceneOp>, Option<GuideAnswer>) {
        let id = self.survey.id.clone();
        match node.path_hash() {
            n if n == ids::GUIDE_NEXT => {
                if self.answered.is_some() {
                    return (Vec::new(), None);
                }
                (Vec::new(), Some(GuideAnswer::Skip { survey_id: id }))
            }
            n if n == ids::GUIDE_BACK => {
                if self.survey.index <= 1 {
                    return (Vec::new(), None);
                }
                (Vec::new(), Some(GuideAnswer::Back { survey_id: id }))
            }
            _ => {
                let Some((slot, 0)) = row_of(node) else { return (Vec::new(), None) };
                let Some(c) = self.survey.choices.get(slot) else { return (Vec::new(), None) };
                let n = c.n;
                self.answered = Some(n);
                (self.resync(), Some(GuideAnswer::Answer { survey_id: id, choice_n: n }))
            }
        }
    }

    pub fn on_drag(&mut self, node: NodeId, dx: i32, dy: i32) -> (Vec<SceneOp>, bool) {
        if node.path_hash() == ids::GUIDE_HEAD_INPUT {
            return (Vec::new(), self.swipe.feed(dx, dy));
        }
        (Vec::new(), false)
    }

    pub fn release(&mut self) {
        self.swipe.reset();
    }

    pub fn live_ids(&self) -> Vec<u32> {
        self.drawn.ids()
    }
}
