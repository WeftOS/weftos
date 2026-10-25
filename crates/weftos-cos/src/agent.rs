//! The turn loop. One utterance in; `state`, `status`, `token`, `tool` and
//! `message` events out (contract §3). Turns run one at a time in the order
//! received. A `Confirm`-gated tool call is held as a pending action and
//! asked about; the next utterance answers it.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::{Mutex, broadcast};

use crate::CosError;
use crate::conversation::{AnswerKind, Conversation, GuideAnswer, HOST_ENDPOINT, Source, TURN_HISTORY_CAP, TurnId, TurnMessage, UserUtterance, valid_endpoint_name};
use crate::events::{AgentState, CardKind, Event, redact};
use crate::glance_store::GlanceStore;
use crate::guide::{Resolution, UiHandle};
use crate::prompt::system_prompt_with;
use crate::routing::{ModelRoutes, Route, Tier};
use crate::tools::cards::Focus;
use crate::tools::{Gate, ToolLog, ToolRegistry};
use crate::traces::{Trace, TraceBuilder, TraceStore};

/// Default tool rounds per turn (`models.max_tool_rounds`) before the agent
/// is made to answer with what it has.
pub const DEFAULT_MAX_TOOL_ROUNDS: usize = 6;
/// Appended (never stored) to the request that forces a final answer.
const ANSWER_NOW: &str = "[host] No more tool calls are available this turn. Answer the owner's question now in plain language, using only the tool results above. Lead with the answer in one sentence. If something could not be read, say so.";
/// Messages kept in the rolling history (system prompt excluded).
const HISTORY_LIMIT: usize = 40;
const MAX_TOKENS: i32 = 2400;

/// How a turn continues after one tool call.
enum Flow {
    Continue,
    /// Held for a confirmation turn.
    Held,
    /// The call failed; its error went back to the model.
    Failed,
    /// A survey question is on screen; the turn ends with it spoken.
    Asked(String),
}

/// A `Confirm` tool call waiting for the owner's yes.
#[derive(Debug, Clone)]
struct PendingAction {
    /// The turn that asked, and the endpoint it came from: an `mcp` or
    /// `test-*` endpoint can answer only a gate its own endpoint raised.
    turn_id: String,
    endpoint: String,
    tool: String,
    args: Value,
    summary: String,
}

struct Inner {
    routes: ModelRoutes,
    tools: ToolRegistry,
    log: ToolLog,
    ui: UiHandle,
    tx: broadcast::Sender<Event>,
    history: Mutex<Vec<Value>>,
    pending: std::sync::Mutex<Option<PendingAction>>,
    /// One trace per finished turn, for `GET /v1/cos/traces`.
    traces: std::sync::Mutex<TraceStore>,
    turn_gate: Mutex<()>,
    max_rounds: AtomicUsize,
    focus: std::sync::Mutex<Option<Focus>>,
    lanes_running: AtomicU32,
    /// The last 64 turns: who they came from and whether they were spoken.
    turns: std::sync::Mutex<std::collections::VecDeque<TurnRecord>>,
    /// The last [`TURN_HISTORY_CAP`] final messages, for `GET /v1/cos/turns`.
    messages: std::sync::Mutex<std::collections::VecDeque<TurnMessage>>,
    /// The latest `state` and `status` events, for `GET /v1/cos/status`.
    snapshot: std::sync::Mutex<Snapshot>,
    /// Remote endpoints whose engine is `none`: their replies are never
    /// flagged `speak`. Another endpoint's setting does not touch this.
    silent_endpoints: std::sync::Mutex<std::collections::HashSet<String>>,
    /// The glance card and art, and the panel mirror (`GET /v1/cos/glance`).
    glance: GlanceStore,
}

#[derive(Default)]
struct Snapshot {
    state: Option<AgentState>,
    reason: Option<String>,
    task: Option<String>,
    model: Option<String>,
}

struct TurnRecord {
    id: String,
    endpoint: String,
    voice: bool,
}

#[derive(Clone)]
pub struct CosAgent {
    inner: Arc<Inner>,
}

