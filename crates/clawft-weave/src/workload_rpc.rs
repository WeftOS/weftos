//! Daemon handlers for the `workload.*` RPC family (ADR-099 decision 2,
//! card mesh-placement-06).
//!
//! Implemented on this node today:
//!
//! | method              | capability | governance action  | chain kind          |
//! |---------------------|------------|--------------------|---------------------|
//! | `workload.list`     | Read       | —                  | —                   |
//! | `workload.inspect`  | Read       | —                  | —                   |
//! | `workload.install`  | Write      | `workload.install` | `workload.install`  |
//! | `workload.unload`   | Write      | `workload.unload`  | `workload.unload`   |
//!
//! A refused mutation is chained as `workload.refuse`. Every `workload.*`
//! mutation fails closed when no governance gate is configured (ADR-099
//! section 4 default-deny). On the daemon the gate is the [`WorkloadGate`]
//! over the operator's `workload-permits.json` (see `workload_gate`): with
//! no matching permit the action is denied and chained, with one it is
//! permitted and chained, and a revoked package is denied either way. The
//! placement family (`place`, `explain`, `status`, `stop`, `logs`,
//! `revoke`, `unload {instance_id}`) is served by `workload_place_rpc`.
//! `load` and `start` are target-side `workload.ctl` methods, and `migrate`
//! belongs to a later card: it answers "not available on this node".
//! `node.bind`, `node.unbind`, `node.binding` and `node.reset-floor`
//! (ADR-106) are served by `licence_rpc` on a placement build. Any other `workload.*` method is refused and the refusal
//! chained (default deny).
//!
//! [`WorkloadGate`]: clawft_kernel::workload_governance::WorkloadGate

use std::path::Path;
use std::sync::{Arc, OnceLock};

use clawft_kernel::GateBackend;
use clawft_kernel::refusal_budget::RefusalBudget;
use clawft_rpc::Response;
use serde_json::{Value, json};

use crate::rpc_gate::{Audit, decide_as};
use crate::workload_registry::{
    InstallRequest, WorkloadRecord, WorkloadRegistry, WorkloadState, validate_name,
};

// Governance action and chain event names are the kernel's (ADR-099
// sections 4 and 7): the action string and the chain kind are the same.
use clawft_kernel::workload_governance::CATALOG_PRINCIPAL;
use clawft_kernel::chain::{
    EVENT_KIND_WORKLOAD_INSTALL as WORKLOAD_INSTALL, EVENT_KIND_WORKLOAD_REFUSE as WORKLOAD_REFUSE,
    EVENT_KIND_WORKLOAD_UNLOAD as WORKLOAD_UNLOAD,
};

/// Verbs named by ADR-099 whose handlers belong to later cards or to the
/// placement control plane (`workload.revoke` is served there; without it
/// the verb is not available).
const NOT_YET: &[&str] = &[
    "workload.load",
    "workload.start",
    "workload.migrate",
    "workload.revoke",
];

/// ADR-106 phase 1d verbs. Served by `licence_rpc` on a placement build;
/// without placement there are no Seeds to bind.
const NEEDS_PLACEMENT: &[&str] =
    &[
    "workload.node.bind",
    "workload.node.unbind",
    "workload.node.binding",
    "workload.node.reset-floor",
    "workload.cog.checkout",
    "workload.cog.checkout.approve",
    "workload.cog.checkout.status",
    "workload.cog.checkout.release",
    "workload.cog.checkout.renew",
    "workload.cog.checkout.list",
];

