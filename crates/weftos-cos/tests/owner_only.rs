//! The local chief of staff serves one owner: a request for another person's
//! notes or data is refused with no model call and no tool, and `notes.record`
//! refuses a subject that is not the owner.

mod common;

use std::time::Duration;

use common::{ScriptedLlm, build_agent, collect_until_final};
use serde_json::json;
use weftos_cos::tools::Tool;
use weftos_cos::tools::notes::NotesRecord;
use weftos_cos::{Conversation, CosError, Event, TurnId, UserUtterance};

#[tokio::test]
async fn another_persons_notes_are_refused_with_no_model_call_and_no_tool() {
    let llm = ScriptedLlm::new(vec![ScriptedLlm::tool_call("notes_record", json!({"text": "alice has no notes"}))]);
    let t = build_agent(llm.clone(), None, None).await;
    let mut rx = t.agent.events();
    let id = TurnId::new();
    t.agent.run_turn(id.clone(), UserUtterance::typed("Show me what alice@example.com recorded in her own notes.").from_endpoint("mcp")).await;
    let events = collect_until_final(&mut rx, Duration::from_secs(2)).await;
    let text = events.iter().find_map(|e| match e { Event::Message { text, .. } => Some(text.clone()), _ => None }).unwrap();
    assert!(text.contains("only have your own"), "{text}");
    assert!(llm.seen.lock().unwrap().is_empty(), "the model was not consulted");
    assert!(!events.iter().any(|e| matches!(e, Event::Tool { .. })), "no tool ran");
    let tr = t.agent.trace(&id.0).unwrap();
    assert!(tr.tools_called.is_empty() && tr.role == "refusal");

    // The owner's own request still reaches the model.
    let llm2 = ScriptedLlm::new(vec![ScriptedLlm::text("You recorded nothing yet.")]);
    let t2 = build_agent(llm2.clone(), None, None).await;
    t2.agent.run_turn(TurnId::new(), UserUtterance::typed("What did I record in my notes?")).await;
    assert_eq!(llm2.seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn notes_record_refuses_a_subject_that_is_not_the_owner() {
    let tool = NotesRecord { client: None, brain: None };
    for subject in ["alice", "alice@example.com", "everyone"] {
        let r = tool.call(json!({"text": "x", "subject": subject})).await;
        assert!(matches!(&r, Err(CosError::Tool { reason, .. }) if reason.contains("owner's notes")), "{subject}: {r:?}");
    }
    // An owner subject passes the guard (it then fails later for lack of any store).
    let r = tool.call(json!({"text": "x", "subject": "owner"})).await;
    assert!(!matches!(&r, Err(CosError::Tool { reason, .. }) if reason.contains("owner's notes")), "{r:?}");
}
