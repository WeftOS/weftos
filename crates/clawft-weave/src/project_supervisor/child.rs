//! Launching, probing and terminating one project kernel (ADR-103 A6,
//! Phase 2 package G).
//!
//! [`Launcher`] implements the kernel's [`ChildLauncher`], so the `logical`
//! adapter reaches the OS only through it. It does not use
//! `workload_runtime::supervise::Supervised`: that captures stdout and
//! stderr through pipes, and a child whose supervisor (the user daemon) is
//! restarted would die of `SIGPIPE` on its next log line. Children log to
//! `<run>/<id>/kernel.log` instead, in their own process group, and survive
//! a daemon restart to be adopted.
//!
//! Spawn contract (hard requirements from the Phase 2 reviews):
//!
//! * the child gets an **allow-list** environment after `env_clear()`;
//!   provider keys and everything else in the daemon's environment never
//!   reach it ([`child_env`]);
//! * the project token in `spawn.json` is Write-only and project-scoped;
//! * `--profile project` is always passed;
//! * before the process starts the run dir holds the user-key pin
//!   (`user.pub`), the signed `parent-policy.json` and `spawn.json`.

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use clawft_kernel::gate::GovernanceSnapshot;
use clawft_kernel::governance_overlay::{load_overlay, merge as merge_overlay};
use clawft_kernel::overlay_runtime::write_user_pin;
use clawft_kernel::parent_policy::{export_rules, export_rules_to};
use clawft_kernel::token_authority::{Issuer, TokenAuthority};
use clawft_kernel::workload_runtime::{
    ChildIdentity, ChildLauncher, ChildProbe, ChildRef, ChildSpec, RuntimeError,
};
use clawft_types::project::cert::key_id;
use clawft_types::project::token_consts::PROJECT_TOKEN_TTL_SECS;
use clawft_types::project::{ContainerTransport, ProjectSandbox, SpawnFile};
use clawft_types::runtime_paths::{
    LOG_FILE_NAME, PARENT_POLICY_FILE, SOCKET_NAME, SPAWN_JSON_FILE,
};
use ed25519_dalek::SigningKey;
use nix::sys::signal::{Signal, kill, killpg};
use nix::unistd::Pid;
use rand::RngCore;
use tokio::sync::watch;

mod container_launch;
mod process;
mod run_files;

use super::SupervisorConfig;
use super::io::ChildIo;
use super::state;
use crate::mesh_local_registry::{SpawnExpectation, cancel_spawn, expect_spawn, registry};

/// Process groups of every child this daemon started or adopted, until the
/// group is gone (see [`Launcher::supervised_pids`]).
static GROUPS: Mutex<std::collections::BTreeSet<u32>> =
    Mutex::new(std::collections::BTreeSet::new());

pub(crate) fn note_group(pgid: u32) {
    GROUPS
        .lock()
        .expect("GROUPS lock poisoned")
        .insert(pgid);
}

/// See [`Launcher::supervised_pids`].
pub(crate) fn supervised_groups() -> Vec<u32> {
    let mut g = GROUPS.lock().expect("GROUPS lock poisoned");
    g.retain(|pgid| !group_gone(*pgid));
    g.iter().copied().collect()
}

fn group_gone(pgid: u32) -> bool {
    matches!(
        killpg(Pid::from_raw(pgid as i32), None),
        Err(nix::errno::Errno::ESRCH)
    )
}

/// Drop `pgid` from the set now if its group is empty. Called when a waiter
/// sees the leader exit: pruning only at the next accept would leave a window
/// in which an unrelated same-uid group that recycled the number is still
/// classed as a child (fail closed, but wrong). A group that still has
/// members (orphaned grandchildren) stays until it is gone.
pub(crate) fn prune_if_gone(pgid: u32) {
    if group_gone(pgid) {
        GROUPS
            .lock()
            .expect("GROUPS lock poisoned")
            .remove(&pgid);
    }
}

#[cfg(test)]
pub(crate) fn group_noted(pgid: u32) -> bool {
    GROUPS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(&pgid)
}

static EXE_SHA: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Locale variables passed through when set.
const LOCALE_VARS: &[&str] = &[
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "LC_COLLATE",
    "LC_MESSAGES",
    "LC_NUMERIC",
    "LC_TIME",
    "LC_MONETARY",
];

/// How a child ended.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExitInfo {
    /// Exit code, when it exited normally.
    pub code: Option<i32>,
    /// Terminating signal.
    pub signal: Option<i32>,
    /// An adopted child whose `kernel.pid` was removed: it shut down cleanly.
    pub clean_hint: bool,
}

impl ExitInfo {
    /// A clean exit is never restarted (OTP `Transient`).
    pub fn clean(&self) -> bool {
        self.code == Some(0) || self.clean_hint
    }
}

fn exit_info(st: std::process::ExitStatus) -> ExitInfo {
    use std::os::unix::process::ExitStatusExt as _;
    ExitInfo {
        code: st.code(),
        signal: st.signal(),
        clean_hint: false,
    }
}

