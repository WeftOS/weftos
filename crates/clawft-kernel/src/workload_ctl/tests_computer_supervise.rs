//! A `computer` instance held by a node's host service is supervised through
//! the computer adapter: the supervisor polls the hub heartbeat, the kind
//! judges it, and a silent desktop goes Unhealthy then recovers. The clock is
//! injected; the instance is placed straight into the host (a computer has no
//! signed package to place through `workload.ctl place`).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use ed25519_dalek::SigningKey;
use serde_json::json;

use super::host_service::{Placed, WorkloadHostService};
use super::lifecycle::{InstanceLife, LifecycleState};
use super::test_support::{anchors, exchange};
use crate::chain::ChainManager;
use crate::node_registry::node_id_from_pubkey;
use crate::workload_governance::{NodeTrustTier, WorkloadGate, computer_owner_permit};
use crate::workload_kind::{KIND_COMPUTER, computer_workload};
use crate::workload_pkg::ManifestEnvelope;
use crate::workload_runtime::{
    ComputerRuntime, HeartbeatSource, HostContract, RunMode, WorkloadConfig, WorkloadHost,
};

const T0: u64 = 1_000_000;

#[derive(Default)]
struct Hub(Mutex<HashMap<String, u64>>);
impl HeartbeatSource for Hub {
    fn last_seen_ms(&self, d: &str) -> Option<u64> {
        self.0.lock().unwrap().get(d).copied()
    }
}

#[tokio::test]
async fn a_silent_desktop_goes_unhealthy_through_the_supervisor_and_recovers() {
    let key = SigningKey::from_bytes(&[7; 32]);
    let id = node_id_from_pubkey(&key.verifying_key().to_bytes());
    let chain = Arc::new(ChainManager::new(0, 1000));
    let gate = Arc::new(
        WorkloadGate::exempt(0.95, false, "test")
            .with_permit(computer_owner_permit([id.clone()]))
            .unwrap()
            .with_chain(chain.clone()),
    );
    let hub = Arc::new(Hub::default());
    let now = Arc::new(AtomicU64::new(T0));
    let clock = {
        let now = now.clone();
        Arc::new(move || now.load(Ordering::SeqCst))
    };
    let host = Arc::new(
        WorkloadHost::new(
            Arc::new(ComputerRuntime::with_clock(hub.clone(), clock)),
            gate.clone(),
            id.clone(),
            NodeTrustTier::Paired,
        )
        .with_chain(chain.clone()),
    );
    let svc = WorkloadHostService::new(key, exchange(&id, &chain), anchors(), gate)
        .with_route("computer", host.clone())
        .with_chain(chain.clone());

    let w = computer_workload(&ManifestEnvelope::new(
        KIND_COMPUTER,
        json!({"desktop": "xpc", "host_kind": "desktop", "capabilities": ["screen.capture"], "access": "own"}),
    ))
    .unwrap();
    let cfg = WorkloadConfig {
        mode: RunMode::Listener,
        args: Vec::new(),
        host: HostContract::default_feed(),
        node_id: "n".into(),
    };
    let handle = host.load(&w, &cfg).await.unwrap();
    host.start(&handle).await.unwrap();
    svc.instances.lock().await.insert(
        handle.instance_id.clone(),
        Placed {
            handle: handle.clone(),
            route: "computer".into(),
            name: "xpc".into(),
            variant: "computer".into(),
            decision_id: None,
            last: None,
            ingest: None,
            cogbus: None,
            ingest_state: "none",
            life: InstanceLife::new(LifecycleState::Running, T0),
            continuous: true,
            desired_running: true,
            lease_stopped: false,
            requester: id,
            licence: None,
        },
    );
    let at = |ms: u64| {
        now.store(T0 + ms, Ordering::SeqCst);
        T0 + ms
    };

    // Heard just now: polled, healthy, nothing changes.
    hub.0.lock().unwrap().insert("xpc".into(), T0);
    assert!(svc.supervise(at(10_000)).await.is_empty());
    assert_eq!(svc.lifecycle_of(&handle.instance_id).await, Some(LifecycleState::Running));

    // Silent past 15 s x 3: each poll is a miss; the third makes it Unhealthy,
    // naming the heartbeat.
    assert!(svc.supervise(at(60_000)).await.is_empty(), "miss 1");
    assert!(svc.supervise(at(75_000)).await.is_empty(), "miss 2");
    let steps = svc.supervise(at(90_000)).await;
    let unhealthy = steps
        .iter()
        .find(|s| s.to == LifecycleState::Unhealthy)
        .unwrap_or_else(|| panic!("no unhealthy step in {steps:?}"));
    assert!(unhealthy.reason.contains("hub heartbeat stale"), "{}", unhealthy.reason);
    // Alert, never restart: the instance stays Unhealthy, no step restarts it,
    // and the chain carries the alert with the named reason.
    assert!(!steps.iter().any(|s| s.to == LifecycleState::Restarting), "{steps:?}");
    assert_eq!(svc.lifecycle_of(&handle.instance_id).await, Some(LifecycleState::Unhealthy));
    let alerts: Vec<_> = super::test_support::events(&chain, crate::chain::EVENT_KIND_WORKLOAD_LIFECYCLE)
        .into_iter()
        .filter(|(_, p)| p["alert"] == true)
        .collect();
    assert_eq!(alerts.len(), 1, "{alerts:?}");
    assert!(alerts[0].1["reason"].as_str().unwrap().contains("hub heartbeat stale"));
    assert_eq!(alerts[0].1["restart"], "never");
    // Further silent polls neither restart it nor repeat the alert.
    assert!(svc.supervise(at(105_000)).await.is_empty());
    assert_eq!(svc.lifecycle_of(&handle.instance_id).await, Some(LifecycleState::Unhealthy));

    // The desktop pings again: the next polls are healthy and nothing moves.
    hub.0.lock().unwrap().insert("xpc".into(), T0 + 100_000);
    let back = svc.supervise(at(120_000)).await;
    assert!(back.iter().any(|s| s.to == LifecycleState::Running && s.reason == "health recovered"), "{back:?}");
    assert_eq!(svc.lifecycle_of(&handle.instance_id).await, Some(LifecycleState::Running));
    assert!(svc.supervise(at(135_000)).await.is_empty());
}
