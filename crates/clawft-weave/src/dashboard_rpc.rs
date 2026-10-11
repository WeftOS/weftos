//! `dashboard.status` (Read) and `dashboard.token.rotate` (Admin): the local
//! RPC entry, the mesh entry for a remote controller, and daemon start.
//!
//! Both methods take an optional `node`. Absent, or this node's own mesh id,
//! they run here. Otherwise they are sent to that peer over the signed
//! `workload.ctl` mesh wire (ADR-099 section 7, the transport placement already
//! uses) and run by the peer's [`MeshAdmin`] hook. What it takes, in order:
//!
//! 1. the local caller holds Admin (rotate) on this daemon;
//! 2. the peer is in `workload-peers.json` and, for rotate, at tier `pinned`
//!    (the operator's word, never the peer's own claim);
//! 3. the peer serves `workload-host` and lists THIS node's key as a controller;
//! 4. the peer's `[dashboard]` is enabled and `allow_remote_rotate` is not off.
//!
//! The request is signed with the node key, expires, is replay-guarded, and is
//! chained on both nodes (`node_admin.*`).

use std::sync::Arc;

#[cfg(all(feature = "placement", unix))]
use async_trait::async_trait;
#[cfg(all(feature = "placement", unix))]
use clawft_kernel::workload_ctl::NodeAdmin;
use clawft_rpc::Response;
use serde_json::{Value, json};

use crate::dashboard_cfg::DashboardConfig;
use crate::dashboard_report::{self, Ambient, Dashboard, SupervisorChildren};
use crate::dashboard_workspaces::{ManifestWorkspaces, WorkspaceSource};
use crate::rpc_ext::{ExtCall, ExtFuture, KernelRef};

const STATUS: &str = "dashboard.status";
const ROTATE: &str = "dashboard.token.rotate";
const ACTIONS: &str = "dashboard.actions";
const ACTIONS_DEFAULT: usize = 20;

/// The mesh-side hook: answers a controller's `dashboard.*` request from the
/// running reporter. Installed on the node's `workload-host` at placement build;
/// the reporter is looked up per call, so it may start after the host does.
pub struct MeshAdmin;

#[cfg(all(feature = "placement", unix))]
#[async_trait]
impl NodeAdmin for MeshAdmin {
    async fn call(&self, method: &str, requester: &str, body: &Value) -> Result<Value, String> {
        self.call_raw(method, requester, body).await.map(|(v, _)| v)
    }

    async fn call_raw(&self, method: &str, requester: &str, body: &Value) -> Result<(Value, Option<Vec<u8>>), String> {
        // ADR-108 P3b: a fetch peer's `project.fetch` (the host already checked
        // the peer and grant files; the server checks them again per project).
        // A raw chunk's bytes go back as the frame after the signed answer.
        if method == clawft_kernel::workload_ctl::msg::method::PROJECT_FETCH {
            let h = crate::project_fetch_serve::global().ok_or("project fetch is not enabled on this node")?;
            return h.serve_raw(requester, body).await;
        }
        let d = dashboard_report::global()
            .ok_or("the dashboard reporter is not enabled on this node ([dashboard] enabled = true)")?;
        DashAdmin(d).call(method, requester, body).await.map(|v| (v, None))
    }
}

/// [`MeshAdmin`] for a given reporter (what it does once it has found one).
pub struct DashAdmin(pub Arc<Dashboard>);

#[cfg(all(feature = "placement", unix))]
#[async_trait]
impl NodeAdmin for DashAdmin {
    async fn call(&self, method: &str, requester: &str, _body: &Value) -> Result<Value, String> {
        tracing::info!(%method, %requester, "dashboard request from a mesh controller");
        run_local(&self.0, method, true).await
    }
}

/// Run `method` against the local reporter. `remote` marks a mesh caller.
pub async fn run_local(d: &Dashboard, method: &str, remote: bool) -> Result<Value, String> {
    match method {
        STATUS => Ok(d.status_json()),
        ACTIONS if remote => Err("dashboard.actions is local only".into()),
        ACTIONS => Ok(d.actions_json(ACTIONS_DEFAULT)),
        ROTATE if remote && !d.config().allow_remote_rotate => {
            Err("remote rotation is switched off on this node ([dashboard] allow_remote_rotate = false)".into())
        }
        ROTATE => d.rotate().await,
        other => Err(format!("{other} is not a dashboard method")),
    }
}

#[cfg(all(feature = "placement", unix))]
fn is_local(node: &str) -> bool {
    crate::workload_place_rpc::local_mesh_node_id().is_some_and(|me| me == node)
}

/// Send `method` to peer `node` on the signed `workload.ctl` mesh wire.
#[cfg(all(feature = "placement", unix))]
async fn remote(kernel: &KernelRef, node: &str, method: &str) -> Result<Value, String> {
    crate::workload_place_rpc::node_admin(kernel, node, method, json!({})).await
}

