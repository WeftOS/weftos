//! Kernel boot sequence and state machine.
//!
//! The [`Kernel`] struct is the central coordinator. It wraps
//! [`AppContext`] and manages the process table, service registry,
//! IPC, and health subsystem through a boot/shutdown lifecycle.

use std::sync::Arc;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};

use clawft_core::bootstrap::AppContext;
use clawft_core::bus::MessageBus;
use clawft_platform::Platform;
use clawft_types::config::Config;

#[cfg(feature = "native")]
use crate::a2a::A2ARouter;
use crate::app::AppManager;
use crate::capability::AgentCapabilities;
#[cfg(feature = "native")]
use crate::capability::CapabilityChecker;
use crate::cluster::{ClusterConfig, ClusterMembership};
use crate::console::{BootEvent, BootLog, BootPhase, KernelEventLog};
use crate::error::{KernelError, KernelResult};
use crate::health::HealthSystem;
use crate::ipc::KernelIpc;
use crate::process::{ProcessEntry, ProcessState, ProcessTable, ResourceUsage};
use crate::service::ServiceRegistry;
use crate::supervisor::AgentSupervisor;
#[cfg(feature = "native")]
use crate::topic::TopicRouter;
use clawft_types::config::KernelConfig;

/// Boot-time warning emitted when `vector.backend` needs the `diskann`
/// cargo feature but the kernel was built without it (WEFT-656). See
/// `vector_diskann` module docs for the full mismatch story.
#[cfg(feature = "ecc")]
const DISKANN_FEATURE_MISMATCH_WARNING: &str = "vector.backend requires the \
    `diskann` cargo feature (DiskAnn or Hybrid) but this kernel binary was \
    built WITHOUT it — DiskAnnBackend is silently degrading to a \
    brute-force O(n) linear-scan stub instead of real Vamana-graph ANN \
    search. Rebuild with `--features diskann` for the real backend, or set \
    `vector.strict = true` in kernel config to turn this into a boot error \
    instead of a silent degrade.";

/// Boot-time error emitted for the same mismatch when `vector.strict =
/// true`: refuse to boot into a silently degraded vector backend.
#[cfg(feature = "ecc")]
const DISKANN_FEATURE_MISMATCH_ERROR: &str = "vector.backend requires the \
    `diskann` cargo feature (DiskAnn or Hybrid) but this kernel binary was \
    built WITHOUT it, and `vector.strict = true` refuses to silently fall \
    back to the brute-force O(n) stub. Rebuild with `--features diskann`, \
    or set `vector.strict = false` to accept the degraded stub.";

/// Kernel lifecycle state.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum KernelState {
    /// Kernel is in the process of booting.
    Booting,
    /// Kernel is running and accepting commands.
    Running,
    /// Kernel is shutting down.
    ShuttingDown,
    /// Kernel has been halted.
    Halted,
}

impl std::fmt::Display for KernelState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KernelState::Booting => write!(f, "booting"),
            KernelState::Running => write!(f, "running"),
            KernelState::ShuttingDown => write!(f, "shutting_down"),
            KernelState::Halted => write!(f, "halted"),
        }
    }
}

// ── Feature-gated subsystem bundles ──────────────────────────────
//
// These group related feature-gated fields so the Kernel struct
// has fewer conditional fields (3 instead of 18) and init code
// is easier to read.

/// Exochain subsystem: chain manager, resource tree, governance gate, and
/// optional external chain-head anchor (WEFT-137 / ADR-041).
#[cfg(feature = "exochain")]
pub struct ChainSubsystem {
    pub(crate) chain_manager: Option<Arc<crate::chain::ChainManager>>,
    pub(crate) tree_manager: Option<Arc<crate::tree_manager::TreeManager>>,
    pub(crate) governance_gate: Option<Arc<dyn crate::gate::GateBackend>>,
    /// External chain-head anchoring controller (file ledger / external stub).
    pub(crate) chain_anchor: Option<Arc<crate::chain_anchor::AnchoringController>>,
    /// Project governance (profile `project` only): overlay reload and
    /// parent-policy updates (ADR-103 D8).
    pub(crate) overlay: Option<Arc<crate::overlay_runtime::OverlayRuntime>>,
    /// Exclusive lock on the chain in use; released when the kernel drops.
    pub(crate) _chain_lock: Option<crate::chain_storage::ChainLock>,
}

/// ECC cognitive substrate: HNSW, causal graph, cognitive tick,
/// cross-references, impulse queue, and boot-time calibration.
#[cfg(feature = "ecc")]
pub struct EccSubsystem {
    pub(crate) hnsw: Option<Arc<crate::hnsw_service::HnswService>>,
    pub(crate) causal: Option<Arc<crate::causal::CausalGraph>>,
    pub(crate) tick: Option<Arc<crate::cognitive_tick::CognitiveTick>>,
    pub(crate) crossrefs: Option<Arc<crate::crossref::CrossRefStore>>,
    pub(crate) impulses: Option<Arc<crate::impulse::ImpulseQueue>>,
    pub(crate) calibration: Option<crate::calibration::EccCalibration>,
    pub(crate) vector_backend: Option<Arc<dyn crate::vector_backend::VectorBackend>>,
    pub(crate) eml_coherence:
        Option<Arc<std::sync::Mutex<crate::eml_coherence::EmlCoherenceModel>>>,
    /// Spatial BVH service (ADR-056 / WEFT-718). Present when
    /// `[kernel.spatial].enabled = true`.
    pub(crate) spatial: Option<Arc<crate::spatial_service::SpatialService>>,
}

/// OS-patterns observability: metrics, structured logging, timers,
/// and the dead-letter queue.
#[cfg(feature = "os-patterns")]
pub struct ObservabilitySubsystem {
    pub(crate) metrics_registry: Option<Arc<crate::metrics::MetricsRegistry>>,
    pub(crate) log_service: Option<Arc<crate::log_service::LogService>>,
    pub(crate) timer_service: Option<Arc<crate::timer::TimerService>>,
    pub(crate) dead_letter_queue: Option<Arc<crate::dead_letter::DeadLetterQueue>>,
}

/// Where a booting kernel gets its node identity from.
enum NodeBinding {
    /// A persisted key seed (`None`: ephemeral, tests and one-shot boots).
    Key(Option<[u8; 32]>),
    /// The machine mesh service's node id (P3-U); no key is held here.
    Service(String),
}

/// The WeftOS kernel.
///
/// Wraps `AppContext<P>` in a managed boot sequence with process
/// tracking, service lifecycle, IPC, and health monitoring.
///
/// # Lifecycle
///
/// 1. Call [`Kernel::boot`] to initialize all subsystems.
/// 2. The kernel transitions from `Booting` -> `Running`.
/// 3. Call [`Kernel::shutdown`] to gracefully stop everything.
/// 4. The kernel transitions from `Running` -> `ShuttingDown` -> `Halted`.
pub struct Kernel<P: Platform> {
    state: KernelState,
    config: KernelConfig,
    app_context: Option<AppContext<P>>,
    bus: Arc<MessageBus>,
    process_table: Arc<ProcessTable>,
    service_registry: Arc<ServiceRegistry>,
    ipc: Arc<KernelIpc>,
    #[cfg(feature = "native")]
    a2a_router: Arc<A2ARouter>,
    #[cfg(feature = "native")]
    cron_service: Arc<crate::cron::CronService>,
    #[cfg(feature = "native")]
    assessment_service: Arc<crate::assessment::AssessmentService>,
    health: HealthSystem,
    supervisor: AgentSupervisor<P>,
    boot_log: BootLog,
    event_log: Arc<KernelEventLog>,
    boot_time: Instant,
    cluster_membership: Arc<ClusterMembership>,
    /// Installed application catalog; rehydrated from disk at boot.
    app_manager: Arc<AppManager>,
    revocation_list: Arc<crate::revocation::RevocationList>,
    #[cfg(feature = "native")]
    agent_registry: crate::agent_registry::AgentRegistry,
    #[cfg(feature = "native")]
    node_registry: crate::node_registry::NodeRegistry,
    #[cfg(feature = "native")]
    substrate_service: crate::substrate_service::SubstrateService,
    #[cfg(feature = "exochain")]
    chain: ChainSubsystem,
    #[cfg(feature = "ecc")]
    ecc: EccSubsystem,
    #[cfg(feature = "os-patterns")]
    observability: ObservabilitySubsystem,
}

impl<P: Platform> Kernel<P> {
    /// Boot the kernel from configuration and platform.
    ///
    /// This is the primary entry point. It:
    /// 1. Creates subsystems (process table, service registry, IPC, health)
    /// 2. Creates AppContext (reuses existing bootstrap)
    /// 3. Registers the kernel process (PID 0)
    /// 4. Starts all registered services
    /// 5. Transitions to Running state
    ///
    /// # Errors
    ///
    /// Returns [`KernelError::Boot`] if any critical subsystem fails
    /// to initialize.
    pub async fn boot(
        config: Config,
        kernel_config: KernelConfig,
        platform: Arc<P>,
    ) -> KernelResult<Self> {
        Self::boot_with_node_key(config, kernel_config, platform, None).await
    }

    /// Boot the kernel with a persisted node identity.
    ///
    /// `node_key_seed` is the 32-byte Ed25519 seed of this node's key
    /// (the daemon's `<runtime>/node.key`). The mesh, cluster and status
    /// node id is derived from it with
    /// [`crate::node_id::node_id_from_pubkey`], so the id is stable across
    /// restarts. With `None` (tests, one-shot CLI boots) an ephemeral
    /// key is generated and the id lasts for this process only.
    ///
    /// # Errors
    ///
    /// Same as [`Kernel::boot`]; additionally fails when mesh is enabled
    /// and its listener cannot bind (ADR-103 D2).
    pub async fn boot_with_node_key(
        config: Config,
        kernel_config: KernelConfig,
        platform: Arc<P>,
        node_key_seed: Option<[u8; 32]>,
    ) -> KernelResult<Self> {
        Self::boot_bound(config, kernel_config, platform, NodeBinding::Key(node_key_seed)).await
    }

    /// Boot as a client of the machine mesh service (ADR-103 P3-U).
    ///
    /// The node id is the service's (`hello_ack.node_id`); this kernel holds
    /// no node key, binds no mesh listener and starts no mesh runtime. There
    /// is no ephemeral-identity fallback: a malformed id fails the boot.
    ///
    /// # Errors
    ///
    /// [`KernelError::Boot`] when `service_node_id` is not a node id, plus
    /// everything [`Kernel::boot`] can return.
    pub async fn boot_in_service_mode(
        config: Config,
        kernel_config: KernelConfig,
        platform: Arc<P>,
        service_node_id: String,
    ) -> KernelResult<Self> {
        if !crate::node_id::is_node_id(&service_node_id) {
            return Err(KernelError::Boot(format!(
                "service mode needs the mesh service's node id, got {service_node_id:?}"
            )));
        }
        Self::boot_bound(
            config,
            kernel_config,
            platform,
            NodeBinding::Service(service_node_id),
        )
        .await
    }