impl CosAgent {
    /// Build the agent. The `ui.*` tools are added to `tools` here, since
    /// they need the agent's emitter and guide state.
    pub fn new(routes: ModelRoutes, tools: ToolRegistry, log: ToolLog) -> Self {
        let (tx, _) = broadcast::channel(256);
        let quick_model = routes.quick().model_id.clone();
        let ui = UiHandle::new(tx.clone());
        let tools = crate::tools::ui::register(tools, &ui);
        let glance = GlanceStore::default();
        let tools = crate::tools::glance::register(tools, &ui, &glance);
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            // Mirror the panel from the agent's own events.
            let (mut rx, mirror) = (tx.subscribe(), glance.clone());
            rt.spawn(async move {
                loop {
                    match rx.recv().await {
                        Ok(ev) => mirror.observe(&ev),
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(_) => break,
                    }
                }
            });
        }
        Self {
            inner: Arc::new(Inner {
                routes,
                tools,
                log,
                ui,
                tx,
                history: Mutex::new(Vec::new()),
                pending: std::sync::Mutex::new(None),
                traces: std::sync::Mutex::new(TraceStore::default()),
                turn_gate: Mutex::new(()),
                max_rounds: AtomicUsize::new(DEFAULT_MAX_TOOL_ROUNDS),
                focus: std::sync::Mutex::new(None),
                lanes_running: AtomicU32::new(0),
                turns: std::sync::Mutex::new(std::collections::VecDeque::new()),
                messages: std::sync::Mutex::new(std::collections::VecDeque::new()),
                // Idle with the quick-tier model from the start, so a status
                // read before the first turn is not null.
                snapshot: std::sync::Mutex::new(Snapshot { state: Some(AgentState::Idle), model: Some(quick_model), ..Snapshot::default() }),
                silent_endpoints: std::sync::Mutex::new(std::collections::HashSet::new()),
                glance,
            }),
        }
    }

    /// Tool rounds per turn before a tool-free answer is forced (min 1).
    pub fn with_max_tool_rounds(self, rounds: usize) -> Self {
        self.inner.max_rounds.store(rounds.max(1), Ordering::Relaxed);
        self
    }

    /// The card focus the `card.*` tools share, so the status line can name
    /// the card in play.
    pub fn with_focus(self, focus: Focus) -> Self {
        *self.inner.focus.lock().unwrap_or_else(|e| e.into_inner()) = Some(focus);
        self
    }

    /// Keep the glance card and art under `dir` (owner-only files), and
    /// restore a card that is still live.
    pub fn with_glance_dir(self, dir: std::path::PathBuf) -> Self {
        self.inner.glance.set_dir(dir);
        self
    }

    pub fn glance_store(&self) -> &GlanceStore {
        &self.inner.glance
    }

    fn record_turn(&self, id: &TurnId, endpoint: String, voice: bool) {
        let mut v = self.inner.turns.lock().unwrap_or_else(|e| e.into_inner());
        v.push_back(TurnRecord { id: id.0.clone(), endpoint, voice });
        while v.len() > 64 {
            v.pop_front();
        }
    }

    fn remember_user(&self, id: &TurnId, endpoint: &str, text: &str) {
        self.inner.remember(id.clone(), "user", endpoint, redact(text.trim()));
    }

    /// Whether `turn` began as a spoken utterance. Remembers the last 64 turns.
    pub fn is_voice_turn(&self, turn: &TurnId) -> bool {
        self.inner.turns.lock().unwrap_or_else(|e| e.into_inner()).iter().any(|t| t.id == turn.0 && t.voice)
    }

    /// Whether the agent process itself reads this turn's reply aloud: a
    /// spoken turn of the `host` endpoint. A remote endpoint speaks its own.
    pub fn host_speaks(&self, turn: &TurnId) -> bool {
        self.inner.turns.lock().unwrap_or_else(|e| e.into_inner()).iter().any(|t| t.id == turn.0 && t.voice && t.endpoint == HOST_ENDPOINT)
    }

    /// The endpoint `turn` belongs to, while it is among the last 64.
    pub fn turn_endpoint(&self, turn: &TurnId) -> Option<String> {
        self.inner.turns.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|t| t.id == turn.0).map(|t| t.endpoint.clone())
    }

    /// Set which remote endpoints are silent (`voice` engine `none`). Replaces
    /// the previous set.
    pub fn set_silent_endpoints(&self, names: impl IntoIterator<Item = String>) {
        *self.inner.silent_endpoints.lock().unwrap_or_else(|e| e.into_inner()) = names.into_iter().collect();
    }

    /// Run a turn to completion (tests and the typed dev loop use this; the
    /// API spawns it).
    pub async fn run_turn(&self, turn_id: TurnId, utterance: UserUtterance) {
        let inner = self.inner.clone();
        // The endpoint is read before a survey answer rewrites the utterance.
        let endpoint = self.turn_endpoint(&turn_id).or_else(|| utterance.endpoint.clone()).unwrap_or_else(|| HOST_ENDPOINT.to_string());
        let _serial = inner.turn_gate.lock().await;
        let started = std::time::Instant::now();
        inner.ui.lock().current_turn = Some(turn_id.clone());
        // A spoken/typed number or "skip" answers a pending survey question.
        let utterance = inner.ui.resolve_spoken(utterance);
        inner.emit(Event::state(AgentState::Thinking));
        let mut tb = TraceBuilder::new();
        let result = inner.turn(&turn_id, &endpoint, &utterance, &mut tb).await;
        match result {
            Ok(()) => {
                inner.record_trace(tb.finish(&turn_id.0, &endpoint, started.elapsed().as_millis() as u64));
                inner.emit(Event::state(AgentState::Idle));
            }
            Err(e) => {
                let reason = redact(&e.to_string());
                tb.failed = true;
                inner.emit(Event::Message { turn_id: turn_id.clone(), text: format!("I hit a problem: {reason}"), is_final: true, endpoint: None, speak: false });
                inner.record_trace(tb.finish(&turn_id.0, &endpoint, started.elapsed().as_millis() as u64));
                inner.emit(Event::blocked(reason));
            }
        }
    }
}

