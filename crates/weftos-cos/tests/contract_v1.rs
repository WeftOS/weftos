//! The frozen conversation API (`docs/plans/adr-118-cos-contract.md` §3,
//! `cos.v1`). The examples in the document are executable: each must parse
//! as the code's own type and serialize back to the same JSON, and every
//! event kind the code has must be documented. The HTTP half checks the
//! status codes, the event order of a turn and the version string.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{ScriptedLlm, build_agent};
use serde_json::{Value, json};
use weftos_cos::conversation::{Conversation, GuideAnswer, UserUtterance};
use weftos_cos::events::Event;
use weftos_cos::server::{API_VERSION, PttRequest};

const DOC: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/plans/adr-118-cos-contract.md"));

/// Fenced blocks whose info string is `json <tag>`.
fn blocks(tag: &str) -> Vec<Value> {
    let open = format!("```json {tag}\n");
    let mut out = Vec::new();
    let mut rest = DOC;
    while let Some(i) = rest.find(&open) {
        let body = &rest[i + open.len()..];
        let end = body.find("```").expect("closed block");
        out.push(serde_json::from_str(&body[..end]).unwrap_or_else(|e| panic!("{tag} block is not JSON: {e}")));
        rest = &body[end..];
    }
    out
}

/// Every `kind` the event type can serialize. Adding a variant without
/// documenting it fails `every_event_kind_is_documented`.
const KINDS: &[&str] = &["state", "status", "token", "message", "tool", "page", "layout", "checklist", "steps", "ask", "card"];

/// Exhaustive on purpose: a new `Event` variant fails to compile here until
/// it is added to `KINDS` and documented in §3.2.
fn kind_of(e: &Event) -> &'static str {
    match e {
        Event::State { .. } => "state",
        Event::Status { .. } => "status",
        Event::Token { .. } => "token",
        Event::Message { .. } => "message",
        Event::Tool { .. } => "tool",
        Event::Page { .. } => "page",
        Event::Layout { .. } => "layout",
        Event::Checklist { .. } => "checklist",
        Event::Steps { .. } => "steps",
        Event::Ask { .. } => "ask",
        Event::Card { .. } => "card",
    }
}

#[test]
fn documented_events_round_trip_through_the_code() {
    let events = blocks("cos-event");
    assert!(events.len() >= KINDS.len());
    for doc in &events {
        let ev: Event = serde_json::from_value(doc.clone()).unwrap_or_else(|e| panic!("{doc} is not an Event: {e}"));
        assert_eq!(&serde_json::to_value(&ev).unwrap(), doc, "round trip changed {doc}");
        assert_eq!(kind_of(&ev), doc["kind"].as_str().unwrap());
    }
}

#[test]
fn every_event_kind_is_documented() {
    let documented: std::collections::BTreeSet<String> = blocks("cos-event").iter().map(|e| e["kind"].as_str().unwrap().to_string()).collect();
    let all: std::collections::BTreeSet<String> = KINDS.iter().map(|k| k.to_string()).collect();
    assert_eq!(documented, all);
    for k in KINDS {
        assert!(DOC.contains(&format!("| `{k}` |")), "{k} missing from the §3.2 table");
    }
}

#[test]
fn documented_requests_parse_as_the_code_types() {
    let reqs = blocks("cos-request");
    let utt: UserUtterance = serde_json::from_value(reqs[0].clone()).unwrap();
    assert_eq!(serde_json::to_value(&utt).unwrap(), reqs[0]);
    // v1.1: the same body with an endpoint round-trips, and the v1 body above
    // did not gain one.
    let remote: UserUtterance = serde_json::from_value(reqs[1].clone()).unwrap();
    assert_eq!(remote.endpoint.as_deref(), Some("mentra"));
    assert_eq!(serde_json::to_value(&remote).unwrap(), reqs[1]);
    assert_eq!(utt.endpoint, None);
    let ans: GuideAnswer = serde_json::from_value(reqs[2].clone()).unwrap();
    assert_eq!(serde_json::to_value(&ans).unwrap(), reqs[2]);
    let ptt: PttRequest = serde_json::from_value(reqs[3].clone()).unwrap();
    assert!(ptt.down);
    assert_eq!(ptt.endpoint, None);
}

#[test]
fn the_document_names_the_version_the_server_reports() {
    assert_eq!(API_VERSION, "cos.v1");
    assert!(DOC.contains("version `cos.v1`") && DOC.contains("\"api\": \"cos.v1\""));
    // The additive v1.1 section exists and the routes table names the field.
    assert!(DOC.contains("### 3.3 Endpoints and remote access"));
    assert!(DOC.contains("\"endpoint\": string"));
}

async fn serve() -> (std::net::SocketAddr, common::TestAgent) {
    let llm = ScriptedLlm::new(vec![ScriptedLlm::text("Two of three nodes are up.")]);
    let t = build_agent(llm, None, None).await;
    let conv: Arc<dyn Conversation> = Arc::new(t.agent.clone());
    let (addr, server) = weftos_cos::server::bind(conv, "127.0.0.1:0".parse().unwrap()).await.unwrap();
    tokio::spawn(server);
    (addr, t)
}

