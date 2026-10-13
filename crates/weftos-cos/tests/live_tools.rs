//! The live-data tools against a double: `board.read`, `board.claim`,
//! `board.note`, `card.current` (harness) and `mesh.routes` / `nodes.health`
//! (daemon RPC). Claim and note change the shared board, so they wait for a
//! yes. The one live smoke test is skipped without a harness credential.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use common::{FakeHarness, ScriptedLlm, collect_until_final, harness_client};
use serde_json::{Value, json};
use weftos_cos::CosError;
use weftos_cos::agent::CosAgent;
use weftos_cos::routing::{ModelRoutes, Route};
use weftos_cos::tools::cards::{BoardClaim, BoardNote, BoardRead, CardCurrent, Focus};
use weftos_cos::tools::nodes::{MeshRoutes, MeshRpc};
use weftos_cos::tools::{Tool, ToolLog, ToolRegistry};
use weftos_cos::{Conversation, Event, TurnId, UserUtterance};

const WEFT_902: &str = "22222222-2222-4222-8222-222222222222";

fn card_tools(addr: std::net::SocketAddr) -> (ToolRegistry, Focus) {
    let client = harness_client(addr, None);
    let focus = Focus::default();
    let tools = ToolRegistry::new()
        .with(Arc::new(BoardRead { client: client.clone(), focus: focus.clone() }))
        .with(Arc::new(BoardClaim { client: client.clone(), focus: focus.clone() }))
        .with(Arc::new(BoardNote { client: client.clone(), focus: focus.clone() }))
        .with(Arc::new(CardCurrent { client, focus: focus.clone() }));
    (tools, focus)
}

fn agent(llm: Arc<ScriptedLlm>, tools: ToolRegistry) -> CosAgent {
    let route = Route::new("local/fast-model", llm);
    CosAgent::new(ModelRoutes::new(route.clone(), route, 0.3), tools, ToolLog::new(None))
}

#[tokio::test]
async fn board_read_returns_the_card_and_current_follows_it() {
    let (addr, _) = FakeHarness::default().start().await;
    let (tools, focus) = card_tools(addr);
    // Nothing read yet: the most recent in-progress card.
    let cur = tools.find("card.current").unwrap().call(json!({})).await.unwrap();
    assert_eq!(cur.result["source"], "in_progress");
    assert_eq!(cur.result["card"]["ref"], "WEFT-901");

    let read = tools.find("board.read").unwrap().call(json!({"ticket": "WEFT-902"})).await.unwrap();
    assert_eq!(read.result["card"]["title"], "Pad button on GPIO");
    assert_eq!(read.result["card"]["status"], "todo");
    assert_eq!(focus.get().as_deref(), Some("WEFT-902"));

    let cur = tools.find("card.current").unwrap().call(json!({})).await.unwrap();
    assert_eq!(cur.result["source"], "focus");
    assert_eq!(cur.result["card"]["ref"], "WEFT-902");
}

#[tokio::test]
async fn board_read_refuses_a_missing_argument_and_an_unknown_card() {
    let (addr, _) = FakeHarness::default().start().await;
    let (tools, _) = card_tools(addr);
    let read = tools.find("board.read").unwrap();
    assert!(read.call(json!({})).await.unwrap_err().to_string().contains("missing argument"));
    assert!(read.call(json!({"ticket": "WEFT-9999"})).await.unwrap_err().to_string().contains("ticket not found"));
}

