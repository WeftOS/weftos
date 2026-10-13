//! `Console`: the host-facing API (contract §2) over a display session.
//! It owns the models (avatar, status, chat log, page, guide view) and
//! turns every call into a batch of scene ops for the session.

use weftos_display_client::{DisplayError, DisplaySession, ObjectEvent, SENSOR_PTT_DOWN, SENSOR_PTT_UP};
use weftos_leaf_scene::{NodeId, SceneOp};

use crate::avatar::Avatar;
use crate::chat_log::ChatLog;
use crate::chat_view::ChatView;
use crate::chrome::Chrome;
use crate::guide::{Checklist, ChecklistView, GuideAnswer, GuideView, Steps, StepsView, Survey, SurveyView};
use crate::page::{PageDoc, PageView};
use crate::prim::text;
use crate::status::{AgentState, StatusLine, StatusView};
use crate::theme::{ids, nid, BODY, CHAT, CHAT_GUIDE, DIM, GUIDE_HEAD, PAD};

/// Shown in the simulator's system-strip placeholder.
const APP_TITLE: &str = "chief of staff";
/// A pause in dragging this long ends the gesture (the client reports
/// no pointer-up, so swipes and sub-line remainders reset here).
const GESTURE_IDLE_MS: u32 = 250;

/// The three layouts of ADR-118 §1a.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Layout {
    #[default]
    Split,
    Page,
    Guide,
}

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

pub struct Console<'a> {
    session: &'a mut DisplaySession,
    chrome: Chrome,
    avatar: Avatar,
    status: StatusView,
    chat: ChatView,
    layout: Layout,
    page_doc: PageDoc,
    page: Option<PageView>,
    guide: Option<GuideView>,
    time: String,
    now_ms: u32,
    last_drag_ms: Option<u32>,
    /// Batching: ops wait here until [`Console::flush`] instead of going
    /// out one publish per call.
    batching: bool,
    queued: Vec<SceneOp>,
}

/// More queued ops than this (a flush that keeps failing) collapse into
/// one full redraw from the models.
const QUEUE_CAP: usize = 2048;

type Res<T = ()> = Result<T, DisplayError>;

impl<'a> Console<'a> {
    /// Draw the split layout on `session`.
    pub fn new(session: &'a mut DisplaySession) -> Res<Self> {
        Self::with_options(session, ConsoleOptions::default())
    }

    pub fn with_options(session: &'a mut DisplaySession, options: ConsoleOptions) -> Res<Self> {
        let mut c = Self {
            session,
            chrome: Chrome {
                system_strip_placeholder: options.system_strip_placeholder,
            },
            avatar: Avatar::new(),
            status: StatusView::new(),
            chat: ChatView::new(),
            layout: Layout::Split,
            page_doc: PageDoc::example(),
            page: None,
            guide: None,
            time: "--:--".into(),
            now_ms: 0,
            last_drag_ms: None,
            batching: false,
            queued: Vec::new(),
        };
        c.redraw()?;
        Ok(c)
    }

    /// The session, for polling events from the host loop.
    pub fn session(&mut self) -> &mut DisplaySession {
        self.session
    }

