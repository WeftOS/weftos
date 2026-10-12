//! The daemon's inference placement wiring (cards mesh-placement-19 and
//! -20; ADR-101 sections 5 to 8): the placement table, the loopback proxy
//! per role, the adapters (observing a server somebody else runs, or
//! starting and stopping one through the model lab's launcher), the memory
//! budget, the mesh hub, and the hooks that let local agents, the LLM
//! service and the voice TTS use a placed role.
//!
//! Off unless the operator writes `<runtime>/inference.json` (format in
//! [`crate::infer_cfg`]). Everything defaults to off and deny.
//!
//! Service mode (ADR-103): the daemon does not own the mesh there and the
//! service hands it deliveries as an unverified peer, so it holds no
//! admission grant for anyone. Local placement works; serving and using
//! remote instances do not, and the status says so.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use clawft_kernel::chain::ChainManager;
use clawft_kernel::infer_proxy::{
    InferHub, InferProxy, PlacementTable, ProxyAudit, ProxyLimits, ServeGate, Upstream,
};
use clawft_kernel::mesh_runtime::MeshRuntime;
use clawft_kernel::model_manifest::{ModelRegistry, ModelTrust};
use clawft_kernel::workload_governance::{NodeTrustTier, WorkloadGate};
use clawft_kernel::workload_runtime::infer::{
    InferConfig, InferRuntime, InferenceSpec, ManagedConfig, ResidencyLedger, GB, import_roster,
};
use clawft_kernel::workload_runtime::types::{
    InstanceHandle, RunMode, VerifiedWorkload, WorkloadConfig, WorkloadRuntime,
};
use clawft_kernel::workload_runtime::{HostContract, WorkloadHost};
use tokio::sync::Mutex;
use tracing::{info, warn};

pub use crate::infer_cfg::{CONFIG_FILE, FileCfg, MeshCfg, RoleCfg, load_config};
use crate::infer_cfg::{DEFAULT_ADVERT, DEFAULT_SYNC, Resolved, flavor_key, resolve_roles};

/// What became of a role's proxy.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProxyState {
    /// No `proxy_port`.
    Off,
    /// Listening here (loopback).
    Listening(SocketAddr),
    /// Listening beyond loopback, behind a bearer token and a permit.
    Exposed(SocketAddr),
    /// The port is held by another server, which stays the address.
    Adopted(SocketAddr),
    /// Not started, with the reason.
    Refused(String),
}

/// Where a managed role stands.
#[derive(Debug, Default)]
pub(crate) struct RunState {
    pub wanted: bool,
    pub started: bool,
    /// Why the role is not running (unplaceable, refused, not available).
    pub reason: Option<String>,
    pub retry_at: Option<Instant>,
}

/// A managed role's host: every place, load, start, stop and unload goes
/// through the workload gate and is chained.
pub(crate) struct Managed {
    pub host: WorkloadHost,
}

pub(crate) struct RoleState {
    pub cfg: RoleCfg,
    pub spec: InferenceSpec,
    pub rt: Arc<InferRuntime>,
    pub managed: Option<Managed>,
    pub handle: Mutex<Option<InstanceHandle>>,
    pub run: std::sync::Mutex<RunState>,
    pub proxy: std::sync::Mutex<ProxyState>,
    pub _proxy: std::sync::Mutex<Option<InferProxy>>,
    /// The listener beyond loopback, when configured.
    pub exposed_proxy: std::sync::Mutex<Option<ProxyState>>,
    pub _exposed: std::sync::Mutex<Option<InferProxy>>,
}

/// Everything the daemon holds for inference placement.
pub struct InferState {
    pub(crate) node_id: String,
    pub(crate) table: Arc<PlacementTable>,
    pub(crate) hub: Option<Arc<InferHub>>,
    pub(crate) roles: Vec<RoleState>,
    pub(crate) mesh_note: String,
    pub(crate) ledger: Arc<ResidencyLedger>,
    pub(crate) skipped_roster: Vec<(String, String)>,
    pub(crate) stopping: AtomicBool,
}

static STATE: OnceLock<Arc<InferState>> = OnceLock::new();

/// The running state, when inference placement is on.
pub fn state() -> Option<&'static Arc<InferState>> {
    STATE.get()
}

