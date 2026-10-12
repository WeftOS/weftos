//! Dashboard actions (ADR-108 P2): work the dashboard queues for this node and
//! hands over in the heartbeat answer, `{ok, actions: [{id, kind, payload,
//! created_at}]}`. Kinds: `install`, `update`, `remove`, `pair`, `route`, `agent.install`, `agent.uninstall`.
//!
//! The reporter keeps a bounded in-memory queue, acknowledges each action with
//! `POST /api/nodes/actions/{id}/result` (`{status, result}`, node token as the
//! bearer), first `running` and then the handler's outcome, and keeps a bounded
//! log for `weaver dashboard actions`.
//!
//! Nothing here executes anything from an action. Each kind is answered by an
//! [`ActionHandler`]; until P3/P4 plug real ones in, [`NotImplemented`] answers
//! `failed` and unknown kinds answer `failed` with a clear error.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Most actions waiting to run at once; the dashboard redelivers the rest.
pub const MAX_QUEUE: usize = 64;
/// Most actions (and outcomes) remembered for `dashboard.actions`.
pub const MAX_LOG: usize = 128;
/// Kinds the dashboard defines (ADR-108; `route` is ADR-116 R3, `agent.install` ADR-121 O3).
pub const KINDS: [&str; 7] = ["install", "update", "remove", "pair", "route", "agent.install", "agent.uninstall"];

/// An action as the dashboard sends it.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct Action {
    pub id: String,
    pub kind: String,
    #[serde(default)]
    pub payload: Value,
    #[serde(default)]
    pub created_at: Option<String>,
}

/// Terminal outcome of a handler.
#[derive(Debug, Clone, PartialEq)]
pub struct ActionOutcome {
    /// `succeeded` or `failed`.
    pub status: &'static str,
    pub result: Value,
}

impl ActionOutcome {
    pub fn failed(error: impl Into<String>, kind: &str) -> Self {
        Self { status: "failed", result: json!({ "error": error.into(), "kind": kind }) }
    }
}

/// Runs one kind of action. Called once per action id.
#[async_trait]
pub trait ActionHandler: Send + Sync {
    /// The action kind this handler answers.
    fn kind(&self) -> &str;
    async fn handle(&self, action: &Action) -> ActionOutcome;
}

