//! Per-turn traces and status at startup: what a turn records, that arguments
//! never appear, the trace routes and their auth, and the status snapshot
//! before the first turn.

mod common;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use common::{ScriptedLlm, build_agent, collect_until_final};
use serde_json::{Value, json};
use tower::ServiceExt;
use weftos_cos::endpoints::TokenStore;
use weftos_cos::server::router_auth;
use weftos_cos::{Conversation, TurnId, UserUtterance};

fn request(path: &str, peer: &str, token: Option<&str>) -> Request<Body> {
    let mut b = Request::builder().method("GET").uri(path);
    if let Some(t) = token {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    let mut req = b.body(Body::empty()).unwrap();
    req.extensions_mut().insert(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
    req
}

#[tokio::test]
async fn a_turn_records_model_role_tools_latency_and_no_arguments() {
    let llm = ScriptedLlm::new(vec![
        ScriptedLlm::tool_call("nodes_health", json!({"verbose": "needle-in-the-args"})),
        ScriptedLlm::text("Two of three nodes are healthy."),
    ]);
    let t = build_agent(llm, None, None).await;
    let mut rx = t.agent.events();
    let id = TurnId::new();
    t.agent.run_turn(id.clone(), UserUtterance::typed("which node is struggling").from_endpoint("mcp")).await;
    let _ = collect_until_final(&mut rx, Duration::from_secs(2)).await;

    let tr = t.agent.trace(&id.0).expect("a trace for the finished turn");
    assert_eq!((tr.runtime.as_str(), tr.model.as_str(), tr.role.as_str()), ("local", "local/fast-model", "quick"));
    assert_eq!(tr.endpoint, "mcp");
    assert!(tr.tools_offered.iter().any(|n| n == "nodes.health"), "offered: {:?}", tr.tools_offered);
    assert_eq!(tr.tools_called.len(), 1);
    assert_eq!(tr.tools_called[0].name, "nodes.health");
    assert!(tr.tools_called[0].ok);
    assert_eq!(tr.tools_called[0].args_digest.len(), 16);
    assert_eq!(tr.rounds, 2, "one tool round and the answer");
    assert_eq!(tr.cost_usd, 0.0);
    assert!(tr.gates.is_empty() && !tr.failed);

    let wire = serde_json::to_string(&tr).unwrap();
    assert!(!wire.contains("needle-in-the-args"), "a trace holds a digest, never the arguments: {wire}");
}

#[tokio::test]
async fn a_held_action_is_a_gate_and_the_yes_is_a_confirmation_trace() {
    let llm = ScriptedLlm::new(vec![ScriptedLlm::tool_call("work_kickoff", json!({"card": "WEFT-902"}))]);
    let t = build_agent(llm, None, None).await;
    let mut rx = t.agent.events();
    let first = TurnId::new();
    t.agent.run_turn(first.clone(), UserUtterance::typed("kick off WEFT-902")).await;
    let _ = collect_until_final(&mut rx, Duration::from_secs(2)).await;
    let held = t.agent.trace(&first.0).unwrap();
    assert_eq!(held.gates, vec!["work.kickoff:held"]);
    assert!(held.tools_called.is_empty(), "a held call has not run");

    let second = TurnId::new();
    t.agent.run_turn(second.clone(), UserUtterance::typed("yes")).await;
    let done = t.agent.trace(&second.0).unwrap();
    assert_eq!(done.role, "confirmation");
    assert_eq!(done.gates, vec!["work.kickoff:confirmed"]);
    assert_eq!(done.tools_called[0].name, "work.kickoff");
}

#[tokio::test]
async fn a_failed_turn_is_traced_as_failed() {
    // The scripted provider never errors, so a turn that cannot reach any
    // model is built from a provider that refuses.
    struct Down;
    #[async_trait::async_trait]
    impl clawft_core::pipeline::transport::LlmProvider for Down {
        async fn complete(&self, _: &str, _: &[Value], _: &[Value], _: Option<i32>, _: Option<f64>, _: Option<&Value>) -> Result<Value, String> {
            Err("down".into())
        }
    }
    use weftos_cos::routing::{ModelRoutes, Route};
    let routes = ModelRoutes::new(Route::new("local/m", Arc::new(Down)), Route::new("local/m", Arc::new(Down)), 0.3);
    let agent = weftos_cos::CosAgent::new(routes, weftos_cos::tools::ToolRegistry::new(), weftos_cos::tools::ToolLog::default());
    let id = TurnId::new();
    agent.run_turn(id.clone(), UserUtterance::typed("hello")).await;
    let tr = agent.trace(&id.0).unwrap();
    assert!(tr.failed);
    assert_eq!(agent.status()["state"], "blocked");
}

#[tokio::test]
async fn trace_routes_list_since_and_fetch_one() {
    let t = build_agent(ScriptedLlm::new((0..3).map(|i| ScriptedLlm::text(&format!("r{i}"))).collect()), None, None).await;
    let conv: Arc<dyn Conversation> = Arc::new(t.agent.clone());
    let app = router_auth(conv, None, None);
    let ids: Vec<TurnId> = (0..3).map(|_| TurnId::new()).collect();
    for id in &ids {
        t.agent.run_turn(id.clone(), UserUtterance::typed("hello")).await;
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let get = |path: String| {
        let app = app.clone();
        async move {
            let r = app.oneshot(request(&path, "127.0.0.1:1", None)).await.unwrap();
            let code = r.status();
            let bytes = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
            (code, serde_json::from_slice::<Value>(&bytes).unwrap())
        }
    };

    let (code, all) = get("/v1/cos/traces".into()).await;
    assert_eq!(code, StatusCode::OK);
    let listed: Vec<&str> = all["traces"].as_array().unwrap().iter().map(|t| t["turn_id"].as_str().unwrap()).collect();
    assert_eq!(listed, ids.iter().map(|i| i.0.as_str()).collect::<Vec<_>>(), "oldest first");

    // `since` is inclusive: the second turn's own timestamp returns it and the third.
    let ts = all["traces"][1]["ts"].as_str().unwrap().replace(':', "%3A");
    let (_, later) = get(format!("/v1/cos/traces?since={ts}")).await;
    assert_eq!(later["traces"].as_array().unwrap().len(), 2);
    let (_, last) = get("/v1/cos/traces?limit=1".into()).await;
    assert_eq!(last["traces"][0]["turn_id"], ids[2].0.as_str());

    let (code, one) = get(format!("/v1/cos/traces/{}", ids[0])).await;
    assert_eq!((code, one["turn_id"].as_str()), (StatusCode::OK, Some(ids[0].0.as_str())));
    let (code, miss) = get("/v1/cos/traces/nope".into()).await;
    assert_eq!(code, StatusCode::NOT_FOUND);
    assert!(miss["error"].is_string());

    // A malformed query refuses; it never reads as "no filter".
    for bad in ["since=yesterday", "since=", "limit=0", "limit=201", "limit=x", "limit"] {
        let (code, v) = get(format!("/v1/cos/traces?{bad}")).await;
        assert_eq!(code, StatusCode::BAD_REQUEST, "{bad}");
        assert!(v["error"].is_string());
    }
}

#[tokio::test]
async fn trace_and_tool_routes_need_a_token_off_loopback() {
    let t = build_agent(ScriptedLlm::new(vec![ScriptedLlm::text("ok")]), None, None).await;
    let dir = tempfile::tempdir().unwrap();
    let store = TokenStore::new(dir.path());
    let token = std::fs::read_to_string(store.add("mentra").unwrap()).unwrap();
    let conv: Arc<dyn Conversation> = Arc::new(t.agent.clone());
    let app = router_auth(conv, None, Some(Arc::new(store)));
    let tail = "100.64.0.9:50000";
    let status = |path: &str, tok: Option<&str>, peer: &str| {
        let app = app.clone();
        let req = request(path, peer, tok);
        async move { app.oneshot(req).await.unwrap().status() }
    };
    for path in ["/v1/cos/traces", "/v1/cos/traces/abc", "/v1/cos/tools", "/v1/cos/devices", "/v1/cos/status"] {
        assert_eq!(status(path, None, tail).await, StatusCode::UNAUTHORIZED, "{path} without a token");
        assert_eq!(status(path, Some("wfe_wrong"), tail).await, StatusCode::UNAUTHORIZED, "{path} wrong token");
    }
    assert_eq!(status("/v1/cos/traces", Some(&token), tail).await, StatusCode::OK);
    assert_eq!(status("/v1/cos/traces", None, "127.0.0.1:1").await, StatusCode::OK, "loopback needs no token");
}

#[tokio::test]
async fn status_before_the_first_turn_is_idle_with_the_quick_model() {
    let t = build_agent(ScriptedLlm::new(vec![ScriptedLlm::text("ok")]), None, None).await;
    let s = t.agent.status();
    assert_eq!(s["state"], "idle");
    assert_eq!(s["model"], "local/fast-model");
    assert!(s["pending"].is_null());
    assert_eq!(s["lanes_running"], 0);

    // It then follows the turn: a planning turn names the plan-tier model.
    let mut rx = t.agent.events();
    t.agent.run_turn(TurnId::new(), UserUtterance::typed("plan the roadmap for next week")).await;
    let _ = collect_until_final(&mut rx, Duration::from_secs(2)).await;
    let s = t.agent.status();
    assert_eq!(s["model"], "anthropic/big-model");
    assert_eq!(s["state"], "idle");
}

#[tokio::test]
async fn a_programmatic_endpoint_cannot_answer_a_gate_the_phone_asked() {
    let llm = ScriptedLlm::new(vec![ScriptedLlm::tool_call("work_kickoff", json!({"card": "WEFT-902"})), ScriptedLlm::text("a normal answer")]);
    let t = build_agent(llm, None, None).await;
    let mut rx = t.agent.events();
    let held = t.agent.utter(UserUtterance::typed("kick off WEFT-902").from_endpoint("dashboard")).await.unwrap();
    let _ = collect_until_final(&mut rx, Duration::from_secs(2)).await;
    assert_eq!(t.agent.status()["pending"]["turn_id"], held.0.as_str());
    assert_eq!(t.agent.status()["pending"]["endpoint"], "dashboard");

    // An mcp "yes" is an ordinary turn; the gate stays held and nothing ran.
    let m = t.agent.utter(UserUtterance::typed("yes").from_endpoint("mcp")).await.unwrap();
    let _ = collect_until_final(&mut rx, Duration::from_secs(2)).await;
    assert_eq!(t.agent.status()["pending"]["turn_id"], held.0.as_str(), "still held");
    assert!(!t.dir.path().join("staging").exists(), "the kickoff did not run");
    assert!(t.agent.trace(&m.0).unwrap().gates.is_empty());

    // The owner's own phone can answer it.
    t.agent.utter(UserUtterance::typed("yes").from_endpoint("dashboard")).await.unwrap();
    let _ = collect_until_final(&mut rx, Duration::from_secs(2)).await;
    assert!(t.agent.status()["pending"].is_null());
    assert!(t.dir.path().join("staging").exists());
}