impl InferState {
    /// One role's status entry (tests).
    #[cfg(test)]
    pub(crate) fn role_json_for_test(&self, role: &str) -> serde_json::Value {
        self.role_json(self.find(role).expect("role"))
    }

    /// Wakes on every placement change.
    pub fn table_changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.table.subscribe()
    }
}

pub(crate) fn wl_cfg(node_id: &str) -> WorkloadConfig {
    WorkloadConfig {
        mode: RunMode::Listener,
        args: vec![],
        host: HostContract::default_feed(),
        node_id: node_id.into(),
    }
}

pub(crate) fn workload(spec: &InferenceSpec) -> Result<VerifiedWorkload, String> {
    VerifiedWorkload::inference(spec.clone()).map_err(|e| e.to_string())
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Where the daemon's pieces come from, for [`init`].
pub struct InitParts<'a> {
    /// Runtime dir holding `inference.json`.
    pub dir: &'a Path,
    /// This node's id.
    pub node_id: String,
    /// The mesh runtime this daemon owns (None in service mode or without a mesh).
    pub mesh: Option<Arc<MeshRuntime>>,
    /// True when the machine mesh service owns the mesh (ADR-103).
    pub service_mode: bool,
    /// Where binds, refusals and allowlist changes are recorded.
    pub audit: Option<Arc<dyn ProxyAudit>>,
    /// Proxy and forwarding limits.
    pub limits: ProxyLimits,
    /// The workload gate (managed roles and exposure beyond loopback need it).
    pub gate: Option<Arc<WorkloadGate>>,
    /// The chain (the host chains runtime actions).
    pub chain: Option<Arc<ChainManager>>,
}

fn managed_for(
    r: &Resolved,
    dir: &Path,
    cfg: &FileCfg,
    registry: &Result<Arc<ModelRegistry>, String>,
    ledger: &Arc<ResidencyLedger>,
    p: &InitParts<'_>,
) -> Result<(Arc<InferRuntime>, Managed), String> {
    let gate = p.gate.clone().ok_or("no workload governance gate: managed roles are governed")?;
    let registry = registry.clone()?;
    let mut mc = ManagedConfig::new(registry, dir.join("infer").join("instances")).with_ledger(ledger.clone());
    if r.spec.runtime != clawft_kernel::workload_runtime::infer::InferFlavor::Ollama {
        let key = flavor_key(r.spec.runtime);
        let prog = cfg
            .serve_programs
            .get(key)
            .ok_or_else(|| format!("no serve_programs entry for {key} (the launcher path is configured, never guessed)"))?;
        let meta = std::fs::metadata(prog).map_err(|e| format!("serve program {prog}: {e}"))?;
        {
            use std::os::unix::fs::MetadataExt;
            if !meta.is_file() || meta.mode() & 0o111 == 0 {
                return Err(format!("serve program {prog} is not an executable file"));
            }
            // The daemon runs this as itself: only its own user or root may
            // own it, and nobody else may be able to replace it.
            let me = nix::unistd::geteuid().as_raw();
            if meta.uid() != me && meta.uid() != 0 {
                return Err(format!("serve program {prog} is owned by neither the daemon's user nor root"));
            }
            if meta.mode() & 0o022 != 0 {
                return Err(format!("serve program {prog} must not be writable by group or others"));
            }
        }
        mc = mc.with_serve_program(prog);
    }
    let rt = Arc::new(InferRuntime::new(InferConfig::managed(r.spec.runtime, mc)));
    let mut host = WorkloadHost::new(rt.clone() as Arc<dyn WorkloadRuntime>, gate, crate::infer_expose::PRINCIPAL, NodeTrustTier::Pinned);
    if let Some(c) = &p.chain {
        host = host.with_chain(c.clone());
    }
    Ok((rt, Managed { host }))
}

