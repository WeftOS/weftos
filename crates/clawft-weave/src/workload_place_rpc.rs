//! Daemon side of the placement control plane (ADR-099 sections 3 and 7,
//! card mesh-placement-12): `workload.place`, `workload.explain`,
//! `workload.status`, `workload.stop`, `workload.logs` and
//! `workload.unload {instance_id}`.
//!
//! The daemon is the controller. It signs with its node key
//! (`<runtime>/node.key`), or in service mode with its placement control key
//! (`placement_boot`, ADR-106 phase 3), chains every decision on the kernel chain, and
//! reads operator policy from the runtime directory (see
//! `workload_place_policy`: permits, trust, peers bound to node keys, the
//! optional container adapter and Seeds). Known targets and placements are
//! persisted in `workload-placements.json`, so instances stay manageable
//! across a daemon restart.
//!
//! This node is a candidate too, through its own `workload-host` (native
//! adapter, the node's re-probed signed facts), in process.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use clawft_kernel::artifact_store::ArtifactStore;
use clawft_kernel::boot::Kernel;
use clawft_kernel::chain::ChainManager;
use clawft_kernel::mesh_artifact::{ArtifactExchange, ExchangeConfig};
use clawft_kernel::workload_ctl::msg::method;
use clawft_kernel::workload_ctl::{
    CtlConfig, FactsSource, LifecyclePolicy, MeshConnector, PlaceOrder, PlacementControlPlane,
    StorePinOrder, WorkloadHostService,
};
use clawft_kernel::workload_governance::WorkloadGate;
use clawft_kernel::workload_runtime::RunMode;
use clawft_platform::NativePlatform;
use clawft_rpc::Response;
use clawft_types::placement::TrustTier;
use ed25519_dalek::SigningKey;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{OnceCell, RwLock};

use crate::workload_host_serve::{HostParts, load_host_config, local_host, serve};
pub use crate::workload_place_policy::{
    PEERS_FILE, PERMITS_FILE, STATE_FILE, TRUST_FILE, load_anchors, load_peers, load_permits,
};
use crate::workload_place_policy::{load_container, load_seeds};

/// Methods served here (the rest of `workload.*` is in `workload_rpc`).
pub const METHODS: &[&str] = &[
    "workload.revoke",
    "workload.place",
    "workload.explain",
    "workload.status",
    "workload.stop",
    "workload.logs",
];

struct Boot {
    key: SigningKey,
    runtime_dir: PathBuf,
    /// The node id peers address this node by on the mesh: the key's own id
    /// when the daemon holds the node key, the machine's in service mode.
    mesh_node_id: String,
}

static BOOT: OnceLock<Boot> = OnceLock::new();
static PLANE: OnceCell<Arc<PlacementControlPlane>> = OnceCell::const_new();
/// The effective-rules hash (hex) the control plane's gate was built from, or
/// `None` on a kernel without a governance overlay. Compared on every call:
/// the gate is fixed at build, so a later governance push must not leave an
/// older, possibly looser, gate deciding (ADR-103 D8: applies after a restart).
static BUILT_RULES: OnceLock<Option<String>> = OnceLock::new();
/// What `workload.revoke` and the mesh revocation hook act on, set once the
/// control plane is built.
static REVOKER: OnceLock<Arc<crate::workload_revoke_rpc::Revoker>> = OnceLock::new();

/// When the plane last failed to build, for the revoke verb's backoff.
static BUILD_FAILED: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);
/// How long the revoke verb waits before trying to build the plane again.
const BUILD_RETRY: std::time::Duration = std::time::Duration::from_secs(30);

/// The daemon's revoker, made on first use from the kernel's list (so a
/// notice or the verb that arrives before placement is up still has one).
fn revoker_for(
    list: Arc<clawft_kernel::revocation::RevocationList>,
) -> Arc<crate::workload_revoke_rpc::Revoker> {
    REVOKER
        .get_or_init(|| Arc::new(crate::workload_revoke_rpc::Revoker::new(list)))
        .clone()
}
/// This node's `workload-host` (in-process target and, when configured,
/// served to other nodes).
static HOST: OnceLock<Arc<WorkloadHostService>> = OnceLock::new();
/// Keeps the cog ingest bridge listening for the daemon's life.
static INGEST: OnceLock<crate::cog_ingest_serve::IngestRuntime> = OnceLock::new();
/// Where it is served, once serving started.
static SERVED: OnceLock<SocketAddr> = OnceLock::new();
const LOCAL_ADDR: &str = "mem://local";
/// How often this node's `workload-host` polls instance health.
const SUPERVISE_TICK: std::time::Duration = std::time::Duration::from_secs(5);
/// How often the controller checks for lost nodes.
const LIFECYCLE_TICK: std::time::Duration = std::time::Duration::from_secs(10);

/// Record the daemon key and runtime dir (call once at daemon boot).
pub fn init(key: SigningKey, runtime_dir: PathBuf) {
    let id = clawft_kernel::node_id_from_pubkey(&key.verifying_key().to_bytes());
    init_with_mesh_id(key, runtime_dir, id);
}

/// [`init`] where the signing key is not the mesh identity (service mode:
/// the control key signs, the machine's node id is what peers address).
pub fn init_with_mesh_id(key: SigningKey, runtime_dir: PathBuf, mesh_node_id: String) {
    let _ = BOOT.set(Boot { key, runtime_dir, mesh_node_id });
}

/// This node's in-process `workload-host`, once placement is built.
pub fn in_process_host() -> Option<Arc<WorkloadHostService>> {
    HOST.get().cloned()
}

/// Read-only controller view for `fleet.snapshot`: known targets and placed
/// instances with the controller's own lifecycle record. Memory only, no
/// peer is contacted (`workload.status` is a Write verb because it does
/// contact them). `None` until the control plane is built.
pub fn controller_view() -> Option<Value> {
    let plane = PLANE.get()?;
    let instances: Vec<Value> = plane
        .placements()
        .into_iter()
        .map(|rec| {
            let lifecycle = plane.life_summary(&rec.instance_id);
            json!({ "placement": rec, "lifecycle": lifecycle })
        })
        .collect();
    Some(json!({
        "controller": plane.node_id(),
        "targets": plane.targets(),
        "instances": instances,
        "unsettled": plane.unsettled().len(),
    }))
}

