//! This daemon's cogbus (ADR-122 phase B1): the broker on
//! `<runtime dir>/cogbus.sock`, the registrar the workload host calls at
//! place, start, stop and unload, and `cogbus.list`.
//!
//! The runtime dir is the one placement boots with (`BOOT.runtime_dir` in
//! `workload_place_rpc`, the same dir that holds `cog-ingest.json`), so the
//! user daemon and each project daemon get their own socket. If the socket
//! cannot be bound (a path over the Unix socket length limit, a non-socket
//! file in the way) the daemon carries on and cogs are placed without
//! `WEFTOS_COGBUS`, the way a down ingest bridge places them without a token.
//!
//! Only native cogs get the socket: they run as the daemon's own uid (the
//! daemon's native adapter has no `run_as`), which is the uid the broker
//! checks the peer against. Container and VM routes have no view of a host
//! socket yet, so they get no broker in B1.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use clawft_kernel::workload_ctl::host_cogbus::{CogbusAttach, CogbusRegistrar, CogbusRegistration};
use clawft_rpc::Response;
use serde_json::{Value, json};
use weftos_cogbus::server::{ServerHandle, serve};
use weftos_cogbus::{Broker, BrokerConfig, InputSpec, InstanceSpec, OutputSpec, SOCKET_NAME};

/// Keeps the broker listening for the daemon's life.
static RUNTIME: OnceLock<CogbusRuntime> = OnceLock::new();

/// The running broker and its socket.
pub struct CogbusRuntime {
    /// The broker.
    pub broker: Arc<Broker>,
    _server: ServerHandle,
}

impl CogbusRuntime {
    /// Socket path.
    pub fn socket(&self) -> &Path {
        self._server.path()
    }
}

/// Start the broker for node `node_id` in `dir`.
pub async fn start(dir: &Path, node_id: &str) -> Result<CogbusRuntime, String> {
    let path: PathBuf = dir.join(SOCKET_NAME);
    let broker = Arc::new(Broker::new(BrokerConfig::new(node_id)));
    let server = serve(broker.clone(), &path)
        .await
        .map_err(|e| format!("cannot bind {}: {e}", path.display()))?;
    Ok(CogbusRuntime { broker, _server: server })
}

/// Start the broker and keep it for the daemon's life; the registrar to
/// hand the workload host, or `None` (logged) when it could not start.
pub async fn install(dir: &Path, node_id: &str) -> Option<Arc<dyn CogbusRegistrar>> {
    match start(dir, node_id).await {
        Ok(rt) => {
            tracing::info!(socket = %rt.socket().display(), "cogbus");
            let reg = Registrar::new(rt.broker.clone(), rt.socket());
            let _ = RUNTIME.set(rt);
            Some(Arc::new(reg))
        }
        Err(e) => {
            tracing::warn!(error = %e, "cogbus not started; cogs run without WEFTOS_COGBUS");
            None
        }
    }
}

/// The broker behind the workload host.
pub struct Registrar {
    broker: Arc<Broker>,
    socket: String,
    uid: u32,
}

impl Registrar {
    /// Registrar over `broker` serving `socket`, for cogs run as this uid.
    pub fn new(broker: Arc<Broker>, socket: &Path) -> Self {
        Self {
            broker,
            socket: socket.to_string_lossy().into_owned(),
            uid: nix::unistd::geteuid().as_raw(),
        }
    }
}

/// The broker's view of a registration.
pub fn spec_of(r: &CogbusRegistration) -> InstanceSpec {
    InstanceSpec {
        instance_id: r.instance_id.clone(),
        cog_id: r.cog_id.clone(),
        project_id: r.project_id.clone(),
        uid: r.uid,
        outputs: r
            .outputs
            .iter()
            .map(|o| OutputSpec { topic: o.topic.clone(), schema: o.schema.clone(), class: o.class })
            .collect(),
        inputs: r
            .inputs
            .iter()
            .map(|i| InputSpec { topic: i.topic.clone(), schema: i.schema.clone(), policy: i.policy })
            .collect(),
    }
}

impl CogbusRegistrar for Registrar {
    fn attach(&self, route: &str) -> Option<CogbusAttach> {
        (route == "native").then(|| CogbusAttach { socket: self.socket.clone(), uid: Some(self.uid) })
    }

    fn register(&self, r: &CogbusRegistration) -> Result<(), String> {
        self.broker
            .register(spec_of(r), r.token.expose())
            .map_err(|e| e.message)
    }

    fn revoke(&self, instance_id: &str) {
        self.broker.revoke(instance_id);
    }
}

/// The `cogbus.list` document.
pub fn list_json(rt: Option<&CogbusRuntime>) -> Value {
    match rt {
        Some(rt) => json!({
            "running": true,
            "socket": rt.socket().to_string_lossy(),
            "topics": rt.broker.snapshot(),
        }),
        None => json!({ "running": false, "topics": [] }),
    }
}

/// Dispatch one `cogbus.*` verb.
pub async fn handle(method: &str, _params: Value) -> Response {
    match method {
        "cogbus.list" => Response::success(list_json(RUNTIME.get())),
        other => Response::error(format!("unknown method: {other}")),
    }
}

#[cfg(test)]
#[path = "cogbus_serve_tests.rs"]
mod tests;
