//! The [`WorkloadRuntime`] trait and the values that cross it
//! (ADR-099 section 5).

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use clawft_types::placement::Capability;
use serde::{Deserialize, Serialize};

use super::cog_spec::CogSpec;
use super::evidence::RunEvidence;
use super::host_contract::HostContract;
use crate::workload_governance::NetworkPolicy;

/// Whether the layer controls an instance or only observes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlMode {
    /// The adapter starts, stops and unloads instances.
    Managed,
    /// The adapter registers and health-checks something it did not start.
    Adopted,
}

/// How a cog runs (ADR-100 section 4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "mode")]
pub enum RunMode {
    /// One cycle (`--once`), then exit.
    Once,
    /// Loop with `--interval <secs>`.
    Interval {
        /// Seconds between cycles (1..=3600).
        secs: u32,
    },
    /// Long-running listener; no mode flag.
    Listener,
}

impl RunMode {
    /// Mode flags appended to the argv.
    pub fn args(&self) -> Vec<String> {
        match self {
            RunMode::Once => vec!["--once".into()],
            RunMode::Interval { secs } => vec!["--interval".into(), secs.to_string()],
            RunMode::Listener => Vec::new(),
        }
    }

    /// Boundary validation.
    pub fn validate(&self) -> Result<(), RuntimeError> {
        match self {
            RunMode::Interval { secs } if !(1..=3600).contains(secs) => Err(
                RuntimeError::InvalidConfig("interval must be 1..=3600 seconds".into()),
            ),
            _ => Ok(()),
        }
    }
}

/// Where a workload's payload came from.
#[derive(Debug, Clone)]
pub enum WorkloadSource {
    /// Our own signed package, verified against pinned anchors
    /// (native and container adapters).
    SignedPackage(Box<SignedPayload>),
    /// A cog in a device's own store (Seed adapter). Accepted only when the
    /// operator pinned it by id and version.
    StorePin {
        /// Store name (`cognitum`).
        registry: String,
        /// Optional expected SHA-256 of the store binary.
        sha256: Option<String>,
    },
    /// A project workload, authorised by a project certificate rather than
    /// a signed package. Type only: no adapter accepts it yet.
    Project(ProjectPayload),
    /// An inference endpoint (ADR-101): adopted or managed by an
    /// `infer.*` adapter. Authorised by the operator's request and the
    /// operator-attested model manifest, not by a signed package.
    Inference(Box<InferencePayload>),
}

/// The spec of a [`WorkloadSource::Inference`] workload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InferencePayload {
    /// What to adopt or run.
    pub spec: super::infer::InferenceSpec,
}

/// Identity of the project a [`WorkloadSource::Project`] workload belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectPayload {
    /// Explicit adapter selected by the certificate-verifying supervisor.
    pub adapter: String,
    /// Project id.
    pub project_id: String,
    /// Key id of the project signing key.
    pub key_id: String,
    /// Serial of the project certificate.
    pub cert_serial: u64,
    /// Key id of the user key that issued the certificate.
    pub user_key_id: String,
    /// Hash (hex) of the project policy in force.
    pub policy_hash: String,
    /// The project root the child kernel runs in (canonical, from the
    /// manifest). The `logical` adapter refuses admission when it is gone.
    pub root: std::path::PathBuf,
}

/// The verified payload of a signed package.
#[derive(Debug, Clone)]
pub struct SignedPayload {
    /// Package id (BLAKE3 of the signed statement).
    pub package_id: String,
    /// Hex Ed25519 public keys of accepted signers.
    pub signer_keys: Vec<String>,
    /// Parsed `cog.toml` subset.
    pub spec: CogSpec,
    /// Binaries by arch (`aarch64`, `armv7`, ...), each re-checked against
    /// its pinned BLAKE3 when the workload was built.
    pub binaries: BTreeMap<String, Binary>,
    /// BLAKE3 of the manifest and every file (for revocation refs).
    pub artifact_hashes: Vec<String>,
}

/// One verified binary.
#[derive(Debug, Clone)]
pub struct Binary {
    /// BLAKE3 hex (its artifact id).
    pub blake3: String,
    /// Content.
    pub bytes: std::sync::Arc<Vec<u8>>,
}

/// A workload an adapter may run. Built only from a verified package
/// ([`VerifiedWorkload::from_package`]) or an operator store pin
/// ([`VerifiedWorkload::store_pin`]).
#[derive(Debug, Clone)]
pub struct VerifiedWorkload {
    /// Workload kind (`cog`).
    pub kind: String,
    /// Workload id (cog id).
    pub id: String,
    /// Version.
    pub version: String,
    /// Payload provenance.
    pub source: WorkloadSource,
}

