//! Hosted-API roles, model pinning and per-role stall timeouts. The "hosted
//! API" is a fake on a random loopback port; no test reaches the internet.
//! The key variable is one the test process already has (`PATH`), so no test
//! mutates the environment.

use std::sync::Arc;
use std::time::Duration;

use super::hosted::{HostedRoute, RolePolicy, parse_hosted_base, pin_model, valid_env_name};
use super::listener::{InferProxy, OccupiedPolicy, Started};
use super::support::*;
use super::table::PlacementTable;
use super::types::*;

const KEY_VAR: &str = "PATH";

fn key() -> String {
    std::env::var(KEY_VAR).unwrap()
}

fn route(base: &str, var: &str) -> HostedRoute {
    HostedRoute { base_url: format!("{base}/v1"), api_key_env: var.into(), model: "grok-test".into() }
}

fn table() -> Arc<PlacementTable> {
    Arc::new(PlacementTable::new("node-a", None, None))
}

async fn start(t: &Arc<PlacementTable>, limits: ProxyLimits) -> InferProxy {
    let a = "127.0.0.1:0".parse().unwrap();
    match InferProxy::start("hard", a, OccupiedPolicy::Refuse, t.clone(), limits, None).await.unwrap() {
        Started::Running(p) => p,
        Started::Adopted(_) => panic!("adopted"),
    }
}

async fn hosted(var: &str, reply: Reply) -> (Fake, InferProxy) {
    let up = fake(reply).await;
    let t = table();
    t.register_hosted("hard", route(&up.base(), var)).unwrap();
    (up, start(&t, small_limits()).await)
}

