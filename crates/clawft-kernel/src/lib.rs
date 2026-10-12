//! WeftOS kernel layer for clawft.
//!
//! This crate provides the kernel abstraction layer that sits between
//! the CLI/API surface and `clawft-core`. It introduces:
//!
//! - **Boot sequence** ([`boot::Kernel`]) -- lifecycle management
//!   wrapping `AppContext` with structured startup/shutdown.
//! - **Process table** ([`process::ProcessTable`]) -- PID-based
//!   agent tracking with state machine transitions.
//! - **Service registry** ([`service::ServiceRegistry`]) -- named
//!   service lifecycle with health checks.
//! - **IPC** ([`ipc::KernelIpc`]) -- typed message envelopes over
//!   the existing `MessageBus`.
//! - **Capabilities** ([`capability::AgentCapabilities`]) -- permission
//!   model for agent processes.
//! - **Health monitoring** ([`health::HealthSystem`]) -- aggregated
//!   health checks across all services.
//! - **Console** ([`console`]) -- boot event types and output
//!   formatting for the interactive kernel terminal.
//! - **Configuration** ([`config::KernelConfig`]) -- kernel-specific
//!   settings embedded in the root config.
//! - **Containers** ([`container::ContainerManager`]) -- sidecar
//!   container lifecycle and health integration.
//! - **Applications** ([`app::AppManager`]) -- application manifest
//!   parsing, validation, and lifecycle state machine.
//! - **Cluster** ([`cluster::ClusterMembership`]) -- multi-node
//!   cluster membership, peer tracking, and health.
//! - **Environments** ([`environment::EnvironmentManager`]) --
//!   governance-scoped dev/staging/prod environments.
//! - **Governance** ([`governance::GovernanceEngine`]) -- three-branch
//!   constitutional governance with effect algebra scoring.
//! - **Agency** ([`agency::Agency`]) -- agent-first architecture
//!   with roles, spawn permissions, and agent manifests.
//!
//! # Feature Flags
//!
//! - `native` (default) -- enables tokio runtime, native file I/O.
//! - `mesh` (default) -- mesh transport, framing, IPC (JSON KernelMessage).
//! - `mesh-rvf` -- experimental RVF IPC encoding API surface (ADR-031 /
//!   WEFT-683); encode/decode currently return `UnsupportedEncoding`.
//! - `wasm-sandbox` -- enables WASM tool runner (Phase K3).
//! - `containers` -- enables container manager (Phase K4).
//! - `ecc` -- enables ECC cognitive substrate (Phase K3c).
//!
//! ## Crate Ecosystem
//!
//! WeftOS is built from these crates:
//!
//! | Crate | Role |
//! |-------|------|
//! | [`weftos`](https://crates.io/crates/weftos) | Product facade -- re-exports kernel, core, types |
//! | [`clawft-kernel`](https://crates.io/crates/clawft-kernel) | Kernel: processes, services, governance, mesh, ExoChain |
//! | [`clawft-core`](https://crates.io/crates/clawft-core) | Agent framework: pipeline, context, tools, skills |
//! | [`clawft-types`](https://crates.io/crates/clawft-types) | Shared type definitions |
//! | [`clawft-platform`](https://crates.io/crates/clawft-platform) | Platform abstraction (native/WASM/browser) |
//! | [`clawft-plugin`](https://crates.io/crates/clawft-plugin) | Plugin SDK for tools, channels, and extensions |
//! | [`clawft-llm`](https://crates.io/crates/clawft-llm) | LLM provider abstraction (11 providers + local) |
//! | [`exo-resource-tree`](https://crates.io/crates/exo-resource-tree) | Hierarchical resource namespace with Merkle integrity |
//!
//! Source: <https://github.com/weave-logic-ai/weftos>

// `test-support` exposes a process-wide child-root override for integration
// tests; it must never reach a release build.
#[cfg(all(feature = "test-support", not(debug_assertions)))]
compile_error!("clawft-kernel: the `test-support` feature is for debug test builds only");

// WEFT-504: `ecc` pulls native-only deps (blake3, clawft-core/vector-memory,
// clawft-bvh). Browser / pure wasm32-unknown-unknown consumers must build
// with `--no-default-features` (mesh + ecc off). `scripts/build.sh browser`
// never enables ecc; `scripts/build.sh check|gate` assert the rejection.
#[cfg(all(
    feature = "ecc",
    target_arch = "wasm32",
    target_os = "unknown"
))]
compile_error!(
    "feature \"ecc\" is not supported on wasm32-unknown-unknown (WEFT-504); \
     use --no-default-features for kernel WASM checks, or scripts/build.sh browser \
     (clawft-wasm never enables ecc)"
);