#[async_trait]
impl Conversation for CosAgent {
    async fn utter(&self, utterance: UserUtterance) -> Result<TurnId, CosError> {
        let text = utterance.text.trim();
        if text.is_empty() {
            return Err(CosError::Config("empty utterance".into()));
        }
        if text.chars().count() > 8000 {
            return Err(CosError::Config("utterance longer than 8000 characters".into()));
        }
        let endpoint = match utterance.endpoint.as_deref() {
            None => HOST_ENDPOINT.to_string(),
            Some(e) if valid_endpoint_name(e) => e.to_string(),
            Some(e) => return Err(CosError::Config(format!("invalid endpoint {e:?}: use 1 to 32 characters of a-z, 0-9, _ or -"))),
        };
        let turn_id = TurnId::new();
        self.remember_user(&turn_id, &endpoint, &utterance.text);
        self.record_turn(&turn_id, endpoint, utterance.source == Source::Voice);
        let agent = self.clone();
        let id = turn_id.clone();
        tokio::spawn(async move { agent.run_turn(id, utterance).await });
        Ok(turn_id)
    }

    async fn answer(&self, answer: GuideAnswer) -> Result<TurnId, CosError> {
        self.answer_from(answer, None).await
    }

    async fn answer_from(&self, answer: GuideAnswer, endpoint: Option<String>) -> Result<TurnId, CosError> {
        let turn_id = TurnId::new();
        if answer.kind == AnswerKind::Answer
            && let Some(card) = self.inner.glance.live(chrono::Utc::now()).filter(|c| c.id == answer.id)
        {
            let (card, choice) = self.inner.glance.answer(&card.id, &answer.value, chrono::Utc::now())?;
            self.inner.ui.emit(Event::Card { card: CardKind::Clear, id: card.id, title: String::new(), detail: String::new(), alert: String::new(), options: Vec::new(), ttl_minutes: 0, reason: "answered".into() });
            let endpoint = endpoint.filter(|e| valid_endpoint_name(e)).unwrap_or_else(|| HOST_ENDPOINT.to_string());
            let utterance = UserUtterance::touch(format!("Answer to card \"{}\": {}", card.title, choice)).from_endpoint(endpoint.clone());
            self.remember_user(&turn_id, &endpoint, &utterance.text);
            self.record_turn(&turn_id, endpoint, false);
            let agent = self.clone();
            let id = turn_id.clone();
            tokio::spawn(async move { agent.run_turn(id, utterance).await });
            return Ok(turn_id);
        }
        match self.inner.ui.apply_answer(&answer)? {
            Resolution::Turn(utterance) => {
                self.remember_user(&turn_id, HOST_ENDPOINT, &utterance.text);
                self.record_turn(&turn_id, HOST_ENDPOINT.to_string(), false);
                let agent = self.clone();
                let id = turn_id.clone();
                tokio::spawn(async move { agent.run_turn(id, utterance).await });
            }
            Resolution::Updated { note } => {
                // The view changed and was re-emitted; the model learns about
                // it on its next turn without being woken now.
                self.inner.history.lock().await.push(json!({"role": "user", "content": note}));
            }
        }
        Ok(turn_id)
    }

    fn glance(&self) -> serde_json::Value {
        self.inner.glance.snapshot(chrono::Utc::now())
    }

    fn glance_art_png(&self) -> Option<Vec<u8>> {
        self.inner.glance.art_png()
    }

    fn events(&self) -> broadcast::Receiver<Event> {
        self.inner.tx.subscribe()
    }

    fn recent_turns(&self, limit: usize) -> Vec<TurnMessage> {
        let m = self.inner.messages.lock().unwrap_or_else(|e| e.into_inner());
        m.iter().skip(m.len().saturating_sub(limit)).cloned().collect()
    }

