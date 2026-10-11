//! The turn loop. One utterance in; `state`, `status`, `token`, `tool` and
//! `message` events out (contract §3). Turns run one at a time in the order
//! received. A `Confirm`-gated tool call is held as a pending action and
//! asked about; the next utterance answers it.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::{Mutex, broadcast};

use crate::CosError;
use crate::conversation::{Conversation, GuideAnswer, Source, TurnId, UserUtterance};
use crate::events::{AgentState, Event, redact};
use crate::guide::{Resolution, UiHandle};
use crate::prompt::system_prompt;
use crate::routing::{ModelRoutes, Route, Tier};
use crate::tools::{Gate, ToolLog, ToolRegistry};

/// Tool rounds per turn before the agent answers with what it has.
const MAX_ROUNDS: usize = 4;
/// Messages kept in the rolling history (system prompt excluded).
const HISTORY_LIMIT: usize = 40;
const MAX_TOKENS: i32 = 1200;

/// How a turn continues after one tool call.
enum Flow {
    Continue,
    /// Held for a confirmation turn.
    Held,
    /// A survey question is on screen; the turn ends with it spoken.
    Asked(String),
}

/// A `Confirm` tool call waiting for the owner's yes.
#[derive(Debug, Clone)]
struct PendingAction {
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
    pending: Mutex<Option<PendingAction>>,
    turn_gate: Mutex<()>,
    lanes_running: AtomicU32,
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
        let ui = UiHandle::new(tx.clone());
        let tools = crate::tools::ui::register(tools, &ui);
        Self {
            inner: Arc::new(Inner {
                routes,
                tools,
                log,
                ui,
                tx,
                history: Mutex::new(Vec::new()),
                pending: Mutex::new(None),
                turn_gate: Mutex::new(()),
                lanes_running: AtomicU32::new(0),
            }),
        }
    }

    /// Run a turn to completion (tests and the typed dev loop use this; the
    /// API spawns it).
    pub async fn run_turn(&self, turn_id: TurnId, utterance: UserUtterance) {
        let inner = self.inner.clone();
        let _serial = inner.turn_gate.lock().await;
        inner.ui.lock().current_turn = Some(turn_id.clone());
        // A spoken/typed number or "skip" answers a pending survey question.
        let utterance = inner.ui.resolve_spoken(utterance);
        inner.emit(Event::state(AgentState::Thinking));
        let result = inner.turn(&turn_id, &utterance).await;
        match result {
            Ok(()) => inner.emit(Event::state(AgentState::Idle)),
            Err(e) => {
                let reason = redact(&e.to_string());
                inner.emit(Event::Message { turn_id: turn_id.clone(), text: format!("I hit a problem: {reason}"), is_final: true });
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
        let turn_id = TurnId::new();
        let agent = self.clone();
        let id = turn_id.clone();
        tokio::spawn(async move { agent.run_turn(id, utterance).await });
        Ok(turn_id)
    }

    async fn answer(&self, answer: GuideAnswer) -> Result<TurnId, CosError> {
        let turn_id = TurnId::new();
        match self.inner.ui.apply_answer(&answer)? {
            Resolution::Turn(utterance) => {
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

    fn events(&self) -> broadcast::Receiver<Event> {
        self.inner.tx.subscribe()
    }
}

fn is_yes(text: &str) -> bool {
    let t = text.trim().trim_end_matches(['.', '!']).to_lowercase();
    matches!(t.as_str(), "yes" | "y" | "yes please" | "go" | "go ahead" | "do it" | "confirm" | "confirmed" | "ok" | "okay" | "yep" | "yeah" | "sure" | "proceed")
}

impl Inner {
    fn emit(&self, ev: Event) {
        let _ = self.tx.send(ev.redacted());
    }

    fn status(&self, task: &str, model: &str) {
        self.emit(Event::Status {
            task: task.into(),
            model: model.into(),
            lanes_running: self.lanes_running.load(Ordering::Relaxed),
        });
    }

    async fn turn(&self, turn_id: &TurnId, utterance: &UserUtterance) -> Result<(), CosError> {
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

        if let Some(pending) = self.pending.lock().await.take() {
            return self.answer_pending(turn_id, utterance, pending).await;
        }

        let (tier, route) = self.routes.pick(&utterance.text);
        let task = match tier {
            Tier::Quick => format!("{source} turn"),
            Tier::Plan => format!("planning ({source})"),
        };
        self.status(&task, &route.model_id);
        tracing::info!(turn = %turn_id, tier = tier.label(), model = %route.model_id, "routed");

        let today = chrono::Local::now().format("%Y-%m-%d (%A)").to_string();
        let system = json!({"role": "system", "content": system_prompt(&self.tools.specs(), &today)});
        let specs = self.tools.openai_specs();

        for _round in 0..MAX_ROUNDS {
            let messages: Vec<Value> = {
                let h = self.history.lock().await;
                std::iter::once(system.clone()).chain(h.iter().cloned()).collect()
            };
            let response = self.call_model(turn_id, route, &messages, &specs).await?;
            let msg = &response["choices"][0]["message"];
            let content = msg["content"].as_str().unwrap_or("").trim().to_string();
            let calls = msg["tool_calls"].as_array().cloned().unwrap_or_default();

            if calls.is_empty() {
                let text = if content.is_empty() { "(no answer)".to_string() } else { content };
                self.history.lock().await.push(json!({"role": "assistant", "content": text}));
                self.emit(Event::state(AgentState::Speaking));
                self.emit(Event::Message { turn_id: turn_id.clone(), text, is_final: true });
                return Ok(());
            }

            self.history.lock().await.push(json!({
                "role": "assistant",
                "content": if content.is_empty() { Value::Null } else { Value::String(content.clone()) },
                "tool_calls": calls,
            }));
            for call in &calls {
                match self.run_tool_call(turn_id, call).await? {
                    Flow::Continue => {}
                    // A confirmation question was asked; the turn ends here.
                    Flow::Held => return Ok(()),
                    // A survey question is on screen: speak it and end the
                    // turn, so the owner's answer always starts a fresh one.
                    Flow::Asked(question) => {
                        let text = if content.is_empty() { question } else { content.clone() };
                        self.history.lock().await.push(json!({"role": "assistant", "content": text}));
                        self.emit(Event::state(AgentState::Speaking));
                        self.emit(Event::Message { turn_id: turn_id.clone(), text, is_final: true });
                        return Ok(());
                    }
                }
            }
        }

        let text = "I ran out of tool rounds for this turn; here is what I have so far. Ask again to continue.".to_string();
        self.history.lock().await.push(json!({"role": "assistant", "content": text}));
        self.emit(Event::Message { turn_id: turn_id.clone(), text, is_final: true });
        Ok(())
    }

    /// One model call, streamed. Token chunks are emitted as the provider
    /// sends them; tool calls arrive assembled in the returned response, and
    /// a provider error (also mid-stream) fails the call.
    async fn call_model(&self, turn_id: &TurnId, route: &Route, messages: &[Value], specs: &[Value]) -> Result<Value, CosError> {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(64);
        let call = route
            .provider
            .complete_stream(&route.bare_model, messages, specs, Some(MAX_TOKENS), Some(0.3), None, tx);
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
    async fn run_tool_call(&self, turn_id: &TurnId, call: &Value) -> Result<Flow, CosError> {
        let call_id = call["id"].as_str().unwrap_or("call").to_string();
        let name = call["function"]["name"].as_str().unwrap_or("").to_string();
        let args: Value = call["function"]["arguments"]
            .as_str()
            .and_then(|s| serde_json::from_str(s).ok())
            .or_else(|| call["function"]["arguments"].as_object().map(|o| Value::Object(o.clone())))
            .unwrap_or_else(|| json!({}));

        let Some(tool) = self.tools.find(&name) else {
            let text = format!("unknown tool `{name}`");
            self.log.record(turn_id, &name, &text, "refused");
            self.emit(Event::Tool { name: name.clone(), summary: text.clone(), status: "refused".into() });
            self.push_tool_result(&call_id, &json!({"error": text})).await;
            return Ok(Flow::Continue);
        };
        let spec = tool.spec();
        let id = spec.id.to_string();

        if spec.gate == Gate::Confirm {
            let summary = describe_action(&id, &args);
            self.log.record(turn_id, &id, &summary, "awaiting_confirmation");
            self.emit(Event::Tool { name: id.clone(), summary: summary.clone(), status: "awaiting_confirmation".into() });
            *self.pending.lock().await = Some(PendingAction { tool: id.clone(), args, summary: summary.clone() });
            let question = format!("About to {summary}. Say yes to go ahead; anything else cancels.");
            self.push_tool_result(&call_id, &json!({"held": true, "reason": "awaiting owner confirmation"})).await;
            self.history.lock().await.push(json!({"role": "assistant", "content": question}));
            self.emit(Event::Message { turn_id: turn_id.clone(), text: question, is_final: true });
            return Ok(Flow::Held);
        }

        let asked = id == "ui.ask";
        let question = args["question"].as_str().unwrap_or_default().to_string();
        self.emit(Event::Tool { name: id.clone(), summary: "running".into(), status: "running".into() });
        match tool.call(args).await {
            Ok(out) => {
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
            }
        }
        Ok(Flow::Continue)
    }

    async fn push_tool_result(&self, call_id: &str, result: &Value) {
        let content = redact(&serde_json::to_string(result).unwrap_or_default());
        self.history.lock().await.push(json!({"role": "tool", "tool_call_id": call_id, "content": content}));
    }

    /// The utterance after a held action: yes runs it, anything else cancels.
    async fn answer_pending(&self, turn_id: &TurnId, utterance: &UserUtterance, pending: PendingAction) -> Result<(), CosError> {
        self.status("confirmation", "none");
        if !is_yes(&utterance.text) {
            let text = format!("Cancelled: {}.", pending.summary);
            self.log.record(turn_id, &pending.tool, &pending.summary, "cancelled");
            self.emit(Event::Tool { name: pending.tool, summary: pending.summary, status: "cancelled".into() });
            self.history.lock().await.push(json!({"role": "assistant", "content": text}));
            self.emit(Event::Message { turn_id: turn_id.clone(), text, is_final: true });
            return Ok(());
        }
        let tool = self.tools.find(&pending.tool).ok_or_else(|| CosError::Tool {
            name: pending.tool.clone(),
            reason: "tool vanished".into(),
        })?;
        self.emit(Event::Tool { name: pending.tool.clone(), summary: pending.summary.clone(), status: "running".into() });
        let text = match tool.call(pending.args).await {
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
        self.emit(Event::Message { turn_id: turn_id.clone(), text, is_final: true });
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
        other => format!("run {other}"),
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
