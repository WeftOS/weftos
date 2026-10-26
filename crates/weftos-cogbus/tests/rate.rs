//! Publish budgets from `[[outputs]].rate` and the stop of a cog that
//! ignores them (ADR-122 gap 5), over a real socket.
#![cfg(unix)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use clawft_types::cog_topics::TopicClass;
use serde_json::json;
use weftos_cogbus::server::{ServerHandle, serve};
use weftos_cogbus::{
    Broker, BrokerConfig, BusActivity, BusAction, BusHooks, BusPolicy, BusRequest, BusSink, Client, ClientError,
    ErrorCode, Grant, InstanceSpec, Outcome, OutputSpec, RefusalLimits,
};

fn token(c: char) -> String {
    std::iter::repeat_n(c, 64).collect()
}

#[derive(Default)]
struct Rows {
    stops: Mutex<Vec<(String, String)>>,
    rows: Mutex<Vec<BusActivity>>,
}

impl BusSink for Rows {
    fn record(&self, a: &BusActivity) {
        self.rows.lock().unwrap().push(a.clone());
    }
    fn stop_requested(&self, id: &str, reason: &str) {
        self.stops.lock().unwrap().push((id.into(), reason.into()));
    }
}

struct Policy {
    deny_stop: bool,
    asked: Mutex<Vec<(BusAction, String, String)>>,
}

impl BusPolicy for Policy {
    fn decide(&self, r: &BusRequest<'_>) -> Result<(), String> {
        self.asked.lock().unwrap().push((r.action, r.instance_id.into(), r.cog_id.into()));
        if self.deny_stop && r.action == BusAction::Stop { Err("keep it running".into()) } else { Ok(()) }
    }
    fn grants(&self, _: &BusRequest<'_>) -> Vec<Grant> {
        Vec::new()
    }
}

struct Rig {
    broker: Arc<Broker>,
    server: ServerHandle,
    rows: Arc<Rows>,
    policy: Arc<Policy>,
    _tmp: tempfile::TempDir,
}

async fn rig(deny_stop: bool, stop_after: u32) -> Rig {
    let tmp = tempfile::tempdir().unwrap();
    let rows = Arc::new(Rows::default());
    let policy = Arc::new(Policy { deny_stop, asked: Mutex::default() });
    let hooks = BusHooks {
        policy: policy.clone(),
        sink: rows.clone(),
        limits: RefusalLimits { stop_after, ..RefusalLimits::default() },
    };
    let broker = Arc::new(Broker::new(BrokerConfig::new("0".repeat(32))).with_hooks(hooks));
    let server = serve(broker.clone(), &tmp.path().join("cogbus.sock")).await.unwrap();
    broker
        .register(
            InstanceSpec {
                instance_id: "radar-1".into(),
                cog_id: "radar".into(),
                project_id: None,
                uid: None,
                outputs: vec![
                    OutputSpec { topic: "telemetry".into(), schema: "telemetry.v1".into(), class: TopicClass::Reading },
                    OutputSpec { topic: "senml".into(), schema: "senml.v1".into(), class: TopicClass::Reading },
                ],
                inputs: vec![],
            },
            &token('r'),
        )
        .unwrap();
    broker.set_output_rate("radar-1", "telemetry", 10.0);
    Rig { broker, server, rows, policy, _tmp: tmp }
}

async fn burst(c: &Client, source: &str, n: usize) -> (usize, usize) {
    let (mut ok, mut limited) = (0, 0);
    for i in 0..n {
        match c.publish("telemetry", source, "telemetry.v1", i as u64, json!({"items": []})).await {
            Ok(_) => ok += 1,
            Err(ClientError::Refused { code: ErrorCode::RateLimited, .. }) => limited += 1,
            Err(e) => panic!("{e:?}"),
        }
    }
    (ok, limited)
}

