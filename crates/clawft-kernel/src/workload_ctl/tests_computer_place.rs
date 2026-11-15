//! The package-less place path for kind `computer` through the host service
//! (owner decisions 1 and 2, 2026-11-15): the order carries a manifest body that
//! names the desktop, the host admits it through the governance gate under the
//! requester's principal, and every other kind still needs a signed package.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use ed25519_dalek::SigningKey;
use serde_json::{Value, json};

use super::host_service::{CtlConfig, PlaceBody, WorkloadHostService};
use super::lifecycle::LifecycleState;
use super::msg::{CTL_VERSION, CtlRequest, RefusalCode, method};
use super::test_support::{anchors, exchange};
use crate::chain::ChainManager;
use crate::node_registry::node_id_from_pubkey;
use crate::workload_governance::{NodeTrustTier, WorkloadGate, computer_owner_permit};
use crate::workload_runtime::{ComputerRuntime, HeartbeatSource, RunMode, WorkloadHost};

#[derive(Default)]
struct Hub(Mutex<HashMap<String, u64>>);
impl HeartbeatSource for Hub {
    fn last_seen_ms(&self, d: &str) -> Option<u64> {
        self.0.lock().unwrap().get(d).copied()
    }
}

struct Rig {
    svc: WorkloadHostService,
    id: String,
}

fn rig(owners: &[&str], with_route: bool) -> Rig {
    let key = SigningKey::from_bytes(&[7; 32]);
    let id = node_id_from_pubkey(&key.verifying_key().to_bytes());
    let chain = Arc::new(ChainManager::new(0, 1000));
    let mut owner_list: Vec<String> = owners.iter().map(|s| s.to_string()).collect();
    owner_list.push(id.clone());
    let gate = Arc::new(
        WorkloadGate::exempt(0.95, false, "test")
            .with_permit(computer_owner_permit(owner_list))
            .unwrap()
            .with_chain(chain.clone()),
    );
    let host = Arc::new(
        WorkloadHost::new(
            Arc::new(ComputerRuntime::new(Arc::new(Hub::default()))),
            gate.clone(),
            id.clone(),
            NodeTrustTier::Pinned,
        )
        .with_chain(chain.clone()),
    );
    let mut svc = WorkloadHostService::new(key, exchange(&id, &chain), anchors(), gate).with_chain(chain);
    if with_route {
        // Also reachable as `native`, so an order routed there is judged by the
        // computer check, not just by a missing adapter.
        svc = svc.with_route("native", host.clone()).with_route("computer", host);
    }
    Rig { svc, id }
}

fn body(desktop: &str) -> Value {
    json!({"desktop": desktop, "host_kind": "desktop", "capabilities": ["screen.capture"], "access": "own"})
}

fn order(name: &str, hash: &str, variant: &str, computer: Option<Value>) -> PlaceBody {
    PlaceBody {
        name: name.into(),
        manifest_hash: hash.into(),
        variant: variant.into(),
        config: CtlConfig { mode: RunMode::Listener, args: Vec::new(), csi_port: 1 },
        start: true,
        project_id: None,
        computer,
    }
}

fn request(requester: &str, b: &PlaceBody) -> CtlRequest {
    CtlRequest {
        version: CTL_VERSION,
        method: method::PLACE.into(),
        requester: requester.into(),
        target: "n".into(),
        nonce: "0".repeat(32),
        issued_at_ms: 0,
        expires_at_ms: u64::MAX,
        decision_id: Some("0".repeat(64)),
        body: serde_json::to_value(b).unwrap(),
    }
}

async fn place(r: &Rig, requester: &str, b: &PlaceBody) -> Result<Value, super::msg::Refusal> {
    r.svc.place(&request(requester, b), None).await
}

#[tokio::test]
async fn an_owner_places_a_computer_without_a_package_and_it_runs() {
    let r = rig(&["laptop"], true);
    for who in [r.id.clone(), "laptop".to_string()] {
        let desk = if who == "laptop" { "mac" } else { "xpc" };
        let out = place(&r, &who, &order(desk, "", "computer", Some(body(desk)))).await.unwrap();
        let iid = out["instance_id"].as_str().unwrap_or_else(|| panic!("{out}")).to_string();
        assert_eq!(r.svc.lifecycle_of(&iid).await, Some(LifecycleState::Running), "{who}");
    }
}

#[tokio::test]
async fn another_principal_is_refused_by_governance() {
    let r = rig(&[], true);
    let e = place(&r, "stranger", &order("xpc", "", "computer", Some(body("xpc")))).await.unwrap_err();
    assert_eq!(e.code, RefusalCode::Governance, "{e:?}");
    assert!(r.svc.instances.lock().await.is_empty());
}

#[tokio::test]
async fn a_package_less_order_for_any_other_kind_is_refused() {
    let r = rig(&[], true);
    // No hash, no computer body: a cog (or any package kind) needs a signed package.
    let e = place(&r, &r.id, &order("cog-a", "", "aarch64-native", None)).await.unwrap_err();
    assert_eq!(e.code, RefusalCode::InvalidRequest);
    // A bad hash is refused as before.
    let e = place(&r, &r.id, &order("cog-a", "xyz", "aarch64-native", None)).await.unwrap_err();
    assert_eq!(e.code, RefusalCode::InvalidRequest);
    assert!(r.svc.instances.lock().await.is_empty());
}

#[tokio::test]
async fn a_malformed_computer_order_is_refused() {
    let r = rig(&[], true);
    let h = "a".repeat(64);
    let cases = [
        // a hash next to a computer body
        order("xpc", &h, "computer", Some(body("xpc"))),
        // routed to another adapter
        order("xpc", "", "aarch64-native", Some(body("xpc"))),
        // the name is not the desktop id
        order("other", "", "computer", Some(body("xpc"))),
        // a bad desktop id
        order("X!", "", "computer", Some(body("X!"))),
        // a bad body
        order("xpc", "", "computer", Some(json!({"desktop": "xpc"}))),
    ];
    for c in cases {
        let e = place(&r, &r.id, &c).await.unwrap_err();
        assert_eq!(e.code, RefusalCode::InvalidRequest, "{c:?}: {e:?}");
    }
    assert!(r.svc.instances.lock().await.is_empty());
}

#[tokio::test]
async fn a_node_with_no_computer_adapter_refuses_the_order() {
    let r = rig(&[], false);
    let e = place(&r, &r.id, &order("xpc", "", "computer", Some(body("xpc")))).await.unwrap_err();
    assert_eq!(e.code, RefusalCode::Admission, "{e:?}");
}

#[tokio::test]
async fn the_same_desktop_cannot_be_placed_twice() {
    let r = rig(&[], true);
    let b = order("xpc", "", "computer", Some(body("xpc")));
    place(&r, &r.id, &b).await.unwrap();
    assert!(place(&r, &r.id, &b).await.is_err());
}
