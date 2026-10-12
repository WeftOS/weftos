//! cos.v1.1 endpoints: replies are attributed to the endpoint that asked,
//! only the host speaks its own turns, a remote endpoint gets the `speak`
//! flag (and `none` for one endpoint silences no other), tokens are required
//! off loopback, a non-tailnet bind is refused, and a cos.v1 client that
//! knows nothing about endpoints still works.

mod common;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use common::{ScriptedLlm, TestAgent, build_agent, collect_until_final};
use serde_json::{Value, json};
use tower::ServiceExt;
use weftos_cos::CosError;
use weftos_cos::conversation::{Conversation, UserUtterance};
use weftos_cos::endpoints::TokenStore;
use weftos_cos::events::Event;
use weftos_cos::server::{bind_remote, router_auth};

async fn agent(replies: usize) -> TestAgent {
    build_agent(ScriptedLlm::new((0..replies).map(|i| ScriptedLlm::text(&format!("reply {i}"))).collect()), None, None).await
}

/// The final message of the next turn: (endpoint, speak, turn id).
async fn final_message(rx: &mut tokio::sync::broadcast::Receiver<Event>) -> (Option<String>, bool, weftos_cos::TurnId) {
    let events = collect_until_final(rx, Duration::from_secs(5)).await;
    events
        .into_iter()
        .find_map(|e| match e {
            Event::Message { endpoint, speak, turn_id, is_final: true, .. } => Some((endpoint, speak, turn_id)),
            _ => None,
        })
        .expect("a final message")
}

#[tokio::test]
async fn the_host_speaks_its_own_turns_and_a_remote_endpoint_gets_the_speak_flag() {
    let t = agent(4).await;
    let mut rx = t.agent.events();

    // No endpoint field: the host's own mic.
    let id = t.agent.utter(UserUtterance::voice("hello")).await.unwrap();
    let (ep, speak, turn) = final_message(&mut rx).await;
    assert_eq!((ep.as_deref(), speak), (Some("host"), false), "the host speaks itself; the flag is for remote endpoints");
    assert_eq!(turn, id);
    assert!(t.agent.host_speaks(&id));

    // The glasses: spoken turn, reply flagged for them, not spoken on the Mac.
    let id = t.agent.utter(UserUtterance::voice("hello").from_endpoint("mentra")).await.unwrap();
    let (ep, speak, _) = final_message(&mut rx).await;
    assert_eq!((ep.as_deref(), speak), (Some("mentra"), true));
    assert!(!t.agent.host_speaks(&id));
    assert!(t.agent.is_voice_turn(&id));
    assert_eq!(t.agent.turn_endpoint(&id).as_deref(), Some("mentra"));

    // A typed turn from a remote endpoint is shown, not spoken.
    t.agent.utter(UserUtterance::typed("hello").from_endpoint("mentra")).await.unwrap();
    let (ep, speak, _) = final_message(&mut rx).await;
    assert_eq!((ep.as_deref(), speak), (Some("mentra"), false));

    // A typed host turn is not spoken by the host either.
    let id = t.agent.utter(UserUtterance::typed("hello")).await.unwrap();
    let _ = final_message(&mut rx).await;
    assert!(!t.agent.host_speaks(&id));
}

#[tokio::test]
async fn none_for_one_endpoint_silences_no_other() {
    let t = agent(3).await;
    t.agent.set_silent_endpoints(["mentra".to_string()]);
    let mut rx = t.agent.events();

    t.agent.utter(UserUtterance::voice("a").from_endpoint("mentra")).await.unwrap();
    assert!(!final_message(&mut rx).await.1, "mentra is silent");

    t.agent.utter(UserUtterance::voice("b").from_endpoint("pad")).await.unwrap();
    assert!(final_message(&mut rx).await.1, "another endpoint still speaks");

    let id = t.agent.utter(UserUtterance::voice("c")).await.unwrap();
    let _ = final_message(&mut rx).await;
    assert!(t.agent.host_speaks(&id), "the Mac console is untouched");
}

#[tokio::test]
async fn a_bad_endpoint_name_is_refused() {
    let t = agent(1).await;
    for bad in ["", "Host", "a b", "../x", &"x".repeat(33)] {
        let r = t.agent.utter(UserUtterance::voice("hi").from_endpoint(bad)).await;
        assert!(matches!(r, Err(CosError::Config(_))), "{bad:?}");
    }
}

