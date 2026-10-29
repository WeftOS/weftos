//! Mesh serving policy: model pinning, path narrowing, generic errors,
//! concurrency, and forged statuses.

use std::sync::Arc;
use std::time::Duration;

use super::listener::{InferProxy, OccupiedPolicy, Started};
use super::mesh_forward::{InferPeer, ServeGate, Served, serve_infer};
use super::support::*;
use super::table::{MeshLocal, PlacementTable};
use super::tests_mesh::*;
use super::types::*;
use super::upstream::Upstream;
use super::wire::{self, Resp};
use crate::mesh_framing::{FrameType, MeshFrame, read_frame, write_frame};
use crate::mesh_test_support::connected_pair;

fn json_post(addr: std::net::SocketAddr, path: &str, v: serde_json::Value) -> Vec<u8> {
    post(addr, path, &v.to_string())
}

#[tokio::test]
async fn the_serving_side_pins_model_and_strips_keep_alive() {
    let c = cluster(Reply::ok("{}")).await;
    advertise(&c);
    let p = proxy_a(&c).await;
    let body = serde_json::json!({
        "model": "someone/elses-70b", "keep_alive": -1, "messages": [], "stream": false
    });
    let r = raw(p.addr(), &json_post(p.addr(), "/v1/chat/completions", body)).await;
    assert_eq!(status(&r), 200, "{r}");
    let sent: serde_json::Value = serde_json::from_slice(&c.up_b.last().body).unwrap();
    assert_eq!(sent["model"], "m");
    assert!(sent.get("keep_alive").is_none());
    assert_eq!(sent["stream"], false, "other fields pass through");
    // A non-JSON-object body is refused before the model server sees it.
    let n = c.up_b.count();
    let r = raw(p.addr(), &post(p.addr(), "/v1/chat/completions", "[1,2]")).await;
    assert_eq!(status(&r), 502, "{r}");
    assert_eq!(c.up_b.count(), n);
}

#[test]
fn pin_body_rules() {
    use super::mesh_policy::pin_body;
    let l = |model: Option<&str>, rt: &str| MeshLocal {
        base: "http://127.0.0.1:1".into(),
        model: model.map(String::from),
        runtime: rt.into(),
        svc: None,
    };
    let v = |b: Vec<u8>| serde_json::from_slice::<serde_json::Value>(&b).unwrap();
    let chat = "/v1/chat/completions";
    let body = br#"{"model":"x","keep_alive":"1h","messages":[],"temperature":0.2}"#;
    let want = |m: Option<&str>| {
        let mut o = serde_json::json!({"messages": [], "temperature": 0.2});
        if let Some(m) = m {
            o["model"] = m.into();
        }
        o
    };
    assert_eq!(v(pin_body(chat, body, &l(Some("m"), "LlamaCpp")).unwrap()), want(Some("m")));
    // No pinned model: the field is dropped, not passed through.
    assert_eq!(v(pin_body(chat, body, &l(None, "LlamaCpp")).unwrap()), want(None));
    // mlx_lm.server fetches whatever repo `model` names: use its placeholder.
    assert_eq!(v(pin_body(chat, body, &l(Some("m"), "MlxLm")).unwrap()), want(Some("default_model")));
    assert!(pin_body(chat, b"not json", &l(None, "Ollama")).is_err());
    assert!(pin_body(chat, b"[1]", &l(None, "Ollama")).is_err());
    assert!(pin_body(chat, b"", &l(None, "Ollama")).unwrap().is_empty());
}