enum Proc {
    Owned {
        pid: u32,
        exit: watch::Receiver<Option<ExitInfo>>,
    },
    Adopted {
        pid: u32,
    },
    Container {
        id: String,
        host_pid: u32,
        cfg: super::container::OperatorConfig,
        mounts: super::container::Mounts,
    },
}

struct Entry {
    proc: Proc,
    /// Set once a stop was requested, so the monitor does not restart.
    stop: Arc<AtomicBool>,
}

/// Token bookkeeping per project: the supervisor honours a refresh only
/// for the token it issued last (or the one before, for in-flight calls).
#[derive(Default)]
struct TokenSlot {
    current: Option<String>,
    previous: Option<String>,
}

/// Everything the launcher needs besides the config.
pub struct LauncherParts {
    /// The user key: signs the parent policy; its public half is pinned.
    pub user_key: SigningKey,
    /// Snapshot of the daemon's governance engine for `parent-policy.json`.
    pub snapshot: Arc<dyn Fn() -> Option<GovernanceSnapshot> + Send + Sync>,
    /// Token authority issuing project tokens (`None`: empty token, the
    /// child's parent link fails closed).
    pub tokens: Option<Arc<TokenAuthority>>,
    /// Handshake and shutdown calls to a child.
    pub io: Arc<dyn ChildIo>,
}

/// The OS side of the `logical` adapter.
pub struct Launcher {
    cfg: Arc<SupervisorConfig>,
    parts: LauncherParts,
    procs: Mutex<HashMap<String, Entry>>,
    token_slots: Mutex<HashMap<String, TokenSlot>>,
    spawns: AtomicU64,
}

/// The environment a child is started with: an allow-list and nothing else.
/// `parent` supplies PATH and the locale variables.
pub fn child_env(
    home: &Path,
    run_dir: &Path,
    project_root: &Path,
    project_id: &str,
    parent: impl Fn(&str) -> Option<String>,
) -> Vec<(String, String)> {
    let mut env = vec![
        ("HOME".to_owned(), home.display().to_string()),
        (
            "PATH".to_owned(),
            parent("PATH").unwrap_or_else(|| "/usr/bin:/bin:/usr/sbin:/sbin".to_owned()),
        ),
        (
            "WEFTOS_RUNTIME_DIR".to_owned(),
            run_dir.display().to_string(),
        ),
        ("WEFTOS_PROJECT_ID".to_owned(), project_id.to_owned()),
        (
            "TMPDIR".to_owned(),
            project_root.join(".weftos/tmp").display().to_string(),
        ),
    ];
    for name in LOCALE_VARS {
        if let Some(v) = parent(name) {
            env.push(((*name).to_owned(), v));
        }
    }
    env
}

