//! The agent streams: token events are the provider's own chunks, tool calls
//! come from the stream's assembled response, and a provider error mid-stream
//! ends the turn visibly instead of with a silently short answer.

mod common;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use clawft_core::pipeline::transport::LlmProvider;
use common::{ScriptedLlm, collect_until_final};
use serde_json::{Value, json};
use tokio::sync::mpsc::Sender;
use weftos_cos::agent::CosAgent;
use weftos_cos::routing::{ModelRoutes, Route};
use weftos_cos::tools::{ToolLog, ToolRegistry};
use weftos_cos::{Conversation, Event, TurnId, UserUtterance};

/// Streams fixed chunks, then either returns `Ok(response)` or fails.
struct Streamer {
    chunks: Vec<&'static str>,
    outcome: Result<Value, String>,
}

#[async_trait]
impl LlmProvider for Streamer {
    async fn complete(&self, _: &str, _: &[Value], _: &[Value], _: Option<i32>, _: Option<f64>, _: Option<&Value>) -> Result<Value, String> {
        Err("complete() must not be used by the agent".into())
    }

    async fn complete_stream(&self, _: &str, _: &[Value], _: &[Value], _: Option<i32>, _: Option<f64>, _: Option<&Value>, tx: Sender<String>) -> Result<Value, String> {
        for c in &self.chunks {
            let _ = tx.send((*c).to_string()).await;
        }
        self.outcome.clone()
    }
}

fn agent(p: Arc<Streamer>) -> (CosAgent, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let routes = ModelRoutes::new(Route::new("local/fast-model", p.clone()), Route::new("anthropic/big-model", p), 0.3);
    (CosAgent::new(routes, ToolRegistry::new(), ToolLog::new(Some(dir.path().join("tool-log.jsonl")))), dir)
}

fn tokens(events: &[Event]) -> Vec<String> {
    events.iter().filter_map(|e| if let Event::Token { text, .. } = e { Some(text.clone()) } else { None }).collect()
}

#[tokio::test]
async fn tokens_are_the_providers_chunks() {
    let (a, _dir) = agent(Arc::new(Streamer { chunks: vec!["All ", "quiet ", "today."], outcome: Ok(ScriptedLlm::text("All quiet today.")) }));
    let mut rx = a.events();
    a.run_turn(TurnId::new(), UserUtterance::typed("where are we")).await;
    let events = collect_until_final(&mut rx, Duration::from_secs(2)).await;
    assert_eq!(tokens(&events), vec!["All ", "quiet ", "today."]);
    assert!(events.iter().any(|e| matches!(e, Event::Message { text, is_final: true, .. } if text == "All quiet today.")));
}

#[tokio::test]
async fn tool_calls_come_from_the_streamed_response() {
    let call = json!({"id": "r", "choices": [{"message": {"role": "assistant", "content": null,
        "tool_calls": [{"id": "c1", "type": "function", "function": {"name": "no_such_tool", "arguments": "{}"}}]},
        "finish_reason": "tool_calls"}]});
    let (a, _dir) = agent(Arc::new(Streamer { chunks: vec![], outcome: Ok(call) }));
    let mut rx = a.events();
    a.run_turn(TurnId::new(), UserUtterance::typed("do the thing")).await;
    let events = collect_until_final(&mut rx, Duration::from_secs(2)).await;
    assert!(events.iter().any(|e| matches!(e, Event::Tool { name, status, .. } if name == "no_such_tool" && status == "refused")), "{events:?}");
}

#[tokio::test]
async fn provider_error_mid_stream_ends_the_turn_with_a_problem() {
    let (a, _dir) = agent(Arc::new(Streamer { chunks: vec!["partial "], outcome: Err("connection reset".into()) }));
    let mut rx = a.events();
    a.run_turn(TurnId::new(), UserUtterance::typed("where are we")).await;
    let events = collect_until_final(&mut rx, Duration::from_secs(2)).await;
    assert_eq!(tokens(&events), vec!["partial "]);
    let msg = events.iter().find_map(|e| if let Event::Message { text, is_final: true, .. } = e { Some(text.clone()) } else { None }).expect("final message");
    assert!(msg.contains("I hit a problem") && msg.contains("connection reset"), "{msg}");
}
