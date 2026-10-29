//! Svc-backed roles in the daemon: the config rules, and the proxy's
//! in-process session against a real broker with a stand-in provider of
//! `svc.infer/venq-npu` (no socket, no hardware).

use std::sync::Arc;

use clawft_kernel::infer_proxy::{ProxyError, SvcBackend, SvcImage};
use clawft_types::cog_topics::TopicClass;
use serde_json::{Value, json};
use weftos_cogbus::{Broker, BusHooks, BusPolicy, BusRequest, Event, Grant, InstanceSpec, OutputSpec};

use crate::infer_svc::BrokerSvc;
use crate::infer_wire_tests::write_cfg;

const TOPIC: &str = "svc.infer/venq-npu";

#[test]
fn a_svc_role_is_validated_and_resolves_without_a_server() {
    let dir = tempfile::tempdir().unwrap();
    let svc = json!({"topic": TOPIC, "model": "qwen3-vl-4b"});
    write_cfg(dir.path(), json!({"roles": [{"role": "vision", "svc": svc, "proxy_port": 18391}]}));
    let cfg = crate::infer_cfg::load_config(dir.path()).unwrap().unwrap();
    let r = crate::infer_cfg::resolve_roles(&cfg, None).unwrap();
    assert_eq!(r[0].spec.model.as_deref(), Some("qwen3-vl-4b"));
    assert!(!r[0].managed);
    let bad = |role: Value| {
        write_cfg(dir.path(), json!({"roles": [role]}));
        crate::infer_cfg::load_config(dir.path()).unwrap_err()
    };
    bad(json!({"role": "v", "svc": {"topic": "svc.infer/Bad Name", "model": "m"}}));
    bad(json!({"role": "v", "svc": {"topic": "svc.sdr/radio", "model": "m"}}));
    bad(json!({"role": "v", "svc": {"topic": "http://127.0.0.1:9", "model": "m"}}));
    bad(json!({"role": "v", "svc": {"topic": TOPIC, "model": ""}}));
    bad(json!({"role": "v", "svc": {"topic": TOPIC, "model": "m", "extra": 1}}));
    // A svc role owns no server: server fields are refused, not ignored.
    for f in [("flavor", json!("llamacpp")), ("instance_port", json!(9)), ("mode", json!("managed")), ("model", json!("x"))] {
        let mut r = json!({"role": "v", "svc": {"topic": TOPIC, "model": "m"}});
        r[f.0] = f.1;
        bad(r);
    }
}

fn broker() -> (Arc<Broker>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    (crate::cogbus_serve::broker_in("venq", BusHooks::default(), dir.path()), dir)
}

/// A provider of `svc.infer/venq-npu` that answers by describing what it
/// was sent, reading the image blob the way the real NPU cog does.
fn provider(b: &Arc<Broker>) {
    let spec = InstanceSpec {
        instance_id: "npu-infer".into(),
        cog_id: "npu-infer".into(),
        project_id: None,
        uid: None,
        outputs: vec![OutputSpec { topic: "svc.infer".into(), schema: "infer.v1".into(), class: TopicClass::Reading }],
        inputs: vec![],
    };
    let token = "ab".repeat(32);
    b.register(spec, &token).unwrap();
    let mut s = b.authenticate(&token, None).unwrap();
    b.serve_topic(&s, "svc.infer", "venq-npu", "infer.v1").unwrap();
    let mut rx = s.take_pushed().unwrap();
    let b = b.clone();
    tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            let Event::Call(c) = ev else { continue };
            let text = match c.body.get("image_blob") {
                Some(h) => {
                    let hash = h["hash"].as_str().unwrap();
                    let (mut off, mut read) = (0u64, 0usize);
                    loop {
                        let got = b.blob_get(&s, hash, off, None).unwrap();
                        let n = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, got["data"].as_str().unwrap()).unwrap().len();
                        read += n;
                        off += n as u64;
                        if got["eof"] == true || n == 0 {
                            break;
                        }
                    }
                    format!("image {} bytes, read {read}", h["size"])
                }
                None => format!("text {}", c.body["text"]),
            };
            let reply = json!({"text": text, "usage": {"input_tokens": 3, "output_tokens": 1}, "timings": {"total_ms": 5}});
            b.reply_call(&s, c.call, Ok(reply)).unwrap();
        }
    });
}