    fn status(&self) -> serde_json::Value {
        let s = self.inner.snapshot.lock().unwrap_or_else(|e| e.into_inner());
        // A held action, if any: which turn and endpoint asked, and what for.
        // Never the arguments.
        let pending = self
            .inner
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|p| json!({"turn_id": p.turn_id, "endpoint": p.endpoint, "tool": p.tool, "summary": p.summary}));
        json!({
            "state": s.state,
            "reason": s.reason,
            "task": s.task,
            "model": s.model,
            "lanes_running": self.inner.lanes_running.load(Ordering::Relaxed),
            "pending": pending,
        })
    }

    fn traces(&self, since: Option<&str>, limit: usize) -> Vec<Trace> {
        self.inner.traces.lock().unwrap_or_else(|e| e.into_inner()).since(since, limit)
    }

    fn trace(&self, turn_id: &str) -> Option<Trace> {
        self.inner.traces.lock().unwrap_or_else(|e| e.into_inner()).get(turn_id)
    }

    fn tools(&self) -> Vec<serde_json::Value> {
        self.inner
            .tools
            .specs()
            .iter()
            .map(|s| json!({"id": s.id, "description": s.description, "gate": if s.gate == Gate::Confirm { "confirm" } else { "open" }, "parameters": s.parameters}))
            .collect()
    }

    async fn devices_status(&self) -> Result<serde_json::Value, CosError> {
        // The one read-only tool the control plane may run without a model
        // turn; it is looked up by id and must be an open gate.
        let tool = self.inner.tools.find("devices.status").ok_or_else(|| CosError::Config("devices.status is not bound on this host".into()))?;
        if tool.spec().gate != Gate::Open {
            return Err(CosError::Config("devices.status is not read-only on this host".into()));
        }
        Ok(tool.call(json!({})).await?.result)
    }
}

/// Whether an utterance from `asker` may answer a gate raised from `holder`.
/// The owner's own endpoints answer each other's gates; a programmatic
/// endpoint (`mcp`, `test-*`) answers only its own, so a test or tool client
/// can never lift a gate that the phone or the glasses asked.
fn may_answer(holder: &str, asker: &str) -> bool {
    holder == asker || !is_programmatic(asker)
}

/// The endpoint names the MCP server and the eval harness speak as.
pub fn is_programmatic(endpoint: &str) -> bool {
    endpoint == "mcp" || endpoint.strip_prefix("test-").is_some_and(|run| !run.is_empty())
}

/// Whether a successful tool result means a write really happened. Staging
/// (`board.propose`) is not one.
fn is_apply_class(tool: &str, result: &Value) -> bool {
    matches!(tool, "board.claim" | "board.note" | "work.kickoff") || result["applied"] == true
}

fn is_yes(text: &str) -> bool {
    let t = text.trim().trim_end_matches(['.', '!']).to_lowercase();
    matches!(t.as_str(), "yes" | "y" | "yes please" | "go" | "go ahead" | "do it" | "confirm" | "confirmed" | "ok" | "okay" | "yep" | "yeah" | "sure" | "proceed")
}

impl Inner {
    fn record_trace(&self, t: Trace) {
        self.traces.lock().unwrap_or_else(|e| e.into_inner()).push(t);
    }

    fn emit(&self, ev: Event) {
        let ev = self.attribute(ev).redacted();
        self.note_snapshot(&ev);
        if let Event::Message { turn_id, text, is_final: true, endpoint, .. } = &ev {
            self.remember(turn_id.clone(), "assistant", endpoint.as_deref().unwrap_or(HOST_ENDPOINT), text.clone());
        }
        let _ = self.tx.send(ev);
    }

    /// Remember the latest `state` and `status` events (already redacted).
    fn note_snapshot(&self, ev: &Event) {
        let mut s = self.snapshot.lock().unwrap_or_else(|e| e.into_inner());
        match ev {
            Event::State { state, reason } => {
                s.state = Some(state.clone());
                s.reason = reason.clone();
            }
            Event::Status { task, model, .. } => {
                s.task = Some(task.clone());
                s.model = Some(model.clone());
            }
            _ => {}
        }
    }

    /// Keep a final message for `GET /v1/cos/turns` (`text` is redacted).
    fn remember(&self, turn_id: TurnId, role: &str, endpoint: &str, text: String) {
        let mut m = self.messages.lock().unwrap_or_else(|e| e.into_inner());
        m.push_back(TurnMessage {
            turn_id,
            role: role.into(),
            endpoint: endpoint.into(),
            text,
            ts: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        });
        while m.len() > TURN_HISTORY_CAP {
            m.pop_front();
        }
    }

