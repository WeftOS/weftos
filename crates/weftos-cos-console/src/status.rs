//! Status region (nodes 200–299): agent state word, current task,
//! model, lanes running, push-to-talk indicator and the time.

use weftos_leaf_scene::{Node, SceneOp};

use crate::prim::{circle_fill, circle_stroke, text};
use crate::text::{fit, truncate};
use crate::theme::{AMBER, BODY, DIM, GREEN, PAD, SMALL, STATUS};

/// What the host reports (contract §2).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StatusLine {
    pub task: String,
    pub model: String,
    pub lanes_running: u32,
    pub ptt_active: bool,
}

/// Agent state as the console shows it (contract §2).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum AgentState {
    #[default]
    Idle,
    Listening,
    Thinking,
    Speaking,
    Blocked(String),
}

impl AgentState {
    pub fn word(&self) -> &'static str {
        match self {
            AgentState::Idle => "IDLE",
            AgentState::Listening => "LISTENING",
            AgentState::Thinking => "THINKING",
            AgentState::Speaking => "SPEAKING",
            AgentState::Blocked(_) => "BLOCKED",
        }
    }

    pub fn is_blocked(&self) -> bool {
        matches!(self, AgentState::Blocked(_))
    }
}

const ID_STATE: u32 = 200;
const ID_TASK: u32 = 201;
const ID_DETAIL: u32 = 202;
const ID_PTT_RING: u32 = 203;
const ID_PTT_DOT: u32 = 204;
const ID_PTT_LABEL: u32 = 205;
const ID_TIME: u32 = 206;

const ROW1_Y: i32 = STATUS.y + 20;
const ROW2_Y: i32 = STATUS.y + 46;
const TIME_COLS: i32 = 5;
const TIME_X: i32 = STATUS.right() - PAD - TIME_COLS * BODY.cell_w;
const PTT_CX: i32 = TIME_X - 26;
const PTT_CY: i32 = ROW1_Y + 10;
const STATE_COLS: i32 = 10;
const TASK_X: i32 = STATUS.x + PAD + (STATE_COLS + 1) * BODY.cell_w;

#[derive(Debug, Clone, Default)]
pub struct StatusView {
    state: AgentState,
    status: StatusLine,
    time: String,
}

impl StatusView {
    pub fn new() -> Self {
        Self {
            time: "--:--".into(),
            ..Default::default()
        }
    }

    pub fn state(&self) -> &AgentState {
        &self.state
    }

    pub fn status(&self) -> &StatusLine {
        &self.status
    }

    pub fn set_state(&mut self, state: AgentState) {
        self.state = state;
    }

    pub fn set_status(&mut self, status: StatusLine) {
        self.status = status;
    }

    pub fn set_ptt(&mut self, active: bool) {
        self.status.ptt_active = active;
    }

    pub fn set_time(&mut self, hhmm: &str) {
        self.time = hhmm.to_string();
    }

    /// All status nodes for the current values.
    pub fn nodes(&self) -> Vec<Node> {
        let accent = if self.state.is_blocked() { AMBER } else { GREEN };
        let x0 = STATUS.x + PAD;
        let task_cols = ((PTT_CX - 14 - TASK_X) / BODY.cell_w).max(4) as usize;
        let task = if self.status.task.is_empty() {
            "-".to_string()
        } else {
            truncate(&self.status.task, task_cols)
        };
        let detail_cols = ((PTT_CX - 14 - x0) / SMALL.cell_w).max(8) as usize;
        let model = if self.status.model.is_empty() {
            "-"
        } else {
            self.status.model.as_str()
        };
        let lanes = match self.status.lanes_running {
            0 => "no lanes running".to_string(),
            1 => "1 lane running".to_string(),
            n => format!("{n} lanes running"),
        };
        let detail = truncate(&format!("model {model}   {lanes}"), detail_cols);
        let (dot_color, dot_visible) = if self.status.ptt_active {
            (GREEN, true)
        } else {
            (DIM, false)
        };
        let mut dot = circle_fill(ID_PTT_DOT, PTT_CX, PTT_CY, 5, dot_color);
        dot.style.visible = dot_visible;
        vec![
            text(ID_STATE, &BODY, x0, ROW1_Y, fit(self.state.word(), STATE_COLS as usize), accent),
            text(ID_TASK, &BODY, TASK_X, ROW1_Y, task, DIM),
            text(ID_DETAIL, &SMALL, x0, ROW2_Y, detail, DIM),
            circle_stroke(ID_PTT_RING, PTT_CX, PTT_CY, 7, if self.status.ptt_active { GREEN } else { DIM }),
            dot,
            text(ID_PTT_LABEL, &SMALL, PTT_CX - 9, ROW2_Y, "PTT", DIM),
            text(ID_TIME, &BODY, TIME_X, ROW1_Y, fit(&self.time, TIME_COLS as usize), GREEN),
        ]
    }

    pub fn ops(&self) -> Vec<SceneOp> {
        self.nodes().into_iter().map(SceneOp::Update).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::ids;

    #[test]
    fn nodes_stay_in_status_range_and_inside_box() {
        let mut v = StatusView::new();
        v.set_status(StatusLine {
            task: "a very long task description that will not fit on the line".into(),
            model: "claude-sonnet-5-5".into(),
            lanes_running: 3,
            ptt_active: true,
        });
        for n in v.nodes() {
            assert!(ids::STATUS.contains(&n.id.path_hash()));
            let r = n.aabb().unwrap();
            let x = weftos_leaf_scene::from_px_q8(r.x);
            let right = weftos_leaf_scene::from_px_q8(r.right());
            let bottom = weftos_leaf_scene::from_px_q8(r.bottom());
            assert!(x >= STATUS.x && right <= STATUS.right(), "{:?} overflows x", n.id);
            assert!(bottom <= STATUS.bottom(), "{:?} overflows y", n.id);
        }
    }

    #[test]
    fn blocked_uses_amber_word() {
        let mut v = StatusView::new();
        v.set_state(AgentState::Blocked("approval".into()));
        let word = &v.nodes()[0];
        assert_eq!(word.style.fill, Some(AMBER));
    }
}
