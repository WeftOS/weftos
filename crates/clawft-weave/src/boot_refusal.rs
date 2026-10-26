//! Boot refusals and the SIGHUP re-exec plan (ADR-103 Phase 1, package H).
//!
//! A daemon that is refused at boot (instance lock held, chain lock held,
//! legacy chain adoption refused, bad config) cannot succeed by being
//! retried, so it exits with [`EX_CONFIG`] rather than 1. The generated
//! systemd unit lists that code in `RestartPreventExitStatus`. launchd
//! cannot filter on exit codes, so the user daemon leaves a `REFUSED`
//! sentinel in its runtime root on a permanent refusal (and on a clean exit)
//! and the generated plist's `KeepAlive` is `PathState { REFUSED: false }`;
//! see [`write_refused`] and [`clear_refused`]. A transient refusal (an older
//! kernel wrote the legacy chain moments ago) is a plain boot error and
//! leaves no sentinel, so launchd retries it.
//!
//! The re-exec plan keeps the original arguments (notably `--profile user`,
//! which is process state, not environment) minus the one-shot chain flags,
//! and execs a path that survives `weaver update` replacing the binary.

use std::path::{Path, PathBuf};

use crate::instance_lock::LockError;
use crate::service_units::strip_deleted;

/// `sysexits.h` EX_CONFIG: the daemon was refused or misconfigured.
pub const EX_CONFIG: i32 = crate::service_units::REFUSED_EXIT;

/// Flags that apply to one boot only and must not be replayed on re-exec.
/// Set in the environment of a SIGHUP re-exec so the new image knows its
/// instance lock may still be held, briefly, under its own pid.
pub const REEXEC_ENV: &str = "WEFTOS_REEXEC";

/// How long a re-exec waits for an inherited lock descriptor to close.
pub const REEXEC_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

const ONE_SHOT_FLAGS: [&str; 2] = ["--new-chain", "--adopt-legacy-chain"];

/// A boot that retrying will not fix.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct BootRefused(pub String);

/// A live daemon already serves this runtime dir (its socket answers). A
/// permanent refusal, but never a reason to leave the restart sentinel: the
/// daemon that is running is fine.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct AlreadyRunning(pub String);

/// Whether a refused boot should leave the `REFUSED` sentinel. Not when
/// another daemon holds the instance lock or answers on the socket: a
/// duplicate `kernel start` must not mark the live daemon as refused.
pub fn leaves_sentinel(e: &anyhow::Error) -> bool {
    exit_code(e) == EX_CONFIG
        && e.downcast_ref::<AlreadyRunning>().is_none()
        && !matches!(e.downcast_ref::<LockError>(), Some(LockError::Held { .. } | LockError::SelfHeld { .. }))
}

/// Process exit code for an error that ended `kernel start --foreground`.
///
/// 78 only for refusals that retrying cannot fix: instance lock held, chain
/// lock held, legacy chain adoption refused, unusable boot configuration, a
/// live daemon on the socket (or one we may not probe). Everything else
/// (service start, mesh bind, I/O) exits 1 so a service manager retries
/// within its start limit.
pub fn exit_code(e: &anyhow::Error) -> i32 {
    use clawft_kernel::KernelError;
    let refused = e.downcast_ref::<BootRefused>().is_some()
        || e.downcast_ref::<AlreadyRunning>().is_some()
        || matches!(e.downcast_ref::<KernelError>(), Some(KernelError::BootRefused(_)))
        || matches!(e.downcast_ref::<LockError>(), Some(LockError::Held { .. } | LockError::SelfHeld { .. }));
    if refused { EX_CONFIG } else { 1 }
}

/// Leave the "do not restart" sentinel for launchd. Best effort: failing to
/// write it only costs the old behaviour (a restart every throttle interval).
pub fn write_refused(sentinel: &Path, reason: &str) {
    if let Some(dir) = sentinel.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = std::fs::write(sentinel, format!("{reason}\n")) {
        tracing::warn!(path = %sentinel.display(), error = %e, "could not write the REFUSED sentinel");
    }
}

/// Leave the sentinel after a clean exit, but only while nobody else owns the
/// runtime dir: the instance lock was released when the daemon returned, so a
/// successor may already be booting. Taking the lock here proves it is not,
/// and holds it while the file is written.
pub fn write_refused_if_unowned(paths: &clawft_types::runtime_paths::RuntimePaths, reason: &str) {
    // An `Err` means a successor (or any other kernel) owns the dir: leave it alone.
    if let Ok(_lock) = crate::instance_lock::InstanceLock::acquire(paths) {
        write_refused(&paths.refused(), reason);
    }
}

/// Remove the sentinel. Missing is fine.
pub fn clear_refused(sentinel: &Path) {
    let _ = std::fs::remove_file(sentinel);
}