#[test]
fn pin_body_keeps_only_allowlisted_keys_per_path() {
    use super::mesh_policy::pin_body;
    let local = MeshLocal { base: "http://127.0.0.1:1".into(), model: Some("m".into()), runtime: "MlxLm".into(), svc: None };
    let keys = |path: &str, body: serde_json::Value| -> std::collections::BTreeSet<String> {
        let out = pin_body(path, body.to_string().as_bytes(), &local).unwrap();
        serde_json::from_slice::<serde_json::Value>(&out)
            .unwrap()
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect()
    };
    let hostile = serde_json::json!({
        "adapters": "/tmp/x", "draft_model": "evil/draft", "num_draft_tokens": 9,
        "keep_alive": -1, "options": {"num_ctx": 1}, "grammar": "root ::= x",
        "cache_prompt": true, "chat_template_kwargs": {}, "role_mapping": {},
        "id_slot": 3, "slot_id": 3, "totally_unknown": 1,
    });
    let mut chat = hostile.clone();
    for (k, v) in [("messages", serde_json::json!([])), ("max_tokens", 5.into()), ("stream", true.into()),
                   ("tools", serde_json::json!([])), ("tool_choice", "auto".into()), ("seed", 1.into()),
                   ("logit_bias", serde_json::json!({})), ("response_format", serde_json::json!({}))] {
        chat[k] = v;
    }
    let got = keys("/v1/chat/completions", chat);
    let want: std::collections::BTreeSet<String> =
        ["messages", "max_tokens", "stream", "tools", "tool_choice", "seed", "logit_bias", "response_format", "model"]
            .iter().map(|s| s.to_string()).collect();
    assert_eq!(got, want, "everything outside the allowlist is dropped");
    // The per-path split: embeddings and completions keep their own keys only.
    let mut emb = hostile.clone();
    emb["input"] = "x".into();
    emb["messages"] = serde_json::json!([]);
    emb["encoding_format"] = "float".into();
    assert_eq!(keys("/v1/embeddings", emb), ["input", "encoding_format", "model"].iter().map(|s| s.to_string()).collect());
    let mut comp = hostile;
    comp["prompt"] = "x".into();
    comp["tools"] = serde_json::json!([]);
    assert_eq!(keys("/v1/completions", comp), ["prompt", "model"].iter().map(|s| s.to_string()).collect());
}

#[tokio::test]
async fn hostile_body_keys_never_reach_the_model_server() {
    let c = cluster(Reply::ok("{}")).await;
    advertise(&c);
    let p = proxy_a(&c).await;
    let body = serde_json::json!({
        "messages": [], "adapters": "/x", "draft_model": "evil", "keep_alive": -1, "zzz": 1
    });
    let r = raw(p.addr(), &json_post(p.addr(), "/v1/chat/completions", body)).await;
    assert_eq!(status(&r), 200, "{r}");
    let sent: serde_json::Value = serde_json::from_slice(&c.up_b.last().body).unwrap();
    for k in ["adapters", "draft_model", "keep_alive", "zzz"] {
        assert!(sent.get(k).is_none(), "{k} reached the server");
    }
    assert_eq!(sent["model"], "m");
}

#[tokio::test]
async fn the_serving_side_requires_an_enforced_node_grant_and_an_allowlist() {
    use crate::mesh_admit::{Grant, PeerClass, Refusal};
    let up = fake(Reply::ok("{}")).await;
    let t = Arc::new(PlacementTable::new("node-b", None, None));
    t.register_local("hermes", &up.base(), None, "openai-v1", "LlamaCpp").unwrap();
    t.expose_to_mesh("hermes", true);
    t.allow_mesh_peer("hermes", "node-a", true);
    let base = enforced_node();
    let cases: Vec<(&str, Option<Grant>, &str)> = vec![
        ("no grant", None, "node-a"),
        ("leaf", Some(Grant { class: PeerClass::Leaf, ..base.clone() }), "node-a"),
        ("legacy", Some(Grant { class: PeerClass::Legacy, ..base.clone() }), "node-a"),
        ("observe", Some(Grant { admitted: false, ..base.clone() }), "node-a"),
        ("untrusted scope", Some(Grant { trust_scope: false, ..base.clone() }), "node-a"),
        ("observed refusal", Some(Grant { observed: Some(Refusal { code: "x", detail: String::new() }), ..base.clone() }), "node-a"),
        ("not allowlisted", Some(base.clone()), "node-z"),
    ];
    for (name, grant, id) in cases {
        let (mut client, mut server) = connected_pair().await.unwrap();
        let u = Upstream::new(small_limits()).unwrap();
        let tt = t.clone();
        let peer = InferPeer { node_id: id.into(), grant };
        let h = tokio::spawn(async move { serve_infer(&mut server, &peer, tt.as_ref(), &u, &ServeGate::default(), None).await.unwrap() });
        let req = ProxyRequest {
            role: "hermes".into(), method: Method::Get, path: "/v1/models".into(),
            content_type: None, accept: None, authorization: None, body: vec![],
        };
        write_frame(&mut client, &MeshFrame { frame_type: FrameType::InferRequest, payload: wire::encode_request(&req).unwrap() }).await.unwrap();
        assert!(matches!(h.await.unwrap(), Served::Failed(_)), "{name}");
        assert_eq!(up.count(), 0, "{name}: reached the model server");
    }
    // The qualifying, allowlisted peer is served; withdrawing the allowance stops it.
    assert!(t.local_for_peer("hermes", "node-a").is_some());
    t.allow_mesh_peer("hermes", "node-a", false);
    assert!(t.local_for_peer("hermes", "node-a").is_none());
}