    /// Stamp a `message` with its turn's endpoint, and `speak` when a remote
    /// endpoint is to read it aloud itself.
    fn attribute(&self, ev: Event) -> Event {
        let Event::Message { turn_id, text, is_final, .. } = ev else { return ev };
        let rec = self.turns.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|t| t.id == turn_id.0).map(|t| (t.endpoint.clone(), t.voice));
        let (endpoint, speak) = match rec {
            Some((ep, voice)) => {
                let remote = ep != HOST_ENDPOINT;
                let silent = self.silent_endpoints.lock().unwrap_or_else(|e| e.into_inner()).contains(&ep);
                let speak = remote && voice && is_final && !silent;
                (Some(ep), speak)
            }
            None => (None, false),
        };
        Event::Message { turn_id, text, is_final, endpoint, speak }
    }

    fn status(&self, task: &str, model: &str) {
        self.emit(Event::Status {
            task: task.into(),
            model: model.into(),
            lanes_running: self.lanes_running.load(Ordering::Relaxed),
        });
    }

    async fn turn(&self, turn_id: &TurnId, endpoint: &str, utterance: &UserUtterance, tb: &mut TraceBuilder) -> Result<(), CosError> {
        let source = match utterance.source {
            Source::Voice => "voice",
            Source::Typed => "typed",
            Source::Touch => "touch",
        };
        {
            let mut h = self.history.lock().await;
            h.push(json!({"role": "user", "content": utterance.text.trim()}));
            trim_history(&mut h);
        }

        let held = {
            let mut p = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            if p.as_ref().is_some_and(|a| may_answer(&a.endpoint, endpoint)) { p.take() } else { None }
        };
        if let Some(pending) = held {
            return self.answer_pending(turn_id, utterance, pending, tb).await;
        }

        // One owner: another person's notes or data are refused outright,
        // with no model call and no tool.
        if crate::guard::asks_for_someone_elses_data(&utterance.text) {
            tb.model = "none".into();
            tb.role = "refusal".into();
            self.status("refusing", "none");
            let text = crate::guard::REFUSAL.to_string();
            self.history.lock().await.push(json!({"role": "assistant", "content": text}));
            self.emit(Event::state(AgentState::Speaking));
            self.emit(Event::Message { turn_id: turn_id.clone(), text, is_final: true, endpoint: None, speak: false });
            return Ok(());
        }

        let (tier, route) = self.routes.pick(&utterance.text);
        let task = self.task_name(&utterance.text, tier, source);
        self.status(&task, &route.model_id);
        tb.model = route.model_id.clone();
        tb.role = tier.label().to_string();
        tracing::info!(turn = %turn_id, tier = tier.label(), model = %route.model_id, "routed");

        let today = chrono::Local::now().format("%Y-%m-%d (%A)").to_string();
        // External (MCP) servers that were down get another chance here.
        self.tools.refresh().await;
        let offered = self.tools.specs_for(&utterance.text);
        tb.offered(offered.iter().map(|s| s.id.to_string()));
        let system = json!({"role": "system", "content": system_prompt_with(&offered, &today, &self.tools.catalog())});
        let specs: Vec<Value> = offered.iter().map(|s| s.openai_spec()).collect();

        let chain = self.routes.chain(route);
        let max_rounds = self.max_rounds.load(Ordering::Relaxed);
        let mut at = 0;
        let mut seen_calls: HashSet<String> = HashSet::new();
        let mut errors = 0usize;
        let mut force_final = false;
        let mut round = 0usize;
        loop {
            // The last round, a repeated call, or repeated errors: the model
            // gets no tools and an instruction to answer, so a model that
            // would otherwise keep calling tools must write its answer.
            let final_round = force_final || round >= max_rounds;
            let mut messages: Vec<Value> = {
                let h = self.history.lock().await;
                std::iter::once(system.clone()).chain(h.iter().cloned()).collect()
            };
            if final_round {
                messages.push(json!({"role": "user", "content": ANSWER_NOW}));
            }
            let no_specs: Vec<Value> = Vec::new();
            let none_choice = json!("none");
            let (call_specs, choice) = if final_round { (&no_specs, Some(&none_choice)) } else { (&specs, None) };
            // The primary model cannot answer: move down the chain and say so
            // on the status line, which always names the model actually
            // answering.
            tb.rounds += 1;
            let response = loop {
                match self.call_model(turn_id, chain[at], &messages, call_specs, choice).await {
                    Ok(r) => break r,
                    Err(e) if at + 1 < chain.len() => {
                        tracing::warn!(turn = %turn_id, from = %chain[at].model_id, to = %chain[at + 1].model_id, error = %redact(&e.to_string()), "model failed; falling back");
                        at += 1;
                        self.status(&format!("{task} (fallback)"), &chain[at].model_id);
                        tb.model = chain[at].model_id.clone();
                    }
                    Err(e) => return Err(e),
                }
            };
            let msg = &response["choices"][0]["message"];
            let content = msg["content"].as_str().unwrap_or("").trim().to_string();
            let calls = if final_round { Vec::new() } else { msg["tool_calls"].as_array().cloned().unwrap_or_default() };

            if calls.is_empty() {
                let text = if !content.is_empty() {
                    content
                } else if final_round {
                    self.summary_without_model().await
                } else {
                    "(no answer)".to_string()
                };
                // Board writes are only staged: an answer that says one was
                // applied, with no apply-class result behind it, is untrue.
                let text = if !tb.applied && crate::guard::claims_applied_write(&text) {
                    tb.gate("claim", "unbacked-write");
                    crate::guard::rewrite_unbacked_claim(&text, tb.proposal.as_deref())
                } else {
                    text
                };
                self.history.lock().await.push(json!({"role": "assistant", "content": text}));
                self.emit(Event::state(AgentState::Speaking));
                self.emit(Event::Message { turn_id: turn_id.clone(), text, is_final: true, endpoint: None, speak: false });
                return Ok(());
            }

            self.history.lock().await.push(json!({
                "role": "assistant",
                "content": if content.is_empty() { Value::Null } else { Value::String(content.clone()) },
                "tool_calls": calls,
            }));
            for call in &calls {
                // The same call with the same arguments twice in a turn adds
                // nothing: do not run it again, tell the model, and make the
                // next request the answer.
                let key = format!("{}|{}", call["function"]["name"], call["function"]["arguments"]);
                if !seen_calls.insert(key) {
                    let call_id = call["id"].as_str().unwrap_or("call");
                    self.push_tool_result(call_id, &json!({"error": "you already made this exact call this turn; its result is above. Answer now."})).await;
                    force_final = true;
                    continue;
                }
                match self.run_tool_call(turn_id, endpoint, call, tb).await? {
                    Flow::Continue => {}
                    Flow::Failed => {
                        errors += 1;
                        if errors >= 2 {
                            force_final = true;
                        }
                    }
                    // A confirmation question was asked; the turn ends here.
                    Flow::Held => return Ok(()),
                    // A survey question is on screen: speak it and end the
                    // turn, so the owner's answer always starts a fresh one.
                    Flow::Asked(question) => {
                        let text = if content.is_empty() { question } else { content.clone() };
                        self.history.lock().await.push(json!({"role": "assistant", "content": text}));
                        self.emit(Event::state(AgentState::Speaking));
                        self.emit(Event::Message { turn_id: turn_id.clone(), text, is_final: true, endpoint: None, speak: false });
                        return Ok(());
                    }
                }
            }
            round += 1;
        }
    }

    /// The reply when even the tool-free request produced no text: say what
    /// happened rather than nothing.
    async fn summary_without_model(&self) -> String {
        let h = self.history.lock().await;
        let n = h.iter().rev().take_while(|m| m["role"] != "user").filter(|m| m["role"] == "tool").count();
        format!("I read {n} tool result(s) but could not turn them into an answer; ask again, or narrow the question.")
    }

    /// A short real name for what the turn is about: the guide on screen,
    /// else the card in play, else the first words of the question. Falls
    /// back to the source (`typed turn`) only for an empty question.
    fn task_name(&self, text: &str, tier: Tier, source: &str) -> String {
        let guide = {
            let g = self.ui.lock();
            g.steps.values().map(|s| s.title.clone()).chain(g.checklists.values().map(|c| c.title.clone())).find(|t| !t.trim().is_empty())
        };
        let card = self.focus.lock().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(|f| f.get_titled());
        // A survey answer is host-written text, not words worth showing.
        let spoken = if text.starts_with("Answer to Q") { String::new() } else { summarize_question(text) };
        let name = if spoken.is_empty() { guide.or(card).unwrap_or_default() } else { guide.or(card).unwrap_or(spoken) };
        if name.is_empty() {
            return match tier {
                Tier::Quick => format!("{source} turn"),
                Tier::Plan => format!("planning ({source})"),
            };
        }
        match tier {
            Tier::Quick => name,
            Tier::Plan => format!("planning: {name}"),
        }
    }

    /// One model call, streamed. Token chunks are emitted as the provider
    /// sends them; tool calls arrive assembled in the returned response, and
    /// a provider error (also mid-stream) fails the call.
    async fn call_model(&self, turn_id: &TurnId, route: &Route, messages: &[Value], specs: &[Value], tool_choice: Option<&Value>) -> Result<Value, CosError> {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(64);
        let call = route
            .provider
            .complete_stream(&route.bare_model, messages, specs, Some(MAX_TOKENS), Some(0.3), tool_choice, tx);
        let forward = async {
            while let Some(text) = rx.recv().await {
                self.emit(Event::Token { turn_id: turn_id.clone(), text });
            }
        };
        let (result, ()) = tokio::join!(call, forward);
        let v = result.map_err(CosError::Model)?;
        if let Some(err) = v.get("error") {
            return Err(CosError::Model(err.to_string()));
        }
        Ok(v)
    }

    /// Execute one tool call from the model and say how the turn continues.
    async fn run_tool_call(&self, turn_id: &TurnId, endpoint: &str, call: &Value, tb: &mut TraceBuilder) -> Result<Flow, CosError> {
        let call_id = call["id"].as_str().unwrap_or("call").to_string();
        let name = call["function"]["name"].as_str().unwrap_or("").to_string();
        let args: Value = call["function"]["arguments"]
            .as_str()
            .and_then(|s| serde_json::from_str(s).ok())
            .or_else(|| call["function"]["arguments"].as_object().map(|o| Value::Object(o.clone())))
            .unwrap_or_else(|| json!({}));

        let Some(tool) = self.tools.find(&name) else {
            let text = format!("unknown tool `{name}`");
            tb.called(&name, &args, false, 0);
            self.log.record(turn_id, &name, &text, "refused");
            self.emit(Event::Tool { name: name.clone(), summary: text.clone(), status: "refused".into() });
            self.push_tool_result(&call_id, &json!({"error": text})).await;
            return Ok(Flow::Failed);
        };
        let spec = tool.spec();
        let id = spec.id.to_string();

        if spec.gate == Gate::Confirm {
            let summary = describe_action(&id, &args);
            self.log.record(turn_id, &id, &summary, "awaiting_confirmation");
            self.emit(Event::Tool { name: id.clone(), summary: summary.clone(), status: "awaiting_confirmation".into() });
            tb.gate(&id, "held");
            *self.pending.lock().unwrap_or_else(|e| e.into_inner()) = Some(PendingAction { turn_id: turn_id.0.clone(), endpoint: endpoint.to_string(), tool: id.clone(), args, summary: summary.clone() });
            let question = format!("About to {summary}. Say yes to go ahead; anything else cancels.");
            self.push_tool_result(&call_id, &json!({"held": true, "reason": "awaiting owner confirmation"})).await;
            self.history.lock().await.push(json!({"role": "assistant", "content": question}));
            self.emit(Event::Message { turn_id: turn_id.clone(), text: question, is_final: true, endpoint: None, speak: false });
            return Ok(Flow::Held);
        }

        let asked = id == "ui.ask";
        let question = args["question"].as_str().unwrap_or_default().to_string();
        self.emit(Event::Tool { name: id.clone(), summary: "running".into(), status: "running".into() });
        let traced_args = args.clone();
        let started = std::time::Instant::now();
        let outcome = tool.call(args).await;
        tb.called(&id, &traced_args, outcome.is_ok(), started.elapsed().as_millis() as u64);
        match outcome {
            Ok(out) => {
                if id == "board.propose" {
                    tb.proposal = out.result["staged_path"].as_str().map(|p| p.rsplit('/').next().unwrap_or(p).to_string());
                }
                if is_apply_class(&id, &out.result) {
                    tb.applied = true;
                }
                self.log.record(turn_id, &id, &out.summary, "ok");
                self.emit(Event::Tool { name: id, summary: out.summary, status: "ok".into() });
                self.push_tool_result(&call_id, &out.result).await;
                if asked {
                    return Ok(Flow::Asked(question));
                }
            }
            Err(e) => {
                let reason = redact(&e.to_string());
                self.log.record(turn_id, &id, &reason, "error");
                self.emit(Event::Tool { name: id, summary: reason.clone(), status: "error".into() });
                self.push_tool_result(&call_id, &json!({"error": reason})).await;
                return Ok(Flow::Failed);
            }
        }
        Ok(Flow::Continue)
    }

    async fn push_tool_result(&self, call_id: &str, result: &Value) {
        let content = redact(&serde_json::to_string(result).unwrap_or_default());
        self.history.lock().await.push(json!({"role": "tool", "tool_call_id": call_id, "content": content}));
    }

    /// The utterance after a held action: yes runs it, anything else cancels.
    async fn answer_pending(&self, turn_id: &TurnId, utterance: &UserUtterance, pending: PendingAction, tb: &mut TraceBuilder) -> Result<(), CosError> {
        self.status("confirmation", "none");
        tb.model = "none".into();
        tb.role = "confirmation".into();
        if !is_yes(&utterance.text) {
            tb.gate(&pending.tool, "cancelled");
            let text = format!("Cancelled: {}.", pending.summary);
            self.log.record(turn_id, &pending.tool, &pending.summary, "cancelled");
            self.emit(Event::Tool { name: pending.tool, summary: pending.summary, status: "cancelled".into() });
            self.history.lock().await.push(json!({"role": "assistant", "content": text}));
            self.emit(Event::Message { turn_id: turn_id.clone(), text, is_final: true, endpoint: None, speak: false });
            return Ok(());
        }
        let tool = self.tools.find(&pending.tool).ok_or_else(|| CosError::Tool {
            name: pending.tool.clone(),
            reason: "tool vanished".into(),
        })?;
        self.emit(Event::Tool { name: pending.tool.clone(), summary: pending.summary.clone(), status: "running".into() });
        tb.gate(&pending.tool, "confirmed");
        let traced_args = pending.args.clone();
        let started = std::time::Instant::now();
        let outcome = tool.call(pending.args).await;
        tb.called(&pending.tool, &traced_args, outcome.is_ok(), started.elapsed().as_millis() as u64);
        let text = match outcome {
            Ok(out) => {
                if out.result["started"] == true {
                    self.lanes_running.fetch_add(1, Ordering::Relaxed);
                }
                self.log.record(turn_id, &pending.tool, &out.summary, "ok");
                self.emit(Event::Tool { name: pending.tool.clone(), summary: out.summary.clone(), status: "ok".into() });
                format!("Done: {}.", out.summary)
            }
            Err(e) => {
                let reason = redact(&e.to_string());
                self.log.record(turn_id, &pending.tool, &reason, "error");
                self.emit(Event::Tool { name: pending.tool.clone(), summary: reason.clone(), status: "error".into() });
                format!("That failed: {reason}")
            }
        };
        self.history.lock().await.push(json!({"role": "assistant", "content": text}));
        self.emit(Event::Message { turn_id: turn_id.clone(), text, is_final: true, endpoint: None, speak: false });
        Ok(())
    }
}