/// The daemon's runtime directory (where the operator's policy files are),
/// once [`init`] ran.
pub fn runtime_dir() -> Option<PathBuf> {
    BOOT.get().map(|b| b.runtime_dir.clone())
}

/// True for the methods this module serves (`unload` only with an instance id).
pub fn handles(m: &str, params: &Value) -> bool {
    METHODS.contains(&m) || (m == "workload.unload" && params.get("instance_id").is_some())
}

/// Make `plane`'s remote targets match `workload-peers.json` (read on
/// every call, so removing a peer or lowering its tier takes effect
/// without a restart; see `apply_operator_peers`).
pub async fn sync_peers(plane: &PlacementControlPlane, dir: &Path) -> Result<(), String> {
    let peers = load_peers(dir)?;
    for (addr, e) in plane.apply_operator_peers(&peers, &[LOCAL_ADDR]).await {
        tracing::warn!(peer = %addr, error = %e, "workload peer not reachable");
    }
    Ok(())
}

/// Refuse when the governance in force is no longer the governance the gate
/// was built from. Fail closed: the placement gate does not follow a push, so
/// after one it declines to decide until the kernel restarts.
fn governance_changed(built: Option<&Option<String>>, now: Option<&str>) -> Result<(), String> {
    match built {
        Some(b) if b.as_deref() != now => Err(
            "governance changed since placement started; restart this kernel to apply it (ADR-103 D8)"
                .into(),
        ),
        _ => Ok(()),
    }
}

/// The gate for placement decisions. On a project kernel (ADR-103 D8) it is
/// built from the effective rules (parent policy plus overlay) as of this
/// build, so an overlay deny on `workload.*` applies. It does not follow a
/// later push: [`dispatch`] refuses (`governance_changed`) until restart.
/// Permits, the revocation list and the chain are attached by
/// [`crate::workload_gate::build`], shared with the catalog verbs.
fn gate(
    dir: &Path,
    chain: &Arc<ChainManager>,
    effective: Option<(Vec<clawft_kernel::governance::GovernanceRule>, f64, bool)>,
    revocations: Arc<clawft_kernel::revocation::RevocationList>,
) -> Result<Arc<WorkloadGate>, String> {
    crate::workload_gate::build(dir, chain, effective, revocations)
}

/// Give the exchange this node's revocation list (a revoked package, signer
/// or artifact hash stops seeding at once) and, with a mesh, start taking
/// signed revocation notices from peers. A notice that was new here also
/// stops and unloads what this node is running from the revoked subject
/// (`Revoker::enforce`, on a task: the notice sink must not block). The
/// handle the runtime keeps is the only owner of the exchange needed: it
/// lives as long as the runtime does, so the daemon keeps a second one for
/// the operator verb.
fn wire_revocations(
    ex: &Arc<ArtifactExchange>,
    list: Arc<clawft_kernel::revocation::RevocationList>,
    anchors: &clawft_kernel::workload_pkg::TrustAnchors,
    mesh: Option<Arc<clawft_kernel::mesh_runtime::MeshRuntime>>,
    dir: &Path,
) -> Option<Arc<clawft_kernel::mesh_swarm_revoke::RevocationExchange>> {
    let notices = match mesh {
        Some(rt) => {
            let x = clawft_kernel::mesh_swarm_revoke::RevocationExchange::start(
                ex.clone(),
                list,
                anchors.clone(),
                rt,
            );
            x.set_on_applied(Arc::new(|_| enforce_in_background()));
            // Notices already verified are kept and replayed to any peer that
            // joins or recovers, so a node that was down when one was issued
            // still receives it (and unloads what it runs from it).
            let restored = x.with_log_file(dir.join("revocation-notices.json"));
            if restored > 0 {
                tracing::info!(restored, "revocation notices restored for replay");
            }
            Some(x)
        }
        None => {
            ex.set_revocations(list);
            None
        }
    };
    let swept = ex.apply_revocations();
    if !swept.is_empty() {
        tracing::warn!(n = swept.len(), "revoked artifacts evicted at startup");
    }
    notices
}

/// Run the forced unload for a revocation applied from the mesh, off the
/// task that received it.
fn enforce_in_background() {
    let Ok(rt) = tokio::runtime::Handle::try_current() else {
        tracing::warn!("revocation applied outside a runtime: running instances not swept");
        return;
    };
    rt.spawn(async {
        if let Some(r) = REVOKER.get().cloned() {
            let forced = r.enforce().await;
            if !forced.is_empty() {
                tracing::warn!(n = forced.len(), "instances stopped by a mesh revocation");
            }
        }
    });
}

/// The node's one licence exchange (ADR-106 phase 1b), started by the first
/// successful [`wire_licence`] of a build; a rebuild reuses it rather than
/// open a second approval store or a second set of control sinks.
static LICENCE_EXCHANGE: OnceLock<Arc<clawft_kernel::licence::LicenceExchange>> = OnceLock::new();

/// The node's licence exchange, once placement has started it (`None` before,
/// without a mesh, or without the boot-owned licence runtime). The bind RPCs
/// flood accepted bindings through it.
pub fn licence_exchange() -> Option<Arc<clawft_kernel::licence::LicenceExchange>> {
    LICENCE_EXCHANGE.get().cloned()
}

/// The mesh the licence exchange runs over: the kernel's own runtime when it
/// has one (collapsed), else the machine mesh service's links (service mode),
/// never both.
pub(crate) fn licence_links(
    mesh: Option<Arc<clawft_kernel::mesh_runtime::MeshRuntime>>,
) -> Option<Arc<dyn clawft_kernel::licence::LicenceLinks>> {
    match mesh {
        Some(rt) => Some(rt),
        None => crate::cog_swarm::service_licence_links()
            .map(|l| l as Arc<dyn clawft_kernel::licence::LicenceLinks>),
    }
}

