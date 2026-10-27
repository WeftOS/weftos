//! WeftOS: A portable AI kernel for any project.
//!
//! Add WeftOS to your project to get process management, mesh networking,
//! capability-based security, an append-only audit chain, and a cognitive
//! substrate that learns your codebase.
//!
//! # Quick Start
//!
//! ```rust,no_run
//! use weftos::WeftOs;
//!
//! #[tokio::main]
//! async fn main() {
//!     let os = WeftOs::boot_default().await.unwrap();
//!     println!("WeftOS running: {} services", os.service_count());
//!     os.shutdown().await.unwrap();
//! }
//! ```
//!
//! # Feature Flags
//!
//! - `native` (default) -- Tokio runtime, native file I/O
//! - `exochain` -- Append-only hash chain with Ed25519 + ML-DSA-65 signing
//! - `cluster` -- Multi-node clustering via ruvector
//! - `mesh` -- Encrypted peer-to-peer mesh networking
//! - `ecc` -- Ephemeral Causal Cognition (causal DAG, HNSW, cognitive tick)
//! - `wasm-sandbox` -- Wasmtime-based tool execution
//! - `containers` -- Docker/Podman sidecar orchestration
//! - `os-patterns` -- Self-healing, metrics, reliable IPC, timers
//! - `full` -- Everything
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
//! Source: <https://github.com/WeftOS/weftos>

pub mod init;

// Re-export the kernel under the weftos namespace
pub use clawft_kernel as kernel;

// Re-export key types at the top level for ergonomic API
pub use clawft_kernel::{
    // Capabilities
    AgentCapabilities,
    // Agent supervision
    AgentSupervisor,
    // Apps
    AppManager,
    AppManifest,
    AppState,
    // Console
    BootEvent,
    BootLog,
    BootPhase,
    CapabilityChecker,
    // Cluster
    ClusterConfig,
    ClusterMembership,
    ContainerConfig,
    // Containers
    ContainerManager,
    ContainerState,
    // Cron
    CronService,
    EffectVector,
    GlobalPid,
    GovernanceDecision,
    // Governance
    GovernanceEngine,
    GovernanceRequest,
    GovernanceRule,
    HealthStatus,
    // Health
    HealthSystem,
    InstalledApp,
    IpcScope,
    Kernel,
    // Config
    KernelConfigExt,
    // Error
    KernelError,
    // IPC
    KernelIpc,
    KernelMessage,
    KernelResult,
    KernelSignal,
    KernelState,
    MessagePayload,
    MessageTarget,
    NodeState,
    OverallHealth,
    PeerNode,
    // Process management
    Pid,
    ProcessEntry,
    ProcessState,
    ProcessTable,
    ResourceLimits,
    ServiceEntry,
    // Services
    ServiceRegistry,
    SpawnBackend,
    SpawnRequest,
    SpawnResult,
    Subscription,
    SystemService,
    // Topics
    TopicRouter,
};

// Conditional re-exports
#[cfg(feature = "exochain")]
pub use clawft_kernel::{
    CapabilityGate, ChainEvent, ChainManager, GateBackend, GateDecision, GovernanceGate,
    TreeManager, TreeStats,
};

#[cfg(feature = "ecc")]
pub use clawft_kernel::{
    CausalEdgeType, CausalGraph, CognitiveTick, CognitiveTickConfig, CrossRef, CrossRefStore,
    CrossRefType, EccCalibration, HnswSearchResult, HnswService, HnswServiceConfig, ImpulseQueue,
    ImpulseType, UniversalNodeId,
};

#[cfg(feature = "mesh")]
pub use clawft_kernel::{
    BootstrapDiscovery, ClusterServiceRegistry, DedupFilter, DiscoveryCoordinator,
    DistributedProcessTable, HeartbeatConfig, HeartbeatState, HeartbeatTracker, MeshConnectionPool,
    MeshError, MeshIpcEnvelope, MeshPeer, MeshStream, MeshTransport, TcpTransport,
    TransportListener, WeftHandshake,
};

#[cfg(feature = "os-patterns")]
pub use clawft_kernel::{
    DeadLetterQueue, LogService, MetricsRegistry, NamedPipeRegistry, ReconciliationController,
    ReliableQueue, TimerService,
};

use std::path::Path;

#[cfg(feature = "native")]
use std::sync::Arc;

#[cfg(feature = "native")]
use clawft_platform::NativePlatform;

