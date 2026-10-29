//! Calls and blob puts made by an in-process session on behalf of a caller
//! (ADR-101 svc roles): governance sees the caller, the wire cannot say so,
//! and a put image can be released.

use std::sync::{Arc, Mutex};

use clawft_types::cog_topics::{InputPolicy, TopicClass};
use serde_json::json;

use crate::broker::{Broker, BrokerConfig, InstanceSpec, Session};
use crate::policy::{BusAction, BusActivity, BusHooks, BusPolicy, BusRequest, BusSink, Grant, RefusalLimits};
use crate::proto::{ErrorCode, InputSpec, OutputSpec, Request};

/// Denies every request whose agent is `bad`; remembers `(agent, via)`.
struct ByAgent {
    bad: &'static str,
    asked: Mutex<Vec<(BusAction, String, Option<String>)>>,
}

impl BusPolicy for ByAgent {
    fn decide(&self, r: &BusRequest<'_>) -> Result<(), String> {
        self.asked.lock().unwrap().push((r.action, r.instance_id.into(), r.via.map(String::from)));
        if r.instance_id == self.bad { Err(format!("{} is barred", r.instance_id)) } else { Ok(()) }
    }
    fn grants(&self, _: &BusRequest<'_>) -> Vec<Grant> {
        Vec::new()
    }
}

#[derive(Default)]
struct Log(Mutex<Vec<BusActivity>>);

impl BusSink for Log {
    fn record(&self, a: &BusActivity) {
        self.0.lock().unwrap().push(a.clone());
    }
}

fn token(n: u8) -> String {
    format!("{n:02x}").repeat(32)
}

struct Rig {
    broker: Arc<Broker>,
    policy: Arc<ByAgent>,
    log: Arc<Log>,
    proxy: Session,
    _provider: Session,
    _dir: tempfile::TempDir,
}

fn rig() -> Rig {
    rig_with("mesh:bad")
}

fn rig_with(bad: &'static str) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let policy = Arc::new(ByAgent { bad, asked: Mutex::default() });
    let log = Arc::new(Log::default());
    let hooks = BusHooks { policy: policy.clone(), sink: log.clone(), limits: RefusalLimits::default() };
    let blobs = Arc::new(crate::blob::BlobStore::open(&dir.path().join("blobs"), Default::default(), 0).unwrap());
    let broker = Arc::new(Broker::new(BrokerConfig::new("n")).with_hooks(hooks).with_blobs(blobs));
    let svc_out = OutputSpec { topic: "svc.infer".into(), schema: "infer.v1".into(), class: TopicClass::Reading };
    let provider = InstanceSpec {
        instance_id: "npu".into(), cog_id: "npu".into(), project_id: None, uid: None,
        outputs: vec![svc_out], inputs: vec![],
    };
    broker.register(provider, &token(1)).unwrap();
    let p = broker.authenticate(&token(1), None).unwrap();
    broker.serve_topic(&p, "svc.infer", "venq", "infer.v1").unwrap();
    let proxy = InstanceSpec {
        instance_id: "infer-proxy".into(), cog_id: "proxy".into(), project_id: None, uid: None,
        outputs: vec![OutputSpec { topic: "infer.image".into(), schema: "blob.v1".into(), class: TopicClass::Blob }],
        inputs: vec![InputSpec { topic: "svc.infer".into(), schema: "infer.v1".into(), policy: InputPolicy::default() }],
    };
    broker.register(proxy, &token(2)).unwrap();
    let proxy = broker.authenticate(&token(2), None).unwrap();
    Rig { broker, policy, log, proxy, _provider: p, _dir: dir }
}

#[test]
fn governance_is_asked_about_the_caller_a_session_acts_for() {
    let r = rig();
    let call = |id, who| r.broker.call_service_for(&r.proxy, id, "svc.infer/venq", "infer.v1", json!({}), Some(5000), who);
    let e = call(1, "mesh:bad").unwrap_err();
    assert_eq!(e.code, ErrorCode::Denied);
    assert!(e.message.contains("mesh:bad"), "{e}");
    call(2, "mesh:good").unwrap();
    // The same for an image put.
    let put = |who| r.broker.blob_begin_for(&r.proxy, "image/png", 10, Some(60_000), who);
    assert_eq!(put("mesh:bad").unwrap_err().code, ErrorCode::Denied);
    put("local:127.0.0.1:9").unwrap();
    // Governance saw the caller as the agent and the proxy as `via`.
    let asked = r.policy.asked.lock().unwrap().clone();
    assert!(asked.contains(&(BusAction::Call, "mesh:bad".into(), Some("infer-proxy".into()))), "{asked:?}");
    assert!(asked.contains(&(BusAction::BlobPut, "local:127.0.0.1:9".into(), Some("infer-proxy".into()))), "{asked:?}");
    // The activity record of an allowed call names the caller, via the proxy.
    let rows = r.log.0.lock().unwrap().clone();
    let row = rows.iter().find(|a| a.action == "cogbus.call").expect("call recorded");
    assert_eq!((row.instance.as_str(), row.via.as_deref()), ("mesh:good", Some("infer-proxy")));
    // A plain call is still the instance's own, with no `via`.
    r.broker.call_service(&r.proxy, 3, "svc.infer/venq", "infer.v1", json!({}), Some(5000)).unwrap();
    let asked = r.policy.asked.lock().unwrap().clone();
    assert!(asked.contains(&(BusAction::Call, "infer-proxy".into(), None)));
}

