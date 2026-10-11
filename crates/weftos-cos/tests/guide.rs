//! Guide views (ADR-118 §1a, contract §3): the agent drives layout,
//! checklist, steps and survey events; touch answers arrive over
//! `POST /v1/cos/answer` as `touch` turns; a spoken number resolves a
//! pending question; toggles re-emit the checklist without a model turn.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{ScriptedLlm, build_agent, collect_until_final};
use serde_json::json;
use weftos_cos::conversation::Conversation;
use weftos_cos::events::Layout;
use weftos_cos::{AnswerKind, Event, GuideAnswer, TurnId, UserUtterance};

fn last_user(llm: &ScriptedLlm) -> serde_json::Value {
    let seen = llm.seen.lock().unwrap();
    seen.last().unwrap().iter().rev().find(|m| m["role"] == "user").cloned().unwrap()
}

#[tokio::test]
async fn survey_over_http_then_touch_answer_becomes_a_touch_turn() {
    let llm = ScriptedLlm::new(vec![
        ScriptedLlm::tool_call("ui_ask", json!({"question": "What matters most today?", "explain": "Pick one.", "choices": ["Ship C3", "Pad button", "Docs"], "index": 1, "total": 2})),
        // After an ask the loop makes one more model round in the same turn,
        // which a real model uses to speak the question; here it is a stub.
        ScriptedLlm::text("Pick one."),
        // The touch answer turn: the model shows the plan as a checklist.
        ScriptedLlm::tool_call("ui_checklist", json!({"title": "Today", "items": ["Merge C3", "Wire the button"]})),
        ScriptedLlm::text("Here is your list."),
    ]);
    let t = build_agent(llm.clone(), None, None).await;
    let conv: Arc<dyn Conversation> = Arc::new(t.agent.clone());
    let mut rx = conv.events();
    let (addr, server) = weftos_cos::server::bind(conv.clone(), "127.0.0.1:0".parse().unwrap()).await.unwrap();
    tokio::spawn(server);

    t.agent.run_turn(TurnId::new(), UserUtterance::voice("help me plan today")).await;
    let first = collect_until_final(&mut rx, Duration::from_secs(2)).await;
    assert!(first.iter().any(|e| matches!(e, Event::Layout { layout: Layout::Guide })), "{first:?}");
    let ask = first.iter().find_map(|e| match e { Event::Ask { id, choices, index, total, .. } => Some((id.clone(), choices.clone(), *index, *total)), _ => None }).expect("ask event");
    assert_eq!((ask.2, ask.3), (1, 2));
    assert_eq!(ask.1.iter().map(|c| (c.n, c.caption.as_str())).collect::<Vec<_>>(), vec![(1, "Ship C3"), (2, "Pad button"), (3, "Docs")]);

    let client = reqwest::Client::new();
    let res = client.post(format!("http://{addr}/v1/cos/answer")).json(&json!({"kind": "answer", "id": ask.0, "value": 2})).send().await.unwrap();
    assert_eq!(res.status(), 202);
    let second = collect_until_final(&mut rx, Duration::from_secs(2)).await;
    let status = second.iter().find_map(|e| match e { Event::Status { task, .. } => Some(task.clone()), _ => None }).unwrap();
    assert!(status.contains("touch"), "{status}");
    // The model saw the answer as a user turn carrying number and caption.
    let text: String = {
        let seen = llm.seen.lock().unwrap();
        seen.iter()
            .rev()
            .find_map(|req| req.iter().rev().find(|m| m["role"] == "user" && m["content"].as_str().is_some_and(|c| c.starts_with("Answer to"))))
            .and_then(|m| m["content"].as_str().map(str::to_string))
            .expect("answer user message")
    };
    assert!(text.contains("Q1/2") && text.contains("2 — Pad button"), "{text}");

    // The question is consumed: answering it again is refused, as is a bad number.
    let res = client.post(format!("http://{addr}/v1/cos/answer")).json(&json!({"kind": "answer", "id": ask.0, "value": 9})).send().await.unwrap();
    assert_eq!(res.status(), 400);

    // The checklist arrived in the answer turn.
    let cl = second.iter().find_map(|e| match e { Event::Checklist { id, items, title } => Some((id.clone(), items.clone(), title.clone())), _ => None }).expect("checklist event");
    assert_eq!(cl.2, "Today");
    assert_eq!(cl.1.len(), 2);
    assert!(cl.1.iter().all(|i| !i.done));

    // A toggle re-emits the checklist with the item done and wakes no model.
    let calls_before = llm.seen.lock().unwrap().len();
    let res = client.post(format!("http://{addr}/v1/cos/answer")).json(&json!({"kind": "toggle", "id": cl.0, "value": cl.1[0].id})).send().await.unwrap();
    assert_eq!(res.status(), 202);
    let ev = tokio::time::timeout(Duration::from_secs(1), rx.recv()).await.unwrap().unwrap();
    let Event::Checklist { items, .. } = ev else { panic!("expected checklist, got {ev:?}") };
    assert!(items[0].done && !items[1].done);
    assert_eq!(llm.seen.lock().unwrap().len(), calls_before);
}

