//! `Console`: the host-facing API (contract §2) over a display session.
//! It owns the models (avatar, status, chat log, page) and turns every
//! call into a batch of scene ops for the session.

use weftos_leaf_scene::{NodeId, SceneOp};

use crate::avatar::Avatar;
use crate::chat_log::ChatLog;
use crate::chat_view::ChatView;
use crate::chrome::Chrome;
use crate::page::{PageDoc, PageView};
use crate::session::{DisplaySession, ObjectEvent};
use crate::status::{AgentState, StatusLine, StatusView};
use crate::theme::{ids, nid};

/// Shown in the simulator's system-strip placeholder.
const APP_TITLE: &str = "chief of staff";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConsoleOptions {
    /// Draw the system strip. True in the simulator; on the pad the
    /// firmware owns that band (ADR-117 §5).
    pub system_strip_placeholder: bool,
}

impl Default for ConsoleOptions {
    fn default() -> Self {
        Self {
            system_strip_placeholder: true,
        }
    }
}

pub struct Console<'a, S: DisplaySession> {
    session: &'a mut S,
    chrome: Chrome,
    avatar: Avatar,
    status: StatusView,
    chat: ChatView,
    page: Option<PageView>,
    time: String,
}

impl<'a, S: DisplaySession> Console<'a, S> {
    /// Draw the split view on `session`.
    pub fn new(session: &'a mut S) -> Result<Self, S::Error> {
        Self::with_options(session, ConsoleOptions::default())
    }

    pub fn with_options(session: &'a mut S, options: ConsoleOptions) -> Result<Self, S::Error> {
        let mut c = Self {
            session,
            chrome: Chrome {
                system_strip_placeholder: options.system_strip_placeholder,
            },
            avatar: Avatar::new(),
            status: StatusView::new(),
            chat: ChatView::new(),
            page: None,
            time: "--:--".into(),
        };
        c.draw_split()?;
        Ok(c)
    }

    /// The session, for polling events and rendering.
    pub fn session(&mut self) -> &mut S {
        self.session
    }

    pub fn chat(&self) -> &ChatLog {
        self.chat.log()
    }

    pub fn avatar(&self) -> &Avatar {
        &self.avatar
    }

    pub fn status(&self) -> &StatusView {
        &self.status
    }

    pub fn page(&self) -> Option<&PageView> {
        self.page.as_ref()
    }

    pub fn in_page_mode(&self) -> bool {
        self.page.is_some()
    }

    /// Chat and page line ids currently on screen (range tests).
    pub fn live_line_ids(&self) -> Vec<NodeId> {
        match &self.page {
            Some(p) => p.live_ids(),
            None => self.chat.live_ids(),
        }
    }

    fn apply(&mut self, ops: Vec<SceneOp>) -> Result<(), S::Error> {
        if ops.is_empty() {
            return Ok(());
        }
        self.session.apply(&ops)
    }

    /// Clear the display and draw the split view from the models.
    fn draw_split(&mut self) -> Result<(), S::Error> {
        let mut ops = vec![SceneOp::Clear];
        ops.extend(self.chrome.common_ops(APP_TITLE, &self.time));
        ops.extend(self.chrome.split_ops());
        ops.extend(self.avatar.all_ops());
        ops.extend(self.status.ops());
        self.chat.render(&mut ops);
        self.apply(ops)
    }

    pub fn set_agent_state(&mut self, state: AgentState) -> Result<(), S::Error> {
        self.status.set_state(state.clone());
        let mut ops = self.avatar.set_state(state);
        if self.in_page_mode() {
            return Ok(());
        }
        ops.extend(self.status.ops());
        self.apply(ops)
    }

    pub fn set_status(&mut self, status: StatusLine) -> Result<(), S::Error> {
        self.status.set_status(status);
        if self.in_page_mode() {
            return Ok(());
        }
        let ops = self.status.ops();
        self.apply(ops)
    }

    /// Wall-clock text (`HH:MM`) for the status region and the strip
    /// placeholder.
    pub fn set_time(&mut self, hhmm: &str) -> Result<(), S::Error> {
        self.time = hhmm.to_string();
        self.status.set_time(hhmm);
        let mut ops = self.chrome.common_ops(APP_TITLE, &self.time);
        if !self.in_page_mode() {
            ops.extend(self.status.ops());
        }
        self.apply(ops)
    }

