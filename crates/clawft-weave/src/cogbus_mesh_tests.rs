//! Mesh topics through the daemon's pieces: the exposure registry as the
//! hub's gate, and two kernel mesh runtimes carrying raw 0x12/0x13 frames.

use std::sync::Arc;
use std::time::Duration;

use clawft_kernel::exposure::{Audience, ExposureRegistry, ExposureState};
use clawft_kernel::mesh_admit::PeerClass;
use clawft_kernel::mesh_delivery::PeerCtx;
use clawft_kernel::mesh_runtime::MeshRuntime;
use clawft_types::cog_topics::{InputPolicy, MeshLimits, TopicClass};
use serde_json::json;
use weftos_cogbus::mesh::{MeshAudience, PeerInfo, TopicExposures};
use weftos_cogbus::{Broker, BrokerConfig, InputSpec, InstanceSpec, OutputSpec, Session, TopicInfo};

use super::{PairedFn, RegistryExposures, build};
use crate::cogbus_activity::topic_exposure;

const ID: &str = "topic.telemetry/venq-board";

fn info() -> TopicInfo {
    TopicInfo {
        topic: "telemetry/venq-board".into(),
        kind: "telemetry".into(),
        source_id: "venq-board".into(),
        schema: "telemetry.v1".into(),
        class: TopicClass::Reading,
        instance: "board".into(),
        cog_id: "board-telemetry".into(),
        project_id: None,
    }
}

fn paired(yes: bool) -> PairedFn {
    Arc::new(move |_: &str| yes)
}

#[test]
fn the_registry_decides_what_each_peer_is_served() {
    let reg = Arc::new(ExposureRegistry::new("node-a"));
    reg.register(topic_exposure(&info()));
    let ex = RegistryExposures::new(Some(reg.clone()), paired(false));
    assert!(ex.serve(ID, "node-b").unwrap_err().contains("local"), "registered local: never served");
    reg.set_audience(ID, Audience::Paired, "cli").unwrap();
    assert!(ex.serve(ID, "node-b").unwrap_err().contains("not paired"));
    let ex = RegistryExposures::new(Some(reg.clone()), paired(true));
    assert_eq!(ex.serve(ID, "node-b").unwrap().audience, MeshAudience::Paired);
    assert!(reg.get(ID).unwrap().announced, "served topics are announced");
    reg.set_audience(ID, Audience::Nodes(vec!["node-b".into()]), "cli").unwrap();
    reg.set_raw(ID, true, "cli").unwrap();
    let v = ex.serve(ID, "node-b").unwrap();
    assert!(v.raw && matches!(v.audience, MeshAudience::Nodes(_)));
    assert!(ex.serve(ID, "node-c").unwrap_err().contains("not in the audience"));
    reg.set_state(ID, ExposureState::Off, "cli").unwrap();
    assert!(ex.serve(ID, "node-b").unwrap_err().contains("switched off"));
    assert!(ex.serve("topic.nope/x", "node-b").is_err());
}

fn verified(id: &str) -> PeerCtx {
    let mut c = PeerCtx::unauthenticated(id.to_string());
    c.node_verified = true;
    c.class = PeerClass::Node;
    c
}

/// Two runtimes joined by channels, each side delivering what it receives
/// with the sender's verified context, as the admitted connection does.
fn link(a: &Arc<MeshRuntime>, a_id: &str, b: &Arc<MeshRuntime>, b_id: &str) {
    let (tx_ab, mut rx_ab) = tokio::sync::mpsc::channel::<Vec<u8>>(256);
    let (tx_ba, mut rx_ba) = tokio::sync::mpsc::channel::<Vec<u8>>(256);
    a.add_peer(b_id.into(), tx_ab.clone());
    b.add_peer(a_id.into(), tx_ba.clone());
    let (rt, back, ctx) = (b.clone(), tx_ba, verified(a_id));
    tokio::spawn(async move {
        while let Some(bytes) = rx_ab.recv().await {
            let _ = rt.handle_incoming_peer(&bytes, back.clone(), Some(&ctx)).await;
        }
    });
    let (rt, back, ctx) = (a.clone(), tx_ab, verified(b_id));
    tokio::spawn(async move {
        while let Some(bytes) = rx_ba.recv().await {
            let _ = rt.handle_incoming_peer(&bytes, back.clone(), Some(&ctx)).await;
        }
    });
}

