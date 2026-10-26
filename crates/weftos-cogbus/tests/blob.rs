//! The content store over a real socket (ADR-122 B5): put and get with the
//! hello-authenticated token, the manifest, governance and the project rule;
//! a frame handle that resolves and then expires; and a service reply that
//! carries a handle instead of a body over 64 KiB.
#![cfg(unix)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use clawft_types::cog_topics::{BlobHandle, InputPolicy, TopicClass};
use serde_json::json;
use weftos_cogbus::blob::{BlobConfig, BlobStore};
use weftos_cogbus::server::{ServerHandle, serve};
use weftos_cogbus::{
    Broker, BrokerConfig, BusAction, BusHooks, BusPolicy, BusRequest, Client, ClientError, ErrorCode, Grant,
    InputSpec, InstanceSpec, OutputSpec,
};

const P1: &str = "01J9ZXW0PRJCTAAAAAAAAAAAAA";
const P2: &str = "01J9ZXW0PRJCTBBBBBBBBBBBBB";

fn token(c: char) -> String {
    std::iter::repeat_n(c, 64).collect()
}

fn refused<T: std::fmt::Debug>(r: Result<T, ClientError>) -> ErrorCode {
    match r {
        Err(ClientError::Refused { code, .. }) => code,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

fn blob_out() -> OutputSpec {
    OutputSpec { topic: "frames".into(), schema: "blob.v1".into(), class: TopicClass::Reading }
}

fn svc_out() -> OutputSpec {
    OutputSpec { topic: "svc.image".into(), schema: "infer.v1".into(), class: TopicClass::Reading }
}

fn svc_in() -> InputSpec {
    InputSpec { topic: "svc.image".into(), schema: "infer.v1".into(), policy: InputPolicy::DropOldest }
}

fn frames_in() -> InputSpec {
    InputSpec { topic: "frames".into(), schema: "blob.v1".into(), policy: InputPolicy::DropOldest }
}

#[derive(Default)]
struct Deny(Mutex<Vec<BusAction>>, Vec<BusAction>);

impl BusPolicy for Deny {
    fn decide(&self, r: &BusRequest<'_>) -> Result<(), String> {
        self.0.lock().unwrap().push(r.action);
        if self.1.contains(&r.action) { Err("closed".into()) } else { Ok(()) }
    }
    fn grants(&self, _: &BusRequest<'_>) -> Vec<Grant> {
        Vec::new()
    }
}

struct Rig {
    broker: Arc<Broker>,
    server: ServerHandle,
    _tmp: tempfile::TempDir,
}

impl Rig {
    async fn new(hooks: Option<BusHooks>, cfg: BlobConfig) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(BlobStore::open(&tmp.path().join("blobs"), cfg, 0).unwrap());
        let mut b = Broker::new(BrokerConfig::new("0".repeat(32))).with_blobs(store);
        if let Some(h) = hooks {
            b = b.with_hooks(h);
        }
        let broker = Arc::new(b);
        let server = serve(broker.clone(), &tmp.path().join("cogbus.sock")).await.unwrap();
        Self { broker, server, _tmp: tmp }
    }

    fn cog(&self, id: &str, c: char, project: &str, outputs: Vec<OutputSpec>, inputs: Vec<InputSpec>) {
        let spec = InstanceSpec {
            instance_id: id.into(),
            cog_id: id.into(),
            project_id: Some(project.into()),
            uid: None,
            outputs,
            inputs,
        };
        self.broker.register(spec, &token(c)).unwrap();
    }

    async fn connect(&self, c: char) -> Client {
        Client::connect(self.server.path(), &token(c)).await.unwrap()
    }
}

fn data(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i * 7 % 253) as u8).collect()
}

