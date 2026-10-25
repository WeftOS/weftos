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
//! The broker exists before the ingest bridge starts ([`broker`]), so the
//! bridge publishes on it and its store sink takes `store.vector/*` even when
//! the socket could not be bound (ADR-122 section 9, `crate::cogbus_ingest`).
//! The registrar tells the store sink each instance's placement.
//!
//! Only native cogs get the socket: they run as the daemon's own uid (the
//! daemon's native adapter has no `run_as`), which is the uid the broker
//! checks the peer against. Container and VM routes have no view of a host
//! socket yet, so they get no broker in B1.
//!
//! B3: the broker runs with [`crate::cogbus_policy`] (kernel governance and
//! `cogbus-policy.json` grants and denies) and writes
//! [`crate::cogbus_activity`]; `cogbus.list` carries the recent refusals and
//! [`report_json`] is the dashboard report's `topics` section.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use clawft_kernel::cog_ingest::InstanceBinding;
use clawft_kernel::workload_ctl::host_cogbus::{CogbusAttach, CogbusRegistrar, CogbusRegistration};
use clawft_rpc::Response;
use serde_json::{Value, json};
use weftos_cogbus::server::{ServerHandle, serve};
use weftos_cogbus::{
    Broker, BrokerConfig, BusHooks, InputSpec, InstanceSpec, OutputSpec, SOCKET_NAME,
};

use crate::cogbus_activity::CogbusActivity;
use crate::cogbus_ingest::StoreSink;
use crate::cogbus_policy::{CogbusPolicy, Rules};

/// Topic rows in the dashboard report, busiest first.
pub const REPORT_TOPICS: usize = 64;

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

/// A broker for node `node_id` with the default hooks (allow what the
/// manifest allows, `tracing` only), not yet listening.
pub fn broker(node_id: &str) -> Arc<Broker> {
    broker_with(node_id, BusHooks::default())
}

/// A broker for node `node_id` with `hooks`, not yet listening.
pub fn broker_with(node_id: &str, hooks: BusHooks) -> Arc<Broker> {
    Arc::new(Broker::new(BrokerConfig::new(node_id)).with_hooks(hooks))
}

/// The daemon's hooks: governance over `rules` and `dir`'s policy file, the
/// activity log in `dir`.
pub fn hooks(dir: &Path, rules: Option<Rules>) -> BusHooks {
    BusHooks {
        policy: Arc::new(CogbusPolicy::new(dir, rules)),
        sink: Arc::new(CogbusActivity::open(dir)),
        ..BusHooks::default()
    }
}

/// Start the broker for node `node_id` in `dir`.
pub async fn start(dir: &Path, node_id: &str) -> Result<CogbusRuntime, String> {
    start_on(broker(node_id), dir).await
}

/// Serve `broker` on `dir`'s socket.
pub async fn start_on(broker: Arc<Broker>, dir: &Path) -> Result<CogbusRuntime, String> {
    let path: PathBuf = dir.join(SOCKET_NAME);
    let server = serve(broker.clone(), &path)
        .await
        .map_err(|e| format!("cannot bind {}: {e}", path.display()))?;
    Ok(CogbusRuntime { broker, _server: server })
}

/// Serve `broker` and keep it for the daemon's life; the registrar to hand
/// the workload host, or `None` (logged) when the socket could not be bound.
/// `sink` is the ingest bridge's store sink, told of every instance.
pub async fn install(broker: Arc<Broker>, dir: &Path, sink: Option<Arc<StoreSink>>) -> Option<Arc<dyn CogbusRegistrar>> {
    match start_on(broker, dir).await {
        Ok(rt) => {
            tracing::info!(socket = %rt.socket().display(), store_sink = sink.is_some(), "cogbus");
            let mut reg = Registrar::new(rt.broker.clone(), rt.socket());
            if let Some(s) = sink {
                reg = reg.with_store_sink(s);
            }
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
    sink: Option<Arc<StoreSink>>,
}

impl Registrar {
    /// Registrar over `broker` serving `socket`, for cogs run as this uid.
    pub fn new(broker: Arc<Broker>, socket: &Path) -> Self {
        Self {
            broker,
            socket: socket.to_string_lossy().into_owned(),
            uid: nix::unistd::geteuid().as_raw(),
            sink: None,
        }
    }

    /// Tell `sink` where each registered instance's `store.vector` goes.
    pub fn with_store_sink(mut self, sink: Arc<StoreSink>) -> Self {
        self.sink = Some(sink);
        self
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
            .map_err(|e| e.message)?;
        if let Some(s) = &self.sink {
            s.bind(InstanceBinding::new(&r.instance_id, r.project_id.clone(), &r.controller_node));
        }
        Ok(())
    }

    fn revoke(&self, instance_id: &str) {
        self.broker.revoke(instance_id);
        if let Some(s) = &self.sink {
            s.unbind(instance_id);
        }
    }
}

/// The `cogbus.list` document.
pub fn list_json(rt: Option<&CogbusRuntime>) -> Value {
    match rt {
        Some(rt) => json!({
            "running": true,
            "socket": rt.socket().to_string_lossy(),
            "topics": rt.broker.snapshot(),
            "refusals": rt.broker.recent_refusals(),
        }),
        None => json!({ "running": false, "topics": [], "refusals": [] }),
    }
}

fn clip(s: &str) -> String {
    s.chars().take(64).collect()
}

/// The report's `topics` section for `broker`: at most [`REPORT_TOPICS`]
/// rows, busiest first, the total, and how many of the retained recent
/// refusals (at most 32) fell in the last hour. No socket paths, no bodies.
pub fn report_of(broker: &Broker, now_ms: u64) -> Value {
    let mut rows = broker.snapshot();
    let total = rows.len();
    rows.sort_by(|a, b| {
        b.messages
            .cmp(&a.messages)
            .then_with(|| a.topic.cmp(&b.topic))
    });
    let items: Vec<Value> = rows
        .iter()
        .take(REPORT_TOPICS)
        .map(|t| {
            json!({
                "topic": clip(&t.topic), "schema": clip(&t.schema), "class": t.class,
                "writer": t.writer.as_deref().map(clip), "project": t.project_id,
                "subs": t.subscribers, "msgs": t.messages, "drops": t.drops,
            })
        })
        .collect();
    let hour = now_ms.saturating_sub(3_600_000);
    let refused = broker
        .recent_refusals()
        .iter()
        .filter(|a| a.ts_ms >= hour)
        .count();
    json!({ "total": total, "items": items, "refusals_1h": refused })
}

/// `topics` for the heartbeat report, when the broker is running.
pub fn report_json() -> Option<Value> {
    RUNTIME
        .get()
        .map(|rt| report_of(&rt.broker, clawft_kernel::exposure::registry::now_ms()))
}

/// Put [`report_json`] into `report` as `topics`, beside `exposures`.
pub fn attach_report(report: &mut Value) {
    if let Some(topics) = report_json() {
        report["topics"] = topics;
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