#[tokio::test]
async fn spoken_number_resolves_a_pending_question() {
    let llm = ScriptedLlm::new(vec![
        ScriptedLlm::tool_call("ui_ask", json!({"question": "Which lane first?", "choices": ["developer", "tester"]})),
        ScriptedLlm::text("Developer first, then."),
    ]);
    let t = build_agent(llm.clone(), None, None).await;
    let mut rx = t.agent.events();
    t.agent.run_turn(TurnId::new(), UserUtterance::voice("which lane should go first")).await;
    let _ = collect_until_final(&mut rx, Duration::from_secs(2)).await;
    t.agent.run_turn(TurnId::new(), UserUtterance::voice("number one")).await;
    let events = collect_until_final(&mut rx, Duration::from_secs(2)).await;
    let text = last_user(&llm)["content"].as_str().unwrap().to_string();
    assert!(text.contains("1 — developer"), "{text}");
    let status = events.iter().find_map(|e| match e { Event::Status { task, .. } => Some(task.clone()), _ => None }).unwrap();
    assert!(status.contains("voice"), "the spoken source is kept: {status}");
    // Once answered, a plain "two" is an ordinary utterance again.
    t.agent.run_turn(TurnId::new(), UserUtterance::voice("two")).await;
    let _ = collect_until_final(&mut rx, Duration::from_secs(2)).await;
    assert_eq!(last_user(&llm)["content"], "two");
}

#[tokio::test]
async fn steps_and_layout_and_page() {
    let llm = ScriptedLlm::new(vec![
        ScriptedLlm::tool_call("ui_steps", json!({"title": "Release", "steps": ["Tag", "Build", "Publish"], "current": 2})),
        ScriptedLlm::text("You are on build."),
        ScriptedLlm::tool_call("ui_page", json!({})),
        ScriptedLlm::text("Page shown."),
    ]);
    let t = build_agent(llm, None, None).await;
    let conv: Arc<dyn Conversation> = Arc::new(t.agent.clone());
    let mut rx = conv.events();
    t.agent.run_turn(TurnId::new(), UserUtterance::typed("walk me through the release")).await;
    let events = collect_until_final(&mut rx, Duration::from_secs(2)).await;
    let (id, current) = events.iter().find_map(|e| match e { Event::Steps { id, current, steps, .. } if steps.len() == 3 => Some((id.clone(), *current)), _ => None }).unwrap();
    assert_eq!(current, 1, "current is 0-based on the wire; the tool took 1-based 2");

    conv.answer(GuideAnswer { kind: AnswerKind::Step, id: id.clone(), value: json!("next") }).await.unwrap();
    let ev = tokio::time::timeout(Duration::from_secs(1), rx.recv()).await.unwrap().unwrap();
    assert!(matches!(ev, Event::Steps { current: 2, .. }), "{ev:?}");
    conv.answer(GuideAnswer { kind: AnswerKind::Step, id, value: json!("next") }).await.unwrap();
    let ev = tokio::time::timeout(Duration::from_secs(1), rx.recv()).await.unwrap().unwrap();
    assert!(matches!(ev, Event::Steps { current: 2, .. }), "clamped at the last step: {ev:?}");

    t.agent.run_turn(TurnId::new(), UserUtterance::typed("show me the page")).await;
    let events = collect_until_final(&mut rx, Duration::from_secs(2)).await;
    assert!(events.iter().any(|e| matches!(e, Event::Layout { layout: Layout::Page })));
    assert!(events.iter().any(|e| matches!(e, Event::Page { markdown, .. } if markdown.contains("page layout"))));
    let v = serde_json::to_value(events.iter().find(|e| matches!(e, Event::Layout { .. })).unwrap()).unwrap();
    assert_eq!(v, json!({"kind": "layout", "layout": "page"}));
}