/// Place-time configuration for one instance.
#[derive(Debug, Clone)]
pub struct WorkloadConfig {
    /// Run mode.
    pub mode: RunMode,
    /// Extra cog arguments (validated against the cog's `[config]`
    /// `cli_arg`s before use).
    pub args: Vec<String>,
    /// Host contract (feed bind, sensor URL, token, data dir).
    pub host: HostContract,
    /// Instance id suffix; the adapter derives the full id.
    pub node_id: String,
}

/// Result of an admission self-check.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Admission {
    /// Adapter id.
    pub runtime: String,
    /// Arch the adapter will run (`aarch64`, `armv7`).
    pub arch: String,
    /// Whether it runs emulated.
    pub emulated: bool,
    /// Human-readable notes (what was checked).
    pub notes: Vec<String>,
}

/// Opaque handle to a loaded instance.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct InstanceHandle {
    /// Adapter id that owns it.
    pub runtime: String,
    /// Instance id: `<workload id>-<config hash prefix>-<node id>`.
    pub instance_id: String,
    /// Workload id.
    pub workload_id: String,
    /// Whether `load` installed the payload into a device store (Seed
    /// store path). False when it was already installed, and for adapters
    /// that stage rather than install.
    #[serde(default)]
    pub store_installed: bool,
}

/// A running instance that must stop before a console run of another
/// (sensor feed contention). The host gates each as `workload.stop`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Preemption {
    /// Store registry the running cog came from.
    pub registry: String,
    /// Cog id.
    pub workload_id: String,
    /// Installed version.
    pub version: String,
}

/// Lifecycle state of an instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstanceState {
    /// Loaded, not running.
    Loaded,
    /// Running.
    Running,
    /// Exited (finished a once-run or stopped).
    Exited,
    /// Running but unhealthy, or loading.
    Degraded,
    /// Not known to the adapter.
    Unknown,
}

/// Status report, including kind-specific health detail.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstanceStatus {
    /// State.
    pub state: InstanceState,
    /// Exit code when exited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Adapter-specific detail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl InstanceStatus {
    /// Status with only a state.
    pub fn of(state: InstanceState) -> Self {
        Self {
            state,
            exit_code: None,
            detail: None,
        }
    }
}

/// How the link to a remote device is authenticated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkSecurity {
    /// `https://` with an operator-pinned certificate or public key.
    Pinned,
    /// Not pinned (plain http, or https verified only by WebPKI), accepted
    /// because the operator set the explicit lab-link opt-in.
    LabOptIn,
    /// Not pinned and no opt-in.
    Unpinned,
}

impl LinkSecurity {
    /// Whether a remote.api node may be bound or placed on over this link.
    /// An opt-in link is allowed with a loud log; an unpinned one is not.
    pub fn require_pinned(self, node: &str) -> Result<(), String> {
        match self {
            Self::Pinned => Ok(()),
            Self::LabOptIn => {
                tracing::warn!(
                    node,
                    "UNPINNED lab link accepted by operator opt-in: the Seed's identity is not authenticated on this link"
                );
                Ok(())
            }
            Self::Unpinned => Err(format!(
                "{node}: a remote.api node needs a pinned transport (https with a certificate or \
                 public-key pin); set allow_unpinned_lab_link only for a USB or lab link"
            )),
        }
    }
}

/// Adapter failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RuntimeError {
    /// The admission self-check refused the workload on this node.
    #[error("admission refused: {0}")]
    AdmissionRefused(String),
    /// Bad configuration or arguments.
    #[error("invalid config: {0}")]
    InvalidConfig(String),
    /// Unknown instance handle.
    #[error("unknown instance {0}")]
    UnknownInstance(String),
    /// The device no longer holds a cog the operator (or this adapter) put
    /// there: it was removed out of band.
    #[error("{0} is not installed on the Seed")]
    NotInstalled(String),
    /// Wrong lifecycle state for the operation.
    #[error("invalid state: {0}")]
    InvalidState(String),
    /// Governance denied or deferred the transition.
    #[error("governance: {0}")]
    Governance(String),
    /// The backend (process, engine CLI, remote API) failed.
    #[error("backend: {0}")]
    Backend(String),
    /// The operation is not supported by this adapter.
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// A device-store install succeeded but `load` could not finish (the
    /// auto-started cog would not stop). With `rolled_back` the adapter
    /// uninstalled it again; otherwise the cog is still on the device and
    /// `handle` stays loaded so an operator can unload it.
    #[error(
        "store install of {} then failed ({}): {reason}",
        handle.workload_id,
        if *rolled_back { "rolled back" } else { "stranded on the device" }
    )]
    StrandedInstall {
        /// The instance the install created.
        handle: Box<InstanceHandle>,
        /// Whether the install was undone.
        rolled_back: bool,
        /// What failed.
        reason: String,
    },
}

