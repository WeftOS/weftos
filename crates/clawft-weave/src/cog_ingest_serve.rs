//! This daemon's cog ingest bridge and store owner (mesh-placement-10;
//! ADR-100 "Decision 5 resolved"; `docs/cogs/ingest-bridge.md`).
//!
//! A placed cog's vectors enter through the bridge on this node and are
//! forwarded to the node that owns the placing project's store. All knobs
//! are optional; `<runtime>/cog-ingest.json` overrides the defaults:
//!
//! ```json
//! {
//!   "bridge": { "bind": "127.0.0.1:80", "fallback_bind": "127.0.0.1:18080",
//!               "requests_per_sec": 20,
//!               "vectors_per_sec": 2048, "container_bind": "192.168.64.1" },
//!   "routes": [
//!     { "project": "<project id>", "owner": "local",
//!       "controllers": ["<node id>"] },
//!     { "project": "<project id>",
//!       "owner": { "node": "<node id>", "key": "<64 hex>", "addr": "host:9472", "noise": true } },
//!     { "controller": "<node id>", "owner": "local" }
//!   ],
//!   "store_owner": { "listen": "0.0.0.0:9472", "noise": true,
//!                    "forwarders": [ { "key": "<64 hex>", "projects": ["<id>"] },
//!                                    { "key": "<64 hex>", "projects": "*" } ],
//!                    "projects": ["<id>"], "fallback": false }
//! }
//! ```
//!
//! Defaults: the bridge listens on loopback `127.0.0.1:80` (the address
//! released cogs post to); no container listeners; one route sending
//! project-less placements made by this node's own key to this node's
//! store; no store-owner service.
//!
//! Who may place for a project: this node's own key, the node ids in the
//! project route's `controllers`, and the node of the project's bound key
//! (from the identity records, on the user daemon). A project with no route,
//! or a controller with no route, is refused at place time, never placed.
//!
//! `bridge.bind` may be `"off"` (no HTTP listener). When `bind` cannot be
//! bound and `fallback_bind` is set, the bridge listens there instead and
//! native cogs get that address as `COGNITUM_INGEST_URL`. With the node's
//! cogbus running, the bridge publishes every post as a `store.vector`
//! reading and the broker's store sink writes it; cogs that publish
//! `store.vector` on the broker reach the store with no HTTP listener at all
//! (`crate::cogbus_ingest`, ADR-122 section 9).
//!
//! If the bridge cannot bind (the port is taken, or unprivileged on Linux)
//! cogs are still placed, with no token and no URL, and every place result,
//! status and advertisement says `ingest: disabled`. The user daemon and
//! each project daemon would all default to `127.0.0.1:80`, and only the
//! first to bind wins: give the others a distinct `bridge.bind` port in
//! their own runtime dir's `cog-ingest.json` (cogs that honour
//! `COGNITUM_INGEST_URL` follow it; a released cog that posts to the fixed
//! port 80 can be served by one daemon per host).

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use clawft_kernel::cog_ingest::{
    BridgeConfig, BridgeHandle, Forwarder, IngestBridge, IngestHooks, KeyPolicy,
    LocalForwarder, MeshForwarder, OwnerConnector, RateBudget, StaticRouter, StoreOwnerService,
    ProjectDirectory, TokenRegistry, VectorDirectory, owner, valid_project_id,
};
use clawft_kernel::workload_ctl::listen_tcp;
use clawft_kernel::workload_pkg::codec::hex_decode_exact;
use ed25519_dalek::SigningKey;
use crate::scope_gate::ProjectBasis;

#[path = "cog_ingest_config.rs"]
mod config;
pub use config::{
    AnyMarker, BridgeSection, ForwarderKey, ForwarderProjects, INGEST_FILE, IngestConfig, LocalMarker,
    Owner, OwnerSection, RemoteOwner, Route, load_config,
};

/// The identity records of the user daemon: a project's bound key is the
/// node that may place for it. Unavailable (`None`) on a daemon with no
/// project supervisor.
pub struct IdentityDirectory;

impl IdentityDirectory {
    /// True when the identity records can be read here.
    pub fn available() -> bool {
        crate::project_supervisor::global().is_some()
    }
}

impl ProjectDirectory for IdentityDirectory {
    fn bound_node(&self, project_id: &str) -> Option<String> {
        let view = crate::project_supervisor::global()?.identity_view()?;
        let cert = view.current_cert(project_id)?;
        let pk = hex_decode_exact::<32>(&cert.project_pubkey)?;
        Some(clawft_kernel::node_id_from_pubkey(&pk))
    }
}

