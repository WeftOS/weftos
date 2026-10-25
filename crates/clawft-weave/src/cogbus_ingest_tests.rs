//! B4: released-cog posts and broker-native readings reach the right store
//! through the broker's store sink.

use std::path::Path;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use clawft_kernel::cog_ingest::{BridgeConfig, IngestVector, StaticRouter, TokenRegistry};
use clawft_kernel::workload_ctl::host_cogbus::{CogbusRegistrar, CogbusRegistration};
use clawft_kernel::workload_runtime::{CogSpec, HostContract};
use ed25519_dalek::SigningKey;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use weftos_cogbus::{BrokerConfig, InstanceSpec, Session};

use super::*;
use crate::cog_ingest_serve::{IngestConfig, start_on_bus};
use crate::cogbus_serve::Registrar;

const P1: &str = "01J9ZXW0PRJCTAAAAAAAAAAAAA";
const P2: &str = "01J9ZXW0PRJCTBBBBBBBBBBBBB";
const BODY: &str = r#"{"vectors":[[1,[0,1,2,3,4,5,6,7]]],"dedup":true}"#;

/// Project, instance and vectors of one batch that reached a store.
type Seen = (Option<String>, String, Vec<IngestVector>);

/// A fake store owner: records what reaches it.
#[derive(Default)]
struct Rec(StdMutex<Vec<Seen>>);

impl Rec {
    fn seen(&self) -> Vec<Seen> {
        self.0.lock().unwrap().clone()
    }
}

#[async_trait]
impl Forwarder for Rec {
    async fn forward(&self, b: &InstanceBinding, batch: &IngestBatch) -> Result<IngestOutcome, IngestError> {
        self.0.lock().unwrap().push((b.project_id.clone(), b.instance_id.clone(), batch.vectors.clone()));
        let n = batch.vectors.len();
        Ok(IngestOutcome { accepted: n, deduped: 0, total: n })
    }
}

fn routes() -> (Arc<Rec>, Arc<Rec>, Arc<dyn StoreRouter>) {
    let (a, b) = (Arc::new(Rec::default()), Arc::new(Rec::default()));
    let r = StaticRouter::new().with_project_route(P1, a.clone()).with_project_route(P2, b.clone());
    (a, b, Arc::new(r))
}

fn tok(n: u8) -> String {
    format!("{n:02x}").repeat(32)
}

fn contract(token: &str) -> HostContract {
    let mut c = HostContract::default_feed();
    c.token = clawft_types::secret::SecretString::new(token.to_string());
    c
}

