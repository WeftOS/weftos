//! Streams under B3 governance: opens and subscribes ask the policy as
//! `cogbus.publish` / `cogbus.subscribe`, grants open foreign rings, and
//! allowed actions, refusals and `block` demotions reach the activity sink.
#![cfg(unix)]

mod common;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use clawft_types::cog_topics::InputPolicy;
use common::*;
use weftos_cogbus::server::serve;
use weftos_cogbus::stream::StreamOptions;
use weftos_cogbus::{
    Broker, BrokerConfig, BusActivity, BusHooks, BusPolicy, BusRequest, BusSink, ErrorCode, Grant,
    InstanceSpec, Outcome,
};

#[derive(Default)]
struct Log(Mutex<Vec<BusActivity>>);

impl BusSink for Log {
    fn record(&self, a: &BusActivity) {
        self.0.lock().unwrap().push(a.clone());
    }
}

impl Log {
    fn has(&self, action: &str, instance: &str, f: impl Fn(&Outcome) -> bool) -> bool {
        let g = self.0.lock().unwrap();
        g.iter()
            .any(|a| a.action == action && a.instance == instance && f(&a.outcome))
    }
}

/// Denies `deny` (instance, action); grants project `other` to `asr`.
struct Rules {
    deny: Option<(&'static str, &'static str)>,
}

impl BusPolicy for Rules {
    fn decide(&self, r: &BusRequest<'_>) -> Result<(), String> {
        match self.deny {
            Some((i, a)) if r.instance_id == i && r.action.as_str() == a => Err("test rule".into()),
            _ => Ok(()),
        }
    }

    fn grants(&self, r: &BusRequest<'_>) -> Vec<Grant> {
        (r.instance_id == "asr")
            .then(|| Grant {
                project: "other".into(),
                source: None,
            })
            .into_iter()
            .collect()
    }
}

async fn governed(deny: Option<(&'static str, &'static str)>) -> (Rig, Arc<Log>) {
    let log = Arc::new(Log::default());
    let hooks = BusHooks {
        policy: Arc::new(Rules { deny }),
        sink: log.clone(),
        ..BusHooks::default()
    };
    let tmp = tempfile::tempdir().unwrap();
    let broker = Arc::new(Broker::new(BrokerConfig::new("0".repeat(32))).with_hooks(hooks));
    let server = serve(broker.clone(), &tmp.path().join("cogbus.sock"))
        .await
        .unwrap();
    (
        Rig {
            broker,
            server,
            tmp,
        },
        log,
    )
}

fn foreign_mic(rig: &Rig) {
    let spec = InstanceSpec {
        instance_id: "mic-x".into(),
        cog_id: "mic-x".into(),
        project_id: Some("other".into()),
        uid: None,
        outputs: vec![pcm_out()],
        inputs: vec![],
    };
    rig.broker.register(spec, &token('x')).unwrap();
}

#[tokio::test]
async fn governance_gates_stream_opens_and_subscribes_and_logs_them() {
    let (rig, log) = governed(Some(("asr", "cogbus.subscribe"))).await;
    rig.cog("mic", 'a', vec![pcm_out()], vec![]);
    rig.cog("asr", 'b', vec![], vec![pcm_in(InputPolicy::DropOldest)]);
    let (a, b) = (rig.connect('a').await, rig.connect('b').await);
    let _w = a
        .stream_writer(KIND, SOURCE, PCM, StreamOptions::new(64))
        .await
        .unwrap();
    assert!(log.has("cogbus.publish", "mic", |o| *o == Outcome::Ok));
    assert_eq!(
        refused(b.subscribe_stream(TOPIC, PCM).await),
        ErrorCode::Denied
    );
    assert!(log.has(
        "cogbus.subscribe",
        "asr",
        |o| matches!(o, Outcome::Refused { code, .. } if code == "denied")
    ));
    // A manifest refusal is logged too.
    assert_eq!(
        refused(b.subscribe_stream(TOPIC, "iq.cs16.v1").await),
        ErrorCode::SchemaMismatch
    );
    assert!(log.has("cogbus.stream_subscribe", "asr", |o| matches!(
        o,
        Outcome::Refused { .. }
    )));

    let (rig, log) = governed(Some(("mic", "cogbus.publish"))).await;
    rig.cog("mic", 'a', vec![pcm_out()], vec![]);
    let a = rig.connect('a').await;
    assert_eq!(
        refused(
            a.stream_writer(KIND, SOURCE, PCM, StreamOptions::new(64))
                .await
        ),
        ErrorCode::Denied
    );
    assert!(log.has("cogbus.publish", "mic", |o| matches!(
        o,
        Outcome::Refused { .. }
    )));
}

#[tokio::test]
async fn a_grant_opens_another_projects_ring() {
    let (rig, log) = governed(None).await;
    foreign_mic(&rig);
    rig.cog("asr", 'b', vec![], vec![pcm_in(InputPolicy::DropOldest)]);
    rig.cog("vu", 'c', vec![], vec![pcm_in(InputPolicy::DropOldest)]);
    let x = rig.connect('x').await;
    let mut w = x
        .stream_writer(KIND, SOURCE, PCM, StreamOptions::new(64))
        .await
        .unwrap();
    let (b, c) = (rig.connect('b').await, rig.connect('c').await);
    assert_eq!(
        refused(c.subscribe_stream(TOPIC, PCM).await),
        ErrorCode::CrossProject
    );
    let mut r = b.subscribe_stream(TOPIC, PCM).await.unwrap();
    w.write(1, b"g").unwrap();
    assert_eq!(next(&mut r).await.data, b"g");
    assert!(log.has("cogbus.subscribe", "asr", |o| *o == Outcome::Ok));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_block_demotion_is_in_the_activity_log() {
    let (rig, log) = governed(None).await;
    rig.cog("mic", 'a', vec![pcm_out()], vec![]);
    rig.cog("rec", 'b', vec![], vec![pcm_in(InputPolicy::Block)]);
    let (a, b) = (rig.connect('a').await, rig.connect('b').await);
    let opts = StreamOptions {
        ring_bytes: Some(128 + 4 * 64),
        ..StreamOptions::new(8)
    };
    let mut w = a.stream_writer(KIND, SOURCE, PCM, opts).await.unwrap();
    let _r = b.subscribe_stream(TOPIC, PCM).await.unwrap();
    let t = Instant::now();
    while w.block_readers() != 1 {
        assert!(t.elapsed() < Duration::from_secs(5));
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    for i in 1..=4 {
        w.write(i, b"d").unwrap();
    }
    assert_eq!(w.demotions(), 1);
    let t = Instant::now();
    while !log.has("cogbus.stream_demote", "rec", |o| {
        matches!(o, Outcome::Demoted { .. })
    }) {
        assert!(
            t.elapsed() < Duration::from_secs(5),
            "demotion never logged"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}