/// Whether this process was started by the service manager that watches the
/// sentinel (the generated plist sets [`SERVICE_MANAGER_ENV`](crate::service_units::SERVICE_MANAGER_ENV)).
pub fn service_managed() -> bool {
    std::env::var_os(crate::service_units::SERVICE_MANAGER_ENV).is_some_and(|v| !v.is_empty())
}

/// What a successful boot did about the sentinel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusedLift {
    /// No sentinel was present.
    Absent,
    /// The service-managed instance removed it.
    Cleared,
    /// A manual start found it and left it: the launchd job stays parked.
    LeftParked,
}

/// A boot succeeded: lift the sentinel only when `managed` (the service
/// manager started this instance). A manual start leaves it, so launchd,
/// which watches the path, does not launch its own instance beside the manual
/// one and have it refused every throttle interval.
pub fn lift_refused_on_boot(sentinel: &Path, managed: bool) -> RefusedLift {
    if !sentinel.exists() {
        return RefusedLift::Absent;
    }
    if managed {
        clear_refused(sentinel);
        RefusedLift::Cleared
    } else {
        RefusedLift::LeftParked
    }
}

/// The launchd hint only makes sense where launchd exists.
pub fn parked_hint_applies() -> bool {
    cfg!(target_os = "macos")
}

/// Printed by a manual `kernel start --profile user` when the launchd job is
/// parked by the sentinel.
pub fn parked_hint(sentinel: &Path) -> String {
    format!(
        "note: the launchd job is parked ({} exists) and stays parked while this daemon runs. \
To hand control back: `weaver kernel stop --profile user`, then \
`launchctl kickstart gui/$(id -u)/{}`",
        sentinel.display(),
        crate::service_units::LAUNCHD_LABEL
    )
}

/// Everything a SIGHUP re-exec must reproduce, captured once at startup
/// before the daemon changes directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Replay {
    /// Arguments after the program name, `--config` made absolute.
    pub args: Vec<String>,
    /// Absolute `$WEFTOS_RUNTIME_DIR`, when set.
    pub runtime_dir: Option<PathBuf>,
}

static REPLAY: std::sync::OnceLock<Replay> = std::sync::OnceLock::new();

/// Make the value of `--config X`, `-c X`, `-cX` and `--config=X` absolute against `cwd`.
pub fn absolutize_config_args(args: Vec<String>, cwd: &Path) -> Vec<String> {
    let abs = |v: &str| {
        let p = Path::new(v);
        if p.is_absolute() { v.to_owned() } else { cwd.join(p).to_string_lossy().into_owned() }
    };
    let mut out = Vec::with_capacity(args.len());
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        if a == "--config" || a == "-c" {
            out.push(a);
            if let Some(v) = it.next() {
                out.push(abs(&v));
            }
        } else if let Some(v) = a.strip_prefix("--config=") {
            out.push(format!("--config={}", abs(v)));
        } else if let Some(v) = a.strip_prefix("-c").filter(|v| !v.is_empty() && !a.starts_with("--")) {
            // clap's attached short form: `-cREL`.
            out.push(format!("-c{}", abs(v)));
        } else {
            out.push(a);
        }
    }
    out
}

impl Replay {
    /// Build from explicit inputs (no process state).
    pub fn capture(args: Vec<String>, cwd: &Path, runtime_env: Option<&str>) -> Self {
        Self {
            args: absolutize_config_args(args, cwd),
            runtime_dir: runtime_env
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(|v| if Path::new(v).is_absolute() { PathBuf::from(v) } else { cwd.join(v) }),
        }
    }
}

/// Record the replay inputs from the process. Call first thing in
/// `kernel`, before any `chdir`; later calls are ignored.
pub fn capture_replay() {
    let cwd = std::env::current_dir().unwrap_or_default();
    let env = std::env::var("WEFTOS_RUNTIME_DIR").ok();
    let _ = REPLAY.set(Replay::capture(std::env::args().skip(1).collect(), &cwd, env.as_deref()));
}

/// The captured replay inputs, or the current process state if nothing was
/// captured (tests, unusual entry points).
pub fn replay() -> Replay {
    REPLAY.get().cloned().unwrap_or_else(|| {
        let cwd = std::env::current_dir().unwrap_or_default();
        let env = std::env::var("WEFTOS_RUNTIME_DIR").ok();
        Replay::capture(std::env::args().skip(1).collect(), &cwd, env.as_deref())
    })
}