#[test]
fn a_caller_name_must_look_like_one() {
    let r = rig();
    for bad in ["", "root", "mesh: x", "mesh:a\nb", "infer-proxy"] {
        let e = r.broker.call_service_for(&r.proxy, 1, "svc.infer/venq", "infer.v1", json!({}), None, bad);
        assert_eq!(e.unwrap_err().code, ErrorCode::BadRequest, "{bad:?}");
    }
}

#[test]
fn a_wire_request_cannot_set_on_behalf() {
    // Requests refuse unknown fields, and there is no field to name a caller.
    for f in [
        r#"{"op":"call","id":10,"topic":"svc.infer/venq","schema":"infer.v1","body":{},"on_behalf":"mesh:good"}"#,
        r#"{"op":"blob_begin","id":11,"media_type":"image/png","size":10,"on_behalf":"mesh:good"}"#,
        r#"{"op":"call","id":10,"topic":"svc.infer/venq","schema":"infer.v1","body":{},"via":"x"}"#,
    ] {
        assert!(serde_json::from_str::<Request>(f).is_err(), "accepted {f}");
    }
    // Without the field the same frames parse, so the refusal is the field.
    serde_json::from_str::<Request>(r#"{"op":"call","id":10,"topic":"svc.infer/venq","schema":"infer.v1","body":{}}"#).unwrap();
    serde_json::from_str::<Request>(r#"{"op":"blob_begin","id":11,"media_type":"image/png","size":10}"#).unwrap();
}

fn put(r: &Rig, who: &str, bytes: &[u8]) -> String {
    use base64::Engine;
    let b = &r.broker;
    let up = b.blob_begin_for(&r.proxy, "image/png", bytes.len() as u64, Some(60_000), who).unwrap()["upload"].as_u64().unwrap();
    let data = base64::engine::general_purpose::STANDARD.encode(bytes);
    let h = b.blob_chunk(&r.proxy, up, 0, &data, true).unwrap();
    h["handle"]["hash"].as_str().unwrap().to_string()
}

#[test]
fn releasing_a_blob_frees_its_bytes_and_its_quota() {
    let r = rig();
    let hash = put(&r, "local:a", &[7u8; 100]);
    let store = r.broker.blob_store().unwrap();
    assert_eq!(store.stats().bytes, 100);
    assert!(r.broker.blob_release(&r.proxy, &hash).unwrap(), "the bytes went");
    assert_eq!(store.stats().bytes, 0);
    assert_eq!(store.stats().blobs, 0);
    // Gone: a second release finds nothing.
    assert_eq!(r.broker.blob_release(&r.proxy, &hash).unwrap_err().code, ErrorCode::NotFound);
}

#[test]
fn only_a_putter_may_release_a_blob() {
    let r = rig();
    let hash = put(&r, "local:a", &[7u8; 100]);
    // The provider can read what it is handed, but did not put it.
    let other = r.broker.authenticate(&token(1), None).unwrap();
    let e = r.broker.blob_release(&other, &hash).unwrap_err();
    assert_eq!(e.code, ErrorCode::CrossProject);
    assert_eq!(r.broker.blob_store().unwrap().stats().bytes, 100, "still there");
}

#[test]
fn bytes_put_twice_survive_one_release() {
    let r = rig();
    let a = put(&r, "local:a", &[9u8; 64]);
    let b = put(&r, "local:b", &[9u8; 64]);
    assert_eq!(a, b, "one stored copy");
    let store = r.broker.blob_store().unwrap();
    assert_eq!(store.stats().blobs, 1);
    assert!(!r.broker.blob_release(&r.proxy, &a).unwrap(), "another call still holds the bytes");
    assert_eq!(store.stats().bytes, 64);
    assert!(r.broker.blob_release(&r.proxy, &a).unwrap());
    assert_eq!(store.stats().bytes, 0);
}

/// A deny that names either side applies: the caller the session acts for,
/// or the instance that carries the call (`via`).
#[test]
fn a_deny_naming_the_carrier_refuses_an_on_behalf_call_and_put() {
    let r = rig_with("infer-proxy");
    let e = r.broker.call_service_for(&r.proxy, 1, "svc.infer/venq", "infer.v1", json!({}), Some(5000), "mesh:good").unwrap_err();
    assert_eq!(e.code, ErrorCode::Denied, "{e}");
    let e = r.broker.blob_begin_for(&r.proxy, "image/png", 10, Some(60_000), "mesh:good").unwrap_err();
    assert_eq!(e.code, ErrorCode::Denied, "{e}");
    assert_eq!(r.broker.blob_store().unwrap().stats().reserved, 0);
}

#[test]
fn a_deny_naming_the_caller_still_refuses_and_no_deny_allows() {
    let r = rig_with("mesh:bad");
    let e = r.broker.call_service_for(&r.proxy, 1, "svc.infer/venq", "infer.v1", json!({}), Some(5000), "mesh:bad").unwrap_err();
    assert_eq!(e.code, ErrorCode::Denied, "{e}");
    assert_eq!(
        r.broker.blob_begin_for(&r.proxy, "image/png", 10, Some(60_000), "mesh:bad").unwrap_err().code,
        ErrorCode::Denied
    );
    // Neither the caller nor the carrier is named: allowed.
    r.broker.call_service_for(&r.proxy, 2, "svc.infer/venq", "infer.v1", json!({}), Some(5000), "mesh:good").unwrap();
    r.broker.blob_begin_for(&r.proxy, "image/png", 10, Some(60_000), "mesh:good").unwrap();
}