/// Refuse a project the identity records do not know or have revoked.
/// `dir` is `None` where there are no records (a project daemon): the
/// host's own policy decides there, and the project is only
/// [`ProjectBasis::Claimed`] (logged as `claimed`, the way the scope gate
/// labels a claim it could not verify). A project the records vouch for is
/// [`ProjectBasis::Verified`].
pub fn check_project_registered(dir: Option<&dyn ProjectDirectory>, project_id: &str) -> Result<ProjectBasis, String> {
    if !valid_project_id(project_id) {
        return Err(format!("project {project_id:?} is not a valid project id"));
    }
    let basis = match dir {
        Some(d) if d.bound_node(project_id).is_none() => {
            return Err(format!("project {project_id} is not registered, or its key is revoked"));
        }
        Some(_) => ProjectBasis::Verified,
        None => ProjectBasis::Claimed,
    };
    tracing::debug!(project = project_id, project_basis = basis.label(), "cog ingest project check");
    Ok(basis)
}

/// [`check_project_registered`] against this daemon's identity records.
pub fn check_project(project_id: &str) -> Result<ProjectBasis, String> {
    let dir = IdentityDirectory;
    check_project_registered(
        IdentityDirectory::available().then_some(&dir as &dyn ProjectDirectory),
        project_id,
    )
}

