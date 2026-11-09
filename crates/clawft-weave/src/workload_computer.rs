//! `workload.place {computer: <body>}` and `workload.explain {computer: <body>}`
//! (owner decisions D1, D2, D5 of 2026-11-09): a `computer` workload is a
//! desktop declaration, not a package. The body is checked by the `computer`
//! kind (`clawft_kernel::workload_kind::ComputerKind`), the workload and its
//! placement spec are built from it, and the answer says what was accepted.
//!
//! Nothing is started: there is no `computer` runtime adapter yet (the hub
//! heartbeat that would judge health, and the adapter that binds an instance to
//! a hub connection, are the next slice). The reply says so in `placed: false`
//! and `adapter_ready: false` rather than pretending a run.

use clawft_kernel::workload_kind::{
    ComputerBody, ComputerKind, KIND_COMPUTER, WorkloadKind, computer_workload,
};
use clawft_kernel::workload_pkg::ManifestEnvelope;
use serde_json::{Value, json};

/// Validate a `workload.place|explain` request that carries a `computer` body.
/// The request may carry nothing else: a package, pin or peer next to a
/// computer body is refused, not ignored.
pub fn declare(params: &Value) -> Result<Value, String> {
    let obj = params.as_object().ok_or("params must be an object")?;
    if let Some(k) = obj.keys().find(|k| k.as_str() != "computer") {
        return Err(format!("unexpected parameter {k:?} next to `computer`"));
    }
    let body = &params["computer"];
    let env = ManifestEnvelope::new(KIND_COMPUTER, body.clone());
    // `validate` is the kind's own check; `computer_workload` repeats it, so
    // the two can never disagree about what is accepted.
    let workload = computer_workload(&env).map_err(|e| e.to_string())?;
    let spec = ComputerKind.spec(&workload)?;
    let b = ComputerBody::parse(body).map_err(|e| e.to_string())?;
    Ok(json!({
        "kind": KIND_COMPUTER,
        "desktop": b.desktop,
        "host_kind": b.host_kind,
        "access": b.access,
        "per_action_ask": b.allowlist.is_some(),
        "migratable": ComputerKind.migratable(),
        "adapter": ComputerKind.adapters(),
        "spec": serde_json::to_value(&spec).map_err(|e| e.to_string())?,
        "placed": false,
        "adapter_ready": false,
        "note": "declaration accepted; the computer adapter that binds it to a hub connection is not built yet",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_valid_own_computer_is_accepted_and_pinned_to_its_desktop() {
        let v = declare(&json!({"computer": {
            "desktop": "xpc", "host_kind": "desktop",
            "capabilities": ["screen.capture"], "access": "own"}}))
        .unwrap();
        assert_eq!((v["desktop"].as_str(), v["access"].as_str()), (Some("xpc"), Some("own")));
        assert_eq!(v["migratable"], false);
        assert_eq!(v["per_action_ask"], false);
        assert_eq!(v["placed"], false);
        let spec = v["spec"].to_string();
        assert!(spec.contains("computer.desktop.xpc"), "{spec}");
    }

    #[test]
    fn a_granted_computer_asks_per_action() {
        let v = declare(&json!({"computer": {
            "desktop": "macbook", "host_kind": "desktop", "capabilities": ["input.click"],
            "access": "granted", "allowlist": {"apps": ["Terminal"], "displays": ["main"]}}}))
        .unwrap();
        assert_eq!(v["per_action_ask"], true);
    }

    #[test]
    fn bad_bodies_and_extra_parameters_are_refused() {
        for bad in [
            json!({}),
            json!({"computer": 3}),
            json!({"computer": {"desktop": "X!", "host_kind": "desktop", "capabilities": ["a"], "access": "own"}}),
            json!({"computer": {"desktop": "xpc", "host_kind": "desktop", "capabilities": ["a"], "access": "granted"}}),
            json!({"computer": {"desktop": "xpc", "host_kind": "desktop", "capabilities": ["a"], "access": "own", "x": 1}}),
            json!({"package_dir": "/tmp/p", "computer": {"desktop": "xpc", "host_kind": "desktop", "capabilities": ["a"], "access": "own"}}),
            json!([]),
        ] {
            assert!(declare(&bad).is_err(), "{bad}");
        }
    }
}