/// The gate context for a catalog action. The catalog records what the
/// caller says (a name, a kind and a manifest hash) and verifies nothing, so
/// the package is `unsigned`: a permit has to say `min_package_trust =
/// "unsigned"` to allow it. The package id is the catalog name (what an
/// operator revokes with `--package`), and a `blake3:` manifest hash is also
/// named as an artifact, so a revocation of either denies the install. The
/// node is this one (`pinned`) and nothing here touches the network. The
/// decision is made as the `catalog` principal ([`CATALOG_PRINCIPAL`]), so
/// the permit that accepts unsigned packages can be, and has to be, limited
/// to it: it cannot also admit an unsigned placement. The
/// gate derives its own effect vector from these fields and ignores a
/// hand-written `effect`.
fn workload_ctx(kind: &str, name: &str, manifest_hash: Option<&str>) -> Value {
    let hashes: Vec<&str> = manifest_hash
        .and_then(|h| h.strip_prefix("blake3:"))
        .into_iter()
        .collect();
    json!({
        "kind": kind,
        "workload": {
            "kind": kind,
            "package_trust": "unsigned",
            "node_tier": "pinned",
            "network": "none",
            "secrets": false,
            "emulated": false,
            "resource_cost": 0.0,
            "package_id": name,
            "artifact_hashes": hashes,
        },
    })
}

static REGISTRY: OnceLock<Arc<WorkloadRegistry>> = OnceLock::new();

/// Install the persisted daemon catalog (call once at daemon boot).
/// Later calls are ignored; returns whether this call won.
pub fn init_registry(path: &Path) -> bool {
    REGISTRY
        .set(Arc::new(WorkloadRegistry::with_persist_path(path)))
        .is_ok()
}

/// The daemon catalog (in-memory if boot never initialised it).
pub fn registry() -> Arc<WorkloadRegistry> {
    REGISTRY
        .get_or_init(|| Arc::new(WorkloadRegistry::new()))
        .clone()
}

fn name_param(params: &Value, method: &str) -> Result<String, String> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{method} requires a string 'name'"))?;
    validate_name(name)?;
    Ok(name.to_owned())
}

/// `workload.list` → `[WorkloadRecord, ...]` sorted by name.
pub fn handle_list(reg: &WorkloadRegistry) -> Response {
    Response::success(json!(reg.list()))
}

/// `workload.inspect {name}` → one record.
pub fn handle_inspect(reg: &WorkloadRegistry, params: &Value) -> Response {
    let name = match name_param(params, "workload.inspect") {
        Ok(n) => n,
        Err(e) => return Response::error(e),
    };
    match reg.get(&name) {
        Some(rec) => Response::success(json!(rec)),
        None => Response::error(format!("workload not found: {name}")),
    }
}

fn refuse(audit: Audit<'_>, action: &str, name: &str, reason: String) -> Response {
    audit(
        WORKLOAD_REFUSE,
        json!({ "action": action, "name": name, "reason": &reason }),
    );
    Response::error(reason)
}

/// `workload.install {name, kind, manifest_hash, version?}`.
///
/// Validates, asks the gate for `workload.install`, and on Permit
/// records the workload and chains `workload.install`.
pub fn handle_install(
    reg: &WorkloadRegistry,
    params: Value,
    node_id: &str,
    gate: Option<&dyn GateBackend>,
    audit: Audit<'_>,
) -> Response {
    let req: InstallRequest = match serde_json::from_value(params) {
        Ok(r) => r,
        Err(e) => return Response::error(format!("invalid workload.install params: {e}")),
    };
    if let Err(e) = req.validate() {
        return Response::error(e);
    }
    if reg.contains(&req.name) {
        return Response::error(format!("workload '{}' is already installed", req.name));
    }
    let mut ctx = workload_ctx(&req.kind, &req.name, Some(&req.manifest_hash));
    ctx["name"] = json!(&req.name);
    ctx["manifest_hash"] = json!(&req.manifest_hash);
    ctx["node_id"] = json!(node_id);
    ctx["effect"] = json!({ "risk": 0.4, "security": 0.4 });
    if let Err(reason) = decide_as(CATALOG_PRINCIPAL, gate, WORKLOAD_INSTALL, &ctx, true) {
        return refuse(audit, WORKLOAD_INSTALL, &req.name, reason);
    }
    let record = WorkloadRecord {
        name: req.name,
        kind: req.kind,
        manifest_hash: req.manifest_hash,
        version: req.version,
        state: WorkloadState::Installed,
        node_id: node_id.to_owned(),
        installed_at: chrono::Utc::now(),
    };
    if let Err(e) = reg.insert(record.clone()) {
        return Response::error(e);
    }
    audit(
        WORKLOAD_INSTALL,
        json!({
            "name": &record.name,
            "kind": &record.kind,
            "manifest_hash": &record.manifest_hash,
            "version": &record.version,
            "node_id": &record.node_id,
        }),
    );
    Response::success(json!(record))
}

