//! The project supervisor: per-project child kernels under the user daemon
//! (ADR-103 A6, Phase 2 package G).
//!
//! The user daemon owns one [`Supervisor`]. A project becomes a running
//! child through the `project` workload kind and the `logical` adapter
//! (`clawft-kernel`), driven by a [`WorkloadHost`] so every transition is
//! gated (only the supervisor principal may run the `project` kind) and
//! chained as `workload.*`. The supervisor adds, around that:
//!
//! * [`child`]: spawn contract (allow-list environment, `spawn.json`,
//!   `--profile project`), terminate (graceful `kernel.shutdown`, then
//!   `SIGTERM`, then `SIGKILL`);
//! * [`restart`]: OTP `one_for_one` plus `Transient`: crashes restart with
//!   1 s..30 s backoff inside a budget, then `failed`; clean exits never do;
//! * [`adopt`]: children that outlived a daemon restart are verified and
//!   adopted, never signalled when unverifiable;
//! * [`idle`]: graceful stop of idle projects.
//!
//! Locking: one async mutex per project serialises start, stop and restart,
//! so concurrent `ensure_running` calls start exactly one child. The state
//! mutex is a plain `std` mutex held only for short, non-awaiting sections.

pub mod adopt;
mod boot;
pub mod child;
mod control;
pub mod container;
pub mod idle;
pub mod io;
mod lifecycle;
mod prepare;
pub mod restart;
pub mod sandbox;
pub mod state;
mod status;
mod types;
mod wasmtime;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use clawft_kernel::gate::GateBackend;
use clawft_kernel::workload_governance::{
    NodeTrustTier, SUPERVISOR_PRINCIPAL, WorkloadGate, project_supervisor_permit,
};
use clawft_kernel::workload_kind::{ProjectFacts, ProjectPrepareError, prepare_project};
use clawft_kernel::workload_runtime::{
    ChildIdentity, ChildLauncher, ChildProbe, ChildRef, HostContract, InstanceHandle,
    LogicalRuntime, RunMode, RuntimeError, VerifiedWorkload, WorkloadConfig, WorkloadHost,
};
use clawft_types::project::{ChildState, ProjectManifest, ProjectSandbox, ServeVia};
use clawft_types::runtime_paths::{LOCK_FILE_NAME, SOCKET_NAME};
use serde_json::{Value, json};

use self::adopt::Found;
use self::child::{ExitInfo, Launcher, LauncherParts};
use self::restart::{Decision, RestartTracker};
use crate::project_cert_rpc::CertEnv;

#[derive(Default)]
struct SlotState {
    state: ChildState,
    handle: Option<InstanceHandle>,
    tracker: Option<RestartTracker>,
    generation: u64,
    last_exit: Option<ExitInfo>,
    failed: Option<String>,
    restarts: u32,
    /// When the liveness pass first saw this `running` child's session
    /// expired (and it has not registered again since).
    expired_since: Option<Instant>,
    /// When the liveness pass first saw this `running` adopted child still
    /// without a registered session (see `Status::unregistered_secs`).
    unregistered_since: Option<Instant>,
}

#[cfg(test)]
mod container_path_tests {
    use super::overlapping_parent_path;
    use std::path::{Path, PathBuf};

    #[test]
    fn writable_project_cannot_contain_or_live_inside_parent_runtime() {
        let protected = vec![PathBuf::from("/home/user/projects/a/.weftos/run")];
        assert!(overlapping_parent_path(Path::new("/home/user/projects/a"), &protected).is_some());
        assert!(
            overlapping_parent_path(
                Path::new("/home/user/projects/a/.weftos/run/child"),
                &protected
            )
            .is_some()
        );
        assert!(overlapping_parent_path(Path::new("/home/user/projects/b"), &protected).is_none());
    }
}

#[derive(Default)]
struct Slot {
    gate: tokio::sync::Mutex<()>,
    st: Mutex<SlotState>,
}

impl Slot {
    fn st(&self) -> std::sync::MutexGuard<'_, SlotState> {
        self.st.lock().expect("supervisor slot state lock poisoned")
    }
}

/// The supervisor.
pub struct Supervisor {
    cfg: Arc<SupervisorConfig>,
    deps: Deps,
    launcher: Arc<Launcher>,
    host: WorkloadHost,
    wasm_host: WorkloadHost,
    slots: Mutex<HashMap<String, Arc<Slot>>>,
    leftovers: Mutex<Vec<Found>>,
}

static GLOBAL: OnceLock<Arc<Supervisor>> = OnceLock::new();

fn overlapping_parent_path(project: &Path, protected_paths: &[PathBuf]) -> Option<PathBuf> {
    protected_paths.iter().find_map(|protected| {
        let protected = protected
            .canonicalize()
            .unwrap_or_else(|_| protected.clone());
        (project.starts_with(&protected) || protected.starts_with(project)).then_some(protected)
    })
}

/// Install `sup` as the process supervisor. `false` when one is installed
/// already (the first wins).
pub fn install_global(sup: Arc<Supervisor>) -> bool {
    GLOBAL.set(sup).is_ok()
}

/// The user daemon's supervisor, once installed ([`post_boot`]).
pub fn global() -> Option<Arc<Supervisor>> {
    GLOBAL.get().cloned()
}