/// The node's licence exchange, started once over `policy` (the boot-owned
/// one) and `links`; later calls return the first. Hands it to the steward
/// relay as its flood.
/// Serializes [`ensure_licence`]: check, wire and set as one step, so two
/// concurrent callers cannot each start an exchange (and install its sinks).
static LICENCE_START: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// `cell`'s value, made by `make` at most once even under concurrent callers
/// (`lock` held from the check to the set). `None` when `make` declines.
fn once_guarded<T: Clone>(
    cell: &OnceLock<T>,
    lock: &std::sync::Mutex<()>,
    make: impl FnOnce() -> Option<T>,
) -> Option<T> {
    if let Some(x) = cell.get() {
        return Some(x.clone());
    }
    let _g = lock.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(x) = cell.get() {
        return Some(x.clone());
    }
    let x = make()?;
    Some(cell.get_or_init(|| x).clone())
}

pub(crate) fn ensure_licence(
    dir: &Path,
    policy: Option<&Arc<clawft_kernel::licence::MeshCheckoutPolicy>>,
    anchors: &clawft_kernel::workload_pkg::TrustAnchors,
    chain: &Arc<ChainManager>,
    links: Option<Arc<dyn clawft_kernel::licence::LicenceLinks>>,
    posture: clawft_kernel::licence::PostureFn,
) -> Option<Arc<clawft_kernel::licence::LicenceExchange>> {
    let x = once_guarded(&LICENCE_EXCHANGE, &LICENCE_START, || {
        wire_licence(dir, policy?, anchors, chain, links, posture)
    })?;
    crate::cog_swarm::set_grant_flood(x.clone());
    Some(x)
}

#[cfg(test)]
mod ensure_once_tests {
    use super::once_guarded;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier, Mutex, OnceLock};

    #[test]
    fn concurrent_callers_make_exactly_one() {
        let cell: Arc<OnceLock<Arc<usize>>> = Arc::default();
        let lock = Arc::new(Mutex::new(()));
        let made = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new(Barrier::new(8));
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let (cell, lock, made, gate) = (cell.clone(), lock.clone(), made.clone(), gate.clone());
                std::thread::spawn(move || {
                    gate.wait();
                    once_guarded(&cell, &lock, || {
                        made.fetch_add(1, Ordering::SeqCst);
                        std::thread::sleep(std::time::Duration::from_millis(20));
                        Some(Arc::new(i))
                    })
                    .unwrap()
                })
            })
            .collect();
        let got: Vec<Arc<usize>> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(made.load(Ordering::SeqCst), 1, "one exchange built (and so one set of sinks installed)");
        assert!(got.iter().all(|x| Arc::ptr_eq(x, &got[0])), "every caller gets that one");
        // A declined make leaves the cell empty for a later caller.
        let empty: OnceLock<Arc<usize>> = OnceLock::new();
        assert!(once_guarded(&empty, &lock, || None).is_none() && empty.get().is_none());
    }
}

/// Start the licence exchange (ADR-106 phase 1b) over the boot-owned store
/// and policy (`licence_boot`; the caller passes them in): open the approval
/// store, and with a mesh carry bindings, grants
/// and approvals by flood and catch-up sync. Inert while the local mesh id is
/// unset (no `mesh_nonce` yet), exactly like the policy. Floods and sync go
/// to licensed peers only (`CtxAdmission`: verified and class `node`), so an
/// admitted leaf gets none. The store tick is `licence_boot`'s.
fn wire_licence(
    dir: &Path,
    policy: &Arc<clawft_kernel::licence::MeshCheckoutPolicy>,
    anchors: &clawft_kernel::workload_pkg::TrustAnchors,
    chain: &Arc<ChainManager>,
    links: Option<Arc<dyn clawft_kernel::licence::LicenceLinks>>,
    posture: clawft_kernel::licence::PostureFn,
) -> Option<Arc<clawft_kernel::licence::LicenceExchange>> {
    use clawft_kernel::licence as l;
    // The store's own sink is `licence_boot`'s; this one chains the
    // exchange's events (`sync_bad_signature`).
    let sink: Arc<dyn l::LicenceEventSink> = Arc::new(l::ChainLicenceSink::new(chain.clone()));
    let store = policy.store().clone();
    let runtime = links?;
    let anchors = Arc::new(anchors.clone());
    let approvals = Arc::new(l::ApprovalStore::open_or_poisoned(
        &dir.join("licence"),
        anchors.clone(),
        store.local_mesh_id().clone(),
    ));
    if let Some(why) = approvals.poisoned() {
        tracing::warn!(%why, "approval store unreadable; approvals stay off");
    }
    Some(l::LicenceExchange::start(l::LicenceExchangeParts {
        store,
        approvals,
        anchors,
        runtime,
        posture,
        admission: Arc::new(l::CtxAdmission),
        sink,
        config: l::LicenceExchangeConfig::default(),
    }))
}