/// What keeps the bridge and the owner service alive. Dropping it stops
/// both.
pub struct IngestRuntime {
    /// Wiring handed to the workload host (disabled if the bridge could not
    /// bind).
    pub hooks: IngestHooks,
    /// Address the shared bridge is bound to, if it is.
    pub bridge_addr: Option<SocketAddr>,
    /// Why the bridge is not running.
    pub bridge_error: Option<String>,
    /// Address the store-owner service is bound to, if serving.
    pub owner_addr: Option<SocketAddr>,
    /// The cogbus store sink, when started on a broker.
    pub store_sink: Option<Arc<crate::cogbus_ingest::StoreSink>>,
    _listener: Option<BridgeHandle>,
    owner_task: Option<tokio::task::JoinHandle<()>>,
    sink_task: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for IngestRuntime {
    fn drop(&mut self) {
        for t in [&self.owner_task, &self.sink_task].into_iter().flatten() {
            t.abort();
        }
    }
}

/// Start the bridge and, if configured, the owner service for the node
/// owning `key`. A bridge that cannot bind does not fail the daemon: the
/// runtime comes back with disabled hooks and `bridge_error` (the owner
/// service, if any, keeps running and is reported).
pub async fn start(
    cfg: &IngestConfig,
    key: &SigningKey,
    projects_dir: Option<Arc<dyn ProjectDirectory>>,
) -> Result<IngestRuntime, String> {
    start_in(cfg, key, projects_dir, None).await
}

/// Subdirectory of the runtime dir holding the owner's per-project vector logs.
pub const VECTOR_STORE_DIR: &str = "cog-ingest-vectors";

/// [`start`], keeping the vectors this node owns in `store_dir` (one capped log
/// per project) so they survive a restart. `None` keeps them in memory only.
pub async fn start_in(
    cfg: &IngestConfig,
    key: &SigningKey,
    projects_dir: Option<Arc<dyn ProjectDirectory>>,
    store_dir: Option<PathBuf>,
) -> Result<IngestRuntime, String> {
    start_on_bus(cfg, key, projects_dir, store_dir, None).await
}

/// [`start_in`], with the bridge publishing on `bus` and the broker's store
/// sink writing every `store.vector` reading (ADR-122 section 9).
pub async fn start_on_bus(
    cfg: &IngestConfig,
    key: &SigningKey,
    projects_dir: Option<Arc<dyn ProjectDirectory>>,
    store_dir: Option<PathBuf>,
    bus: Option<Arc<weftos_cogbus::Broker>>,
) -> Result<IngestRuntime, String> {
    cfg.validate()?;
    let node_id = clawft_kernel::node_id_from_pubkey(&key.verifying_key().to_bytes());
    let routes: Vec<Route> = if cfg.routes.is_empty() {
        vec![Route {
            project: None,
            controller: Some(node_id.clone()),
            owner: Owner::Local(LocalMarker::Local),
            controllers: vec![],
        }]
    } else {
        cfg.routes.clone()
    };
    // What local cogs may write here, and what remote bridges may write
    // here, are separate allow-lists over one set of stores.
    let (mut local_projects, mut local_fallback) = (HashSet::<String>::new(), false);
    for r in &routes {
        if let Owner::Local(_) = r.owner {
            match &r.project {
                Some(p) => {
                    local_projects.insert(p.clone());
                }
                None => local_fallback = true,
            }
        }
    }
    let mut stores = VectorDirectory::new(std::iter::empty(), false);
    if let Some(dir) = store_dir {
        stores = stores.with_persistence(dir);
    }
    let local_dir = Arc::new(stores.view(local_projects, local_fallback));

    let mut router = StaticRouter::new();
    let mut controllers: HashMap<String, HashSet<String>> = HashMap::new();
    let connector = Arc::new(OwnerConnector::new(true));
    for r in &routes {
        let fwd: Arc<dyn Forwarder> = match &r.owner {
            Owner::Local(_) => Arc::new(LocalForwarder::new(node_id.clone(), local_dir.clone())),
            Owner::Remote(o) => {
                let owner_key = hex_decode_exact::<32>(&o.key).ok_or("owner key")?;
                let c: Arc<dyn clawft_kernel::workload_ctl::CtlConnector> = if o.noise {
                    connector.clone()
                } else {
                    tracing::warn!(owner = %o.node, "cog-store link without Noise: batches travel in clear (signed, not encrypted)");
                    Arc::new(OwnerConnector::new(false))
                };
                Arc::new(MeshForwarder::new(key.clone(), &o.node, owner_key, &o.addr, c))
            }
        };
        router = match (&r.project, &r.controller) {
            (Some(p), _) => {
                controllers
                    .entry(p.clone())
                    .or_default()
                    .extend(r.controllers.iter().cloned());
                router.with_project_route(p, fwd)
            }
            (_, Some(c)) => router.with_controller(c, fwd),
            _ => router,
        };
    }

    let (mut owner_addr, mut owner_task) = (None, None);
    if let Some(s) = &cfg.store_owner {
        let mut policy = KeyPolicy::new();
        for f in &s.forwarders {
            let k = hex_decode_exact::<32>(&f.key).ok_or("forwarder key")?;
            policy = match &f.projects {
                ForwarderProjects::Any(_) => policy.allow_any(k),
                ForwarderProjects::List(ps) => {
                    let ps: Vec<&str> = ps.iter().map(String::as_str).collect();
                    policy.allow_projects(k, &ps)
                }
            };
        }
        if !s.noise {
            tracing::warn!("cog-store listener without Noise: batches travel in clear (signed, not encrypted)");
        }
        // Only what `store_owner` lists: not the local routes' stores.
        let owner_dir = Arc::new(stores.view(s.projects.iter().cloned(), s.fallback));
        let svc = Arc::new(StoreOwnerService::new(key.clone(), Arc::new(policy), owner_dir));
        let listener = listen_tcp(&s.listen)
            .await
            .map_err(|e| format!("cog-store listen {}: {e}", s.listen))?;
        owner_addr = Some(listener.local_addr().map_err(|e| e.to_string())?);
        let noise = s.noise;
        owner_task = Some(tokio::spawn(async move {
            if let Err(e) = owner::serve_listener(listener, svc, noise).await {
                tracing::warn!(error = %e, "cog-store listener stopped");
            }
        }));
    }

    let registry = Arc::new(TokenRegistry::new());
    let container_bind = cfg
        .bridge
        .container_bind
        .as_deref()
        .map(|c| c.parse::<IpAddr>())
        .transpose()
        .map_err(|e| e.to_string())?;
    let budget = || {
        let (r, v) = (cfg.bridge.requests_per_sec, cfg.bridge.vectors_per_sec);
        RateBudget::new(std::time::Duration::from_secs(1), r, v)
    };
    let router: Arc<dyn clawft_kernel::cog_ingest::StoreRouter> = Arc::new(router);
    let (store_sink, sink_task, router) = match bus {
        Some(b) => {
            let a = crate::cogbus_ingest::attach(b, router, budget())?;
            (Some(a.sink), Some(a.task), a.router)
        }
        None => (None, None, router),
    };
    let bridge = IngestBridge::new(
        registry.clone(),
        router,
        budget(),
        BridgeConfig {
            allow_non_loopback: container_bind.is_some_and(|ip| !ip.is_loopback()),
            ..Default::default()
        },
    );
    let (listener, bridge_error) = crate::cogbus_ingest::bind_bridge(&bridge, &cfg.bridge).await;
    if let Some(why) = &bridge_error {
        tracing::warn!(error = %why, owner = ?owner_addr, cogbus_store_sink = store_sink.is_some(),
            "cog ingest bridge disabled; cogs are placed without a token (ingest: disabled)");
    }
    let bridge_addr = listener.as_ref().map(BridgeHandle::addr);
    let mut hooks = match bridge_addr {
        Some(addr) => IngestHooks::new(node_id.clone(), registry, bridge, addr, container_bind),
        None => IngestHooks::disabled(node_id, bridge_error.clone().unwrap_or_default()),
    }
    .with_project_controllers(controllers);
    if let Some(d) = projects_dir {
        hooks = hooks.with_project_directory(d);
    }
    Ok(IngestRuntime {
        hooks,
        bridge_addr,
        bridge_error,
        owner_addr,
        store_sink,
        _listener: listener,
        owner_task,
        sink_task,
    })
}

#[cfg(test)]
#[path = "cog_ingest_serve_tests.rs"]
mod tests;
