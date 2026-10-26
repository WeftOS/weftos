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
//! Only native cogs get the socket. Each runs under its own uid when the
//! daemon can switch uids and `cogbus-isolation.json` allows it; otherwise
//! under the daemon's uid, marked `shared-uid` ([`crate::cogbus_isolation`]).
//! The broker checks the peer against that uid. The socket is `0660`, group
//! `cogbus` when the group exists and the daemon may assign it. Container and
//! VM routes have no view of a host socket yet, so they get no broker in B1.
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
use weftos_cogbus::blob::{BlobConfig, BlobStore};
use weftos_cogbus::server::{ServeOptions, ServerHandle, SocketReport, serve_with};
use weftos_cogbus::{
    Broker, BrokerConfig, BusHooks, InputSpec, InstanceSpec, OutputSpec, SOCKET_NAME,
};

use crate::cogbus_activity::CogbusActivity;
use crate::cogbus_isolation::Isolator;
use crate::cogbus_ingest::StoreSink;
use crate::cogbus_policy::{CogbusPolicy, Rules};

/// The content store's directory under the runtime dir.
pub const BLOB_DIR: &str = "cogbus-blobs";

/// Topic rows in the dashboard report, busiest first.
pub const REPORT_TOPICS: usize = 64;

/// Keeps the broker listening for the daemon's life.
static RUNTIME: OnceLock<CogbusRuntime> = OnceLock::new();

/// The running broker and its socket.
pub struct CogbusRuntime {
    /// The broker.
    pub broker: Arc<Broker>,
    /// Uid allocation and the isolation table.
    pub isolator: Arc<Isolator>,
    _server: ServerHandle,
}

impl CogbusRuntime {
    /// Socket path.
    pub fn socket(&self) -> &Path {
        self._server.path()
    }