    pub fn push_user_line(&mut self, text: impl Into<String>) -> Result<(), S::Error> {
        self.chat.log_mut().push_user(text);
        self.render_chat()
    }

    /// Append `delta_or_full` to agent message `msg_id` (a new entry
    /// when `msg_id` is new).
    pub fn push_agent_text(&mut self, delta_or_full: &str, msg_id: u64) -> Result<(), S::Error> {
        self.chat.log_mut().push_agent(delta_or_full, msg_id);
        self.render_chat()
    }

    fn render_chat(&mut self) -> Result<(), S::Error> {
        if self.in_page_mode() {
            return Ok(());
        }
        let mut ops = Vec::new();
        self.chat.render(&mut ops);
        self.apply(ops)
    }

    /// Expand the visual panel to the full content area and show
    /// `page`. The split comes back with `hide_page` or a swipe.
    pub fn show_page(&mut self, page: PageDoc) -> Result<(), S::Error> {
        let mut view = PageView::new(&page);
        let mut ops = vec![SceneOp::Clear];
        ops.extend(self.chrome.common_ops(APP_TITLE, &self.time));
        ops.extend(view.open_ops());
        let mut scratch = Vec::new();
        self.chat.clear(&mut scratch); // drop the chat nodes' bookkeeping; Clear took the nodes
        self.page = Some(view);
        self.apply(ops)
    }

    pub fn hide_page(&mut self) -> Result<(), S::Error> {
        if self.page.take().is_none() {
            return Ok(());
        }
        self.draw_split()
    }

    pub fn on_event(&mut self, ev: ObjectEvent) -> Result<(), S::Error> {
        match ev {
            ObjectEvent::Drag { node, dx, dy } => self.on_drag(node, dx, dy),
            ObjectEvent::Tap { node } => self.on_tap(node),
            ObjectEvent::Release { node: _ } => {
                self.chat.log_mut().end_gesture();
                if let Some(p) = self.page.as_mut() {
                    p.release();
                }
                Ok(())
            }
            ObjectEvent::Sensor { name, .. } => match name.as_str() {
                "ptt.down" => self.set_ptt(true),
                "ptt.up" => self.set_ptt(false),
                _ => Ok(()),
            },
        }
    }

    fn set_ptt(&mut self, active: bool) -> Result<(), S::Error> {
        self.status.set_ptt(active);
        if self.in_page_mode() {
            return Ok(());
        }
        let ops = self.status.ops();
        self.apply(ops)
    }

    fn on_drag(&mut self, node: NodeId, dx: i32, dy: i32) -> Result<(), S::Error> {
        if let Some(p) = self.page.as_mut() {
            if node == nid(ids::PAGE_INPUT) {
                let (ops, close) = p.drag(dx, dy);
                if close {
                    return self.hide_page();
                }
                return self.apply(ops);
            }
            return Ok(());
        }
        let on_log = node == nid(ids::CHAT_INPUT) || node == nid(ids::CHAT_MARKER_BG);
        if on_log && self.chat.log_mut().scroll_by_px(dy) {
            return self.render_chat();
        }
        Ok(())
    }

    fn on_tap(&mut self, node: NodeId) -> Result<(), S::Error> {
        if self.in_page_mode() {
            return Ok(());
        }
        if node == nid(ids::CHAT_MARKER_BG) && self.chat.log_mut().scroll_to_bottom() {
            return self.render_chat();
        }
        Ok(())
    }

    /// Drain the session's events into `on_event`. Returns how many
    /// were handled.
    pub fn pump_events(&mut self) -> Result<usize, S::Error> {
        let mut n = 0;
        while let Some(ev) = self.session.poll_event() {
            self.on_event(ev)?;
            n += 1;
        }
        Ok(n)
    }

    /// Advance animations to `now_ms` (host monotonic clock).
    pub fn tick(&mut self, now_ms: u32) -> Result<(), S::Error> {
        let ops = self.avatar.tick(now_ms);
        if self.in_page_mode() {
            return Ok(());
        }
        self.apply(ops)
    }
}