    pub fn layout(&self) -> Layout {
        self.layout
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

    pub fn guide(&self) -> Option<&GuideView> {
        self.guide.as_ref()
    }

    pub fn in_page_mode(&self) -> bool {
        self.layout == Layout::Page
    }

    /// Where the chat log is drawn in the current layout (the host puts
    /// its development typing line over its last row).
    pub fn chat_area(&self) -> crate::theme::Box {
        self.chat.area()
    }

    /// Recycled line ids currently on screen (chat, or page lines).
    pub fn live_line_ids(&self) -> Vec<NodeId> {
        match (self.layout, &self.page) {
            (Layout::Page, Some(p)) => p.live_ids(),
            _ => self.chat.live_ids(),
        }
    }

    fn apply(&mut self, ops: Vec<SceneOp>) -> Res {
        if ops.is_empty() {
            return Ok(());
        }
        if !self.batching {
            return self.session.apply(&ops);
        }
        // A batch that opens with Clear is a full frame: it supersedes
        // everything queued before it (latest wins).
        if matches!(ops.first(), Some(SceneOp::Clear)) {
            self.queued.clear();
        }
        self.queued.extend(ops);
        if self.queued.len() > QUEUE_CAP {
            self.redraw()?;
        }
        Ok(())
    }

    /// Queue ops until [`Console::flush`] (one publish per host frame, so a
    /// burst of agent events cannot flood the daemon). Off by default.
    pub fn set_batching(&mut self, on: bool) {
        self.batching = on;
    }

    /// Publish the queued ops as one batch. When the session says the op
    /// rate is exceeded nothing is lost: the queue stays for the next call.
    pub fn flush(&mut self) -> Res {
        if self.queued.is_empty() {
            return Ok(());
        }
        match self.session.apply(&self.queued) {
            Ok(()) => {
                self.queued.clear();
                Ok(())
            }
            Err(DisplayError::OpRate { .. }) => Ok(()),
            Err(e) => {
                self.queued.clear();
                Err(e)
            }
        }
    }

    /// Clear the display and draw the current layout from the models.
    fn redraw(&mut self) -> Res {
        let mut ops = vec![SceneOp::Clear];
        ops.extend(self.chrome.common_ops(APP_TITLE, &self.time));
        match self.layout {
            Layout::Split => {
                ops.extend(self.chrome.split_ops());
                ops.extend(self.avatar.all_ops());
                ops.extend(self.status.ops());
                self.chat.set_area(CHAT);
                self.chat.render(&mut ops);
            }
            Layout::Page => {
                let doc = self.page_doc.clone();
                let view = self.page.get_or_insert_with(|| PageView::new(&doc));
                ops.extend(view.open_ops());
            }
            Layout::Guide => {
                ops.extend(self.chrome.guide_ops());
                ops.extend(self.status.ops_compact());
                self.chat.set_area(CHAT_GUIDE);
                self.chat.render(&mut ops);
                match self.guide.as_mut() {
                    Some(g) => ops.extend(g.open_ops()),
                    None => ops.push(SceneOp::Insert(text(
                        ids::GUIDE_TITLE,
                        &BODY,
                        GUIDE_HEAD.x + PAD + 2,
                        GUIDE_HEAD.y + 20,
                        "no guide yet",
                        DIM,
                    ))),
                }
            }
        }
        self.apply(ops)
    }

    /// Switch layouts. Returning to `Page` or `Guide` shows the last
    /// page or guide view again.
    pub fn set_layout(&mut self, layout: Layout) -> Res {
        if layout == self.layout {
            return Ok(());
        }
        if self.layout == Layout::Page {
            self.page = None; // rebuilt from `page_doc` next time
        }
        self.layout = layout;
        self.redraw()
    }

    pub fn set_agent_state(&mut self, state: AgentState) -> Res {
        self.status.set_state(state.clone());
        let mut ops = self.avatar.set_state(state);
        match self.layout {
            Layout::Split => ops.extend(self.status.ops()),
            Layout::Guide => ops = self.status.ops_compact(),
            Layout::Page => return Ok(()),
        }
        self.apply(ops)
    }

    pub fn set_status(&mut self, status: StatusLine) -> Res {
        self.status.set_status(status);
        self.apply_status()
    }

    fn apply_status(&mut self) -> Res {
        let ops = match self.layout {
            Layout::Split => self.status.ops(),
            Layout::Guide => self.status.ops_compact(),
            Layout::Page => return Ok(()),
        };
        self.apply(ops)
    }

    /// Wall-clock text (`HH:MM`) for the status region and the strip
    /// placeholder.
    pub fn set_time(&mut self, hhmm: &str) -> Res {
        self.time = hhmm.to_string();
        self.status.set_time(hhmm);
        let mut ops = self.chrome.common_ops(APP_TITLE, &self.time);
        match self.layout {
            Layout::Split => ops.extend(self.status.ops()),
            Layout::Guide => ops.extend(self.status.ops_compact()),
            Layout::Page => {}
        }
        self.apply(ops)
    }

    pub fn push_user_line(&mut self, text: impl Into<String>) -> Res {
        self.chat.log_mut().push_user(text);
        self.render_chat()
    }

    /// Append `delta_or_full` to agent message `msg_id` (a new entry
    /// when `msg_id` is new).
    pub fn push_agent_text(&mut self, delta_or_full: &str, msg_id: u64) -> Res {
        self.chat.log_mut().push_agent(delta_or_full, msg_id);
        self.render_chat()
    }

    fn render_chat(&mut self) -> Res {
        if self.layout == Layout::Page {
            return Ok(());
        }
        let mut ops = Vec::new();
        self.chat.render(&mut ops);
        self.apply(ops)
    }

    /// Show `page` in the page layout.
    pub fn show_page(&mut self, page: PageDoc) -> Res {
        self.page_doc = page;
        self.page = None;
        self.layout = Layout::Page;
        self.redraw()
    }

    /// Back to the split from the page layout.
    pub fn hide_page(&mut self) -> Res {
        if self.layout != Layout::Page {
            return Ok(());
        }
        self.set_layout(Layout::Split)
    }

    fn show_guide(&mut self, view: GuideView) -> Res {
        self.guide = Some(view);
        self.layout = Layout::Guide;
        self.redraw()
    }

    pub fn show_checklist(&mut self, list: Checklist) -> Res {
        self.show_guide(GuideView::Checklist(ChecklistView::new(list)))
    }

    /// The agent sets a checklist item; a no-op for another checklist.
    pub fn set_item(&mut self, checklist_id: &str, item_id: &str, done: bool) -> Res {
        let ops = match self.guide.as_mut() {
            Some(GuideView::Checklist(v)) if v.list().id == checklist_id => v.set_item(item_id, done),
            _ => return Ok(()),
        };
        if self.layout != Layout::Guide {
            return Ok(());
        }
        self.apply(ops)
    }

    pub fn show_steps(&mut self, steps: Steps) -> Res {
        self.show_guide(GuideView::Steps(StepsView::new(steps)))
    }

    /// Ask one survey question (2–6 choices).
    pub fn ask(&mut self, survey: Survey) -> Res {
        self.show_guide(GuideView::Survey(SurveyView::new(survey)))
    }

    /// Route one display event. Guide touches come back as answers.
    pub fn on_event(&mut self, ev: ObjectEvent) -> Res<Option<GuideAnswer>> {
        match ev {
            ObjectEvent::Drag { node, dx, dy } => {
                self.last_drag_ms = Some(self.now_ms);
                self.on_drag(node, dx, dy)?;
                Ok(None)
            }
            ObjectEvent::Tap { node } => {
                self.release_gestures();
                self.on_tap(node)
            }
            ObjectEvent::Sensor { name, .. } => {
                match name.as_str() {
                    SENSOR_PTT_DOWN => self.set_ptt(true)?,
                    SENSOR_PTT_UP => self.set_ptt(false)?,
                    _ => {}
                }
                Ok(None)
            }
        }
    }

    fn set_ptt(&mut self, active: bool) -> Res {
        self.status.set_ptt(active);
        self.apply_status()
    }

    fn on_drag(&mut self, node: NodeId, dx: i32, dy: i32) -> Res {
        let on_log = node == nid(ids::CHAT_INPUT) || node == nid(ids::CHAT_MARKER_BG);
        match self.layout {
            Layout::Page => {
                let Some(p) = self.page.as_mut() else { return Ok(()) };
                if node != nid(ids::PAGE_INPUT) {
                    return Ok(());
                }
                let (ops, close) = p.drag(dx, dy);
                if close {
                    return self.set_layout(Layout::Split);
                }
                self.apply(ops)
            }
            Layout::Guide if !on_log => {
                let Some(g) = self.guide.as_mut() else { return Ok(()) };
                let (ops, close) = g.on_drag(node, dx, dy);
                if close {
                    return self.set_layout(Layout::Split);
                }
                self.apply(ops)
            }
            _ => {
                if on_log && self.chat.log_mut().scroll_by_px(dy) {
                    return self.render_chat();
                }
                Ok(())
            }
        }
    }

    fn on_tap(&mut self, node: NodeId) -> Res<Option<GuideAnswer>> {
        if self.layout != Layout::Page && node == nid(ids::CHAT_MARKER_BG) {
            if self.chat.log_mut().scroll_to_bottom() {
                self.render_chat()?;
            }
            return Ok(None);
        }
        if self.layout == Layout::Guide {
            if let Some(g) = self.guide.as_mut() {
                let (ops, answer) = g.on_tap(node);
                self.apply(ops)?;
                return Ok(answer);
            }
        }
        Ok(None)
    }

    fn release_gestures(&mut self) {
        self.chat.log_mut().end_gesture();
        if let Some(p) = self.page.as_mut() {
            p.release();
        }
        if let Some(g) = self.guide.as_mut() {
            g.release();
        }
        self.last_drag_ms = None;
    }

    /// Drain the session's events into `on_event`, collecting answers.
    pub fn pump_events(&mut self) -> Res<Vec<GuideAnswer>> {
        let mut answers = Vec::new();
        while let Some(ev) = self.session.poll_event() {
            if let Some(a) = self.on_event(ev)? {
                answers.push(a);
            }
        }
        Ok(answers)
    }

    /// Advance animations and gesture timing to `now_ms` (host
    /// monotonic clock).
    pub fn tick(&mut self, now_ms: u32) -> Res {
        self.now_ms = now_ms;
        if matches!(self.last_drag_ms, Some(t) if now_ms.saturating_sub(t) > GESTURE_IDLE_MS) {
            self.release_gestures();
        }
        let ops = self.avatar.tick(now_ms);
        if self.layout != Layout::Split {
            return Ok(());
        }
        self.apply(ops)
    }
}
