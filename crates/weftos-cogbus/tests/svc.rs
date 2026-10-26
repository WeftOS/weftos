//! Service calls over a real socket (ADR-122 section 12): request/reply on
//! `svc.infer/<node>` with the same manifest, project and governance rules
//! as readings, deadlines, and the provider going away.
#![cfg(unix)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use clawft_types::cog_topics::{InputPolicy, TopicClass};
use serde_json::{Value, json};
use weftos_cogbus::server::{ServerHandle, serve};
use weftos_cogbus::{
    Broker, BrokerConfig, BusAction, BusActivity, BusHooks, BusPolicy, BusRequest, BusSink, Client,
    ClientError, ErrorCode, Grant, InputSpec, InstanceSpec, OutputSpec, RefusalLimits,
};

const P1: &str = "01J9ZXW0PRJCTAAAAAAAAAAAAA";
const P2: &str = "01J9ZXW0PRJCTBBBBBBBBBBBBB";
const SCHEMA: &str = "infer.v1";
const TOPIC: &str = "svc.infer/venq-npu";

fn token(c: char) -> String {
    std::iter::repeat_n(c, 64).collect()
}

fn refused<T: std::fmt::Debug>(r: Result<T, ClientError>) -> ErrorCode {
    match r {
        Err(ClientError::Refused { code, .. }) => code,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

fn svc_out() -> OutputSpec {
    OutputSpec {
        topic: "svc.infer".into(),
        schema: SCHEMA.into(),
        class: TopicClass::Reading,
    }
}

fn svc_in() -> InputSpec {
    InputSpec {
        topic: "svc.infer".into(),
        schema: SCHEMA.into(),
        policy: InputPolicy::DropOldest,
    }
}

struct Rig {
    broker: Arc<Broker>,
    server: ServerHandle,
    _tmp: tempfile::TempDir,
}

impl Rig {
    async fn new(hooks: Option<BusHooks>) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let mut b = Broker::new(BrokerConfig::new("0".repeat(32)));
        if let Some(h) = hooks {
            b = b.with_hooks(h);
        }
        let broker = Arc::new(b);
        let server = serve(broker.clone(), &tmp.path().join("cogbus.sock"))
            .await
            .unwrap();
        Self {
            broker,
            server,
            _tmp: tmp,
        }
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

    /// provider `p` (token p), callers `a` and `b` in P1, `x` in P2, `n` with no input.
    fn standard(&self) {
        self.cog("npu", 'p', P1, vec![svc_out()], vec![]);
        self.cog("a", 'a', P1, vec![], vec![svc_in()]);
        self.cog("b", 'b', P1, vec![], vec![svc_in()]);
        self.cog("x", 'x', P2, vec![], vec![svc_in()]);
        self.cog("n", 'n', P1, vec![], vec![]);
    }
}

/// Provider `p` echoes every call's body until its connection closes.
async fn start_echo(rig: &Rig) {
    let p = rig.connect('p').await;
    let mut svc = p.serve("svc.infer", "venq-npu", SCHEMA).await.unwrap();
    tokio::spawn(async move {
        while let Some(c) = svc.recv().await {
            let r = p.reply(c.call, Ok(json!({ "echo": c.body, "from": c.from }))).await;
            r.unwrap();
        }
    });
}

#[tokio::test]
async fn two_callers_share_one_provider_and_each_gets_its_own_reply() {
    let rig = Rig::new(None).await;
    rig.standard();
    start_echo(&rig).await;
    let a = rig.connect('a').await;
    let b = rig.connect('b').await;
    let (ra, rb) = tokio::join!(
        a.invoke(TOPIC, SCHEMA, json!({"q": "a"}), Duration::from_secs(5)),
        b.invoke(TOPIC, SCHEMA, json!({"q": "b"}), Duration::from_secs(5)),
    );
    let (ra, rb) = (ra.unwrap(), rb.unwrap());
    assert_eq!(ra["echo"], json!({"q": "a"}));
    assert_eq!(ra["from"], "a");
    assert_eq!(rb["echo"], json!({"q": "b"}));
    assert_eq!(rb["from"], "b");
    assert_eq!(rig.broker.pending_calls(), 0);
}

#[tokio::test]
async fn the_manifest_project_and_service_rules_hold() {
    let rig = Rig::new(None).await;
    rig.standard();
    start_echo(&rig).await;
    let n = rig.connect('n').await;
    let a = rig.connect('a').await;
    let x = rig.connect('x').await;
    let t = Duration::from_secs(2);
    // a cog without the input cannot call
    assert_eq!(refused(n.invoke(TOPIC, SCHEMA, json!({}), t).await), ErrorCode::Undeclared);
    // the schema must match the declaration
    assert_eq!(refused(a.invoke(TOPIC, "telemetry.v1", json!({}), t).await), ErrorCode::SchemaMismatch);
    // another project needs a grant
    assert_eq!(refused(x.invoke(TOPIC, SCHEMA, json!({}), t).await), ErrorCode::CrossProject);
    // nothing serves this name
    assert_eq!(refused(a.invoke("svc.infer/other", SCHEMA, json!({}), t).await), ErrorCode::Unavailable);
    // not a service topic, wildcard, bad names
    assert_eq!(refused(a.invoke("telemetry/venq", SCHEMA, json!({}), t).await), ErrorCode::BadRequest);
    assert_eq!(refused(a.invoke("svc.infer/*", SCHEMA, json!({}), t).await), ErrorCode::BadRequest);
    // a service is not a reading topic
    assert_eq!(
        refused(a.subscribe("svc.infer/venq-npu", SCHEMA).await),
        ErrorCode::Unsupported
    );
    let p = rig.connect('p').await;
    assert_eq!(
        refused(p.publish("svc.infer", "venq-npu", SCHEMA, 1, json!({})).await),
        ErrorCode::Unsupported
    );
    // a cog that declares no service output cannot serve
    assert_eq!(refused(a.serve("svc.infer", "venq-npu", SCHEMA).await), ErrorCode::Undeclared);
    // a request over the body limit is refused before it is routed
    let big = json!({ "t": "x".repeat(65_600) });
    assert_eq!(refused(a.invoke(TOPIC, SCHEMA, big, t).await), ErrorCode::TooLarge);
}

#[tokio::test]
async fn one_instance_provides_a_topic_and_a_second_is_refused() {
    let rig = Rig::new(None).await;
    rig.standard();
    rig.cog("npu2", 'q', P1, vec![svc_out()], vec![]);
    let p = rig.connect('p').await;
    let _svc = p.serve("svc.infer", "venq-npu", SCHEMA).await.unwrap();
    let q = rig.connect('q').await;
    assert_eq!(refused(q.serve("svc.infer", "venq-npu", SCHEMA).await), ErrorCode::WriterConflict);
    // a different name is its own service
    q.serve("svc.infer", "other", SCHEMA).await.unwrap();
}

#[tokio::test]
async fn a_call_past_its_deadline_times_out_and_a_late_reply_is_refused() {
    let rig = Rig::new(None).await;
    rig.standard();
    let p = rig.connect('p').await;
    let mut svc = p.serve("svc.infer", "venq-npu", SCHEMA).await.unwrap();
    let a = rig.connect('a').await;
    let started = std::time::Instant::now();
    let r = a.invoke(TOPIC, SCHEMA, json!({}), Duration::from_millis(200)).await;
    assert_eq!(refused(r), ErrorCode::Timeout);
    let waited = started.elapsed();
    assert!(waited >= Duration::from_millis(190) && waited < Duration::from_secs(2), "{waited:?}");
    assert_eq!(rig.broker.pending_calls(), 0);
    let late = svc.recv().await.unwrap();
    assert_eq!(late.timeout_ms, 200);
    assert_eq!(refused(p.reply(late.call, Ok(json!({}))).await), ErrorCode::BadRequest);
}

#[tokio::test]
async fn a_provider_error_reaches_the_caller_as_a_service_error() {
    let rig = Rig::new(None).await;
    rig.standard();
    let p = Arc::new(rig.connect('p').await);
    let mut svc = p.serve("svc.infer", "venq-npu", SCHEMA).await.unwrap();
    let pp = p.clone();
    tokio::spawn(async move {
        let c = svc.recv().await.unwrap();
        pp.reply(c.call, Err("unknown model".into())).await.unwrap();
    });
    let a = rig.connect('a').await;
    match a.invoke(TOPIC, SCHEMA, json!({}), Duration::from_secs(2)).await {
        Err(ClientError::Refused { code: ErrorCode::ServiceError, message }) => {
            assert_eq!(message, "unknown model")
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn a_provider_that_goes_away_fails_the_calls_waiting_on_it() {
    let rig = Rig::new(None).await;
    rig.standard();
    let p = rig.connect('p').await;
    let mut svc = p.serve("svc.infer", "venq-npu", SCHEMA).await.unwrap();
    let a = rig.connect('a').await;
    let call = tokio::spawn(async move { a.invoke(TOPIC, SCHEMA, json!({}), Duration::from_secs(30)).await });
    let _ = svc.recv().await.unwrap();
    drop((svc, p));
    let r = tokio::time::timeout(Duration::from_secs(3), call).await.unwrap().unwrap();
    assert_eq!(refused(r), ErrorCode::Unavailable);
    // and with the provider gone, a new call is refused at once
    let b = rig.connect('b').await;
    assert_eq!(refused(b.invoke(TOPIC, SCHEMA, json!({}), Duration::from_secs(1)).await), ErrorCode::Unavailable);
}

#[tokio::test]
async fn a_cog_cannot_answer_a_call_it_was_not_given() {
    let rig = Rig::new(None).await;
    rig.standard();
    rig.cog("npu2", 'q', P1, vec![svc_out()], vec![]);
    let p = rig.connect('p').await;
    let mut svc = p.serve("svc.infer", "venq-npu", SCHEMA).await.unwrap();
    let q = rig.connect('q').await;
    let _svc2 = q.serve("svc.infer", "other", SCHEMA).await.unwrap();
    let a = Arc::new(rig.connect('a').await);
    let aa = a.clone();
    let call = tokio::spawn(async move { aa.invoke(TOPIC, SCHEMA, json!({}), Duration::from_secs(2)).await });
    let c = svc.recv().await.unwrap();
    assert_eq!(refused(q.reply(c.call, Ok(json!({"forged": true}))).await), ErrorCode::BadRequest);
    p.reply(c.call, Ok(json!({"real": true}))).await.unwrap();
    assert_eq!(call.await.unwrap().unwrap(), json!({"real": true}));
}

#[tokio::test]
async fn a_caller_has_a_bounded_number_of_calls_in_flight() {
    let rig = Rig::new(None).await;
    rig.standard();
    let p = rig.connect('p').await;
    let _svc = p.serve("svc.infer", "venq-npu", SCHEMA).await.unwrap();
    let a = Arc::new(rig.connect('a').await);
    let mut waiting = Vec::new();
    for _ in 0..16 {
        let a = a.clone();
        waiting.push(tokio::spawn(async move {
            a.invoke(TOPIC, SCHEMA, json!({}), Duration::from_secs(30)).await
        }));
    }
    while rig.broker.pending_calls() < 16 {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(refused(a.invoke(TOPIC, SCHEMA, json!({}), Duration::from_secs(1)).await), ErrorCode::Busy);
    // another caller is not held back by the first one's backlog
    let b = rig.connect('b').await;
    let r = b.invoke(TOPIC, SCHEMA, json!({}), Duration::from_millis(100)).await;
    assert_eq!(refused(r), ErrorCode::Timeout);
    for w in waiting {
        w.abort();
    }
}

#[tokio::test]
async fn an_oversize_reply_fails_the_caller_and_the_provider() {
    let rig = Rig::new(None).await;
    rig.standard();
    let p = Arc::new(rig.connect('p').await);
    let mut svc = p.serve("svc.infer", "venq-npu", SCHEMA).await.unwrap();
    let pp = p.clone();
    let prov = tokio::spawn(async move {
        let c = svc.recv().await.unwrap();
        pp.reply(c.call, Ok(json!({"t": "x".repeat(65_600)}))).await
    });
    let a = rig.connect('a').await;
    assert_eq!(refused(a.invoke(TOPIC, SCHEMA, json!({}), Duration::from_secs(2)).await), ErrorCode::TooLarge);
    assert_eq!(refused(prov.await.unwrap()), ErrorCode::TooLarge);
}

#[derive(Default)]
struct Deny {
    actions: Vec<BusAction>,
    asked: Mutex<Vec<(BusAction, String)>>,
}

impl BusPolicy for Deny {
    fn decide(&self, r: &BusRequest<'_>) -> Result<(), String> {
        self.asked.lock().unwrap().push((r.action, r.topic.to_string()));
        if self.actions.contains(&r.action) {
            Err(format!("{} is closed", r.action.as_str()))
        } else {
            Ok(())
        }
    }
    fn grants(&self, _: &BusRequest<'_>) -> Vec<Grant> {
        Vec::new()
    }
}

#[derive(Default)]
struct Rows(Mutex<Vec<(String, bool)>>);

impl BusSink for Rows {
    fn record(&self, a: &BusActivity) {
        self.0
            .lock()
            .unwrap()
            .push((format!("{}:{}", a.action, a.topic), a.outcome == weftos_cogbus::Outcome::Ok));
    }
}

fn hooks(policy: Arc<Deny>, rows: Arc<Rows>) -> BusHooks {
    BusHooks {
        policy,
        sink: rows,
        limits: RefusalLimits::default(),
    }
}

#[tokio::test]
async fn governance_decides_serve_and_every_call_and_the_log_shows_both() {
    // serve closed
    let rows = Arc::new(Rows::default());
    let rig = Rig::new(Some(hooks(Arc::new(Deny { actions: vec![BusAction::Serve], ..Default::default() }), rows.clone()))).await;
    rig.standard();
    let p = rig.connect('p').await;
    assert_eq!(refused(p.serve("svc.infer", "venq-npu", SCHEMA).await), ErrorCode::Denied);

    // call closed, serve open
    let policy = Arc::new(Deny { actions: vec![BusAction::Call], ..Default::default() });
    let rows = Arc::new(Rows::default());
    let rig = Rig::new(Some(hooks(policy.clone(), rows.clone()))).await;
    rig.standard();
    let p = rig.connect('p').await;
    let _svc = p.serve("svc.infer", "venq-npu", SCHEMA).await.unwrap();
    let a = rig.connect('a').await;
    assert_eq!(refused(a.invoke(TOPIC, SCHEMA, json!({}), Duration::from_secs(1)).await), ErrorCode::Denied);
    assert_eq!(rig.broker.pending_calls(), 0);
    let asked = policy.asked.lock().unwrap().clone();
    assert!(asked.contains(&(BusAction::Serve, TOPIC.to_string())), "{asked:?}");
    assert!(asked.contains(&(BusAction::Call, TOPIC.to_string())), "{asked:?}");
    let logged = rows.0.lock().unwrap().clone();
    assert!(logged.contains(&(format!("cogbus.serve:{TOPIC}"), true)), "{logged:?}");
    assert!(logged.contains(&(format!("cogbus.call:{TOPIC}"), false)), "{logged:?}");
}

#[tokio::test]
async fn an_allowed_call_is_logged_and_answered() {
    let rows = Arc::new(Rows::default());
    let rig = Rig::new(Some(hooks(Arc::new(Deny::default()), rows.clone()))).await;
    rig.standard();
    start_echo(&rig).await;
    let a = rig.connect('a').await;
    let v: Value = a.invoke(TOPIC, SCHEMA, json!({"q": 1}), Duration::from_secs(2)).await.unwrap();
    assert_eq!(v["echo"], json!({"q": 1}));
    let logged = rows.0.lock().unwrap().clone();
    assert!(logged.contains(&(format!("cogbus.call:{TOPIC}"), true)), "{logged:?}");
}
