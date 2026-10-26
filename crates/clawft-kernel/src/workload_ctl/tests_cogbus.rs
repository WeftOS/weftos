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

use super::host_cogbus::{CogbusAttach, CogbusRegistrar, CogbusRegistration, Isolation};
use super::msg::method;
use super::test_support::*;
use super::transport::MeshConnector;
use crate::workload_runtime::{CogIdentity, RunMode};

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

fn me() -> (u32, u32) {
    // SAFETY: neither call can fail.
    unsafe { (libc::geteuid(), libc::getegid()) }
}

#[derive(Default)]
struct FakeBus {
    reachable: bool,
    /// Hand each instance an identity (this process's own uid and gid, so
    /// staging its data directory needs no privilege; the tests only load).
    own_uid: bool,
    /// `require_cog_uid` is on.
    refuse_shared: bool,
    /// Refuse every registration.
    fail_register: bool,
    log: Mutex<Vec<CogbusRegistration>>,
    live: Mutex<BTreeSet<String>>,
    released: Mutex<Vec<String>>,
    abandoned: Mutex<Vec<u32>>,
}

impl CogbusRegistrar for FakeBus {
    fn attach(&self, route: &str) -> Option<CogbusAttach> {
        if !(self.reachable && route == "native") {
            return None;
        }
        let mut a = CogbusAttach::shared("/run/test/cogbus.sock", 4242);
        a.refuse_shared = self.refuse_shared;
        a.note = Some("test bus".into());
        if self.own_uid {
            let (uid, gid) = me();
            a.uid = Some(uid);
            a.identity = Some(CogIdentity { uid, gid, groups: vec![gid] });
            a.isolation = Isolation::PerCogUid;
            a.note = None;
        }
        Some(a)
    }
    fn release(&self, id: &str) {
        self.released.lock().unwrap().push(id.to_string());
    }
    fn abandon(&self, uid: u32) {
        self.abandoned.lock().unwrap().push(uid);
    }
    fn register(&self, r: &CogbusRegistration) -> Result<(), String> {
        if self.fail_register {
            return Err("registration refused".into());
        }
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
    let (host, plane, report, tmp) = place_order(bus, seed, port, true).await;
    (host, plane, report.placed.expect("placed").instance_id, tmp)
}

/// Place, with or without starting; the report says whether it was accepted.
async fn place_order(
    bus: Arc<FakeBus>,
    seed: u8,
    port: u16,
    start: bool,
) -> (HostNode, super::plane::PlacementControlPlane, super::plane_place::PlaceReport, tempfile::TempDir) {
    let ctl = SigningKey::from_bytes(&[seed + 1; 32]);
    let host = host_node_native_only(seed, board_caps("pi5"), true, &ctl);
    assert!(host.svc.set_cogbus(bus));
    let tmp = tempfile::tempdir().unwrap();
    let pkg = package_toml(tmp.path(), "bus-cog", &[(arch(), COG)], None, TOPICS);
    let conn = Arc::new(MeshConnector::new(false));
    let addr = conn.register_local("host", host.svc.clone());
    let (plane, _chain) = controller(&ctl, conn);
    plane.add_target(&addr, TrustTier::Paired).await.unwrap();
    let base = mode_order(&pkg, RunMode::Listener, None);
    let mut order = super::plane_place::PlaceOrder {
        project_id: Some(PROJECT.into()),
        start,
        ..base
    };
    order.config.csi_port = port;
    let report = plane.place(&order).await.unwrap();
    (host, plane, report, tmp)
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
    let ctl = SigningKey::from_bytes(&[62; 32]);
    assert_eq!(r.controller_node, crate::node_id_from_pubkey(&ctl.verifying_key().to_bytes()), "the placing controller");
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

#[tokio::test]
async fn a_cog_with_its_own_uid_registers_it_and_unload_frees_it() {
    let bus = Arc::new(FakeBus { reachable: true, own_uid: true, ..Default::default() });
    // Loaded, not started: this test process cannot switch to uid 60010.
    let (_host, plane, report, _tmp) = place_order(bus.clone(), 67, 15067, false).await;
    let iid = report.placed.expect("placed").instance_id;

    let r = bus.log.lock().unwrap()[0].clone();
    assert_eq!((r.uid, r.isolation), (Some(me().0), Isolation::PerCogUid), "the broker is told the cog's own uid");
    assert!(bus.abandoned.lock().unwrap().is_empty(), "a registered identity is not given back");

    plane.instance(method::UNLOAD, &iid).await.unwrap();
    assert_eq!(*bus.released.lock().unwrap(), vec![iid], "unload frees the uid");
}

#[tokio::test]
async fn a_stop_keeps_the_uid_reserved() {
    let bus = Arc::new(FakeBus { reachable: true, own_uid: true, ..Default::default() });
    let (_host, plane, report, _tmp) = place_order(bus.clone(), 69, 15069, false).await;
    let iid = report.placed.expect("placed").instance_id;
    // The never-started instance refuses the stop itself, after the broker
    // has already been told to stop accepting it.
    let _ = plane.instance(method::STOP, &iid).await;
    assert!(!bus.live.lock().unwrap().contains(&iid));
    assert!(bus.released.lock().unwrap().is_empty(), "a stopped instance may start again as the same uid");
    plane.instance(method::UNLOAD, &iid).await.unwrap();
}

#[tokio::test]
async fn a_registration_that_fails_gives_the_identity_back() {
    let bus = Arc::new(FakeBus { reachable: true, own_uid: true, fail_register: true, ..Default::default() });
    let (_host, _plane, report, _tmp) = place_order(bus.clone(), 71, 15071, false).await;
    assert!(report.placed.is_none(), "{}", report.explain);
    assert_eq!(*bus.abandoned.lock().unwrap(), vec![me().0]);
    assert!(bus.released.lock().unwrap().is_empty());
}

#[tokio::test]
async fn require_cog_uid_refuses_a_bus_cog_that_would_share_a_uid() {
    let bus = Arc::new(FakeBus { reachable: true, refuse_shared: true, ..Default::default() });
    let (_host, _plane, report, _tmp) = place_order(bus.clone(), 73, 15073, true).await;
    assert!(report.placed.is_none(), "{}", report.explain);
    let a = &report.attempts[0];
    assert_eq!(a.code.as_deref(), Some("admission"));
    assert!(a.reason.as_deref().unwrap().contains("require_cog_uid"), "{a:?}");
    assert!(bus.log.lock().unwrap().is_empty(), "nothing registered");
}

#[tokio::test]
async fn require_cog_uid_lets_an_isolated_cog_through() {
    let bus = Arc::new(FakeBus { reachable: true, own_uid: true, refuse_shared: true, ..Default::default() });
    let (_host, plane, report, _tmp) = place_order(bus, 75, 15075, false).await;
    let iid = report.placed.expect("an isolated cog is not refused").instance_id;
    plane.instance(method::UNLOAD, &iid).await.unwrap();
}

#[tokio::test]
async fn the_broker_can_stop_a_running_cog_and_it_stays_stopped_with_the_cause_chained() {
    let bus = Arc::new(FakeBus { reachable: true, ..Default::default() });
    let (host, plane, iid, _tmp) = place(bus.clone(), 77, 15077).await;
    assert!(bus.live.lock().unwrap().contains(&iid));
    bus_file(&host, &iid).await;

    let s = host.svc.stop_for_bus(&iid, "21 refusals in 60 s, last rate_limited").await.unwrap().expect("a live instance");
    assert_eq!(s.to, super::lifecycle::LifecycleState::Stopped);
    assert!(s.reason.starts_with("cogbus: "), "{}", s.reason);
    assert!(!bus.live.lock().unwrap().contains(&iid), "revoked on the bus");
    assert_eq!(host.svc.lifecycle_of(&iid).await, Some(super::lifecycle::LifecycleState::Stopped));

    // Chained: the host's gated workload.stop carries the cause, and the
    // lifecycle event says cogbus.
    let events = host.chain.tail(50);
    // (The gate chains its own workload.stop decision first; the runtime outcome carries the cause.)
    let stop = events
        .iter()
        .filter(|e| e.kind == crate::chain::EVENT_KIND_WORKLOAD_STOP)
        .find(|e| e.payload.as_ref().is_some_and(|p| p.get("cause").is_some()))
        .expect("workload.stop with a cause chained");
    let payload = stop.payload.clone().unwrap_or_default();
    assert_eq!(payload["cause"]["source"], "cogbus");
    assert!(payload["cause"]["reason"].as_str().unwrap().contains("rate_limited"));
    let life = events.iter().rfind(|e| e.kind == crate::chain::EVENT_KIND_WORKLOAD_LIFECYCLE).expect("lifecycle chained");
    assert_eq!(life.payload.clone().unwrap_or_default()["phase"], "cogbus");

    // Not an instance this host runs, or one already stopped: nothing to do.
    assert!(host.svc.stop_for_bus("nope", "x").await.unwrap().is_none());
    assert!(host.svc.stop_for_bus(&iid, "again").await.unwrap().is_none());
    // The supervisor does not bring it back (an operator start would).
    host.svc.supervise(u64::MAX / 2).await;
    assert_eq!(host.svc.lifecycle_of(&iid).await, Some(super::lifecycle::LifecycleState::Stopped));
    plane.instance(method::UNLOAD, &iid).await.unwrap();
}
