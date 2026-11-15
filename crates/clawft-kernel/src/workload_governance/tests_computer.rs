//! The `computer` kind's production permit (owner decision 2, 2026-11-15):
//! only the owner's own principals, only at operator-attested trust or better.

use serde_json::{Value, json};

use super::*;
use crate::chain::ChainManager;
use crate::gate::GateBackend;
use std::sync::Arc;

const OWNER: &str = "owner-node";

fn ctx(trust: &str) -> Value {
    json!({"workload": {
        "kind": "computer", "package_trust": trust, "node_tier": "pinned",
        "network": "none", "secrets": false, "emulated": false, "resource_cost": 0.0,
        "package_id": "xpc", "signer_keys": [], "artifact_hashes": [],
    }})
}

fn gate() -> WorkloadGate {
    WorkloadGate::exempt(0.95, false, "test")
        .with_chain(Arc::new(ChainManager::new(0, 1000)))
        .with_permit(computer_owner_permit([OWNER]))
        .unwrap()
}

#[test]
fn an_owner_principal_is_admitted_for_every_computer_action() {
    let g = gate();
    for a in ["workload.place", "workload.load", "workload.start", "workload.stop", "workload.unload"] {
        assert!(g.check(OWNER, a, &ctx("operator_attested")).is_permit(), "{a}");
    }
}

#[test]
fn another_principal_is_refused() {
    let g = gate();
    assert!(g.check("someone-else", "workload.place", &ctx("operator_attested")).is_deny());
    // A paired peer that holds only a fetch grant is no owner either.
    assert!(g.check("", "workload.place", &ctx("operator_attested")).is_deny());
}

#[test]
fn trust_below_operator_attested_is_refused_even_for_the_owner() {
    let g = gate();
    for t in ["unsigned", "adopted_unverified", "project_cert", "signed_unpinned"] {
        assert!(g.check(OWNER, "workload.place", &ctx(t)).is_deny(), "{t}");
    }
    assert!(g.check(OWNER, "workload.place", &ctx("operator_attested")).is_permit());
}

#[test]
fn the_permit_reaches_only_the_computer_kind_and_no_network() {
    let g = gate();
    let mut other = ctx("operator_attested");
    other["workload"]["kind"] = json!("cog");
    assert!(g.check(OWNER, "workload.place", &other).is_deny());
    let mut net = ctx("operator_attested");
    net["workload"]["network"] = json!("lan");
    assert!(g.check(OWNER, "workload.place", &net).is_deny());
    // Migration and revocation are not part of it.
    assert!(g.check(OWNER, "workload.migrate", &ctx("operator_attested")).is_deny());
}

#[test]
fn a_permit_with_no_owner_is_refused_by_the_gate() {
    let none: [&str; 0] = [];
    assert!(WorkloadGate::exempt(0.95, false, "test")
        .with_permit(computer_owner_permit(none))
        .is_err());
}