async fn post(addr: SocketAddr, token: &str, body: &str) -> String {
    let raw = format!(
        "POST /api/v1/store/ingest HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    s.write_all(raw.as_bytes()).await.unwrap();
    let mut out = String::new();
    tokio::time::timeout(Duration::from_secs(10), s.read_to_string(&mut out)).await.unwrap().unwrap();
    out
}

/// A bridge over `router` with two posting instances, one per project.
async fn bridge(router: Arc<dyn StoreRouter>) -> (Arc<IngestBridge>, BridgeHandle) {
    let reg = Arc::new(TokenRegistry::new());
    for (id, p, n) in [("cog-a", P1, 1), ("cog-b", P2, 2)] {
        reg.register(InstanceBinding::new(id, Some(p.into()), "ctl"), &contract(&tok(n))).unwrap();
    }
    let budget = RateBudget::new(Duration::from_secs(60), 3, 2048);
    let br = IngestBridge::new(reg, router, budget, BridgeConfig::default());
    let h = br.bind("127.0.0.1:0".parse().unwrap(), BridgeScope::Any).await.unwrap();
    (br, h)
}

fn reader(b: &Broker, n: u8, project: &str) -> Session {
    let spec = InstanceSpec {
        instance_id: format!("reader-{n}"),
        cog_id: "reader".into(),
        project_id: Some(project.into()),
        uid: None,
        outputs: vec![],
        inputs: vec![weftos_cogbus::InputSpec {
            topic: STORE_KIND.into(),
            schema: STORE_SCHEMA.into(),
            policy: Default::default(),
        }],
    };
    b.register(spec, &tok(100 + n)).unwrap();
    let mut s = b.authenticate(&tok(100 + n), None).unwrap();
    b.subscribe(&mut s, "store.vector/*", STORE_SCHEMA).unwrap();
    s
}

#[tokio::test]
async fn a_released_cogs_post_is_published_and_answered_exactly_as_before() {
    let (plain_a, _, plain_router) = routes();
    let (_pb, plain) = bridge(plain_router).await;
    let (a, b, router) = routes();
    let broker = Arc::new(Broker::new(BrokerConfig::new("n")));
    let bus_router = attach(broker.clone(), router, RateBudget::default()).unwrap().router;
    let (_bb, bus) = bridge(bus_router).await;
    let (r1, r2) = (reader(&broker, 1, P1), reader(&broker, 2, P2));

    // Same answers, byte for byte: accepted, unknown token, bad body, rate limit.
    for (token, body) in [(tok(1), BODY), ("f".repeat(64), BODY), (tok(1), "{}"), (tok(1), BODY), (tok(1), BODY)] {
        assert_eq!(post(bus.addr(), &token, body).await, post(plain.addr(), &token, body).await);
    }
    assert_eq!(plain_a.seen().len(), 2, "two accepted posts, then the third request was rate limited");
    let seen = a.seen();
    assert_eq!(seen.len(), 2);
    assert_eq!((seen[0].0.as_deref(), seen[0].1.as_str()), (Some(P1), "cog-a"));
    assert_eq!(seen[0].2, plain_a.seen()[0].2, "the store gets the floats the bridge parsed");
    assert!(b.seen().is_empty(), "project B's store saw nothing");

    // On the broker: P1's reader got the readings, P2's did not.
    let got = r1.drain();
    assert_eq!(got.len(), 2);
    assert_eq!((got[0].topic.as_str(), got[0].schema.as_str()), ("store.vector/cog-a", STORE_SCHEMA));
    assert_eq!(batch_of(&got[0].body).unwrap().vectors, seen[0].2);
    assert!(r2.drain().is_empty());
    let row = broker.snapshot().into_iter().find(|t| t.topic == "store.vector/cog-a").unwrap();
    assert_eq!((row.writer.as_deref(), row.project_id.as_deref(), row.messages), (Some(BRIDGE_WRITER), Some(P1), 2));
}

#[tokio::test]
async fn posts_from_two_projects_reach_only_their_own_store() {
    let (a, b, router) = routes();
    let broker = Arc::new(Broker::new(BrokerConfig::new("n")));
    let bus_router = attach(broker.clone(), router, RateBudget::default()).unwrap().router;
    let (_br, h) = bridge(bus_router).await;
    assert!(post(h.addr(), &tok(1), BODY).await.starts_with("HTTP/1.1 200"));
    assert!(post(h.addr(), &tok(2), BODY).await.starts_with("HTTP/1.1 200"));
    let (sa, sb) = (a.seen(), b.seen());
    assert_eq!((sa.len(), sa[0].1.as_str()), (1, "cog-a"));
    assert_eq!((sb.len(), sb[0].0.as_deref(), sb[0].1.as_str()), (1, Some(P2), "cog-b"));
    let mut projects: Vec<_> = broker.snapshot().into_iter().map(|t| (t.topic, t.project_id)).collect();
    projects.sort();
    assert_eq!(projects, vec![
        ("store.vector/cog-a".to_string(), Some(P1.to_string())),
        ("store.vector/cog-b".to_string(), Some(P2.to_string())),
    ]);
}

const NATIVE_TOML: &str = r#"
[cog]
id = "venq-radar"
version = "0.1.0"

[[outputs]]
topic = "store.vector"
schema = "store.vector.v1"
class = "reading"
rate = "1 Hz"
"#;

/// Register a broker-native cog the way the host does, and open its session.
fn native(reg: &Registrar, broker: &Broker, id: &str, project: &str, n: u8) -> Session {
    let spec = CogSpec::parse(NATIVE_TOML.as_bytes()).unwrap();
    let r = CogbusRegistration {
        instance_id: id.into(),
        cog_id: spec.id,
        project_id: Some(project.into()),
        controller_node: "ctl".into(),
        token: clawft_types::secret::SecretString::new(tok(n)),
        uid: None,
        outputs: spec.outputs,
        inputs: spec.inputs,
    };
    reg.register(&r).unwrap();
    broker.authenticate(&tok(n), None).unwrap()
}

fn vector_body(id: u64, v: f32) -> Value {
    json!({"vectors": [[id, [v, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]]], "metrics": {"presence": 1.0}})
}

async fn until<T>(mut f: impl FnMut() -> Option<T>) -> T {
    for _ in 0..200 {
        if let Some(t) = f() {
            return t;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out");
}

#[tokio::test]
async fn a_broker_native_cog_reaches_its_projects_store_through_the_sink() {
    let (a, b, router) = routes();
    let broker = Arc::new(Broker::new(BrokerConfig::new("n")));
    let sink = attach(broker.clone(), router, RateBudget::default()).unwrap().sink;
    let reg = Registrar::new(broker.clone(), Path::new("/run/test/cogbus.sock")).with_store_sink(sink.clone());
    let s1 = native(&reg, &broker, "radar-1", P1, 1);
    let s2 = native(&reg, &broker, "radar-2", P2, 2);
    broker.publish(&s1, STORE_KIND, "radar-1", STORE_SCHEMA, 1, vector_body(9, 0.5)).unwrap();
    broker.publish(&s2, STORE_KIND, "radar-2", STORE_SCHEMA, 1, vector_body(9, 0.25)).unwrap();
    let sa = until(|| a.seen().pop()).await;
    let sb = until(|| b.seen().pop()).await;
    assert_eq!((sa.0.as_deref(), sa.1.as_str(), sa.2[0].id, sa.2[0].values[0]), (Some(P1), "radar-1", 9, 0.5));
    assert_eq!((sb.0.as_deref(), sb.1.as_str(), sb.2[0].values[0]), (Some(P2), "radar-2", 0.25));
    assert_eq!((a.seen().len(), b.seen().len()), (1, 1));

    // A reading naming another project than the instance's placement, a
    // revoked instance and a malformed body are refused by the sink.
    let r = SinkReading {
        project_id: Some(P2.into()),
        instance_id: "radar-1".into(),
        delivery: weftos_cogbus::Delivery {
            sub: 0, topic: "store.vector/radar-1".into(), schema: STORE_SCHEMA.into(), source_id: "radar-1".into(),
            node_id: "n".into(), t_ns: 1, seq: 1, recv_ns: 1, body: vector_body(1, 1.0),
        },
    };
    assert_eq!(sink.take(&r).await.unwrap_err(), IngestError::Forbidden);
    let mut r = SinkReading { project_id: Some(P1.into()), ..r };
    r.delivery.body = json!({"vectors": [[1, [0.0]]]});
    assert!(matches!(sink.take(&r).await.unwrap_err(), IngestError::Malformed(_)));
    reg.revoke("radar-1");
    r.delivery.body = vector_body(1, 1.0);
    assert_eq!(sink.take(&r).await.unwrap_err(), IngestError::Unauthorized);
    assert_eq!(a.seen().len(), 1);
}

#[test]
fn source_ids_are_the_instance_or_its_digest() {
    assert_eq!(source_of("board-telemetry-0a1b2c3d-n1"), "board-telemetry-0a1b2c3d-n1");
    let long = format!("cog-0a1b2c3d-{}", "f".repeat(64));
    let s = source_of(&long);
    assert!(s.starts_with("i-") && s.len() == 18 && valid_source_id(&s));
    assert_ne!(source_of("Cog-A"), source_of("Cog-B"));
}

#[test]
fn bridge_bind_may_be_off_or_have_a_loopback_fallback() {
    let c = |j: &str| serde_json::from_str::<IngestConfig>(j).unwrap().validate();
    assert!(c(r#"{"bridge":{"bind":"off"}}"#).is_ok());
    assert!(c(r#"{"bridge":{"bind":"127.0.0.1:80","fallback_bind":"127.0.0.1:0"}}"#).is_ok());
    assert!(c(r#"{"bridge":{"bind":"off","fallback_bind":"127.0.0.1:0"}}"#).is_err());
    assert!(c(r#"{"bridge":{"fallback_bind":"0.0.0.0:8080"}}"#).is_err());
    assert!(c(r#"{"bridge":{"bind":"of"}}"#).is_err());
}

/// What the owner has stored for `id`, by upserting a probe id.
async fn total_for(sink: &StoreSink, id: &str) -> usize {
    let probe = IngestBatch { vectors: vec![IngestVector { id: 999, values: [9.0; 8] }], dedup: false };
    sink.deliver(&InstanceBinding::new(id, Some(P1.into()), "ctl"), &probe).await.unwrap().total
}

async fn daemon(bind: &str) -> (crate::cog_ingest_serve::IngestRuntime, Arc<Broker>, Registrar) {
    let cfg: IngestConfig = serde_json::from_str(&format!(
        r#"{{"bridge":{{{bind}}},"routes":[{{"project":"{P1}","owner":"local"}}]}}"#
    ))
    .unwrap();
    let broker = Arc::new(Broker::new(BrokerConfig::new("n")));
    let rt = start_on_bus(&cfg, &SigningKey::from_bytes(&[9; 32]), None, None, Some(broker.clone())).await.unwrap();
    let reg = Registrar::new(broker.clone(), Path::new("/run/test/cogbus.sock"))
        .with_store_sink(rt.store_sink.clone().unwrap());
    (rt, broker, reg)
}

#[tokio::test]
async fn without_port_80_the_fallback_serves_http_and_the_broker_path_is_unaffected() {
    let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let taken = held.local_addr().unwrap();
    let (rt, broker, reg) = daemon(&format!(r#""bind":"{taken}","fallback_bind":"127.0.0.1:0""#)).await;
    let addr = rt.bridge_addr.unwrap();
    assert!(addr != taken && addr.ip().is_loopback() && rt.hooks.is_enabled());
    let (c, _) = rt.hooks.prepare("native", HostContract::default_feed()).await.unwrap();
    assert_eq!(c.ingest_url.unwrap(), format!("http://{addr}/api/v1/store/ingest"), "COGNITUM_INGEST_URL follows");

    // A broker-native cog stores with no HTTP at all.
    let s = native(&reg, &broker, "venq-1", P1, 1);
    broker.publish(&s, STORE_KIND, "venq-board", STORE_SCHEMA, 1, vector_body(1, 0.5)).unwrap();
    let sink = rt.store_sink.clone().unwrap();
    until_total(&sink, "venq-1", 2).await;

    // The same instance posting over the fallback lands in the same store
    // namespace: its id 1 is a duplicate now.
    let ct = contract(&tok(1));
    rt.hooks.registry().register(InstanceBinding::new("venq-1", Some(P1.into()), "ctl"), &ct).unwrap();
    let out = post(addr, &tok(1), r#"{"vectors":[[1,[0.5,0,0,0,0,0,0,0]]],"dedup":true}"#).await;
    assert!(out.starts_with("HTTP/1.1 200") && out.contains(r#""accepted":0"#) && out.contains(r#""deduped":1"#), "{out}");
}

async fn until_total(sink: &StoreSink, id: &str, want: usize) {
    for _ in 0..200 {
        if total_for(sink, id).await == want {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("store never held {want} vectors for {id}");
}

#[tokio::test]
async fn with_the_bridge_off_the_broker_path_still_stores() {
    let (rt, broker, reg) = daemon(r#""bind":"off""#).await;
    assert!(rt.bridge_addr.is_none() && !rt.hooks.is_enabled());
    assert!(rt.bridge_error.as_deref().unwrap().contains("off"));
    let s = native(&reg, &broker, "venq-2", P1, 2);
    broker.publish(&s, STORE_KIND, "venq-board", STORE_SCHEMA, 1, vector_body(1, 0.5)).unwrap();
    until_total(&rt.store_sink.clone().unwrap(), "venq-2", 2).await;
}

#[tokio::test]
async fn a_daemon_where_port_80_binds_keeps_it() {
    // The primary bind is used whenever it binds; the fallback is not tried.
    let (rt, _b, _r) = daemon(r#""bind":"127.0.0.1:0","fallback_bind":"127.0.0.1:1""#).await;
    assert!(rt.bridge_addr.unwrap().port() != 1 && rt.bridge_error.is_none());
}