// ── ECC cognitive substrate modules (K3c) ────────────────────────
#[cfg(feature = "ecc")]
pub mod artifact_store;
// mesh-placement-07: signed workload package manifests (ADR-100 section 1).
#[cfg(all(feature = "ecc", feature = "exochain"))]
pub mod workload_pkg;
// P2-B: workload-kind registry (cog-boundary audit M3).
#[cfg(all(feature = "ecc", feature = "exochain"))]
pub mod workload_kind;
// mesh-placement-17: model manifest, adopt-in-place and weights locality (ADR-101 section 3).
#[cfg(all(feature = "ecc", feature = "exochain", feature = "native"))]
pub mod model_manifest;
// mesh-placement-09: WorkloadRuntime adapters (native, container, Seed API).
#[cfg(all(feature = "workload-runtime", unix))]
pub mod workload_runtime;
// mesh-placement-19: stable inference address, loopback proxy, mesh forwarding.
#[cfg(all(feature = "workload-runtime", feature = "mesh", unix))]
pub mod infer_proxy;
// mesh-placement-10: cog ingest bridge and store forwarding.
#[cfg(all(feature = "workload-runtime", feature = "mesh", unix))]
pub mod cog_ingest;
// mesh-placement-12: placement control plane and the workload-host service.
#[cfg(all(feature = "workload-runtime", feature = "mesh", unix))]
pub mod workload_ctl;
#[cfg(feature = "ecc")]
pub mod calibration;
#[cfg(feature = "ecc")]
pub mod causal;
#[cfg(feature = "ecc")]
pub mod causal_predict;
#[cfg(feature = "ecc")]
pub mod ecc_segment;
#[cfg(feature = "ecc")]
pub mod lewm_invariant;
#[cfg(feature = "ecc")]
pub mod cognitive_tick;
#[cfg(feature = "ecc")]
pub mod coherence;
#[cfg(feature = "ecc")]
pub mod context_graft;
#[cfg(feature = "ecc")]
pub mod context_promote;
#[cfg(feature = "ecc")]
pub mod crossref;
#[cfg(feature = "ecc")]
pub mod democritus;
#[cfg(feature = "ecc")]
pub mod duplex;
#[cfg(feature = "ecc")]
pub mod thin_edge;
#[cfg(all(feature = "ecc", feature = "exochain"))]
pub mod causal_state_fold;
#[cfg(feature = "ecc")]
pub mod embedding;
#[cfg(feature = "ecc")]
pub mod embedding_e5;
#[cfg(feature = "ecc")]
pub mod embedding_onnx;
#[cfg(feature = "ecc")]
pub mod embedding_qwen3;
#[cfg(feature = "ecc")]
pub mod eml_coherence;
#[cfg(feature = "ecc")]
pub mod eml_kernel;
#[cfg(feature = "ecc")]
pub mod eml_persistence;
#[cfg(feature = "ecc")]
pub mod floor;
#[cfg(feature = "ecc")]
pub mod hnsw_eml;
#[cfg(feature = "ecc")]
pub mod hnsw_service;
#[cfg(feature = "ecc")]
pub mod impulse;
#[cfg(feature = "ecc")]
pub mod persistence;
#[cfg(feature = "ecc")]
pub mod profile_store;
#[cfg(feature = "ecc")]
pub mod quantum_backend;
#[cfg(all(feature = "ecc", feature = "quantum-braket"))]
pub mod quantum_braket;
#[cfg(all(feature = "ecc", feature = "quantum-pasqal"))]
pub mod quantum_pasqal;
#[cfg(feature = "ecc")]
pub mod quantum_register;
#[cfg(feature = "ecc")]
pub mod quantum_state;
#[cfg(feature = "ecc")]
pub mod talk_loop;
#[cfg(feature = "ecc")]
pub mod vector_backend;
#[cfg(feature = "ecc")]
pub mod vector_diskann;
#[cfg(feature = "ecc")]
pub mod vector_hnsw;
#[cfg(feature = "ecc")]
pub mod vector_hybrid;
#[cfg(feature = "ecc")]
pub mod vector_quantization;
#[cfg(feature = "ecc")]
pub mod spatial_backend;
#[cfg(feature = "ecc")]
pub mod spatial_bvh;
#[cfg(feature = "ecc")]
pub mod spatial_service;
#[cfg(feature = "ecc")]
pub mod view_resolver;
#[cfg(feature = "ecc")]
pub mod weaver;

#[cfg(feature = "sensor")]
pub mod sensor_graph;