/// One line the owner can say yes to. Arguments are shown only for the
/// fields a person needs to decide; never a credential.
fn describe_action(tool: &str, args: &Value) -> String {
    match tool {
        "work.kickoff" => {
            let card = args["card"].as_str().unwrap_or("?");
            let team = args["team"].as_str().unwrap_or("weftos-core");
            match args["lane"].as_str() {
                Some(lane) => format!("start the {lane} lane of {team} on {card}"),
                None => format!("start the {team} team on {card}"),
            }
        }
        "board.claim" => format!("claim {} on the board", args["ticket"].as_str().unwrap_or("?")),
        "board.note" => {
            let text = args["text"].as_str().unwrap_or("");
            let clipped: String = text.chars().take(80).collect();
            format!("comment on {}: \"{clipped}\"", args["ticket"].as_str().unwrap_or("?"))
        }
        other => {
            let shown: String = args.to_string().chars().take(160).collect();
            if args.as_object().is_some_and(|o| !o.is_empty()) { format!("run {other} with {shown}") } else { format!("run {other}") }
        }
    }
}

/// The first six words of the question, trimmed of punctuation and capitalised.
fn summarize_question(text: &str) -> String {
    let joined = text.split_whitespace().take(6).collect::<Vec<_>>().join(" ");
    let mut words: Vec<&str> = joined.trim_matches(|c: char| !c.is_alphanumeric()).split(' ').collect();
    // Do not end on a dangling article or preposition.
    while words.len() > 2 && matches!(words.last().map(|w| w.to_lowercase()).as_deref(), Some("the" | "a" | "an" | "of" | "on" | "in" | "to" | "and" | "for" | "is" | "are")) {
        words.pop();
    }
    let joined = words.join(" ");
    let mut chars = joined.chars();
    match chars.next() {
        Some(f) => f.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

fn trim_history(h: &mut Vec<Value>) {
    while h.len() > HISTORY_LIMIT {
        h.remove(0);
        // Never leave an orphan tool result at the head.
        while h.first().is_some_and(|m| m["role"] == "tool") {
            h.remove(0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn question_becomes_a_short_task_name() {
        assert_eq!(summarize_question("where are we on the pad? and also the glasses"), "Where are we on the pad");
        assert_eq!(summarize_question("  ?! "), "");
        assert_eq!(summarize_question("what is in progress on the board"), "What is in progress");
    }

    #[test]
    fn staging_is_not_an_applied_write() {
        assert!(!is_apply_class("board.propose", &json!({"staged_path": "/x/y.yaml", "applied": false})));
        assert!(!is_apply_class("board.read", &json!({})));
        assert!(is_apply_class("board.claim", &json!({})));
        assert!(is_apply_class("ext.write", &json!({"applied": true})));
    }

    #[test]
    fn yes_words() {
        assert!(is_yes("Yes."));
        assert!(is_yes("go ahead"));
        assert!(!is_yes("no"));
        assert!(!is_yes("yes but only the tester"));
    }

    #[test]
    fn describes_a_kickoff_for_a_human() {
        let s = describe_action("work.kickoff", &json!({"card": "WEFT-700", "lane": "developer"}));
        assert_eq!(s, "start the developer lane of weftos-core on WEFT-700");
    }
}