/// Read the roster file the operator named: a regular file (checked before
/// and after opening, so a FIFO or device is never read), and never more
/// than the importer's cap.
fn read_roster(path: &str) -> Result<String, String> {
    use std::io::Read;
    let max = clawft_kernel::workload_runtime::infer::roster::MAX_ROSTER_BYTES;
    let fail = |m: String| format!("{CONFIG_FILE}: roster {path}: {m}");
    let before = std::fs::metadata(path).map_err(|e| fail(e.to_string()))?;
    if !before.is_file() {
        return Err(fail("not a regular file".into()));
    }
    let file = std::fs::File::open(path).map_err(|e| fail(e.to_string()))?;
    let meta = file.metadata().map_err(|e| fail(e.to_string()))?;
    if !meta.is_file() {
        return Err(fail("not a regular file".into()));
    }
    if meta.len() > max as u64 {
        return Err(fail(format!("over {max} bytes")));
    }
    let mut text = String::new();
    file.take(max as u64 + 1).read_to_string(&mut text).map_err(|e| fail(e.to_string()))?;
    if text.len() > max {
        return Err(fail(format!("over {max} bytes")));
    }
    Ok(text)
}

/// The resolver consumers use. A provider role (`local`, OpenAI-compatible
/// over HTTP) may resolve to a role served by another node (through this
/// node's proxy). Any other named role (the voice TTS speaks Ollama's native
/// API, which the mesh does not carry to peers) resolves only to a server on
/// this node, and otherwise the consumer keeps its own configured endpoint.
pub(crate) fn role_resolver(
    table: Arc<PlacementTable>,
    provider_roles: std::collections::HashSet<String>,
) -> clawft_types::placement::roles::RoleResolver {
    Arc::new(move |role: &str| {
        if provider_roles.contains(role) {
            table.base_url_for_role(role)
        } else {
            table.local_base_for_role(role)
        }
    })
}

/// Build the placement state from `<runtime>/inference.json` and run one
/// local pass, without installing anything process-wide. `Ok(None)` when
/// the file is absent (the default: off).
pub async fn build(p: InitParts<'_>) -> Result<Option<(Arc<InferState>, FileCfg)>, String> {
    let Some(cfg) = load_config(p.dir)? else { return Ok(None) };
    let imported = match &cfg.roster {
        Some(r) => {
            Some(import_roster(&read_roster(&r.file)?, &r.overlay)?)
        }
        None => None,
    };
    let resolved = resolve_roles(&cfg, imported.as_ref())?;

    let upstream = Arc::new(Upstream::new(p.limits.clone()).map_err(|e| e.to_string())?);
    let (hub, mesh_note) = match (&p.mesh, p.service_mode) {
        (Some(rt), false) => {
            let hub = InferHub::new(rt.clone(), upstream.clone(), Arc::new(ServeGate::default()), p.audit.clone());
            (Some(hub), "hub: adverts and requests travel as control messages".to_string())
        }
        (_, true) => (
            None,
            "unavailable in service mode: the mesh service hands this daemon deliveries as an \
             unverified peer, so it holds no admission grant to serve or trust a remote node on"
                .to_string(),
        ),
        (None, false) => (None, "unavailable: this daemon has no mesh runtime".to_string()),
    };
    let dialer = hub.clone().map(|h| h as Arc<dyn clawft_kernel::infer_proxy::MeshDialer>);
    let mut table = PlacementTable::new(p.node_id.clone(), dialer, p.audit.clone());
    // ADR-120: the daemon's shared, persisted exposure registry (in-memory when absent).
    if let Some(reg) = crate::expose_rt::global() {
        table = table.with_exposures(reg);
    }
    let table = Arc::new(table);
    if let Some(h) = &hub {
        h.attach(table.clone());
        h.install();
        for role in &cfg.mesh.expose {
            table.expose_to_mesh(role, true);
        }
        for (role, peers) in &cfg.mesh.serve_peers {
            peers.iter().for_each(|n| table.allow_mesh_peer(role, n, true));
        }
        for (role, nodes) in &cfg.mesh.remote_nodes {
            nodes.iter().for_each(|n| table.allow_remote_node(role, n, true));
        }
    } else if !cfg.mesh.expose.is_empty() || !cfg.mesh.serve_peers.is_empty() || !cfg.mesh.remote_nodes.is_empty() {
        warn!(reason = %mesh_note, "{CONFIG_FILE}: mesh settings ignored");
    }

    // One ledger for every managed role: the unified-memory budget and the
    // co-residency rule.
    let ledger = Arc::new(ResidencyLedger::new(cfg.budget_gb.map(|g| (g * GB as f64) as u64)));
    let registry: Result<Arc<ModelRegistry>, String> = if resolved.iter().any(|r| r.managed) {
        (|| {
            let reg = ModelRegistry::open(p.dir.join("models").join("registry.json")).map_err(|e| format!("model registry: {e}"))?;
            let anchors = crate::workload_place_policy::load_anchors(p.dir)?;
            reg.set_trust(ModelTrust::new(anchors));
            Ok(Arc::new(reg))
        })()
    } else {
        Err("no managed roles".into())
    };

    let mut roles = Vec::new();
    for r in &resolved {
        let mut run = RunState { wanted: r.cfg.autostart && r.managed, ..RunState::default() };
        let (rt, managed) = if r.managed {
            match managed_for(r, p.dir, &cfg, &registry, &ledger, &p) {
                Ok((rt, m)) => (rt, Some(m)),
                Err(why) => {
                    warn!(role = %r.cfg.role, %why, "managed role not available");
                    run.reason = Some(format!("not available: {why}"));
                    run.wanted = false;
                    (Arc::new(InferRuntime::new(InferConfig::adopted(r.spec.runtime))), None)
                }
            }
        } else {
            (Arc::new(InferRuntime::new(InferConfig::adopted(r.spec.runtime))), None)
        };
        let px = crate::infer_managed::start_proxies(r, &table, &p).await;
        let (proxy, handle) = px.local;
        let (exposed_state, exposed_handle) = match px.exposed {
            Some((s, h)) => (Some(s), h),
            None => (None, None),
        };
        roles.push(RoleState {
            cfg: r.cfg.clone(),
            spec: r.spec.clone(),
            rt,
            managed,
            handle: Mutex::new(None),
            run: std::sync::Mutex::new(run),
            proxy: std::sync::Mutex::new(proxy),
            _proxy: std::sync::Mutex::new(handle),
            exposed_proxy: std::sync::Mutex::new(exposed_state),
            _exposed: std::sync::Mutex::new(exposed_handle),
        });
    }
    let state = Arc::new(InferState {
        node_id: p.node_id,
        table: table.clone(),
        hub: hub.clone(),
        roles,
        mesh_note,
        ledger,
        skipped_roster: imported.map(|i| i.skipped).unwrap_or_default(),
        stopping: AtomicBool::new(false),
    });
    state.sync_once().await;
    Ok(Some((state, cfg)))
}