#[tokio::test]
async fn the_proxy_session_puts_the_image_as_a_blob_and_calls_the_service() {
    let (b, _dir) = broker();
    let svc = BrokerSvc::open(b.clone(), None).unwrap();
    assert!(!b.serves(TOPIC));
    // Nothing serves it yet: unavailable, not a hang.
    let e = svc.call(TOPIC, json!({"model": "m", "text": "hi"}), None, "local:test").await.unwrap_err();
    assert!(matches!(e, ProxyError::NoInstance(_)), "{e}");
    provider(&b);
    assert!(b.serves(TOPIC));
    let r = svc.call(TOPIC, json!({"model": "m", "text": "hi"}), None, "local:test").await.unwrap();
    assert_eq!(r["text"], "text \"hi\"");
    // An image larger than one blob slice (32 KiB) and than a call body (64 KiB).
    let mut png = vec![0x89, b'P', b'N', b'G'];
    png.extend(std::iter::repeat_n(5u8, 100_000));
    let img = SvcImage { media_type: "image/png", bytes: png.clone() };
    let r = svc.call(TOPIC, json!({"model": "m", "text": "what"}), Some(img), "mesh:mac").await.unwrap();
    assert_eq!(r["text"], format!("image {} bytes, read {}", png.len(), png.len()));
    assert_eq!(r["usage"]["input_tokens"], 3);
}

#[tokio::test]
async fn bus_policy_applies_to_the_proxy_like_any_caller() {
    // A provider in another project with no grant: the call is refused by the
    // broker's project rule, and the proxy reports it as a refusal.
    let (b, _dir) = broker();
    let spec = InstanceSpec {
        instance_id: "npu-infer".into(), cog_id: "npu-infer".into(),
        project_id: Some("01J9ZXW0PRJCTAAAAAAAAAAAAA".into()), uid: None,
        outputs: vec![OutputSpec { topic: "svc.infer".into(), schema: "infer.v1".into(), class: TopicClass::Reading }],
        inputs: vec![],
    };
    let token = "cd".repeat(32);
    b.register(spec, &token).unwrap();
    let s = b.authenticate(&token, None).unwrap();
    b.serve_topic(&s, "svc.infer", "venq-npu", "infer.v1").unwrap();
    let svc = BrokerSvc::open(b.clone(), None).unwrap();
    let e = svc.call(TOPIC, json!({"model": "m", "text": "hi"}), None, "local:test").await.unwrap_err();
    assert!(matches!(e, ProxyError::Refused(_)), "{e}");
}

#[tokio::test]
async fn a_failed_open_leaves_nothing_registered_so_a_retry_works() {
    let (b, _dir) = broker();
    let first = BrokerSvc::open(b.clone(), None).unwrap();
    // The instance id is taken while the first session lives.
    assert!(BrokerSvc::open(b.clone(), None).is_err());
    drop(first);
    // Dropping revokes it; opening again works.
    BrokerSvc::open(b.clone(), None).unwrap();
}

#[tokio::test]
async fn a_full_content_store_is_busy_not_a_bad_gateway() {
    let (b, _dir) = broker();
    let svc = BrokerSvc::open(b.clone(), None).unwrap();
    provider(&b);
    // Fill the instance's upload slots: 4 uploads open and never finished.
    let store = b.blob_store().unwrap().clone();
    let filler: Vec<u64> = (0..4)
        .map(|i| store.begin(now_ms(), "infer-proxy", None, "image/png", 10 + i, None).unwrap())
        .collect();
    let mut png = vec![0x89, b'P', b'N', b'G'];
    png.extend([1u8; 64]);
    let img = SvcImage { media_type: "image/png", bytes: png.clone() };
    let e = svc.call(TOPIC, json!({"model": "m", "text": "x"}), Some(img.clone()), "local:a").await.unwrap_err();
    assert!(matches!(e, ProxyError::Busy(_)), "{e}");
    assert_eq!(e.status(), 429);
    // The failed put aborted only its own upload (none began): the slots stay
    // taken until they are freed, then the next call works.
    let e = svc.call(TOPIC, json!({"model": "m", "text": "x"}), Some(img.clone()), "local:a").await.unwrap_err();
    assert!(matches!(e, ProxyError::Busy(_)), "{e}");
    for id in filler {
        store.abort_upload("infer-proxy", id);
    }
    let r = svc.call(TOPIC, json!({"model": "m", "text": "x"}), Some(img), "local:a").await.unwrap();
    assert_eq!(r["text"], format!("image {} bytes, read {}", png.len(), png.len()));
}