#[tokio::test]
async fn a_cog_within_its_declared_rate_is_never_refused_and_one_above_it_is() {
    let rig = rig(false, 1000).await;
    let c = Client::connect(rig.server.path(), &token('r')).await.unwrap();
    // 10 Hz declared: 20/s sustained and a 20-message burst. 15 at once is fine.
    assert_eq!(burst(&c, "s0", 15).await, (15, 0));
    // A flood is cut at the burst: about 20 pass in total, the rest are limited.
    let (ok, limited) = burst(&c, "s1", 200).await;
    assert!((18..=30).contains(&ok) && ok + limited == 200, "{ok} ok, {limited} limited");
    // Budgets are per topic: another source of the kind has its own.
    assert_eq!(burst(&c, "s2", 15).await, (15, 0));
    // The budget refills.
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(burst(&c, "s1", 5).await, (5, 0));
    // A kind with no declared rate is not limited.
    for i in 0..100 {
        c.publish("senml", "x", "senml.v1", i, json!({"bn": "x", "e": []})).await.ok();
    }
    assert_eq!(rig.rows.stops.lock().unwrap().len(), 0, "a handful of refusals is not a stop");
}

#[tokio::test]
async fn a_cog_that_keeps_flooding_is_stopped_through_governance_and_the_runtime_is_asked() {
    let rig = rig(false, 20).await;
    let c = Client::connect(rig.server.path(), &token('r')).await.unwrap();
    // Keep flooding until the broker gives up on the connection.
    let mut closed = false;
    for i in 0..2000u64 {
        match c.publish("telemetry", "s0", "telemetry.v1", i, json!({"items": []})).await {
            Err(ClientError::Closed) => {
                closed = true;
                break;
            }
            Err(ClientError::Refused { code: ErrorCode::Revoked, .. }) => {
                closed = true;
                break;
            }
            _ => {}
        }
    }
    assert!(closed, "the connection is closed once the instance is stopped");
    let stops = rig.rows.stops.lock().unwrap().clone();
    assert_eq!(stops.len(), 1, "{stops:?}");
    assert_eq!(stops[0].0, "radar-1");
    assert!(stops[0].1.contains("rate_limited") && stops[0].1.contains("refusals"), "{}", stops[0].1);
    // Governance saw the stop, as the instance, by its cog id.
    let asked = rig.policy.asked.lock().unwrap().clone();
    assert!(asked.contains(&(BusAction::Stop, "radar-1".into(), "radar".into())), "{asked:?}");
    assert!(rig.broker.instance("radar-1").is_none(), "revoked on the bus");
    let rows = rig.rows.rows.lock().unwrap();
    assert!(rows.iter().any(|a| a.action == "cogbus.stop" && matches!(a.outcome, Outcome::Stopped { .. })));
}

#[tokio::test]
async fn governance_can_keep_the_runtime_from_stopping_the_cog_and_says_so() {
    let rig = rig(true, 20).await;
    let c = Client::connect(rig.server.path(), &token('r')).await.unwrap();
    for i in 0..2000u64 {
        if matches!(c.publish("telemetry", "s0", "telemetry.v1", i, json!({"items": []})).await, Err(ClientError::Closed | ClientError::Refused { code: ErrorCode::Revoked, .. })) {
            break;
        }
    }
    assert!(rig.rows.stops.lock().unwrap().is_empty(), "no stop was requested");
    let rows = rig.rows.rows.lock().unwrap();
    let denied: Vec<_> = rows
        .iter()
        .filter(|a| a.action == "cogbus.stop" && matches!(&a.outcome, Outcome::Refused { code, .. } if code == "denied"))
        .collect();
    assert_eq!(denied.len(), 1, "the denial is in the activity log");
    // It was still stopped on the bus.
    assert!(rig.broker.instance("radar-1").is_none());
}

#[tokio::test]
async fn refusals_other_than_rate_also_ask_for_the_stop() {
    let rig = rig(false, 5).await;
    let c = Client::connect(rig.server.path(), &token('r')).await.unwrap();
    for _ in 0..30 {
        let r = c.publish("nope", "s0", "telemetry.v1", 1, json!({})).await;
        if matches!(r, Err(ClientError::Closed | ClientError::Refused { code: ErrorCode::Revoked, .. })) {
            break;
        }
        assert!(matches!(&r, Err(ClientError::Refused { code: ErrorCode::Undeclared, .. })) || r.is_err(), "{r:?}");
    }
    let stops = rig.rows.stops.lock().unwrap().clone();
    assert_eq!(stops.len(), 1);
    assert!(stops[0].1.contains("undeclared"), "{}", stops[0].1);
}