#[tokio::test]
async fn routes_answer_with_the_documented_statuses() {
    let (addr, _t) = serve().await;
    let c = reqwest::Client::new();
    let url = |p: &str| format!("http://{addr}{p}");

    let h: Value = c.get(url("/healthz")).send().await.unwrap().json().await.unwrap();
    assert_eq!(h, json!({"ok": true, "api": "cos.v1"}));

    let ok = c.post(url("/v1/cos/utterance")).json(&json!({"text": "hello", "source": "voice"})).send().await.unwrap();
    assert_eq!(ok.status(), 202);
    let body: Value = ok.json().await.unwrap();
    assert_eq!(body["turn_id"].as_str().unwrap().len(), 32);

    for bad in [json!({"text": "  ", "source": "typed"}), json!({"text": "x", "source": "pigeon"}), json!({"text": "a".repeat(8001), "source": "typed"}), json!({"source": "typed"})] {
        let r = c.post(url("/v1/cos/utterance")).json(&bad).send().await.unwrap();
        assert_eq!(r.status(), 400, "{bad}");
        assert!(r.json::<Value>().await.unwrap()["error"].is_string());
    }
    let r = c.post(url("/v1/cos/utterance")).header("content-type", "application/json").body("{not json").send().await.unwrap();
    assert_eq!(r.status(), 400);

    // An answer for a survey that was never asked is refused, not ignored.
    let r = c.post(url("/v1/cos/answer")).json(&json!({"kind": "answer", "id": "nope", "value": 1})).send().await.unwrap();
    assert_eq!(r.status(), 400);
    let r = c.post(url("/v1/cos/answer")).json(&json!({"kind": "wave", "id": "q1"})).send().await.unwrap();
    assert_eq!(r.status(), 400);

    // No microphone: push-to-talk is unavailable, and says why.
    let r = c.post(url("/v1/cos/ptt")).json(&json!({"down": true})).send().await.unwrap();
    assert_eq!(r.status(), 503);
    assert!(r.json::<Value>().await.unwrap()["error"].as_str().unwrap().contains("--mic"));
    let r = c.post(url("/v1/cos/ptt")).json(&json!({"up": true})).send().await.unwrap();
    assert_eq!(r.status(), 400);
}

#[tokio::test]
async fn a_turn_streams_in_the_documented_order() {
    let (addr, _t) = serve().await;
    let c = reqwest::Client::new();
    let mut sse = c.get(format!("http://{addr}/v1/cos/events")).send().await.unwrap();
    assert!(sse.headers()["content-type"].to_str().unwrap().starts_with("text/event-stream"));
    c.post(format!("http://{addr}/v1/cos/utterance")).json(&json!({"text": "where are we", "source": "typed"})).send().await.unwrap();

    let mut body = String::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline && !body.contains("\"state\":\"idle\"") {
        if let Ok(Ok(Some(chunk))) = tokio::time::timeout(Duration::from_millis(300), sse.chunk()).await {
            body.push_str(&String::from_utf8_lossy(&chunk));
        }
    }
    let events: Vec<Event> = body.lines().filter_map(|l| l.strip_prefix("data: ")).map(|d| serde_json::from_str(d).expect("every line is an Event")).collect();
    let shape: Vec<String> = events
        .iter()
        .map(|e| match e {
            Event::State { state, .. } => format!("state:{}", serde_json::to_value(state).unwrap().as_str().unwrap()),
            Event::Status { .. } => "status".into(),
            Event::Token { .. } => "token".into(),
            Event::Message { is_final, .. } => format!("message:{is_final}"),
            other => format!("other:{other:?}"),
        })
        .collect();
    let compact: Vec<&str> = shape.iter().map(String::as_str).fold(Vec::new(), |mut v, s| {
        if v.last() != Some(&s) {
            v.push(s);
        }
        v
    });
    assert_eq!(compact, ["state:thinking", "status", "token", "state:speaking", "message:true", "state:idle"]);
}

#[tokio::test]
async fn glance_routes_answer_as_documented() {
    assert!(DOC.contains("`GET /v1/cos/glance`") && DOC.contains("`GET /v1/cos/glance/art.png`") && DOC.contains("Answer to card"));
    let (addr, _t) = serve().await;
    let c = reqwest::Client::new();
    let snap: Value = c.get(format!("http://{addr}/v1/cos/glance")).send().await.unwrap().json().await.unwrap();
    for k in ["card", "fields", "art"] {
        assert!(snap.get(k).is_some(), "{k}");
    }
    assert!(snap["card"].is_null() && snap["art"].is_null());
    for k in ["state", "task", "next", "alert", "model", "ts"] {
        assert!(snap["fields"].get(k).is_some(), "fields.{k}");
    }
    assert_eq!(c.get(format!("http://{addr}/v1/cos/glance/art.png")).send().await.unwrap().status(), 404);
    let stale = c.post(format!("http://{addr}/v1/cos/answer")).json(&json!({"kind": "answer", "id": "deadbeef", "value": "ship"})).send().await.unwrap();
    assert_eq!(stale.status(), 400);
}
