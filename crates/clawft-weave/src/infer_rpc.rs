//! Operator verbs for inference placement (card mesh-placement-19):
//!
//! - `infer.status` (Read): roles, proxy state, where each role resolves,
//!   exposure, and why the mesh is or is not available.
//! - `infer.start {role}` / `infer.stop {role}` (Admin): bring a managed
//!   role up or take it down through the governed host (chained); an
//!   unplaceable start (memory budget, co-residency) is refused with its
//!   reason, which the status shows.
//! - `infer.expose {role, exposed}` (Admin): let a role leave loopback for
//!   mesh peers (or stop). Audited on the chain.
//! - `infer.allow {role, node, direction, allowed}` (Admin): `direction` is
//!   `serve` (this node serves `node` the role) or `consume` (this node may
//!   use `node`'s instance). Default deny both ways. Audited on the chain.
//!
//! Changes are in effect until restart; `<runtime>/inference.json` holds
//! the persistent allowlists. In service mode the mesh verbs refuse with
//! the reason (see [`crate::infer_wire`]).

use clawft_kernel::exposure::ExposureState;
use clawft_kernel::workload_pkg::manifest::valid_token;
use clawft_rpc::Response;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::infer_wire::{InferState, state};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExposeParams {
    role: String,
    exposed: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RoleParams {
    role: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AllowParams {
    role: String,
    node: String,
    direction: String,
    allowed: bool,
}

fn off() -> Response {
    Response::error("inference placement is off (no inference.json in the runtime directory)".to_string())
}

fn require_mesh(st: &InferState) -> Result<(), String> {
    if st.hub.is_none() {
        return Err(format!("mesh serving is {}", st.mesh_note));
    }
    Ok(())
}

/// Expose (or stop exposing) `role` to the mesh as an operator act: the
/// placement table and the exposure registry (ADR-120) both record it, and
/// the registry keeps the decision across restarts.
pub(crate) fn set_role_exposed(st: &InferState, role: &str, on: bool) -> Result<(), String> {
    require_mesh(st)?;
    if !role_known(st, role) {
        return Err(format!("unknown role {role:?}"));
    }
    st.table.expose_to_mesh(role, on);
    let reg = st.table.exposures();
    let id = clawft_kernel::infer_proxy::PlacementTable::exposure_id(role);
    if reg.get(&id).is_some() {
        let state = if on { ExposureState::On } else { ExposureState::Off };
        reg.set_state(&id, state, "infer.expose")?;
    }
    Ok(())
}

fn role_known(st: &InferState, role: &str) -> bool {
    st.roles.iter().any(|r| r.cfg.role == role)
}

/// Dispatch one `infer.*` verb.
pub async fn handle(method: &str, params: Value) -> Response {
    let Some(st) = state() else { return off() };
    match method {
        "infer.status" => Response::success(status(st)),
        "infer.start" | "infer.stop" => {
            let p: RoleParams = match serde_json::from_value(params) {
                Ok(p) => p,
                Err(e) => return Response::error(format!("invalid params: {e}")),
            };
            let r = if method == "infer.start" { st.start_role(&p.role).await } else { st.stop_role(&p.role).await };
            match r {
                Ok(v) => Response::success(v),
                Err(e) => Response::error(e),
            }
        }
        "infer.expose" => {
            let p: ExposeParams = match serde_json::from_value(params) {
                Ok(p) => p,
                Err(e) => return Response::error(format!("invalid params: {e}")),
            };
            if let Err(why) = set_role_exposed(st, &p.role, p.exposed) {
                return Response::error(why);
            }
            Response::success(json!({"role": p.role, "exposed": p.exposed}))
        }
        "infer.allow" => {
            let p: AllowParams = match serde_json::from_value(params) {
                Ok(p) => p,
                Err(e) => return Response::error(format!("invalid params: {e}")),
            };
            if let Err(why) = require_mesh(st) {
                return Response::error(why);
            }
            if !role_known(st, &p.role) || !valid_token(&p.node, 128) {
                return Response::error("unknown role or malformed node id".to_string());
            }
            match p.direction.as_str() {
                "serve" => st.table.allow_mesh_peer(&p.role, &p.node, p.allowed),
                "consume" => st.table.allow_remote_node(&p.role, &p.node, p.allowed),
                _ => return Response::error("direction is serve or consume".to_string()),
            }
            Response::success(json!({
                "role": p.role, "node": p.node, "direction": p.direction, "allowed": p.allowed
            }))
        }
        _ => Response::error(format!("unknown method {method}")),
    }
}

/// `infer.status` body (tests).
#[cfg(test)]
pub(crate) fn status_for_test(st: &InferState) -> Value {
    status(st)
}

fn status(st: &InferState) -> Value {
    let roles: Vec<Value> = st.roles.iter().map(|r| st.role_json(r)).collect();
    let resident: Vec<Value> = st
        .ledger
        .snapshot()
        .into_iter()
        .map(|(role, b)| json!({"role": role, "gb": b as f64 / 1e9}))
        .collect();
    json!({
        "node_id": st.node_id,
        "mesh": {"available": st.hub.is_some(), "note": st.mesh_note},
        "peers": st.hub.as_ref().map(|h| h.qualifying_peers()).unwrap_or_default(),
        "memory": {
            "budget_gb": st.ledger.budget().map(|b| b as f64 / 1e9),
            "used_gb": st.ledger.used() as f64 / 1e9,
            "resident": resident,
        },
        "roster_skipped": st.skipped_roster.iter().map(|(id, why)| json!({"id": id, "why": why})).collect::<Vec<_>>(),
        "roles": roles,
    })
}