async fn build(
    kernel: &Arc<RwLock<Kernel<NativePlatform>>>,
) -> Result<Arc<PlacementControlPlane>, String> {
    let boot = BOOT
        .get()
        .ok_or("placement not initialised (no daemon key)")?;
    let dir = &boot.runtime_dir;
    let k = kernel.read().await;
    let chain = k
        .chain_manager()
        .cloned()
        .ok_or("placement needs the kernel chain (decisions are chained)")?;
    let membership = k.cluster_membership().clone();
    let effective = k.governance_overlay().map(|o| o.effective_rules_and_hash());
    let (effective, built) = match effective {
        Some((rules, hash)) => (Some(rules), Some(hash)),
        None => (None, None),
    };
    let _ = BUILT_RULES.set(built);
    let revocations = k.revocation_list().clone();
    let mesh = k.a2a_router().mesh_runtime().cloned();
    let posture = {
        let p = crate::licence_boot::posture(k.kernel_config().mesh.as_ref(), k.governance_gate().is_some());
        Arc::new(move || p) as clawft_kernel::licence::PostureFn
    };
    let kgate = k.governance_gate().cloned();
    // ADR-122 B3: the broker decides with the same rules (no chain event).
    let bus_rules = effective.clone().or_else(|| {
        kgate.as_ref().and_then(|g| g.governance_snapshot()).map(|s| (s.rules, s.risk_threshold, s.human_approval_required))
    });
    drop(k);
    let pk = boot.key.verifying_key().to_bytes();
    let id = clawft_kernel::node_id_from_pubkey(&pk);
    let anchors = load_anchors(dir)?;
    let gate = gate(dir, &chain, effective, revocations.clone())?;
    // `open_file` indexes the blobs already on disk (installed workloads'
    // files from earlier runs), so the exchange finds them present and never
    // takes ownership of, or evicts, bytes it did not create.
    let store = ArtifactStore::open_file(dir.join("workload-artifacts"))
        .map_err(|e| format!("workload artifact store: {e}"))?;
    // The licence boundary is explicit: what this node may hand to peers is
    // decided by the signed manifests (`redistributable = true`), except for
    // Cognitum cogs under a valid checkout grant of a bound Seed (ADR-106).
    // The policy reads the binding live, so a binding that arrives later
    // needs no restart; with none (or no mesh id yet) it is exactly
    // `ManifestPolicy`. The daemon built it at boot (`licence_boot`), with the
    // mesh id from the configured nonce and the store the bind RPC writes to;
    // that is the node's only grant store. Without that runtime (not
    // installed, or a test harness) an inert policy over the same files stands
    // in, and no licence exchange is started over it.
    let boot_policy = crate::licence_boot::policy();
    let policy = match &boot_policy {
        Some(p) => p.clone(),
        None => clawft_kernel::licence::MeshCheckoutPolicy::open(
            dir,
            anchors.clone(),
            revocations.clone(),
            clawft_kernel::licence::LocalMeshId::unset(),
        ),
    };
    let cfg = ExchangeConfig {
        redistribution: policy.clone(),
        ..ExchangeConfig::default()
    };
    // Peers address this node's swarm by its mesh id (the machine's in service mode).
    let mut ex = ArtifactExchange::new(&boot.mesh_node_id, Arc::new(store), cfg)
        .map_err(|e| e.to_string())?;
    ex.set_chain_manager(chain.clone());
    let ex = Arc::new(ex);
    // The licence exchange (floods and sync) over the boot-owned store, then
    // the swarm transport: artifact sessions and checkout over the machine
    // mesh's stamped deliveries (ADR-106 5.4), late-bound behind the link. A
    // steward relay floods the grants it obtains through the exchange.
    // Only the licence holder (or a collapsed node) starts the exchange here;
    // otherwise holder-follow starts it when the role arrives.
    let licence_policy = boot_policy.as_ref().filter(|_| crate::licence_boot::holder_refusal().is_none());
    ensure_licence(dir, licence_policy, &anchors, &chain, licence_links(mesh.clone()), posture);
    let cog_mesh = crate::cog_swarm::install(&ex, policy.store());
    // ADR-106 phase 3: the steward relay, from `licence-link.json`.
    crate::licence_steward::wire(crate::licence_steward::WireArgs {
        // The mesh id, which the binding names as steward (the machine's in
        // service mode); `id` is the signer's own id (the control key's there).
        dir, key: &boot.key, node_id: boot.mesh_node_id.clone(), store: policy.store(), exchange: &ex,
        chain: &chain, gate: kgate, mesh: &cog_mesh,
    });
    // The revoker exists before the first notice can arrive: a notice that
    // beats the rest of this build is enforced by the sweep at its end.
    let revoker = revoker_for(revocations.clone());
    revoker.set_exchange(ex.clone());
    let notices = wire_revocations(&ex, revocations.clone(), &anchors, mesh, dir);
    let serving = load_host_config(dir)?;
    let container = load_container(dir, &dir.join("workload-containers"))?;
    let bus_container = crate::cogbus_serve::container_route(container.as_ref());
    // `describe` always answers with the facts the daemon re-probes.
    let (fm, fid) = (membership.clone(), id.clone());
    let facts: FactsSource = Arc::new(move || {
        let now = chrono::Utc::now().timestamp().max(0) as u64;
        fm.facts().get(&fid, now).map(|c| c.signed)
    });
    let identity: Arc<dyn clawft_kernel::cog_ingest::ProjectDirectory> =
        Arc::new(crate::cog_ingest_serve::IdentityDirectory);
    // ADR-122 section 9: the broker exists first, so the bridge publishes on
    // it and its store sink serves cogs that never touch the HTTP port.
    let (bus_hooks, bus_activity) = crate::cogbus_serve::hooks_with_activity(dir, bus_rules);
    let broker = crate::cogbus_serve::broker_in(&id, bus_hooks, dir);
    let ingest = match crate::cog_ingest_serve::load_config(dir) {
        Ok(cfg) => match crate::cog_ingest_serve::start_on_bus(
            &cfg,
            &boot.key,
            Some(identity),
            Some(dir.join(crate::cog_ingest_serve::VECTOR_STORE_DIR)),
            Some(broker.clone()),
        )
        .await
        {
            Ok(rt) => Some(rt),
            Err(e) => {
                tracing::warn!(error = %e, "cog ingest not started");
                None
            }
        },
        Err(e) => {
            tracing::warn!(error = %e, "cog ingest config invalid; cogs run without ingest");
            None
        }
    };
    let hooks = ingest.as_ref().map(|r| r.hooks.clone());
    let store_sink = ingest.as_ref().and_then(|r| r.store_sink.clone());
    if let Some(rt) = ingest {
        tracing::info!(bridge = ?rt.bridge_addr, disabled = ?rt.bridge_error, store_owner = ?rt.owner_addr, "cog ingest");
        let _ = INGEST.set(rt);
    }
    let local = Arc::new(local_host(HostParts {
        key: &boot.key,
        dir,
        chain: &chain,
        gate: gate.clone(),
        exchange: ex.clone(),
        anchors: anchors.clone(),
        facts,
        serving: serving.as_ref(),
        container,
        ingest: hooks,
        revocations: revocations.clone(),
    })?);
    // ADR-106 phase 3: grant plus approval before a Cognitum-origin cog runs here.
    local.set_licence_gate(crate::licence_steward::run_gate(policy.store()));
    // Mesh-routed `dashboard.*` calls from an authorised controller key.
    local.set_node_admin(Arc::new(crate::dashboard_rpc::MeshAdmin));
    // ADR-122: the cog data plane broker beside the ingest bridge; down, cogs
    // are placed without WEFTOS_COGBUS.
    if let Some(bus) = crate::cogbus_serve::install_with(broker, dir, store_sink, bus_container).await {
        local.set_cogbus(bus);
    }
    // Put back what was placed here before the daemon restarted: after the
    // cogbus and ingest wiring, so each gets a fresh token like a new placement.
    let restored = local.restore_instances().await;
    if !restored.restored.is_empty() || !restored.failed.is_empty() {
        tracing::info!(restored = restored.restored.len(), failed = restored.failed.len(), "placed instances restored after restart");
    }
    // ADR-122 gap 5: a cog the broker stopped on the bus is stopped here too.
    bus_activity.set_stop_hook(crate::cogbus_serve::stop_hook(Arc::downgrade(&local)));
    let _ = HOST.set(local.clone());
    // ADR-108 P3b: serve `project.fetch` for the projects registered here.
    match clawft_types::runtime_paths::home_dir() {
        Some(home) => {
            let manifests = clawft_rpc::resolve::manifests_dir(&home);
            let fetch = crate::project_fetch_serve::FetchHost::new(dir.clone(), manifests).with_chain(chain.clone());
            let _ = crate::project_fetch_serve::install_global(Arc::new(fetch));
        }
        None => tracing::warn!("no home directory: project.fetch is not served on this node"),
    }
    let conn = Arc::new(MeshConnector::new(true));
    let local_addr = conn.register_local("local", local);
    let seeds = load_seeds(dir, gate.clone(), &chain)?;
    let plane =
        PlacementControlPlane::new(boot.key.clone(), gate, chain, ex.clone(), anchors.clone(), conn)
            .with_membership(membership)
            .with_state_file(dir.join(STATE_FILE))
            .map_err(|e| format!("placement state: {e}"))?;
    for s in seeds {
        plane
            .add_seed(&s.node_id, s.host, s.tier)
            .map_err(|e| format!("seed {}: {e}", s.node_id))?;
    }
    if let Err(e) = plane.add_target(&local_addr, TrustTier::Pinned).await {
        tracing::warn!(error = %e, "this node is not a placement candidate yet (no signed facts?)");
    }
    // Targets restored from the state file are re-described with the key
    // each was learned with.
    plane.refresh().await;
    let plane = Arc::new(plane);
    // The key notices are signed with must be one this node itself trusts to
    // revoke (a peer would refuse any other).
    let pk = boot.key.verifying_key().to_bytes();
    let notice_key = anchors
        .signer(&pk)
        .filter(|k| {
            matches!(
                k.origin,
                clawft_kernel::workload_pkg::KeyOrigin::Operator
                    | clawft_kernel::workload_pkg::KeyOrigin::Weftos
            )
        })
        .map(|_| boot.key.clone());
    revoker.attach(HOST.get().cloned(), plane.clone(), notices, notice_key);
    // Startup sweep (a revocation that arrived while this node was down or
    // before placement was up) and the periodic retry of failed teardowns.
    let swept = revoker.enforce().await;
    if !swept.is_empty() {
        tracing::warn!(n = swept.len(), "instances stopped by a revocation at startup");
    }
    revoker.spawn_tick(crate::workload_revoke_rpc::ENFORCE_TICK);
    // Health heartbeat and bounded restarts on this node, and the controller's
    // node-loss detection and rescheduling.
    if let Some(h) = HOST.get() {
        h.spawn_supervisor(SUPERVISE_TICK);
    }
    plane.spawn_lifecycle(LIFECYCLE_TICK);
    Ok(plane)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlaceParams {
    package_dir: PathBuf,
    #[serde(default)]
    peers: Vec<String>,
    #[serde(default)]
    pin: Option<String>,
    #[serde(default)]
    prefer: Vec<String>,
    #[serde(default)]
    avoid: Vec<String>,
    #[serde(default)]
    allow_emulated: bool,
    /// `once`, `listener`, or absent for `interval`.
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    interval: Option<u32>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    csi_port: Option<u16>,
    #[serde(default)]
    start: Option<bool>,
    /// Project the cog is placed for (a project id). Its ingested vectors
    /// go to that project's store; absent, to the placing controller's.
    /// Refused here unless the project is registered and not revoked, and
    /// again by the target host unless this controller may place for it.
    #[serde(default)]
    project: Option<String>,
    /// `false`: never reschedule this instance onto another node when its
    /// node is lost (an alert is chained instead). Default `true`; a `pin`
    /// also holds it in place.
    #[serde(default)]
    migratable: Option<bool>,
}

/// `workload.place {store_pin: ...}`: an operator-pinned store cog on a
/// Seed's operator-assigned node id (ADR-100 section 5).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StorePinParams {
    node_id: String,
    #[serde(default = "cognitum")]
    registry: String,
    id: String,
    version: String,
    #[serde(default)]
    sha256: Option<String>,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    interval: Option<u32>,
    #[serde(default)]
    csi_port: Option<u16>,
    #[serde(default)]
    start: Option<bool>,
}

