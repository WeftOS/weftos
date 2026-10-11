//! Shared test harness: a scripted LLM provider, a fake harness API, and an
//! agent builder. Nothing here touches a daemon, a microphone or the network
//! beyond the loopback fake.

#![allow(dead_code)]

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use clawft_core::pipeline::transport::LlmProvider;
use serde_json::{Value, json};
use tokio::sync::broadcast;
use weftos_cos::agent::CosAgent;
use weftos_cos::events::Event;
use weftos_cos::routing::{ModelRoutes, Route};
use weftos_cos::tools::board::BoardPropose;
use weftos_cos::tools::harness::{HarnessClient, NotesRecord, ProjectsActivity, SecretToken};
use weftos_cos::tools::kickoff::WorkKickoff;
use weftos_cos::tools::nodes::{NodesHealth, StaticHealth};
use weftos_cos::tools::{ToolLog, ToolRegistry};

pub const TEST_TOKEN: &str = "wfb_0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// A provider that replays scripted responses in order and records every
/// request it saw. The last response repeats when the script runs out.
pub struct ScriptedLlm {
    script: Mutex<VecDeque<Value>>,
    pub seen: Mutex<Vec<Vec<Value>>>,
    pub models: Mutex<Vec<String>>,
}

impl ScriptedLlm {
    pub fn new(responses: Vec<Value>) -> Arc<Self> {
        Arc::new(Self { script: Mutex::new(responses.into()), seen: Mutex::new(vec![]), models: Mutex::new(vec![]) })
    }

    pub fn text(content: &str) -> Value {
        json!({"id": "r", "choices": [{"message": {"role": "assistant", "content": content}, "finish_reason": "stop"}]})
    }

    pub fn tool_call(name: &str, args: Value) -> Value {
        json!({"id": "r", "choices": [{"message": {"role": "assistant", "content": null,
            "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": name, "arguments": args.to_string()}}]},
            "finish_reason": "tool_calls"}]})
    }

    fn next(&self, model: &str, messages: &[Value]) -> Value {
        self.models.lock().unwrap().push(model.to_string());
        self.seen.lock().unwrap().push(messages.to_vec());
        let mut s = self.script.lock().unwrap();
        if s.len() > 1 { s.pop_front().unwrap() } else { s.front().cloned().unwrap_or_else(|| Self::text("(empty script)")) }
    }
}

#[async_trait]
impl LlmProvider for ScriptedLlm {
    async fn complete(&self, model: &str, messages: &[Value], _: &[Value], _: Option<i32>, _: Option<f64>, _: Option<&Value>) -> Result<Value, String> {
        Ok(self.next(model, messages))
    }
}

/// A fake dashboard harness: tickets, goals, an events route and a comment
/// sink. Records every POST body.
#[derive(Clone, Default)]
pub struct FakeHarness {
    pub posts: Arc<Mutex<Vec<(String, Value)>>>,
    pub with_events_route: bool,
}