/// Stable code for chain payloads.
impl RuntimeError {
    /// Machine-readable code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::AdmissionRefused(_) => "admission-refused",
            Self::InvalidConfig(_) => "invalid-config",
            Self::UnknownInstance(_) => "unknown-instance",
            Self::NotInstalled(_) => "not-installed",
            Self::InvalidState(_) => "invalid-state",
            Self::Governance(_) => "governance",
            Self::Backend(_) => "backend",
            Self::Unsupported(_) => "unsupported",
            Self::StrandedInstall { .. } => "stranded-install",
        }
    }
}

/// A runtime adapter (ADR-099 section 5).
///
/// Adapters never make governance decisions; [`super::WorkloadHost`] gates
/// and chains every transition around them.
#[async_trait]
pub trait WorkloadRuntime: Send + Sync {
    /// Adapter id (`native`, `container.docker`, `remote.api.cognitum-seed`).
    fn id(&self) -> &str;
    /// Capabilities a node advertises when this adapter works.
    fn provides(&self) -> Vec<Capability>;
    /// Self-check: can this node run `w`? Reserves nothing it cannot undo.
    async fn admit(&self, w: &VerifiedWorkload) -> Result<Admission, RuntimeError>;
    /// Stage the payload and config; returns a handle for the instance.
    async fn load(
        &self,
        w: &VerifiedWorkload,
        cfg: &WorkloadConfig,
    ) -> Result<InstanceHandle, RuntimeError>;
    /// Start the instance.
    async fn start(&self, h: &InstanceHandle) -> Result<(), RuntimeError>;
    /// Stop it (terminate, then kill after `grace`) and return evidence.
    async fn stop(&self, h: &InstanceHandle, grace: Duration) -> Result<RunEvidence, RuntimeError>;
    /// Remove staged payload and state.
    async fn unload(&self, h: InstanceHandle) -> Result<(), RuntimeError>;
    /// Current status.
    async fn status(&self, h: &InstanceHandle) -> InstanceStatus;
    /// Managed or adopted.
    fn control_mode(&self) -> ControlMode;
    /// The file an instance's binary was staged to, when the adapter runs it
    /// from disk (the licence run gate rehashes it at each start). `None`:
    /// not run from a file this node can read (a container image is tagged
    /// by the binary's BLAKE3; a remote device holds its own copy).
    async fn staged_payload(&self, _h: &InstanceHandle) -> Option<std::path::PathBuf> {
        None
    }
    /// How the link to a remote device is authenticated. Fails closed:
    /// only a remote adapter with a pinned transport says `Pinned`.
    fn link_security(&self) -> LinkSecurity {
        LinkSecurity::Unpinned
    }
    /// Network exposure instances of this adapter actually get, reported
    /// to the gate as the request's `network` (never assumed).
    fn network_exposure(&self) -> NetworkPolicy;
    /// Re-attach an instance this adapter loaded before the controller
    /// restarted (its in-memory table is gone but the device still holds
    /// it). The adapter verifies the instance is still there and still the
    /// pinned version, so `stop` / `unload` work again; it starts and
    /// changes nothing on the device.
    async fn adopt(&self, h: &InstanceHandle, _w: &VerifiedWorkload) -> Result<(), RuntimeError> {
        Err(RuntimeError::Unsupported(format!(
            "{} cannot re-adopt {}",
            self.id(),
            h.instance_id
        )))
    }
    /// Instances that must stop before a console run of `h`. The host
    /// gates each one as `workload.stop` before calling [`Self::preempt`].
    async fn console_preemptions(
        &self,
        _h: &InstanceHandle,
    ) -> Result<Vec<Preemption>, RuntimeError> {
        Ok(Vec::new())
    }
    /// Stop one preempted instance.
    async fn preempt(&self, p: &Preemption) -> Result<(), RuntimeError> {
        Err(RuntimeError::Unsupported(format!(
            "{} cannot preempt {}",
            self.id(),
            p.workload_id
        )))
    }
    /// Restart one preempted instance after the console run.
    async fn resume(&self, p: &Preemption) -> Result<(), RuntimeError> {
        Err(RuntimeError::Unsupported(format!(
            "{} cannot resume {}",
            self.id(),
            p.workload_id
        )))
    }
    /// Run one console cycle (`[console]` limits apply). `command` must be
    /// one of the cog's `[console].allowed_commands`.
    async fn console(&self, h: &InstanceHandle, command: &str)
    -> Result<RunEvidence, RuntimeError>;
}