fn cognitum() -> String {
    "cognitum".into()
}

fn run_mode(mode: Option<&str>, interval: Option<u32>) -> Result<RunMode, String> {
    Ok(match mode {
        Some("once") => RunMode::Once,
        Some("listener") => RunMode::Listener,
        None | Some("interval") => RunMode::Interval {
            secs: interval.unwrap_or(10),
        },
        Some(other) => return Err(format!("unknown mode {other:?} (once|interval|listener)")),
    })
}

fn store_pin_order(v: Value) -> Result<StorePinOrder, String> {
    let p: StorePinParams =
        serde_json::from_value(v).map_err(|e| format!("invalid store_pin: {e}"))?;
    Ok(StorePinOrder {
        node_id: p.node_id,
        registry: p.registry,
        id: p.id,
        version: p.version,
        sha256: p.sha256,
        config: CtlConfig {
            mode: run_mode(p.mode.as_deref(), p.interval)?,
            args: Vec::new(),
            csi_port: p.csi_port.unwrap_or(5006),
        },
        start: p.start.unwrap_or(true),
    })
}

fn order(p: PlaceParams, dry_run: bool) -> Result<(PlaceOrder, Vec<String>), String> {
    let mode = run_mode(p.mode.as_deref(), p.interval)?;
    if let Some(proj) = &p.project {
        crate::cog_ingest_serve::check_project(proj)?;
    }
    if !p.package_dir.is_absolute() {
        return Err(format!(
            "package_dir {} must be absolute (the daemon does not share the caller's working directory)",
            p.package_dir.display()
        ));
    }
    if !p.package_dir.is_dir() {
        return Err(format!(
            "package_dir {} is not a directory",
            p.package_dir.display()
        ));
    }
    Ok((
        PlaceOrder {
            package_dir: p.package_dir,
            config: CtlConfig {
                mode,
                args: p.args,
                csi_port: p.csi_port.unwrap_or(5006),
            },
            pin: p.pin,
            prefer: p.prefer,
            avoid: p.avoid,
            allow_emulated: p.allow_emulated,
            start: p.start.unwrap_or(true),
            dry_run,
            project_id: p.project,
        },
        p.peers,
    ))
}