impl FakeHarness {
    pub async fn start(self) -> (SocketAddr, Arc<Mutex<Vec<(String, Value)>>>) {
        let posts = self.posts.clone();
        let mut app = Router::new()
            .route("/api/harness/tickets", get(tickets).post(ticket_post))
            .route("/api/harness/goals", get(goals));
        if self.with_events_route {
            // Dashboard PR #19: POST only, project id in the path.
            app = app.route("/api/harness/projects/{id}/events", axum::routing::post(events_post));
        }
        let app = app.with_state(self.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (addr, posts)
    }
}

fn authed(headers: &axum::http::HeaderMap) -> bool {
    headers.get("authorization").and_then(|v| v.to_str().ok()) == Some(&format!("Bearer {TEST_TOKEN}"))
}

async fn tickets(headers: axum::http::HeaderMap) -> (axum::http::StatusCode, Json<Value>) {
    if !authed(&headers) {
        return (axum::http::StatusCode::UNAUTHORIZED, Json(json!({"error": "Invalid harness credential"})));
    }
    (axum::http::StatusCode::OK, Json(json!({"items": [
        {"id": "11111111-1111-4111-8111-111111111111", "status": "in_progress", "priority": "high",
         "title": "Console C2 screen", "source_url": "https://example.invalid/WEFT-901"},
        {"id": "22222222-2222-4222-8222-222222222222", "status": "todo", "priority": "medium",
         "title": "Pad button on GPIO", "source_url": "https://example.invalid/WEFT-902"}
    ]})))
}

async fn goals() -> Json<Value> {
    Json(json!({"items": [{"id": "g1", "status": "active", "title": "Console on the pad"}]}))
}

pub const TEST_PROJECT: &str = "33333333-3333-4333-8333-333333333333";

async fn ticket_post(State(h): State<FakeHarness>, Json(body): Json<Value>) -> Json<Value> {
    h.posts.lock().unwrap().push(("/api/harness/tickets".into(), body));
    Json(json!({"id": "11111111-1111-4111-8111-111111111111", "status": "in_progress"}))
}

async fn events_post(State(h): State<FakeHarness>, axum::extract::Path(id): axum::extract::Path<String>, headers: axum::http::HeaderMap, Json(body): Json<Value>) -> (axum::http::StatusCode, Json<Value>) {
    if !authed(&headers) {
        return (axum::http::StatusCode::UNAUTHORIZED, Json(json!({"error": "Invalid harness credential"})));
    }
    if id != TEST_PROJECT {
        return (axum::http::StatusCode::NOT_FOUND, Json(json!({"error": "not_found"})));
    }
    h.posts.lock().unwrap().push((format!("/api/harness/projects/{id}/events"), body));
    (axum::http::StatusCode::CREATED, Json(json!({"id": "e1", "created": true})))
}

/// Build an agent over a scripted provider (same provider for both tiers
/// unless `plan` is given), a fake harness when `harness` is set, and a
/// tempdir for staging and the tool log.
pub struct TestAgent {
    pub agent: CosAgent,
    pub dir: tempfile::TempDir,
    pub llm: Arc<ScriptedLlm>,
}

pub async fn build_agent(llm: Arc<ScriptedLlm>, plan: Option<Arc<ScriptedLlm>>, harness: Option<SocketAddr>) -> TestAgent {
    let dir = tempfile::tempdir().unwrap();
    let quick = Route::new("local/fast-model", llm.clone());
    let plan_route = Route::new("anthropic/big-model", plan.map(|p| p as Arc<dyn LlmProvider>).unwrap_or_else(|| llm.clone()));
    let routes = ModelRoutes::new(quick, plan_route, 0.3);
    let mut tools = ToolRegistry::new()
        .with(Arc::new(BoardPropose { staging_dir: dir.path().join("staging") }))
        .with(Arc::new(WorkKickoff { staging_dir: dir.path().join("staging"), runtime_command: None }))
        .with(Arc::new(NodesHealth { source: Arc::new(StaticHealth(json!({"reachable": true, "summary": {"healthy_nodes": 2, "total_nodes": 3}, "nodes": [1, 2, 3]}))) }));
    if let Some(addr) = harness {
        let client = Arc::new(HarnessClient::new(&format!("http://{addr}"), "/api/harness/projects/{project_id}/events", Some(TEST_PROJECT.into()), SecretToken::for_tests(TEST_TOKEN)));
        tools = tools.with(Arc::new(ProjectsActivity { client: client.clone() })).with(Arc::new(NotesRecord { client }));
    }
    let agent = CosAgent::new(routes, tools, ToolLog::new(Some(dir.path().join("tool-log.jsonl"))));
    TestAgent { agent, dir, llm }
}

/// Drain events until a final message arrives (or the timeout).
pub async fn collect_until_final(rx: &mut broadcast::Receiver<Event>, timeout: Duration) -> Vec<Event> {
    let mut out = Vec::new();
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Ok(ev)) => {
                let done = matches!(ev, Event::Message { is_final: true, .. });
                out.push(ev);
                if done {
                    // Allow the trailing state event through.
                    if let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
                        out.push(ev);
                    }
                    return out;
                }
            }
            Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
            _ => return out,
        }
    }
}

pub fn kinds(events: &[Event]) -> Vec<&'static str> {
    events
        .iter()
        .map(|e| match e {
            Event::State { .. } => "state",
            Event::Token { .. } => "token",
            Event::Message { .. } => "message",
            Event::Tool { .. } => "tool",
            Event::Page { .. } => "page",
            Event::Status { .. } => "status",
            Event::Layout { .. } => "layout",
            Event::Checklist { .. } => "checklist",
            Event::Steps { .. } => "steps",
            Event::Ask { .. } => "ask",
        })
        .collect()
}