fn cog(b: &Broker, id: &str, c: char, outputs: Vec<OutputSpec>, inputs: Vec<InputSpec>) -> Session {
    let spec = InstanceSpec { instance_id: id.into(), cog_id: format!("{id}-cog"), project_id: None, uid: None, outputs, inputs };
    let token: String = std::iter::repeat_n(c, 64).collect();
    b.register(spec, &token).unwrap();
    b.authenticate(&token, None).unwrap()
}

#[tokio::test]
async fn a_reading_crosses_two_mesh_runtimes_as_raw_frames() {
    let (rt_a, rt_b) = (Arc::new(MeshRuntime::new("node-a".into())), Arc::new(MeshRuntime::new("node-b".into())));
    link(&rt_a, "node-a", &rt_b, "node-b");
    let (broker_a, broker_b) = (Arc::new(Broker::new(BrokerConfig::new("node-a"))), Arc::new(Broker::new(BrokerConfig::new("node-b"))));
    let reg_a = Arc::new(ExposureRegistry::new("node-a"));
    let reg_b = Arc::new(ExposureRegistry::new("node-b"));
    let hub_a = build(broker_a.clone(), rt_a.clone(), Arc::new(RegistryExposures::new(Some(reg_a.clone()), paired(false))));
    let hub_b = build(broker_b.clone(), rt_b.clone(), Arc::new(RegistryExposures::new(Some(reg_b), paired(false))));
    // node-a: the board cog, its limits, its topic exposed to node-b by name.
    let out = OutputSpec { topic: "telemetry".into(), schema: "telemetry.v1".into(), class: TopicClass::Reading };
    let board = cog(&broker_a, "board", 'b', vec![out], vec![]);
    broker_a.set_mesh_limits("board", MeshLimits::STARTING);
    broker_a.publish(&board, "telemetry", "venq-board", "telemetry.v1", 1, json!({"n": 1})).unwrap();
    reg_a.register(topic_exposure(&info()));
    reg_a.set_audience(ID, Audience::Nodes(vec!["node-b".into()]), "cli").unwrap();
    let to_b: Arc<dyn Fn() -> Vec<PeerInfo> + Send + Sync> = Arc::new(|| vec![PeerInfo { node_id: "node-b".into(), admitted: true }]);
    tokio::spawn(hub_a.clone().run(to_b, Duration::from_millis(50)));
    tokio::spawn(hub_b.clone().run(Arc::new(Vec::new), Duration::from_secs(60)));
    // node-b: a reader subscribes by name once node-a's advert has arrived.
    let input = InputSpec { topic: "telemetry".into(), schema: "telemetry.v1".into(), policy: InputPolicy::DropOldest };
    let mut reader = cog(&broker_b, "reader", 'r', vec![], vec![input]);
    let name = "weftos://home/sensors/venq-board/topics/telemetry";
    let mut subscribed = false;
    for _ in 0..100 {
        if broker_b.subscribe_with(&mut reader, name, "telemetry.v1", false).is_ok() {
            subscribed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(subscribed, "node-b never saw node-a's advert");
    let mut got = Vec::new();
    for n in 2..200 {
        broker_a.publish(&board, "telemetry", "venq-board", "telemetry.v1", n, json!({"n": n})).unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        got.extend(reader.drain());
        if !got.is_empty() {
            break;
        }
    }
    assert!(!got.is_empty(), "no reading arrived");
    assert!(got.iter().all(|d| d.node_id == "node-a" && d.topic == "telemetry/venq-board"), "{got:?}");
    let log = reg_a.activity().query(Some(ID), 10);
    assert!(log.iter().any(|r| r.caller_node == "node-b" && r.outcome == "ok"), "{log:?}");
    // Off on node-a ends it at once.
    reg_a.set_state(ID, ExposureState::Off, "cli").unwrap();
    hub_a.withdraw(ID);
    assert!(hub_a.served().is_empty());
}
