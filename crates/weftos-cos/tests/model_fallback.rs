//! The status line names the model that answered: a failing primary model
//! falls back once, and `status` says so. With no fallback the turn ends
//! with the problem, as before.

mod common;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use clawft_core::pipeline::transport::LlmProvider;
use common::{ScriptedLlm, collect_until_final};
use serde_json::Value;
use weftos_cos::agent::CosAgent;
use weftos_cos::routing::{ModelRoutes, Route};
use weftos_cos::tools::{ToolLog, ToolRegistry};
use weftos_cos::{Conversation, Event, TurnId, UserUtterance};

struct Down;

#[async_trait]
impl LlmProvider for Down {
    async fn complete(&self, _: &str, _: &[Value], _: &[Value], _: Option<i32>, _: Option<f64>, _: Option<&Value>) -> Result<Value, String> {
        Err("connection refused".into())
    }

    async fn complete_stream(
        &self,
        _: &str,
        _: &[Value],
        _: &[Value],
        _: Option<i32>,
        _: Option<f64>,
        _: Option<&Value>,
        _: tokio::sync::mpsc::Sender<String>,
    ) -> Result<Value, String> {
        Err("connection refused".into())
    }
}

fn statuses(events: &[Event]) -> Vec<(String, String)> {
    events.iter().filter_map(|e| match e { Event::Status { task, model, .. } => Some((task.clone(), model.clone())), _ => None }).collect()
}

#[tokio::test]
async fn a_dead_local_model_falls_back_and_the_status_names_the_cloud_model() {
    let cloud = ScriptedLlm::new(vec![ScriptedLlm::text("Two of three nodes are up.")]);
    let quick = Route::new("local/mlx-qwen", Arc::new(Down));
    let routes = ModelRoutes::new(quick.clone(), quick, 0.3).with_fallback(vec![Route::new("anthropic/claude-haiku", cloud.clone())]);
    let agent = CosAgent::new(routes, ToolRegistry::new(), ToolLog::new(None));
    let mut rx = agent.events();
    agent.run_turn(TurnId::new(), UserUtterance::typed("where are we")).await;
    let events = collect_until_final(&mut rx, Duration::from_secs(2)).await;
    let s = statuses(&events);
    assert_eq!(s[0].1, "local/mlx-qwen", "the first status is the primary: {s:?}");
    assert_eq!(s.last().unwrap(), &("typed turn (fallback)".to_string(), "anthropic/claude-haiku".to_string()));
    assert!(events.iter().any(|e| matches!(e, Event::Message { text, .. } if text.contains("Two of three"))));
    assert_eq!(cloud.models.lock().unwrap().as_slice(), ["claude-haiku"]);
}

#[tokio::test]
async fn without_a_fallback_the_turn_reports_the_problem() {
    let quick = Route::new("local/mlx-qwen", Arc::new(Down));
    let agent = CosAgent::new(ModelRoutes::new(quick.clone(), quick, 0.3), ToolRegistry::new(), ToolLog::new(None));
    let mut rx = agent.events();
    agent.run_turn(TurnId::new(), UserUtterance::typed("where are we")).await;
    let events = collect_until_final(&mut rx, Duration::from_secs(2)).await;
    assert!(events.iter().any(|e| matches!(e, Event::Message { text, .. } if text.contains("I hit a problem") && text.contains("connection refused"))));
    assert_eq!(statuses(&events).len(), 1);
}

#[tokio::test]
async fn the_chain_walks_past_every_dead_model_and_names_the_one_that_answers() {
    let ok = ScriptedLlm::new(vec![ScriptedLlm::text("Up.")]);
    let quick = Route::new("local/mlx-qwen", Arc::new(Down));
    let routes = ModelRoutes::new(quick.clone(), quick, 0.3)
        .with_fallback(vec![Route::new("anthropic/no-credit", Arc::new(Down)), Route::new("openrouter/anthropic/claude-haiku", ok.clone())]);
    let agent = CosAgent::new(routes, ToolRegistry::new(), ToolLog::new(None));
    let mut rx = agent.events();
    agent.run_turn(TurnId::new(), UserUtterance::voice("status")).await;
    let events = collect_until_final(&mut rx, Duration::from_secs(2)).await;
    let models: Vec<String> = statuses(&events).into_iter().map(|s| s.1).collect();
    assert_eq!(models, ["local/mlx-qwen", "anthropic/no-credit", "openrouter/anthropic/claude-haiku"]);
    assert!(events.iter().any(|e| matches!(e, Event::Message { text, .. } if text == "Up.")));
    assert_eq!(ok.models.lock().unwrap().as_slice(), ["anthropic/claude-haiku"], "the adapter gets the bare model");
}

#[tokio::test]
async fn a_failing_plan_model_falls_back_to_the_quick_model_first() {
    let local = ScriptedLlm::new(vec![ScriptedLlm::text("Plan, locally.")]);
    let routes = ModelRoutes::new(Route::new("local/fast", local.clone()), Route::new("anthropic/big", Arc::new(Down)), 0.3);
    let agent = CosAgent::new(routes, ToolRegistry::new(), ToolLog::new(None));
    let mut rx = agent.events();
    agent.run_turn(TurnId::new(), UserUtterance::typed("prioritise the console work for next week")).await;
    let events = collect_until_final(&mut rx, Duration::from_secs(2)).await;
    let s = statuses(&events);
    assert_eq!(s[0].1, "anthropic/big");
    assert_eq!(s.last().unwrap().1, "local/fast");
}