#[tokio::test]
async fn one_caller_has_at_most_two_image_calls_in_flight() {
    let (b, _dir) = broker();
    let svc = BrokerSvc::open(b.clone(), None).unwrap();
    // Hold two slots the way two in-flight calls would.
    let s1 = svc.image_slot("mesh:n1").unwrap();
    let _s2 = svc.image_slot("mesh:n1").unwrap();
    assert!(matches!(svc.image_slot("mesh:n1"), Err(ProxyError::Busy(_))));
    // Another caller is unaffected, and a released slot frees room.
    assert!(svc.image_slot("local:x").is_ok());
    drop(s1);
    assert!(svc.image_slot("mesh:n1").is_ok());
}

fn png(extra: usize) -> SvcImage {
    let mut bytes = vec![0x89, b'P', b'N', b'G'];
    bytes.extend(std::iter::repeat_n(3u8, extra));
    SvcImage { media_type: "image/png", bytes }
}

/// Refuses every request whose governed agent is `bad`.
struct Bar(&'static str);

impl BusPolicy for Bar {
    fn decide(&self, r: &BusRequest<'_>) -> Result<(), String> {
        if r.instance_id == self.0 { Err(format!("{} is barred", r.instance_id)) } else { Ok(()) }
    }
    fn grants(&self, _: &BusRequest<'_>) -> Vec<Grant> {
        Vec::new()
    }
}

#[tokio::test]
async fn governance_refuses_the_real_caller_not_only_the_proxy() {
    let dir = tempfile::tempdir().unwrap();
    let hooks = BusHooks { policy: Arc::new(Bar("mesh:bad-node")), ..BusHooks::default() };
    let b = crate::cogbus_serve::broker_in("venq", hooks, dir.path());
    let svc = BrokerSvc::open(b.clone(), None).unwrap();
    provider(&b);
    let body = || json!({"model": "m", "text": "x"});
    // A text call and an image call (the blob put) by the barred node are
    // refused; the image never reaches the store.
    let e = svc.call(TOPIC, body(), None, "mesh:bad-node").await.unwrap_err();
    assert!(matches!(e, ProxyError::Refused(_)), "{e}");
    let e = svc.call(TOPIC, body(), Some(png(64)), "mesh:bad-node").await.unwrap_err();
    assert!(matches!(e, ProxyError::Refused(_)), "{e}");
    assert_eq!(b.blob_store().unwrap().stats().blobs, 0);
    // Another node, and a local client, still get through.
    assert!(svc.call(TOPIC, body(), None, "mesh:good-node").await.is_ok());
    assert!(svc.call(TOPIC, body(), Some(png(64)), "mesh:good-node").await.is_ok());
    assert!(svc.call(TOPIC, body(), None, "local:127.0.0.1:5").await.is_ok());
}

#[tokio::test]
async fn a_call_leaves_no_image_blob_behind() {
    let (b, _dir) = broker();
    let svc = BrokerSvc::open(b.clone(), None).unwrap();
    provider(&b);
    let store = b.blob_store().unwrap().clone();
    for i in 0..5 {
        let r = svc.call(TOPIC, json!({"model": "m", "text": "x"}), Some(png(1000 + i)), "local:a").await.unwrap();
        assert!(r["text"].as_str().unwrap().starts_with("image "), "{r}");
        assert_eq!(store.stats().bytes, 0, "after call {i}");
    }
    assert_eq!(store.stats().blobs, 0);
    // A failed call (nothing serves a topic) frees it too.
    let e = svc.call("svc.infer/nowhere", json!({"model": "m"}), Some(png(500)), "local:a").await.unwrap_err();
    assert!(matches!(e, ProxyError::NoInstance(_)), "{e}");
    assert_eq!(store.stats().bytes, 0);
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64
}

#[tokio::test]
async fn one_callers_failed_put_leaves_another_callers_open_upload_alone() {
    let dir = tempfile::tempdir().unwrap();
    let hooks = BusHooks { policy: Arc::new(Bar("mesh:bad-node")), ..BusHooks::default() };
    let b = crate::cogbus_serve::broker_in("venq", hooks, dir.path());
    let svc = BrokerSvc::open(b.clone(), None).unwrap();
    provider(&b);
    let store = b.blob_store().unwrap().clone();
    // Caller A is mid-upload under the shared proxy owner.
    let a_up = store.begin(now_ms(), "infer-proxy", None, "image/png", 10, None).unwrap();
    assert!(store.chunk(now_ms(), "infer-proxy", a_up, 0, &[1u8; 4], false).unwrap().is_none());
    // Caller B is denied at the put.
    let e = svc.call(TOPIC, json!({"model": "m", "text": "x"}), Some(png(64)), "mesh:bad-node").await.unwrap_err();
    assert!(matches!(e, ProxyError::Refused(_)), "{e}");
    // A's upload is still open and completes.
    let h = store.chunk(now_ms(), "infer-proxy", a_up, 4, &[2u8; 6], true).expect("A's upload survived B's failure");
    assert!(h.is_some());
}

#[tokio::test]
async fn a_failed_chunk_aborts_only_its_own_upload() {
    let (b, _dir) = broker();
    let svc = BrokerSvc::open(b.clone(), None).unwrap();
    provider(&b);
    let store = b.blob_store().unwrap().clone();
    let a_up = store.begin(now_ms(), "infer-proxy", None, "image/png", 10, None).unwrap();
    // Fill the other upload slots so B's begin is over quota.
    let mut filler = Vec::new();
    while let Ok(id) = store.begin(now_ms(), "infer-proxy", None, "image/png", 10, None) {
        filler.push(id);
    }
    let e = svc.call(TOPIC, json!({"model": "m", "text": "x"}), Some(png(64)), "local:b").await.unwrap_err();
    assert!(matches!(e, ProxyError::Busy(_)), "{e}");
    store.chunk(now_ms(), "infer-proxy", a_up, 0, &[3u8; 10], true).expect("A's upload survived B's quota failure");
}

fn deny_file(dir: &std::path::Path, agent: &str) {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join("cogbus-policy.json");
    std::fs::write(&p, json!({"deny": [{"agents": [agent], "reason": "barred"}]}).to_string()).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
}

#[tokio::test]
async fn a_policy_deny_naming_the_carrier_or_the_caller_refuses_neither_allows() {
    let body = || json!({"model": "m", "text": "x"});
    for (denied, expect_ok_for) in [("infer-proxy", None), ("mesh:bad-node", Some("mesh:good-node")), ("nobody", Some("mesh:bad-node"))] {
        let dir = tempfile::tempdir().unwrap();
        deny_file(dir.path(), denied);
        let b = crate::cogbus_serve::broker_in("venq", crate::cogbus_serve::hooks(dir.path(), None), dir.path());
        let svc = BrokerSvc::open(b.clone(), None).unwrap();
        provider(&b);
        match expect_ok_for {
            // The carrier is named: every call and put is refused.
            None => {
                for who in ["mesh:good-node", "local:127.0.0.1"] {
                    let e = svc.call(TOPIC, body(), None, who).await.unwrap_err();
                    assert!(matches!(e, ProxyError::Refused(_)), "{denied} {who}: {e}");
                    let e = svc.call(TOPIC, body(), Some(png(64)), who).await.unwrap_err();
                    assert!(matches!(e, ProxyError::Refused(_)), "{denied} {who}: {e}");
                }
                assert_eq!(b.blob_store().unwrap().stats().reserved, 0);
            }
            Some(ok) => {
                assert!(svc.call(TOPIC, body(), Some(png(64)), ok).await.is_ok(), "{denied}: {ok} allowed");
                if denied != "nobody" {
                    let e = svc.call(TOPIC, body(), None, denied).await.unwrap_err();
                    assert!(matches!(e, ProxyError::Refused(_)), "{denied}: {e}");
                }
            }
        }
    }
}
