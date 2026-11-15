//! The `computer` adapter and its kind's judgment, on an injected clock: a
//! fresh hub heartbeat is healthy, a stale one is not (with a named reason),
//! recovery returns to healthy, and no connection is its own failure.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;

use super::computer::{ComputerRuntime, HeartbeatSource};
use super::types::{InstanceHandle, InstanceState, WorkloadConfig, WorkloadRuntime};
use super::{HostContract, RunMode};
use crate::workload_kind::{
    Health, HealthSample, KIND_COMPUTER, SampleState, WorkloadKind, computer_workload, ComputerKind,
};
use crate::workload_pkg::ManifestEnvelope;

const T0: u64 = 1_000_000;
/// The kind's window: 15 s x 3.
const WINDOW: u64 = 45_000;

#[derive(Default)]
struct FakeHub(Mutex<HashMap<String, u64>>);
impl FakeHub {
    fn hear(&self, desktop: &str, at: u64) {
        self.0.lock().unwrap().insert(desktop.into(), at);
    }
    fn drop_conn(&self, desktop: &str) {
        self.0.lock().unwrap().remove(desktop);
    }
}
impl HeartbeatSource for FakeHub {
    fn last_seen_ms(&self, desktop: &str) -> Option<u64> {
        self.0.lock().unwrap().get(desktop).copied()
    }
}

struct Rig {
    hub: Arc<FakeHub>,
    now: Arc<AtomicU64>,
    rt: ComputerRuntime,
    h: InstanceHandle,
}

fn cfg() -> WorkloadConfig {
    WorkloadConfig {
        mode: RunMode::Listener,
        args: Vec::new(),
        host: HostContract::default_feed(),
        node_id: "n".into(),
    }
}

async fn rig(start: bool) -> Rig {
    let hub = Arc::new(FakeHub::default());
    let now = Arc::new(AtomicU64::new(T0));
    let clock = {
        let now = now.clone();
        Arc::new(move || now.load(Ordering::SeqCst))
    };
    let rt = ComputerRuntime::with_clock(hub.clone(), clock);
    let w = computer_workload(&ManifestEnvelope::new(
        KIND_COMPUTER,
        json!({"desktop": "xpc", "host_kind": "desktop", "capabilities": ["screen.capture"], "access": "own"}),
    ))
    .unwrap();
    let h = rt.load(&w, &cfg()).await.unwrap();
    if start {
        rt.start(&h).await.unwrap();
    }
    Rig { hub, now, rt, h }
}

/// One poll the way the supervisor takes it: status, heartbeat age, kind's judgment.
async fn poll(r: &Rig) -> Health {
    let st = r.rt.status(&r.h).await;
    let sample = HealthSample {
        state: match st.state {
            InstanceState::Loaded => SampleState::Loaded,
            InstanceState::Running => SampleState::Running,
            InstanceState::Exited => SampleState::Exited,
            InstanceState::Degraded => SampleState::Degraded,
            InstanceState::Unknown => SampleState::Unknown,
        },
        exit_code: st.exit_code,
        continuous: true,
        heartbeat_age_ms: r.rt.heartbeat_age_ms(&r.h).await,
    };
    ComputerKind.judge(&sample)
}

#[tokio::test]
async fn a_fresh_heartbeat_is_healthy() {
    let r = rig(true).await;
    r.hub.hear("xpc", T0);
    r.now.store(T0 + 20_000, Ordering::SeqCst);
    assert_eq!(poll(&r).await, Health::Healthy);
    assert_eq!(r.rt.status(&r.h).await.state, InstanceState::Running);
    // Exactly at the window is still fresh.
    r.now.store(T0 + WINDOW, Ordering::SeqCst);
    assert_eq!(poll(&r).await, Health::Healthy);
}

#[tokio::test]
async fn a_stale_heartbeat_is_unhealthy_with_the_named_reason_then_recovers() {
    let r = rig(true).await;
    r.hub.hear("xpc", T0);
    r.now.store(T0 + WINDOW + 1_000, Ordering::SeqCst);
    match poll(&r).await {
        Health::Unhealthy(why) => {
            assert!(why.starts_with("hub heartbeat stale: last seen 46s ago"), "{why}");
        }
        other => panic!("expected unhealthy, got {other:?}"),
    }
    let st = r.rt.status(&r.h).await;
    assert_eq!(st.state, InstanceState::Degraded);
    assert!(st.detail.unwrap().contains("hub heartbeat stale"));
    // The desktop pings again: healthy.
    r.hub.hear("xpc", T0 + WINDOW + 500);
    assert_eq!(poll(&r).await, Health::Healthy);
    assert_eq!(r.rt.status(&r.h).await.state, InstanceState::Running);
}

#[tokio::test]
async fn no_connection_is_unhealthy_under_its_own_reason() {
    let r = rig(true).await;
    // Never connected.
    match poll(&r).await {
        Health::Unhealthy(why) => assert_eq!(why, "no hub connection for this desktop"),
        other => panic!("{other:?}"),
    }
    // Connected, then dropped.
    r.hub.hear("xpc", T0);
    assert_eq!(poll(&r).await, Health::Healthy);
    r.hub.drop_conn("xpc");
    assert!(matches!(poll(&r).await, Health::Unhealthy(w) if w.contains("no hub connection")));
}

#[tokio::test]
async fn another_desktops_heartbeat_does_not_count() {
    let r = rig(true).await;
    r.hub.hear("macbook", T0);
    assert!(matches!(poll(&r).await, Health::Unhealthy(w) if w.contains("no hub connection")));
}

#[tokio::test]
async fn a_loaded_instance_is_not_judged_and_the_lifecycle_is_managed() {
    let r = rig(false).await;
    assert_eq!(r.rt.status(&r.h).await.state, InstanceState::Loaded);
    assert_eq!(poll(&r).await, Health::Healthy);
    r.rt.start(&r.h).await.unwrap();
    assert!(r.rt.start(&r.h).await.is_err(), "already running");
    assert!(r.rt.unload(r.h.clone()).await.is_err(), "stop before unload");
    r.rt.stop(&r.h, Duration::from_secs(1)).await.unwrap();
    assert_eq!(r.rt.status(&r.h).await.state, InstanceState::Loaded);
    r.rt.unload(r.h.clone()).await.unwrap();
    assert_eq!(r.rt.status(&r.h).await.state, InstanceState::Unknown);
}

#[tokio::test]
async fn only_computer_workloads_are_admitted() {
    let r = rig(false).await;
    let other = crate::workload_runtime::VerifiedWorkload {
        kind: "computer".into(),
        id: "x".into(),
        version: "1".into(),
        source: crate::workload_runtime::WorkloadSource::StorePin { registry: "r".into(), sha256: None },
    };
    assert!(r.rt.admit(&other).await.is_err());
}

#[test]
fn process_kinds_are_judged_as_before() {
    // A sample with no heartbeat on a non-computer path is untouched.
    let s = HealthSample { state: SampleState::Running, exit_code: None, continuous: true, heartbeat_age_ms: None };
    assert_eq!(crate::workload_kind::judge_process(&s), Health::Healthy);
}
