//! ADR-121 decision 9: `notes.record` writes the installation brain with
//! brain metadata, and a note the schema refuses is refused before anything
//! is written to the history or the board.

mod common;

use std::sync::Arc;

use common::{FakeHarness, harness_client};
use serde_json::json;
use weftos_cos::brain::{self, Brain};
use weftos_cos::config::BrainConfig;
use weftos_cos::tools::Tool;
use weftos_cos::tools::harness::NotesRecord;

const DIGEST: &str = "sha256:537fbd002578137017a15183931acc31d8e8800500ec4c6fcb72d472bb5942d6";

fn brain_in(dir: &std::path::Path) -> Arc<Brain> {
    let cfg = BrainConfig {
        path: Some(dir.join("cos.rvf")),
        installation: Some("weftos://local/installations/cos-test".into()),
        package: Some(DIGEST.into()),
        catalog: None,
    };
    Arc::new(Brain::open(&cfg, dir).unwrap())
}

#[tokio::test]
async fn a_note_lands_in_history_and_in_the_brain_with_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let (addr, posts) = FakeHarness { with_events_route: true, ..Default::default() }.start().await;
    let b = brain_in(dir.path());
    let tool = NotesRecord { client: Some(harness_client(addr, Some(common::TEST_PROJECT))), brain: Some(b.clone()) };
    let out = tool
        .call(json!({"text": "Move the agent to PG after C4", "kind": "decision", "why": "PG holds the primary store", "where": ["WEFT-901", "docs/plans/coordinator-move-to-pg.md"]}))
        .await
        .unwrap();
    assert!(out.summary.contains("project history") && out.summary.contains("installation brain"), "{}", out.summary);
    assert_eq!(posts.lock().unwrap().len(), 1, "one history event");

    let id = out.result["brain"]["id"].as_str().unwrap();
    let meta = b.get(id).unwrap();
    brain::validate(&meta).unwrap();
    assert_eq!(meta["kind"], "decision");
    assert_eq!(meta["why"], "PG holds the primary store");
    assert_eq!(meta["scope"], "installation");
    assert_eq!(meta["author"], "chief-of-staff");
    assert_eq!(meta["installation"], "weftos://local/installations/cos-test");
    assert_eq!(meta["package"], DIGEST);
    assert_eq!(b.search("agent to PG", 1)[0].0, id);
}

#[tokio::test]
async fn a_note_the_schema_refuses_writes_nowhere() {
    let dir = tempfile::tempdir().unwrap();
    let (addr, posts) = FakeHarness { with_events_route: true, ..Default::default() }.start().await;
    let b = brain_in(dir.path());
    let tool = NotesRecord { client: Some(harness_client(addr, Some(common::TEST_PROJECT))), brain: Some(b.clone()) };
    let err = tool.call(json!({"text": "a note about a machine path", "where": ["/Users/someone/secret"]})).await.unwrap_err().to_string();
    assert!(err.contains("absolute machine path"), "{err}");
    let err = tool.call(json!({"text": "a note with a bad kind", "kind": "gossip"})).await.unwrap_err().to_string();
    assert!(err.contains("kind"), "{err}");
    assert!(posts.lock().unwrap().is_empty(), "history untouched");
    assert!(b.is_empty(), "brain untouched");
}

#[tokio::test]
async fn the_brain_alone_is_enough_when_there_is_no_harness() {
    let dir = tempfile::tempdir().unwrap();
    let b = brain_in(dir.path());
    let tool = NotesRecord { client: None, brain: Some(b.clone()) };
    let out = tool.call(json!({"text": "Owner prefers short spoken replies"})).await.unwrap();
    assert!(out.summary.contains("installation brain") && !out.summary.contains("history"));
    assert_eq!(b.len(), 1);
    // Nowhere to write at all is a refusal, not a silent success.
    let nowhere = NotesRecord { client: None, brain: None };
    assert!(nowhere.call(json!({"text": "this goes nowhere"})).await.is_err());
}