/// Placeholder for kinds whose phase has not landed: always `failed`.
pub struct NotImplemented(pub &'static str);

#[async_trait]
impl ActionHandler for NotImplemented {
    fn kind(&self) -> &str {
        self.0
    }
    async fn handle(&self, action: &Action) -> ActionOutcome {
        ActionOutcome::failed("not implemented in this build (ADR-108 P3/P4)", &action.kind)
    }
}

/// The handlers of this build.
pub fn default_handlers() -> HashMap<String, Arc<dyn ActionHandler>> {
    let mut m: HashMap<String, Arc<dyn ActionHandler>> = KINDS
        .iter()
        .map(|k| (k.to_string(), Arc::new(NotImplemented(k)) as Arc<dyn ActionHandler>))
        .collect();
    if let Some(env) = crate::project_install_handlers::InstallEnv::from_process() {
        for h in crate::project_install_handlers::handlers(env) {
            m.insert(h.kind().to_owned(), h);
        }
    }
    m.insert(crate::mesh_pair::KIND.to_owned(), Arc::new(crate::mesh_pair::PairHandler::global()));
    m.insert(crate::router_action::KIND.to_owned(), Arc::new(crate::router_action::RouteHandler::from_process()));
    m.insert(crate::agent_install::KIND.to_owned(), Arc::new(crate::agent_install::AgentInstallHandler::from_process()));
    m.insert(crate::agent_uninstall::KIND.to_owned(), Arc::new(crate::agent_uninstall::AgentUninstallHandler::from_process()));
    m
}

/// An action and what became of it (no payload is kept: it may carry secrets).
#[derive(Debug, Clone, Serialize)]
pub struct ActionRecord {
    pub id: String,
    pub kind: String,
    pub created_at: Option<String>,
    pub received_at: String,
    /// `queued`, `running`, `succeeded` or `failed`.
    pub status: String,
    pub result: Option<Value>,
    /// The dashboard has the final status.
    pub acked: bool,
}

/// Queue and log. Held behind a mutex by the reporter.
#[derive(Default)]
pub struct ActionBook {
    queue: VecDeque<Action>,
    log: VecDeque<ActionRecord>,
}

fn id_ok(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

impl ActionBook {
    /// Actions in a heartbeat answer body; absent or malformed `actions` is none.
    pub fn parse(body: &str) -> Vec<Action> {
        let Ok(v) = serde_json::from_str::<Value>(body) else { return Vec::new() };
        let Some(Value::Array(items)) = v.get("actions") else { return Vec::new() };
        items
            .iter()
            .filter_map(|a| serde_json::from_value::<Action>(a.clone()).ok())
            .filter(|a| id_ok(&a.id) && !a.kind.is_empty() && a.kind.len() <= 32)
            .collect()
    }

    /// Queue the ones not seen before (the dashboard repeats an action until it
    /// has a final result). Returns how many were new.
    pub fn enqueue(&mut self, actions: Vec<Action>) -> usize {
        let mut added = 0;
        for a in actions {
            if self.log.iter().any(|r| r.id == a.id) {
                continue;
            }
            if self.queue.len() >= MAX_QUEUE {
                break;
            }
            self.log_push(ActionRecord {
                id: a.id.clone(),
                kind: a.kind.clone(),
                created_at: a.created_at.clone(),
                received_at: chrono::Utc::now().to_rfc3339(),
                status: "queued".into(),
                result: None,
                acked: false,
            });
            self.queue.push_back(a);
            added += 1;
        }
        added
    }

    fn log_push(&mut self, r: ActionRecord) {
        if self.log.len() >= MAX_LOG {
            // Drop the oldest finished record; never one still waiting.
            if let Some(i) = self.log.iter().position(|r| r.acked) {
                self.log.remove(i);
            } else {
                self.log.pop_front();
            }
        }
        self.log.push_back(r);
    }

    pub fn pop(&mut self) -> Option<Action> {
        self.queue.pop_front()
    }

    pub fn set(&mut self, id: &str, status: &str, result: Option<Value>, acked: bool) {
        if let Some(r) = self.log.iter_mut().find(|r| r.id == id) {
            r.status = status.into();
            if result.is_some() {
                r.result = result;
            }
            r.acked = acked;
        }
    }

    /// Finished actions whose final status the dashboard has not yet accepted.
    pub fn unacked(&self) -> Vec<ActionRecord> {
        self.log.iter().filter(|r| !r.acked && matches!(r.status.as_str(), "succeeded" | "failed")).cloned().collect()
    }

    pub fn queued(&self) -> usize {
        self.queue.len()
    }

    pub fn recorded(&self) -> usize {
        self.log.len()
    }

    /// Newest first.
    pub fn recent(&self, limit: usize) -> Vec<ActionRecord> {
        self.log.iter().rev().take(limit).cloned().collect()
    }
}

impl crate::dashboard_report::Dashboard {
    /// Queue the actions in a heartbeat answer (absent `actions`: nothing).
    pub(crate) fn ingest_actions(&self, body: &str) {
        let actions = ActionBook::parse(body);
        if actions.is_empty() {
            return;
        }
        let added = self.book.lock().unwrap_or_else(|e| e.into_inner()).enqueue(actions);
        if added > 0 {
            tracing::info!(count = added, "dashboard queued actions for this node");
        }
    }

    /// Replace the handler for a kind (P3/P4 plug in here).
    pub fn set_action_handler(&self, h: Arc<dyn ActionHandler>) {
        self.handlers.lock().unwrap_or_else(|e| e.into_inner()).insert(h.kind().to_owned(), h);
    }

    /// `dashboard.actions`: newest first, plus the counts. No payloads, no token.
    pub fn actions_json(&self, limit: usize) -> Value {
        let b = self.book.lock().unwrap_or_else(|e| e.into_inner());
        json!({ "queued": b.queued(), "recorded": b.recorded(), "actions": b.recent(limit) })
    }

    async fn post_result(&self, id: &str, status: &str, result: Option<&Value>) -> bool {
        let Ok(token) = self.current_token() else { return false };
        let mut body = json!({ "status": status });
        if let Some(r) = result {
            body["result"] = r.clone();
        }
        match self.post(&format!("/api/nodes/actions/{id}/result"), &token, &body).await {
            Ok((s, _)) if (200..300).contains(&s) => true,
            Ok((s, _)) => {
                tracing::warn!(action = %id, status = s, "dashboard did not accept an action result");
                false
            }
            Err(e) => {
                tracing::warn!(action = %id, error = %e, "could not post an action result");
                false
            }
        }
    }

    /// Acknowledge every queued action (`running`, then the handler's outcome),
    /// first re-sending final results the dashboard has not accepted yet. A
    /// handler runs once per id, however often the dashboard redelivers it.
    pub(crate) async fn process_actions(&self) {
        let pending = self.book.lock().unwrap_or_else(|e| e.into_inner()).unacked();
        for r in pending {
            let ok = self.post_result(&r.id, &r.status, r.result.as_ref()).await;
            self.book.lock().unwrap_or_else(|e| e.into_inner()).set(&r.id, &r.status, None, ok);
        }
        loop {
            let Some(action) = self.book.lock().unwrap_or_else(|e| e.into_inner()).pop() else { break };
            self.book.lock().unwrap_or_else(|e| e.into_inner()).set(&action.id, "running", None, false);
            self.post_result(&action.id, "running", None).await;
            let handler = self.handlers.lock().unwrap_or_else(|e| e.into_inner()).get(&action.kind).cloned();
            let outcome = match handler {
                Some(h) => h.handle(&action).await,
                None => ActionOutcome::failed(format!("unknown action kind {:?}", action.kind), &action.kind),
            };
            self.book.lock().unwrap_or_else(|e| e.into_inner()).set(
                &action.id,
                outcome.status,
                Some(outcome.result.clone()),
                false,
            );
            let ok = self.post_result(&action.id, outcome.status, Some(&outcome.result)).await;
            self.book.lock().unwrap_or_else(|e| e.into_inner()).set(&action.id, outcome.status, None, ok);
        }
    }
}

#[cfg(test)]
#[path = "dashboard_actions_tests.rs"]
mod tests;