    /// Mode, owner and group the socket was bound with.
    pub fn socket_report(&self) -> &SocketReport {
        self._server.report()
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

/// The daemon's broker: `hooks`, and the content store in
/// `<dir>/cogbus-blobs` (default quota 2 GiB, TTL-evicted). A store that
/// cannot be opened is logged and blob requests are `unsupported`.
pub fn broker_in(node_id: &str, hooks: BusHooks, dir: &Path) -> Arc<Broker> {
    let now = clawft_kernel::exposure::registry::now_ms();
    let mut b = Broker::new(BrokerConfig::new(node_id)).with_hooks(hooks);
    match BlobStore::open(&dir.join(BLOB_DIR), BlobConfig::default(), now) {
        Ok(store) => b = b.with_blobs(Arc::new(store)),
        Err(e) => tracing::warn!(error = %e, "cogbus content store not opened; blob requests are unsupported"),
    }
    Arc::new(b)
}

/// The daemon's hooks: governance over `rules` and `dir`'s policy file, the
/// activity log in `dir`.
pub fn hooks(dir: &Path, rules: Option<Rules>) -> BusHooks {
    hooks_with_activity(dir, rules).0
}

/// [`hooks`], and the activity sink, so the daemon can give it the workload
/// stop hook once the host exists.
pub fn hooks_with_activity(dir: &Path, rules: Option<Rules>) -> (BusHooks, Arc<CogbusActivity>) {
    let activity = Arc::new(CogbusActivity::open(dir));
    let hooks = BusHooks {
        policy: Arc::new(CogbusPolicy::new(dir, rules)),
        sink: activity.clone(),
        ..BusHooks::default()
    };
    (hooks, activity)
}

/// The stop hook for `host`: when the broker stopped a cog on the bus and
/// governance agreed, stop the workload through the host (its gated
/// `workload.stop`, chained with the cause) and leave it stopped.
pub fn stop_hook(host: std::sync::Weak<clawft_kernel::workload_ctl::host_service::WorkloadHostService>) -> crate::cogbus_activity::StopHook {
    Arc::new(move |instance: &str, reason: &str| {
        let (Some(host), Ok(rt)) = (host.upgrade(), tokio::runtime::Handle::try_current()) else {
            tracing::warn!(target: "cogbus", instance, reason, "cannot stop the workload (no host or runtime)");
            return;
        };
        let (instance, reason) = (instance.to_string(), reason.to_string());
        rt.spawn(async move {
            match host.stop_for_bus(&instance, &reason).await {
                Ok(Some(s)) => tracing::warn!(target: "cogbus", instance = %s.instance_id, workload = %s.workload, reason = %s.reason, "workload stopped by the cogbus"),
                Ok(None) => tracing::info!(target: "cogbus", %instance, "no live workload instance to stop"),
                Err(e) => tracing::error!(target: "cogbus", %instance, error = %e, "workload stop failed; the cog keeps running without the bus"),
            }
        });
    })
}

/// Start the broker for node `node_id` in `dir`.
pub async fn start(dir: &Path, node_id: &str) -> Result<CogbusRuntime, String> {
    start_on(broker(node_id), dir).await
}

/// Serve `broker` on `dir`'s socket with every cog on the daemon's uid.
pub async fn start_on(broker: Arc<Broker>, dir: &Path) -> Result<CogbusRuntime, String> {
    start_with(broker, dir, Arc::new(Isolator::shared()), None).await
}

/// Serve `broker` on `dir`'s socket, group `group` (its gid) when given.
pub async fn start_with(
    broker: Arc<Broker>,
    dir: &Path,
    isolator: Arc<Isolator>,
    group: Option<u32>,
) -> Result<CogbusRuntime, String> {
    let path: PathBuf = dir.join(SOCKET_NAME);
    let server = serve_with(broker.clone(), &path, ServeOptions { group })
        .await
        .map_err(|e| format!("cannot bind {}: {e}", path.display()))?;
    Ok(CogbusRuntime { broker, isolator, _server: server })
}

/// Serve `broker` and keep it for the daemon's life; the registrar to hand
/// the workload host, or `None` (logged) when the socket could not be bound.
/// `sink` is the ingest bridge's store sink, told of every instance.
pub async fn install(broker: Arc<Broker>, dir: &Path, sink: Option<Arc<StoreSink>>) -> Option<Arc<dyn CogbusRegistrar>> {
    let cfg = crate::cogbus_isolation::load(dir).unwrap_or_else(|e| {
        tracing::warn!(error = %e, "cogbus-isolation.json ignored; cogs share the daemon's uid");
        Default::default()
    });
    let isolator = Arc::new(Isolator::detect(cfg.clone()));
    let group = crate::cogbus_isolation::group_gid(&cfg.group);
    match start_with(broker, dir, isolator.clone(), group).await {
        Ok(rt) => {
            tracing::info!(
                socket = %rt.socket().display(), store_sink = sink.is_some(),
                isolation = %isolator.report()["mode"], reason = ?isolator.blocker(),
                socket_group_applied = rt.socket_report().group_applied, "cogbus"
            );
            // The NPU service's telemetry feeds node facts (cogbus_npu_facts).
            if let Err(e) = crate::cogbus_npu_facts::attach(&rt.broker) {
                tracing::warn!(error = %e, "npu service state will not appear in node facts");
            }
            let mut reg = Registrar::for_runtime(&rt);
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
    isolator: Arc<Isolator>,
    sink: Option<Arc<StoreSink>>,
}

impl Registrar {
    /// Registrar over `broker` serving `socket`, for cogs run as this uid.
    pub fn new(broker: Arc<Broker>, socket: &Path) -> Self {
        Self::with_isolator(broker, socket, Arc::new(Isolator::shared()))
    }

    /// Registrar over a running broker, sharing its isolation table.
    pub fn for_runtime(rt: &CogbusRuntime) -> Self {
        Self::with_isolator(rt.broker.clone(), rt.socket(), rt.isolator.clone())
    }

    /// Registrar allocating identities from `isolator`.
    pub fn with_isolator(broker: Arc<Broker>, socket: &Path, isolator: Arc<Isolator>) -> Self {
        Self {
            broker,
            socket: socket.to_string_lossy().into_owned(),
            uid: nix::unistd::geteuid().as_raw(),
            isolator,
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
        if route != "native" {
            return None;
        }
        let plan = self.isolator.plan();
        Some(CogbusAttach {
            socket: self.socket.clone(),
            uid: Some(plan.identity.as_ref().map_or(self.uid, |i| i.uid)),
            identity: plan.identity,
            isolation: plan.isolation,
            refuse_shared: self.isolator.require_cog_uid(),
            note: plan.note,
        })
    }

    fn register(&self, r: &CogbusRegistration) -> Result<(), String> {
        self.broker
            .register(spec_of(r), r.token.expose())
            .map_err(|e| e.message)?;
        self.isolator.bind(&r.instance_id, &r.cog_id, r.uid, r.isolation);
        // Declared message rates become publish budgets (ADR-122 gap 5).
        for o in &r.outputs {
            if let Some(per_sec) = o.rate.as_deref().and_then(weftos_cogbus::rate::parse_rate) {
                self.broker.set_output_rate(&r.instance_id, &o.topic, per_sec);
            }
        }
        if let Some(s) = &self.sink {
            s.bind(InstanceBinding::new(&r.instance_id, r.project_id.clone(), &r.controller_node));
        }
        Ok(())
    }

    fn revoke(&self, instance_id: &str) {
        self.broker.revoke(instance_id);
        self.isolator.unbind(instance_id);
        if let Some(s) = &self.sink {
            s.unbind(instance_id);
        }
    }

    fn release(&self, instance_id: &str) {
        self.isolator.release(instance_id);
    }

    fn abandon(&self, uid: u32) {
        self.isolator.abandon(uid);
    }
}

/// The `cogbus.list` document.
pub fn list_json(rt: Option<&CogbusRuntime>) -> Value {
    match rt {
        Some(rt) => json!({
            "running": true,
            "socket": rt.socket().to_string_lossy(),
            "socket_mode": format!("{:04o}", rt.socket_report().mode),
            "socket_uid": rt.socket_report().uid,
            "socket_gid": rt.socket_report().gid,
            "socket_group_applied": rt.socket_report().group_applied,
            "socket_note": rt.socket_report().note,
            "isolation": rt.isolator.report(),
            "blobs": rt.broker.blob_store().map(|b| b.stats()),
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