/// `workload.unload {name}`: drop an installed workload from this node.
pub fn handle_unload(
    reg: &WorkloadRegistry,
    params: &Value,
    gate: Option<&dyn GateBackend>,
    audit: Audit<'_>,
) -> Response {
    let name = match name_param(params, "workload.unload") {
        Ok(n) => n,
        Err(e) => return Response::error(e),
    };
    let Some(rec) = reg.get(&name) else {
        return Response::error(format!("workload not found: {name}"));
    };
    // Taking a record out names no package: revocation never blocks teardown.
    let mut ctx = workload_ctx(&rec.kind, &rec.name, None);
    ctx["workload"]["package_id"] = Value::Null;
    ctx["name"] = json!(&rec.name);
    ctx["manifest_hash"] = json!(&rec.manifest_hash);
    ctx["node_id"] = json!(&rec.node_id);
    ctx["effect"] = json!({ "risk": 0.2, "security": 0.1 });
    if let Err(reason) = decide_as(CATALOG_PRINCIPAL, gate, WORKLOAD_UNLOAD, &ctx, true) {
        return refuse(audit, WORKLOAD_UNLOAD, &name, reason);
    }
    match reg.remove(&name) {
        Some(rec) => {
            audit(
                WORKLOAD_UNLOAD,
                json!({ "name": &rec.name, "kind": &rec.kind, "node_id": &rec.node_id }),
            );
            Response::success(json!({ "unloaded": rec.name }))
        }
        None => Response::error(format!("workload not found: {name}")),
    }
}

/// Longest method name chained or echoed for an unknown `workload.*`.
pub const MAX_SHOWN_METHOD: usize = 64;

/// Deny an unknown `workload.*` method (default deny, ADR-099 section 4).
/// The caller picks the name and may be anonymous, so the name is cut to
/// [`MAX_SHOWN_METHOD`] characters and the refusal is chained only within
/// `budget`; the rest are counted and the count rides on the next one
/// chained (`suppressed`).
pub fn deny_unknown(audit: Audit<'_>, method: &str, budget: &RefusalBudget) -> Response {
    let shown: String = method.chars().take(MAX_SHOWN_METHOD).collect();
    let reason = format!("{shown}: not a workload method; denied by default (ADR-099 section 4)");
    if let Some(suppressed) = budget.take() {
        audit(
            WORKLOAD_REFUSE,
            json!({ "action": shown, "name": "", "reason": &reason,
                    "method_bytes": method.len(), "suppressed": suppressed }),
        );
    }
    Response::error(reason)
}

/// Route one `workload.*` call. `gate`/`audit` come from the kernel.
pub fn route(
    method: &str,
    params: Value,
    reg: &WorkloadRegistry,
    node_id: &str,
    gate: Option<&dyn GateBackend>,
    audit: Audit<'_>,
) -> Response {
    match method {
        "workload.list" => handle_list(reg),
        "workload.inspect" => handle_inspect(reg, &params),
        "workload.install" => handle_install(reg, params, node_id, gate, audit),
        "workload.unload" => handle_unload(reg, &params, gate, audit),
        m if NEEDS_PLACEMENT.contains(&m) => Response::error(format!(
            "{m} is not available in this build (it needs the placement feature)"
        )),
        m if NOT_YET.contains(&m) => Response::error(format!(
            "{m} is not available on this node yet (ADR-099: needs runtime adapters / \
             mesh control plane)"
        )),
        other if other.starts_with("workload.") => {
            static BUDGET: OnceLock<RefusalBudget> = OnceLock::new();
            deny_unknown(audit, other, BUDGET.get_or_init(RefusalBudget::default))
        }
        other => Response::error(format!("unknown method: {other}")),
    }
}