fn request(method: &str, path: &str, peer: &str, token: Option<&str>, body: Option<Value>) -> Request<Body> {
    let mut b = Request::builder().method(method).uri(path);
    if let Some(tok) = token {
        b = b.header("authorization", format!("Bearer {tok}"));
    }
    let body = match body {
        Some(v) => {
            b = b.header("content-type", "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let mut req = b.body(body).unwrap();
    let peer: SocketAddr = peer.parse().unwrap();
    req.extensions_mut().insert(ConnectInfo(peer));
    req
}

#[tokio::test]
async fn off_loopback_a_token_is_required_and_binds_the_endpoint() {
    let t = agent(3).await;
    let dir = tempfile::tempdir().unwrap();
    let store = TokenStore::new(dir.path());
    let token = std::fs::read_to_string(store.add("mentra").unwrap()).unwrap();
    let other = std::fs::read_to_string(store.add("pad").unwrap()).unwrap();
    let conv: Arc<dyn Conversation> = Arc::new(t.agent.clone());
    let app = router_auth(conv, None, Some(Arc::new(store)));
    let tail = "100.64.0.9:50000";
    let utt = json!({"text": "hello", "source": "voice"});
    let status = |req: Request<Body>| {
        let app = app.clone();
        async move { app.oneshot(req).await.unwrap().status() }
    };

    // No token, wrong token, unknown peer: refused on every route.
    for (m, p, b) in [("POST", "/v1/cos/utterance", Some(utt.clone())), ("GET", "/v1/cos/events", None), ("GET", "/healthz", None), ("POST", "/v1/cos/answer", Some(json!({"kind": "skip", "id": "q"})))] {
        assert_eq!(status(request(m, p, tail, None, b.clone())).await, StatusCode::UNAUTHORIZED, "{m} {p} without a token");
        assert_eq!(status(request(m, p, tail, Some("wfe_wrong"), b)).await, StatusCode::UNAUTHORIZED, "{m} {p} wrong token");
    }
    let mut no_peer = request("GET", "/healthz", tail, None, None);
    no_peer.extensions_mut().remove::<ConnectInfo<SocketAddr>>();
    assert_eq!(status(no_peer).await, StatusCode::UNAUTHORIZED, "an unknown peer is not loopback");

    // Loopback needs no token.
    assert_eq!(status(request("GET", "/healthz", "127.0.0.1:50000", None, None)).await, StatusCode::OK);
    let mut rx = t.agent.events();
    assert_eq!(status(request("POST", "/v1/cos/utterance", "127.0.0.1:50000", None, Some(utt.clone()))).await, StatusCode::ACCEPTED);
    assert_eq!(final_message(&mut rx).await.0.as_deref(), Some("host"));

    // A good token: accepted, and the turn belongs to the token's endpoint.
    assert_eq!(status(request("GET", "/healthz", tail, Some(&token), None)).await, StatusCode::OK);
    assert_eq!(status(request("POST", "/v1/cos/utterance", tail, Some(&token), Some(utt.clone()))).await, StatusCode::ACCEPTED);
    let (ep, speak, _) = final_message(&mut rx).await;
    assert_eq!((ep.as_deref(), speak), (Some("mentra"), true));

    // It cannot speak as another endpoint, or as the host, or use the Mac mic.
    for claimed in ["host", "pad"] {
        let body = json!({"text": "hello", "source": "voice", "endpoint": claimed});
        assert_eq!(status(request("POST", "/v1/cos/utterance", tail, Some(&token), Some(body))).await, StatusCode::FORBIDDEN, "claiming {claimed}");
    }
    let ptt = json!({"down": true});
    assert_eq!(status(request("POST", "/v1/cos/ptt", tail, Some(&token), Some(ptt.clone()))).await, StatusCode::FORBIDDEN);
    assert_eq!(status(request("POST", "/v1/cos/ptt", tail, Some(&other), Some(ptt))).await, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn ptt_names_only_the_host_endpoint() {
    let t = agent(1).await;
    let conv: Arc<dyn Conversation> = Arc::new(t.agent.clone());
    let (addr, server) = weftos_cos::server::bind(conv, "127.0.0.1:0".parse().unwrap()).await.unwrap();
    tokio::spawn(server);
    let c = reqwest::Client::new();
    let url = format!("http://{addr}/v1/cos/ptt");
    let r = c.post(&url).json(&json!({"down": true, "endpoint": "mentra"})).send().await.unwrap();
    assert_eq!(r.status(), 400);
    assert!(r.json::<Value>().await.unwrap()["error"].as_str().unwrap().contains("host microphone"));
    // `host` is the default and is allowed; this agent has no mic, so 503.
    let r = c.post(&url).json(&json!({"down": true, "endpoint": "host"})).send().await.unwrap();
    assert_eq!(r.status(), 503);
}

#[tokio::test]
async fn a_non_tailnet_bind_is_refused_before_a_socket_opens() {
    let t = agent(1).await;
    let dir = tempfile::tempdir().unwrap();
    for bad in ["0.0.0.0:18300", "127.0.0.1:0", "192.168.1.5:18300", "10.0.0.2:18300", "[::]:18300", "[fd7a:115c:a1e0::1]:18300"] {
        let conv: Arc<dyn Conversation> = Arc::new(t.agent.clone());
        let r = bind_remote(conv, None, "127.0.0.1:0".parse().unwrap(), bad.parse().unwrap(), TokenStore::new(dir.path())).await;
        assert!(matches!(r, Err(CosError::Config(ref m)) if m.contains("not a tailnet address")), "{bad}");
    }
    // The local listener stays loopback.
    let conv: Arc<dyn Conversation> = Arc::new(t.agent.clone());
    let r = bind_remote(conv, None, "0.0.0.0:18300".parse().unwrap(), "100.64.0.1:18300".parse().unwrap(), TokenStore::new(dir.path())).await;
    assert!(matches!(r, Err(CosError::Config(_))));
}

#[tokio::test]
async fn a_cos_v1_client_without_endpoint_fields_still_works() {
    let t = agent(1).await;
    let conv: Arc<dyn Conversation> = Arc::new(t.agent.clone());
    let (addr, server) = weftos_cos::server::bind(conv, "127.0.0.1:0".parse().unwrap()).await.unwrap();
    tokio::spawn(server);
    let mut rx = t.agent.events();
    let r = reqwest::Client::new().post(format!("http://{addr}/v1/cos/utterance")).json(&json!({"text": "where are we", "source": "voice"})).send().await.unwrap();
    assert_eq!(r.status(), 202);
    let (ep, speak, _) = final_message(&mut rx).await;
    assert_eq!((ep.as_deref(), speak), (Some("host"), false));
    // The old shape still deserializes, and the new fields stay off the wire when unset.
    let m: Event = serde_json::from_value(json!({"kind": "message", "turn_id": "t", "text": "x", "final": true})).unwrap();
    assert!(matches!(m, Event::Message { endpoint: None, speak: false, .. }));
    let v = serde_json::to_value(&m).unwrap();
    assert!(v.get("endpoint").is_none() && v.get("speak").is_none());
}