/// The main WeftOS instance -- boots and manages the kernel.
#[cfg(feature = "native")]
pub struct WeftOs {
    kernel: clawft_kernel::Kernel<NativePlatform>,
    project_root: std::path::PathBuf,
}

#[cfg(feature = "native")]
impl WeftOs {
    /// Boot WeftOS with default configuration.
    pub async fn boot_default() -> Result<Self, KernelError> {
        Self::boot_in(std::env::current_dir().unwrap_or_else(|_| ".".into())).await
    }

    /// Boot WeftOS in a specific project directory.
    pub async fn boot_in(project_root: impl Into<std::path::PathBuf>) -> Result<Self, KernelError> {
        let project_root = project_root.into();
        let config = clawft_types::config::Config::default();
        let kernel_config = clawft_types::config::KernelConfig::default();
        let platform = Arc::new(NativePlatform::new());

        let kernel = Kernel::boot(config, kernel_config, platform).await?;

        Ok(Self {
            kernel,
            project_root,
        })
    }

    /// Boot WeftOS with custom configuration.
    pub async fn boot_with(
        config: clawft_types::config::Config,
        kernel_config: clawft_types::config::KernelConfig,
        project_root: impl Into<std::path::PathBuf>,
    ) -> Result<Self, KernelError> {
        let project_root = project_root.into();
        let platform = Arc::new(NativePlatform::new());
        let kernel = Kernel::boot(config, kernel_config, platform).await?;
        Ok(Self {
            kernel,
            project_root,
        })
    }

    /// Get the project root directory.
    pub fn project_root(&self) -> &Path {
        &self.project_root
    }

    /// Get the kernel state.
    pub fn state(&self) -> &KernelState {
        self.kernel.state()
    }

    /// Get the number of registered services.
    pub fn service_count(&self) -> usize {
        self.kernel.services().len()
    }

    /// Get the number of active processes.
    pub fn process_count(&self) -> usize {
        self.kernel.process_table().len()
    }

    /// Get a reference to the underlying kernel.
    pub fn kernel(&self) -> &Kernel<NativePlatform> {
        &self.kernel
    }

    /// Get a mutable reference to the underlying kernel.
    pub fn kernel_mut(&mut self) -> &mut Kernel<NativePlatform> {
        &mut self.kernel
    }

    /// Shut down WeftOS gracefully.
    pub async fn shutdown(mut self) -> KernelResult<()> {
        tracing::info!("WeftOS shutting down");
        self.kernel.shutdown().await
    }
}

/// Version information.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Check if WeftOS is initialized in the given directory.
pub fn is_initialized(path: impl AsRef<Path>) -> bool {
    path.as_ref().join(".weftos").exists() || path.as_ref().join("weave.toml").exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_not_initialized_in_random_dir() {
        assert!(!is_initialized("/tmp/nonexistent-weftos-test"));
    }

    #[test]
    fn init_creates_structure() {
        let dir = std::env::temp_dir().join("weftos-test-init");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let result = init::init_project(&dir).unwrap();
        assert!(result.weftos_dir_created);
        assert!(result.weave_toml_created);
        assert!(dir.join(".weftos").exists());
        assert!(dir.join("weave.toml").exists());
        assert!(dir.join(".weftos/chain").exists());
        assert!(dir.join(".weftos/logs").exists());

        // Cleanup
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn init_detects_rust_project() {
        let dir = std::env::temp_dir().join("weftos-test-rust");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"test\"").unwrap();

        let _result = init::init_project(&dir).unwrap();
        let config = std::fs::read_to_string(dir.join("weave.toml")).unwrap();
        assert!(config.contains("language = \"rust\""));
        assert!(config.contains("*.rs"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn weftos_boots_and_reports_state() {
        // Same defaults as `boot_default`, but the chain lives in a temp
        // dir: shutdown persists the chain and must never touch ~/.clawft.
        let chain_dir =
            std::env::temp_dir().join(format!("weftos-lib-test-chain-{}", std::process::id()));
        let kernel_config = clawft_types::config::KernelConfig {
            chain: Some(clawft_types::config::ChainConfig::isolated_in(&chain_dir)),
            ..clawft_types::config::KernelConfig::default()
        };
        let os = WeftOs::boot_with(
            clawft_types::config::Config::default(),
            kernel_config,
            std::env::current_dir().unwrap(),
        )
        .await
        .unwrap();
        assert!(matches!(os.state(), KernelState::Running));
        assert!(os.service_count() > 0);
        os.shutdown().await.unwrap();
    }
}