/// Daemon entry point: pulls gate, chain and node id from the kernel.
#[cfg(any(unix, windows))]
pub async fn dispatch(
    method: &str,
    params: Value,
    kernel: Arc<
        tokio::sync::RwLock<clawft_kernel::boot::Kernel<clawft_platform::NativePlatform>>,
    >,
) -> Response {
    #[cfg(all(feature = "placement", unix))]
    if crate::licence_rpc::handles(method) {
        return crate::licence_rpc::dispatch(method, params, kernel).await;
    }
    #[cfg(all(feature = "placement", unix))]
    if crate::licence_checkout_rpc::handles(method) {
        return crate::licence_checkout_rpc::dispatch(method, params, kernel).await;
    }
    #[cfg(all(feature = "placement", unix))]
    if crate::cog_check_rpc::handles(method) {
        return crate::cog_check_rpc::dispatch(method, params, kernel).await;
    }
    #[cfg(all(feature = "placement", unix))]
    if crate::workload_place_rpc::handles(method, &params) {
        return crate::workload_place_rpc::dispatch(method, params, kernel).await;
    }
    #[cfg(all(feature = "placement", unix))]
    let policy_dir = crate::workload_place_rpc::runtime_dir();
    #[cfg(not(all(feature = "placement", unix)))]
    let policy_dir: Option<std::path::PathBuf> = None;
    dispatch_in(method, params, kernel, policy_dir.as_deref()).await
}

/// [`dispatch`] with the operator's policy directory given (`None`: the
/// kernel's own gate decides, which default-denies `workload.*`).
#[cfg(any(unix, windows))]
pub(crate) async fn dispatch_in(
    method: &str,
    params: Value,
    kernel: Arc<
        tokio::sync::RwLock<clawft_kernel::boot::Kernel<clawft_platform::NativePlatform>>,
    >,
    policy_dir: Option<&Path>,
) -> Response {
    // The catalog verbs are decided by the workload gate (default deny, the
    // operator's permits, the revocation list). A broken permits file fails
    // closed for them; the read-only verbs never need it.
    #[cfg(all(feature = "placement", unix))]
    let workload_gate: Option<Arc<dyn GateBackend>> = match (
        matches!(method, "workload.install" | "workload.unload"),
        policy_dir,
    ) {
        (true, Some(dir)) => match crate::workload_gate::from_kernel(&kernel, dir).await {
            Ok(g) => Some(g as Arc<dyn GateBackend>),
            Err(e) => {
                return Response::error(format!(
                    "governance denied '{method}': workload policy unavailable (fail closed): {e}"
                ));
            }
        },
        _ => None,
    };
    #[cfg(not(all(feature = "placement", unix)))]
    let workload_gate: Option<Arc<dyn GateBackend>> = {
        let _ = policy_dir;
        None
    };
    let k = kernel.read().await;
    let node_id = k.cluster_membership().local_node_id().to_owned();
    let gate = workload_gate.or_else(|| k.governance_gate().cloned());
    let chain = k.chain_manager().cloned();
    drop(k);
    let audit = |kind: &str, payload: Value| {
        if let Some(cm) = &chain {
            cm.append("workload", kind, Some(payload));
        }
    };
    route(method, params, &registry(), &node_id, gate.as_deref(), &audit)
}

#[cfg(test)]
#[path = "workload_rpc_tests.rs"]
mod tests;

#[cfg(all(test, feature = "placement", unix))]
#[path = "workload_gate_daemon_tests.rs"]
mod gate_daemon_tests;