#[cfg(feature = "native")]
pub mod a2a;
pub mod agency;
#[cfg(feature = "native")]
pub mod agent_loop;
#[cfg(feature = "native")]
pub mod agent_registry;
pub mod app;
pub mod assessment;
pub mod boot;
pub mod capability;
/// Cross-node signed capability advertisements (WEFT-147).
pub mod capability_claim;
#[cfg(feature = "exochain")]
pub mod chain;
#[cfg(feature = "exochain")]
pub mod chain_anchor;
#[cfg(feature = "exochain")]
pub mod chain_rule_hash;
/// Project governance overlay, kernel side: load, merge, hash (ADR-103 D8).
#[cfg(feature = "exochain")]
pub mod governance_overlay;
/// A project kernel's live governance: boot, reload, parent updates.
#[cfg(feature = "exochain")]
pub mod overlay_runtime;
/// Trust root, rollback pin and revocation checks of a project kernel.
#[cfg(feature = "exochain")]
pub mod overlay_trust;
/// The signed parent policy the user daemon exports (ADR-103 D8).
#[cfg(feature = "exochain")]
pub mod parent_policy;
#[cfg(feature = "exochain")]
pub mod chain_subscribe;
#[cfg(all(test, feature = "exochain"))]
mod chain_hash_tests;
#[cfg(all(test, feature = "exochain"))]
mod governance_overlay_tests;
#[cfg(all(test, feature = "exochain"))]
mod governance_overlay_prop_tests;
#[cfg(all(test, feature = "exochain"))]
mod overlay_runtime_tests;
#[cfg(all(test, feature = "exochain"))]
mod overlay_swap_tests;
#[cfg(all(test, feature = "exochain"))]
mod overlay_trust_tests;
#[cfg(all(test, feature = "exochain"))]
mod overlay_boot_tests;
#[cfg(feature = "exochain")]
pub mod chain_migrate;
pub mod chain_storage;
#[cfg(feature = "exochain")]
pub mod token_authority;
#[cfg(all(feature = "exochain", feature = "native"))]
pub mod project_identity;
#[cfg(all(feature = "exochain", feature = "native"))]
pub mod user_key_rotation;
// S10 key rotation needs Clock (mesh) + ChainManager (exochain). WEFT-107.
#[cfg(all(feature = "exochain", feature = "mesh"))]
pub mod key_rotation;
pub mod cluster;
pub mod config;
pub mod console;
pub mod container;
pub mod cron;
pub mod environment;
pub mod error;
#[cfg(feature = "exochain")]
pub mod gate;
pub mod governance;
pub mod governance_project;
pub mod rule_distribution;
#[cfg(all(feature = "ecc", feature = "exochain"))]
pub mod placement_vocabulary;
pub mod health;
pub mod heartbeat;
pub mod ipc;
/// The one node-id derivation (ADR-103 D11).
pub mod node_id;
/// File-backed node key shared by the daemon and one-shot foreground boots.
#[cfg(any(feature = "mesh", feature = "exochain"))]
pub mod node_key;
#[cfg(feature = "native")]
pub mod node_registry;
/// Node facts probes and TTL cache (ADR-099 section 2, card mesh-placement-03).
#[cfg(feature = "native")]
pub mod node_facts;
/// Mesh exposures: registry, serve check, activity log (ADR-120).
#[cfg(feature = "native")]
pub mod exposure;
/// Signed node facts, beside `capability_claim` (card mesh-placement-03).
#[cfg(all(feature = "native", any(feature = "mesh", feature = "exochain")))]
pub mod node_facts_advert;
/// Node facts and deltas over the mesh with receiver-assigned trust (card mesh-placement-03).
#[cfg(all(feature = "native", feature = "mesh"))]
pub mod node_facts_exchange;
pub mod process;
pub mod rate_limit;
pub mod revocation;
/// Governance actions, audit events and permits for placed workloads (ADR-099).
#[cfg(feature = "exochain")]
pub mod workload_governance;
/// Bound on chain writes caused by unauthenticated requests.
pub mod refusal_budget;
pub mod service;
#[cfg(all(feature = "native", feature = "exochain"))]
pub mod stream_anchor;
#[cfg(feature = "native")]
pub mod substrate_service;
pub mod supervisor;
pub mod topic;
#[cfg(feature = "exochain")]
pub mod tree_manager;

// ── Self-healing & process management modules (08a) ─────────────
#[cfg(feature = "os-patterns")]
pub mod monitor;
#[cfg(feature = "os-patterns")]
pub mod reconciler;
#[allow(clippy::new_without_default)]
pub mod wasm_runner;

// ── Reliable IPC & observability modules (08b) ──────────────────
#[cfg(feature = "os-patterns")]
pub mod dead_letter;
#[cfg(feature = "os-patterns")]
pub mod log_service;
#[cfg(feature = "os-patterns")]
pub mod metrics;
#[cfg(feature = "os-patterns")]
pub mod named_pipe;
#[cfg(feature = "os-patterns")]
pub mod reliable_queue;
#[cfg(feature = "os-patterns")]
pub mod timer;

// ── Content integrity & operational services (08c) ───────────────
pub mod auth_service;
pub mod config_service;
#[cfg(feature = "http-api")]
pub mod http_api;
/// Transport-agnostic REST + SSE facade types (WEFT-122 / Cognitum gaps #6–#8).
///
/// Bound by `clawft-services` axum handlers; not a standalone server.
#[cfg(feature = "http-api")]
pub mod http_facade;
pub mod tree_view;

