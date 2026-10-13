//! The console host (ADR-118 C3): one synchronous loop that joins the
//! console screen to the chief-of-staff agent over its HTTP/SSE API
//! (contract §3) and carries the push-to-talk button to it (§4).
//!
//! The loop owns the display session on the main thread (SDL needs it
//! there). Each [`Host::step`] pumps display events into the console,
//! drains what the agent threads delivered, and ticks animations; the
//! binary calls it at about 30 frames a second.
//!
//! Policy that lives here, not in the console or the agent:
//! - `ptt.down` / `ptt.up` go to `POST /v1/cos/ptt`; the agent owns the
//!   microphone. The transcript comes back in the `up` reply and is shown
//!   as the user's line.
//! - Guide touches become `POST /v1/cos/answer`.
//! - With the agent unreachable the avatar shows `Blocked("agent
//!   offline")` and the stream is retried with backoff. A drop mid-stream
//!   ends any open agent line; the next token starts a new one.
//! - On open the host fetches the agent's recent turns
//!   (`GET /v1/cos/turns`) and fills the chat log before any live event is
//!   applied; events that arrive meanwhile are held, then replayed, and a
//!   turn already shown (by `turn_id` and role) is never drawn twice. A
//!   failed fetch is noted once and ignored; one retry follows the first
//!   stream connect.
//! - Development typing (Mac window only): Enter opens a typed line in
//!   the chat area, Enter sends it as a `typed` utterance, Escape closes
//!   it. While it is open the space bar types a space instead of talking.

pub mod agent_client;
pub mod agent_event;
pub mod input_line;
pub mod supervise;

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use serde_json::{json, Value};
use weftos_display_client::{
    DisplaySession, ObjectEvent, SensorValue, KEY_BACKSPACE, KEY_ESCAPE, KEY_RETURN, SENSOR_KEY_CHAR, SENSOR_KEY_NAMED, SENSOR_PTT_DOWN,
    SENSOR_PTT_UP,
};

use crate::{AgentState, Console, DisplayError, GuideAnswer, Layout, StatusLine, StepDir};
use agent_client::{AgentLink, ClientConfig, HistoryEntry, Inbound, Post, Posted};
use agent_event::{agent_state, checklist, page_from_markdown, parse_layout, steps, survey, AgentEvent};
use input_line::InputLine;

/// The agent's default bind (contract §3).
pub const DEFAULT_AGENT: &str = "http://127.0.0.1:18300";
/// The blocked reason shown while the agent cannot be reached.
pub const OFFLINE_REASON: &str = "agent offline";

#[derive(Debug, Clone)]
pub struct HostConfig {
    /// Base URL of the agent's conversation API.
    pub agent: String,
    /// Accept typed utterances from the window's keyboard.
    pub typing: bool,
    /// Reconnect backoff bounds for the event stream.
    pub reconnect_min: Duration,
    pub reconnect_max: Duration,
}

impl Default for HostConfig {
    fn default() -> Self {
        Self {
            agent: DEFAULT_AGENT.into(),
            typing: true,
            reconnect_min: Duration::from_millis(250),
            reconnect_max: Duration::from_secs(4),
        }
    }
}

/// How many earlier messages the host asks for on open.
pub const HISTORY_LIMIT: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HistoryState {
    /// A fetch is out; inbound traffic is held until it answers.
    Pending,
    Loaded,
    /// The fetch failed; `retry` says whether the first connect may try again.
    Failed { retry: bool },
}

/// An agent turn with text on screen.
struct Turn {
    msg_id: u64,
    streamed: String,
}

type Res<T = ()> = Result<T, DisplayError>;

pub struct Host<'a> {
    console: Console<'a>,
    cfg: HostConfig,
    link: AgentLink,
    online: bool,
    offline_shown: bool,
    turns: HashMap<String, Turn>,
    next_msg: u64,
    /// The agent's last status, kept so it can be restored after an outage.
    status: StatusLine,
    ptt_held: bool,
    input: Option<InputLine>,
    clock: String,
    history: HistoryState,
    /// A fetch failed once already: the retry stays quiet.
    history_failed_before: bool,
    /// Inbound messages held while the history fetch is out.
    held: Vec<Inbound>,
    /// Turns whose assistant text / user line are already in the log.
    shown_assistant: HashSet<String>,
    shown_user: HashSet<String>,
    /// Lines for the supervisor's log, drained by [`Host::take_notices`].
    notices: Vec<String>,
}