/// Arguments (after the program name) for a re-exec: the originals without
/// the one-shot flags.
pub fn reexec_args<I: IntoIterator<Item = String>>(original: I) -> Vec<String> {
    let mut args: Vec<String> = original
        .into_iter()
        .filter(|a| !ONE_SHOT_FLAGS.contains(&a.as_str()))
        .collect();
    // A plain `kernel start` that is being re-exec'd is a daemon that was
    // already running: the one-release refusal of project-rooted daemons
    // beside a user daemon (ADR-103 A6) must not take it down on SIGHUP.
    let plain_start = args.iter().any(|a| a == "start")
        && !args.iter().any(|a| a == "--profile" || a.starts_with("--profile="))
        && !args.iter().any(|a| a == "--legacy-project-daemon");
    if plain_start {
        args.push("--legacy-project-daemon".to_owned());
    }
    args
}

/// What to exec for a restart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReexecPlan {
    /// `current_exe` without Linux's ` (deleted)` marker, so a binary
    /// replaced by an update still starts.
    pub exe: PathBuf,
    pub args: Vec<String>,
    /// Absolute runtime dir to pass as `WEFTOS_RUNTIME_DIR` (via
    /// `Command::env`, never `set_var`).
    pub runtime_dir: Option<PathBuf>,
}