// ── Mesh networking modules (K6) ──────────────────────────────
// WEFT-151: mesh_bootstrap / mesh_dedup / mesh_listener / mesh_log are
// unit-tested library orphans (no boot/daemon/CLI callers). See each
// module's "Status (WEFT-151 audit)" docs for wiring schedule. Do not
// hard-delete; do not apply #[deprecated] under clippy -D warnings.
#[cfg(feature = "mesh")]
pub mod mesh;
#[cfg(feature = "mesh")]
pub mod mesh_clock;
// mesh-placement-11: swarm-ready artifact piece protocol (ADR-099 section 6).
#[cfg(all(feature = "mesh", feature = "ecc", feature = "exochain", feature = "native"))]
pub mod mesh_artifact;
#[cfg(all(test, feature = "mesh", feature = "ecc", feature = "exochain", feature = "native"))]
mod mesh_artifact_large_tests;
#[cfg(all(feature = "mesh", feature = "ecc", feature = "exochain", feature = "native"))]
pub mod mesh_artifact_peers;
#[cfg(all(feature = "mesh", feature = "ecc", feature = "exochain", feature = "native"))]
pub mod mesh_artifact_pkg;
#[cfg(all(test, feature = "mesh", feature = "ecc", feature = "exochain", feature = "native"))]
mod mesh_artifact_tests;
#[cfg(all(test, feature = "mesh", feature = "ecc", feature = "exochain", feature = "native"))]
mod mesh_artifact_rogue_tests;
#[cfg(all(feature = "mesh", feature = "ecc", feature = "exochain", feature = "native"))]
pub mod mesh_artifact_transfer;
#[cfg(all(feature = "mesh", feature = "ecc", feature = "exochain", feature = "native"))]
pub mod mesh_artifact_types;
#[cfg(all(feature = "mesh", feature = "ecc", feature = "exochain", feature = "native"))]
pub mod mesh_artifact_wire;
// mesh-placement-25: swarm distribution (multi-source fetch, seeding, cache, bans, revocation).
#[cfg(all(feature = "mesh", feature = "ecc", feature = "exochain", feature = "native"))]
pub mod mesh_swarm_cache;
#[cfg(all(feature = "mesh", feature = "ecc", feature = "exochain", feature = "native"))]
pub mod mesh_swarm_fetch;
#[cfg(all(feature = "mesh", feature = "ecc", feature = "exochain", feature = "native"))]
pub mod mesh_swarm_governance;
#[cfg(all(feature = "mesh", feature = "ecc", feature = "exochain", feature = "native"))]
pub mod mesh_swarm_lookup;
#[cfg(all(feature = "mesh", feature = "ecc", feature = "exochain", feature = "native"))]
pub mod mesh_swarm_pkg;
#[cfg(all(feature = "mesh", feature = "ecc", feature = "exochain", feature = "native"))]
pub mod mesh_swarm_picker;
#[cfg(all(feature = "mesh", feature = "ecc", feature = "exochain", feature = "native"))]
pub mod mesh_swarm_rate;
#[cfg(all(feature = "mesh", feature = "ecc", feature = "exochain", feature = "native"))]
pub mod mesh_swarm_revoke;
#[cfg(all(feature = "mesh", feature = "ecc", feature = "exochain", feature = "native"))]
pub mod mesh_swarm_state;
// ADR-106 phase 1a: Seed licence proxy, member side (checkout grants, approvals, policy).
#[cfg(all(feature = "mesh", feature = "ecc", feature = "exochain", feature = "native"))]
pub mod licence;
// ADR-106 phase 1c: artifact streams and the checkout relay over machine-mesh deliveries.
#[cfg(all(feature = "mesh", feature = "ecc", feature = "exochain", feature = "native"))]
pub mod mesh_artifact_tunnel;
#[cfg(all(feature = "mesh", feature = "ecc", feature = "exochain", feature = "native"))]
pub mod mesh_cog;
#[cfg(all(test, feature = "mesh", feature = "ecc", feature = "exochain", feature = "native"))]
mod mesh_swarm_tests;
#[cfg(feature = "mesh")]
pub mod mesh_admit;
#[cfg(feature = "mesh")]
pub mod mesh_admit_gate;
#[cfg(feature = "mesh")]
pub mod mesh_assess;
#[cfg(feature = "mesh")]
pub mod mesh_bootstrap;
#[cfg(feature = "mesh")]
pub mod mesh_chain;
#[cfg(feature = "mesh")]
pub mod mesh_dedup;
#[cfg(feature = "mesh")]
pub mod mesh_delivery;
#[cfg(feature = "mesh")]
pub mod mesh_discovery;
#[cfg(feature = "mesh")]
pub mod mesh_framing;
#[cfg(feature = "mesh")]
pub mod mesh_heartbeat;
#[cfg(feature = "mesh")]
pub mod mesh_liveness;
#[cfg(feature = "mesh")]
pub mod mesh_load;
#[cfg(feature = "mesh")]
pub mod mesh_ipc;
#[cfg(feature = "mesh")]
pub mod mesh_kad;
#[cfg(feature = "mesh")]
pub mod mesh_limits;
#[cfg(feature = "mesh")]
pub mod mesh_listener;
#[cfg(feature = "mesh")]
pub mod mesh_log;
#[cfg(feature = "mesh")]
pub mod mesh_mdns;
#[cfg(feature = "mesh")]
pub mod mesh_mode;
#[cfg(feature = "mesh")]
pub mod mesh_noise;
#[cfg(feature = "mesh")]
pub mod mesh_process;
#[cfg(feature = "mesh")]
pub mod mesh_runtime;
#[cfg(feature = "mesh")]
pub mod mesh_leaf;
#[cfg(feature = "mesh")]
pub mod mesh_leaf_bind;
#[cfg(feature = "mesh")]
pub mod mesh_sensor;
#[cfg(feature = "mesh")]
pub mod mesh_serve;
#[cfg(feature = "mesh")]
pub mod mesh_service;
#[cfg(feature = "mesh")]
pub mod mesh_service_adv;
#[cfg(feature = "mesh")]
pub mod mesh_system_service;
#[cfg(feature = "mesh")]
pub mod mesh_tcp;
/// Multi-node mesh test fixtures (WEFT-112). Available outside `cfg(test)` so
/// downstream crates and integration tests can construct in-memory meshes.
#[cfg(feature = "mesh")]
pub mod mesh_test_support;
#[cfg(feature = "mesh")]
pub mod mesh_tree;
#[cfg(feature = "mesh")]
pub mod mesh_ws;
/// QUIC transport via quinn (WEFT-118 / ADR-026). Feature-gated `quic`.
#[cfg(all(feature = "mesh", feature = "quic"))]
pub mod mesh_quic;

