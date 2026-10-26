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

/// Close without reading the answer; the broker's reply may or may not fail.
fn rst(s: tokio::net::UnixStream) {
    drop(s);
}

#[tokio::test]
async fn a_connection_that_vanishes_before_the_hello_reply_gives_its_slot_back() {
    let rig = rig(|c| c.max_conns_per_instance = 2).await;
    // Say hello and disappear without reading the answer, many times over.
    for _ in 0..40 {
        let mut s = UnixStream::connect(rig.server.path()).await.unwrap();
        say(&mut s, &Request::Hello { id: 1, token: token('c') }).await;
        rst(s);
    }
    // Every such connection ended, however its reply went: both slots are free.
    let mut ok = false;
    for _ in 0..100 {
        let a = Client::connect(rig.server.path(), &token('c')).await;
        let b = Client::connect(rig.server.path(), &token('c')).await;
        if a.is_ok() && b.is_ok() {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert!(ok, "slots leaked: {:?}", rig.broker.recent_refusals().len());
}

#[tokio::test]
async fn a_session_dropped_on_any_path_releases_its_slot() {
    // The slot belongs to the Session, not to a call the server must remember.
    let rig = rig(|c| c.max_conns_per_instance = 1).await;
    let s = rig.broker.authenticate(&token('c'), None).unwrap();
    assert_eq!(refused(rig.broker.authenticate(&token('c'), None).map(|_| ()).map_err(|e| ClientError::Refused { code: e.code, message: e.message })), ErrorCode::Busy);
    drop(s);
    rig.broker.authenticate(&token('c'), None).expect("the dropped session's slot is free");
}

#[tokio::test]
async fn a_silent_unauthenticated_connection_is_closed_at_the_hello_deadline() {
    let rig = rig(|c| c.hello_timeout_ms = 150).await;
    let mut s = UnixStream::connect(rig.server.path()).await.unwrap();
    let t0 = std::time::Instant::now();
    assert!(next(&mut s).await.is_none(), "closed without a word");
    assert!(t0.elapsed() < Duration::from_secs(2), "{:?}", t0.elapsed());
}

#[tokio::test]
async fn one_uid_cannot_hold_more_than_its_share_of_unauthenticated_connections() {
    let rig = rig(|c| {
        c.max_prehello_per_uid = 3;
        c.hello_timeout_ms = 30_000;
    })
    .await;
    let mut idle = Vec::new();
    for _ in 0..3 {
        idle.push(UnixStream::connect(rig.server.path()).await.unwrap());
    }
    // The fourth is closed at once, whatever the deadline.
    let mut over = UnixStream::connect(rig.server.path()).await.unwrap();
    assert!(next(&mut over).await.is_none(), "refused before any hello");
    // Saying hello frees a slot: the idle ones are still waiting, so a real
    // cog is refused too while this uid is at its share ...
    let mut again = UnixStream::connect(rig.server.path()).await.unwrap();
    assert!(next(&mut again).await.is_none());
    // ... until one of them goes.
    drop(idle.pop());
    let mut ok = false;
    for _ in 0..100 {
        if Client::connect(rig.server.path(), &token('c')).await.is_ok() {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert!(ok);
}

#[tokio::test]
async fn the_node_caps_total_and_unauthenticated_connections() {
    let rig = rig(|c| {
        c.max_prehello_conns = 2;
        c.hello_timeout_ms = 30_000;
    })
    .await;
    let _a = UnixStream::connect(rig.server.path()).await.unwrap();
    let _b = UnixStream::connect(rig.server.path()).await.unwrap();
    let mut c = UnixStream::connect(rig.server.path()).await.unwrap();
    assert!(next(&mut c).await.is_none(), "over the node's pre-hello cap");

    let rig = self::rig(|c| c.max_conns = 2).await;
    let a = Client::connect(rig.server.path(), &token('c')).await.unwrap();
    let _b = Client::connect(rig.server.path(), &token('c')).await.unwrap();
    assert!(Client::connect(rig.server.path(), &token('c')).await.is_err(), "over the node's connection cap");
    drop(a);
    let mut ok = false;
    for _ in 0..100 {
        if Client::connect(rig.server.path(), &token('c')).await.is_ok() {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert!(ok, "a closed connection frees its place");
}

#[tokio::test]
async fn readings_and_streams_share_one_subscription_cap() {
    use weftos_cogbus::{InputSpec as In};
    let rig = rig(|c| c.max_subs_per_conn = 2).await;
    rig.broker
        .register(
            InstanceSpec {
                instance_id: "mix".into(),
                cog_id: "mix".into(),
                project_id: None,
                uid: None,
                outputs: vec![],
                inputs: vec![
                    In { topic: "telemetry".into(), schema: "telemetry.v1".into(), policy: InputPolicy::DropOldest },
                    In { topic: "audio.pcm".into(), schema: "pcm.s16le.v1".into(), policy: InputPolicy::DropOldest },
                ],
            },
            &token('m'),
        )
        .unwrap();
    let c = Client::connect(rig.server.path(), &token('m')).await.unwrap();
    let _r = c.subscribe_stream("audio.pcm/mic-0", "pcm.s16le.v1").await.unwrap();
    let _s = c.subscribe_stream("audio.pcm/mic-1", "pcm.s16le.v1").await.unwrap();
    // Two stream readers fill the allowance: a reading subscription is refused too.
    assert_eq!(refused(c.subscribe("telemetry/s0", "telemetry.v1").await.map(|_| ())), ErrorCode::Busy);
}