#[tokio::test]
async fn a_hosted_role_replaces_the_auth_and_pins_the_model() {
    let (up, p) = hosted(KEY_VAR, Reply::ok(r#"{"id":"x"}"#)).await;
    let body = r#"{"model":"something-else","messages":[{"role":"user","content":"hi"}],"stream":false}"#;
    let req = String::from_utf8(post(p.addr(), "/v1/chat/completions?x=1", body))
        .unwrap()
        .replace("Content-Type:", "Authorization: Bearer client-secret\r\nContent-Type:");
    let resp = raw(p.addr(), req.as_bytes()).await;
    assert_eq!(status(&resp), 200, "{resp}");
    let seen = up.last();
    assert_eq!(seen.request_line, "POST /v1/chat/completions HTTP/1.1");
    assert_eq!(seen.header("authorization"), Some(format!("Bearer {}", key()).as_str()));
    assert!(!format!("{seen:?}").contains("client-secret"));
    let sent: serde_json::Value = serde_json::from_slice(&seen.body).unwrap();
    assert_eq!(sent["model"], "grok-test");
    assert_eq!(sent["messages"][0]["content"], "hi");
    assert_eq!(sent["stream"], false);
}

#[tokio::test]
async fn a_hosted_role_streams_chunks_through() {
    let mut r = Reply::ok("");
    r.content_type = "text/event-stream";
    r.chunks = (0..4).map(|i| format!("data: {i}\n\n").into_bytes()).collect();
    r.gap = Duration::from_millis(20);
    let (_up, p) = hosted(KEY_VAR, r).await;
    let resp = raw(p.addr(), &post(p.addr(), "/v1/chat/completions", r#"{"stream":true}"#)).await;
    assert_eq!(status(&resp), 200);
    assert_eq!(body(&resp), "data: 0\n\ndata: 1\n\ndata: 2\n\ndata: 3\n\n");
}

#[tokio::test]
async fn models_is_forwarded_and_other_paths_are_not() {
    let (up, p) = hosted(KEY_VAR, Reply::ok(r#"{"data":[]}"#)).await;
    assert_eq!(status(&raw(p.addr(), &get(p.addr(), "/v1/models")).await), 200);
    assert_eq!(up.last().request_line, "GET /v1/models HTTP/1.1");
    let n = up.count();
    let resp = raw(p.addr(), &post(p.addr(), "/v1/embeddings", "{}")).await;
    assert_eq!(status(&resp), 403, "{resp}");
    assert_eq!(up.count(), n);
}

#[tokio::test]
async fn an_unset_key_variable_fails_the_request_naming_the_variable_only() {
    let var = "WEFTOS_TEST_HOSTED_KEY_NOT_SET";
    let (up, p) = hosted(var, Reply::ok("{}")).await;
    let resp = raw(p.addr(), &post(p.addr(), "/v1/chat/completions", "{}")).await;
    assert_eq!(status(&resp), 502, "{resp}");
    assert!(resp.contains(var), "{resp}");
    assert_eq!(up.count(), 0, "nothing is sent without a key");
}

#[tokio::test]
async fn the_key_value_is_in_no_error_debug_or_audit() {
    // An upstream that is gone: the request fails after the key was read.
    let dead = {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap()
    };
    let audit = Arc::new(Audit::default());
    let t = Arc::new(PlacementTable::new("node-a", None, Some(audit.clone() as Arc<dyn ProxyAudit>)));
    let r = route(&format!("http://{dead}"), KEY_VAR);
    t.register_hosted("hard", r.clone()).unwrap();
    let p = start(&t, small_limits()).await;
    let resp = raw(p.addr(), &post(p.addr(), "/v1/chat/completions", "{}")).await;
    assert_eq!(status(&resp), 502, "{resp}");
    let k = key();
    assert!(!resp.contains(&k));
    let up = super::upstream::Upstream::new(small_limits()).unwrap();
    struct Null;
    #[async_trait::async_trait]
    impl ResponseSink for Null {
        async fn head(&mut self, _: u16, _: Option<&str>) -> Result<(), ProxyError> {
            Ok(())
        }
        async fn chunk(&mut self, _: &[u8]) -> Result<(), ProxyError> {
            Ok(())
        }
    }
    let req = ProxyRequest {
        role: "hard".into(),
        method: Method::Post,
        path: "/v1/chat/completions".into(),
        content_type: None,
        accept: None,
        authorization: None,
        body: b"{}".to_vec(),
    };
    let e = up.forward_hosted(&r, &req, &mut Null, Duration::from_secs(1)).await.unwrap_err();
    assert!(!format!("{e} {e:?}").contains(&k));
    assert!(!format!("{r:?} {:?}", r.bearer().unwrap()).contains(&k), "Debug redacts the key");
    assert!(!format!("{:?}", t.resolve("hard")).contains(&k));
    assert!(!format!("{:?}", audit.0.lock().unwrap()).contains(&k));
}

#[test]
fn base_urls_are_https_or_a_loopback_literal() {
    for ok in ["https://api.x.ai/v1", "https://example.com:8443", "http://127.0.0.1:9/v1", "http://[::1]:9"] {
        assert!(parse_hosted_base(ok).is_ok(), "{ok}");
    }
    for bad in [
        "http://api.x.ai/v1",
        "http://localhost:9",
        "http://10.0.0.1",
        "ftp://x",
        "https://user:pw@api.x.ai/v1",
        "https://api.x.ai/v1?k=v",
        "https://api.x.ai/v1#f",
        "api.x.ai",
        "",
    ] {
        assert!(parse_hosted_base(bad).is_err(), "{bad}");
    }
    assert!(valid_env_name("GROK_API_KEY") && valid_env_name("_k1"));
    assert!(!valid_env_name("") && !valid_env_name("1K") && !valid_env_name("A B") && !valid_env_name("A=b") && !valid_env_name("sk-live-abc"));
}

#[test]
fn a_request_path_cannot_move_the_destination() {
    let r = route("http://127.0.0.1:9", KEY_VAR);
    assert_eq!(r.url_for("/v1/chat/completions?x=1").unwrap().as_str(), "http://127.0.0.1:9/v1/chat/completions");
    assert_eq!(r.url_for("/v1/models").unwrap().as_str(), "http://127.0.0.1:9/v1/models");
    for p in ["//evil.example/v1/chat/completions", "/v1/chat/completions/../x", "/v1/files", "/api/chat", "@evil/v1/models"] {
        assert!(r.url_for(p).is_err(), "{p}");
    }
}

#[test]
fn pinning_rewrites_only_the_model() {
    let out = pin_model(br#"{"model":"a","n":1,"messages":[]}"#, "m").unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!((v["model"].as_str(), v["n"].as_i64()), (Some("m"), Some(1)));
    let v: serde_json::Value = serde_json::from_slice(&pin_model(b"{}", "m").unwrap()).unwrap();
    assert_eq!(v["model"], "m");
    assert!(pin_model(b"", "m").unwrap().is_empty());
    for bad in [&b"not json"[..], b"[1]", b"\"s\"", b"null"] {
        assert!(matches!(pin_model(bad, "m"), Err(ProxyError::BadRequest(_))));
    }
}

#[tokio::test]
async fn a_pinned_local_role_rewrites_the_model_and_refuses_a_non_json_body() {
    let up = fake(Reply::ok("{}")).await;
    let t = table();
    t.register_local("hard", &up.base(), Some("loaded".into()), "openai-v1", "MlxLm").unwrap();
    t.set_role_policy("hard", RolePolicy { stall: None, pin_model: Some("loaded".into()) });
    let p = start(&t, small_limits()).await;
    let resp = raw(p.addr(), &post(p.addr(), "/v1/chat/completions", r#"{"model":"grok-4.6","x":1}"#)).await;
    assert_eq!(status(&resp), 200, "{resp}");
    let sent: serde_json::Value = serde_json::from_slice(&up.last().body).unwrap();
    assert_eq!((sent["model"].as_str(), sent["x"].as_i64()), (Some("loaded"), Some(1)));
    let n = up.count();
    let resp = raw(p.addr(), &post(p.addr(), "/v1/chat/completions", "not json")).await;
    assert_eq!(status(&resp), 400, "{resp}");
    assert_eq!(up.count(), n);
    // Unpinned roles still pass the body through untouched.
    t.set_role_policy("hard", RolePolicy::default());
    let body_in = r#"{"model":"grok-4.6"}"#;
    raw(p.addr(), &post(p.addr(), "/v1/chat/completions", body_in)).await;
    assert_eq!(up.last().body, body_in.as_bytes());
}

#[tokio::test]
async fn a_roles_own_stall_timeout_applies_to_that_role_only() {
    // The fake sends a chunk after 1.5 s; the listener default stall is 0.8 s.
    let mut slow = Reply::ok(r#"{"ok":true}"#);
    slow.gap = Duration::from_millis(1500);
    let up = fake(slow).await;
    let t = table();
    t.register_local("hard", &up.base(), Some("m".into()), "openai-v1", "LlamaCpp").unwrap();
    t.register_local("other", &up.base(), Some("m".into()), "openai-v1", "LlamaCpp").unwrap();
    t.set_role_policy("hard", RolePolicy { stall: Some(Duration::from_secs(5)), pin_model: None });
    let p = start(&t, small_limits()).await;
    let resp = raw(p.addr(), &post(p.addr(), "/v1/chat/completions", "{}")).await;
    assert_eq!(body(&resp), r#"{"ok":true}"#, "{resp}");
    // The role without its own setting keeps the default: cut mid-body.
    let q = match InferProxy::start("other", "127.0.0.1:0".parse().unwrap(), OccupiedPolicy::Refuse, t.clone(), small_limits(), None)
        .await
        .unwrap()
    {
        Started::Running(p) => p,
        Started::Adopted(_) => panic!("adopted"),
    };
    let resp = raw(q.addr(), &post(q.addr(), "/v1/chat/completions", "{}")).await;
    assert_eq!(body(&resp), "", "{resp}");
}

#[test]
fn a_hosted_role_is_never_advertised_or_served_to_a_peer() {
    let t = table();
    t.register_hosted("hard", route("http://127.0.0.1:9", KEY_VAR)).unwrap();
    t.expose_to_mesh("hard", true);
    t.allow_mesh_peer("hard", "node-b", true);
    assert!(t.advertisement("hard", 1).is_none());
    assert!(t.local_for_peer("hard", "node-b").is_none());
    assert!(t.local_base_for_role("hard").is_none());
    assert!(matches!(t.resolve("hard"), Some(Target::Hosted { .. })));
}