/// Plan the re-exec from `current_exe` and the captured [`Replay`]; with no
/// arguments at all, fall back to the plain foreground start.
pub fn reexec_plan(current_exe: &Path, replay: Replay) -> ReexecPlan {
    let mut args = reexec_args(replay.args);
    if args.is_empty() {
        args = ["kernel", "start", "--foreground"].map(String::from).to_vec();
    }
    ReexecPlan { exe: strip_deleted(current_exe), args, runtime_dir: replay.runtime_dir }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn profile_survives_and_one_shot_flags_do_not() {
        let got = reexec_args(v(&[
            "kernel", "start", "--foreground", "--profile", "user", "--new-chain", "--adopt-legacy-chain",
        ]));
        assert_eq!(got, v(&["kernel", "start", "--foreground", "--profile", "user"]));
    }

    #[test]
    fn a_re_exec_of_a_plain_daemon_is_marked_legacy_so_the_refusal_cannot_kill_it() {
        let got = reexec_args(v(&["kernel", "start", "--foreground"]));
        assert_eq!(got, v(&["kernel", "start", "--foreground", "--legacy-project-daemon"]));
        // Already marked: not doubled. Profiles carry their own meaning.
        let marked = v(&["kernel", "start", "--legacy-project-daemon"]);
        assert_eq!(reexec_args(marked.clone()), marked);
        let project = v(&["kernel", "start", "--foreground", "--profile", "project", "--project", "X"]);
        assert_eq!(reexec_args(project.clone()), project);
        let user = v(&["kernel", "start", "--foreground", "--profile=user"]);
        assert_eq!(reexec_args(user.clone()), user);
    }

    #[test]
    fn plan_strips_deleted_marker_and_defaults_args() {
        let r = Replay { args: v(&["kernel", "start", "--foreground"]), runtime_dir: None };
        let p = reexec_plan(Path::new("/opt/bin/weaver (deleted)"), r);
        assert_eq!(p.exe, PathBuf::from("/opt/bin/weaver"));
        assert_eq!(p.args, v(&["kernel", "start", "--foreground", "--legacy-project-daemon"]));
        let p = reexec_plan(Path::new("/x/weaver"), Replay { args: Vec::new(), runtime_dir: None });
        assert_eq!(p.args, v(&["kernel", "start", "--foreground"]));
    }

    #[test]
    fn relative_config_is_absolute_in_every_spelling_after_a_chdir() {
        // Captured with cwd=/work, replayed after the daemon moved to ~/.weftos.
        let cwd = Path::new("/work");
        for (given, want) in [
            (v(&["kernel", "start", "--config", "w.toml"]), v(&["kernel", "start", "--config", "/work/w.toml"])),
            (v(&["kernel", "-c", "sub/w.toml", "start"]), v(&["kernel", "-c", "/work/sub/w.toml", "start"])),
            (v(&["kernel", "start", "--config=w.toml"]), v(&["kernel", "start", "--config=/work/w.toml"])),
            (v(&["--config", "/abs/w.toml"]), v(&["--config", "/abs/w.toml"])),
            (v(&["kernel", "start", "-cw.toml"]), v(&["kernel", "start", "-c/work/w.toml"])),
            (v(&["kernel", "start", "-c/abs/w.toml"]), v(&["kernel", "start", "-c/abs/w.toml"])),
        ] {
            let r = Replay::capture(given, cwd, None);
            assert_eq!(r.args, want);
            let moved = tempfile::tempdir().unwrap();
            let _ = moved; // the plan never consults the current directory
            // (a plain `start` also gains the legacy marker, tested below)
            assert_eq!(reexec_plan(Path::new("/x/weaver"), r).args, reexec_args(want));
        }
    }

    #[test]
    fn runtime_dir_is_captured_absolute_and_blank_is_unset() {
        let cwd = Path::new("/work");
        assert_eq!(Replay::capture(vec![], cwd, Some("rt")).runtime_dir, Some(PathBuf::from("/work/rt")));
        assert_eq!(Replay::capture(vec![], cwd, Some("/rt")).runtime_dir, Some(PathBuf::from("/rt")));
        assert_eq!(Replay::capture(vec![], cwd, Some("  ")).runtime_dir, None);
        assert_eq!(Replay::capture(vec![], cwd, None).runtime_dir, None);
    }

    #[test]
    fn only_irreparable_refusals_exit_78() {
        use clawft_kernel::KernelError;
        let held = anyhow::Error::new(LockError::Held { root: "/r".into(), pid: "7".into() });
        assert_eq!(exit_code(&held), EX_CONFIG);
        assert_eq!(exit_code(&anyhow::Error::new(BootRefused("socket live".into()))), 78);
        assert_eq!(exit_code(&anyhow::Error::new(KernelError::BootRefused("chain in use".into()))), 78);
        // Retryable: everything else.
        for e in [
            anyhow::Error::new(KernelError::Boot("service start failed: x".into())),
            // The transient legacy age-window refusal (chain_storage).
            anyhow::Error::new(KernelError::Boot(
                "the legacy chain at /h/.clawft/chain.json looks in use by an older kernel".into(),
            )),
            anyhow::Error::new(KernelError::Boot("mesh enabled but the listener could not bind".into())),
            anyhow::anyhow!("socket write failed"),
            anyhow::Error::new(LockError::Io { path: "/x".into(), source: std::io::Error::other("e") }),
        ] {
            assert_eq!(exit_code(&e), 1, "{e}");
        }
    }

    #[test]
    fn refused_sentinel_round_trips_and_clear_is_idempotent() {
        let d = tempfile::tempdir().unwrap();
        let s = d.path().join("run").join("REFUSED");
        write_refused(&s, "instance lock held");
        assert_eq!(std::fs::read_to_string(&s).unwrap(), "instance lock held\n");
        clear_refused(&s);
        assert!(!s.exists());
        clear_refused(&s);
    }

    #[test]
    fn only_a_service_managed_boot_lifts_the_sentinel() {
        let d = tempfile::tempdir().unwrap();
        let s = d.path().join("run").join("REFUSED");
        assert_eq!(lift_refused_on_boot(&s, false), RefusedLift::Absent);
        write_refused(&s, "stopped cleanly");
        // A manual start (no marker) leaves the launchd job parked.
        assert_eq!(lift_refused_on_boot(&s, false), RefusedLift::LeftParked);
        assert!(s.exists());
        assert!(parked_hint(&s).contains("launchctl kickstart gui/$(id -u)/ai.weftos.user"));
        assert_eq!(parked_hint_applies(), cfg!(target_os = "macos"));
        // The launchd-started instance (marker set) clears it.
        assert_eq!(lift_refused_on_boot(&s, true), RefusedLift::Cleared);
        assert!(!s.exists());
        assert_eq!(lift_refused_on_boot(&s, true), RefusedLift::Absent);
    }

    #[test]
    fn duplicate_start_refusals_leave_no_sentinel_but_other_refusals_do() {
        use clawft_kernel::KernelError;
        let held = anyhow::Error::new(LockError::Held { root: "/r".into(), pid: "7".into() });
        let live = anyhow::Error::new(AlreadyRunning("daemon already running".into()));
        assert_eq!((exit_code(&held), exit_code(&live)), (78, 78));
        assert!(!leaves_sentinel(&held));
        assert!(!leaves_sentinel(&live));
        assert!(leaves_sentinel(&anyhow::Error::new(KernelError::BootRefused("chain".into()))));
        assert!(leaves_sentinel(&anyhow::Error::new(BootRefused("config".into()))));
        assert!(!leaves_sentinel(&anyhow::Error::new(KernelError::Boot("transient".into()))));
    }

    #[test]
    fn clean_exit_sentinel_is_written_only_when_no_successor_holds_the_lock() {
        use clawft_types::runtime_paths::RuntimePaths;
        let d = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::at(d.path());
        // A successor already booted: it holds the lock, so no sentinel.
        let successor = crate::instance_lock::InstanceLock::acquire(&paths).unwrap();
        write_refused_if_unowned(&paths, "stopped cleanly");
        assert!(!paths.refused().exists());
        drop(successor);
        write_refused_if_unowned(&paths, "stopped cleanly");
        assert_eq!(std::fs::read_to_string(paths.refused()).unwrap(), "stopped cleanly\n");
        // The lock was released again afterwards.
        assert!(crate::instance_lock::InstanceLock::acquire(&paths).is_ok());
    }
}