/// The arguments a child kernel is started with.
pub fn child_args(project_id: &str) -> Vec<String> {
    [
        "kernel",
        "start",
        "--foreground",
        "--profile",
        "project",
        "--project",
        project_id,
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect()
}

/// True when an ADOPTED `pid` is alive. After a SIGHUP re-exec of the user
/// daemon (`weaver update`) the new image is still the parent of the
/// children but has no waiter for them, so a child that exits stays a zombie
/// and `kill(pid, 0)` would call it alive forever; reaping it here (only for
/// adopted pids: an owned child has a waiter thread that must not lose its
/// status) tells the truth. `ECHILD` means it is not our child: fall back to
/// the signal-0 probe.
pub fn adopted_alive(pid: u32) -> bool {
    use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
    match waitpid(Pid::from_raw(pid as i32), Some(WaitPidFlag::WNOHANG)) {
        Ok(WaitStatus::StillAlive) => true,
        Ok(_) => false,
        Err(_) => pid_alive(pid),
    }
}

/// True when `pid` names a live process (EPERM counts: it exists).
pub fn pid_alive(pid: u32) -> bool {
    match kill(Pid::from_raw(pid as i32), None) {
        Ok(()) => true,
        Err(nix::errno::Errno::EPERM) => true,
        Err(_) => false,
    }
}

fn backend(msg: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::Backend(msg.to_string())
}

fn validate_child_endpoint(socket: &Path) -> Result<(), String> {
    if socket.file_name().and_then(|s| s.to_str()) != Some("child.sock")
        || socket
            .parent()
            .and_then(Path::file_name)
            .and_then(|s| s.to_str())
            != Some("child-ipc")
    {
        return Err("container parent must be the dedicated child.sock endpoint".into());
    }
    Ok(())
}

fn inspected_container_probe(
    result: Result<super::container::Inspected, String>,
    engine: String,
    cid: String,
) -> ChildProbe {
    match result {
        Ok(i) if i.running => match i.host_pid {
            Some(host_pid) => ChildProbe::Running {
                identity: ChildIdentity::Container {
                    engine,
                    immutable_container_id: cid,
                    host_pid,
                },
            },
            None => ChildProbe::Unverifiable {
                reason: "running container has no inspected host PID".into(),
            },
        },
        Ok(i) => ChildProbe::Exited {
            code: i.exit_code,
            signal: None,
        },
        Err(reason) => ChildProbe::Unverifiable {
            reason: format!("container {cid}: {reason}"),
        },
    }
}

impl Launcher {
    /// A launcher over `cfg`.
    pub fn new(cfg: Arc<SupervisorConfig>, parts: LauncherParts) -> Self {
        Self {
            cfg,
            parts,
            procs: Mutex::new(HashMap::new()),
            token_slots: Mutex::new(HashMap::new()),
            spawns: AtomicU64::new(0),
        }
    }

    /// How many children this launcher has started (tests).
    pub fn spawn_count(&self) -> u64 {
        self.spawns.load(Ordering::SeqCst)
    }

    /// `<run_root>/<id>` (`runtime_paths::child_run_dir`; ids are validated
    /// before they get here). This is the child's `$WEFTOS_RUNTIME_DIR`.
    pub fn run_dir(&self, id: &str) -> PathBuf {
        clawft_types::runtime_paths::child_run_dir(&self.cfg.run_root, id)
            .unwrap_or_else(|| self.cfg.run_root.join(id))
    }

    fn procs(&self) -> std::sync::MutexGuard<'_, HashMap<String, Entry>> {
        self.procs.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn token_slots(&self) -> std::sync::MutexGuard<'_, HashMap<String, TokenSlot>> {
        self.token_slots.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Register a verified, already-running child (adoption).
    pub fn adopt(&self, id: &str, pid: u32) {
        note_group(pid);
        self.procs().insert(
            id.to_owned(),
            Entry {
                proc: Proc::Adopted { pid },
                stop: Arc::new(AtomicBool::new(false)),
            },
        );
    }

    pub async fn adopt_container(&self, id: &str, host_pid: u32) -> Result<(), String> {
        self.require_child_endpoint()?;
        let st = state::read(&self.run_dir(id)).ok_or("missing state")?;
        let saved = st.container.ok_or("missing container state")?;
        let manifest = clawft_types::project::find_by_id(&self.cfg.manifests_dir, id)
            .map_err(|e| e.to_string())?
            .ok_or("missing manifest")?;
        let cfg = super::container::OperatorConfig::load(&self.cfg.home)?;
        if cfg.engine != saved.engine {
            return Err("operator engine changed".into());
        }
        let mounts = self.container_mounts(id, &manifest.root);
        if saved.host_socket != mounts.runtime.join(SOCKET_NAME) {
            return Err("persisted host socket differs from the supervised runtime".into());
        }
        let inspected = self
            .container_client(cfg.clone())
            .inspect(&saved.id, id, &mounts)
            .await?;
        if !inspected.running || inspected.host_pid != Some(host_pid) {
            return Err("container changed during adoption".into());
        }
        self.procs().insert(
            id.into(),
            Entry {
                proc: Proc::Container {
                    id: saved.id,
                    host_pid,
                    cfg,
                    mounts,
                },
                stop: Arc::new(AtomicBool::new(false)),
            },
        );
        Ok(())
    }

    /// Forget a project's process entry.
    pub fn forget(&self, id: &str) {
        self.procs().remove(id);
    }

    /// True when a stop was requested for the current process of `id`.
    pub fn stop_requested(&self, id: &str) -> bool {
        self.procs()
            .get(id)
            .is_some_and(|e| e.stop.load(Ordering::SeqCst))
    }

    /// The pid of the live child of `id`.
    pub fn pid_of(&self, id: &str) -> Option<u32> {
        match self.procs().get(id).map(|e| &e.proc) {
            Some(Proc::Owned { pid, exit }) if exit.borrow().is_none() => Some(*pid),
            Some(Proc::Adopted { pid }) if adopted_alive(*pid) => Some(*pid),
            Some(Proc::Container { host_pid, .. }) => Some(*host_pid),
            _ => None,
        }
    }

    /// Process-group ids of every supervised child whose group still exists.
    /// Each child leads its own group (`process_group(0)`, so the group id is
    /// its pid). A group stays in the set until `killpg(pgid, 0)` reports
    /// `ESRCH`, so grandchildren orphaned by the leader's exit are still
    /// recognised (`child_peer`, review S9). Never reaps: unlike
    /// [`adopted_alive`] it does not `waitpid`, because this runs on the
    /// accept path.
    pub fn supervised_pids(&self) -> Vec<u32> {
        supervised_groups()
    }
}

#[cfg(test)]
mod container_endpoint_tests {
    use super::{inspected_container_probe, validate_child_endpoint};
    use clawft_kernel::workload_runtime::ChildProbe;
    use std::path::Path;
    #[test]
    fn container_endpoint_cannot_be_owner_socket() {
        assert!(validate_child_endpoint(Path::new("/run/child-ipc/child.sock")).is_ok());
        assert!(validate_child_endpoint(Path::new("/run/kernel.sock")).is_err());
        assert!(validate_child_endpoint(Path::new("/run/child-ipc/kernel.sock")).is_err());
    }
    #[test]
    fn engine_failure_never_proves_liveness() {
        let probe = inspected_container_probe(
            Err("engine unavailable".into()),
            "docker".into(),
            "a".repeat(64),
        );
        assert!(matches!(probe, ChildProbe::Unverifiable { .. }));
    }
}
