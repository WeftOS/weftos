//! Contract §3: an utterance over HTTP produces state, status, token and a
//! final message on the SSE stream, stamped with the turn id the POST
//! returned; the server refuses non-loopback binds.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{ScriptedLlm, build_agent};
use weftos_cos::conversation::Conversation;

#[tokio::test]
async fn utterance_produces_state_tokens_and_message() {
    let llm = ScriptedLlm::new(vec![ScriptedLlm::text("We are on C3; the pad button is next.")]);
    let t = build_agent(llm.clone(), None, None).await;
    let conv: Arc<dyn Conversation> = Arc::new(t.agent.clone());
    let (addr, server) = weftos_cos::server::bind(conv.clone(), "127.0.0.1:0".parse().unwrap()).await.unwrap();
    tokio::spawn(server);

    // Subscribe first so nothing is missed.
    let client = reqwest::Client::new();
    let mut sse = client.get(format!("http://{addr}/v1/cos/events")).send().await.unwrap();
    assert_eq!(sse.headers()["content-type"].to_str().unwrap(), "text/event-stream");

    let res = client
        .post(format!("http://{addr}/v1/cos/utterance"))
        .json(&serde_json::json!({"text": "where are we", "source": "typed"}))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 202);
    let turn_id = res.json::<serde_json::Value>().await.unwrap()["turn_id"].as_str().unwrap().to_string();

    let mut body = String::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline && !body.contains("\"final\":true") {
        match tokio::time::timeout(Duration::from_millis(500), sse.chunk()).await {
            Ok(Ok(Some(chunk))) => body.push_str(&String::from_utf8_lossy(&chunk)),
            Ok(Ok(None)) => break,
            _ => {}
        }
    }
    let events: Vec<serde_json::Value> = body
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter_map(|d| serde_json::from_str(d).ok())
        .collect();
    let kinds: Vec<&str> = events.iter().map(|e| e["kind"].as_str().unwrap()).collect();
    assert!(kinds.contains(&"state"), "kinds: {kinds:?}");
    assert!(kinds.contains(&"status"), "kinds: {kinds:?}");
    assert!(kinds.contains(&"token"), "kinds: {kinds:?}");
    let msg = events.iter().find(|e| e["kind"] == "message").expect("message event");
    assert_eq!(msg["turn_id"], turn_id);
    assert_eq!(msg["final"], true);
    assert_eq!(msg["text"], "We are on C3; the pad button is next.");
    let status = events.iter().find(|e| e["kind"] == "status").unwrap();
    assert_eq!(status["model"], "local/fast-model");
    assert!(status["lanes_running"].is_number());
}

#[tokio::test]
async fn rejects_bad_bodies_and_non_loopback_binds() {
    let llm = ScriptedLlm::new(vec![ScriptedLlm::text("ok")]);
    let t = build_agent(llm, None, None).await;
    let conv: Arc<dyn Conversation> = Arc::new(t.agent.clone());
    assert!(weftos_cos::server::bind(conv.clone(), "0.0.0.0:0".parse().unwrap()).await.is_err());

    let (addr, server) = weftos_cos::server::bind(conv, "127.0.0.1:0".parse().unwrap()).await.unwrap();
    tokio::spawn(server);
    let client = reqwest::Client::new();
    let res = client.post(format!("http://{addr}/v1/cos/utterance")).json(&serde_json::json!({"text": "   ", "source": "typed"})).send().await.unwrap();
    assert_eq!(res.status(), 400);
    let res = client.post(format!("http://{addr}/v1/cos/utterance")).json(&serde_json::json!({"text": "hi", "source": "carrier-pigeon"})).send().await.unwrap();
    assert_eq!(res.status(), 400);
}

#[tokio::test]
async fn planning_turns_route_to_the_plan_model() {
    let quick = ScriptedLlm::new(vec![ScriptedLlm::text("quick")]);
    let plan = ScriptedLlm::new(vec![ScriptedLlm::text("plan")]);
    let t = build_agent(quick.clone(), Some(plan.clone()), None).await;
    let mut rx = t.agent.events();
    t.agent.run_turn(weftos_cos::TurnId::new(), weftos_cos::UserUtterance::typed("prioritise the console work for next week")).await;
    let events = common::collect_until_final(&mut rx, Duration::from_secs(2)).await;
    let status = events.iter().find_map(|e| match e { weftos_cos::Event::Status { model, task, .. } => Some((model.clone(), task.clone())), _ => None }).unwrap();
    assert_eq!(status.0, "anthropic/big-model");
    assert!(status.1.starts_with("planning"));
    assert_eq!(plan.models.lock().unwrap().as_slice(), ["big-model"], "adapter gets the bare model");
    assert!(quick.models.lock().unwrap().is_empty());
}

#[tokio::test]
async fn status_route_reports_the_latest_state_task_and_model() {
    let t = common::build_agent(common::ScriptedLlm::new(vec![common::ScriptedLlm::text("ok")]), None, None).await;
    let conv: std::sync::Arc<dyn weftos_cos::conversation::Conversation> = std::sync::Arc::new(t.agent.clone());
    let (addr, server) = weftos_cos::server::bind(conv.clone(), "127.0.0.1:0".parse().unwrap()).await.unwrap();
    tokio::spawn(server);
    let url = format!("http://{addr}/v1/cos/status");
    let first = reqwest::get(&url).await.unwrap();
    assert_eq!(first.status(), 200);
    assert_eq!(first.json::<serde_json::Value>().await.unwrap()["state"], serde_json::Value::Null);

    conv.utter(weftos_cos::UserUtterance::typed("hello")).await.unwrap();
    for _ in 0..100 {
        let v = reqwest::get(&url).await.unwrap().json::<serde_json::Value>().await.unwrap();
        if v["state"] == "idle" {
            assert!(v["lanes_running"].is_number());
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("state never became idle");
}
