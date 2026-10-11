//! No secrets in events: the harness credential is used on the wire and
//! never appears in any event, tool summary, log line or model message, even
//! when the model tries to echo it or a tool result carries one.

mod common;

use std::time::Duration;

use common::{FakeHarness, ScriptedLlm, TEST_TOKEN, build_agent, collect_until_final};
use serde_json::json;
use weftos_cos::tools::harness::SecretToken;
use weftos_cos::{Conversation, Event, TurnId, UserUtterance};

#[tokio::test]
async fn harness_tools_use_the_token_without_leaking_it() {
    let (addr, posts) = FakeHarness { with_events_route: true, ..Default::default() }.start().await;
    let llm = ScriptedLlm::new(vec![
        ScriptedLlm::tool_call("projects_activity", json!({"limit": 10})),
        ScriptedLlm::tool_call("notes_record", json!({"text": "Moving the agent to PG after C4"})),
        // A model that misbehaves and tries to repeat the credential.
        ScriptedLlm::text(&format!("Two cards open. By the way your token is {TEST_TOKEN}.")),
    ]);
    let t = build_agent(llm.clone(), None, Some(addr)).await;
    let mut rx = t.agent.events();
    t.agent.run_turn(TurnId::new(), UserUtterance::typed("where are we, and note we move the agent to PG after C4")).await;
    let events = collect_until_final(&mut rx, Duration::from_secs(5)).await;

    let dump = serde_json::to_string(&events).unwrap();
    assert!(!dump.contains(TEST_TOKEN), "token leaked into events: {dump}");
    assert!(!dump.contains("Bearer "));
    let msg = events.iter().find_map(|e| match e { Event::Message { text, .. } => Some(text.clone()), _ => None }).unwrap();
    assert!(msg.contains("[redacted]"), "{msg}");

    let statuses: Vec<_> = events.iter().filter_map(|e| match e { Event::Tool { name, status, .. } => Some((name.as_str(), status.as_str())), _ => None }).collect();
    assert!(statuses.contains(&("projects.activity", "ok")), "{statuses:?}");
    assert!(statuses.contains(&("notes.record", "ok")), "{statuses:?}");

    // The note went to the project-history route (PR #19 shape) as kind: note.
    let expected = format!("/api/harness/projects/{}/events", common::TEST_PROJECT);
    let posts = posts.lock().unwrap();
    let (path, body) = posts.iter().find(|(p, _)| *p == expected).expect("note posted");
    assert_eq!(path, &expected);
    assert_eq!(body["kind"], "note");
    assert_eq!(body["title"], "Moving the agent to PG after C4");
    assert_eq!(body["summary"], "");
    assert!(body.get("details_md").is_none());
    assert!(body["occurred_at"].as_str().is_some());

    // Neither the log nor the model-facing history carries the token.
    let log = std::fs::read_to_string(t.dir.path().join("tool-log.jsonl")).unwrap();
    assert!(!log.contains(TEST_TOKEN));
    for req in t.llm.seen.lock().unwrap().iter() {
        assert!(!serde_json::to_string(req).unwrap().contains(TEST_TOKEN));
    }
    assert_eq!(format!("{:?}", SecretToken::for_tests(TEST_TOKEN)), "SecretToken([redacted])");
}

#[tokio::test]
async fn tool_failure_reasons_are_redacted() {
    // Without the project-events route, notes.record fails; the HTTP error
    // text must not carry anything secret-shaped either.
    let (addr, _) = FakeHarness::default().start().await;
    let llm = ScriptedLlm::new(vec![
        ScriptedLlm::tool_call("notes_record", json!({"text": "a note with no route"})),
        ScriptedLlm::text("The note could not be recorded; the history route is not available."),
    ]);
    let t = build_agent(llm, None, Some(addr)).await;
    let mut rx = t.agent.events();
    t.agent.run_turn(TurnId::new(), UserUtterance::typed("note: a note with no route")).await;
    let events = collect_until_final(&mut rx, Duration::from_secs(5)).await;
    let err = events.iter().find_map(|e| match e { Event::Tool { name, status, summary } if name == "notes.record" && status == "error" => Some(summary.clone()), _ => None }).expect("error event");
    assert!(err.contains("HTTP 404"), "{err}");
    assert!(!serde_json::to_string(&events).unwrap().contains(TEST_TOKEN));
}

#[test]
fn token_file_must_be_private() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("board-token");
    std::fs::write(&path, format!("{TEST_TOKEN}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    let err = SecretToken::from_file(&path).unwrap_err().to_string();
    assert!(err.contains("0600"), "{err}");
    assert!(!err.contains(TEST_TOKEN));
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert!(SecretToken::from_file(&path).is_ok());
    std::fs::write(&path, "not-a-token\n").unwrap();
    assert!(SecretToken::from_file(&path).is_err());
}

#[tokio::test]
async fn ticket_note_comments_the_card_and_records_history_on_its_project() {
    use weftos_cos::tools::Tool;
    use weftos_cos::tools::harness::NotesRecord;
    let (addr, posts) = FakeHarness { with_events_route: true, ..Default::default() }.start().await;
    // No default project: the project comes from the ticket.
    let tool = NotesRecord { client: common::harness_client(addr, None) };
    let out = tool.call(json!({"text": "Button debounce is 30 ms", "ticket": "WEFT-902"})).await.unwrap();
    assert_eq!(out.result["project_id"], common::TEST_PROJECT);
    assert!(out.summary.contains("on WEFT-902"), "{}", out.summary);
    let posts = posts.lock().unwrap();
    let comment = posts.iter().find(|(p, _)| p == "/api/harness/tickets").expect("ticket comment");
    assert_eq!(comment.1["action"], "comment");
    assert_eq!(comment.1["ticket_id"], "22222222-2222-4222-8222-222222222222");
    let event = posts.iter().find(|(p, _)| p == &format!("/api/harness/projects/{}/events", common::TEST_PROJECT)).expect("history event");
    assert_eq!(event.1["kind"], "note");
    assert_eq!(event.1["title"], "Button debounce is 30 ms");
}

#[tokio::test]
async fn note_without_a_resolvable_project_is_refused() {
    use weftos_cos::tools::Tool;
    use weftos_cos::tools::harness::NotesRecord;
    let (addr, posts) = FakeHarness { with_events_route: true, ..Default::default() }.start().await;
    let tool = NotesRecord { client: common::harness_client(addr, None) };
    let err = tool.call(json!({"text": "a note about nothing in particular"})).await.unwrap_err().to_string();
    assert!(err.contains("no project to record against"), "{err}");
    let err = tool.call(json!({"text": "a note", "project": "not-a-uuid"})).await.unwrap_err().to_string();
    assert!(err.contains("UUID"), "{err}");
    assert!(posts.lock().unwrap().is_empty(), "nothing was written");
}