#[cfg(not(all(feature = "placement", unix)))]
fn is_local(_node: &str) -> bool {
    false
}

#[cfg(not(all(feature = "placement", unix)))]
async fn remote(_kernel: &KernelRef, _node: &str, _method: &str) -> Result<Value, String> {
    Err("sending a dashboard request to another node needs a unix build with mesh placement".into())
}

async fn handle_inner(
    method: &str,
    params: Value,
    kernel: &KernelRef,
    is_admin: bool,
) -> Result<Value, String> {
    let node = match params.get("node") {
        None | Some(Value::Null) => None,
        Some(Value::String(n)) if !n.is_empty() && n.len() <= 128 => Some(n.as_str()),
        Some(_) => return Err("`node` must be a non-empty node id string".into()),
    };
    let limit = params.get("limit").and_then(Value::as_u64).map_or(ACTIONS_DEFAULT, |n| n.clamp(1, 128) as usize);
    match node {
        Some(n) if method == ACTIONS && !is_local(n) => Err("dashboard.actions is local only".into()),
        _ if method == ACTIONS => match dashboard_report::global() {
            Some(d) => Ok(d.actions_json(limit)),
            None => Ok(json!({ "enabled": false, "queued": 0, "recorded": 0, "actions": [] })),
        },
        // Contacting a peer is an Admin act even for the read-only status.
        Some(_) if !is_admin => Err("sending a dashboard request to another node needs Admin".into()),
        Some(n) if !is_local(n) => remote(kernel, n, method).await,
        _ => match dashboard_report::global() {
            Some(d) => run_local(&d, method, false).await,
            None if method == STATUS => Ok(json!({ "enabled": false })),
            None => Err("the dashboard reporter is not enabled on this node ([dashboard] enabled = true in weave.toml)".into()),
        },
    }
}

/// Route handler for `dashboard.status` and `dashboard.token.rotate`.
pub fn handle(call: ExtCall) -> ExtFuture {
    Box::pin(async move {
        let admin = call.ctx.caps.allows(crate::capability::Capability::Admin);
        match handle_inner(&call.method, call.params, &call.ctx.kernel, admin).await {
            Ok(v) => Response::success(v),
            Err(e) => Response::error(e),
        }
    })
}

/// Start the reporter from `[dashboard]` in the user's `weave.toml`. A disabled
/// section does nothing; an invalid one logs why and leaves the daemon running.
pub async fn start(kernel: &KernelRef, home: &std::path::Path, gateway: Option<String>) {
    let cfg = match crate::dashboard_cfg::load(home).await {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "dashboard reporter not started: invalid [dashboard] section");
            return;
        }
    };
    start_with(kernel, cfg, home, gateway).await;
}

async fn start_with(kernel: &KernelRef, cfg: DashboardConfig, home: &std::path::Path, gateway: Option<String>) {
    if !cfg.enabled {
        return;
    }
    if let Err(e) = crate::dashboard_cfg::check_token_file(cfg.token_path().unwrap_or(std::path::Path::new(""))) {
        tracing::error!(error = %e, "dashboard reporter not started: refusing the token file");
        return;
    }
    let mesh_listen = kernel
        .read()
        .await
        .kernel_config()
        .mesh
        .as_ref()
        .filter(|m| m.enabled)
        .map(|m| m.listen_addr.clone());
    #[cfg(all(feature = "placement", unix))]
    crate::mesh_pair_rpc::set_mesh_listen(mesh_listen.clone());
    #[cfg(all(feature = "placement", unix))]
    let pair_source = crate::mesh_pair_rpc::DaemonPairSource { mesh_listen: mesh_listen.clone() };
    let ambient = Ambient { mesh_listen, gateway_url: cfg.gateway_url.clone().or(gateway) };
    let children = Arc::new(SupervisorChildren { manifests_dir: crate::user_daemon::manifests_dir(home) });
    let workspaces: Option<Arc<dyn WorkspaceSource>> = cfg
        .report_workspaces
        .then(|| Arc::new(ManifestWorkspaces { manifests_dir: crate::user_daemon::manifests_dir(home) }) as _);
    let d = match Dashboard::with_workspaces(cfg, ambient, children, workspaces) {
        Ok(d) => d,
        Err(e) => {
            tracing::error!(error = %e, "dashboard reporter not started");
            return;
        }
    };
    #[cfg(all(feature = "placement", unix))]
    d.set_pair_source(Arc::new(pair_source));
    d.set_route_source(Arc::new(crate::dashboard_routes::RouterRoutes::daemon()));
    d.set_service_source(Arc::new(crate::dashboard_services::ProjectServices::daemon(home)));
    if !dashboard_report::install_global(d.clone()) {
        return;
    }
    d.spawn_loop(dashboard_report::FIRST_BEAT);
    tracing::info!(node = %d.config().node_id, interval_secs = d.config().interval_secs, "dashboard reporter started");
}

#[cfg(test)]
#[path = "dashboard_mesh_tests.rs"]
mod mesh_tests;
