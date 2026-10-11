//! The guide layout's views (ADR-118 §1a): checklist, steps and survey,
//! drawn in the L-shaped guide area (left column plus top right).
//! Nodes 500–999: header fixtures at 500..560, rows from 560 in
//! strides of ten.

use std::collections::BTreeMap;

use weftos_leaf_scene::{Layer, Node, NodeId, SceneOp};

use crate::prim::{hit_rect, rect_stroke, tappable, text};
use crate::text::{center, truncate, wrap};
use crate::theme::{ids, FontMetrics, Box, BODY, DIM, GREEN, GUIDE_HEAD, GUIDE_SIDE, PAD, TOUCH_MIN};

pub use crate::guide_checklist::ChecklistView;
pub use crate::guide_steps::StepsView;
pub use crate::guide_survey::SurveyView;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ChecklistItem {
    pub id: String,
    pub text: String,
    pub done: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Checklist {
    pub id: String,
    pub title: String,
    pub items: Vec<ChecklistItem>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Steps {
    pub id: String,
    pub title: String,
    pub steps: Vec<String>,
    /// Index of the current step.
    pub current: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Choice {
    pub n: u8,
    pub caption: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Survey {
    pub id: String,
    pub question: String,
    pub explain: String,
    /// 2..=6 choices.
    pub choices: Vec<Choice>,
    /// 1-based question number and the total, shown as `Q2/5`.
    pub index: u32,
    pub total: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepDir {
    Next,
    Back,
}

/// What a touch in a guide view means to the agent (contract §2).
/// `Back` on a survey is this lane's addition beside `Skip`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuideAnswer {
    Answer { survey_id: String, choice_n: u8 },
    Toggle { checklist_id: String, item_id: String, done: bool },
    Step { steps_id: String, dir: StepDir },
    Skip { survey_id: String },
    Back { survey_id: String },
}

/// Horizontal travel on the guide header that returns to the split.
pub const SWIPE_PX: i32 = 120;
/// First row's top edge in the left column (below the "GUIDE" label).
pub const ROWS_TOP: i32 = GUIDE_SIDE.y + 40;
/// Text column inside a row, after the box or button.
pub const ROW_TEXT_X: i32 = 56;
pub const ROW_COLS: usize = BODY.cols(GUIDE_SIDE.w - ROW_TEXT_X - 8);
/// Head text column.
pub const HEAD_X: i32 = GUIDE_HEAD.x + PAD + 2;
pub const HEAD_COLS: usize = BODY.cols(GUIDE_HEAD.w - 2 * PAD - 4);
pub const BUTTON_W: i32 = 120;
pub const BUTTON_Y: i32 = GUIDE_HEAD.bottom() - TOUCH_MIN - 10;

/// Node id for part `k` of side row `slot`.
pub const fn row_id(slot: usize, k: u32) -> u32 {
    ids::GUIDE_ROWS + slot as u32 * ids::GUIDE_ROW_STRIDE + k
}

/// Inverse of [`row_id`]: `(slot, part)`.
pub fn row_of(node: NodeId) -> Option<(usize, u32)> {
    let n = node.path_hash();
    if !ids::GUIDE.contains(&n) || n < ids::GUIDE_ROWS {
        return None;
    }
    let off = n - ids::GUIDE_ROWS;
    Some(((off / ids::GUIDE_ROW_STRIDE) as usize, off % ids::GUIDE_ROW_STRIDE))
}

/// Keeps what is drawn and emits only the difference.
#[derive(Debug, Clone, Default)]
pub struct Drawn {
    nodes: BTreeMap<u32, Node>,
}

impl Drawn {
    /// Bring the scene to exactly `want`: changed nodes are updated,
    /// absent ones removed.
    pub fn sync(&mut self, want: Vec<Node>, ops: &mut Vec<SceneOp>) {
        let mut next = BTreeMap::new();
        for n in want {
            let id = n.id.path_hash();
            if self.nodes.get(&id) != Some(&n) {
                ops.push(SceneOp::Update(n.clone()));
            }
            next.insert(id, n);
        }
        for id in self.nodes.keys() {
            if !next.contains_key(id) {
                ops.push(SceneOp::Remove(crate::theme::nid(*id)));
            }
        }
        self.nodes = next;
    }

    pub fn ids(&self) -> Vec<u32> {
        self.nodes.keys().copied().collect()
    }

    pub fn close(&mut self, ops: &mut Vec<SceneOp>) {
        self.sync(Vec::new(), ops);
    }
}

/// Rightward-swipe detector over incremental drags.
#[derive(Debug, Clone, Copy, Default)]
pub struct Swipe {
    dx: i32,
    dy: i32,
}

impl Swipe {
    /// Feed a drag delta; true when the gesture became a swipe right.
    pub fn feed(&mut self, dx: i32, dy: i32) -> bool {
        self.dx += dx;
        self.dy += dy;
        if self.dx >= SWIPE_PX && self.dx > 2 * self.dy.abs() {
            *self = Self::default();
            return true;
        }
        false
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// Row scrolling for the side list, in whole rows.
#[derive(Debug, Clone, Copy, Default)]
pub struct RowScroll {
    pub offset: usize,
    px: i32,
}

impl RowScroll {
    /// Drag by `dy` pixels over rows of `row_h`; true when the offset
    /// moved. `max` is the largest offset allowed.
    pub fn drag(&mut self, dy: i32, row_h: i32, max: usize) -> bool {
        self.px += dy;
        let rows = self.px / row_h;
        self.px -= rows * row_h;
        if rows == 0 {
            return false;
        }
        let before = self.offset;
        let target = self.offset as i64 - rows as i64;
        self.offset = target.clamp(0, max as i64) as usize;
        before != self.offset
    }

    pub fn reset_px(&mut self) {
        self.px = 0;
    }

    /// Keep row `idx` inside a window of `visible` rows.
    pub fn reveal(&mut self, idx: usize, visible: usize) {
        if idx < self.offset {
            self.offset = idx;
        } else if idx >= self.offset + visible {
            self.offset = idx + 1 - visible;
        }
    }
}

/// Touch target over the head band, for the swipe back to the split.
pub fn head_input() -> Node {
    hit_rect(ids::GUIDE_HEAD_INPUT, Layer::Bg, GUIDE_HEAD.x, GUIDE_HEAD.y, GUIDE_HEAD.w, GUIDE_HEAD.h)
}

/// Title at the top left of the head band, leaving `reserve` cells on
/// the right for a counter.
pub fn head_title(title: &str, reserve: usize) -> Node {
    let cols = HEAD_COLS.saturating_sub(reserve + 2);
    text(ids::GUIDE_TITLE, &BODY, HEAD_X, GUIDE_HEAD.y + 20, truncate(title, cols), GREEN)
}

/// Right-aligned counter (`3/7`, `Q2/5`, `Step 2 of 5`) on the title row.
pub fn head_counter(s: &str) -> Node {
    let x = GUIDE_HEAD.right() - PAD - 2 - s.chars().count() as i32 * BODY.cell_w;
    text(ids::GUIDE_COUNTER, &BODY, x, GUIDE_HEAD.y + 20, s, GREEN)
}

/// Wrapped head text from `y` down, at most `max_lines`, on the
/// `GUIDE_TEXT` ids.
pub fn head_lines(s: &str, font: &FontMetrics, cols: usize, y: i32, pitch: i32, max_lines: usize, color: weftos_leaf_scene::Rgba) -> Vec<Node> {
    wrap(s, cols)
        .into_iter()
        .take(max_lines)
        .enumerate()
        .map(|(i, l)| text(ids::GUIDE_TEXT + i as u32, font, HEAD_X, y + i as i32 * pitch, l, color))
        .collect()
}

/// A 120×64 rectangular button: frame plus centred label. `first_id`
/// is the frame (the touch target); the label is `first_id + 1`.
pub fn button(first_id: u32, x: i32, label: &str, bright: bool) -> Vec<Node> {
    let color = if bright { GREEN } else { DIM };
    let frame = tappable(rect_stroke(first_id, Layer::Widget, x, BUTTON_Y, BUTTON_W, TOUCH_MIN, color));
    let label = text(
        first_id + 1,
        &BODY,
        x,
        BUTTON_Y + (TOUCH_MIN - BODY.cell_h) / 2,
        center(label, BODY.cols(BUTTON_W)),
        color,
    );
    vec![frame, label]
}

/// Back and next (or skip) buttons at the foot of the head band.
pub fn back_button(label: &str, bright: bool) -> Vec<Node> {
    button(ids::GUIDE_BACK, GUIDE_HEAD.x + PAD, label, bright)
}

pub fn next_button(label: &str, bright: bool) -> Vec<Node> {
    button(ids::GUIDE_NEXT, GUIDE_HEAD.right() - PAD - BUTTON_W, label, bright)
}

/// Row touch target spanning the left column.
pub fn row_input(slot: usize, y: i32, h: i32) -> Node {
    hit_rect(row_id(slot, 0), Layer::Widget, GUIDE_SIDE.x, y, GUIDE_SIDE.w, h)
}

/// The box a row occupies.
pub fn row_box(slot: usize, row_h: i32) -> Box {
    Box::new(GUIDE_SIDE.x, ROWS_TOP + slot as i32 * row_h, GUIDE_SIDE.w, row_h)
}

/// Rows that fit the left column at `row_h`.
pub const fn rows_visible(row_h: i32) -> usize {
    ((GUIDE_SIDE.bottom() - ROWS_TOP) / row_h) as usize
}

/// One of the three views, so the console can hold "the guide".
#[derive(Debug, Clone)]
pub enum GuideView {
    Checklist(ChecklistView),
    Steps(StepsView),
    Survey(SurveyView),
}

impl GuideView {
    pub fn open_ops(&mut self) -> Vec<SceneOp> {
        match self {
            Self::Checklist(v) => v.open_ops(),
            Self::Steps(v) => v.open_ops(),
            Self::Survey(v) => v.open_ops(),
        }
    }

    pub fn on_tap(&mut self, node: NodeId) -> (Vec<SceneOp>, Option<GuideAnswer>) {
        match self {
            Self::Checklist(v) => v.on_tap(node),
            Self::Steps(v) => v.on_tap(node),
            Self::Survey(v) => v.on_tap(node),
        }
    }

    /// Returns `(ops, close)`; `close` asks for the split layout.
    pub fn on_drag(&mut self, node: NodeId, dx: i32, dy: i32) -> (Vec<SceneOp>, bool) {
        match self {
            Self::Checklist(v) => v.on_drag(node, dx, dy),
            Self::Steps(v) => v.on_drag(node, dx, dy),
            Self::Survey(v) => v.on_drag(node, dx, dy),
        }
    }

    pub fn release(&mut self) {
        match self {
            Self::Checklist(v) => v.release(),
            Self::Steps(v) => v.release(),
            Self::Survey(v) => v.release(),
        }
    }

    pub fn live_ids(&self) -> Vec<u32> {
        match self {
            Self::Checklist(v) => v.live_ids(),
            Self::Steps(v) => v.live_ids(),
            Self::Survey(v) => v.live_ids(),
        }
    }
}