impl Supervisor {
    /// The identity records (project bindings and revocations) as of now;
    /// `None` when they cannot be read. Used to check a project id before a
    /// placement names it (the cog ingest bridge).
    pub fn identity_view(&self) -> Option<clawft_kernel::project_identity::RevocationView> {
        crate::project_cert_rpc::current_view(&self.deps.cert_env).ok()
    }

    /// A supervisor over `cfg` and `deps`.
    pub fn new(cfg: SupervisorConfig, deps: Deps) -> Arc<Self> {
        let cfg = Arc::new(cfg);
        let launcher = Arc::new(Launcher::new(
            Arc::clone(&cfg),
            LauncherParts {
                user_key: deps.cert_env.user_key.clone(),
                snapshot: Arc::clone(&deps.snapshot),
                tokens: deps.tokens.clone(),
                io: Arc::clone(&deps.io),
            },
        ));
        // A `project` workload is authorised by a project certificate, and
        // the certificate's revocation is the project identity record
        // (`project_identity`), not the subject list: hence `exempt`.
        const WHY: &str = "project certificates revoke through project_identity";
        let gate: Arc<dyn GateBackend> = deps.gate.clone().unwrap_or_else(|| {
            Arc::new(
                WorkloadGate::exempt(0.95, false, WHY)
                    .with_permit(project_supervisor_permit())
                    .unwrap_or_else(|_| WorkloadGate::exempt(0.95, false, WHY))
                    .with_chain(Arc::clone(&deps.cert_env.chain)),
            )
        });
        let runtime = Arc::new(LogicalRuntime::new(
            Arc::clone(&launcher) as Arc<dyn ChildLauncher>
        ));
        let host = WorkloadHost::new(runtime, Arc::clone(&gate), SUPERVISOR_PRINCIPAL, NodeTrustTier::Paired)
            .with_chain(Arc::clone(&deps.cert_env.chain));
        let wasm_runtime = Arc::new(LogicalRuntime::wasmtime_project(
            Arc::clone(&launcher) as Arc<dyn ChildLauncher>
        ));
        let wasm_host = WorkloadHost::new(wasm_runtime, gate, SUPERVISOR_PRINCIPAL, NodeTrustTier::Paired)
            .with_chain(Arc::clone(&deps.cert_env.chain));
        Arc::new(Self {
            cfg,
            deps,
            launcher,
            host,
            wasm_host,
            slots: Mutex::new(HashMap::new()),
            leftovers: Mutex::new(Vec::new()),
        })
    }

    /// The config.
    pub fn config(&self) -> &SupervisorConfig {
        &self.cfg
    }

    /// The launcher (token refresh, tests).
    pub fn launcher(&self) -> &Arc<Launcher> {
        &self.launcher
    }

    /// `<run_root>/<id>` (`runtime_paths::child_run_dir`; ids are validated
    /// before they get here).
    pub fn run_dir(&self, id: &str) -> PathBuf {
        clawft_types::runtime_paths::child_run_dir(&self.cfg.run_root, id)
            .unwrap_or_else(|| self.cfg.run_root.join(id))
    }

    fn socket(&self, id: &str) -> PathBuf {
        state::read(&self.run_dir(id))
            .and_then(|s| s.container.map(|c| c.host_socket))
            .unwrap_or_else(|| self.run_dir(id).join(SOCKET_NAME))
    }

    fn slot(&self, id: &str) -> Arc<Slot> {
        let mut m = self.slots.lock().expect("supervisor slots lock poisoned");
        Arc::clone(m.entry(id.to_owned()).or_default())
    }

    fn chain(&self, kind: &str, payload: Value) {
        self.deps
            .cert_env
            .chain
            .append(CHAIN_SOURCE, kind, Some(payload));
    }

    /// Record a master-approved nested registration in the user chain.
    pub(crate) fn record_nested_registration(&self, master: &str, child: &str) {
        self.chain(
            "project.nested.register",
            json!({
                "master_project_id": master,
                "child_project_id": child,
                "registration_level": "isolated",
            }),
        );
    }

    fn tracker_for(&self, m: &ProjectManifest) -> RestartTracker {
        let serve = m.serve.clone().unwrap_or_default();
        RestartTracker::new(
            serve.restart_max(),
            Duration::from_secs(serve.restart_window_secs()),
            self.cfg.backoff_initial,
            self.cfg.backoff_max,
        )
    }

    fn host_cfg(id: &str) -> WorkloadConfig {
        WorkloadConfig {
            mode: RunMode::Listener,
            args: Vec::new(),
            host: HostContract::default_feed(),
            node_id: id.to_owned(),
        }
    }

    fn set_state(&self, id: &str, slot: &Slot, state: ChildState) {
        let (restarts, exit, failed) = {
            let mut st = slot.st();
            st.state = state;
            (st.restarts, st.last_exit, st.failed.clone())
        };
        state::update(&self.run_dir(id), |s| {
            s.state = state;
            s.restarts = restarts;
            s.last_exit_code = exit.and_then(|e| e.code);
            s.failed_reason = failed;
            if matches!(state, ChildState::Stopped | ChildState::Failed) {
                s.pid = None;
            }
        });
    }

    async fn probe_running(&self, id: &str) -> Option<u32> {
        match self.launcher.probe(id).await {
            ChildProbe::Running { identity } => Some(identity.host_pid()),
            _ => None,
        }
    }

}

pub use boot::post_boot;
pub use types::{CHAIN_SOURCE, Deps, MAX_SOCKET_PATH, Running, Status, SupError, SupervisorConfig};
