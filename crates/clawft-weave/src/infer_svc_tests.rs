//! Svc-backed roles in the daemon: the config rules, and the proxy's
//! in-process session against a real broker with a stand-in provider of
//! `svc.infer/venq-npu` (no socket, no hardware).

use std::sync::Arc;

use clawft_kernel::infer_proxy::{ProxyError, SvcBackend, SvcImage};
use clawft_types::cog_topics::TopicClass;
use serde_json::{Value, json};
use weftos_cogbus::{Broker, BusHooks, Event, InstanceSpec, OutputSpec};

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