fn instance_id(params: &Value, m: &str) -> Result<String, String> {
    params
        .get("instance_id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| format!("{m} requires a string 'instance_id'"))
}

/// Serve one placement method on `plane`.
pub async fn route(plane: &PlacementControlPlane, m: &str, params: Value) -> Response {
    let res: Result<Value, String> = async {
        match m {
            "workload.place" if params.get("store_pin").is_some() => {
                let o = store_pin_order(params["store_pin"].clone())?;
                let rec = plane.place_store_pin(&o).await.map_err(|e| e.to_string())?;
                Ok(json!({ "placed": rec, "route": clawft_kernel::workload_ctl::SEED_ROUTE }))
            }
            "workload.place" | "workload.explain" => {
                let p: PlaceParams = serde_json::from_value(params).map_err(|e| format!("invalid {m} params: {e}"))?;
                let policy = LifecyclePolicy {
                    migratable: p.migratable.unwrap_or(true),
                    ..LifecyclePolicy::default()
                };
                let (o, peers) = order(p, m == "workload.explain")?;
                // A caller cannot assign trust: a peer named in a request
                // is `discovered` unless the operator already knows it
                // (workload-peers.json assigns the tier).
                let known: Vec<String> = plane.targets().into_iter().map(|t| t.addr).collect();
                for addr in peers.into_iter().filter(|a| !known.contains(a)) {
                    plane.add_target(&addr, TrustTier::Discovered).await.map_err(|e| format!("peer {addr}: {e}"))?;
                }
                plane.refresh().await;
                let r = plane.place_with(&o, policy).await.map_err(|e| e.to_string())?;
                serde_json::to_value(r).map_err(|e| e.to_string())
            }
            "workload.status" if params.get("instance_id").is_none() => {
                plane.settle_unsettled().await;
                let mut rows = Vec::new();
                for rec in plane.placements() {
                    let st = plane.instance(method::STATUS, &rec.instance_id).await;
                    rows.push(json!({ "placement": rec, "status": st.map_err(|e| e.to_string()),
                                      "lifecycle": plane.lifecycle_of(&rec.instance_id) }));
                }
                let host = HOST.get().map(|h| h.advertisement());
                Ok(json!({ "controller": plane.node_id(), "targets": plane.targets(), "instances": rows,
                           "unsettled": plane.unsettled(),
                           "workload_host": host, "served_on": SERVED.get().map(|a| a.to_string()) }))
            }
            "workload.status" | "workload.stop" | "workload.logs" | "workload.unload" => {
                let id = instance_id(&params, m)?;
                plane.instance(m, &id).await.map_err(|e| e.to_string())
            }
            other => Err(format!("{other} is not a placement method")),
        }
    }
    .await;
    match res {
        Ok(v) => Response::success(v),
        Err(e) => Response::error(e),
    }
}

/// Build the control plane if it is not built yet (the cog mesh, the licence
/// exchange and the steward relay come with it).
pub async fn ensure_started(kernel: Arc<RwLock<Kernel<NativePlatform>>>) -> Result<(), String> {
    PLANE.get_or_try_init(|| build(&kernel)).await.map(|_| ())
}

/// Daemon entry: lazily builds the control plane, then serves `m`.
pub async fn dispatch(
    m: &str,
    params: Value,
    kernel: Arc<RwLock<Kernel<NativePlatform>>>,
) -> Response {
    dispatch_checked(m, params, kernel, BUILT_RULES.get()).await
}

/// `workload.revoke` is served before every guard below and does not need
/// the control plane: it is the incident verb. A broken permits, trust,
/// container, host or Seed file stops `build`, a governance push stops
/// `dispatch_checked` until restart, and a bad peers file stops `sync_peers`;
/// none of them may stop an operator revoking a leaked key. The plane is
/// still tried (so instances can be torn down and the exchange swept); when
/// it cannot be built the revocation is applied to the kernel's list alone,
/// which every gate and exchange the daemon builds reads.
async fn revoke_dispatch(params: Value, kernel: &Arc<RwLock<Kernel<NativePlatform>>>) -> Response {
    let list = kernel.read().await.revocation_list().clone();
    let revoker = revoker_for(list);
    // One build attempt per BUILD_RETRY: a plane that cannot be built (a
    // broken policy file) is not rebuilt, with all that costs, on every call
    // of the incident verb.
    let recently_failed = BUILD_FAILED
        .lock()
        .ok()
        .and_then(|g| *g)
        .is_some_and(|t| t.elapsed() < BUILD_RETRY);
    if !recently_failed {
        match PLANE.get_or_try_init(|| build(kernel)).await {
            Ok(_) => {}
            Err(e) => {
                if let Ok(mut g) = BUILD_FAILED.lock() {
                    *g = Some(std::time::Instant::now());
                }
                tracing::warn!(error = %e, "placement unavailable: revoking against the revocation list alone");
            }
        }
    }
    match revoker.revoke(params).await {
        Ok(v) => Response::success(v),
        Err(e) => Response::error(e),
    }
}

/// [`dispatch`] with the governance the plane was built from given (so a
/// test can stand in for a later governance push).
pub(crate) async fn dispatch_checked(
    m: &str,
    params: Value,
    kernel: Arc<RwLock<Kernel<NativePlatform>>>,
    built: Option<&Option<String>>,
) -> Response {
    if m == "workload.revoke" {
        return revoke_dispatch(params, &kernel).await;
    }
    match ready_plane(&kernel, built).await {
        Ok(plane) => route(&plane, m, params).await,
        Err(e) => Response::error(e),
    }
}

/// The control plane with its guards applied (built, governance unchanged,
/// this node's host a target, operator peers synced). Shared by every verb
/// that talks to peers.
async fn ready_plane(
    kernel: &Arc<RwLock<Kernel<NativePlatform>>>,
    built: Option<&Option<String>>,
) -> Result<Arc<PlacementControlPlane>, String> {
    let plane = PLANE
        .get_or_try_init(|| build(kernel))
        .await
        .map_err(|e| format!("placement unavailable: {e}"))?
        .clone();
    let now = kernel.read().await.governance_overlay().map(|o| o.applied().effective_hash);
    governance_changed(built, now.as_deref())?;
    if HOST.get().is_some() && !plane.targets().iter().any(|t| t.addr == LOCAL_ADDR) {
        // Facts were not probed yet when the plane was built.
        let _ = plane.add_target(LOCAL_ADDR, TrustTier::Pinned).await;
    }
    if let Some(dir) = BOOT.get().map(|b| b.runtime_dir.clone())
        && let Err(e) = sync_peers(&plane, &dir).await
    {
        // Fail closed: a broken peers file places nowhere remote.
        let _ = plane.apply_operator_peers(&[], &[LOCAL_ADDR]).await;
        return Err(e);
    }
    Ok(plane)
}

/// This node's id as its peers address it on the mesh, once placement is
/// initialised (the key's own id, or the machine's in service mode).
pub fn local_mesh_node_id() -> Option<String> {
    BOOT.get().map(|b| b.mesh_node_id.clone())
}

/// The control plane, if it has been built (never builds it).
pub fn plane_if_built() -> Option<Arc<PlacementControlPlane>> {
    PLANE.get().cloned()
}

/// The public half of the key this node's `workload-host` and controller
/// sign with (the node key, or the control key in service mode): what a
/// peer pins in its `workload-peers.json` (ADR-108 P2b).
pub fn local_signing_pubkey() -> Option<[u8; 32]> {
    BOOT.get().map(|b| b.key.verifying_key().to_bytes())
}

/// Send node-admin method `m` (`dashboard.status`, `dashboard.token.rotate`)
/// to the peer `node` over the signed `workload.ctl` wire and return its
/// result. The peer must be in `workload-peers.json` (rotation needs tier
/// `pinned`) and must list this node's key as a controller in its
/// `workload-host.json`.
pub async fn node_admin(
    kernel: &Arc<RwLock<Kernel<NativePlatform>>>,
    node: &str,
    m: &str,
    body: Value,
) -> Result<Value, String> {
    let plane = ready_plane(kernel, BUILT_RULES.get()).await?;
    plane.node_admin(node, m, body).await.map_err(|e| match e {
        clawft_kernel::workload_ctl::PlaneError::Unknown(_) => format!(
            "{e}: add it to workload-peers.json (tier pinned) and make sure it serves workload-host \
             with this node's key as a controller"
        ),
        other => other.to_string(),
    })
}

/// Daemon boot: when `workload-host.json` is present, build the control
/// plane (and with it this node's `workload-host`) and serve the host to
/// the configured controllers. Returns where it is served.
pub async fn start_serving(
    kernel: Arc<RwLock<Kernel<NativePlatform>>>,
) -> Result<Option<SocketAddr>, String> {
    let dir = BOOT
        .get()
        .map(|b| b.runtime_dir.clone())
        .ok_or("placement not initialised (no daemon key)")?;
    let Some(cfg) = load_host_config(&dir)? else {
        return Ok(None);
    };
    PLANE.get_or_try_init(|| build(&kernel)).await?;
    let host = HOST.get().cloned().ok_or("workload-host was not built")?;
    let adv = host.advertisement();
    let bound = serve(&cfg, host).await?;
    let _ = SERVED.set(bound);
    tracing::info!(addr = %bound, advertise = %cfg.advertised(), node = %adv.node_id,
        methods = adv.methods.len(), "workload-host served");
    Ok(Some(bound))
}

#[cfg(test)]
mod governance_changed_tests {
    use super::governance_changed;

    #[test]
    fn a_push_after_build_makes_placement_refuse() {
        assert!(governance_changed(None, Some("a")).is_ok(), "not built yet");
        let plain: Option<String> = None;
        assert!(governance_changed(Some(&plain), None).is_ok(), "no overlay, none now");
        let built = Some("aa".to_owned());
        assert!(governance_changed(Some(&built), Some("aa")).is_ok());
        let e = governance_changed(Some(&built), Some("bb")).unwrap_err();
        assert!(e.contains("restart"), "{e}");
        assert!(governance_changed(Some(&built), None).is_err());
    }
}

#[cfg(test)]
#[path = "workload_place_rpc_tests.rs"]
pub(crate) mod tests;

#[cfg(test)]
#[path = "workload_place_rpc_daemon_tests.rs"]
mod daemon_tests;

#[cfg(test)]
mod revocation_wiring_tests {
    use super::*;
    use clawft_kernel::artifact_store::ArtifactStore;
    use clawft_kernel::ipc::{KernelMessage, MessagePayload, MessageTarget};
    use clawft_kernel::mesh_ipc::MeshIpcEnvelope;
    use clawft_kernel::mesh_runtime::{MeshRuntime, REVOKE_TOPIC};
    use clawft_kernel::mesh_swarm_revoke::sign_revocation;
    use clawft_kernel::revocation::{RevocationKind, RevocationList};
    use clawft_kernel::workload_pkg::{KeyOrigin, TrustAnchors};
    use ed25519_dalek::SigningKey;

    #[tokio::test]
    async fn the_daemon_exchange_gets_the_revocation_list_and_takes_mesh_notices() {
        let tmp = tempfile::tempdir().unwrap();
        let list = Arc::new(RevocationList::new(tmp.path().join("revoked.json")));
        let ex = Arc::new(
            ArtifactExchange::new("n", Arc::new(ArtifactStore::new_memory()), ExchangeConfig::default())
                .unwrap(),
        );
        let key = SigningKey::from_bytes(&[4; 32]);
        let pk = key.verifying_key().to_bytes();
        let mut anchors = TrustAnchors::default();
        anchors
            .push_signer("op", &clawft_kernel::workload_pkg::codec::hex_encode(&pk), KeyOrigin::Operator)
            .unwrap();
        let rt = Arc::new(MeshRuntime::new("n".into()));
        wire_revocations(&ex, list.clone(), &anchors, Some(rt.clone()), tmp.path());
        // The exchange already has a list: a second one is refused.
        assert!(!ex.set_revocations(list.clone()));

        // A signed notice arriving over the mesh lands in the node's list.
        let hash = clawft_kernel::workload_pkg::codec::hex_encode(&[9u8; 32]);
        let notice = sign_revocation(RevocationKind::ArtifactHash, &hash, "test", 1, &key).unwrap();
        let msg = KernelMessage::new(
            0,
            MessageTarget::Topic(REVOKE_TOPIC.into()),
            MessagePayload::Json(serde_json::to_value(&notice).unwrap()),
        );
        let bytes = MeshIpcEnvelope::new("peer".into(), "n".into(), msg).to_bytes().unwrap();
        let (tx, _rx) = tokio::sync::mpsc::channel(4);
        rt.handle_incoming_peer(&bytes, tx, None).await.unwrap();
        assert!(list.is_subject_revoked(RevocationKind::ArtifactHash, &hash));
    }

    #[tokio::test]
    async fn the_daemon_licence_exchange_is_inert_until_the_mesh_id_is_set_then_takes_bindings() {
        use clawft_kernel::licence::{
            AdmissionPosture, BindState, BindingRecord, LocalMeshId, MeshCheckoutPolicy, MeshId,
            sign_binding,
        };
        use clawft_kernel::mesh_runtime::COG_BINDING_TOPIC;
        let tmp = tempfile::tempdir().unwrap();
        let list = Arc::new(RevocationList::new(tmp.path().join("revoked.json")));
        let key = SigningKey::from_bytes(&[4; 32]);
        let hex = clawft_kernel::workload_pkg::codec::hex_encode;
        let mut anchors = TrustAnchors::default();
        anchors.push_signer("op", &hex(&key.verifying_key().to_bytes()), KeyOrigin::Operator).unwrap();
        let local = LocalMeshId::unset();
        let policy = MeshCheckoutPolicy::open(tmp.path(), anchors.clone(), list, local.clone());
        let chain = Arc::new(ChainManager::new(0, 1000));
        let rt = Arc::new(MeshRuntime::new("n".into()));
        let enforce = AdmissionPosture { enforce: true, verdict_source_bound: true, open_membership: false };
        wire_licence(tmp.path(), &policy, &anchors, &chain, Some(rt.clone()), Arc::new(move || enforce));

        let mesh = MeshId::derive(&[1; 32], &[2; 32]);
        let rec = BindingRecord {
            v: 2,
            device_id: "seed-x".into(),
            device_pubkey: hex(&[5; 32]),
            mesh_id: mesh.to_hex(),
            grant_pubkey: hex(&[6; 32]),
            steward_node_id: "steward".into(),
            steward_pubkey: hex(&[7; 32]),
            state: BindState::Bound,
            seq: 1,
            bound_at: 1,
        };
        let signed = sign_binding(&rec, &key).unwrap();
        let send = |rt: Arc<MeshRuntime>, v: serde_json::Value| async move {
            let msg = KernelMessage::new(0, MessageTarget::Topic(COG_BINDING_TOPIC.into()), MessagePayload::Json(v));
            let bytes = MeshIpcEnvelope::new("peer".into(), "n".into(), msg).to_bytes().unwrap();
            let (tx, _rx) = tokio::sync::mpsc::channel(4);
            rt.handle_incoming_peer(&bytes, tx, None).await.unwrap();
        };
        send(rt.clone(), serde_json::to_value(&signed).unwrap()).await;
        assert!(policy.store().held_signed_binding().is_none(), "inert: no local mesh id yet");
        local.set(Some(mesh));
        send(rt, serde_json::to_value(&signed).unwrap()).await;
        assert_eq!(policy.store().held_signed_binding().map(|b| b.0), Some(1));
    }
}
