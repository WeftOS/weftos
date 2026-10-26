//! Idle connections and per-connection / per-instance caps (ADR-122 gap 4).
#![cfg(unix)]

use std::sync::Arc;
use std::time::Duration;

use clawft_types::cog_topics::{InputPolicy, TopicClass};
use serde_json::json;
use tokio::net::UnixStream;
use weftos_cogbus::proto::{read_frame, write_frame};
use weftos_cogbus::server::{ServerHandle, serve};
use weftos_cogbus::{Broker, BrokerConfig, Client, ClientError, ErrorCode, Event, InputSpec, InstanceSpec, OutputSpec, Request};

fn token(c: char) -> String {
    std::iter::repeat_n(c, 64).collect()
}

fn refused<T: std::fmt::Debug>(r: Result<T, ClientError>) -> ErrorCode {
    match r {
        Err(ClientError::Refused { code, .. }) => code,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

struct Rig {
    broker: Arc<Broker>,
    server: ServerHandle,
    _tmp: tempfile::TempDir,
}

async fn rig(tune: impl FnOnce(&mut BrokerConfig)) -> Rig {
    let tmp = tempfile::tempdir().unwrap();
    let mut cfg = BrokerConfig::new("0".repeat(32));
    tune(&mut cfg);
    let broker = Arc::new(Broker::new(cfg));
    let server = serve(broker.clone(), &tmp.path().join("cogbus.sock")).await.unwrap();
    broker
        .register(
            InstanceSpec {
                instance_id: "cog".into(),
                cog_id: "cog".into(),
                project_id: None,
                uid: None,
                outputs: vec![OutputSpec { topic: "telemetry".into(), schema: "telemetry.v1".into(), class: TopicClass::Reading }],
                inputs: vec![InputSpec { topic: "telemetry".into(), schema: "telemetry.v1".into(), policy: InputPolicy::DropOldest }],
            },
            &token('c'),
        )
        .unwrap();
    Rig { broker, server, _tmp: tmp }
}

fn quick(c: &mut BrokerConfig) {
    c.idle_ping_ms = 120;
    c.pong_timeout_ms = 120;
}

async fn say(s: &mut UnixStream, req: &Request) {
    write_frame(s, req).await.unwrap();
}

async fn next(s: &mut UnixStream) -> Option<Event> {
    let f = tokio::time::timeout(Duration::from_secs(3), read_frame(s)).await.expect("a frame or EOF").unwrap()?;
    Some(serde_json::from_slice(&f).unwrap())
}

async fn raw_hello(rig: &Rig) -> UnixStream {
    let mut s = UnixStream::connect(rig.server.path()).await.unwrap();
    say(&mut s, &Request::Hello { id: 1, token: token('c') }).await;
    assert!(matches!(next(&mut s).await, Some(Event::Ok { .. })));
    s
}

#[tokio::test]
async fn a_silent_connection_is_pinged_and_closed_without_a_pong() {
    let rig = rig(quick).await;
    let mut s = raw_hello(&rig).await;
    assert!(matches!(next(&mut s).await, Some(Event::Ping)), "pinged after the silence");
    match next(&mut s).await {
        Some(Event::Error { code: ErrorCode::Timeout, message, .. }) => assert!(message.contains("idle"), "{message}"),
        other => panic!("expected the idle close, got {other:?}"),
    }
    assert!(next(&mut s).await.is_none(), "closed");
    // The connection slot is free again and the close is in the refusal log.
    assert!(rig.broker.recent_refusals().iter().any(|a| a.instance == "cog"));
    Client::connect(rig.server.path(), &token('c')).await.unwrap();
}

#[tokio::test]
async fn a_pong_or_any_frame_keeps_the_connection() {
    let rig = rig(quick).await;
    let mut s = raw_hello(&rig).await;
    for round in 0..4 {
        assert!(matches!(next(&mut s).await, Some(Event::Ping)), "round {round}");
        if round % 2 == 0 {
            say(&mut s, &Request::Pong { id: 0 }).await;
        } else {
            // Any frame is proof of life: a list instead of a pong.
            say(&mut s, &Request::List { id: 9 }).await;
            assert!(matches!(next(&mut s).await, Some(Event::Ok { id: 9, .. })));
        }
    }
}

#[tokio::test]
async fn the_client_library_answers_pings_by_itself() {
    let rig = rig(quick).await;
    let c = Client::connect(rig.server.path(), &token('c')).await.unwrap();
    let mut feed = c.subscribe("telemetry/*", "telemetry.v1").await.unwrap();
    // Five silent periods: a subscriber with nothing to say stays connected.
    tokio::time::sleep(Duration::from_millis(1300)).await;
    c.publish("telemetry", "s0", "telemetry.v1", 1, json!({"items": []})).await.unwrap();
    let d = tokio::time::timeout(Duration::from_secs(2), feed.recv()).await.unwrap().unwrap();
    assert_eq!(d.topic, "telemetry/s0");
}

#[tokio::test]
async fn a_connection_holds_a_bounded_number_of_subscriptions() {
    let rig = rig(|c| c.max_subs_per_conn = 3).await;
    let a = Client::connect(rig.server.path(), &token('c')).await.unwrap();
    let mut subs = Vec::new();
    for i in 0..3 {
        subs.push(a.subscribe(&format!("telemetry/s{i}"), "telemetry.v1").await.unwrap());
    }
    assert_eq!(refused(a.subscribe("telemetry/s3", "telemetry.v1").await.map(|_| ())), ErrorCode::Busy);
    // Another connection of the same instance has its own allowance.
    let b = Client::connect(rig.server.path(), &token('c')).await.unwrap();
    b.subscribe("telemetry/s9", "telemetry.v1").await.unwrap();
    // Dropping one frees a slot.
    a.unsubscribe(subs.remove(0).id).await.unwrap();
    a.subscribe("telemetry/s3", "telemetry.v1").await.unwrap();
}

#[tokio::test]
async fn an_instance_holds_a_bounded_number_of_connections() {
    let rig = rig(|c| c.max_conns_per_instance = 2).await;
    let a = Client::connect(rig.server.path(), &token('c')).await.unwrap();
    let _b = Client::connect(rig.server.path(), &token('c')).await.unwrap();
    assert_eq!(refused(Client::connect(rig.server.path(), &token('c')).await), ErrorCode::Busy);
    assert!(
        rig.broker.recent_refusals().iter().any(|r| r.instance == "cog"),
        "the refusal is attributed to the instance"
    );
    // Closing one frees its slot.
    drop(a);
    let mut ok = false;
    for _ in 0..50 {
        if Client::connect(rig.server.path(), &token('c')).await.is_ok() {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    assert!(ok, "a slot was freed by the close");
}

#[tokio::test]
async fn reregistering_an_instance_starts_with_no_connections_counted() {
    let rig = rig(|c| c.max_conns_per_instance = 1).await;
    let _a = Client::connect(rig.server.path(), &token('c')).await.unwrap();
    assert_eq!(refused(Client::connect(rig.server.path(), &token('c')).await), ErrorCode::Busy);
    let spec = rig.broker.instance("cog").unwrap();
    rig.broker.revoke("cog");
    rig.broker.register(spec, &token('c')).unwrap();
    Client::connect(rig.server.path(), &token('c')).await.expect("a fresh registration has a fresh allowance");
}
