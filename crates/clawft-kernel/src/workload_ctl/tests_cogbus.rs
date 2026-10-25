//! A cog placed through the real placement path is registered with the
//! node's cogbus (ADR-122 B1) with its manifest topics and project, gets
//! `WEFTOS_COGBUS` / `WEFTOS_COGBUS_TOKEN`, and is revoked at stop and
//! unload and registered again at start. A route the broker cannot reach
//! gets neither.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clawft_types::cog_topics::{InputPolicy, TopicClass};
use clawft_types::placement::TrustTier;
use ed25519_dalek::SigningKey;

use super::host_cogbus::{CogbusAttach, CogbusRegistrar, CogbusRegistration};
use super::msg::method;
use super::test_support::*;
use super::transport::MeshConnector;
use crate::workload_runtime::RunMode;

const PROJECT: &str = "01J9ZXW0PRJCTAAAAAAAAAAAAA";

const COG: &str = r#"#!/bin/sh
echo "$WEFTOS_COGBUS|$WEFTOS_COGBUS_TOKEN|$COGNITUM_COG_TOKEN" > "$COGNITUM_COG_DATA_DIR/bus"
exec sleep 30
"#;

const TOPICS: &str = r#"
[[outputs]]
topic = "telemetry"
schema = "telemetry.v1"
class = "reading"

[[inputs]]
topic = "detections"
schema = "detections.v1"
policy = "sample"
"#;

#[derive(Default)]
struct FakeBus {
    reachable: bool,
    log: Mutex<Vec<CogbusRegistration>>,
    live: Mutex<BTreeSet<String>>,
}

impl CogbusRegistrar for FakeBus {
    fn attach(&self, route: &str) -> Option<CogbusAttach> {
        (self.reachable && route == "native")
            .then(|| CogbusAttach { socket: "/run/test/cogbus.sock".into(), uid: Some(4242) })
    }
    fn register(&self, r: &CogbusRegistration) -> Result<(), String> {
        if !self.live.lock().unwrap().insert(r.instance_id.clone()) {
            return Err("already registered".into());
        }
        self.log.lock().unwrap().push(r.clone());
        Ok(())
    }
    fn revoke(&self, id: &str) {
        self.live.lock().unwrap().remove(id);
    }
}

async fn place(bus: Arc<FakeBus>, seed: u8, port: u16) -> (HostNode, super::plane::PlacementControlPlane, String, tempfile::TempDir) {
    let ctl = SigningKey::from_bytes(&[seed + 1; 32]);
    let host = host_node_native_only(seed, board_caps("pi5"), true, &ctl);
    assert!(host.svc.set_cogbus(bus));
    let tmp = tempfile::tempdir().unwrap();
    let pkg = package_toml(tmp.path(), "bus-cog", &[(arch(), COG)], None, TOPICS);
    let conn = Arc::new(MeshConnector::new(false));
    let addr = conn.register_local("host", host.svc.clone());
    let (plane, _chain) = controller(&ctl, conn);
    plane.add_target(&addr, TrustTier::Paired).await.unwrap();
    let mut order = mode_order(&pkg, RunMode::Listener, None);
    order.config.csi_port = port;
    order.project_id = Some(PROJECT.into());
    let report = plane.place(&order).await.unwrap();
    let iid = report.placed.expect("placed").instance_id;
    (host, plane, iid, tmp)
}

async fn bus_file(host: &HostNode, iid: &str) -> String {
    let f = host._tmp.path().join("instances").join(iid).join("data/bus");
    for _ in 0..100 {
        if let Ok(s) = std::fs::read_to_string(&f)
            && s.ends_with('\n')
        {
            return s.trim().to_string();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("cog never wrote {}", f.display());
}

#[tokio::test]
async fn a_placed_cog_is_registered_with_its_topics_and_handed_the_socket() {
    let bus = Arc::new(FakeBus { reachable: true, ..Default::default() });
    let (host, plane, iid, _tmp) = place(bus.clone(), 61, 15061).await;

    let r = bus.log.lock().unwrap()[0].clone();
    assert_eq!((r.instance_id.as_str(), r.cog_id.as_str()), (iid.as_str(), "bus-cog"));
    assert_eq!((r.project_id.as_deref(), r.uid), (Some(PROJECT), Some(4242)));
    assert_eq!(r.outputs.len(), 1);
    assert_eq!((r.outputs[0].topic.as_str(), r.outputs[0].schema.as_str(), r.outputs[0].class),
        ("telemetry", "telemetry.v1", TopicClass::Reading));
    assert_eq!((r.inputs[0].topic.as_str(), r.inputs[0].policy), ("detections", InputPolicy::Sample));

    let env = bus_file(&host, &iid).await;
    let parts: Vec<&str> = env.split('|').collect();
    assert_eq!(parts[0], "/run/test/cogbus.sock");
    assert_eq!(parts[1], r.token.expose(), "the registered token is the one handed over");
    assert_eq!(parts[1], parts[2], "the instance token, reused for the broker");

    plane.instance(method::STOP, &iid).await.unwrap();
    assert!(!bus.live.lock().unwrap().contains(&iid), "stop revokes");
    plane.instance(method::START, &iid).await.unwrap();
    assert!(bus.live.lock().unwrap().contains(&iid), "start registers again");
    plane.instance(method::STOP, &iid).await.unwrap();
    plane.instance(method::UNLOAD, &iid).await.unwrap();
    assert!(bus.live.lock().unwrap().is_empty(), "unload revokes");
    assert_eq!(bus.log.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn a_route_the_broker_cannot_reach_gets_no_socket_and_no_registration() {
    let bus = Arc::new(FakeBus::default());
    let (host, plane, iid, _tmp) = place(bus.clone(), 63, 15063).await;
    let env = bus_file(&host, &iid).await;
    assert!(env.starts_with("||"), "no WEFTOS_COGBUS*: {env}");
    assert!(bus.log.lock().unwrap().is_empty());
    plane.instance(method::UNLOAD, &iid).await.unwrap();
}