#[tokio::test]
async fn a_frame_handle_resolves_locally_for_its_project_and_not_for_another() {
    let rig = Rig::new(None, BlobConfig::default()).await;
    rig.cog("cam", 'c', P1, vec![blob_out()], vec![]);
    rig.cog("viewer", 'v', P1, vec![], vec![]);
    rig.cog("outsider", 'o', P2, vec![], vec![]);
    let cam = rig.connect('c').await;
    let frame = data(300_000);
    let h = cam.put_blob("image/png", &frame, Some(Duration::from_secs(60))).await.unwrap();
    assert_eq!(h.hash, blake3::hash(&frame).to_hex().to_string());
    assert_eq!((h.size, h.media_type.as_str()), (300_000, "image/png"));
    assert_eq!(cam.get_blob(&h).await.unwrap(), frame);
    assert_eq!(rig.connect('v').await.get_blob(&h).await.unwrap(), frame, "same project");
    assert_eq!(refused(rig.connect('o').await.get_blob(&h).await), ErrorCode::CrossProject);
    assert_eq!(refused(cam.get_blob(&BlobHandle { hash: "0".repeat(64), ..h.clone() }).await), ErrorCode::NotFound);
}

#[tokio::test]
async fn a_handle_expires_on_its_ttl() {
    let rig = Rig::new(None, BlobConfig::default()).await;
    rig.cog("cam", 'c', P1, vec![blob_out()], vec![]);
    let cam = rig.connect('c').await;
    let h = cam.put_blob("image/png", b"short lived", Some(Duration::from_millis(150))).await.unwrap();
    assert_eq!(cam.get_blob(&h).await.unwrap(), b"short lived");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(refused(cam.get_blob(&h).await), ErrorCode::NotFound);
    assert_eq!(rig.broker.blob_store().unwrap().stats().blobs, 0, "dropped on the failed read");
}

#[tokio::test]
async fn the_manifest_the_quota_and_governance_all_apply() {
    let deny = Arc::new(Deny(Mutex::default(), vec![BusAction::BlobGet]));
    let hooks = BusHooks { policy: deny.clone(), ..BusHooks::default() };
    let cfg = BlobConfig { quota_bytes: 1000, per_instance_bytes: 1000, ..Default::default() };
    let rig = Rig::new(Some(hooks), cfg).await;
    rig.cog("cam", 'c', P1, vec![blob_out()], vec![]);
    rig.cog("plain", 'p', P1, vec![], vec![]);
    let cam = rig.connect('c').await;
    // No declared blob output: refused.
    assert_eq!(refused(rig.connect('p').await.put_blob("image/png", b"x", None).await), ErrorCode::Undeclared);
    // Quota.
    let h = cam.put_blob("image/png", &data(900), None).await.unwrap();
    assert_eq!(refused(cam.put_blob("image/png", &data(200), None).await), ErrorCode::QuotaExceeded);
    // Governance is asked for put once and for get once, and a denial refuses.
    assert_eq!(refused(cam.get_blob(&h).await), ErrorCode::Denied);
    let asked = deny.0.lock().unwrap().clone();
    assert_eq!(asked.iter().filter(|a| **a == BusAction::BlobPut).count(), 2, "{asked:?}");
    assert!(asked.contains(&BusAction::BlobGet));
    // Oversize and bad shapes.
    assert_eq!(refused(cam.put_blob("nonsense", b"x", None).await), ErrorCode::BadRequest);
}

#[tokio::test]
async fn a_reply_carries_a_handle_when_the_result_is_over_the_body_limit() {
    let rig = Rig::new(None, BlobConfig::default()).await;
    rig.cog("sd15", 's', P1, vec![svc_out(), blob_out()], vec![]);
    rig.cog("ui", 'u', P1, vec![], vec![svc_in()]);
    // A caller in another project, with the service open to it only through
    // its project rule, cannot call; so the cross-project case is the
    // handle passed in a reply to a same-project caller below.
    let provider = rig.connect('s').await;
    let mut svc = provider.serve("svc.image", "npu", "infer.v1").await.unwrap();
    let image = data(400_000);
    let want = image.clone();
    tokio::spawn(async move {
        while let Some(c) = svc.recv().await {
            let h = provider.put_blob("image/png", &image, Some(Duration::from_secs(30))).await.unwrap();
            provider.reply(c.call, Ok(json!({ "image": h }))).await.unwrap();
        }
    });
    let ui = rig.connect('u').await;
    let r = ui.invoke("svc.image/npu", "infer.v1", json!({"text": "a cat"}), Duration::from_secs(5)).await.unwrap();
    assert!(serde_json::to_vec(&r).unwrap().len() < 1000, "the reply is small");
    let h: BlobHandle = serde_json::from_value(r["image"].clone()).unwrap();
    assert_eq!(ui.get_blob(&h).await.unwrap(), want, "the 400 KB image arrives through the handle");
}