/// [`build`], then make it the daemon's: the global state the `infer.*`
/// verbs read, the hooks the consumers follow, cache invalidation, and the
/// periodic sync and announcement loop.
pub async fn init(p: InitParts<'_>) -> Result<Option<Arc<InferState>>, String> {
    let Some((state, cfg)) = build(p).await? else { return Ok(None) };
    let table = state.table.clone();

    // Consumers: the `local` provider (agents, the LLM service) and named
    // roles (the voice TTS) follow placement unless an explicit setting
    // chose their endpoint (checked where each consumer builds its client).
    let provider_roles: HashMap<String, String> = cfg
        .roles
        .iter()
        .filter_map(|r| r.provider.clone().map(|p| (p, r.role.clone())))
        .collect();
    let resolver = role_resolver(table.clone(), provider_roles.values().cloned().collect());
    clawft_types::placement::roles::install(resolver.clone(), provider_roles.clone());
    if !provider_roles.is_empty() {
        clawft_core::placement_hook::install(resolver, Duration::from_secs(5), provider_roles);
    }
    // Cached router answers follow every table change.
    let mut rx = table.subscribe();
    tokio::spawn(async move {
        while rx.changed().await.is_ok() {
            clawft_core::placement_hook::invalidate_all();
        }
    });
    let (st, sync, advert) = (
        state.clone(),
        cfg.sync_secs.unwrap_or(DEFAULT_SYNC).max(1),
        cfg.advert_secs.unwrap_or(DEFAULT_ADVERT).max(1),
    );
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(sync));
        let mut since_advert = 0u64;
        loop {
            tick.tick().await;
            if st.stopping.load(Ordering::Relaxed) {
                break;
            }
            st.sync_once().await;
            since_advert += sync;
            if since_advert >= advert {
                since_advert = 0;
                if let Some(h) = &st.hub {
                    h.announce(now_secs()).await;
                }
            }
        }
    });
    info!(roles = cfg.roles.len(), mesh = %state.mesh_note, "inference placement on");
    let _ = STATE.set(state.clone());
    Ok(Some(state))
}