#[tokio::test]
async fn permits_are_taken_before_a_frame_is_read() {
    // A peer at its limit is refused without this node reading (and so
    // buffering) its frame.
    let gate = ServeGate::new(1, 8);
    assert!(gate.acquire_for_test("p"));
    let (mut client, mut server) = connected_pair().await.unwrap();
    let u = Upstream::new(small_limits()).unwrap();
    let f = |_: &str, _: &str| None::<MeshLocal>;
    let peer = trusted_peer("p");
    // Nothing was sent by the client: serve_infer must answer anyway.
    let r = tokio::time::timeout(Duration::from_millis(300), serve_infer(&mut server, &peer, &f, &u, &gate, None))
        .await
        .expect("must not wait for a frame")
        .unwrap();
    assert!(matches!(r, Served::Failed(ref m) if m == "refused"), "{r:?}");
    let reply = read_frame(&mut client).await.unwrap();
    assert!(matches!(wire::decode_resp(&reply.payload).unwrap(), Resp::Error(_)));
}

#[tokio::test]
async fn peers_get_only_the_inference_paths() {
    let c = cluster(Reply::ok("{}")).await;
    advertise(&c);
    let p = proxy_a(&c).await;
    for path in ["/api/ps", "/api/show", "/api/tags", "/health", "/api/chat"] {
        let r = raw(p.addr(), &get(p.addr(), path)).await;
        assert_eq!(status(&r), 502, "{path}: {r}");
    }
    assert_eq!(c.up_b.count(), 0, "a peer reached a non-inference endpoint");
    for path in ["/v1/models", "/v1/embeddings", "/v1/completions", "/v1/chat/completions"] {
        let r = raw(p.addr(), &post(p.addr(), path, "{}")).await;
        assert_eq!(status(&r), 200, "{path}: {r}");
    }
}

#[tokio::test]
async fn peers_are_told_a_generic_reason_only() {
    let audit = Arc::new(Audit::default());
    let up = fake(Reply::ok("{}")).await;
    let dead = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let t = Arc::new(PlacementTable::new("node-b", None, None));
    t.register_local("hermes", &format!("http://127.0.0.1:{dead}"), None, "openai-v1", "LlamaCpp").unwrap();
    t.expose_to_mesh("hermes", true);
    t.allow_mesh_peer("hermes", "node-a", true);
    drop(up);
    let (mut client, mut server) = connected_pair().await.unwrap();
    let u = Upstream::new(small_limits()).unwrap();
    let tt = t.clone();
    let f = move |r: &str, p: &str| tt.local_for_peer(r, p);
    let peer = trusted_peer("node-a");
    let a2 = audit.clone();
    let h = tokio::spawn(async move { serve_infer(&mut server, &peer, &f, &u, &ServeGate::default(), Some(a2.as_ref())).await });
    let req = ProxyRequest {
        role: "hermes".into(), method: Method::Get, path: "/v1/models".into(),
        content_type: None, accept: None, authorization: None, body: vec![],
    };
    write_frame(&mut client, &MeshFrame { frame_type: FrameType::InferRequest, payload: wire::encode_request(&req).unwrap() }).await.unwrap();
    let f = read_frame(&mut client).await.unwrap();
    let Resp::Error(m) = wire::decode_resp(&f.payload).unwrap() else { panic!() };
    assert_eq!(m, "upstream error");
    assert!(!m.contains(&dead.to_string()) && !m.contains("127.0.0.1"));
    let Served::Failed(w) = h.await.unwrap().unwrap() else { panic!() };
    assert_eq!(w, "upstream error");
    // The detail stays in the local audit trail.
    let detail = audit.0.lock().unwrap().iter().map(|(_, p)| p.to_string()).collect::<String>();
    assert!(detail.contains("upstream"), "{detail}");
}

