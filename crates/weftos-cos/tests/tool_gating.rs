//! Tool gating: a kickoff asks first and runs only on a yes; anything else
//! cancels. Open tools run on the model's say-so and are recorded.

mod common;

use std::time::Duration;

use common::{ScriptedLlm, build_agent, collect_until_final, kinds};
use serde_json::json;
use weftos_cos::{Conversation, Event, TurnId, UserUtterance};

fn tool_events(events: &[Event]) -> Vec<(String, String)> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::Tool { name, status, .. } => Some((name.clone(), status.clone())),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn kickoff_asks_first_then_runs_on_yes() {
    let llm = ScriptedLlm::new(vec![
        ScriptedLlm::tool_call("work_kickoff", json!({"card": "WEFT-902", "lane": "developer"})),
        ScriptedLlm::text("should not be reached"),
    ]);
    let t = build_agent(llm.clone(), None, None).await;
    let mut rx = t.agent.events();

    t.agent.run_turn(TurnId::new(), UserUtterance::typed("start the developer lane on WEFT-902")).await;
    let first = collect_until_final(&mut rx, Duration::from_secs(2)).await;
    assert_eq!(tool_events(&first), vec![("work.kickoff".to_string(), "awaiting_confirmation".to_string())]);
    let question = first.iter().find_map(|e| match e { Event::Message { text, .. } => Some(text.clone()), _ => None }).unwrap();
    assert!(question.contains("start the developer lane of weftos-core on WEFT-902"), "{question}");
    assert!(question.to_lowercase().contains("yes"));
    assert!(!t.dir.path().join("staging").exists(), "nothing staged before the yes");
    assert_eq!(llm.seen.lock().unwrap().len(), 1, "the model is not consulted again until the owner answers");

    t.agent.run_turn(TurnId::new(), UserUtterance::voice("yes")).await;
    let second = collect_until_final(&mut rx, Duration::from_secs(2)).await;
    let statuses = tool_events(&second);
    assert_eq!(statuses.last().unwrap(), &("work.kickoff".to_string(), "ok".to_string()));
    let reply = second.iter().find_map(|e| match e { Event::Message { text, .. } => Some(text.clone()), _ => None }).unwrap();
    assert!(reply.contains("not built"), "the stub is reported, not hidden: {reply}");
    let staged: Vec<_> = std::fs::read_dir(t.dir.path().join("staging")).unwrap().collect();
    assert_eq!(staged.len(), 1);
    assert_eq!(llm.seen.lock().unwrap().len(), 1, "a confirmation turn needs no model call");

    let log = std::fs::read_to_string(t.dir.path().join("tool-log.jsonl")).unwrap();
    assert!(log.contains("awaiting_confirmation"));
    assert!(log.lines().any(|l| l.contains("\"status\":\"ok\"")));
}

#[tokio::test]
async fn anything_but_yes_cancels() {
    let llm = ScriptedLlm::new(vec![ScriptedLlm::tool_call("work_kickoff", json!({"card": "WEFT-902"}))]);
    let t = build_agent(llm, None, None).await;
    let mut rx = t.agent.events();
    t.agent.run_turn(TurnId::new(), UserUtterance::typed("kick off WEFT-902")).await;
    let _ = collect_until_final(&mut rx, Duration::from_secs(2)).await;
    t.agent.run_turn(TurnId::new(), UserUtterance::voice("no, wait until tomorrow")).await;
    let events = collect_until_final(&mut rx, Duration::from_secs(2)).await;
    assert_eq!(tool_events(&events), vec![("work.kickoff".to_string(), "cancelled".to_string())]);
    assert!(!t.dir.path().join("staging").exists());
}

#[tokio::test]
async fn open_tools_run_and_feed_the_model() {
    let llm = ScriptedLlm::new(vec![
        ScriptedLlm::tool_call("nodes_health", json!({})),
        ScriptedLlm::text("Two of three nodes are healthy."),
    ]);
    let t = build_agent(llm.clone(), None, None).await;
    let mut rx = t.agent.events();
    t.agent.run_turn(TurnId::new(), UserUtterance::typed("is the mesh healthy")).await;
    let events = collect_until_final(&mut rx, Duration::from_secs(2)).await;
    assert_eq!(tool_events(&events), vec![("nodes.health".to_string(), "running".to_string()), ("nodes.health".to_string(), "ok".to_string())]);
    assert!(kinds(&events).ends_with(&["message", "state"]));
    let second_call = &t.llm.seen.lock().unwrap()[1];
    let tool_msg = second_call.iter().find(|m| m["role"] == "tool").expect("tool result in history");
    assert!(tool_msg["content"].as_str().unwrap().contains("healthy_nodes"));
}
