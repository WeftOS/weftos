//! A local model that keeps calling tools must still end the turn with a real
//! answer: repeated identical calls and the round limit both switch the next
//! request to tool-free, and the status names the task.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use clawft_core::pipeline::transport::LlmProvider;
use common::collect_until_final;
use serde_json::{Value, json};
use weftos_cos::CosError;
use weftos_cos::agent::CosAgent;
use weftos_cos::routing::{ModelRoutes, Route};
use weftos_cos::tools::{Gate, Tool, ToolLog, ToolOutcome, ToolRegistry, ToolSpec};
use weftos_cos::{Conversation, Event, TurnId, UserUtterance};

/// Calls `probe` every time it is offered tools (with `vary` it changes the
/// argument each time); answers in text when the request carries none.
struct Looper {
    vary: bool,
    calls: Mutex<u32>,
    /// (tool count, tool_choice) of each request.
    requests: Mutex<Vec<(usize, Option<Value>)>>,
}

impl Looper {
    fn new(vary: bool) -> Arc<Self> {
        Arc::new(Self { vary, calls: Mutex::new(0), requests: Mutex::new(vec![]) })
    }

    fn respond(&self, specs: &[Value], choice: Option<&Value>) -> Value {
        self.requests.lock().unwrap().push((specs.len(), choice.cloned()));
        if specs.is_empty() {
            return json!({"choices": [{"message": {"role": "assistant", "content": "The pad is flashed and joined; the probe returned the same state each time."}}]});
        }
        let mut n = self.calls.lock().unwrap();
        *n += 1;
        let args = if self.vary { json!({"n": *n}) } else { json!({}) };
        json!({"choices": [{"message": {"role": "assistant", "content": null, "tool_calls": [
            {"id": format!("c{n}"), "type": "function", "function": {"name": "probe", "arguments": args.to_string()}}]}}]})
    }
}

#[async_trait]
impl LlmProvider for Looper {
    async fn complete(&self, _: &str, _: &[Value], specs: &[Value], _: Option<i32>, _: Option<f64>, choice: Option<&Value>) -> Result<Value, String> {
        Ok(self.respond(specs, choice))
    }

    async fn complete_stream(
        &self,
        _: &str,
        _: &[Value],
        specs: &[Value],
        _: Option<i32>,
        _: Option<f64>,
        choice: Option<&Value>,
        _: tokio::sync::mpsc::Sender<String>,
    ) -> Result<Value, String> {
        Ok(self.respond(specs, choice))
    }
}

struct Probe;

#[async_trait]
impl Tool for Probe {
    fn spec(&self) -> ToolSpec {
        ToolSpec { id: "probe", description: "read state", parameters: json!({"type": "object", "properties": {}}), gate: Gate::Open }
    }

    async fn call(&self, _: Value) -> Result<ToolOutcome, CosError> {
        Ok(ToolOutcome { summary: "probed".into(), result: json!({"state": "same"}) })
    }
}

fn agent(llm: Arc<Looper>, rounds: Option<usize>) -> CosAgent {
    let route = Route::new("local/glm", llm);
    let tools = ToolRegistry::new().with(Arc::new(Probe));
    let a = CosAgent::new(ModelRoutes::new(route.clone(), route, 0.3), tools, ToolLog::new(None));
    match rounds {
        Some(n) => a.with_max_tool_rounds(n),
        None => a,
    }
}

async fn run(a: &CosAgent, text: &str) -> Vec<Event> {
    let mut rx = a.events();
    a.run_turn(TurnId::new(), UserUtterance::typed(text)).await;
    collect_until_final(&mut rx, Duration::from_secs(2)).await
}

fn final_text(events: &[Event]) -> String {
    events.iter().rev().find_map(|e| match e { Event::Message { text, is_final: true, .. } => Some(text.clone()), _ => None }).expect("a final message")
}

#[tokio::test]
async fn an_identical_repeated_call_forces_a_real_answer_after_two_calls() {
    let llm = Looper::new(false);
    let events = run(&agent(llm.clone(), None), "where are we on the pad").await;
    let text = final_text(&events);
    assert!(text.starts_with("The pad is flashed"), "got {text}");
    assert!(!text.contains("ran out of tool rounds"));
    let reqs = llm.requests.lock().unwrap();
    assert_eq!(reqs.len(), 3, "call, repeat, forced answer: {reqs:?}");
    assert_eq!(reqs[2], (0, Some(json!("none"))), "the last request has no tools and tool_choice none");
    assert_eq!(reqs[0].1, None);
}

#[tokio::test]
async fn the_round_limit_forces_a_real_answer_even_when_calls_differ() {
    let llm = Looper::new(true);
    let events = run(&agent(llm.clone(), Some(3)), "what is going on").await;
    assert!(final_text(&events).starts_with("The pad is flashed"));
    let reqs = llm.requests.lock().unwrap();
    assert_eq!(reqs.len(), 4, "three tool rounds then the forced answer: {reqs:?}");
    assert_eq!(reqs[3].0, 0);
}

#[tokio::test]
async fn the_status_names_the_task_not_the_source() {
    let llm = Looper::new(false);
    let events = run(&agent(llm, None), "where are we on the pad?").await;
    let task = events.iter().find_map(|e| match e { Event::Status { task, .. } => Some(task.clone()), _ => None }).unwrap();
    assert_eq!(task, "Where are we on the pad");
}