#[tokio::test]
async fn the_serving_side_bounds_concurrency_per_peer() {
    let mut r = Reply::ok("slow");
    r.hold = Duration::from_millis(500);
    let up = fake(r).await;
    let t = Arc::new(PlacementTable::new("node-b", None, None));
    t.register_local("hermes", &up.base(), None, "openai-v1", "LlamaCpp").unwrap();
    t.expose_to_mesh("hermes", true);
    t.allow_mesh_peer("hermes", "p1", true);
    t.allow_mesh_peer("hermes", "p2", true);
    let gate = Arc::new(ServeGate::new(1, 8));
    let one = |peer: &'static str| {
        let (t, gate) = (t.clone(), gate.clone());
        async move {
            let (mut client, mut server) = connected_pair().await.unwrap();
            let u = Upstream::new(small_limits()).unwrap();
            let f = move |r: &str, p: &str| t.local_for_peer(r, p);
            let p = trusted_peer(peer);
            let h = tokio::spawn(async move { serve_infer(&mut server, &p, &f, &u, &gate, None).await.unwrap() });
            let req = ProxyRequest {
                role: "hermes".into(), method: Method::Get, path: "/v1/models".into(),
                content_type: None, accept: None, authorization: None, body: vec![],
            };
            write_frame(&mut client, &MeshFrame { frame_type: FrameType::InferRequest, payload: wire::encode_request(&req).unwrap() }).await.unwrap();
            (h, client)
        }
    };
    let (first, _c1) = one("p1").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (second, _c2) = one("p1").await;
    let (other, _c3) = one("p2").await;
    let s2 = second.await.unwrap();
    assert!(matches!(&s2, Served::Failed(m) if m == "refused"), "same peer is over its limit: {s2:?}");
    assert_eq!(first.await.unwrap(), Served::Ok);
    assert_eq!(other.await.unwrap(), Served::Ok, "another peer is unaffected");
    // Total cap.
    let tiny = Arc::new(ServeGate::new(8, 1));
    let a = tiny.acquire_for_test("x");
    assert!(a);
    assert!(!tiny.acquire_for_test("y"), "node-wide cap");
}

#[tokio::test]
async fn a_peer_cannot_inject_a_forged_response_with_a_1xx_status() {
    let up = fake(Reply::ok("{}")).await;
    let _ = up;
    let peer_table = Arc::new(PlacementTable::new("peer", None, None));
    peer_table.register_local("hermes", "http://127.0.0.1:9", None, "openai-v1", "LlamaCpp").unwrap();
    peer_table.expose_to_mesh("hermes", true);
    let forged = b"HTTP/1.1 200 OK\r\nSet-Cookie: x=1\r\n\r\nforged".to_vec();
    for status in [100u16, 101, 199] {
        let frames = vec![
            {
                let mut h = vec![0u8];
                h.extend(format!("{{\"status\":{status}}}").into_bytes());
                h
            },
            wire::encode_resp(&Resp::Chunk(forged.clone())),
            end(),
        ];
        let dialer: Arc<dyn MeshDialer> = Arc::new(Script(frames, false));
        let a = Arc::new(PlacementTable::new("node-a", Some(dialer), None));
        a.allow_remote_node("hermes", "peer", true);
        assert!(a.ingest_advertisement("peer", &peer_table.advertisement("hermes", 1).unwrap()));
        let p = match InferProxy::start("hermes", "127.0.0.1:0".parse().unwrap(), OccupiedPolicy::Refuse, a, small_limits(), None).await.unwrap() {
            Started::Running(p) => p,
            _ => panic!(),
        };
        let r = raw(p.addr(), &get(p.addr(), "/v1/models")).await;
        assert_eq!(status_of(&r), 502, "{status}: {r}");
        assert!(!r.contains("forged") && !r.contains("Set-Cookie"), "{r}");
    }
}