fn tool_statuses(events: &[Event]) -> Vec<(String, String)> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::Tool { name, status, .. } => Some((name.clone(), status.clone())),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn claim_waits_for_a_yes_and_then_posts_the_claim() {
    let (addr, posts) = FakeHarness::default().start().await;
    let (tools, _) = card_tools(addr);
    let llm = ScriptedLlm::new(vec![ScriptedLlm::tool_call("board_claim", json!({"ticket": "WEFT-902"}))]);
    let a = agent(llm, tools);
    let mut rx = a.events();

    a.run_turn(TurnId::new(), UserUtterance::voice("claim the pad button card")).await;
    let first = collect_until_final(&mut rx, Duration::from_secs(2)).await;
    assert_eq!(tool_statuses(&first), vec![("board.claim".to_string(), "awaiting_confirmation".to_string())]);
    let question = first.iter().find_map(|e| match e { Event::Message { text, .. } => Some(text.clone()), _ => None }).unwrap();
    assert!(question.contains("claim WEFT-902 on the board"), "{question}");
    assert!(posts.lock().unwrap().is_empty(), "nothing is written before the yes");

    a.run_turn(TurnId::new(), UserUtterance::voice("yes")).await;
    let second = collect_until_final(&mut rx, Duration::from_secs(2)).await;
    assert_eq!(tool_statuses(&second).last().unwrap(), &("board.claim".to_string(), "ok".to_string()));
    let posts = posts.lock().unwrap();
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0].1, json!({"ticket_id": WEFT_902, "action": "claim"}));
}

#[tokio::test]
async fn a_board_comment_is_gated_and_carries_the_text() {
    let (addr, posts) = FakeHarness::default().start().await;
    let (tools, _) = card_tools(addr);
    let llm = ScriptedLlm::new(vec![ScriptedLlm::tool_call("board_note", json!({"ticket": "WEFT-902", "text": "Debounce is 30 ms"}))]);
    let a = agent(llm, tools);
    let mut rx = a.events();
    a.run_turn(TurnId::new(), UserUtterance::typed("comment on the pad card that debounce is 30 ms")).await;
    let _ = collect_until_final(&mut rx, Duration::from_secs(2)).await;
    assert!(posts.lock().unwrap().is_empty());
    a.run_turn(TurnId::new(), UserUtterance::typed("yes")).await;
    let _ = collect_until_final(&mut rx, Duration::from_secs(2)).await;
    let posts = posts.lock().unwrap();
    assert_eq!(posts[0].1, json!({"ticket_id": WEFT_902, "action": "comment", "value": "Debounce is 30 ms"}));
}

#[tokio::test]
async fn card_writes_without_a_valid_credential_are_refused_and_leak_nothing() {
    let (addr, posts) = FakeHarness::default().start().await;
    let bad = Arc::new(weftos_cos::tools::harness::HarnessClient::new(
        &format!("http://{addr}"),
        "/api/harness/projects/{project_id}/events",
        None,
        weftos_cos::tools::harness::SecretToken::for_tests(format!("wfb_{}", "e".repeat(64))),
    ));
    let claim = BoardClaim { client: bad, focus: Focus::default() };
    let err = claim.call(json!({"ticket": "WEFT-902"})).await.unwrap_err().to_string();
    assert!(err.contains("HTTP 401"), "{err}");
    assert!(!err.contains(&"e".repeat(64)));
    assert!(posts.lock().unwrap().is_empty());
}

/// Records which methods were asked; answers from a table.
struct FakeRpc {
    reachable: bool,
    asked: Mutex<Vec<String>>,
}

#[async_trait]
impl MeshRpc for FakeRpc {
    async fn call(&self, method: &str) -> Result<Option<Value>, CosError> {
        self.asked.lock().unwrap().push(method.to_string());
        if !self.reachable {
            return Ok(None);
        }
        match method {
            "ipc.topics" => Ok(Some(json!([{"topic": "mesh.heartbeat"}, {"topic": "kernel.log"}]))),
            "cluster.facts" => Ok(Some(json!({"nodes": [{"node_id": "pg"}]}))),
            other => Err(CosError::Tool { name: "mesh.routes".into(), reason: format!("unexpected {other}") }),
        }
    }
}

#[tokio::test]
async fn mesh_routes_reads_topics_and_facts_only() {
    let rpc = Arc::new(FakeRpc { reachable: true, asked: Mutex::new(vec![]) });
    let out = MeshRoutes { rpc: rpc.clone() }.call(json!({})).await.unwrap();
    assert_eq!(out.summary, "2 route(s) on the daemon");
    assert_eq!(out.result["facts"]["nodes"][0]["node_id"], "pg");
    assert_eq!(*rpc.asked.lock().unwrap(), vec!["ipc.topics", "cluster.facts"]);
    for m in rpc.asked.lock().unwrap().iter() {
        assert!(clawft_rpc::handshake::is_read_only_method(m), "{m} must be read-only");
    }
}