// Re-export key types at the crate level for convenience.
#[cfg(feature = "native")]
pub use a2a::A2ARouter;
pub use agency::{
    Agency, AgentHealth, AgentInterface, AgentManifest, AgentPriority, AgentResources,
    AgentRestartPolicy, AgentRole, InterfaceProtocol, ResponseMode,
};
#[cfg(feature = "native")]
pub use agent_registry::{
    AgentRegistry, RegisteredAgent, publish_payload, register_payload, subscribe_payload,
};
pub use app::{
    AgentSpec, AppCapabilities, AppError, AppHooks, AppManager, AppManifest, AppState, AppsFile,
    InstalledApp, ServiceSpec, ToolSource, ToolSpec,
};
#[cfg(feature = "ecc")]
pub use artifact_store::{ArtifactBackend, ArtifactStore, ArtifactType, StoredArtifact};
pub use assessment::{
    AnalysisContext, Analyzer, AnalyzerRegistry, AssessmentDiff, AssessmentReport,
    AssessmentService, AssessmentSummary, ComparisonReport, Finding,
    PeerInfo as AssessmentPeerInfo,
};
#[cfg(feature = "os-patterns")]
pub use auth_service::{
    AuditEntry, AuthService, AuthToken, CredentialGrant, CredentialRequest, CredentialType,
    HashedCredential, IssuedToken, StoredCredential as AuthStoredCredential,
};
pub use boot::{Kernel, KernelState};
#[cfg(feature = "ecc")]
pub use calibration::{EccCalibration, EccCalibrationConfig};
pub use capability::{
    AgentCapabilities, CapabilityChecker, CapabilityElevationRequest, ElevationResult, IpcScope,
    ResourceLimits, ResourceType, SandboxPolicy, ToolPermissions,
};
pub use capability_claim::{
    CAPABILITY_ALLOWLIST, CapabilityClaim, CapabilityClaimError, SignedCapabilityAdvertisement,
    validate_capabilities, validate_capability,
};
#[cfg(any(feature = "mesh", feature = "exochain"))]
pub use capability_claim::{apply_verified_capabilities, sign_claim, verify_signed_advertisement};
#[cfg(feature = "ecc")]
pub use causal::{
    CausalEdge, CausalEdgeType, CausalGraph, CausalNode, ChangeEvent, ChangePrediction,
    CouplingPair, SPECTRAL_EML_MIN_NODES, SPECTRAL_RFF_MIN_NODES, SpectralMethod, SpectralResult,
    select_spectral_method,
};
#[cfg(feature = "ecc")]
pub use ecc_segment::{
    CalibrationProfileSegment, EccSegment, EccSegmentCodec, EccSegmentError, EccSegmentType,
    SpectralCheckpoint, decode_ecc_segment, encode_ecc_segment, segment_from_wire, segment_to_wire,
};
#[cfg(feature = "ecc")]
pub use lewm_invariant::{
    DecouplingRule, InvariantCheck, WmWriteKind, WorldModelFacade, check_wm_write,
    local_ecc_sufficient_without_wm,
};
#[cfg(feature = "ecc")]
pub use causal_predict::{
    CausalCollapseModel, CausalRankRequest, CausalRankResponse, CoherenceTracker, CollapseFeatures,
    ConversationState, EvidenceRanking, detect_conversation_cycle, predict_delta_lambda2,
    rank_evidence_by_impact,
};
#[cfg(feature = "exochain")]
pub use chain::{
    AnchorReceipt, AppendSignedError, ChainAnchor, ChainCheckpoint, ChainEvent, ChainLoggable,
    ChainManager, ChainStatus, ChainVerifyResult, CustodyAttestation, GovernanceDecisionEvent,
    IpcDeadLetterEvent, MockAnchor, RestartEvent,
};
#[cfg(all(feature = "exochain", feature = "mesh"))]
pub use key_rotation::{
    DEFAULT_GRACE_PERIOD_SECS, KIND_KEY_ROTATION, SOURCE_IDENTITY, KeyRotationError,
    KeyRotationPayload, KeyRotationVerifier, build_key_rotation_payload, format_rotation_summary,
    rotate_chain_signing_key, rotate_chain_signing_key_now, verify_key_rotation_event,
    verify_key_rotation_payload,
};
#[cfg(feature = "exochain")]
pub use chain_anchor::{
    AnchorFrequencyPolicy, AnchoringController, ExternalIntentEntry, ExternalLedgerAnchor,
    FileLedgerAnchor, FileLedgerEntry,
};
pub use clawft_types::config::{
    ClusterNetworkConfig, KernelConfig, PairingConfig, ProfilesConfig,
    VectorBackendKind as VectorBackendKindConfig, VectorConfig, VectorDiskAnnConfig,
    VectorEvictionPolicy, VectorHnswConfig, VectorHybridConfig,
};
#[cfg(feature = "cluster")]
pub use cluster::ClusterService;
#[cfg(feature = "ecc")]
pub use cluster::NodeEccCapability;
#[cfg(any(feature = "mesh", feature = "exochain"))]
pub use cluster::NodeIdentity;
pub use cluster::{
    ClusterConfig, ClusterError, ClusterMembership, NodeId, NodePlatform, NodeState, PairedHost,
    PairedHostsFile, PairingGate, PairingWindowResult, PeerNode,
};
#[cfg(feature = "ecc")]
pub use cognitive_tick::{CognitiveTick, CognitiveTickConfig, CognitiveTickStats};
#[cfg(feature = "ecc")]
pub use coherence::{
    CoherenceBand, CoherenceDampener, CoherenceSignals, DAMPEN_FACTOR, compute_coherence,
};
pub use config::KernelConfigExt;
#[cfg(feature = "os-patterns")]
pub use config_service::{ConfigChange, ConfigEntry, ConfigService, ConfigValue, SecretRef};
pub use console::{BootEvent, BootLog, BootPhase, KernelEventLog, LogLevel};
pub use container::{
    ContainerConfig, ContainerError, ContainerManager, ContainerService, ContainerState,
    ManagedContainer, PortMapping, RestartPolicy, VolumeMount,
};
pub use cron::{CronError, CronService};
#[cfg(feature = "ecc")]
pub use crossref::{CrossRef, CrossRefStore, CrossRefType, StructureTag, UniversalNodeId};
#[cfg(feature = "ecc")]
pub use democritus::{DemocritusConfig, DemocritusLoop, DemocritusTickResult};
#[cfg(feature = "ecc")]
pub use embedding::{
    EmbeddingError, EmbeddingProvider, LlmEmbeddingConfig, LlmEmbeddingProvider,
    MockEmbeddingProvider, select_embedding_provider,
};
#[cfg(feature = "ecc")]
pub use embedding_e5::{
    E5EmbeddingProvider, E5_DIMS, E5_MAX_TOKENS, E5_MODEL_NAME, PASSAGE_PREFIX, QUERY_PREFIX,
};
#[cfg(feature = "ecc")]
pub use embedding_onnx::{
    AstEmbeddingProvider, OnnxEmbeddingProvider, RustCodeFeatures, SentenceTransformerProvider,
    extract_rust_features, preprocess_markdown, split_sentences,
};
pub use environment::{
    AuditLevel, Environment, EnvironmentClass, EnvironmentError, EnvironmentManager,
    GovernanceBranches, GovernanceScope, LearningMode,
};
#[cfg(feature = "ecc")]
pub use duplex::{
    DuplexChannel, DuplexImpulse, DuplexState, EdgeCommand, FloorVerdict, MediaPayload, PayloadKind,
    StreamObservation,
};
#[cfg(feature = "ecc")]
pub use thin_edge::{
    ControlMessage, EdgeReflexShadow, EdgeState, LocalhostDuplexSession, MediaFrame, ObserveOutcome,
    ThinEdge,
};
#[cfg(all(feature = "ecc", feature = "exochain"))]
pub use causal_state_fold::{
    NodeStateTransition, fold_node_states, fold_node_states_from_chain, fold_node_states_until,
};
pub use error::{KernelError, KernelResult};
#[cfg(feature = "ecc")]
pub use floor::{
    ContentReadiness, ERL_BARGE_FLOOR, ERL_BARGE_HYSTERESIS, ErlBargeConfig, ErlBargeGate,
    FloorCandidate, FloorDecision, FloorState, UrgencySignals, compute_urgency, contending_count,
    crowd_density, erl_admits_barge, erl_admits_barge_opt, evaluate_floor, floor_verdict_from_erl,
    floor_verdict_from_erl_config,
};
#[cfg(feature = "exochain")]
pub use gate::{CapabilityGate, GateBackend, GateDecision, GovernanceGate};
pub use governance::{
    EffectVector, GateEffectKind, GatePrincipal, GovernanceBranch, GovernanceDecision,
    GovernanceEngine, GovernanceRequest, GovernanceResult, GovernanceRule, GovernanceRuleType,
    RuleSeverity, selector_matches, with_effect_context,
};
pub use rule_distribution::{
    EscalationOutcome, EscalationRecord, RuleDistribution, RuleGossipEnvelope, VersionedRule,
};
pub use health::{HealthStatus, HealthSystem, OverallHealth};
#[cfg(feature = "os-patterns")]
pub use health::{ProbeConfig, ProbeResult, ProbeState};
#[cfg(feature = "ecc")]
pub use hnsw_eml::{
    ArmMetrics, DistanceTrainingPoint, EfPrediction, EfStrategy, EfTrainingPoint,
    HnswBenchmarkParams, HnswEmlBenchmark, HnswEmlConfig, HnswEmlManager, HnswEmlStatus,
    HnswScalingPoint, PathPredictBenchmark, PathPrediction, PathTrainingPoint, ProbeReport,
    RebuildPrediction, RebuildTrainingPoint, SearchStrategy, SpectrumForm, TriageRecord,
    probe_corpus, run_hnsw_benchmark, run_hnsw_benchmark_with, run_path_predict_benchmark,
    triage_strategy,
};
#[cfg(feature = "ecc")]
pub use hnsw_service::{
    HnswSearchResult, HnswService, HnswServiceConfig, MultiKey, MultiKeyConfig,
    RELATIONSHIP_HNSW_PREFIX, entity_search_keys, relationship_hnsw_id, relationship_search_keys,
};
#[cfg(feature = "ecc")]
pub use impulse::{ImpulseQueue, ImpulseType};
pub use ipc::{
    CompositePid, ExitReason as SignalExitReason, GlobalPid, KernelIpc, KernelMessage, KernelSignal,
    MeshNodeId, MessagePayload, MessageTarget, ProcessDown as SignalProcessDown,
};
#[cfg(feature = "mesh")]
pub use mesh::{
    MAX_MESSAGE_SIZE, MeshError, MeshPeer, MeshStream, MeshTransport, TransportListener,
    WeftHandshake,
};
#[cfg(feature = "mesh")]
pub use mesh_clock::{Clock, MockClock, MonoTime, RealClock};
#[cfg(all(feature = "mesh", feature = "ecc", feature = "exochain", feature = "native"))]
pub use mesh_artifact::{ArtifactExchange, ExchangeConfig, ExchangeError};
#[cfg(all(feature = "mesh", feature = "ecc", feature = "exochain", feature = "native"))]
pub use mesh_artifact_pkg::{ExchangedPackage, PackageExchangeError};
#[cfg(all(feature = "mesh", feature = "ecc", feature = "exochain", feature = "native"))]
pub use mesh_artifact_transfer::{
    FetchError, FetchOutcome, PeerLink, PeerSet, PieceScheduler, SequentialScheduler, ServeStats,
};
#[cfg(all(feature = "mesh", feature = "ecc", feature = "exochain", feature = "native"))]
pub use mesh_artifact_wire::{
    ArtifactDescriptor, ArtifactId, ArtifactKey, ArtifactMsg, Bitfield, WireError,
};
#[cfg(feature = "mesh")]
pub use mesh_assess::{AssessmentEnvelope, AssessmentTransport};
#[cfg(feature = "mesh")]
pub use mesh_bootstrap::{BootstrapDiscovery, PeerExchangeDiscovery};
#[cfg(feature = "mesh")]
pub use mesh_chain::{
    ChainBridgeEvent, ChainForkStatus, ChainSyncRequest, ChainSyncResponse, SyncStateDigest,
};
#[cfg(feature = "mesh")]
pub use mesh_dedup::DedupFilter;
#[cfg(feature = "mesh")]
pub use mesh_discovery::{
    DiscoveredPeer, DiscoveryBackend, DiscoveryCoordinator, DiscoveryError, DiscoverySource,
    MeshPeerEvent, MeshPeerEventBus,
};
#[cfg(feature = "mesh")]
pub use mesh_framing::{Frame, FrameType, MeshFrame, MsgType};
#[cfg(feature = "mesh")]
pub use mesh_heartbeat::{
    HeartbeatConfig, HeartbeatState, HeartbeatTracker, PeerHeartbeat, PingRequest, PingResponse,
};
#[cfg(feature = "mesh")]
pub use mesh_ipc::{MeshIpcEnvelope, MeshIpcError, Scope as MeshScope};
#[cfg(feature = "mesh")]
pub use mesh_delivery::LocalDelivery;
#[cfg(feature = "mesh")]
pub use mesh_mode::MeshMode;
#[cfg(feature = "mesh")]
pub use mesh_kad::{
    ALPHA, DhtEntry, DhtKey, K_BUCKET_SIZE, KEY_BITS, KademliaDiscovery, KademliaTable,
    NamespacedDhtKey, bucket_index, leading_zeros, xor_distance,
};
#[cfg(feature = "mesh")]
pub use mesh_listener::{JoinRequest, JoinResponse, MeshConnectionPool, PeerInfo};
#[cfg(feature = "mesh")]
pub use mesh_log::{LogAggregator, LogQuery as MeshLogQuery, RemoteLogEntry};
#[cfg(feature = "mesh")]
pub use mesh_mdns::{MdnsAnnouncement, MdnsDiscovery, WEFTOS_SERVICE_NAME};
#[cfg(feature = "mesh")]
pub use mesh_noise::{EncryptedChannel, EncryptedPeer, NoiseConfig, NoisePattern};
#[cfg(feature = "mesh")]
pub use mesh_process::{
    ConsensusEntry, ConsensusOp, ConsensusRole, ConsistentHashRing, CrdtGossipState,
    DistributedProcessTable, MetadataConsensus, ProcessAdvertisement, ProcessStatus,
    ResourceSummary,
};
#[cfg(feature = "mesh")]
pub use mesh_runtime::{DiscoveryState, MeshRuntime, PeerConnection};
#[cfg(feature = "mesh")]
pub use mesh_service::{
    // Note: `ServiceEndpoint` alias lives on `mesh_service` only — crate root
    // already exports `service::ServiceEndpoint` (local kernel service API).
    RemoteServiceEndpoint, ServiceResolutionCache, ServiceResolveRequest, ServiceResolveResponse,
};
#[cfg(feature = "mesh")]
pub use mesh_system_service::MeshService;
#[cfg(feature = "mesh")]
pub use mesh_service_adv::{ClusterServiceRegistry, ServiceAdvertisement};
#[cfg(feature = "mesh")]
pub use mesh_tcp::TcpTransport;
#[cfg(feature = "mesh")]
pub use mesh_tree::{
    MerkleProof, TreeDiffType, TreeNodeDiff, TreeSyncAction, TreeSyncRequest, TreeSyncResponse,
};
#[cfg(feature = "mesh")]
pub use mesh_ws::WsTransport;
#[cfg(all(feature = "mesh", feature = "quic"))]
pub use mesh_quic::QuicTransport;
#[cfg(feature = "os-patterns")]
pub use monitor::{ExitReason, MonitorRegistry, ProcessDown, ProcessLink, ProcessMonitor};
#[cfg(feature = "native")]
pub use node_registry::{
    DerivedGrantError, DerivedWriteGrant, GrantScope, MESH_CANONICAL_PREFIX, NodeRegistry,
    RegisteredNode, node_publish_payload, path_belongs_to, required_path_prefix,
};
pub use node_id::{is_node_id, node_id_from_pubkey};
#[cfg(any(feature = "mesh", feature = "exochain"))]
pub use node_key::{NODE_KEY_FILE, NodeKeyError, load_or_generate_key_file, load_or_generate_node_key};
#[cfg(feature = "ecc")]
pub use persistence::PersistenceConfig;
pub use process::{Pid, ProcessEntry, ProcessState, ProcessTable, ResourceUsage};
#[cfg(feature = "ecc")]
pub use profile_store::{ProfileEntry, ProfileError, ProfileMeta, ProfileStore};
#[cfg(feature = "ecc")]
pub use quantum_backend::{
    BackendStatus, EvolutionParams, JobHandle, JobStatus, QuantumBackend, QuantumError,
    QuantumResults,
};
#[cfg(all(feature = "ecc", feature = "quantum-braket"))]
pub use quantum_braket::{BraketBackend, BraketConfig, BraketDevice};
#[cfg(all(feature = "ecc", feature = "quantum-pasqal"))]
pub use quantum_pasqal::{PasqalBackend, PasqalConfig, PasqalDevice};
#[cfg(feature = "ecc")]
pub use quantum_register::{
    LayoutMethod, RegisterConstraints, build_register, build_register_with,
};
#[cfg(feature = "ecc")]
pub use quantum_state::{
    Complex, Hypothesis, HypothesisSuperposition, QuantumCognitiveState, QuantumEvidenceRanking,
};
#[cfg(feature = "os-patterns")]
pub use reconciler::{DesiredAgentState, DriftEvent, ReconciliationController};
pub use revocation::{
    RevocationError, RevocationKind, RevocationList, RevokedHost, RevokedSubject,
};
pub use service::{
    KernelServiceApi, McpAdapter, ServiceApi, ServiceAuditLevel, ServiceContract, ServiceEndpoint,
    ServiceEntry, ServiceInfo, ServiceRegistry, ServiceType, ShellAdapter, SystemService,
};
#[cfg(all(feature = "native", feature = "exochain"))]
pub use stream_anchor::{StreamWindowAnchor, TopicAnchor, topic_matches};
#[cfg(feature = "native")]
pub use substrate_service::{
    AclDenialEvent, EgressDenied, GateDenied, Sensitivity as SubstrateSensitivity,
    SubstrateListEntry, SubstrateListSnapshot, SubstrateReadSnapshot, SubstrateService,
};
pub use supervisor::{AgentSupervisor, EnclaveConfig, SpawnBackend, SpawnRequest, SpawnResult};
#[cfg(feature = "os-patterns")]
pub use supervisor::{
    ResourceCheckResult, RestartBudget, RestartStrategy, RestartTracker, check_resource_usage,
};
#[cfg(feature = "ecc")]
pub use talk_loop::{TalkModeConfig, TalkModeLoop, TalkTickResult};
#[cfg(feature = "exochain")]
pub use tree_manager::{TreeManager, TreeStats};
pub use tree_view::{AgentTreeView, TreeScope};
#[cfg(feature = "ecc")]
pub use vector_backend::{
    SearchResult as VectorSearchResult, VectorBackend, VectorBackendKind, VectorError, VectorResult,
};
#[cfg(feature = "ecc")]
pub use vector_diskann::{DiskAnnBackend, DiskAnnConfig};
#[cfg(feature = "ecc")]
pub use vector_hnsw::HnswBackend;
#[cfg(feature = "ecc")]
pub use vector_hybrid::{EvictionPolicy, HybridBackend, HybridConfig};
#[cfg(feature = "ecc")]
pub use view_resolver::{SingleViewResolver, ViewResolver};
#[cfg(feature = "ecc")]
pub use weaver::{
    ConfidenceGap, ConfidenceReport, DataSource, ExportedModel, IngestResult, MetaDecisionType,
    MetaLoomEvent, ModelingSession, ModelingSuggestion, StrategyPattern, TickResult, WeaverCommand,
    WeaverEngine, WeaverError, WeaverKnowledgeBase, WeaverResponse,
};
// ── 08b re-exports ──────────────────────────────────────────────
#[cfg(feature = "os-patterns")]
pub use dead_letter::{DeadLetter, DeadLetterQueue, DeadLetterReason};
#[cfg(feature = "os-patterns")]
pub use log_service::{LogEntry, LogQuery, LogService};
#[cfg(feature = "os-patterns")]
pub use metrics::{
    Histogram, METRIC_ACTIVE_AGENTS, METRIC_ACTIVE_SERVICES, METRIC_AGENT_CRASHES,
    METRIC_AGENT_SPAWNS, METRIC_CHAIN_LENGTH, METRIC_GOVERNANCE_EVAL_MS, METRIC_IPC_LATENCY_MS,
    METRIC_MESSAGES_DELIVERED, METRIC_MESSAGES_DROPPED, METRIC_MESSAGES_SENT,
    METRIC_TOOL_EXECUTION_MS, METRIC_TOOL_EXECUTIONS, MetricSnapshot, MetricsRegistry,
};
#[cfg(feature = "os-patterns")]
pub use named_pipe::{NamedPipe, NamedPipeRegistry, PipeInfo};
#[cfg(feature = "os-patterns")]
pub use reliable_queue::{DeliveryResult, PendingDelivery, ReliableConfig, ReliableQueue};
#[cfg(feature = "os-patterns")]
pub use timer::{TimerEntry, TimerInfo, TimerService};
pub use topic::{SubscriberId, SubscriberSink, Subscription, TopicRouter};
#[cfg(feature = "native")]
pub use wasm_runner::AgentSendTool;
pub use wasm_runner::{
    AgentInspectTool, AgentListTool, AgentResumeTool, AgentSpawnTool, AgentStopTool,
    AgentSuspendTool, BackendSelection, BuiltinTool, BuiltinToolSpec, Certificate,
    CompiledModuleCache, DeployedTool, FsCopyTool, FsCreateDirTool, FsExistsTool, FsGlobTool,
    FsMoveTool, FsReadDirTool, FsReadFileTool, FsRemoveTool, FsStatTool, FsWriteFileTool,
    IpcSendTool, IpcSubscribeTool, SandboxConfig, ShellPipeline, SysCronAddTool, SysCronListTool,
    SysCronRemoveTool, SysEnvGetTool, SysServiceHealthTool, SysServiceListTool, ToolCategory,
    ToolError, ToolRegistry, ToolSigningAuthority, ToolVersion, WasiFsScope, WasmError,
    WasmSandboxConfig, WasmTool, WasmToolResult, WasmToolRunner, WasmValidation,
    builtin_tool_catalog, compute_module_hash, verify_tool_signature,
};
#[cfg(feature = "exochain")]
pub use wasm_runner::{SysChainQueryTool, SysChainStatusTool, SysTreeInspectTool, SysTreeReadTool};