fn status_of(r: &str) -> u16 {
    status(r)
}

#[tokio::test]
async fn a_local_server_answering_1xx_is_a_502() {
    let mut r = Reply::ok("forged");
    r.status = 101;
    let up = fake(r).await;
    let t = Arc::new(PlacementTable::new("node-a", None, None));
    t.register_local("hermes", &up.base(), None, "openai-v1", "LlamaCpp").unwrap();
    let p = match InferProxy::start("hermes", "127.0.0.1:0".parse().unwrap(), OccupiedPolicy::Refuse, t, small_limits(), None).await.unwrap() {
        Started::Running(p) => p,
        _ => panic!(),
    };
    let r = raw(p.addr(), &get(p.addr(), "/v1/models")).await;
    assert_eq!(status(&r), 502, "{r}");
    assert!(!r.contains("forged"));
}

#[tokio::test]
async fn a_peer_that_does_not_qualify_holds_no_slot_and_costs_no_read() {
    use crate::mesh_admit::{Grant, PeerClass};
    let t = Arc::new(PlacementTable::new("node-b", None, None));
    t.allow_mesh_peer("hermes", "listed", true);
    let gate = ServeGate::new(1, 1);
    let u = Upstream::new(small_limits()).unwrap();
    let peers = [
        InferPeer { node_id: "listed".into(), grant: Some(Grant { class: PeerClass::Leaf, ..enforced_node() }) },
        InferPeer { node_id: "listed".into(), grant: None },
        InferPeer { node_id: "stranger".into(), grant: Some(enforced_node()) },
    ];
    for peer in peers {
        let (mut client, mut server) = connected_pair().await.unwrap();
        // Nothing is sent: a qualifying peer would wait for a frame holding
        // the only slot; a refused one must answer at once.
        let r = tokio::time::timeout(
            Duration::from_millis(300),
            serve_infer(&mut server, &peer, t.as_ref(), &u, &gate, None),
        )
        .await
        .expect("a non-qualifying peer must not be waited on")
        .unwrap();
        assert!(matches!(r, Served::Failed(ref m) if m == "refused"), "{r:?}");
        let reply = read_frame(&mut client).await.unwrap();
        assert!(matches!(wire::decode_resp(&reply.payload).unwrap(), Resp::Error(_)));
        assert!(gate.acquire_for_test_and_release("listed"), "a slot was held");
    }
}

#[tokio::test]
async fn the_first_frame_has_the_short_head_timeout() {
    let t = Arc::new(PlacementTable::new("node-b", None, None));
    t.allow_mesh_peer("hermes", "p", true);
    let limits = ProxyLimits { head_timeout: Duration::from_millis(150), stall_timeout: Duration::from_secs(30), ..small_limits() };
    let u = Upstream::new(limits).unwrap();
    let (_client, mut server) = connected_pair().await.unwrap();
    let t0 = std::time::Instant::now();
    let r = serve_infer(&mut server, &trusted_peer("p"), t.as_ref(), &u, &ServeGate::default(), None).await.unwrap();
    assert!(matches!(r, Served::Failed(_)));
    assert!(t0.elapsed() < Duration::from_secs(5), "waited {:?}", t0.elapsed());
}