impl<'a> Host<'a> {
    /// Draw the console on `session` and start talking to the agent.
    pub fn open(session: &'a mut DisplaySession, cfg: HostConfig) -> Res<Self> {
        let mut console = Console::new(session)?;
        // From here on, ops queue and go out once per `step`.
        console.set_batching(true);
        let link = AgentLink::connect(ClientConfig {
            base: cfg.agent.clone(),
            reconnect_min: cfg.reconnect_min,
            reconnect_max: cfg.reconnect_max,
        });
        link.fetch_history(HISTORY_LIMIT);
        Ok(Self {
            console,
            cfg,
            link,
            online: false,
            offline_shown: false,
            turns: HashMap::new(),
            next_msg: 0,
            status: StatusLine::default(),
            ptt_held: false,
            input: None,
            clock: String::new(),
            history: HistoryState::Pending,
            history_failed_before: false,
            held: Vec::new(),
            shown_assistant: HashSet::new(),
            shown_user: HashSet::new(),
            notices: Vec::new(),
        })
    }

    /// Log lines the host wants written (a failed history fetch).
    pub fn take_notices(&mut self) -> Vec<String> {
        std::mem::take(&mut self.notices)
    }

    pub fn console(&self) -> &Console<'a> {
        &self.console
    }

    pub fn console_mut(&mut self) -> &mut Console<'a> {
        &mut self.console
    }

    /// Whether the event stream is open right now.
    pub fn is_online(&self) -> bool {
        self.online
    }

    /// The typed line's text while it is open.
    pub fn typing(&self) -> Option<&str> {
        self.input.as_ref().map(InputLine::text)
    }

    /// Set the status clock (`HH:MM`); redraws only when it changes.
    pub fn set_clock(&mut self, hhmm: &str) -> Res {
        if self.clock == hhmm {
            return Ok(());
        }
        self.clock = hhmm.to_string();
        self.console.set_time(hhmm)
    }

    /// One frame: display events in, agent messages in, animations on.
    /// Returns `false` once the display session has closed.
    pub fn step(&mut self, now_ms: u32) -> Res<bool> {
        while let Some(ev) = self.console.session().poll_event() {
            self.on_display_event(ev)?;
        }
        while let Some(msg) = self.link.try_recv() {
            self.on_inbound(msg)?;
        }
        self.console.tick(now_ms)?;
        self.console.flush()?;
        Ok(!self.console.session().is_closed())
    }

    /// Scene ops that bypass the console's queue (the typed line): the
    /// queue goes out first so the order on screen is the order drawn.
    fn draw(&mut self, ops: &[weftos_display_client::SceneOp]) -> Res {
        self.console.flush()?;
        self.console.session().apply(ops)
    }

    // ── display side ────────────────────────────────────────────────

    fn on_display_event(&mut self, ev: ObjectEvent) -> Res {
        if let ObjectEvent::Sensor { name, value } = &ev {
            match name.as_str() {
                SENSOR_PTT_DOWN => return self.on_ptt(true, ev),
                SENSOR_PTT_UP => return self.on_ptt(false, ev),
                SENSOR_KEY_CHAR | SENSOR_KEY_NAMED if self.cfg.typing => return self.on_key(name.clone(), value.clone()),
                _ => {}
            }
        }
        let before = self.console.layout();
        if let Some(answer) = self.console.on_event(ev)? {
            if let Some(body) = answer_body(answer) {
                self.link.post(Post::Answer(body));
            }
        }
        if self.console.layout() != before {
            self.refresh_input()?;
        }
        Ok(())
    }

    fn on_ptt(&mut self, down: bool, ev: ObjectEvent) -> Res {
        if self.input.is_some() {
            return Ok(()); // the space bar is typing
        }
        self.ptt_held = down;
        self.console.on_event(ev)?;
        if !self.online {
            return Ok(());
        }
        if down {
            self.console.set_agent_state(AgentState::Listening)?;
        }
        self.link.post(Post::Ptt { down });
        Ok(())
    }

    fn on_key(&mut self, name: String, value: SensorValue) -> Res {
        let SensorValue::Text(key) = value else { return Ok(()) };
        if self.console.layout() == Layout::Page {
            return Ok(());
        }
        let open = self.input.is_some();
        match (name.as_str(), open) {
            (SENSOR_KEY_NAMED, false) if key == KEY_RETURN => {
                let line = InputLine::new(self.console.chat_area());
                let ops = line.draw_ops();
                self.input = Some(line);
                self.draw(&ops)
            }
            (SENSOR_KEY_NAMED, true) if key == KEY_RETURN => self.send_typed(),
            (SENSOR_KEY_NAMED, true) if key == KEY_ESCAPE => self.close_input(),
            (SENSOR_KEY_NAMED, true) if key == KEY_BACKSPACE => {
                let ops = self.input.as_mut().map(InputLine::backspace).unwrap_or_default();
                self.draw(&ops)
            }
            (SENSOR_KEY_CHAR, true) => {
                let ops = self.input.as_mut().map(|l| l.push(&key)).unwrap_or_default();
                self.draw(&ops)
            }
            _ => Ok(()),
        }
    }

    fn send_typed(&mut self) -> Res {
        let text = self.input.as_mut().map(InputLine::take).unwrap_or_default();
        self.close_input()?;
        let text = text.trim().to_string();
        if text.is_empty() {
            return Ok(());
        }
        self.console.push_user_line(&text)?;
        self.link.post(Post::Utterance { text, source: "typed" });
        Ok(())
    }

    fn close_input(&mut self) -> Res {
        let Some(line) = self.input.take() else { return Ok(()) };
        self.draw(&line.close_ops())
    }

    /// After a redraw (layout change, guide view), put the typed line
    /// back over the chat area, or drop it where there is no chat.
    fn refresh_input(&mut self) -> Res {
        let Some(line) = self.input.as_mut() else { return Ok(()) };
        if self.console.layout() == Layout::Page {
            self.input = None;
            return Ok(());
        }
        line.set_area(self.console.chat_area());
        let ops = line.draw_ops();
        self.draw(&ops)
    }

    // ── agent side ──────────────────────────────────────────────────

    fn on_inbound(&mut self, msg: Inbound) -> Res {
        match msg {
            Inbound::History(result) => return self.on_history(result),
            m if self.history == HistoryState::Pending => {
                self.held.push(m);
                return Ok(());
            }
            _ => {}
        }
        match msg {
            Inbound::History(_) => Ok(()),
            Inbound::Connected => {
                self.online = true;
                if self.history == (HistoryState::Failed { retry: true }) {
                    self.history = HistoryState::Pending;
                    self.link.fetch_history(HISTORY_LIMIT);
                }
                if self.offline_shown {
                    self.offline_shown = false;
                    self.console.set_agent_state(AgentState::Idle)?;
                    self.apply_status()?;
                }
                Ok(())
            }
            Inbound::Disconnected { .. } => {
                self.online = false;
                // Open agent lines end here; the next token starts anew.
                self.turns.clear();
                if !self.offline_shown {
                    self.offline_shown = true;
                    self.console.set_agent_state(AgentState::Blocked(OFFLINE_REASON.into()))?;
                    let mut st = self.status_line();
                    st.task = "reconnecting".into();
                    self.console.set_status(st)?;
                }
                Ok(())
            }
            Inbound::Event(ev) => self.apply_event(ev),
            Inbound::Posted(p) => self.on_posted(p),
        }
    }

    /// The history reply: draw the earlier turns, then replay what was held.
    fn on_history(&mut self, result: Result<Vec<HistoryEntry>, String>) -> Res {
        let retried = self.history_failed_before;
        match result {
            Ok(entries) => {
                self.history = HistoryState::Loaded;
                // A turn whose live events are already held is drawn from them.
                for m in &self.held {
                    if let Inbound::Event(AgentEvent::Token { turn_id, .. } | AgentEvent::Message { turn_id, .. }) = m {
                        self.shown_assistant.insert(turn_id.clone());
                    }
                    if let Inbound::Posted(p) = m {
                        if let Some(id) = posted_turn_id(p) {
                            self.shown_user.insert(id);
                        }
                    }
                }
                for e in entries {
                    match e.role.as_str() {
                        "user" if self.shown_user.insert(e.turn_id.clone()) => self.console.push_user_line(&e.text)?,
                        "assistant" if self.shown_assistant.insert(e.turn_id.clone()) => {
                            let id = self.fresh_msg();
                            self.console.push_agent_text(&e.text, id)?;
                        }
                        _ => {}
                    }
                }
            }
            Err(e) => {
                self.history = HistoryState::Failed { retry: !retried };
                self.history_failed_before = true;
                if !retried {
                    self.notices.push(format!("console host: could not load recent turns: {e}"));
                }
            }
        }
        for m in std::mem::take(&mut self.held) {
            self.on_inbound(m)?;
        }
        Ok(())
    }

    fn apply_event(&mut self, ev: AgentEvent) -> Res {
        let before = self.console.layout();
        let redraws = matches!(ev, AgentEvent::Layout { .. } | AgentEvent::Page { .. } | AgentEvent::Checklist { .. } | AgentEvent::Steps { .. } | AgentEvent::Ask { .. });
        match ev {
            AgentEvent::State { state, reason } => {
                self.offline_shown = false;
                self.console.set_agent_state(agent_state(&state, reason.as_deref()))?;
            }
            AgentEvent::Status { task, model, lanes_running } => {
                self.status = StatusLine { task, model, lanes_running, ptt_active: false };
                self.apply_status()?;
            }
            AgentEvent::Token { turn_id, text } => {
                self.shown_assistant.insert(turn_id.clone());
                let id = self.turn_msg(&turn_id);
                if let Some(t) = self.turns.get_mut(&turn_id) {
                    t.streamed.push_str(&text);
                }
                if *self.console.status().state() == AgentState::Thinking {
                    self.console.set_agent_state(AgentState::Speaking)?;
                }
                self.console.push_agent_text(&text, id)?;
            }
            AgentEvent::Message { turn_id, text, is_final } => {
                self.shown_assistant.insert(turn_id.clone());
                let id = self.turn_msg(&turn_id);
                let streamed = self.turns.get(&turn_id).map(|t| t.streamed.clone()).unwrap_or_default();
                // The final message repeats what the tokens streamed; show
                // only what is new.
                let delta = match text.strip_prefix(streamed.as_str()) {
                    Some(rest) => rest.to_string(),
                    None if streamed.is_empty() => text.clone(),
                    None => format!("\n{text}"),
                };
                if !delta.is_empty() {
                    self.console.push_agent_text(&delta, id)?;
                }
                if is_final {
                    self.turns.remove(&turn_id);
                } else if let Some(t) = self.turns.get_mut(&turn_id) {
                    t.streamed = text;
                }
            }
            AgentEvent::Page { markdown, .. } => self.console.show_page(page_from_markdown(&markdown))?,
            AgentEvent::Layout { layout } => {
                if let Some(l) = parse_layout(&layout) {
                    self.console.set_layout(l)?;
                }
            }
            AgentEvent::Checklist { id, title, items } => self.console.show_checklist(checklist(id, title, items))?,
            AgentEvent::Steps { id, title, steps: list, current } => self.console.show_steps(steps(id, title, list, current))?,
            AgentEvent::Ask { id, question, explain, choices, index, total } => self.console.ask(survey(id, question, explain, choices, index, total))?,
            AgentEvent::Tool { .. } | AgentEvent::Unknown => {}
        }
        if redraws || self.console.layout() != before {
            self.refresh_input()?;
        }
        Ok(())
    }

    fn on_posted(&mut self, p: Posted) -> Res {
        if let Some(id) = posted_turn_id(&p) {
            self.shown_user.insert(id);
        }
        match (p.post, p.result) {
            (Post::Ptt { down: false }, Ok(v)) => match v["posted"]["text"].as_str() {
                Some(text) => self.console.push_user_line(text),
                None => {
                    let reason = v["nothing"].as_str().unwrap_or("nothing heard").to_string();
                    self.note(format!("(heard nothing: {reason})"))?;
                    self.console.set_agent_state(AgentState::Idle)
                }
            },
            (Post::Ptt { .. }, Err(e)) => {
                self.note(format!("(push-to-talk failed: {e})"))?;
                self.console.set_agent_state(AgentState::Idle)
            }
            (Post::Utterance { .. }, Err(e)) => self.note(format!("(not sent: {e})")),
            (Post::Answer(_), Err(e)) => self.note(format!("(answer refused: {e})")),
            _ => Ok(()),
        }
    }

    /// A short host-side note in the log, as a dim agent line.
    fn note(&mut self, text: String) -> Res {
        let id = self.fresh_msg();
        self.console.push_agent_text(&text, id)
    }

    fn status_line(&self) -> StatusLine {
        StatusLine { ptt_active: self.ptt_held, ..self.status.clone() }
    }

    fn apply_status(&mut self) -> Res {
        let st = self.status_line();
        self.console.set_status(st)
    }

    fn fresh_msg(&mut self) -> u64 {
        self.next_msg += 1;
        self.next_msg
    }

    /// The chat message id for `turn_id`, opened on first sight.
    fn turn_msg(&mut self, turn_id: &str) -> u64 {
        if let Some(t) = self.turns.get(turn_id) {
            return t.msg_id;
        }
        let id = self.fresh_msg();
        self.turns.insert(turn_id.to_string(), Turn { msg_id: id, streamed: String::new() });
        id
    }
}