    async fn boot_bound(
        config: Config,
        mut kernel_config: KernelConfig,
        platform: Arc<P>,
        binding: NodeBinding,
    ) -> KernelResult<Self> {
        let (node_key_seed, service_node_id) = match binding {
            NodeBinding::Key(seed) => (seed, None),
            NodeBinding::Service(id) => (None, Some(id)),
        };
        // `required` means a machine mesh service must carry the mesh. A boot
        // path that did not link one (one-shot CLI boot, foreground boot,
        // tests) cannot honour it and must not quietly run its own listener.
        #[cfg(feature = "mesh")]
        if service_node_id.is_none()
            && let Some(m) = kernel_config.mesh.as_ref()
            && m.enabled
            && m.service == clawft_types::config::MeshServicePolicy::Required
        {
            return Err(KernelError::Boot(
                "kernel.mesh.service = \"required\" but this boot did not link a machine mesh \
                 service (start the user daemon with the service running, or set \
                 kernel.mesh.service = \"auto\")"
                    .into(),
            ));
        }
        let boot_time = Instant::now();
        let mut boot_log = BootLog::new();
        // Resolve the chain location once (explicit config, else
        // runtime root: $WEFTOS_RUNTIME_DIR, project, or ~/.clawft) so restore, verify and
        // shutdown persistence all use the same, possibly isolated, files.
        let pin = crate::chain_storage::pin_chain_storage_noted(&mut kernel_config);
        // An older, lock-unaware kernel may still be writing the legacy
        // chain: refuse before taking any lock or touching the chain.
        if let Some(msg) = pin.refusal {
            // A refusal that clears by itself is a plain boot error (exit 1,
            // a service manager retries); the rest are permanent (exit 78).
            return Err(if pin.refusal_transient {
                KernelError::Boot(msg)
            } else {
                KernelError::BootRefused(msg)
            });
        }
        let (pinned_chain, chain_note) = (pin.path, pin.warning);
        // One kernel per chain: hold chain.lock beside the chain in use for
        // the kernel's lifetime (released when the kernel is dropped).
        #[cfg(feature = "exochain")]
        let chain_lock = match pinned_chain.as_deref() {
            Some(ckpt) => Some(
                crate::chain_storage::ChainLock::try_acquire(ckpt).map_err(|e| match e {
                    crate::chain_storage::ChainLockError::InUse(m) => KernelError::BootRefused(m),
                    crate::chain_storage::ChainLockError::Other(m) => KernelError::Boot(m),
                })?,
            ),
            None => None,
        };

        info!("WeftOS kernel booting");
        boot_log.push(BootEvent::info(
            BootPhase::Init,
            format!("WeftOS v{} booting...", env!("CARGO_PKG_VERSION")),
        ));
        if let Some(ref p) = pinned_chain {
            boot_log.push(BootEvent::info(
                BootPhase::Init,
                format!("Chain storage: {}", p.display()),
            ));
        }
        // One resolver for every runtime file (cluster peers, apps, revoked
        // hosts); the chain, socket and node key use the same root.
        let runtime_paths = crate::chain_storage::boot_runtime_paths(pinned_chain.as_deref());
        boot_log.push(BootEvent::info(
            BootPhase::Init,
            format!("Runtime dir: {}", runtime_paths.root().display()),
        ));
        // ADR-103 D8: a `project` kernel's rules are the user's signed parent
        // policy plus the project overlay; any failure refuses the boot.
        let project_profile = kernel_config.profile.is_some();
        #[cfg(feature = "exochain")]
        let mut overlay_prepared = if project_profile {
            let p = crate::overlay_runtime::prepare(&runtime_paths)
                .map_err(|e| KernelError::BootRefused(e.boot_message()))?;
            p.apply_limits(&mut kernel_config);
            Some(p)
        } else {
            let p = crate::overlay_runtime::prepare_nested()
                .map_err(|e| KernelError::BootRefused(e.boot_message()))?;
            if let Some(p) = &p { p.apply_limits(&mut kernel_config); }
            p
        };
        #[cfg(not(feature = "exochain"))]
        if project_profile {
            return Err(KernelError::BootRefused(
                "profile `project` needs the exochain feature (governance overlay)".into(),
            ));
        }
        #[cfg(feature = "exochain")]
        let mut overlay_runtime: Option<Arc<crate::overlay_runtime::OverlayRuntime>> = None;
        if let Some(w) = chain_note {
            warn!("{w}");
            boot_log.push(BootEvent::warn(BootPhase::Init, w));
        }
        boot_log.push(BootEvent::info(BootPhase::Init, "PID 0 (kernel)"));
        // WEFT-70: surface macOS / non-Linux OS-sandbox downgrade in boot
        // log (banner also prints this via console::boot_banner).
        if let Some(msg) = clawft_plugin::sandbox::sandbox_platform_banner_warning() {
            warn!(%msg, "sandbox platform restriction");
            boot_log.push(BootEvent::warn(BootPhase::Init, msg));
        }

        // 1. Create subsystems
        let process_table = Arc::new(ProcessTable::new(kernel_config.max_processes));
        let service_registry = Arc::new(ServiceRegistry::new());

        boot_log.push(BootEvent::info(
            BootPhase::Config,
            format!("Max processes: {}", kernel_config.max_processes),
        ));
        boot_log.push(BootEvent::info(
            BootPhase::Config,
            format!(
                "Health check interval: {}s",
                kernel_config.health_check_interval_secs
            ),
        ));

        // 2. Create AppContext
        let app_context = AppContext::new(config, platform)
            .await
            .map_err(|e| KernelError::Boot(format!("AppContext init failed: {e}")))?;

        // 3. Create IPC from the AppContext's MessageBus
        let bus = app_context.bus().clone();
        let ipc = Arc::new(KernelIpc::new(bus.clone()));

        // 4. Create health system
        let health = HealthSystem::new(kernel_config.health_check_interval_secs);

        // 5. Register kernel process (PID 0)
        let kernel_entry = ProcessEntry {
            pid: 0,
            agent_id: "kernel".to_owned(),
            state: ProcessState::Running,
            capabilities: AgentCapabilities::default(),
            resource_usage: ResourceUsage::default(),
            cancel_token: crate::process::CancellationToken::new(),
            parent_pid: None,
        };
        process_table
            .insert_with_pid(kernel_entry)
            .map_err(|e| KernelError::Boot(format!("failed to register kernel process: {e}")))?;

        boot_log.push(BootEvent::info(
            BootPhase::Services,
            "Service registry ready",
        ));

        // 5a. Construct A2ARouter (per-PID inboxes, capability-checked routing)
        #[cfg(feature = "native")]
        let capability_checker = Arc::new(CapabilityChecker::new(process_table.clone()));
        #[cfg(feature = "native")]
        let topic_router = Arc::new(TopicRouter::new(process_table.clone()));
        #[cfg(feature = "native")]
        let a2a_router = Arc::new(A2ARouter::new(
            process_table.clone(),
            capability_checker,
            topic_router,
        ));

        #[cfg(feature = "native")]
        boot_log.push(BootEvent::info(BootPhase::Services, "A2A router ready"));

        // 5b. Register cron service (K0 gate requirement)
        #[cfg(feature = "native")]
        let cron_svc = Arc::new(crate::cron::CronService::new());
        #[cfg(feature = "native")]
        if let Err(e) = service_registry.register(cron_svc.clone()) {
            error!(error = %e, "failed to register cron service");
        } else {
            boot_log.push(BootEvent::info(
                BootPhase::Services,
                "Cron service registered",
            ));
        }

        // Node identity (ADR-025 / ADR-103 D11): the node id is derived from
        // the Ed25519 node key, never allocated per boot. Mesh handshake,
        // cluster membership, heartbeats and status all use this one id.
        let node_id: String = if let Some(id) = service_node_id.clone() {
            // Service mode: the id belongs to the machine mesh service; this
            // kernel never derives or generates one (no silent fallback).
            id
        } else {
            #[cfg(any(feature = "mesh", feature = "exochain"))]
            {
                let identity = match node_key_seed {
                    Some(seed) => crate::cluster::NodeIdentity::from_signing_key(
                        ed25519_dalek::SigningKey::from_bytes(&seed),
                    ),
                    None => {
                        warn!("no persisted node key supplied; using an ephemeral node identity");
                        crate::cluster::NodeIdentity::generate()
                    }
                };
                identity.node_id().to_owned()
            }
            #[cfg(not(any(feature = "mesh", feature = "exochain")))]
            {
                // No ed25519 in this build: hash the seed (or fresh random
                // bytes) with the same SHA-256 derivation so the id shape
                // is identical.
                let seed = node_key_seed.unwrap_or_else(|| {
                    let mut b = [0u8; 32];
                    b[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
                    b[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
                    b
                });
                crate::node_id::node_id_from_pubkey(&seed)
            }
        };

        // 5b½ / 5d prep — mesh-aware assessment service (WEFT-117).
        //
        // When mesh is enabled we share the key-derived node_id up front and
        // share one MeshCoordinator between AssessmentService and
        // AssessmentTransport. Assessment is registered after that
        // decision so `assess.mesh.status` sees a live coordinator.
        #[cfg(all(feature = "native", feature = "mesh"))]
        let mesh_config_early = kernel_config.mesh.clone().unwrap_or_default();
        #[cfg(all(feature = "native", feature = "mesh"))]
        let mesh_node_id: Option<String> = if mesh_config_early.enabled {
            Some(node_id.clone())
        } else {
            None
        };
        #[cfg(all(feature = "native", feature = "mesh"))]
        let mesh_project_name = kernel_config
            .cluster
            .as_ref()
            .and_then(|c| c.node_name.clone())
            .unwrap_or_else(|| "local".into());
        #[cfg(all(feature = "native", feature = "mesh"))]
        let assess_mesh_coord: Option<Arc<crate::assessment::mesh::MeshCoordinator>> =
            mesh_node_id.as_ref().map(|id| {
                Arc::new(crate::assessment::mesh::MeshCoordinator::new(
                    id.clone(),
                    mesh_project_name.clone(),
                ))
            });

        // 5b½. Register assessment service
        #[cfg(feature = "native")]
        let assessment_svc = {
            #[cfg(feature = "mesh")]
            {
                if let Some(ref coord) = assess_mesh_coord {
                    Arc::new(crate::assessment::AssessmentService::with_mesh_coordinator(
                        Arc::clone(coord),
                    ))
                } else {
                    Arc::new(crate::assessment::AssessmentService::new())
                }
            }
            #[cfg(not(feature = "mesh"))]
            {
                Arc::new(crate::assessment::AssessmentService::new())
            }
        };
        #[cfg(feature = "native")]
        if let Err(e) = service_registry.register(assessment_svc.clone()) {
            error!(error = %e, "failed to register assessment service");
        } else {
            let mesh_note = {
                #[cfg(feature = "mesh")]
                {
                    if assess_mesh_coord.is_some() {
                        " (mesh coordination enabled)"
                    } else {
                        ""
                    }
                }
                #[cfg(not(feature = "mesh"))]
                {
                    ""
                }
            };
            boot_log.push(BootEvent::info(
                BootPhase::Services,
                format!("Assessment service registered{mesh_note}"),
            ));
        }

        // 5c. Register container service (K4)
        let container_manager = std::sync::Arc::new(crate::container::ContainerManager::new(
            crate::container::ContainerConfig::default(),
        ));
        let container_service = std::sync::Arc::new(crate::container::ContainerService::new(
            container_manager.clone(),
        ));
        if let Err(e) = service_registry.register(container_service) {
            error!(error = %e, "failed to register container service");
        } else {
            boot_log.push(BootEvent::info(
                BootPhase::Services,
                "Container service registered",
            ));
        }

        // Host revocation list: loaded here (not at step 6b) so the mesh
        // listener's admission gate and the kernel share one instance.
        let revocation_list = Arc::new(crate::revocation::RevocationList::load(
            runtime_paths.revoked_hosts(),
        ));

        // Collapsed-mode verdict source for mesh admission; bound to the
        // governance gate once that exists (step 9, below the mesh block).
        #[cfg(all(feature = "native", feature = "mesh", feature = "exochain"))]
        let mesh_verdicts = Arc::new(crate::mesh_admit::GateVerdictSource::late());
        #[cfg(all(feature = "native", feature = "mesh", feature = "exochain"))]
        let mesh_closed_without_gate = mesh_config_early.admission
            == clawft_types::config::MeshAdmissionMode::Enforce
            && !mesh_config_early.admission_open_membership;

        // 5d. Initialize mesh transport (K6) if configured.
        //     Must happen before cluster (step 7) because cluster needs mesh
        //     to reach peer nodes.
        #[cfg(all(feature = "native", feature = "mesh"))]
        let mesh_runtime = {
            let mesh_config = mesh_config_early;
            if service_node_id.is_none()
                && crate::mesh_mode::select(&mesh_config) == crate::mesh_mode::MeshMode::Collapsed
            {
                let node_id = mesh_node_id
                    .clone()
                    .expect("mesh enabled implies mesh_node_id");
                let mut runtime = if mesh_config.discovery {
                    let kad_id = blake3::hash(node_id.as_bytes());
                    crate::mesh_runtime::MeshRuntime::with_discovery(
                        node_id.clone(),
                        *kad_id.as_bytes(),
                    )
                } else {
                    crate::mesh_runtime::MeshRuntime::new(node_id.clone())
                };

                // Wire mesh into the A2A router for incoming message injection.
                runtime.set_local_router(Arc::clone(&a2a_router));

                let runtime = Arc::new(runtime);
                runtime.set_leaf_discovery_any(mesh_config.leaf_discovery_any);
                // A bad or wildcard-without-opt-in leaf bind aborts boot.
                if let Some(addrs) = mesh_config.leaf_listen_addr.as_ref() {
                    let specs = crate::mesh_leaf_bind::LeafBindSpec::parse_all(&addrs.entries(), mesh_config.leaf_listen_any)
                        .map_err(|e| KernelError::Boot(format!("kernel.mesh: {e}")))?;
                    runtime.set_leaf_bind_specs(specs);
                }
                // ADR-103 leaf admission is opt-in until a service-owned
                // registry exists. An ephemeral node key cannot sign durable
                // acknowledgments, so only a persisted node key may serve it.
                if let Some(seed) = node_key_seed {
                    let leaf_root = runtime_paths.root().join("leaf");
                    if leaf_root.join("registry").is_dir() {
                        let ingress = crate::mesh_leaf::LeafIngress::open(
                            &leaf_root,
                            ed25519_dalek::SigningKey::from_bytes(&seed),
                        ).map_err(|e| KernelError::Boot(format!("leaf ingress: {e}")))?;
                        runtime.set_leaf_ingress(Arc::new(ingress)).map_err(|_|
                            KernelError::Boot("leaf ingress already installed".into()))?;
                    }
                }

                // WEFT-117: register AssessmentTransport so the mesh
                // accept/event loop demuxes FrameType::AssessmentSync
                // and can drain FindingDiff / gossip pushes to peers.
                if let Some(ref coord) = assess_mesh_coord {
                    let assess_transport =
                        Arc::new(crate::mesh_assess::AssessmentTransport::new(Arc::clone(
                            coord,
                        )));
                    runtime.set_assessment_transport(Arc::clone(&assess_transport));

                    // Periodic drain of pending assessment broadcasts
                    // (diff-only when available) + lightweight gossip.
                    let rt_tick = Arc::clone(&runtime);
                    let assess_for_tick = Arc::clone(&assessment_svc);
                    let transport_for_tick = Arc::clone(&assess_transport);
                    tokio::spawn(async move {
                        let mut interval =
                            tokio::time::interval(std::time::Duration::from_secs(15));
                        loop {
                            interval.tick().await;
                            // Keep transport's published report in sync so
                            // RequestReport and gossip_tick have data.
                            if let Some(report) = assess_for_tick.get_latest() {
                                transport_for_tick.publish_report(report);
                            }
                            // Prefer pending (FindingDiff / gossip from run).
                            let pushed = rt_tick.push_pending_assessment().await;
                            if pushed == 0 {
                                let _ = rt_tick.assessment_gossip_tick().await;
                            }
                        }
                    });

                    boot_log.push(BootEvent::info(
                        BootPhase::Network,
                        "AssessmentTransport wired into mesh event loop",
                    ));
                }

                // Wire the A2A router BACK to the mesh runtime — the
                // counterpart of `set_local_router` above. Without
                // this, the A2A router's `Topic` handler has no mesh
                // runtime to consult, so locally-published topics
                // (e.g. via `ipc.publish` / `weaver leaf push`) are
                // never forwarded to mesh-subscribed peers like leaf
                // devices. `set_local_router` is mesh→A2A (inbound);
                // this is A2A→mesh (outbound). Both directions are
                // required for the leaf-push topic forwarding path
                // (`peers_for_topic` → `send_to_peer`) to work.
                a2a_router.set_mesh_runtime(Arc::clone(&runtime));
                runtime.set_enforcing(
                    mesh_config.admission == clawft_types::config::MeshAdmissionMode::Enforce,
                );

                // Clone values needed by seed-peer loop before moving mesh_config.
                let seed_peers = mesh_config.seed_peers.clone();
                let transport_name_for_seeds = mesh_config.transport.clone();
                let transport_display = mesh_config.transport.clone();
                let listen_display = mesh_config.listen_addr.clone();
                let noise_enabled = mesh_config.noise;

                // Generate or load Noise keypair.
                let noise_config = if noise_enabled {
                    let private_key: [u8; 32] = if let Some(ref key_path) =
                        mesh_config.noise_key_path
                    {
                        let key_bytes = std::fs::read(key_path).unwrap_or_else(|_| {
                            tracing::warn!("noise key file not found, generating ephemeral key");
                            let mut key = [0u8; 32];
                            use rand::RngCore;
                            rand::thread_rng().fill_bytes(&mut key);
                            key.to_vec()
                        });
                        let mut arr = [0u8; 32];
                        arr.copy_from_slice(&key_bytes[..32.min(key_bytes.len())]);
                        arr
                    } else {
                        let mut key = [0u8; 32];
                        use rand::RngCore;
                        rand::thread_rng().fill_bytes(&mut key);
                        key
                    };
                    Some(Arc::new(crate::mesh_noise::NoiseConfig {
                        pattern: crate::mesh_noise::NoisePattern::XX,
                        local_private_key: private_key,
                        remote_static_key: None,
                    }))
                } else {
                    None
                };
                let noise_for_seeds = noise_config.clone();

                // Bind the configured transport; the accept loop and seed
                // dialing live in `mesh_serve` (P3-K0).
                let listen_addr = mesh_config.listen_addr.clone();
                let transport = crate::mesh_serve::transport_for(&mesh_config.transport, None);

                // Bind synchronously so a taken port fails boot loudly instead of
                // leaving a kernel that silently has no mesh (ADR-103 D1/D2).
                // Disabled mesh never reaches this block, so mesh-off cannot fail here.
                let listener = transport.listen(&listen_addr).await.map_err(|e| {
                    KernelError::Boot(format!(
                        "mesh enabled but the listener could not bind {listen_addr} ({e}); \
                         another kernel on this machine is probably already using that port. \
                         Stop it, set kernel.mesh.listen_addr to a free address, or set \
                         kernel.mesh.enabled = false"
                    ))
                })?;

                // Admission (P3-K1): AllowAll unless a genesis hash is pinned
                // (observe) or enforcement is requested (needs the pin).
                let pinned = mesh_config
                    .genesis_hash
                    .as_deref()
                    .map(|h| {
                        crate::mesh_admit::hex_decode(h)
                            .and_then(|b| <[u8; 32]>::try_from(b).ok())
                            .ok_or_else(|| {
                                KernelError::Boot(
                                    "kernel.mesh.genesis_hash must be 64 hex characters".into(),
                                )
                            })
                    })
                    .transpose()?;
                let dial_identity: Option<Arc<crate::mesh_admit::DialIdentity>> =
                    match (mesh_config.admission, pinned, node_key_seed) {
                        (clawft_types::config::MeshAdmissionMode::Off, _, _) => None,
                        (_, Some(genesis), Some(seed)) => {
                            Some(Arc::new(crate::mesh_admit::DialIdentity {
                                key: ed25519_dalek::SigningKey::from_bytes(&seed),
                                genesis,
                                platform: std::env::consts::OS.to_owned(),
                                capabilities: vec![],
                            }))
                        }
                        _ => None,
                    };
                let admission_gate: Arc<dyn crate::mesh_admit::AdmissionGate> = {
                    use clawft_types::config::MeshAdmissionMode as Adm;
                    match (mesh_config.admission, pinned) {
                        (Adm::Off, _) | (Adm::Observe, None) => {
                            Arc::new(crate::mesh_admit::AllowAll)
                        }
                        (Adm::Enforce, None) => {
                            return Err(KernelError::Boot(
                                "kernel.mesh.admission = \"enforce\" requires kernel.mesh.genesis_hash"
                                    .into(),
                            ));
                        }
                        (mode, Some(genesis)) => {
                            #[cfg(feature = "exochain")]
                            let verdicts: Arc<dyn crate::mesh_admit::VerdictSource> =
                                mesh_verdicts.clone();
                            #[cfg(not(feature = "exochain"))]
                            let verdicts: Arc<dyn crate::mesh_admit::VerdictSource> = {
                                if mode == Adm::Enforce && !mesh_config.admission_open_membership {
                                    return Err(KernelError::Boot(
                                        "kernel.mesh.admission = \"enforce\" needs a governance \
                                         gate; this build has none. Set \
                                         kernel.mesh.admission_open_membership = true to admit \
                                         any valid hello"
                                            .into(),
                                    ));
                                }
                                Arc::new(crate::mesh_admit::OpenVerdicts)
                            };
                            Arc::new(crate::mesh_admit::CryptoGate::new(
                                genesis,
                                Arc::clone(&revocation_list),
                                verdicts,
                                mode,
                            ))
                        }
                    }
                };

                let require_authenticated_seeds = crate::mesh_admit_gate::nested_peer_ceiling_active();
                if let (Some(identity), Some(noise)) = (dial_identity.as_ref(), noise_config.as_ref()) {
                    let noise_static = crate::mesh_noise::noise_static_public(&noise.local_private_key)
                        .ok_or_else(|| KernelError::Boot("cannot derive mesh Noise key".into()))?;
                    runtime.set_authentication(crate::mesh_runtime::MeshAuthentication {
                        gate: Arc::clone(&admission_gate), identity: Arc::clone(identity),
                        noise_static, require_authenticated_seeds,
                    });
                } else if require_authenticated_seeds {
                    return Err(KernelError::Boot("nested mesh requires reciprocal authenticated admission".into()));
                }
                let rt = Arc::clone(&runtime);
                let conn_limits = crate::mesh_limits::Limits {
                    first_frame: std::time::Duration::from_secs(
                        mesh_config.first_frame_timeout_secs.max(1),
                    ),
                    per_ip: mesh_config.max_connections_per_ip.max(1),
                    ..crate::mesh_limits::Limits::default()
                };
                // Held by MeshService so `stop` ends the accept loop and the
                // seed dial loops instead of leaving them running.
                let listener_task = tokio::spawn(async move {
                    crate::mesh_serve::serve_listener_with(
                        rt,
                        listener,
                        noise_config,
                        transport.name(),
                        &listen_addr,
                        admission_gate,
                        conn_limits,
                    )
                    .await;
                });

                // Connect to seed peers.
                let mut mesh_tasks = crate::mesh_serve::connect_seeds(
                    &runtime,
                    &seed_peers,
                    &transport_name_for_seeds,
                    noise_for_seeds,
                    dial_identity,
                );
                mesh_tasks.push(listener_task);

                // WEFT-119: register Mesh as a SystemService so start/stop/
                // health_check are available via the kernel service surface.
                // start_all() (phase 8) flips the service to started; stop_all()
                // on shutdown disconnects peers.
                let mesh_svc = Arc::new(crate::mesh_system_service::MeshService::with_endpoints(
                    Arc::clone(&runtime),
                    listen_display.clone(),
                    transport_display.clone(),
                ));
                mesh_svc.adopt_tasks(mesh_tasks);
                if let Err(e) = service_registry.register(mesh_svc) {
                    error!(error = %e, "failed to register mesh service");
                } else {
                    boot_log.push(BootEvent::info(
                        BootPhase::Network,
                        "Mesh service registered (SystemService lifecycle)",
                    ));
                }

                boot_log.push(BootEvent::info(
                    BootPhase::Network,
                    format!(
                        "Mesh transport started ({} on {}, {} seed peers)",
                        transport_display,
                        listen_display,
                        seed_peers.len(),
                    ),
                ));

                Some(runtime)
            } else {
                boot_log.push(BootEvent::info(
                    BootPhase::Network,
                    if service_node_id.is_some() {
                        "Mesh transport owned by the machine mesh service"
                    } else {
                        "Mesh transport disabled"
                    },
                ));
                None
            }
        };

        // 6. Create cluster membership (universal, always present)
        let cluster_config = ClusterConfig {
            node_id: node_id.clone(),
            node_name: kernel_config
                .cluster
                .as_ref()
                .and_then(|c| c.node_name.clone())
                .unwrap_or_else(|| "local".into()),
            heartbeat_interval_secs: kernel_config
                .cluster
                .as_ref()
                .map(|c| c.heartbeat_interval_secs)
                .unwrap_or(5),
            ..ClusterConfig::default()
        };
        // Cluster peer membership persists to disk so joins survive restarts.
        let cluster_peers_path = runtime_paths.cluster_peers();
        let cluster_membership =
            Arc::new(ClusterMembership::new(cluster_config).with_persist_path(&cluster_peers_path));

        boot_log.push(BootEvent::info(
            BootPhase::Network,
            format!(
                "Cluster membership ready (node {}, {} peer(s) rehydrated)",
                cluster_membership.local_node_id(),
                cluster_membership.len(),
            ),
        ));

        // 6a. AppManager with on-disk manifest store (WEFT-136).
        // Installs survive kernel restarts via atomic apps.json (mirrors cluster_peers).
        let apps_persist_path =
            runtime_paths.apps();
        let app_manager = Arc::new(AppManager::new().with_persist_path(&apps_persist_path));
        boot_log.push(BootEvent::info(
            BootPhase::Services,
            format!(
                "AppManager ready ({} app(s) rehydrated from {})",
                app_manager.len(),
                apps_persist_path.display(),
            ),
        ));

        // 6b. Host revocation list (persistent ban list), loaded before the
        //     mesh block (5d) so the listener's admission gate shares it.
        {
            let count = revocation_list.len();
            if count > 0 {
                boot_log.push(BootEvent::info(
                    BootPhase::Network,
                    format!("Host revocation list loaded ({count} banned hosts)"),
                ));
            } else {
                boot_log.push(BootEvent::info(
                    BootPhase::Network,
                    "Host revocation list ready (empty)",
                ));
            }
        }

        // 7. Register cluster service (when feature-gated ruvector integration is enabled)
        //    WEFT-120: when mesh is live, subscribe ClusterService to the mesh
        //    peer-event stream so membership tracks joins/leaves/partitions.
        #[cfg(feature = "cluster")]
        {
            use crate::cluster::ClusterService;
            use ruvector_cluster::StaticDiscovery;

            let net_config = kernel_config.cluster.clone().unwrap_or_default();
            let seed_addrs: Vec<std::net::SocketAddr> = net_config
                .seed_nodes
                .iter()
                .filter_map(|s| s.parse().ok())
                .collect();
            let seed_nodes: Vec<ruvector_cluster::ClusterNode> = seed_addrs
                .into_iter()
                .map(|addr| ruvector_cluster::ClusterNode::new(addr.to_string(), addr))
                .collect();
            let discovery = Box::new(StaticDiscovery::new(seed_nodes));
            let node_id = cluster_membership.local_node_id().to_owned();

            match ClusterService::new(
                net_config,
                node_id,
                discovery,
                Arc::clone(&cluster_membership),
            ) {
                Ok(cluster_svc) => {
                    let svc = Arc::new(cluster_svc);

                    // WEFT-120: ClusterService ← mesh peer-event bus.
                    #[cfg(all(feature = "native", feature = "mesh"))]
                    {
                        if let Some(ref mr) = mesh_runtime {
                            let rx = mr.subscribe_peer_events();
                            svc.spawn_mesh_peer_listener(rx);
                            boot_log.push(BootEvent::info(
                                BootPhase::Network,
                                "ClusterService subscribed to mesh peer-event stream",
                            ));
                        }
                    }

                    if let Err(e) = service_registry.register(svc) {
                        error!(error = %e, "failed to register cluster service");
                    } else {
                        boot_log.push(BootEvent::info(
                            BootPhase::Network,
                            "Cluster service registered (ruvector)",
                        ));
                    }
                }
                Err(e) => {
                    error!(error = %e, "failed to create cluster service");
                    boot_log.push(BootEvent::info(
                        BootPhase::Network,
                        format!("Cluster service failed: {e}"),
                    ));
                }
            }
        }

        // WEFT-120 fallback: mesh without cluster feature — membership still
        // tracks live peers via the peer-event bus.
        #[cfg(all(feature = "native", feature = "mesh", not(feature = "cluster")))]
        {
            if let Some(ref mr) = mesh_runtime {
                let rx = mr.subscribe_peer_events();
                cluster_membership.spawn_mesh_peer_listener(rx);
                boot_log.push(BootEvent::info(
                    BootPhase::Network,
                    "ClusterMembership subscribed to mesh peer-event stream",
                ));
            }
        }

        // 8. Start services (none registered by default at boot, unless cluster feature added one)
        service_registry
            .start_all()
            .await
            .map_err(|e| KernelError::Boot(format!("service start failed: {e}")))?;

        // 8b. Initialize local exochain (when exochain feature is enabled)
        //     Restores from checkpoint file if available; otherwise fresh genesis.
        #[cfg(feature = "exochain")]
        let chain_manager = {
            let chain_config = kernel_config.chain.clone().unwrap_or_default();
            if chain_config.enabled {
                // Load or generate Ed25519 signing key for chain integrity.
                // ADR-103 A7, one project key: a project kernel's chain is
                // signed by the project key `pre_boot` loaded (the node key
                // seed), confirmed against `project.key`; never a
                // `chain.key` beside the checkpoint. Anything else refuses.
                let signing_key = if project_profile {
                    let child = crate::overlay_runtime::child_paths(&runtime_paths)
                        .map_err(|e| KernelError::BootRefused(e.boot_message()))?;
                    let path = child.project_key().ok_or_else(|| {
                        KernelError::BootRefused("profile `project` has no project key path".into())
                    })?;
                    let key = crate::project_identity::project_chain_key(&path, node_key_seed)
                        .map_err(|e| KernelError::BootRefused(e.to_string()))?;
                    boot_log.push(BootEvent::info(
                        BootPhase::Services,
                        format!("Ed25519 signing key is the project key: {}", path.display()),
                    ));
                    Some((key, path))
                } else if let Some(ref ckpt_path) =
                    chain_config.effective_checkpoint_path()
                {
                    // `~/.weftos/user.key` wins over `chain.key` for the user
                    // chain (ADR-103 D-5); every other chain keeps its own key.
                    let key_path = clawft_types::config::chain_paths::chain_key_for_checkpoint(
                        std::path::Path::new(ckpt_path),
                    );
                    // The user key is held to the same standard as the mesh
                    // client's reader: a symlink, a non-regular file or a key
                    // readable beyond its owner is refused, not followed.
                    #[cfg(unix)]
                    if key_path.file_name().is_some_and(|n| n == "user.key") {
                        use std::os::unix::fs::PermissionsExt;
                        if let Ok(m) = std::fs::symlink_metadata(&key_path)
                            && (!m.file_type().is_file() || m.permissions().mode() & 0o077 != 0)
                        {
                            return Err(KernelError::Boot(format!(
                                "{} must be a regular file readable only by its owner (chmod 600); refusing to sign the chain with it",
                                key_path.display()
                            )));
                        }
                    }
                    match crate::chain::ChainManager::load_or_create_key(&key_path) {
                        Ok(key) => {
                            boot_log.push(BootEvent::info(
                                BootPhase::Services,
                                format!("Ed25519 signing key loaded: {}", key_path.display()),
                            ));
                            Some((key, key_path))
                        }
                        Err(e) => {
                            warn!(error = %e, "failed to load/create signing key");
                            boot_log.push(BootEvent::info(
                                BootPhase::Services,
                                format!("Signing key unavailable: {e} — chain will be unsigned"),
                            ));
                            None
                        }
                    }
                } else {
                    None
                };

                let cm = if let Some(ref ckpt_path) = chain_config.effective_checkpoint_path() {
                    let json_path = std::path::PathBuf::from(ckpt_path);
                    // Derive RVF path from JSON path: same directory, `.rvf` extension
                    let rvf_path = json_path.with_extension("rvf");

                    if rvf_path.exists() {
                        // Prefer RVF format (cryptographic integrity verification)
                        match crate::chain::ChainManager::load_from_rvf(
                            &rvf_path,
                            chain_config.checkpoint_interval,
                        ) {
                            Ok(restored) => {
                                let seq = restored.sequence();
                                boot_log.push(BootEvent::info(
                                    BootPhase::Services,
                                    format!(
                                        "Chain restored from RVF (seq={}, chain_id={})",
                                        seq, chain_config.chain_id,
                                    ),
                                ));
                                Arc::new(restored)
                            }
                            Err(e) => {
                                error!(error = %e, "failed to restore RVF chain, trying JSON fallback");
                                // Fall back to JSON, which is unsigned: never once
                                // this node has cut over to ML-DSA-65 (ADR-129).
                                if json_path.exists()
                                    && matches!(crate::chain_pq::read_cutover_record(&rvf_path), Ok(None))
                                {
                                    match crate::chain::ChainManager::load_from_file(
                                        &json_path,
                                        chain_config.checkpoint_interval,
                                    ) {
                                        Ok(restored) => {
                                            let seq = restored.sequence();
                                            boot_log.push(BootEvent::info(
                                                BootPhase::Services,
                                                format!(
                                                    "Chain restored from JSON fallback (seq={}, chain_id={})",
                                                    seq, chain_config.chain_id,
                                                ),
                                            ));
                                            Arc::new(restored)
                                        }
                                        Err(e2) => {
                                            error!(error = %e2, "JSON fallback also failed; refusing to boot");
                                            return Err(KernelError::Boot(unrestorable_chain_msg(
                                                &rvf_path,
                                                &format!("RVF: {e}; JSON: {e2}"),
                                            )));
                                        }
                                    }
                                } else {
                                    return Err(KernelError::Boot(unrestorable_chain_msg(
                                        &rvf_path,
                                        &e.to_string(),
                                    )));
                                }
                            }
                        }
                    } else if json_path.exists() {
                        // Legacy JSON format
                        match crate::chain::ChainManager::load_from_file(
                            &json_path,
                            chain_config.checkpoint_interval,
                        ) {
                            Ok(restored) => {
                                let seq = restored.sequence();
                                boot_log.push(BootEvent::info(
                                    BootPhase::Services,
                                    format!(
                                        "Chain restored from JSON (seq={}, chain_id={}, will migrate to RVF)",
                                        seq, chain_config.chain_id,
                                    ),
                                ));
                                Arc::new(restored)
                            }
                            Err(e) => {
                                error!(error = %e, "failed to restore chain; refusing to boot");
                                return Err(KernelError::Boot(unrestorable_chain_msg(
                                    &json_path,
                                    &e.to_string(),
                                )));
                            }
                        }
                    } else {
                        Arc::new(crate::chain::ChainManager::new(
                            chain_config.chain_id,
                            chain_config.checkpoint_interval,
                        ))
                    }
                } else {
                    Arc::new(crate::chain::ChainManager::new(
                        chain_config.chain_id,
                        chain_config.checkpoint_interval,
                    ))
                };

                // Attach the Ed25519 signing key and, beside it, the node's
                // real ML-DSA-65 key (ADR-129): generated on the first boot
                // after the upgrade, then reused. Without it the chain stays
                // Ed25519-signed; nothing is derived from the Ed25519 key.
                let mut cm = cm;
                if let Some((key, key_path)) = signing_key
                    && let Some(inner) = Arc::get_mut(&mut cm)
                {
                    inner.set_signing_key(key);
                    let pq_path = crate::chain_pq::pq_key_path_for(&key_path);
                    match crate::chain_pq::load_or_create_pq_key(&key_path) {
                        Ok((ml_key, created)) => {
                            let id = ml_key.verifying_key().key_id();
                            inner.set_ml_dsa_key(ml_key);
                            boot_log.push(BootEvent::info(
                                BootPhase::Services,
                                format!(
                                    "Dual signing enabled (Ed25519 + ML-DSA-65 {id}, {} {})",
                                    if created { "generated" } else { "loaded" },
                                    pq_path.display()
                                ),
                            ));
                        }
                        Err(e) => {
                            warn!(error = %e, "ML-DSA-65 key unavailable; chain signing is Ed25519 only");
                            boot_log.push(BootEvent::info(
                                BootPhase::Services,
                                format!("ML-DSA-65 key unavailable: {e} — checkpoints will be Ed25519-only"),
                            ));
                        }
                    }
                }

                boot_log.push(BootEvent::info(
                    BootPhase::Services,
                    format!(
                        "Local chain ready (chain_id={}, seq={}, signed={})",
                        chain_config.chain_id,
                        cm.sequence(),
                        cm.has_signing_key(),
                    ),
                ));

                // ADR-103 A7: a project kernel stamps its effective rule hash on
                // every chain event, boot events included.
                if let Some(p) = overlay_prepared.as_ref() {
                    p.commit(&cm)
                        .map_err(|e| KernelError::BootRefused(e.boot_message()))?;
                    p.install_provider(&cm);
                }

                // Log boot phases to chain
                cm.append(
                    "kernel",
                    "boot.init",
                    Some(serde_json::json!({"version": "0.1.0"})),
                );
                cm.append(
                    "kernel",
                    "boot.config",
                    Some(serde_json::json!({
                        "max_processes": kernel_config.max_processes,
                        "health_interval": kernel_config.health_check_interval_secs,
                    })),
                );
                cm.append(
                    "kernel",
                    "boot.services",
                    Some(serde_json::json!({
                        "count": service_registry.len(),
                    })),
                );

                Some(cm)
            } else {
                boot_log.push(BootEvent::info(BootPhase::Services, "Local chain disabled"));
                None
            }
        };

        // Every subject revocation and un-revocation the list takes, from the
        // operator verb or a mesh notice, is chained (`workload.revoke`,
        // `workload.unrevoke`): the list records it itself, so no caller can
        // skip it.
        #[cfg(feature = "exochain")]
        if let Some(ref cm) = chain_manager {
            crate::workload_governance::chain_revocations(&revocation_list, Arc::clone(cm));
        }

        // 8b′. External chain-head anchoring controller (WEFT-137 / ADR-041).
        // Built from `kernel.chain.external_anchor` when present. Backend
        // `file` is the useful default; `external` wires an intent-log stub.
        #[cfg(feature = "exochain")]
        let chain_anchor = {
            let chain_config = kernel_config.chain.clone().unwrap_or_default();
            if !chain_config.enabled {
                None
            } else if let Some(ref acfg) = chain_config.external_anchor {
                match crate::chain_anchor::AnchoringController::from_config(acfg) {
                    Ok(Some(ctrl)) => {
                        boot_log.push(BootEvent::info(
                            BootPhase::Services,
                            format!(
                                "Chain external anchor ready (backend={}, min_interval={}s, min_events={})",
                                ctrl.backend_name(),
                                acfg.min_interval_secs,
                                acfg.min_events_between,
                            ),
                        ));
                        // Best-effort initial head anchor when a chain exists.
                        if let Some(ref cm) = chain_manager {
                            match ctrl.try_anchor_chain_head(cm) {
                                Ok(Some(receipt)) => {
                                    boot_log.push(BootEvent::info(
                                        BootPhase::Services,
                                        format!(
                                            "Chain head anchored at boot (tx_id={})",
                                            receipt.tx_id
                                        ),
                                    ));
                                }
                                Ok(None) => {
                                    // Policy suppressed (e.g. zero-event fresh chain
                                    // still anchors when gates are zero; otherwise skip).
                                }
                                Err(e) => {
                                    warn!(error = %e, "boot-time chain head anchor failed");
                                }
                            }
                        }
                        Some(Arc::new(ctrl))
                    }
                    Ok(None) => {
                        boot_log.push(BootEvent::info(
                            BootPhase::Services,
                            "Chain external anchor configured as none — skipped",
                        ));
                        None
                    }
                    Err(e) => {
                        warn!(error = %e, "failed to build chain external anchor");
                        boot_log.push(BootEvent::info(
                            BootPhase::Services,
                            format!("Chain external anchor unavailable: {e}"),
                        ));
                        None
                    }
                }
            } else {
                None
            }
        };

        // 8b-mesh. Wire the chain manager into the mesh runtime so every
        //          successful `handle_incoming` appends a `peer.envelope`
        //          event to the ExoChain. Makes mesh activity auditable
        //          via `weaver chain local` without adding a topic
        //          subscriber.
        #[cfg(all(feature = "native", feature = "mesh", feature = "exochain"))]
        if let (Some(mr), Some(cm)) = (mesh_runtime.as_ref(), chain_manager.as_ref()) {
            mr.set_chain_manager(Arc::clone(cm));
        }

        // 8c. Bootstrap resource tree via TreeManager (when exochain feature is enabled)
        //     First attempt to restore from checkpoint; fall back to fresh bootstrap.
        #[cfg(feature = "exochain")]
        let tree_manager = {
            let rt_config = kernel_config.resource_tree.clone().unwrap_or_default();
            if rt_config.enabled {
                if let Some(ref cm) = chain_manager {
                    let tm = Arc::new(crate::tree_manager::TreeManager::new(Arc::clone(cm)));

                    // Derive tree checkpoint path from chain checkpoint path
                    let chain_cfg = kernel_config.chain.clone().unwrap_or_default();
                    let tree_ckpt_path = chain_cfg
                        .effective_checkpoint_path()
                        .map(|p| std::path::PathBuf::from(p).with_extension("tree.json"));

                    let mut restored_from_checkpoint = false;
                    if let Some(ref tree_path) = tree_ckpt_path
                        && tree_path.exists()
                    {
                        match tm.load_checkpoint(tree_path) {
                            Ok(()) => {
                                let stats = tm.stats();
                                // Verify tree root hash against the chain's last recorded hash.
                                let chain_tree_hash = cm.last_tree_root_hash();
                                if let Some(ref expected) = chain_tree_hash {
                                    if stats.root_hash == *expected {
                                        boot_log.push(BootEvent::info(
                                            BootPhase::ResourceTree,
                                            format!(
                                                "Resource tree restored from checkpoint ({} nodes, root={}..., hash verified)",
                                                stats.node_count,
                                                &stats.root_hash[..12],
                                            ),
                                        ));
                                        restored_from_checkpoint = true;
                                    } else {
                                        warn!(
                                            expected = %expected,
                                            actual = %stats.root_hash,
                                            "tree checkpoint root hash mismatch — falling back to fresh bootstrap"
                                        );
                                        boot_log.push(BootEvent::info(
                                            BootPhase::ResourceTree,
                                            format!(
                                                "Tree checkpoint hash mismatch (expected={}..., got={}...), bootstrapping fresh",
                                                &expected[..std::cmp::min(12, expected.len())],
                                                &stats.root_hash[..12],
                                            ),
                                        ));
                                        // Don't set restored_from_checkpoint — falls through to bootstrap.
                                    }
                                } else {
                                    // No hash in chain to verify against — accept as-is.
                                    boot_log.push(BootEvent::info(
                                        BootPhase::ResourceTree,
                                        format!(
                                            "Resource tree restored from checkpoint ({} nodes, root={}...)",
                                            stats.node_count,
                                            &stats.root_hash[..12],
                                        ),
                                    ));
                                    restored_from_checkpoint = true;
                                }
                            }
                            Err(e) => {
                                error!(error = %e, "failed to restore tree checkpoint, bootstrapping fresh");
                            }
                        }
                    }

                    if !restored_from_checkpoint {
                        if let Err(e) = tm.bootstrap() {
                            error!(error = %e, "failed to bootstrap resource tree");
                            // Still allow boot to proceed without tree
                        } else {
                            let stats = tm.stats();
                            boot_log.push(BootEvent::info(
                                BootPhase::ResourceTree,
                                format!(
                                    "Resource tree bootstrapped ({} nodes, root={}...)",
                                    stats.node_count,
                                    &stats.root_hash[..12],
                                ),
                            ));
                        }
                    }

                    // Register cron service in tree with manifest (5b wiring)
                    if let Err(e) = tm.register_service_with_manifest("cron", "scheduler") {
                        tracing::debug!(error = %e, "failed to register cron in tree (may already exist)");
                    }

                    // K4: Register container namespace in the resource tree
                    {
                        use exo_resource_tree::model::{ResourceId, ResourceKind};
                        if let Err(e) = tm.insert(
                            ResourceId::new("/kernel/containers"),
                            ResourceKind::Namespace,
                            ResourceId::new("/kernel"),
                        ) {
                            tracing::debug!(
                                error = %e,
                                "failed to register containers namespace (may already exist)"
                            );
                        }
                    }

                    // K3c: Register ECC namespaces in the resource tree
                    #[cfg(feature = "ecc")]
                    {
                        use exo_resource_tree::model::{ResourceId, ResourceKind};
                        let ecc_namespaces = [
                            ("/kernel/services/ecc", "/kernel/services"),
                            ("/kernel/services/ecc/hnsw", "/kernel/services/ecc"),
                            ("/kernel/services/ecc/causal", "/kernel/services/ecc"),
                            ("/kernel/services/ecc/tick", "/kernel/services/ecc"),
                            ("/kernel/services/ecc/calibration", "/kernel/services/ecc"),
                            ("/kernel/services/ecc/crossrefs", "/kernel/services/ecc"),
                        ];
                        for (path, parent) in &ecc_namespaces {
                            if let Err(e) = tm.insert(
                                ResourceId::new(*path),
                                ResourceKind::Namespace,
                                ResourceId::new(*parent),
                            ) {
                                tracing::debug!(
                                    error = %e, path = *path,
                                    "failed to register ECC namespace (may already exist)"
                                );
                            }
                        }
                        boot_log.push(BootEvent::info(
                            BootPhase::Ecc,
                            "ECC resource tree namespaces registered",
                        ));
                    }

                    // K3: Register tool namespaces in the resource tree
                    {
                        use exo_resource_tree::model::{ResourceId, ResourceKind};
                        let tool_namespaces = [
                            ("/kernel/tools", "/kernel"),
                            ("/kernel/tools/fs", "/kernel/tools"),
                            ("/kernel/tools/agent", "/kernel/tools"),
                            ("/kernel/tools/sys", "/kernel/tools"),
                            ("/kernel/tools/ipc", "/kernel/tools"),
                            #[cfg(feature = "ecc")]
                            ("/kernel/tools/ecc", "/kernel/tools"),
                        ];
                        for (path, parent) in &tool_namespaces {
                            if let Err(e) = tm.insert(
                                ResourceId::new(*path),
                                ResourceKind::Namespace,
                                ResourceId::new(*parent),
                            ) {
                                tracing::debug!(
                                    error = %e, path = *path,
                                    "failed to register tool namespace (may already exist)"
                                );
                            }
                        }
                        // Register each built-in tool as a Tool node
                        let catalog = crate::wasm_runner::builtin_tool_catalog();
                        for spec in &catalog {
                            let cat_path = match spec.category {
                                crate::wasm_runner::ToolCategory::Filesystem => "/kernel/tools/fs",
                                crate::wasm_runner::ToolCategory::Agent => "/kernel/tools/agent",
                                crate::wasm_runner::ToolCategory::System => "/kernel/tools/sys",
                                crate::wasm_runner::ToolCategory::Ecc => "/kernel/tools/ecc",
                                crate::wasm_runner::ToolCategory::User => "/kernel/tools",
                            };
                            let tool_path = format!("{}/{}", cat_path, spec.name.replace('.', "/"));
                            if let Err(e) = tm.insert(
                                ResourceId::new(&tool_path),
                                ResourceKind::Tool,
                                ResourceId::new(cat_path),
                            ) {
                                tracing::debug!(
                                    error = %e, tool = %spec.name,
                                    "failed to register tool node (may already exist)"
                                );
                            }
                        }
                        boot_log.push(BootEvent::info(
                            BootPhase::ResourceTree,
                            format!(
                                "Registered {} built-in tools in resource tree",
                                catalog.len()
                            ),
                        ));
                    }

                    Some(tm)
                } else {
                    boot_log.push(BootEvent::info(
                        BootPhase::ResourceTree,
                        "Resource tree requires chain — skipped",
                    ));
                    None
                }
            } else {
                boot_log.push(BootEvent::info(
                    BootPhase::ResourceTree,
                    "Resource tree disabled",
                ));
                None
            }
        };

        // 8f. Initialize governance engine with chain-anchored genesis rules
        //
        // The governance gate is the constitutional layer of the kernel.
        // At boot, genesis rules are written to the chain as immutable
        // entries. Once anchored, they cannot be modified — only superseded
        // by a `governance.root.supersede` event that references the
        // original genesis sequence.
        #[cfg(feature = "exochain")]
        let governance_gate: Option<Arc<dyn crate::gate::GateBackend>> = {
            if let Some(ref cm) = chain_manager {
                use crate::gate::GovernanceGate;
                use crate::governance::{GovernanceBranch, GovernanceRule, RuleSeverity};

                // Default risk threshold (0.7 for production safety)
                let (risk_threshold, human_approval) = overlay_prepared
                    .as_ref()
                    .map_or((0.7, false), |p| p.engine_params());

                let mut gate =
                    GovernanceGate::new(risk_threshold, human_approval).with_chain(Arc::clone(cm));

                // ── Genesis governance rules (immutable chain entries) ────
                // These are the constitutional rules that govern all agent
                // behavior. Once written to the chain, they cannot be
                // modified — only superseded by a governance.root.supersede
                // event.

                let genesis_rules = vec![
                    // ── Core constitutional rules (GOV-001 .. GOV-007) ──────
                    GovernanceRule {
                        id: "GOV-001".into(),
                        description: "High-risk operations require elevated review".into(),
                        branch: GovernanceBranch::Judicial,
                        severity: RuleSeverity::Blocking,
                        active: true,
                        reference_url: None,
                        sop_category: None,
                        rule_type: Default::default(),
                        action_selector: None,
                        tool_selector: None,
                        force_on_match: false,
                    },
                    GovernanceRule {
                        id: "GOV-002".into(),
                        description: "Security-sensitive actions must not exceed security threshold".into(),
                        branch: GovernanceBranch::Judicial,
                        severity: RuleSeverity::Blocking,
                        active: true,
                        reference_url: None,
                        sop_category: None,
                        rule_type: Default::default(),
                        action_selector: None,
                        tool_selector: None,
                        force_on_match: false,
                    },
                    GovernanceRule {
                        id: "GOV-003".into(),
                        description: "Privacy-impacting operations flagged for review".into(),
                        branch: GovernanceBranch::Legislative,
                        severity: RuleSeverity::Warning,
                        active: true,
                        reference_url: None,
                        sop_category: None,
                        rule_type: Default::default(),
                        action_selector: None,
                        tool_selector: None,
                        force_on_match: false,
                    },
                    GovernanceRule {
                        id: "GOV-004".into(),
                        description: "Novel/unprecedented actions require advisory logging".into(),
                        branch: GovernanceBranch::Executive,
                        severity: RuleSeverity::Advisory,
                        active: true,
                        reference_url: None,
                        sop_category: None,
                        rule_type: Default::default(),
                        action_selector: None,
                        tool_selector: None,
                        force_on_match: false,
                    },
                    GovernanceRule {
                        id: "GOV-005".into(),
                        description: "Filesystem write operations scored for risk".into(),
                        branch: GovernanceBranch::Legislative,
                        severity: RuleSeverity::Warning,
                        active: true,
                        reference_url: None,
                        sop_category: None,
                        rule_type: Default::default(),
                        action_selector: None,
                        tool_selector: None,
                        force_on_match: false,
                    },
                    GovernanceRule {
                        id: "GOV-006".into(),
                        description: "Agent spawn operations require governance clearance".into(),
                        branch: GovernanceBranch::Executive,
                        severity: RuleSeverity::Blocking,
                        active: true,
                        reference_url: None,
                        sop_category: None,
                        rule_type: Default::default(),
                        action_selector: None,
                        tool_selector: None,
                        force_on_match: false,
                    },
                    GovernanceRule {
                        id: "GOV-007".into(),
                        description: "IPC messages between agents logged for audit trail".into(),
                        branch: GovernanceBranch::Judicial,
                        severity: RuleSeverity::Advisory,
                        active: true,
                        reference_url: None,
                        sop_category: None,
                        rule_type: Default::default(),
                        action_selector: None,
                        tool_selector: None,
                        force_on_match: false,
                    },
                    // ── AI-SDLC SOP rules: Legislative (6) ──────────────────
                    GovernanceRule {
                        id: "SOP-L001".into(),
                        description: "AI-IRB approval required before high-impact deployments".into(),
                        branch: GovernanceBranch::Legislative,
                        severity: RuleSeverity::Blocking,
                        active: true,
                        reference_url: Some("https://github.com/AISDLC/AI-SDLC-SOPs/blob/main/sops/SOP-1300-01-AI_IRB_Approval.md".into()),
                        sop_category: Some("governance".into()),
                        rule_type: Default::default(),
                        action_selector: None,
                        tool_selector: None,
                        force_on_match: false,
                    },
                    GovernanceRule {
                        id: "SOP-L002".into(),
                        description: "Version control and branching policies must be enforced".into(),
                        branch: GovernanceBranch::Legislative,
                        severity: RuleSeverity::Warning,
                        active: true,
                        reference_url: Some("https://github.com/AISDLC/AI-SDLC-SOPs/blob/main/sops/SOP-1003-01-AI_Version_Control.md".into()),
                        sop_category: Some("governance".into()),
                        rule_type: Default::default(),
                        action_selector: None,
                        tool_selector: None,
                        force_on_match: false,
                    },
                    GovernanceRule {
                        id: "SOP-L003".into(),
                        description: "Requirements must include AI-IRB ethical review".into(),
                        branch: GovernanceBranch::Legislative,
                        severity: RuleSeverity::Warning,
                        active: true,
                        reference_url: Some("https://github.com/AISDLC/AI-SDLC-SOPs/blob/main/sops/SOP-1040-01-AI_Requirements.md".into()),
                        sop_category: Some("engineering".into()),
                        rule_type: Default::default(),
                        action_selector: None,
                        tool_selector: None,
                        force_on_match: false,
                    },
                    GovernanceRule {
                        id: "SOP-L004".into(),
                        description: "Release planning must follow structured lifecycle gates".into(),
                        branch: GovernanceBranch::Legislative,
                        severity: RuleSeverity::Advisory,
                        active: true,
                        reference_url: Some("https://github.com/AISDLC/AI-SDLC-SOPs/blob/main/sops/SOP-1005-01-AI_Release_Planning.md".into()),
                        sop_category: Some("lifecycle".into()),
                        rule_type: Default::default(),
                        action_selector: None,
                        tool_selector: None,
                        force_on_match: false,
                    },
                    GovernanceRule {
                        id: "SOP-L005".into(),
                        description: "Data protection and PII handling must comply with policy".into(),
                        branch: GovernanceBranch::Legislative,
                        severity: RuleSeverity::Blocking,
                        active: true,
                        reference_url: Some("https://github.com/AISDLC/AI-SDLC-SOPs/blob/main/sops/SOP-1303-01-AI_Data_Protection.md".into()),
                        sop_category: Some("ethics".into()),
                        rule_type: Default::default(),
                        action_selector: None,
                        tool_selector: None,
                        force_on_match: false,
                    },
                    GovernanceRule {
                        id: "SOP-L006".into(),
                        description: "Risk register must be maintained and reviewed".into(),
                        branch: GovernanceBranch::Legislative,
                        severity: RuleSeverity::Warning,
                        active: true,
                        reference_url: Some("https://github.com/AISDLC/AI-SDLC-SOPs/blob/main/sops/SOP-1062-01-AI_Risk_Register.md".into()),
                        sop_category: Some("governance".into()),
                        rule_type: Default::default(),
                        action_selector: None,
                        tool_selector: None,
                        force_on_match: false,
                    },
                    // ── AI-SDLC SOP rules: Executive (5) ────────────────────
                    GovernanceRule {
                        id: "SOP-E001".into(),
                        description: "Secure coding standards must be followed".into(),
                        branch: GovernanceBranch::Executive,
                        severity: RuleSeverity::Warning,
                        active: true,
                        reference_url: Some("https://github.com/AISDLC/AI-SDLC-SOPs/blob/main/sops/SOP-1200-01-AI_Secure_Coding.md".into()),
                        sop_category: Some("engineering".into()),
                        rule_type: Default::default(),
                        action_selector: None,
                        tool_selector: None,
                        force_on_match: false,
                    },
                    GovernanceRule {
                        id: "SOP-E002".into(),
                        description: "Deployment requires governance clearance checkpoint".into(),
                        branch: GovernanceBranch::Executive,
                        severity: RuleSeverity::Blocking,
                        active: true,
                        reference_url: Some("https://github.com/AISDLC/AI-SDLC-SOPs/blob/main/sops/SOP-1220-01-AI_Deployment_Clearance.md".into()),
                        sop_category: Some("lifecycle".into()),
                        rule_type: Default::default(),
                        action_selector: None,
                        tool_selector: None,
                        force_on_match: false,
                    },
                    GovernanceRule {
                        id: "SOP-E003".into(),
                        description: "Incident response procedures must be documented and followed".into(),
                        branch: GovernanceBranch::Executive,
                        severity: RuleSeverity::Warning,
                        active: true,
                        reference_url: Some("https://github.com/AISDLC/AI-SDLC-SOPs/blob/main/sops/SOP-1008-01-AI_Incident_Response.md".into()),
                        sop_category: Some("security".into()),
                        rule_type: Default::default(),
                        action_selector: None,
                        tool_selector: None,
                        force_on_match: false,
                    },
                    GovernanceRule {
                        id: "SOP-E004".into(),
                        description: "Decommissioning must follow structured teardown procedure".into(),
                        branch: GovernanceBranch::Executive,
                        severity: RuleSeverity::Advisory,
                        active: true,
                        reference_url: Some("https://github.com/AISDLC/AI-SDLC-SOPs/blob/main/sops/SOP-1011-01-AI_Decommissioning.md".into()),
                        sop_category: Some("lifecycle".into()),
                        rule_type: Default::default(),
                        action_selector: None,
                        tool_selector: None,
                        force_on_match: false,
                    },
                    GovernanceRule {
                        id: "SOP-E005".into(),
                        description: "Third-party AI procurement requires screening".into(),
                        branch: GovernanceBranch::Executive,
                        severity: RuleSeverity::Advisory,
                        active: true,
                        reference_url: Some("https://github.com/AISDLC/AI-SDLC-SOPs/blob/main/sops/SOP-1004-01-AI_Procurement_Screening.md".into()),
                        sop_category: Some("governance".into()),
                        rule_type: Default::default(),
                        action_selector: None,
                        tool_selector: None,
                        force_on_match: false,
                    },
                    // ── AI-SDLC SOP rules: Judicial (4) ─────────────────────
                    GovernanceRule {
                        id: "SOP-J001".into(),
                        description: "Bias and fairness assessments required for model outputs".into(),
                        branch: GovernanceBranch::Judicial,
                        severity: RuleSeverity::Blocking,
                        active: true,
                        reference_url: Some("https://github.com/AISDLC/AI-SDLC-SOPs/blob/main/sops/SOP-1301-01-AI_Bias_Fairness.md".into()),
                        sop_category: Some("ethics".into()),
                        rule_type: Default::default(),
                        action_selector: None,
                        tool_selector: None,
                        force_on_match: false,
                    },
                    GovernanceRule {
                        id: "SOP-J002".into(),
                        description: "Explainability documentation required for decision systems".into(),
                        branch: GovernanceBranch::Judicial,
                        severity: RuleSeverity::Warning,
                        active: true,
                        reference_url: Some("https://github.com/AISDLC/AI-SDLC-SOPs/blob/main/sops/SOP-1302-01-AI_Explainability.md".into()),
                        sop_category: Some("ethics".into()),
                        rule_type: Default::default(),
                        action_selector: None,
                        tool_selector: None,
                        force_on_match: false,
                    },
                    GovernanceRule {
                        id: "SOP-J003".into(),
                        description: "Model drift detection and monitoring must be active".into(),
                        branch: GovernanceBranch::Judicial,
                        severity: RuleSeverity::Warning,
                        active: true,
                        reference_url: Some("https://github.com/AISDLC/AI-SDLC-SOPs/blob/main/sops/SOP-1009-01-AI_Drift_Detection.md".into()),
                        sop_category: Some("lifecycle".into()),
                        rule_type: Default::default(),
                        action_selector: None,
                        tool_selector: None,
                        force_on_match: false,
                    },
                    GovernanceRule {
                        id: "SOP-J004".into(),
                        description: "Quality records must be maintained for audit compliance".into(),
                        branch: GovernanceBranch::Judicial,
                        severity: RuleSeverity::Advisory,
                        active: true,
                        reference_url: Some("https://github.com/AISDLC/AI-SDLC-SOPs/blob/main/sops/SOP-2002-01-AI_Quality_Records.md".into()),
                        sop_category: Some("quality".into()),
                        rule_type: Default::default(),
                        action_selector: None,
                        tool_selector: None,
                        force_on_match: false,
                    },
                    // ── Browser policy rules (S7 / WEFT-108) ────────────────
                    // Browser nodes are untrusted; these rules constrain IPC
                    // topics, spawn, and network without relying on effect
                    // magnitude. Operators may supersede via governance.root.
                ]
                .into_iter()
                .chain(crate::governance::browser_policy_default_rules())
                // WEFT-342: soul.binding_thread_intact (binding-thread integrity)
                .chain(crate::governance::binding_thread_default_rules())
                // ADR-099 s4: default-deny for workload.* actions.
                .chain(crate::workload_governance::default_rules())
                .collect::<Vec<_>>();
                // ADR-103 D8 (project profile): the effective rules replace the
                // default set.
                let genesis_rules = match overlay_prepared.as_ref() {
                    Some(p) => p.rules().to_vec(),
                    None => genesis_rules,
                };

                // Anchor genesis rules to chain
                let genesis_seq = cm.sequence();
                let rules_json: Vec<serde_json::Value> = genesis_rules
                    .iter()
                    .map(|r| {
                        serde_json::json!({
                            "id": r.id,
                            "description": r.description,
                            "branch": format!("{}", r.branch),
                            "severity": format!("{}", r.severity),
                            "reference_url": r.reference_url,
                            "sop_category": r.sop_category,
                            "rule_type": format!("{}", r.rule_type),
                        })
                    })
                    .collect();

                // The governance.genesis event is the ROOT — it establishes the
                // constitutional rules. Its chain sequence is the governance root.
                cm.append(
                    "governance",
                    "governance.genesis",
                    Some(serde_json::json!({
                        "version": "2.0.0",
                        "risk_threshold": risk_threshold,
                        "human_approval_required": human_approval,
                        "rules": rules_json,
                        "rule_count": genesis_rules.len(),
                        "genesis_seq": genesis_seq,
                    })),
                );

                // Anchor each rule individually for granular verification
                for rule in &genesis_rules {
                    cm.append(
                        "governance",
                        "governance.rule",
                        Some(serde_json::json!({
                            "rule_id": rule.id,
                            "branch": format!("{}", rule.branch),
                            "severity": format!("{}", rule.severity),
                            "genesis_seq": genesis_seq,
                        })),
                    );
                }

                // Add rules to the gate
                for rule in genesis_rules {
                    gate = gate.add_rule(rule);
                }

                boot_log.push(BootEvent::info(
                    BootPhase::Services,
                    format!(
                        "Governance genesis anchored (seq={}, {} rules, threshold={:.1})",
                        genesis_seq,
                        gate.engine().rule_count(),
                        risk_threshold,
                    ),
                ));

                match overlay_prepared.take() {
                    Some(p) => {
                        let (gate, rt) = p.into_runtime(gate, Arc::clone(cm));
                        overlay_runtime = Some(rt);
                        Some(gate)
                    }
                    None => Some(Arc::new(gate) as Arc<dyn crate::gate::GateBackend>),
                }
            } else {
                if overlay_prepared.is_some() {
                    return Err(KernelError::BootRefused(
                        "profile `project` needs a chain: governance cannot run open".into(),
                    ));
                }
                boot_log.push(BootEvent::info(
                    BootPhase::Services,
                    "Governance: no chain available, using open governance",
                ));
                None
            }
        };

        #[cfg(all(feature = "native", feature = "mesh", feature = "exochain"))]
        match &governance_gate {
            Some(g) => mesh_verdicts.bind(Arc::clone(g)),
            None if mesh_closed_without_gate => {
                error!(
                    "mesh admission = enforce but no governance gate exists: every peer will be \
                     refused. Enable the chain/governance, or set \
                     kernel.mesh.admission_open_membership = true to admit any valid hello"
                );
                mesh_verdicts.bind_closed();
            }
            None => {
                warn!("mesh admission_open_membership: admitting any peer with a valid hello");
                mesh_verdicts.bind_open();
            }
        }

        // Wire governance gate into A2A router for dual-layer enforcement.
        // The A2ARouter is already behind Arc, so we use set_gate() which
        // relies on OnceLock interior mutability.
        #[cfg(feature = "exochain")]
        if let Some(ref gate) = governance_gate {
            a2a_router.set_gate(Arc::clone(gate));
        }

        // 8d. Log cluster and boot.ready chain events
        #[cfg(feature = "exochain")]
        if let Some(ref cm) = chain_manager {
            cm.append(
                "kernel",
                "boot.cluster",
                Some(serde_json::json!({
                    "node_id": cluster_membership.local_node_id(),
                })),
            );

            let elapsed_ms = boot_time.elapsed().as_millis() as u64;
            let mut ready_payload = serde_json::json!({
                "elapsed_ms": elapsed_ms,
                "processes": process_table.len(),
                "services": service_registry.len(),
            });

            if let Some(ref tm) = tree_manager {
                let root_hash = tm.stats().root_hash;
                ready_payload
                    .as_object_mut()
                    .unwrap()
                    .insert("tree_root_hash".to_string(), serde_json::json!(root_hash));
            }

            cm.append("kernel", "boot.ready", Some(ready_payload));

            // Emit boot manifest as a chain event (becomes an ExochainCheckpoint
            // segment in RVF persistence, capturing the complete boot state).
            let mut manifest = serde_json::json!({
                "version": env!("CARGO_PKG_VERSION"),
                "node_id": cluster_membership.local_node_id(),
                "process_count": process_table.len(),
                "service_count": service_registry.len(),
                "chain_sequence": cm.sequence(),
                "boot_elapsed_ms": boot_time.elapsed().as_millis() as u64,
            });
            if let Some(ref tm) = tree_manager {
                let stats = tm.stats();
                manifest.as_object_mut().unwrap().insert(
                    "tree_root_hash".to_string(),
                    serde_json::json!(stats.root_hash),
                );
                manifest.as_object_mut().unwrap().insert(
                    "tree_node_count".to_string(),
                    serde_json::json!(stats.node_count),
                );
            }
            cm.append("kernel", "boot.manifest", Some(manifest));
        }

        // 8e. Initialize ECC cognitive substrate (when ecc feature is enabled)
        #[cfg(feature = "ecc")]
        let (
            ecc_hnsw,
            ecc_causal,
            ecc_tick,
            ecc_crossrefs,
            ecc_impulses,
            ecc_calibration,
            ecc_vector_backend,
            ecc_eml_model,
            ecc_spatial,
        ) = {
            use crate::calibration::{EccCalibrationConfig, run_calibration};
            use crate::causal::CausalGraph;
            use crate::cognitive_tick::{CognitiveTick, CognitiveTickConfig};
            use crate::crossref::CrossRefStore;
            use crate::hnsw_service::{HnswService, HnswServiceConfig};
            use crate::impulse::ImpulseQueue;
            use crate::service::SystemService;
            use crate::spatial_service::SpatialService;
            use crate::vector_backend::VectorBackend;
            use crate::vector_diskann::{
                DiskAnnBackend, DiskAnnConfig, DiskAnnFeatureStatus, diskann_feature_status,
            };
            use crate::vector_hnsw::HnswBackend;
            use crate::vector_hybrid::{HybridBackend, HybridConfig};

            boot_log.push(BootEvent::info(
                BootPhase::Ecc,
                "Initializing ECC cognitive substrate",
            ));

            let hnsw = Arc::new(HnswService::new(HnswServiceConfig::default()));
            let causal = Arc::new(CausalGraph::new());
            let crossrefs = Arc::new(CrossRefStore::new());
            let impulses = Arc::new(ImpulseQueue::new());

            // Construct the vector backend based on kernel config.
            let vector_config = kernel_config.vector.clone();
            let vector_strict = vector_config.as_ref().map(|v| v.strict).unwrap_or(false);
            let vector_backend: Arc<dyn VectorBackend> =
                match vector_config.as_ref().map(|v| v.backend) {
                    Some(clawft_types::config::VectorBackendKind::DiskAnn) => {
                        match diskann_feature_status(
                            clawft_types::config::VectorBackendKind::DiskAnn,
                            cfg!(feature = "diskann"),
                            vector_strict,
                        ) {
                            DiskAnnFeatureStatus::StubRejected => {
                                return Err(KernelError::BootRefused(
                                    DISKANN_FEATURE_MISMATCH_ERROR.to_string(),
                                ));
                            }
                            DiskAnnFeatureStatus::StubDegraded => {
                                boot_log.push(BootEvent::warn(
                                    BootPhase::Ecc,
                                    DISKANN_FEATURE_MISMATCH_WARNING,
                                ));
                            }
                            DiskAnnFeatureStatus::Ok => {}
                        }
                        let da_cfg = vector_config
                            .as_ref()
                            .and_then(|v| v.diskann.as_ref())
                            .map(|d| DiskAnnConfig {
                                max_points: d.max_points,
                                dimensions: d.dimensions,
                                num_neighbors: d.num_neighbors,
                                search_list_size: d.search_list_size,
                                data_path: d.data_path.clone(),
                                use_pq: d.use_pq,
                                pq_num_chunks: d.pq_num_chunks,
                            })
                            .unwrap_or_default();
                        boot_log.push(BootEvent::info(
                            BootPhase::Ecc,
                            if cfg!(feature = "diskann") {
                                "Vector backend: DiskANN (real: Vamana graph + PQ)"
                            } else {
                                "Vector backend: DiskANN (STUB: brute-force O(n) linear scan)"
                            },
                        ));
                        Arc::new(DiskAnnBackend::new(da_cfg))
                    }
                    Some(clawft_types::config::VectorBackendKind::Hybrid) => {
                        match diskann_feature_status(
                            clawft_types::config::VectorBackendKind::Hybrid,
                            cfg!(feature = "diskann"),
                            vector_strict,
                        ) {
                            DiskAnnFeatureStatus::StubRejected => {
                                return Err(KernelError::BootRefused(
                                    DISKANN_FEATURE_MISMATCH_ERROR.to_string(),
                                ));
                            }
                            DiskAnnFeatureStatus::StubDegraded => {
                                boot_log.push(BootEvent::warn(
                                    BootPhase::Ecc,
                                    DISKANN_FEATURE_MISMATCH_WARNING,
                                ));
                            }
                            DiskAnnFeatureStatus::Ok => {}
                        }
                        let hnsw_cfg = vector_config
                            .as_ref()
                            .and_then(|v| v.hnsw.as_ref())
                            .map(|h| HnswServiceConfig {
                                ef_construction: h.ef_construction,
                                ef_search: 100,
                                default_dimensions: 384,
                            })
                            .unwrap_or_default();
                        let da_cfg = vector_config
                            .as_ref()
                            .and_then(|v| v.diskann.as_ref())
                            .map(|d| DiskAnnConfig {
                                max_points: d.max_points,
                                dimensions: d.dimensions,
                                num_neighbors: d.num_neighbors,
                                search_list_size: d.search_list_size,
                                data_path: d.data_path.clone(),
                                use_pq: d.use_pq,
                                pq_num_chunks: d.pq_num_chunks,
                            })
                            .unwrap_or_default();
                        let hybrid_cfg = vector_config
                            .as_ref()
                            .and_then(|v| v.hybrid.as_ref())
                            .map(|h| HybridConfig {
                                hot_capacity: h.hot_capacity,
                                promotion_threshold: h.promotion_threshold,
                                eviction_policy: crate::vector_hybrid::EvictionPolicy::Lru,
                            })
                            .unwrap_or_default();
                        boot_log.push(BootEvent::info(
                            BootPhase::Ecc,
                            format!(
                                "Vector backend: Hybrid (hot={}, threshold={}, cold={})",
                                hybrid_cfg.hot_capacity,
                                hybrid_cfg.promotion_threshold,
                                if cfg!(feature = "diskann") {
                                    "DiskANN real"
                                } else {
                                    "DiskANN STUB"
                                },
                            ),
                        ));
                        Arc::new(HybridBackend::new(hnsw_cfg, da_cfg, hybrid_cfg))
                    }
                    _ => {
                        // Default: HNSW only.
                        boot_log.push(BootEvent::info(
                            BootPhase::Ecc,
                            "Vector backend: HNSW (in-memory)",
                        ));
                        Arc::new(HnswBackend::with_defaults())
                    }
                };

            // Run boot-time calibration
            let cal_config = EccCalibrationConfig::default();
            let calibration = run_calibration(&hnsw, &causal, &cal_config);

            boot_log.push(BootEvent::info(
                BootPhase::Ecc,
                format!(
                    "ECC calibration complete (p50={}us, p95={}us, tick={}ms, spectral={})",
                    calibration.compute_p50_us,
                    calibration.compute_p95_us,
                    calibration.tick_interval_ms,
                    calibration.spectral_capable,
                ),
            ));

            // Create cognitive tick with calibrated interval
            let tick_config = CognitiveTickConfig {
                tick_interval_ms: calibration.tick_interval_ms,
                ..CognitiveTickConfig::default()
            };
            let tick = Arc::new(CognitiveTick::new(tick_config));

            // Register ECC services
            if let Err(e) = service_registry.register(hnsw.clone()) {
                tracing::debug!(error = %e, "failed to register HNSW service");
            }
            if let Err(e) = service_registry.register(tick.clone()) {
                tracing::debug!(error = %e, "failed to register cognitive tick service");
            }

            // Start the cognitive tick service explicitly -- start_all() ran
            // before ECC init so this service was never started, which caused
            // health checks to report "degraded - cognitive tick not running".
            if let Err(e) = tick.start().await {
                tracing::warn!(error = %e, "failed to start cognitive tick service");
            }

            // Spawn the DEMOCRITUS two-tier coherence tick loop.
            // This runs in the background, using the EML model for O(1)
            // predictions on every tick and falling back to exact Lanczos
            // spectral analysis when drift is detected.
            let eml_model = Arc::new(std::sync::Mutex::new(
                crate::eml_coherence::EmlCoherenceModel::new(),
            ));
            {
                let tick_clone = tick.clone();
                let causal_clone = causal.clone();
                let hnsw_clone = hnsw.clone();
                let eml_clone = eml_model.clone();
                tokio::spawn(async move {
                    crate::cognitive_tick::run_democritus_loop(
                        tick_clone,
                        causal_clone,
                        hnsw_clone,
                        eml_clone,
                    )
                    .await;
                });
            }
            boot_log.push(BootEvent::info(
                BootPhase::Ecc,
                "DEMOCRITUS two-tier coherence loop spawned",
            ));

            // Log calibration to chain
            #[cfg(feature = "exochain")]
            if let Some(ref cm) = chain_manager {
                cm.append(
                    "ecc",
                    "ecc.boot.calibration",
                    Some(serde_json::json!({
                        "compute_p50_us": calibration.compute_p50_us,
                        "compute_p95_us": calibration.compute_p95_us,
                        "tick_interval_ms": calibration.tick_interval_ms,
                        "spectral_capable": calibration.spectral_capable,
                    })),
                );
            }

            // Spatial BVH (opt-in via [kernel.spatial].enabled)
            let spatial_svc: Option<Arc<SpatialService>> = {
                let spatial_cfg = kernel_config.spatial.clone().unwrap_or_default();
                if spatial_cfg.enabled {
                    let svc = Arc::new(SpatialService::from_spatial_config(&spatial_cfg));
                    #[cfg(feature = "exochain")]
                    if let Some(ref cm) = chain_manager {
                        // Restore from the loaded chain first; attaching the sink after keeps the
                        // replay from being appended a second time.
                        let (applied, skipped) = svc.restore_from_chain(cm);
                        if applied + skipped > 0 {
                            boot_log.push(BootEvent::info(
                                BootPhase::Ecc,
                                format!("Spatial BVH restored from chain ({applied} events applied, {skipped} skipped)"),
                            ));
                        }
                        svc.attach_chain(cm.clone());
                    }
                    if let Err(e) = service_registry.register(svc.clone()) {
                        tracing::debug!(error = %e, "failed to register spatial service");
                    }
                    if let Err(e) = svc.start().await {
                        tracing::warn!(error = %e, "failed to start spatial service");
                    }
                    boot_log.push(BootEvent::info(
                        BootPhase::Ecc,
                        format!(
                            "Spatial BVH ready (backend={}, max_leaves={})",
                            svc.backend().backend_name(),
                            spatial_cfg.max_leaves,
                        ),
                    ));
                    Some(svc)
                } else {
                    boot_log.push(BootEvent::info(
                        BootPhase::Ecc,
                        "Spatial BVH disabled (set kernel.spatial.enabled = true)",
                    ));
                    None
                }
            };

            boot_log.push(BootEvent::info(
                BootPhase::Ecc,
                format!(
                    "ECC ready (hnsw={}, causal={} nodes, tick={}ms, vector={}, spatial={})",
                    hnsw.len(),
                    causal.node_count(),
                    calibration.tick_interval_ms,
                    vector_backend.backend_name(),
                    spatial_svc
                        .as_ref()
                        .map(|s| s.backend().backend_name())
                        .unwrap_or("off"),
                ),
            ));

            (
                Some(hnsw),
                Some(causal),
                Some(tick),
                Some(crossrefs),
                Some(impulses),
                Some(calibration),
                Some(vector_backend),
                Some(eml_model),
                spatial_svc,
            )
        };

        // 8g. Initialize os-patterns observability modules
        #[cfg(feature = "os-patterns")]
        let (metrics_registry, log_svc, timer_svc, dead_letter_queue) = {
            use crate::dead_letter::DeadLetterQueue;
            use crate::log_service::LogService;
            use crate::metrics::MetricsRegistry;
            use crate::timer::TimerService;

            boot_log.push(BootEvent::info(
                BootPhase::Services,
                "Initializing os-patterns observability modules",
            ));

            // MetricsRegistry with built-in + boot-specific gauges
            let registry = Arc::new(MetricsRegistry::with_builtins());
            // Seed additional kernel gauges requested by W3
            registry.gauge_set("kernel.process.count", process_table.len() as i64);
            registry.gauge_set("kernel.agent.count", 0);
            registry.gauge_set("kernel.ipc.messages_sent", 0);
            registry.counter_add("kernel.ipc.messages_failed", 0);
            registry.gauge_set("kernel.chain.height", 0);
            registry.gauge_set("kernel.uptime_secs", 0);

            boot_log.push(BootEvent::info(
                BootPhase::Services,
                "MetricsRegistry ready (built-in + kernel gauges)",
            ));

            // LogService (ring-buffer structured logs)
            let log_svc = Arc::new(LogService::with_default_capacity());
            boot_log.push(BootEvent::info(BootPhase::Services, "LogService ready"));

            // TimerService (sub-second precision timers)
            let timer_svc = Arc::new(TimerService::new());
            boot_log.push(BootEvent::info(BootPhase::Services, "TimerService ready"));

            // DeadLetterQueue (undeliverable message sink)
            let dlq = Arc::new(DeadLetterQueue::with_default_capacity());
            boot_log.push(BootEvent::info(
                BootPhase::Services,
                "DeadLetterQueue ready",
            ));

            // Wire DLQ into the A2A router
            a2a_router.set_dead_letter_queue(Arc::clone(&dlq));

            (Some(registry), Some(log_svc), Some(timer_svc), Some(dlq))
        };

        let elapsed = boot_time.elapsed();
        boot_log.push(BootEvent::info(
            BootPhase::Ready,
            format!(
                "Boot complete in {:.1}s ({} processes, {} services)",
                elapsed.as_secs_f64(),
                process_table.len(),
                service_registry.len(),
            ),
        ));

        info!(
            elapsed_ms = elapsed.as_millis(),
            processes = process_table.len(),
            services = service_registry.len(),
            "kernel boot complete"
        );

        // 9. Create agent supervisor
        let supervisor = AgentSupervisor::new(
            process_table.clone(),
            ipc.clone(),
            AgentCapabilities::default(),
        );

        // 9b. Wire A2ARouter and cron into supervisor
        #[cfg(feature = "native")]
        let supervisor = supervisor.with_a2a_router(a2a_router.clone(), cron_svc.clone());

        // 9c. Wire exochain managers into supervisor
        #[cfg(feature = "exochain")]
        let supervisor = supervisor.with_exochain(tree_manager.clone(), chain_manager.clone());

        // 10. Seed the event ring buffer with boot events
        let event_log = Arc::new(KernelEventLog::new());
        event_log.ingest_boot_log(&boot_log);

        Ok(Self {
            state: KernelState::Running,
            config: kernel_config,
            app_context: Some(app_context),
            bus,
            process_table,
            service_registry,
            ipc,
            #[cfg(feature = "native")]
            a2a_router,
            #[cfg(feature = "native")]
            cron_service: cron_svc,
            #[cfg(feature = "native")]
            assessment_service: assessment_svc,
            health,
            supervisor,
            boot_log,
            event_log,
            boot_time,
            cluster_membership,
            app_manager,
            revocation_list,
            #[cfg(feature = "native")]
            agent_registry: crate::agent_registry::AgentRegistry::new(),
            #[cfg(feature = "native")]
            node_registry: crate::node_registry::NodeRegistry::new(),
            #[cfg(feature = "native")]
            substrate_service: crate::substrate_service::SubstrateService::new(),
            #[cfg(feature = "exochain")]
            chain: ChainSubsystem {
                chain_manager,
                tree_manager,
                governance_gate,
                chain_anchor,
                overlay: overlay_runtime,
                _chain_lock: chain_lock,
            },
            #[cfg(feature = "ecc")]
            ecc: EccSubsystem {
                hnsw: ecc_hnsw,
                causal: ecc_causal,
                tick: ecc_tick,
                crossrefs: ecc_crossrefs,
                impulses: ecc_impulses,
                calibration: ecc_calibration,
                vector_backend: ecc_vector_backend,
                eml_coherence: ecc_eml_model,
                spatial: ecc_spatial,
            },
            #[cfg(feature = "os-patterns")]
            observability: ObservabilitySubsystem {
                metrics_registry,
                log_service: log_svc,
                timer_service: timer_svc,
                dead_letter_queue,
            },
        })
    }

    /// Shut down the kernel gracefully.
    ///
    /// Stops all services, cancels all processes, and transitions
    /// to the `Halted` state.
    pub async fn shutdown(&mut self) -> KernelResult<()> {
        if self.state != KernelState::Running {
            return Err(KernelError::WrongState {
                expected: "Running".into(),
                actual: self.state.to_string(),
            });
        }

        info!("kernel shutting down");
        self.state = KernelState::ShuttingDown;
        self.event_log.info("kernel", "shutdown initiated");

        // Stop all services
        if let Err(e) = self.service_registry.stop_all().await {
            error!(error = %e, "error stopping services during shutdown");
        }

        // Checkpoint tree+chain before shutting down
        #[cfg(feature = "exochain")]
        if let Some(ref tm) = self.chain.tree_manager
            && let Some(ref cm) = self.chain.chain_manager
        {
            let stats = tm.stats();
            cm.append(
                "kernel",
                "shutdown",
                Some(serde_json::json!({
                    "tree_root_hash": stats.root_hash,
                    "chain_seq": cm.sequence(),
                    "tree_nodes": stats.node_count,
                })),
            );
        }

        // Abort all running agent tasks
        #[cfg(feature = "native")]
        self.supervisor.abort_all();

        // Cancel all processes
        for entry in self.process_table.list() {
            if entry.pid == 0 {
                continue; // Don't cancel the kernel process
            }
            entry.cancel_token.cancel();

            // Log agent stop in tree/chain
            #[cfg(feature = "exochain")]
            if let Some(ref tm) = self.chain.tree_manager {
                let _ = tm.unregister_agent(&entry.agent_id, entry.pid, 0);
            }

            let _ = self
                .process_table
                .update_state(entry.pid, ProcessState::Exited(0));
        }

        // Persist chain to RVF checkpoint (primary), JSON as fallback
        #[cfg(feature = "exochain")]
        if let Some(ref cm) = self.chain.chain_manager {
            let chain_config = self.config.chain.clone().unwrap_or_default();
            if let Some(ref ckpt_path) = chain_config.effective_checkpoint_path() {
                let json_path = std::path::PathBuf::from(ckpt_path);
                let rvf_path = json_path.with_extension("rvf");

                // Save RVF format (primary)
                match cm.save_to_rvf(&rvf_path) {
                    Ok(()) => {
                        info!(path = %rvf_path.display(), "chain saved to RVF checkpoint");
                        // ADR-129: remember the cutover so a rewritten file is refused.
                        if let Err(e) = cm.record_cutover(&rvf_path) {
                            error!(error = %e, "failed to write the ML-DSA-65 cutover record");
                        }
                    }
                    Err(e) => {
                        error!(error = %e, "failed to save RVF checkpoint, falling back to JSON");
                        // Fallback: save JSON
                        match cm.save_to_file(&json_path) {
                            Ok(()) => {
                                info!(path = %json_path.display(), "chain saved to JSON checkpoint (fallback)")
                            }
                            Err(e2) => {
                                error!(error = %e2, "failed to save JSON checkpoint fallback")
                            }
                        }
                    }
                }

                // Save tree checkpoint alongside chain
                if let Some(ref tm) = self.chain.tree_manager {
                    let tree_path = json_path.with_extension("tree.json");
                    match tm.save_checkpoint(&tree_path) {
                        Ok(()) => {
                            info!(path = %tree_path.display(), "tree checkpoint saved");
                            cm.append(
                                "tree",
                                "tree.checkpoint",
                                Some(serde_json::json!({
                                    "path": tree_path.display().to_string(),
                                    "root_hash": tm.stats().root_hash,
                                })),
                            );
                        }
                        Err(e) => error!(error = %e, "failed to save tree checkpoint"),
                    }
                }
            }
        }

        self.state = KernelState::Halted;
        self.event_log.info("kernel", "halted");
        info!("kernel halted");
        Ok(())
    }

    /// Get the current kernel state.
    pub fn state(&self) -> &KernelState {
        &self.state
    }

    /// Get the kernel configuration.
    pub fn kernel_config(&self) -> &KernelConfig {
        &self.config
    }

    /// Get the process table.
    pub fn process_table(&self) -> &Arc<ProcessTable> {
        &self.process_table
    }

    /// Get the service registry.
    pub fn services(&self) -> &Arc<ServiceRegistry> {
        &self.service_registry
    }

    /// Get the IPC subsystem.
    pub fn ipc(&self) -> &Arc<KernelIpc> {
        &self.ipc
    }

    /// Get the message bus.
    pub fn bus(&self) -> &Arc<MessageBus> {
        &self.bus
    }

    /// Get the A2A router.
    #[cfg(feature = "native")]
    pub fn a2a_router(&self) -> &Arc<A2ARouter> {
        &self.a2a_router
    }

    /// Get the agent identity registry.
    ///
    /// External clients call `agent.register` to obtain an `agent_id`
    /// bound to an Ed25519 public key. The registry is then consulted
    /// when verifying signatures on `ipc.publish` and
    /// `ipc.subscribe_stream` requests.
    #[cfg(feature = "native")]
    pub fn agent_registry(&self) -> &crate::agent_registry::AgentRegistry {
        &self.agent_registry
    }

    /// Get the node identity registry.
    ///
    /// Maps node-ids to their ed25519 public keys. The daemon
    /// registers its own node here at boot; remote nodes (ESP32
    /// leaves, future kernel-class peers) register via a future
    /// `node.register` RPC. Consulted by the substrate publish gate
    /// to verify node signatures and by the Explorer's tree to
    /// resolve labels.
    #[cfg(feature = "native")]
    pub fn node_registry(&self) -> &crate::node_registry::NodeRegistry {
        &self.node_registry
    }

    /// Get the substrate RPC service.
    ///
    /// Backs the `substrate.read`, `substrate.publish`,
    /// `substrate.subscribe`, and `substrate.notify` RPCs.
    #[cfg(feature = "native")]
    pub fn substrate_service(&self) -> &crate::substrate_service::SubstrateService {
        &self.substrate_service
    }

    /// Get the cron service.
    #[cfg(feature = "native")]
    pub fn cron_service(&self) -> &Arc<crate::cron::CronService> {
        &self.cron_service
    }

    /// Get the assessment service.
    #[cfg(feature = "native")]
    pub fn assessment_service(&self) -> &Arc<crate::assessment::AssessmentService> {
        &self.assessment_service
    }

    /// Get the health system.
    pub fn health(&self) -> &HealthSystem {
        &self.health
    }

    /// Get the agent supervisor.
    pub fn supervisor(&self) -> &AgentSupervisor<P> {
        &self.supervisor
    }

    /// Get the boot log.
    pub fn boot_log(&self) -> &BootLog {
        &self.boot_log
    }

    /// Get the runtime event log (ring buffer).
    pub fn event_log(&self) -> &Arc<KernelEventLog> {
        &self.event_log
    }

    /// Get kernel uptime.
    pub fn uptime(&self) -> std::time::Duration {
        self.boot_time.elapsed()
    }

    /// Get the cluster membership tracker.
    pub fn cluster_membership(&self) -> &Arc<ClusterMembership> {
        &self.cluster_membership
    }

    /// Get the application manager (installed apps rehydrated from disk).
    pub fn app_manager(&self) -> &Arc<AppManager> {
        &self.app_manager
    }

    /// Get the host revocation list.
    pub fn revocation_list(&self) -> &Arc<crate::revocation::RevocationList> {
        &self.revocation_list
    }

    /// Get the local chain manager (when exochain feature is enabled).
    #[cfg(feature = "exochain")]
    pub fn chain_manager(&self) -> Option<&Arc<crate::chain::ChainManager>> {
        self.chain.chain_manager.as_ref()
    }

    /// The project governance runtime (profile `project` only).
    #[cfg(feature = "exochain")]
    pub fn governance_overlay(&self) -> Option<&Arc<crate::overlay_runtime::OverlayRuntime>> {
        self.chain.overlay.as_ref()
    }

    /// Get the tree manager (when exochain feature is enabled).
    #[cfg(feature = "exochain")]
    pub fn tree_manager(&self) -> Option<&Arc<crate::tree_manager::TreeManager>> {
        self.chain.tree_manager.as_ref()
    }

    /// Get the governance gate backend (if configured).
    #[cfg(feature = "exochain")]
    pub fn governance_gate(&self) -> Option<&Arc<dyn crate::gate::GateBackend>> {
        self.chain.governance_gate.as_ref()
    }

    /// Get the external chain-head anchoring controller (WEFT-137), when configured.
    #[cfg(feature = "exochain")]
    pub fn chain_anchor(&self) -> Option<&Arc<crate::chain_anchor::AnchoringController>> {
        self.chain.chain_anchor.as_ref()
    }

    /// Get the ECC HNSW service (if ecc feature enabled).
    #[cfg(feature = "ecc")]
    pub fn ecc_hnsw(&self) -> Option<&Arc<crate::hnsw_service::HnswService>> {
        self.ecc.hnsw.as_ref()
    }

    /// Get the ECC vector backend (if ecc feature enabled).
    ///
    /// Returns the configured vector search backend (HNSW, DiskANN,
    /// or Hybrid) based on the `[kernel.vector]` configuration.
    #[cfg(feature = "ecc")]
    pub fn ecc_vector_backend(&self) -> Option<&Arc<dyn crate::vector_backend::VectorBackend>> {
        self.ecc.vector_backend.as_ref()
    }

    /// Get the ECC spatial BVH service (if enabled via `[kernel.spatial]`).
    #[cfg(feature = "ecc")]
    pub fn ecc_spatial(&self) -> Option<&Arc<crate::spatial_service::SpatialService>> {
        self.ecc.spatial.as_ref()
    }

    /// Get the ECC causal graph (if ecc feature enabled).
    #[cfg(feature = "ecc")]
    pub fn ecc_causal(&self) -> Option<&Arc<crate::causal::CausalGraph>> {
        self.ecc.causal.as_ref()
    }

    /// Get the ECC cognitive tick (if ecc feature enabled).
    #[cfg(feature = "ecc")]
    pub fn ecc_tick(&self) -> Option<&Arc<crate::cognitive_tick::CognitiveTick>> {
        self.ecc.tick.as_ref()
    }

    /// Get the ECC calibration results (if ecc feature enabled).
    #[cfg(feature = "ecc")]
    pub fn ecc_calibration(&self) -> Option<&crate::calibration::EccCalibration> {
        self.ecc.calibration.as_ref()
    }

    /// Get the ECC cross-reference store (if ecc feature enabled).
    #[cfg(feature = "ecc")]
    pub fn ecc_crossrefs(&self) -> Option<&Arc<crate::crossref::CrossRefStore>> {
        self.ecc.crossrefs.as_ref()
    }

    /// Get the ECC impulse queue (if ecc feature enabled).
    #[cfg(feature = "ecc")]
    pub fn ecc_impulses(&self) -> Option<&Arc<crate::impulse::ImpulseQueue>> {
        self.ecc.impulses.as_ref()
    }

    /// Get the EML coherence model (if ecc feature enabled).
    #[cfg(feature = "ecc")]
    pub fn ecc_eml_coherence(
        &self,
    ) -> Option<&Arc<std::sync::Mutex<crate::eml_coherence::EmlCoherenceModel>>> {
        self.ecc.eml_coherence.as_ref()
    }

    /// Get the os-patterns metrics registry (if os-patterns feature enabled).
    #[cfg(feature = "os-patterns")]
    pub fn metrics_registry(&self) -> Option<&Arc<crate::metrics::MetricsRegistry>> {
        self.observability.metrics_registry.as_ref()
    }

    /// Get the os-patterns log service (if os-patterns feature enabled).
    #[cfg(feature = "os-patterns")]
    pub fn log_service(&self) -> Option<&Arc<crate::log_service::LogService>> {
        self.observability.log_service.as_ref()
    }

    /// Get the os-patterns timer service (if os-patterns feature enabled).
    #[cfg(feature = "os-patterns")]
    pub fn timer_service(&self) -> Option<&Arc<crate::timer::TimerService>> {
        self.observability.timer_service.as_ref()
    }

    /// Get the os-patterns dead letter queue (if os-patterns feature enabled).
    #[cfg(feature = "os-patterns")]
    pub fn dead_letter_queue(&self) -> Option<&Arc<crate::dead_letter::DeadLetterQueue>> {
        self.observability.dead_letter_queue.as_ref()
    }

    /// Take ownership of the AppContext for agent loop consumption.
    ///
    /// This is a one-shot operation: after calling this, the kernel
    /// no longer holds the AppContext. Use this before calling
    /// `AppContext::into_agent_loop()`.
    pub fn take_app_context(&mut self) -> Option<AppContext<P>> {
        self.app_context.take()
    }
}

/// Boot refusal text for an existing chain file that failed to restore.
///
/// A chain that fails verification (corruption, or events written by a
/// newer binary, e.g. carrying a `rule_hash` this build cannot verify) must
/// never be replaced by a fresh genesis: the next checkpoint save would
/// overwrite the only copy. The operator moves it aside explicitly
/// (`--new-chain` picks a different location and does not touch this file).
#[cfg(feature = "exochain")]
fn unrestorable_chain_msg(path: &std::path::Path, cause: &str) -> String {
    format!(
        "chain at {} failed to restore ({cause}); refusing to start a fresh chain over it. \
         Move or rename {} (and its .rvf/.json sibling) aside, then restart; \
         if a newer binary wrote it, upgrade",
        path.display(),
        path.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use clawft_platform::NativePlatform;
    use clawft_types::config::{AgentDefaults, AgentsConfig};

    fn test_config() -> Config {
        Config {
            agents: AgentsConfig {
                defaults: AgentDefaults {
                    workspace: "~/.clawft/workspace".into(),
                    model: "test/model".into(),
                    max_tokens: 1024,
                    temperature: 0.5,
                    max_tool_iterations: 5,
                    memory_window: 10,
                },
                ..AgentsConfig::default()
            },
            ..Config::default()
        }
    }

    fn test_kernel_config() -> KernelConfig {
        KernelConfig {
            governance: Default::default(),
            profile: None,
            shared_services: Default::default(),
            enabled: true,
            max_processes: 16,
            health_check_interval_secs: 5,
            cluster: None,
            chain: None,
            resource_tree: None,
            vector: None,
            spatial: None,
            profiles: None,
            pairing: None,
            mesh: None,
            anchor: None,
            ipc_tcp: None,
            llm: None,
            agent: None,
            brand: ::clawft_types::config::DEFAULT_BRAND.to_string(),
        }
    }

    /// WEFT-119: mesh.enabled registers `mesh` SystemService and starts it.
    #[cfg(all(feature = "native", feature = "mesh"))]
    #[tokio::test]
    async fn boot_registers_mesh_system_service() {
        use clawft_types::config::MeshConfig;
        use crate::health::HealthStatus;

        let platform = Arc::new(NativePlatform::new());
        let mut kconfig = test_kernel_config();
        kconfig.mesh = Some(MeshConfig {
            enabled: true,
            transport: "tcp".into(),
            // Ephemeral port so parallel tests do not clash.
            listen_addr: "127.0.0.1:0".into(),
            discovery: false,
            seed_peers: vec![],
            noise: false,
            noise_key_path: None,
            ..MeshConfig::default()
        });

        let mut kernel = Kernel::boot(test_config(), kconfig, platform)
            .await
            .expect("boot with mesh");

        let mesh_svc = kernel
            .services()
            .get("mesh")
            .expect("mesh SystemService should be registered when mesh.enabled");
        assert_eq!(mesh_svc.name(), "mesh");
        assert_eq!(
            mesh_svc.health_check().await,
            HealthStatus::Healthy,
            "start_all should have started mesh"
        );

        kernel.shutdown().await.unwrap();
        // After stop_all, mesh reports not started.
        assert!(
            matches!(
                mesh_svc.health_check().await,
                HealthStatus::Unhealthy(_)
            ),
            "mesh should be unhealthy after shutdown stop"
        );
    }

    /// ADR-103 D2: enabled mesh + unbindable listener fails boot, naming the address.
    #[cfg(all(feature = "native", feature = "mesh"))]
    #[tokio::test]
    async fn boot_fails_when_mesh_listener_cannot_bind() {
        use clawft_types::config::MeshConfig;

        let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = taken.local_addr().unwrap().to_string();

        let mut kconfig = test_kernel_config();
        kconfig.mesh = Some(MeshConfig {
            enabled: true,
            listen_addr: addr.clone(),
            ..MeshConfig::default()
        });
        let err = Kernel::boot(test_config(), kconfig, Arc::new(NativePlatform::new()))
            .await
            .err()
            .expect("boot must fail when the mesh port is taken");
        let msg = err.to_string();
        assert!(msg.contains(&addr), "error should name the address: {msg}");
        assert!(msg.contains("another kernel"), "error should name the cause: {msg}");
    }

    /// A wildcard leaf bind without `leaf_listen_any` fails boot, naming the key.
    #[cfg(all(feature = "native", feature = "mesh"))]
    #[tokio::test]
    async fn boot_refuses_wildcard_leaf_bind_without_opt_in() {
        use clawft_types::config::MeshConfig;

        let mut kconfig = test_kernel_config();
        kconfig.mesh = Some(MeshConfig {
            enabled: true,
            listen_addr: "127.0.0.1:0".into(),
            leaf_listen_addr: Some("0.0.0.0".into()),
            ..MeshConfig::default()
        });
        let err = Kernel::boot(test_config(), kconfig, Arc::new(NativePlatform::new()))
            .await
            .err()
            .expect("wildcard leaf bind must be refused");
        assert!(err.to_string().contains("leaf_listen_any"), "{err}");
    }

    /// A list entry that is a wildcard, or a repeated entry, fails boot.
    #[cfg(all(feature = "native", feature = "mesh"))]
    #[tokio::test]
    async fn boot_refuses_bad_leaf_bind_list_entries() {
        use clawft_types::config::{LeafListenAddr, MeshConfig};

        for (list, needle) in [
            (vec!["192.0.2.10", "0.0.0.0"], "leaf_listen_any"),
            (vec!["192.0.2.10", "192.0.2.10"], "twice"),
            (vec![], "empty"),
        ] {
            let mut kconfig = test_kernel_config();
            kconfig.mesh = Some(MeshConfig {
                enabled: true,
                listen_addr: "127.0.0.1:0".into(),
                leaf_listen_addr: Some(LeafListenAddr::Many(list.iter().map(|s| s.to_string()).collect())),
                ..MeshConfig::default()
            });
            let err = Kernel::boot(test_config(), kconfig, Arc::new(NativePlatform::new()))
                .await
                .err()
                .expect("bad leaf bind list must be refused");
            assert!(err.to_string().contains(needle), "{err}");
        }
    }

    /// P3-U: a symlinked or loose `~/.weftos/user.key` is refused as the chain
    /// signing key instead of being followed.
    #[cfg(all(feature = "native", feature = "exochain", unix))]
    #[tokio::test]
    async fn user_key_symlink_or_loose_mode_is_refused_at_boot() {
        use clawft_types::config::ChainConfig;
        use std::os::unix::fs::PermissionsExt;

        let home = tempfile::tempdir().unwrap();
        let weftos = home.path().join(".weftos");
        let chain_dir = weftos.join("chain");
        std::fs::create_dir_all(&chain_dir).unwrap();
        let mk = |ckpt: &std::path::Path| {
            let mut kc = test_kernel_config();
            kc.chain = Some(ChainConfig {
                enabled: true,
                checkpoint_path: Some(ckpt.display().to_string()),
                ..ChainConfig::default()
            });
            kc
        };
        let ckpt = chain_dir.join("chain.json");

        // Loose permissions.
        let user_key = weftos.join("user.key");
        std::fs::write(&user_key, [3u8; 32]).unwrap();
        std::fs::set_permissions(&user_key, std::fs::Permissions::from_mode(0o644)).unwrap();
        let err = Kernel::boot(test_config(), mk(&ckpt), Arc::new(NativePlatform::new()))
            .await
            .err()
            .expect("loose user.key must refuse boot");
        assert!(err.to_string().contains("user.key"), "{err}");

        // A symlink, even to a private file.
        std::fs::remove_file(&user_key).unwrap();
        let target = home.path().join("elsewhere.key");
        std::fs::write(&target, [3u8; 32]).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::os::unix::fs::symlink(&target, &user_key).unwrap();
        let err = Kernel::boot(test_config(), mk(&ckpt), Arc::new(NativePlatform::new()))
            .await
            .err()
            .expect("symlinked user.key must refuse boot");
        assert!(err.to_string().contains("user.key"), "{err}");
    }

    /// P3-U: service mode takes the service's node id, binds no listener (the
    /// taken port is not touched) and starts no mesh runtime.
    #[cfg(all(feature = "native", feature = "mesh"))]
    #[tokio::test]
    async fn service_mode_uses_the_service_node_id_and_binds_nothing() {
        use clawft_types::config::MeshConfig;

        let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = taken.local_addr().unwrap().to_string();
        let service_id = crate::node_id::node_id_from_pubkey(&[42u8; 32]);

        let mut kconfig = test_kernel_config();
        kconfig.mesh = Some(MeshConfig {
            enabled: true,
            listen_addr: addr,
            ..MeshConfig::default()
        });
        let mut kernel = Kernel::boot_in_service_mode(
            test_config(),
            kconfig,
            Arc::new(NativePlatform::new()),
            service_id.clone(),
        )
        .await
        .expect("service mode must not try to bind the taken port");
        assert_eq!(kernel.cluster_membership().local_node_id(), service_id);
        assert!(kernel.services().get("mesh").is_none(), "no in-kernel mesh service");
        kernel.shutdown().await.unwrap();
    }

    /// P3-U: a malformed service node id fails the boot; there is no
    /// ephemeral-identity fallback.
    #[cfg(all(feature = "native", feature = "mesh"))]
    #[tokio::test]
    async fn service_mode_refuses_a_malformed_node_id() {
        let err = Kernel::boot_in_service_mode(
            test_config(),
            test_kernel_config(),
            Arc::new(NativePlatform::new()),
            "nope".into(),
        )
        .await
        .err()
        .expect("must fail");
        assert!(err.to_string().contains("node id"), "{err}");
    }

    /// P3-U (R2): `kernel.mesh.service = required` fails any boot path that
    /// did not link a service, instead of running its own listener.
    #[cfg(all(feature = "native", feature = "mesh"))]
    #[tokio::test]
    async fn required_service_fails_a_boot_without_a_link() {
        use clawft_types::config::{MeshConfig, MeshServicePolicy};

        let mut kconfig = test_kernel_config();
        kconfig.mesh = Some(MeshConfig {
            enabled: true,
            listen_addr: "127.0.0.1:0".into(),
            service: MeshServicePolicy::Required,
            ..MeshConfig::default()
        });
        let err = Kernel::boot(test_config(), kconfig, Arc::new(NativePlatform::new()))
            .await
            .err()
            .expect("required without a service link must fail");
        assert!(err.to_string().contains("required"), "{err}");
    }

    /// Mesh off means no bind and no error, even if the address is taken.
    #[cfg(all(feature = "native", feature = "mesh"))]
    #[tokio::test]
    async fn boot_with_mesh_disabled_never_binds() {
        use clawft_types::config::MeshConfig;

        let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let mut kconfig = test_kernel_config();
        kconfig.mesh = Some(MeshConfig {
            enabled: false,
            listen_addr: taken.local_addr().unwrap().to_string(),
            ..MeshConfig::default()
        });
        let mut kernel = Kernel::boot(test_config(), kconfig, Arc::new(NativePlatform::new()))
            .await
            .expect("mesh-off boot must not touch the listener");
        kernel.shutdown().await.unwrap();
    }

    /// The node id comes from the node key: same key, same id across boots.
    #[cfg(all(feature = "native", feature = "mesh"))]
    #[tokio::test]
    async fn node_id_derives_from_node_key_and_survives_reboot() {
        let seed = [9u8; 32];
        let expected = crate::node_id::node_id_from_pubkey(
            &ed25519_dalek::SigningKey::from_bytes(&seed)
                .verifying_key()
                .to_bytes(),
        );
        for _ in 0..2 {
            let mut kernel = Kernel::boot_with_node_key(
                test_config(),
                test_kernel_config(),
                Arc::new(NativePlatform::new()),
                Some(seed),
            )
            .await
            .expect("boot with node key");
            assert_eq!(kernel.cluster_membership().local_node_id(), expected);
            kernel.shutdown().await.unwrap();
        }
    }

    /// The mesh runtime id (handshake) equals the cluster membership id.
    #[cfg(all(feature = "native", feature = "mesh"))]
    #[tokio::test]
    async fn mesh_node_id_equals_cluster_node_id() {
        use clawft_types::config::MeshConfig;

        let mut kconfig = test_kernel_config();
        kconfig.mesh = Some(MeshConfig {
            enabled: true,
            listen_addr: "127.0.0.1:0".into(),
            ..MeshConfig::default()
        });
        let mut kernel = Kernel::boot_with_node_key(
            test_config(),
            kconfig,
            Arc::new(NativePlatform::new()),
            Some([3u8; 32]),
        )
        .await
        .expect("boot with mesh");
        let mesh_id = kernel
            .a2a_router()
            .mesh_runtime()
            .expect("mesh runtime attached")
            .node_id()
            .to_owned();
        assert_eq!(mesh_id, kernel.cluster_membership().local_node_id());
        assert!(crate::node_id::is_node_id(&mesh_id));
        kernel.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn boot_and_shutdown() {
        let platform = Arc::new(NativePlatform::new());
        let mut kernel = Kernel::boot(test_config(), test_kernel_config(), platform)
            .await
            .unwrap();

        assert_eq!(*kernel.state(), KernelState::Running);
        // Uptime should be non-negative (boot_time is set before boot completes)
        let _uptime = kernel.uptime();

        // Kernel process should be PID 0
        let kernel_proc = kernel.process_table().get(0).unwrap();
        assert_eq!(kernel_proc.agent_id, "kernel");
        assert_eq!(kernel_proc.state, ProcessState::Running);

        kernel.shutdown().await.unwrap();
        assert_eq!(*kernel.state(), KernelState::Halted);
    }

    #[tokio::test]
    async fn double_shutdown_fails() {
        let platform = Arc::new(NativePlatform::new());
        let mut kernel = Kernel::boot(test_config(), test_kernel_config(), platform)
            .await
            .unwrap();

        kernel.shutdown().await.unwrap();
        let result = kernel.shutdown().await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn boot_log_has_events() {
        let platform = Arc::new(NativePlatform::new());
        let kernel = Kernel::boot(test_config(), test_kernel_config(), platform)
            .await
            .unwrap();

        let log = kernel.boot_log();
        assert!(!log.is_empty());

        let formatted = log.format_all();
        assert!(formatted.contains(&format!("WeftOS v{}", env!("CARGO_PKG_VERSION"))));
        assert!(formatted.contains("Boot complete"));
    }

    #[tokio::test]
    async fn process_table_accessible() {
        let platform = Arc::new(NativePlatform::new());
        let kernel = Kernel::boot(test_config(), test_kernel_config(), platform)
            .await
            .unwrap();

        let pt = kernel.process_table();
        assert_eq!(pt.len(), 1); // kernel process only
        assert_eq!(pt.max_processes(), 16);
    }

    #[tokio::test]
    async fn services_accessible() {
        let platform = Arc::new(NativePlatform::new());
        let kernel = Kernel::boot(test_config(), test_kernel_config(), platform)
            .await
            .unwrap();

        // Base: cron + containers; +cluster; +hnsw +cognitive_tick (ecc); +assess (native)
        let count = kernel.services().len();
        #[cfg(all(feature = "ecc", feature = "cluster"))]
        assert_eq!(
            count, 6,
            "expected cron+containers+assess+cluster+hnsw+cognitive_tick"
        );
        #[cfg(all(feature = "ecc", not(feature = "cluster")))]
        assert_eq!(
            count, 5,
            "expected cron+containers+assess+hnsw+cognitive_tick"
        );
        #[cfg(all(not(feature = "ecc"), feature = "cluster"))]
        assert_eq!(count, 4, "expected cron+containers+assess+cluster");
        #[cfg(all(not(feature = "ecc"), not(feature = "cluster")))]
        assert_eq!(count, 3, "expected cron+containers+assess");
    }

    #[tokio::test]
    async fn take_app_context() {
        let platform = Arc::new(NativePlatform::new());
        let mut kernel = Kernel::boot(test_config(), test_kernel_config(), platform)
            .await
            .unwrap();

        let ctx = kernel.take_app_context();
        assert!(ctx.is_some());

        // Second take returns None
        let ctx2 = kernel.take_app_context();
        assert!(ctx2.is_none());
    }

    #[tokio::test]
    async fn ipc_accessible() {
        let platform = Arc::new(NativePlatform::new());
        let kernel = Kernel::boot(test_config(), test_kernel_config(), platform)
            .await
            .unwrap();

        let ipc = kernel.ipc();
        assert!(Arc::ptr_eq(ipc.bus(), kernel.bus()));
    }

    #[test]
    fn kernel_state_display() {
        assert_eq!(KernelState::Booting.to_string(), "booting");
        assert_eq!(KernelState::Running.to_string(), "running");
        assert_eq!(KernelState::ShuttingDown.to_string(), "shutting_down");
        assert_eq!(KernelState::Halted.to_string(), "halted");
    }

    // ── Full-stack integration helpers ─────────────────────────────

    /// Kernel config with exochain + resource tree enabled. No explicit
    /// checkpoint path: `chain_storage` pins a fresh temp dir per boot in
    /// unit tests, so the operator chain is never touched.
    #[cfg(all(feature = "exochain", feature = "ecc", feature = "wasm-sandbox"))]
    fn test_kernel_config_full_stack() -> KernelConfig {
        use clawft_types::config::{ChainConfig, ResourceTreeConfig};
        KernelConfig {
            enabled: true,
            max_processes: 32,
            health_check_interval_secs: 5,
            cluster: None,
            chain: Some(ChainConfig {
                enabled: true,
                checkpoint_interval: 10_000,
                chain_id: 0,
                checkpoint_path: None,
                external_anchor: None,
            }),
            resource_tree: Some(ResourceTreeConfig {
                enabled: true,
                checkpoint_path: None,
            }),
            vector: None,
            spatial: None,
            profiles: None,
            pairing: None,
            mesh: None,
            anchor: None,
            ipc_tcp: None,
            llm: None,
            agent: None,
            brand: ::clawft_types::config::DEFAULT_BRAND.to_string(),
            ..KernelConfig::default()
        }
    }

    // ── Integration: full-stack kernel ──────────────────────────────

    /// K0-K5 integration: native agent + WASM tool + container service + ECC
    /// all connected in a single kernel with chain witnessing.
    #[tokio::test]
    #[cfg(all(feature = "exochain", feature = "ecc", feature = "wasm-sandbox"))]
    async fn integration_full_stack_kernel() {
        let platform = Arc::new(NativePlatform::new());
        let kernel = Kernel::boot(test_config(), test_kernel_config_full_stack(), platform)
            .await
            .unwrap();

        // ── K0: Kernel is running ──────────────────────────────────
        assert_eq!(*kernel.state(), KernelState::Running);

        // ── K0: Services registered at boot ────────────────────────
        // cron + container + hnsw + cognitive_tick = 4 with ecc
        let svc_count = kernel.services().len();
        assert!(svc_count >= 4, "expected >= 4 services, got {svc_count}");

        // ── K1: Spawn a native agent ───────────────────────────────
        let spawn_result = kernel.supervisor().spawn(crate::supervisor::SpawnRequest {
            agent_id: "integration-agent-1".into(),
            capabilities: None,
            parent_pid: None,
            env: std::collections::HashMap::new(),
            backend: None, // defaults to Native
        });
        assert!(
            spawn_result.is_ok(),
            "native agent spawn failed: {:?}",
            spawn_result.err()
        );
        let agent_pid = spawn_result.unwrap().pid;
        assert!(agent_pid > 0, "agent should get PID > 0");

        // ── K1: Agent appears in process table ─────────────────────
        let processes = kernel.process_table().list();
        assert!(
            processes.iter().any(|p| p.pid == agent_pid),
            "agent not in process table"
        );

        // ── K2: A2A messaging between kernel and agent ─────────────
        let a2a = kernel.a2a_router();
        let _inbox = a2a.create_inbox(agent_pid);

        // Send a message from kernel (PID 0) to agent
        let msg = crate::ipc::KernelMessage::new(
            0, // from kernel
            crate::ipc::MessageTarget::Process(agent_pid),
            crate::ipc::MessagePayload::Json(serde_json::json!({"cmd": "ping"})),
        );
        let send_result = a2a.send(msg).await;
        // This should succeed: PID 0 exists, agent inbox exists
        assert!(
            send_result.is_ok(),
            "A2A send failed: {:?}",
            send_result.err()
        );

        // ── K3: WASM tool execution ────────────────────────────────
        let wasm_config = crate::wasm_runner::WasmSandboxConfig::default();
        let wasm_runner = crate::wasm_runner::WasmToolRunner::new(wasm_config);

        // Minimal WAT module that exports _start and immediately returns
        let noop_wat = r#"(module (func (export "_start")))"#;
        let result = wasm_runner
            .execute_bytes(
                "integration-tool",
                noop_wat.as_bytes(),
                serde_json::json!({}),
            )
            .await;
        assert!(result.is_ok(), "WASM execution failed: {:?}", result.err());
        let wasm_result = result.unwrap();
        assert_eq!(wasm_result.exit_code, 0, "WASM tool should exit cleanly");
        assert!(wasm_result.fuel_consumed > 0, "WASM should consume fuel");

        // ── K3: WASM tool in registry ──────────────────────────────
        let wasm_runner_arc = Arc::new(crate::wasm_runner::WasmToolRunner::new(
            crate::wasm_runner::WasmSandboxConfig::default(),
        ));
        let mut registry = crate::wasm_runner::ToolRegistry::new();
        registry
            .register_wasm_tool(
                "demo-tool",
                "A demo WASM tool for integration testing",
                noop_wat.as_bytes().to_vec(),
                wasm_runner_arc,
            )
            .unwrap();
        assert!(
            registry.get("demo-tool").is_some(),
            "WASM tool should be in registry"
        );
        let tool_list = registry.list();
        assert!(
            tool_list.contains(&"demo-tool".to_string()),
            "WASM tool should appear in listing"
        );

        // ── K4: Container service registered ───────────────────────
        let svc_list = kernel.services().list();
        assert!(
            svc_list.iter().any(|(name, _)| name == "containers"),
            "container service should be registered"
        );

        // ── K3c: ECC cognitive substrate active ────────────────────
        let ecc_hnsw = kernel.ecc_hnsw().expect("HNSW service should be present");
        // After calibration the store is cleared, so it should be empty
        assert_eq!(
            ecc_hnsw.len(),
            0,
            "HNSW should be empty after calibration cleanup"
        );

        let ecc_causal = kernel.ecc_causal().expect("causal graph should be present");
        assert_eq!(
            ecc_causal.node_count(),
            0,
            "causal graph should be empty after cleanup"
        );

        let _ecc_tick = kernel.ecc_tick().expect("cognitive tick should be present");

        let calibration = kernel.ecc_calibration().expect("calibration should exist");
        assert!(
            calibration.compute_p95_us > 0,
            "calibration should have run"
        );
        assert!(
            calibration.tick_interval_ms > 0,
            "tick interval should be auto-calibrated"
        );

        // ── ECC: Insert a vector and search ────────────────────────
        ecc_hnsw.insert(
            "test-doc".into(),
            vec![1.0, 0.0, 0.0, 0.0],
            serde_json::json!({"text": "hello"}),
        );
        assert_eq!(ecc_hnsw.len(), 1);
        let results = ecc_hnsw.search(&[1.0, 0.0, 0.0, 0.0], 5);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "test-doc");

        // ── ECC: Causal graph operations ───────────────────────────
        let node1 = ecc_causal.add_node("concept-A".into(), serde_json::json!({}));
        let node2 = ecc_causal.add_node("concept-B".into(), serde_json::json!({}));
        ecc_causal.link(
            node1,
            node2,
            crate::causal::CausalEdgeType::Causes,
            1.0,
            0,
            0,
        );
        assert_eq!(ecc_causal.node_count(), 2);
        assert_eq!(ecc_causal.edge_count(), 1);
        let path = ecc_causal.find_path(node1, node2, 5);
        assert!(path.is_some(), "should find path between linked nodes");

        // ── ExoChain: Chain has witnessed boot events ──────────────
        let chain = kernel.chain_manager().expect("chain should exist");
        assert!(
            chain.sequence() > 5,
            "chain should have boot events, got seq={}",
            chain.sequence()
        );

        // Verify chain integrity
        let verify_result = chain.verify_integrity();
        assert!(
            verify_result.valid,
            "chain should be valid: {:?}",
            verify_result.errors
        );
        assert!(verify_result.errors.is_empty(), "no integrity errors");

        // ── Governance: genesis rules anchored to chain ────────────
        let _governance = kernel
            .governance_gate()
            .expect("governance gate should exist");
        let all_events = chain.tail(0);
        let genesis_events: Vec<_> = all_events
            .iter()
            .filter(|e| e.kind == "governance.genesis")
            .collect();
        assert!(
            !genesis_events.is_empty(),
            "governance genesis should be on chain"
        );

        // Verify the genesis payload contains the correct rule count
        // Find the kernel's own genesis event (version 2.0.0)
        let genesis_payload = genesis_events
            .iter()
            .filter_map(|e| e.payload.as_ref())
            .find(|p| p.get("version").and_then(|v| v.as_str()) == Some("2.0.0"))
            .expect("should find v2.0.0 governance genesis on chain");
        assert_eq!(
            genesis_payload["rule_count"].as_u64().unwrap(),
            26,
            "genesis should contain 26 rules (22 constitutional/SOP + 3 browser_policy + 1 binding_thread)"
        );
        assert_eq!(
            genesis_payload["version"].as_str().unwrap(),
            "2.0.0",
            "genesis version should be 2.0.0"
        );

        // Each rule should be individually anchored (at least 26)
        let rule_events: Vec<_> = all_events
            .iter()
            .filter(|e| e.kind == "governance.rule")
            .collect();
        assert!(
            rule_events.len() >= 26,
            "at least 26 genesis rules should be individually anchored, got {}",
            rule_events.len(),
        );

        // Verify all rule IDs are present (GOV + SOP + BP + binding_thread)
        let rule_ids: Vec<&str> = rule_events
            .iter()
            .filter_map(|e| e.payload.as_ref()?.get("rule_id")?.as_str())
            .collect();
        for expected_id in &[
            "GOV-001", "GOV-002", "GOV-003", "GOV-004", "GOV-005", "GOV-006", "GOV-007",
            "SOP-L001", "SOP-L002", "SOP-L003", "SOP-L004", "SOP-L005", "SOP-L006", "SOP-E001",
            "SOP-E002", "SOP-E003", "SOP-E004", "SOP-E005", "SOP-J001", "SOP-J002", "SOP-J003",
            "SOP-J004", "BP-001", "BP-002", "BP-003", "soul.binding_thread_intact",
        ] {
            assert!(
                rule_ids.contains(expected_id),
                "{expected_id} should be anchored on chain"
            );
        }

        // ── ExoChain: ECC calibration event logged ─────────────────
        let ecc_events: Vec<_> = all_events
            .iter()
            .filter(|e| e.kind.starts_with("ecc."))
            .collect();
        assert!(
            !ecc_events.is_empty(),
            "ECC boot calibration should be logged to chain"
        );

        // ── Resource Tree: namespaces exist ────────────────────────
        let tree = kernel.tree_manager().expect("tree should exist");
        let stats = tree.stats();
        assert!(
            stats.node_count > 20,
            "tree should have many nodes (got {})",
            stats.node_count
        );

        // ── K2: ServiceApi + adapters compile ──────────────────────
        // (validated by service::tests; here we just confirm registry is queryable)
        assert!(
            kernel.services().get("cron").is_some(),
            "cron service should be accessible"
        );
        assert!(
            kernel.services().get("containers").is_some(),
            "container service should be accessible"
        );

        // ── Graceful shutdown ──────────────────────────────────────
        let mut kernel = kernel;
        kernel.shutdown().await.unwrap();
        assert_eq!(*kernel.state(), KernelState::Halted);
    }

    // ── Integration: cross-backend tools ───────────────────────────

    /// Demonstrates that native (built-in) tools and WASM tools coexist
    /// in a single ToolRegistry and can be dispatched through the same
    /// lookup interface.
    #[test]
    #[cfg(feature = "wasm-sandbox")]
    fn integration_cross_backend_tools() {
        use crate::governance::EffectVector;
        use crate::wasm_runner::{
            BuiltinTool, BuiltinToolSpec, ToolCategory, ToolError, ToolRegistry, WasmSandboxConfig,
            WasmToolRunner,
        };

        // ── A native (Rust) tool ───────────────────────────────────
        struct EchoTool;

        impl BuiltinTool for EchoTool {
            fn name(&self) -> &str {
                "native.echo"
            }
            fn spec(&self) -> &BuiltinToolSpec {
                // Leak a static spec for test simplicity
                static SPEC: std::sync::OnceLock<BuiltinToolSpec> = std::sync::OnceLock::new();
                SPEC.get_or_init(|| BuiltinToolSpec {
                    name: "native.echo".into(),
                    category: ToolCategory::System,
                    description: "Echoes input back".into(),
                    parameters: serde_json::json!({}),
                    gate_action: "tool.native.echo".into(),
                    effect: EffectVector::default(),
                    native: true,
                })
            }
            fn execute(&self, args: serde_json::Value) -> Result<serde_json::Value, ToolError> {
                Ok(serde_json::json!({"echo": args}))
            }
        }

        // ── Build a mixed registry ─────────────────────────────────
        let mut registry = ToolRegistry::new();

        // Register native tool
        registry.register(Arc::new(EchoTool));

        // Register WASM tool
        let runner = Arc::new(WasmToolRunner::new(WasmSandboxConfig::default()));
        let noop_wat = r#"(module (func (export "_start")))"#;
        registry
            .register_wasm_tool(
                "wasm.noop",
                "A no-op WASM tool",
                noop_wat.as_bytes().to_vec(),
                runner,
            )
            .unwrap();

        // ── Both tools accessible through one registry ─────────────
        assert_eq!(registry.list().len(), 2);

        let native = registry
            .get("native.echo")
            .expect("native tool should exist");
        assert!(native.spec().native, "native tool should be marked native");

        let wasm = registry.get("wasm.noop").expect("wasm tool should exist");
        assert!(!wasm.spec().native, "wasm tool should NOT be marked native");

        // ── Native tool executes synchronously ─────────────────────
        let result = native
            .execute(serde_json::json!({"hello": "world"}))
            .unwrap();
        assert_eq!(result["echo"]["hello"], "world");

        // ── Hierarchical registry ──────────────────────────────────
        // Child registry overlays parent; WASM tools from parent visible
        let parent = Arc::new(registry);
        let mut child = ToolRegistry::with_parent(parent);

        // Child sees parent tools
        assert!(
            child.get("native.echo").is_some(),
            "child sees parent native tool"
        );
        assert!(
            child.get("wasm.noop").is_some(),
            "child sees parent wasm tool"
        );

        // Child can shadow
        struct OverrideTool;
        impl BuiltinTool for OverrideTool {
            fn name(&self) -> &str {
                "native.echo"
            }
            fn spec(&self) -> &BuiltinToolSpec {
                static SPEC: std::sync::OnceLock<BuiltinToolSpec> = std::sync::OnceLock::new();
                SPEC.get_or_init(|| BuiltinToolSpec {
                    name: "native.echo".into(),
                    category: ToolCategory::System,
                    description: "Overridden echo".into(),
                    parameters: serde_json::json!({}),
                    gate_action: "tool.native.echo".into(),
                    effect: EffectVector::default(),
                    native: true,
                })
            }
            fn execute(&self, _args: serde_json::Value) -> Result<serde_json::Value, ToolError> {
                Ok(serde_json::json!({"overridden": true}))
            }
        }

        child.register(Arc::new(OverrideTool));
        let result = child
            .get("native.echo")
            .unwrap()
            .execute(serde_json::json!({}))
            .unwrap();
        assert_eq!(
            result["overridden"], true,
            "child should shadow parent tool"
        );

        // Parent WASM tool still reachable from child
        assert!(
            child.get("wasm.noop").is_some(),
            "WASM tool still reachable via parent"
        );
    }

    // ── Boot path coverage tests ─────────────────────────────────

    #[tokio::test]
    async fn boot_reaches_running_state() {
        let platform = Arc::new(NativePlatform::new());
        let kernel = Kernel::boot(test_config(), test_kernel_config(), platform)
            .await
            .unwrap();
        assert_eq!(*kernel.state(), KernelState::Running);
    }

    #[tokio::test]
    async fn boot_registers_cron_service() {
        let platform = Arc::new(NativePlatform::new());
        let kernel = Kernel::boot(test_config(), test_kernel_config(), platform)
            .await
            .unwrap();
        assert!(
            kernel.services().get("cron").is_some(),
            "cron service must be registered at boot"
        );
    }

    #[tokio::test]
    async fn boot_registers_container_service() {
        let platform = Arc::new(NativePlatform::new());
        let kernel = Kernel::boot(test_config(), test_kernel_config(), platform)
            .await
            .unwrap();
        assert!(
            kernel.services().get("containers").is_some(),
            "container service must be registered at boot"
        );
    }

    #[cfg(feature = "exochain")]
    fn test_kernel_config_exochain() -> KernelConfig {
        use clawft_types::config::{ChainConfig, ResourceTreeConfig};
        KernelConfig {
            governance: Default::default(),
            profile: None,
            shared_services: Default::default(),
            enabled: true,
            max_processes: 16,
            health_check_interval_secs: 5,
            cluster: None,
            chain: Some(ChainConfig {
                enabled: true,
                checkpoint_interval: 10_000,
                chain_id: 0,
                checkpoint_path: None,
                external_anchor: None,
            }),
            resource_tree: Some(ResourceTreeConfig {
                enabled: true,
                checkpoint_path: None,
            }),
            vector: None,
            spatial: None,
            profiles: None,
            pairing: None,
            mesh: None,
            anchor: None,
            ipc_tcp: None,
            llm: None,
            agent: None,
            brand: ::clawft_types::config::DEFAULT_BRAND.to_string(),
        }
    }

    /// A second kernel on a chain another kernel holds is refused at boot.
    #[cfg(all(feature = "exochain", unix))]
    #[tokio::test]
    async fn boot_refuses_a_chain_held_by_another_kernel() {
        let dir = tempfile::tempdir().unwrap();
        let ckpt = dir.path().join("chain.json");
        let _holder = crate::chain_storage::ChainLock::acquire(&ckpt).unwrap();
        let mut kc = test_kernel_config_exochain();
        kc.chain.as_mut().unwrap().checkpoint_path = Some(ckpt.to_string_lossy().into_owned());
        let platform = Arc::new(NativePlatform::new());
        let err = Kernel::boot(test_config(), kc, platform)
            .await
            .err()
            .expect("boot must refuse a held chain");
        let msg = err.to_string();
        assert!(msg.contains("in use by another kernel"), "{msg}");
        assert!(msg.contains("--new-chain"), "{msg}");
    }

    /// A chain file that fails verification (corruption, or events a newer
    /// binary wrote that this build cannot verify) is never replaced by a
    /// fresh genesis: boot refuses and the file is left byte-identical.
    #[cfg(all(feature = "exochain", unix))]
    #[tokio::test]
    async fn boot_refuses_an_unrestorable_chain_and_keeps_the_file() {
        for rvf in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let ckpt = dir.path().join("chain.json");
            let chain_file = if rvf { ckpt.with_extension("rvf") } else { ckpt.clone() };
            let cm = crate::chain::ChainManager::new(0, 0);
            cm.append("s", "k", Some(serde_json::json!({"a": 1})));
            if rvf {
                cm.save_to_rvf(&chain_file).unwrap();
            } else {
                cm.save_to_file(&chain_file).unwrap();
            }
            let mut bytes = std::fs::read(&chain_file).unwrap();
            let mid = bytes.len() / 2;
            bytes[mid] ^= 0xFF; // corrupt
            std::fs::write(&chain_file, &bytes).unwrap();

            let mut kc = test_kernel_config_exochain();
            kc.chain.as_mut().unwrap().checkpoint_path =
                Some(ckpt.to_string_lossy().into_owned());
            let platform = Arc::new(NativePlatform::new());
            let err = Kernel::boot(test_config(), kc, platform)
                .await
                .err()
                .expect("boot must refuse an unrestorable chain");
            let msg = err.to_string();
            assert!(msg.contains("refusing to start a fresh chain"), "rvf={rvf}: {msg}");
            assert!(
                msg.contains("Move or rename") && msg.contains(".rvf/.json sibling") && msg.contains("upgrade"),
                "rvf={rvf}: {msg}"
            );
            assert!(!msg.contains("--new-chain"), "rvf={rvf}: {msg}");
            assert_eq!(std::fs::read(&chain_file).unwrap(), bytes, "rvf={rvf}: file changed");
        }
    }

    /// ADR-129: a node whose chain never cut over boots exactly as before,
    /// from a legacy RVF file or, when that is unreadable, from the JSON
    /// fallback. Only once the node holds a cutover record is the unsigned
    /// JSON fallback refused.
    #[cfg(all(feature = "exochain", unix))]
    #[tokio::test]
    async fn boot_json_fallback_is_refused_only_after_the_pq_cutover() {
        for (case, cut_over) in [("legacy", false), ("cut over", true)] {
            let dir = tempfile::tempdir().unwrap();
            let ckpt = dir.path().join("chain.json");
            let rvf = ckpt.with_extension("rvf");
            let cm = crate::chain::ChainManager::new(0, 0);
            cm.append("test", "from-json", None);
            cm.save_to_file(&ckpt).unwrap();
            std::fs::write(&rvf, b"not an rvf chain").unwrap();
            if cut_over {
                let record = crate::chain_pq::Cutover { chain_id: 0, sequence: 1, hash: "00".into() };
                crate::chain_pq::write_cutover_record(&rvf, &record).unwrap();
            }
            let mut kc = test_kernel_config_exochain();
            kc.chain.as_mut().unwrap().checkpoint_path = Some(ckpt.to_string_lossy().into_owned());
            let booted = Kernel::boot(test_config(), kc, Arc::new(NativePlatform::new())).await;
            if cut_over {
                let msg = booted.err().expect("an unsigned JSON fallback after the cutover must be refused").to_string();
                assert!(msg.contains("refusing to start a fresh chain"), "{case}: {msg}");
            } else {
                let kernel = booted.unwrap_or_else(|e| panic!("{case}: boot failed: {e}"));
                let events = kernel.chain_manager().unwrap().tail(0);
                assert!(events.iter().any(|e| e.kind == "from-json"), "{case}: JSON chain not restored");
            }
        }

        // A legacy, Ed25519-only RVF chain with no record loads as before.
        let dir = tempfile::tempdir().unwrap();
        let ckpt = dir.path().join("chain.json");
        let ek = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
        let cm = crate::chain::ChainManager::new(0, 0).with_signing_key(ek);
        cm.append("test", "legacy-rvf", None);
        cm.save_to_rvf(&ckpt.with_extension("rvf")).unwrap();
        let mut kc = test_kernel_config_exochain();
        kc.chain.as_mut().unwrap().checkpoint_path = Some(ckpt.to_string_lossy().into_owned());
        let kernel = Kernel::boot(test_config(), kc, Arc::new(NativePlatform::new())).await.unwrap();
        let events = kernel.chain_manager().unwrap().tail(0);
        assert!(events.iter().any(|e| e.kind == "legacy-rvf"));
    }

    #[cfg(feature = "exochain")]
    #[tokio::test]
    async fn boot_exochain_creates_chain_manager() {
        let platform = Arc::new(NativePlatform::new());
        let kernel = Kernel::boot(test_config(), test_kernel_config_exochain(), platform)
            .await
            .unwrap();
        assert!(
            kernel.chain_manager().is_some(),
            "chain manager must be present with exochain feature and enabled chain config"
        );
    }

    #[cfg(feature = "exochain")]
    #[tokio::test]
    async fn boot_exochain_creates_tree_manager() {
        let platform = Arc::new(NativePlatform::new());
        let kernel = Kernel::boot(test_config(), test_kernel_config_exochain(), platform)
            .await
            .unwrap();
        assert!(
            kernel.tree_manager().is_some(),
            "tree manager must be present with exochain feature and enabled config"
        );
    }

    #[cfg(feature = "exochain")]
    #[tokio::test]
    async fn boot_exochain_chain_has_boot_events() {
        let platform = Arc::new(NativePlatform::new());
        let kernel = Kernel::boot(test_config(), test_kernel_config_exochain(), platform)
            .await
            .unwrap();
        let chain = kernel.chain_manager().unwrap();
        // Chain should have boot.init, boot.config, boot.services, boot.cluster, boot.ready, boot.manifest at minimum
        assert!(
            chain.sequence() >= 6,
            "chain should have at least 6 boot events, got {}",
            chain.sequence()
        );
    }

    #[cfg(feature = "exochain")]
    #[tokio::test]
    async fn boot_exochain_governance_gate_present() {
        let platform = Arc::new(NativePlatform::new());
        let kernel = Kernel::boot(test_config(), test_kernel_config_exochain(), platform)
            .await
            .unwrap();
        assert!(
            kernel.governance_gate().is_some(),
            "governance gate should be present when chain is enabled"
        );
    }

    #[cfg(feature = "ecc")]
    #[tokio::test]
    async fn boot_ecc_registers_hnsw_service() {
        let platform = Arc::new(NativePlatform::new());
        let kernel = Kernel::boot(test_config(), test_kernel_config(), platform)
            .await
            .unwrap();
        assert!(
            kernel.ecc_hnsw().is_some(),
            "HNSW service must be present with ecc feature"
        );
    }

    #[cfg(feature = "ecc")]
    #[tokio::test]
    async fn boot_ecc_registers_cognitive_tick() {
        let platform = Arc::new(NativePlatform::new());
        let kernel = Kernel::boot(test_config(), test_kernel_config(), platform)
            .await
            .unwrap();
        assert!(
            kernel.ecc_tick().is_some(),
            "cognitive tick must be present with ecc feature"
        );
    }

    #[cfg(feature = "ecc")]
    #[tokio::test]
    async fn boot_ecc_calibration_has_valid_results() {
        let platform = Arc::new(NativePlatform::new());
        let kernel = Kernel::boot(test_config(), test_kernel_config(), platform)
            .await
            .unwrap();
        let cal = kernel.ecc_calibration().unwrap();
        assert!(cal.tick_interval_ms > 0, "tick interval must be positive");
        assert!(cal.compute_p50_us > 0, "p50 latency must be measured");
        assert!(cal.compute_p95_us >= cal.compute_p50_us, "p95 >= p50");
    }

    #[cfg(feature = "ecc")]
    #[tokio::test]
    async fn boot_ecc_causal_graph_accessible() {
        let platform = Arc::new(NativePlatform::new());
        let kernel = Kernel::boot(test_config(), test_kernel_config(), platform)
            .await
            .unwrap();
        assert!(
            kernel.ecc_causal().is_some(),
            "causal graph must be accessible"
        );
        assert_eq!(
            kernel.ecc_causal().unwrap().node_count(),
            0,
            "causal graph starts empty"
        );
    }

    #[cfg(feature = "ecc")]
    #[tokio::test]
    async fn boot_ecc_crossrefs_and_impulses_accessible() {
        let platform = Arc::new(NativePlatform::new());
        let kernel = Kernel::boot(test_config(), test_kernel_config(), platform)
            .await
            .unwrap();
        assert!(
            kernel.ecc_crossrefs().is_some(),
            "cross-ref store must be accessible"
        );
        assert!(
            kernel.ecc_impulses().is_some(),
            "impulse queue must be accessible"
        );
    }

    // ── vector.strict / diskann feature mismatch (WEFT-656) ─────────────
    //
    // These assert against the actual compiled feature set (via
    // `#[cfg(not(feature = "diskann"))]`) rather than injecting a fake
    // mismatch, so they only run meaningfully in the default build (ecc
    // on, diskann off) where the mismatch is real.

    #[cfg(all(feature = "ecc", not(feature = "diskann")))]
    #[tokio::test]
    async fn boot_vector_strict_diskann_without_feature_fails() {
        let platform = Arc::new(NativePlatform::new());
        let mut kconfig = test_kernel_config();
        kconfig.vector = Some(clawft_types::config::VectorConfig {
            backend: clawft_types::config::VectorBackendKind::DiskAnn,
            strict: true,
            ..Default::default()
        });
        let result = Kernel::boot(test_config(), kconfig, platform).await;
        assert!(
            result.is_err(),
            "boot must fail when vector.strict=true and the diskann feature \
             is not compiled in"
        );
    }

    #[cfg(all(feature = "ecc", not(feature = "diskann")))]
    #[tokio::test]
    async fn boot_vector_strict_hybrid_without_feature_fails() {
        let platform = Arc::new(NativePlatform::new());
        let mut kconfig = test_kernel_config();
        kconfig.vector = Some(clawft_types::config::VectorConfig {
            backend: clawft_types::config::VectorBackendKind::Hybrid,
            strict: true,
            ..Default::default()
        });
        let result = Kernel::boot(test_config(), kconfig, platform).await;
        assert!(
            result.is_err(),
            "boot must fail for Hybrid too — it wraps DiskAnnBackend as its \
             cold tier"
        );
    }

    #[cfg(all(feature = "ecc", not(feature = "diskann")))]
    #[tokio::test]
    async fn boot_vector_non_strict_diskann_without_feature_warns_and_boots() {
        let platform = Arc::new(NativePlatform::new());
        let mut kconfig = test_kernel_config();
        kconfig.vector = Some(clawft_types::config::VectorConfig {
            backend: clawft_types::config::VectorBackendKind::DiskAnn,
            strict: false,
            ..Default::default()
        });
        let kernel = Kernel::boot(test_config(), kconfig, platform)
            .await
            .unwrap();
        let warned = kernel.boot_log().events().iter().any(|e| {
            e.level == crate::console::LogLevel::Warn && e.message.contains("diskann")
        });
        assert!(
            warned,
            "expected a Warn boot event naming the diskann feature mismatch"
        );
    }

    #[tokio::test]
    async fn boot_cluster_membership_accessible() {
        let platform = Arc::new(NativePlatform::new());
        let kernel = Kernel::boot(test_config(), test_kernel_config(), platform)
            .await
            .unwrap();
        let cm = kernel.cluster_membership();
        assert!(
            !cm.local_node_id().is_empty(),
            "cluster membership should have a node ID"
        );
    }

    #[tokio::test]
    async fn shutdown_transitions_to_halted() {
        let platform = Arc::new(NativePlatform::new());
        let mut kernel = Kernel::boot(test_config(), test_kernel_config(), platform)
            .await
            .unwrap();
        assert_eq!(*kernel.state(), KernelState::Running);
        kernel.shutdown().await.unwrap();
        assert_eq!(*kernel.state(), KernelState::Halted);
    }

    #[tokio::test]
    async fn shutdown_from_halted_fails() {
        let platform = Arc::new(NativePlatform::new());
        let mut kernel = Kernel::boot(test_config(), test_kernel_config(), platform)
            .await
            .unwrap();
        kernel.shutdown().await.unwrap();
        let err = kernel.shutdown().await.unwrap_err();
        match err {
            KernelError::WrongState { expected, actual } => {
                assert_eq!(expected, "Running");
                assert_eq!(actual, "halted");
            }
            other => panic!("expected WrongState, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn process_table_has_kernel_pid_zero() {
        let platform = Arc::new(NativePlatform::new());
        let kernel = Kernel::boot(test_config(), test_kernel_config(), platform)
            .await
            .unwrap();
        let entry = kernel.process_table().get(0).expect("PID 0 should exist");
        assert_eq!(entry.agent_id, "kernel");
        assert_eq!(entry.state, ProcessState::Running);
        assert_eq!(entry.pid, 0);
    }

    #[tokio::test]
    async fn a2a_router_accessible_after_boot() {
        let platform = Arc::new(NativePlatform::new());
        let kernel = Kernel::boot(test_config(), test_kernel_config(), platform)
            .await
            .unwrap();
        let a2a = kernel.a2a_router();
        // Should be able to create an inbox for a future PID
        // (A2ARouter is wired to the same process table)
        let _inbox = a2a.create_inbox(0); // kernel PID
    }

    #[tokio::test]
    async fn boot_log_contains_expected_phases() {
        let platform = Arc::new(NativePlatform::new());
        let kernel = Kernel::boot(test_config(), test_kernel_config(), platform)
            .await
            .unwrap();
        let formatted = kernel.boot_log().format_all();
        assert!(
            formatted.contains(&format!("WeftOS v{}", env!("CARGO_PKG_VERSION"))),
            "should contain version"
        );
        assert!(
            formatted.contains("Service registry ready"),
            "should have service phase"
        );
        assert!(
            formatted.contains("A2A router ready"),
            "should have A2A phase"
        );
        assert!(
            formatted.contains("Boot complete"),
            "should have ready phase"
        );
    }

    #[tokio::test]
    async fn event_log_populated_after_boot() {
        let platform = Arc::new(NativePlatform::new());
        let kernel = Kernel::boot(test_config(), test_kernel_config(), platform)
            .await
            .unwrap();
        // The event_log is populated from boot_log during boot
        let events = kernel.event_log();
        // At minimum there should be some events from boot ingestion
        assert!(
            !events.is_empty(),
            "event log should have entries after boot"
        );
    }

    #[tokio::test]
    async fn health_system_accessible() {
        let platform = Arc::new(NativePlatform::new());
        let kernel = Kernel::boot(test_config(), test_kernel_config(), platform)
            .await
            .unwrap();
        let _health = kernel.health();
        // Health system should be constructable and accessible
    }

    #[tokio::test]
    async fn uptime_is_positive_after_boot() {
        let platform = Arc::new(NativePlatform::new());
        let kernel = Kernel::boot(test_config(), test_kernel_config(), platform)
            .await
            .unwrap();
        let uptime = kernel.uptime();
        assert!(uptime.as_nanos() > 0, "uptime should be positive");
    }

    #[tokio::test]
    async fn cron_service_accessible_and_empty() {
        let platform = Arc::new(NativePlatform::new());
        let kernel = Kernel::boot(test_config(), test_kernel_config(), platform)
            .await
            .unwrap();
        assert_eq!(kernel.cron_service().job_count(), 0, "no cron jobs at boot");
    }

    #[tokio::test]
    async fn max_processes_matches_config() {
        let platform = Arc::new(NativePlatform::new());
        let mut kconfig = test_kernel_config();
        kconfig.max_processes = 42;
        let kernel = Kernel::boot(test_config(), kconfig, platform)
            .await
            .unwrap();
        assert_eq!(kernel.process_table().max_processes(), 42);
    }

    // ── Sprint 09a: KernelState serde + display tests ────────────

    #[test]
    fn kernel_state_serde_roundtrip_all_variants() {
        for state in [
            KernelState::Booting,
            KernelState::Running,
            KernelState::ShuttingDown,
            KernelState::Halted,
        ] {
            let json = serde_json::to_string(&state).unwrap();
            let restored: KernelState = serde_json::from_str(&json).unwrap();
            assert_eq!(restored, state);
        }
    }

    #[test]
    fn kernel_state_equality() {
        assert_eq!(KernelState::Running, KernelState::Running);
        assert_ne!(KernelState::Running, KernelState::Halted);
    }

    #[tokio::test]
    async fn kernel_state_is_running_after_boot() {
        let platform = Arc::new(NativePlatform::new());
        let kernel = Kernel::boot(test_config(), test_kernel_config(), platform)
            .await
            .unwrap();
        assert_eq!(kernel.state(), &KernelState::Running);
    }

    #[tokio::test]
    async fn boot_log_has_entries() {
        let platform = Arc::new(NativePlatform::new());
        let kernel = Kernel::boot(test_config(), test_kernel_config(), platform)
            .await
            .unwrap();
        let log = kernel.boot_log();
        assert!(!log.events().is_empty(), "boot log should have events");
    }

    #[tokio::test]
    async fn bus_is_accessible_after_boot() {
        let platform = Arc::new(NativePlatform::new());
        let kernel = Kernel::boot(test_config(), test_kernel_config(), platform)
            .await
            .unwrap();
        // Bus should exist (used for message passing)
        let _bus = kernel.bus();
    }

    #[tokio::test]
    async fn a2a_router_has_inboxes_after_boot() {
        let platform = Arc::new(NativePlatform::new());
        let kernel = Kernel::boot(test_config(), test_kernel_config(), platform)
            .await
            .unwrap();
        let _router = kernel.a2a_router();
        // Router should be accessible after boot
    }

    #[tokio::test]
    async fn ipc_accessible_after_boot() {
        let platform = Arc::new(NativePlatform::new());
        let kernel = Kernel::boot(test_config(), test_kernel_config(), platform)
            .await
            .unwrap();
        let _ipc = kernel.ipc();
    }

    // ── W5: Test hardening — additional coverage ─────────────────

    #[tokio::test]
    async fn supervisor_accessible_after_boot() {
        let platform = Arc::new(NativePlatform::new());
        let kernel = Kernel::boot(test_config(), test_kernel_config(), platform)
            .await
            .unwrap();
        let _supervisor = kernel.supervisor();
    }

    #[tokio::test]
    async fn kernel_config_accessor_returns_boot_config() {
        let platform = Arc::new(NativePlatform::new());
        let mut kconfig = test_kernel_config();
        kconfig.max_processes = 99;
        kconfig.health_check_interval_secs = 42;
        let kernel = Kernel::boot(test_config(), kconfig, platform)
            .await
            .unwrap();
        assert_eq!(kernel.kernel_config().max_processes, 99);
        assert_eq!(kernel.kernel_config().health_check_interval_secs, 42);
    }

    #[tokio::test]
    async fn shutdown_exits_all_agent_processes() {
        let platform = Arc::new(NativePlatform::new());
        let mut kernel = Kernel::boot(test_config(), test_kernel_config(), platform)
            .await
            .unwrap();

        // Spawn a couple of agents
        let spawn1 = kernel.supervisor().spawn(crate::supervisor::SpawnRequest {
            agent_id: "test-agent-1".into(),
            capabilities: None,
            parent_pid: None,
            env: std::collections::HashMap::new(),
            backend: None,
        });
        let spawn2 = kernel.supervisor().spawn(crate::supervisor::SpawnRequest {
            agent_id: "test-agent-2".into(),
            capabilities: None,
            parent_pid: None,
            env: std::collections::HashMap::new(),
            backend: None,
        });
        assert!(spawn1.is_ok());
        assert!(spawn2.is_ok());
        let pid1 = spawn1.unwrap().pid;
        let pid2 = spawn2.unwrap().pid;

        // Verify agents exist in process table
        assert!(kernel.process_table().get(pid1).is_some());
        assert!(kernel.process_table().get(pid2).is_some());

        kernel.shutdown().await.unwrap();

        // After shutdown, agent processes should no longer be Running.
        // They may be Exited (if the shutdown loop reached them) or still
        // Starting (if spawn() never transitioned them to Running because
        // no agent loop was attached in this test). Either is acceptable;
        // the key assertion is that shutdown completed without error.
        if let Some(entry1) = kernel.process_table().get(pid1) {
            assert!(
                !matches!(entry1.state, ProcessState::Running),
                "agent 1 should not be Running after shutdown, got: {}",
                entry1.state
            );
        }
        if let Some(entry2) = kernel.process_table().get(pid2) {
            assert!(
                !matches!(entry2.state, ProcessState::Running),
                "agent 2 should not be Running after shutdown, got: {}",
                entry2.state
            );
        }
    }

    #[tokio::test]
    async fn shutdown_stops_services() {
        let platform = Arc::new(NativePlatform::new());
        let mut kernel = Kernel::boot(test_config(), test_kernel_config(), platform)
            .await
            .unwrap();

        // Services should be running before shutdown
        let svc_count_before = kernel.services().len();
        assert!(svc_count_before > 0, "should have services before shutdown");

        kernel.shutdown().await.unwrap();
        // After shutdown, services stop_all() was called — the registry is
        // still there but state is Halted
        assert_eq!(*kernel.state(), KernelState::Halted);
    }

    #[tokio::test]
    async fn boot_with_custom_max_processes() {
        let platform = Arc::new(NativePlatform::new());
        let mut kconfig = test_kernel_config();
        kconfig.max_processes = 128;
        let kernel = Kernel::boot(test_config(), kconfig, platform)
            .await
            .unwrap();
        assert_eq!(kernel.process_table().max_processes(), 128);
    }

    #[tokio::test]
    async fn boot_creates_a2a_router_with_topic_router() {
        let platform = Arc::new(NativePlatform::new());
        let kernel = Kernel::boot(test_config(), test_kernel_config(), platform)
            .await
            .unwrap();
        // A2A router should have a topic router wired in
        let _topic_router = kernel.a2a_router().topic_router();
    }

    #[tokio::test]
    async fn event_log_has_shutdown_events() {
        let platform = Arc::new(NativePlatform::new());
        let mut kernel = Kernel::boot(test_config(), test_kernel_config(), platform)
            .await
            .unwrap();

        let events_before = kernel.event_log().len();
        kernel.shutdown().await.unwrap();

        // Shutdown should have added events
        let events_after = kernel.event_log().len();
        assert!(
            events_after > events_before,
            "event log should grow during shutdown (before={events_before}, after={events_after})"
        );
    }

    #[tokio::test]
    async fn boot_log_contains_max_processes() {
        let platform = Arc::new(NativePlatform::new());
        let mut kconfig = test_kernel_config();
        kconfig.max_processes = 64;
        let kernel = Kernel::boot(test_config(), kconfig, platform)
            .await
            .unwrap();
        let formatted = kernel.boot_log().format_all();
        assert!(
            formatted.contains("Max processes: 64"),
            "boot log should contain max_processes config value"
        );
    }

    #[tokio::test]
    async fn boot_log_contains_health_interval() {
        let platform = Arc::new(NativePlatform::new());
        let mut kconfig = test_kernel_config();
        kconfig.health_check_interval_secs = 15;
        let kernel = Kernel::boot(test_config(), kconfig, platform)
            .await
            .unwrap();
        let formatted = kernel.boot_log().format_all();
        assert!(
            formatted.contains("Health check interval: 15s"),
            "boot log should contain health check interval"
        );
    }

    #[tokio::test]
    async fn take_app_context_returns_some_then_none() {
        let platform = Arc::new(NativePlatform::new());
        let mut kernel = Kernel::boot(test_config(), test_kernel_config(), platform)
            .await
            .unwrap();

        assert!(
            kernel.take_app_context().is_some(),
            "first take should succeed"
        );
        assert!(
            kernel.take_app_context().is_none(),
            "second take should return None"
        );
        assert!(
            kernel.take_app_context().is_none(),
            "third take should also return None"
        );
    }

    #[tokio::test]
    async fn kernel_pid_zero_has_default_capabilities() {
        let platform = Arc::new(NativePlatform::new());
        let kernel = Kernel::boot(test_config(), test_kernel_config(), platform)
            .await
            .unwrap();
        let entry = kernel.process_table().get(0).unwrap();
        assert_eq!(entry.agent_id, "kernel");
        assert!(entry.parent_pid.is_none(), "kernel should have no parent");
        // Default capabilities: can_ipc should be true, ipc_scope should be All
        assert!(entry.capabilities.can_ipc, "kernel should be able to IPC");
    }

    #[tokio::test]
    async fn cluster_membership_has_nonempty_node_id() {
        let platform = Arc::new(NativePlatform::new());
        let kernel = Kernel::boot(test_config(), test_kernel_config(), platform)
            .await
            .unwrap();
        let node_id = kernel.cluster_membership().local_node_id();
        assert!(!node_id.is_empty(), "cluster node ID should not be empty");
        // UUID v4 format: 8-4-4-4-12 = 36 chars
        assert!(
            crate::node_id::is_node_id(node_id),
            "node ID should be hex(SHA-256(pubkey)[..16]), got {node_id}"
        );
    }

    #[tokio::test]
    async fn boot_log_contains_cluster_info() {
        let platform = Arc::new(NativePlatform::new());
        let kernel = Kernel::boot(test_config(), test_kernel_config(), platform)
            .await
            .unwrap();
        let formatted = kernel.boot_log().format_all();
        assert!(
            formatted.contains("Cluster membership ready"),
            "boot log should mention cluster membership"
        );
    }

    #[cfg(feature = "exochain")]
    #[tokio::test]
    async fn shutdown_with_exochain_completes() {
        let platform = Arc::new(NativePlatform::new());
        let mut kernel = Kernel::boot(test_config(), test_kernel_config_exochain(), platform)
            .await
            .unwrap();

        assert!(kernel.chain_manager().is_some());
        kernel.shutdown().await.unwrap();
        assert_eq!(*kernel.state(), KernelState::Halted);
    }

    #[cfg(feature = "exochain")]
    #[tokio::test]
    async fn exochain_chain_integrity_valid_after_boot() {
        let platform = Arc::new(NativePlatform::new());
        let kernel = Kernel::boot(test_config(), test_kernel_config_exochain(), platform)
            .await
            .unwrap();
        let chain = kernel.chain_manager().unwrap();
        let result = chain.verify_integrity();
        assert!(result.valid, "chain integrity should be valid after boot");
        assert!(result.errors.is_empty(), "no integrity errors expected");
    }

    #[cfg(feature = "exochain")]
    #[tokio::test]
    async fn exochain_boot_manifest_event_present() {
        let platform = Arc::new(NativePlatform::new());
        let kernel = Kernel::boot(test_config(), test_kernel_config_exochain(), platform)
            .await
            .unwrap();
        let chain = kernel.chain_manager().unwrap();
        let all_events = chain.tail(0);
        let manifest_events: Vec<_> = all_events
            .iter()
            .filter(|e| e.kind == "boot.manifest")
            .collect();
        assert!(
            !manifest_events.is_empty(),
            "boot.manifest event should be on chain"
        );
    }

    // ── os-patterns observability wiring tests (W3) ─────────────────

    #[tokio::test]
    #[cfg(feature = "os-patterns")]
    async fn boot_creates_metrics_registry() {
        let platform = Arc::new(NativePlatform::new());
        let kernel = Kernel::boot(test_config(), test_kernel_config(), platform)
            .await
            .unwrap();

        let registry = kernel.metrics_registry();
        assert!(
            registry.is_some(),
            "MetricsRegistry should be created at boot"
        );
        // Verify a built-in gauge was seeded
        let r = registry.unwrap();
        let val = r.gauge_get("kernel.process.count");
        assert!(val >= 1, "kernel.process.count gauge should be seeded");
    }

    #[tokio::test]
    #[cfg(feature = "os-patterns")]
    async fn boot_creates_log_service() {
        let platform = Arc::new(NativePlatform::new());
        let kernel = Kernel::boot(test_config(), test_kernel_config(), platform)
            .await
            .unwrap();

        assert!(
            kernel.log_service().is_some(),
            "LogService should be created at boot"
        );
    }

    #[tokio::test]
    #[cfg(feature = "os-patterns")]
    async fn boot_creates_timer_service() {
        let platform = Arc::new(NativePlatform::new());
        let kernel = Kernel::boot(test_config(), test_kernel_config(), platform)
            .await
            .unwrap();

        assert!(
            kernel.timer_service().is_some(),
            "TimerService should be created at boot"
        );
    }

    #[tokio::test]
    #[cfg(feature = "os-patterns")]
    async fn boot_creates_dead_letter_queue() {
        let platform = Arc::new(NativePlatform::new());
        let kernel = Kernel::boot(test_config(), test_kernel_config(), platform)
            .await
            .unwrap();

        let dlq = kernel.dead_letter_queue();
        assert!(dlq.is_some(), "DeadLetterQueue should be created at boot");
        assert!(dlq.unwrap().is_empty(), "DLQ should start empty");
    }

    #[tokio::test]
    #[cfg(feature = "os-patterns")]
    async fn kernel_metrics_accessor_returns_registry() {
        let platform = Arc::new(NativePlatform::new());
        let kernel = Kernel::boot(test_config(), test_kernel_config(), platform)
            .await
            .unwrap();

        let registry = kernel.metrics_registry().unwrap();
        // Verify counter_inc works on built-in metric
        registry.counter_inc(crate::metrics::METRIC_MESSAGES_SENT);
        assert_eq!(
            registry.counter_get(crate::metrics::METRIC_MESSAGES_SENT),
            1
        );
    }

    #[tokio::test]
    #[cfg(feature = "os-patterns")]
    async fn dlq_accessible_via_kernel_accessor() {
        let platform = Arc::new(NativePlatform::new());
        let kernel = Kernel::boot(test_config(), test_kernel_config(), platform)
            .await
            .unwrap();

        let dlq = kernel.dead_letter_queue().unwrap();
        assert_eq!(dlq.capacity(), crate::dead_letter::DEFAULT_DLQ_CAPACITY);
    }

    #[tokio::test]
    #[cfg(feature = "os-patterns")]
    async fn failed_a2a_send_routes_to_dlq() {
        use crate::ipc::{KernelMessage, MessageTarget};

        let platform = Arc::new(NativePlatform::new());
        let kernel = Kernel::boot(test_config(), test_kernel_config(), platform)
            .await
            .unwrap();

        // Register a sender agent
        let sender_entry = ProcessEntry {
            pid: 0,
            agent_id: "dlq-sender".to_owned(),
            state: ProcessState::Running,
            capabilities: AgentCapabilities::default(),
            resource_usage: ResourceUsage::default(),
            cancel_token: crate::process::CancellationToken::new(),
            parent_pid: None,
        };
        let sender_pid = kernel.process_table().insert(sender_entry).unwrap();
        kernel.a2a_router().create_inbox(sender_pid);

        // Send to a PID that has no inbox (PID 9999 doesn't exist)
        let msg = KernelMessage::text(sender_pid, MessageTarget::Process(9999), "dead letter test");
        let result = kernel.a2a_router().send(msg).await;
        assert!(result.is_err(), "send to unknown PID should fail");

        // Verify the message landed in the DLQ
        let dlq = kernel.dead_letter_queue().unwrap();
        assert_eq!(dlq.len(), 1, "DLQ should contain 1 dead letter");
    }

    #[tokio::test]
    #[cfg(feature = "os-patterns")]
    async fn kernel_metrics_gauge_seeded_at_boot() {
        let platform = Arc::new(NativePlatform::new());
        let kernel = Kernel::boot(test_config(), test_kernel_config(), platform)
            .await
            .unwrap();

        let registry = kernel.metrics_registry().unwrap();
        // kernel.process.count should reflect at least the kernel PID 0
        assert!(
            registry.gauge_get("kernel.process.count") >= 1,
            "kernel.process.count should be >= 1 at boot"
        );
        // Verify additional gauges exist (they were set to 0, so gauge_get returns 0 not None)
        assert_eq!(
            registry.gauge_get("kernel.uptime_secs"),
            0,
            "kernel.uptime_secs gauge should exist and be 0 at boot"
        );
        assert_eq!(
            registry.gauge_get("kernel.chain.height"),
            0,
            "kernel.chain.height gauge should exist and be 0 at boot"
        );
    }
}