#[tokio::test]
async fn a_handle_is_handed_to_a_provider_in_another_project_by_the_call_that_carries_it() {
    // Provider and caller are in different projects; the handle in the call
    // is the only thing that makes the caller's blob readable by the provider.
    struct Open;
    impl BusPolicy for Open {
        fn decide(&self, _: &BusRequest<'_>) -> Result<(), String> {
            Ok(())
        }
        fn grants(&self, r: &BusRequest<'_>) -> Vec<Grant> {
            vec![Grant { project: P1.into(), source: None }]
                .into_iter()
                .filter(|_| r.project_id == Some(P2))
                .collect()
        }
    }
    let hooks = BusHooks { policy: Arc::new(Open), ..BusHooks::default() };
    let rig = Rig::new(Some(hooks), BlobConfig::default()).await;
    rig.cog("npu", 'n', P1, vec![svc_out()], vec![]);
    rig.cog("app", 'a', P2, vec![blob_out()], vec![svc_in()]);
    let npu = rig.connect('n').await;
    let mut svc = npu.serve("svc.image", "npu", "infer.v1").await.unwrap();
    tokio::spawn(async move {
        while let Some(c) = svc.recv().await {
            let h: BlobHandle = serde_json::from_value(c.body["input"].clone()).unwrap();
            let bytes = npu.get_blob(&h).await;
            let ok = bytes.as_ref().map(|b| b.len()).unwrap_or(0);
            npu.reply(c.call, Ok(json!({ "read": ok }))).await.unwrap();
        }
    });
    let app = rig.connect('a').await;
    let h = app.put_blob("image/jpeg", &data(70_000), None).await.unwrap();
    let r = app.invoke("svc.image/npu", "infer.v1", json!({ "input": h }), Duration::from_secs(5)).await.unwrap();
    assert_eq!(r["read"], 70_000);
    // Without the handle in the call the provider could not have read it.
    let other = app.put_blob("image/jpeg", &data(10), None).await.unwrap();
    let r = app.invoke("svc.image/npu", "infer.v1", json!({ "input": h, "other": other.hash }), Duration::from_secs(5)).await.unwrap();
    assert_eq!(r["read"], 70_000);
}

#[tokio::test]
async fn a_published_handle_reaches_its_subscribers_across_a_granted_project() {
    let rig = Rig::new(None, BlobConfig::default()).await;
    rig.cog("cam", 'c', P1, vec![blob_out()], vec![]);
    rig.cog("sub", 's', P1, vec![], vec![frames_in()]);
    let cam = rig.connect('c').await;
    let sub = rig.connect('s').await;
    let mut feed = sub.subscribe("frames/*", "blob.v1").await.unwrap();
    let h = cam.put_blob("image/png", &data(5000), None).await.unwrap();
    cam.publish("frames", "cam0", "blob.v1", 1, serde_json::to_value(&h).unwrap()).await.unwrap();
    let d = tokio::time::timeout(Duration::from_secs(2), feed.recv()).await.unwrap().unwrap();
    let got: BlobHandle = serde_json::from_value(d.body).unwrap();
    assert_eq!(sub.get_blob(&got).await.unwrap(), data(5000));
}

#[tokio::test]
async fn a_node_without_a_store_says_so() {
    let tmp = tempfile::tempdir().unwrap();
    let broker = Arc::new(Broker::new(BrokerConfig::new("0".repeat(32))));
    let server = serve(broker.clone(), &tmp.path().join("cogbus.sock")).await.unwrap();
    broker
        .register(
            InstanceSpec {
                instance_id: "cam".into(),
                cog_id: "cam".into(),
                project_id: None,
                uid: None,
                outputs: vec![blob_out()],
                inputs: vec![],
            },
            &token('c'),
        )
        .unwrap();
    let c = Client::connect(server.path(), &token('c')).await.unwrap();
    assert_eq!(refused(c.put_blob("image/png", b"x", None).await), ErrorCode::Unsupported);
}