/// The turn id of a user line this host posted (utterance or push-to-talk).
fn posted_turn_id(p: &Posted) -> Option<String> {
    let v = p.result.as_ref().ok()?;
    match p.post {
        Post::Utterance { .. } => v["turn_id"].as_str(),
        Post::Ptt { down: false } => v["posted"]["turn_id"].as_str(),
        _ => None,
    }
    .map(str::to_string)
}

/// A guide touch as the `POST /v1/cos/answer` body (contract §3).
pub fn answer_body(a: GuideAnswer) -> Option<Value> {
    Some(match a {
        GuideAnswer::Answer { survey_id, choice_n } => json!({"kind": "answer", "id": survey_id, "value": choice_n}),
        GuideAnswer::Toggle { checklist_id, item_id, done } => {
            json!({"kind": "toggle", "id": checklist_id, "value": {"item_id": item_id, "done": done}})
        }
        GuideAnswer::Step { steps_id, dir } => {
            let dir = match dir {
                StepDir::Next => "next",
                StepDir::Back => "back",
            };
            json!({"kind": "step", "id": steps_id, "value": dir})
        }
        GuideAnswer::Skip { survey_id } => json!({"kind": "skip", "id": survey_id, "value": null}),
        GuideAnswer::Back { survey_id } => json!({"kind": "back", "id": survey_id, "value": null}),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guide_answers_take_the_contract_shapes() {
        let a = answer_body(GuideAnswer::Answer { survey_id: "q1".into(), choice_n: 2 }).unwrap();
        assert_eq!(a, json!({"kind": "answer", "id": "q1", "value": 2}));
        let t = answer_body(GuideAnswer::Toggle { checklist_id: "c".into(), item_id: "pg".into(), done: true }).unwrap();
        assert_eq!(t["value"], json!({"item_id": "pg", "done": true}));
        let s = answer_body(GuideAnswer::Step { steps_id: "s".into(), dir: StepDir::Back }).unwrap();
        assert_eq!(s, json!({"kind": "step", "id": "s", "value": "back"}));
        let b = answer_body(GuideAnswer::Back { survey_id: "q1".into() }).unwrap();
        assert_eq!(b, json!({"kind": "back", "id": "q1", "value": null}));
    }
}