#[tokio::test]
async fn mesh_routes_says_so_when_the_daemon_is_unreachable() {
    let rpc = Arc::new(FakeRpc { reachable: false, asked: Mutex::new(vec![]) });
    let out = MeshRoutes { rpc }.call(json!({})).await.unwrap();
    assert_eq!(out.result["reachable"], false);
    assert_eq!(out.result["health_known"], false);
    assert!(out.summary.contains("can't reach the daemon"), "{}", out.summary);
    assert!(!out.summary.contains("unhealthy"));
}

/// Live smoke: reads the real board with the owner's harness credential.
/// Skipped (and says so) unless a credential file exists and
/// `WEFTOS_COS_LIVE=1`. Read-only: it never claims, comments or records.
#[tokio::test]
async fn live_smoke_reads_the_real_board() {
    use weftos_cos::config::HarnessConfig;
    use weftos_cos::tools::harness::HarnessClient;
    let cfg = HarnessConfig::default();
    if std::env::var("WEFTOS_COS_LIVE").as_deref() != Ok("1") || !cfg.token_file.exists() {
        eprintln!("skipped: set WEFTOS_COS_LIVE=1 with a harness token file to run the live smoke");
        return;
    }
    let client = Arc::new(HarnessClient::from_config(&cfg).expect("token file"));
    let focus = Focus::default();
    let current = CardCurrent { client: client.clone(), focus: focus.clone() }.call(json!({})).await.expect("card.current");
    eprintln!("live: {}", current.summary);
    let first = client.tickets(None, 1).await.expect("tickets").into_iter().next().expect("a ticket");
    let reference = first["id"].as_str().unwrap().to_string();
    let read = BoardRead { client, focus }.call(json!({"ticket": reference})).await.expect("board.read");
    assert!(read.result["card"]["title"].is_string());
}

/// Live smoke for the daemon tools: reads the local weaver daemon through the
/// read-only RPC allowlist. Skipped unless `WEFTOS_COS_LIVE=1`; when no daemon
/// is running it checks the tools say so instead of failing.
#[tokio::test]
async fn live_smoke_reads_the_daemon_or_says_it_is_unreachable() {
    use weftos_cos::tools::nodes::{DaemonHealthSource, DaemonRpc, NodesHealth};
    if std::env::var("WEFTOS_COS_LIVE").as_deref() != Ok("1") {
        eprintln!("skipped: set WEFTOS_COS_LIVE=1 to read the local daemon");
        return;
    }
    let health = NodesHealth { source: Arc::new(DaemonHealthSource) }.call(json!({})).await.expect("nodes.health");
    let routes = MeshRoutes { rpc: Arc::new(DaemonRpc) }.call(json!({})).await.expect("mesh.routes");
    eprintln!("live: {} / {}", health.summary, routes.summary);
    assert_eq!(health.result["reachable"], routes.result["reachable"]);
}

#[tokio::test]
async fn a_short_id_from_the_activity_list_resolves_a_card() {
    let (addr, _) = FakeHarness::default().start().await;
    let (tools, _) = card_tools(addr);
    let read = tools.find("board.read").unwrap();
    // The list shows the first 8 characters of an id; the card reads back by them.
    let card = read.call(json!({"ticket": "22222222"})).await.unwrap();
    assert_eq!(card.result["card"]["title"], "Pad button on GPIO");
    // Shorter than 8 is not a reference, and an unknown prefix is not found.
    assert!(read.call(json!({"ticket": "2222"})).await.unwrap_err().to_string().contains("ticket not found"));
    assert!(read.call(json!({"ticket": "99999999"})).await.unwrap_err().to_string().contains("ticket not found"));
}

#[tokio::test]
async fn activity_shows_goals_without_their_bodies() {
    let (addr, _) = FakeHarness::default().start().await;
    let client = harness_client(addr, None);
    let out = weftos_cos::tools::harness::ProjectsActivity { client }.call(json!({})).await.unwrap();
    let goal = &out.result["goals"][0];
    assert_eq!(goal["title"], "Console on the pad");
    assert!(goal.get("description").is_none() && goal.get("id").is_none());
}
