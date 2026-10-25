//! ADR-122 B3 on a real socket: refusals the connection carries reach the
//! activity sink, and a cog that keeps getting refused is stopped on the bus
//! (its connection closes, its token stops working).
#![cfg(unix)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use clawft_types::cog_topics::TopicClass;
use serde_json::json;
use weftos_cogbus::server::serve;
use weftos_cogbus::{
    Broker, BrokerConfig, BusActivity, BusHooks, BusSink, Client, ClientError, ErrorCode,
    InstanceSpec, Outcome, OutputSpec, RefusalLimits,
};

const TOKEN: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

#[derive(Default)]
struct Log(Mutex<Vec<BusActivity>>);

impl BusSink for Log {
    fn record(&self, a: &BusActivity) {
        self.0.lock().unwrap().push(a.clone());
    }
}

#[tokio::test]
async fn a_cog_refused_too_often_is_stopped_and_every_refusal_is_logged_once() {
    let tmp = tempfile::tempdir().unwrap();
    let log = Arc::new(Log::default());
    let hooks = BusHooks {
        sink: log.clone(),
        limits: RefusalLimits {
            dedupe_ms: 60_000,
            stop_after: 5,
            window_ms: 60_000,
        },
        ..BusHooks::default()
    };
    let broker = Arc::new(Broker::new(BrokerConfig::new("0".repeat(32))).with_hooks(hooks));
    let spec = InstanceSpec {
        instance_id: "noisy-1".into(),
        cog_id: "noisy".into(),
        project_id: None,
        uid: None,
        outputs: vec![OutputSpec {
            topic: "telemetry".into(),
            schema: "telemetry.v1".into(),
            class: TopicClass::Reading,
        }],
        inputs: vec![],
    };
    broker.register(spec, TOKEN).unwrap();
    let server = serve(broker.clone(), &tmp.path().join("cogbus.sock"))
        .await
        .unwrap();

    let c = Client::connect(server.path(), TOKEN).await.unwrap();
    for _ in 0..4 {
        let r = c
            .publish("detections", "cam-0", "detections.v1", 0, json!({}))
            .await;
        assert!(
            matches!(
                r,
                Err(ClientError::Refused {
                    code: ErrorCode::Undeclared,
                    ..
                })
            ),
            "{r:?}"
        );
    }
    // The fifth refusal stops it: answered, then the connection closes.
    let _ = c
        .publish("detections", "cam-0", "detections.v1", 0, json!({}))
        .await;
    let closed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if c.publish("telemetry", "board", "telemetry.v1", 0, json!({}))
                .await
                .is_err()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(closed.is_ok(), "the connection was not closed");
    assert!(
        Client::connect(server.path(), TOKEN).await.is_err(),
        "the token no longer works"
    );

    let rows = log.0.lock().unwrap().clone();
    let refused: Vec<_> = rows
        .iter()
        .filter(|a| a.topic == "detections/cam-0")
        .collect();
    assert_eq!(
        refused.len(),
        1,
        "five identical refusals are one record: {rows:?}"
    );
    assert_eq!(refused[0].action, "cogbus.publish");
    let stop = rows
        .iter()
        .find(|a| a.action == "cogbus.stop")
        .expect("stop logged");
    assert_eq!(stop.outcome, Outcome::Stopped { refusals: 5 });
    // The reconnect with the dead token is logged too, as an unknown token.
    let hello = rows
        .iter()
        .find(|a| a.action == "cogbus.hello")
        .expect("hello refusal logged");
    assert_eq!(hello.instance, "-");
    assert!(
        broker
            .recent_refusals()
            .iter()
            .any(|a| a.action == "cogbus.stop")
    );
}
