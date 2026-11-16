//! cgroup v2 memory enforcement for native workloads (card 59b07953).
//!
//! Where the host unit declares a delegated cgroup v2 subtree, each
//! supervised instance runs in its own child cgroup with `memory.max` set to
//! `ram_mb`, so the kernel enforces the limit and an OOM kill inside the
//! cgroup is reported as `killed_for_memory`. "Delegated" is an explicit
//! opt-in ([`DELEGATED_ENV`], emitted by the unit generator together with
//! `Delegate=yes`), never inferred from "I can mkdir": a writable cgroup under
//! a unit without `Delegate=` loses its controllers on `daemon-reload`. The
//! RSS watchdog and `RLIMIT_DATA` of `supervise.rs` stay on as the backstop
//! in cgroup mode too, and are the only enforcement where there is no
//! delegation (the reason is logged once).
//!
//! Plain file I/O on the cgroup filesystem only: no new crates, no raw
//! syscalls, no new processes. The child is moved into its cgroup before
//! exec by writing `0` to a `cgroup.procs` handle opened by the parent.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

/// The usual cgroup v2 mount point.
pub const CGROUP_ROOT: &str = "/sys/fs/cgroup";

/// Environment switch the host unit sets (with `Delegate=yes`) to declare that
/// its cgroup is delegated to this process. Without it the cgroup path is not
/// used, however writable the cgroup looks.
pub const DELEGATED_ENV: &str = "WEFTOS_CGROUP_DELEGATED";

/// `cgroup.procs` write failures in the forked child map to this errno, so the
/// parent can tell "could not join the cgroup" from any other spawn error.
pub const JOIN_FAILED_ERRNO: i32 = libc::ECANCELED;

/// Why a cgroup could not be created.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CgroupError {
    /// The host does not offer delegation (no v2 mount, no opt-in, a cgroup we
    /// may not write, no memory controller). The expected fallback case.
    NotDelegated(String),
    /// Delegation looked available but a step failed (EBUSY, I/O error...).
    Failed(String),
}

impl std::fmt::Display for CgroupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotDelegated(s) | Self::Failed(s) => f.write_str(s),
        }
    }
}

/// Where the host finds cgroup v2, injectable so tests can use a fake tree.
#[derive(Debug, Clone)]
pub struct CgroupHost {
    /// cgroup v2 mount point.
    pub root: PathBuf,
    /// File naming this process's cgroup (`/proc/self/cgroup`).
    pub self_cgroup: PathBuf,
    /// The host unit declared the cgroup delegated ([`DELEGATED_ENV`]).
    pub delegated: bool,
}

impl CgroupHost {
    /// The running system.
    pub fn system() -> Self {
        Self {
            root: CGROUP_ROOT.into(),
            self_cgroup: "/proc/self/cgroup".into(),
            delegated: std::env::var(DELEGATED_ENV).is_ok_and(|v| v == "1"),
        }
    }
}

/// One instance's cgroup.
#[derive(Debug)]
pub struct Cgroup {
    dir: PathBuf,
}

static SEQ: AtomicU64 = AtomicU64::new(0);
static WARNED: AtomicBool = AtomicBool::new(false);
/// Per-base outcome of the "make my cgroup a delegated parent" step. Success
/// and "not ours to manage" are final. An enable that failed for another
/// reason (typically EBUSY: a process still sits directly in the cgroup) is
/// retried on a later launch, at most [`ENABLE_RETRIES`] more times and not
/// before [`ENABLE_RETRY_AFTER`] has passed, so it never sleeps and never
/// loops.
struct Setup {
    out: Result<(), CgroupError>,
    tries: u32,
    at: std::time::Instant,
}
static SETUP: Mutex<Option<HashMap<PathBuf, Setup>>> = Mutex::new(None);

/// Later launches that may retry a failed enable (see [`Setup`]).
const ENABLE_RETRIES: u32 = 3;
/// Minimum time between those retries.
const ENABLE_RETRY_AFTER: std::time::Duration = std::time::Duration::from_secs(10);

/// Log a fallback reason once per process (every start would be noise).
pub fn warn_fallback_once(reason: &str) {
    if !WARNED.swap(true, Ordering::SeqCst) {
        tracing::warn!(reason, "cgroup v2 memory.max unavailable; using the RSS watchdog and RLIMIT_DATA");
    }
}

/// The unified-hierarchy path out of a `/proc/<pid>/cgroup` body (`0::/path`).
pub(crate) fn parse_self_cgroup(body: &str) -> Option<&str> {
    body.lines().find_map(|l| l.strip_prefix("0::"))
}

/// Directory of the delegated cgroup this process runs in, or why there is
/// none. After the one-time move the process lives in `<base>/supervisor` and
/// systemd runs `ExecStop*` helpers in `<base>/.control`; the delegated
/// parent is `<base>` in every case.
fn delegated_base(host: &CgroupHost) -> Result<PathBuf, CgroupError> {
    use CgroupError::{Failed, NotDelegated};
    if !host.root.join("cgroup.controllers").exists() {
        return Err(NotDelegated(format!(
            "{} is not a cgroup v2 mount",
            host.root.display()
        )));
    }
    if !host.delegated {
        return Err(NotDelegated(format!(
            "the host unit does not declare a delegated cgroup ({DELEGATED_ENV}=1 with Delegate=yes)"
        )));
    }
    let own = std::fs::read_to_string(&host.self_cgroup)
        .map_err(|e| Failed(format!("read {}: {e}", host.self_cgroup.display())))?;
    let rel = parse_self_cgroup(&own)
        .ok_or_else(|| NotDelegated("no unified (cgroup v2) hierarchy entry".into()))?;
    let rel = ["/supervisor", "/.control"]
        .iter()
        .find_map(|s| rel.strip_suffix(s))
        .unwrap_or(rel);
    Ok(host.root.join(rel.trim_start_matches('/')))
}

/// Make this process's delegated cgroup ready to hand out memory limits, once,
/// at daemon start and before any child (model server, child kernel, cog) is
/// spawned: the daemon moves into `<base>/supervisor`, so everything spawned
/// later inherits that leaf and `<base>` stays free of processes (card
/// 59b07953, R2-M1). `Err(NotDelegated)` is the normal answer on a host
/// without delegation; the caller logs it once.
pub fn prepare_host(host: &CgroupHost) -> Result<(), CgroupError> {
    prepare(&delegated_base(host)?)
}

fn has_controller(file: &Path, name: &str) -> bool {
    std::fs::read_to_string(file)
        .map(|s| s.split_whitespace().any(|c| c == name))
        .unwrap_or(false)
}

/// Value of `key` in a flat-keyed cgroup file such as `memory.events`.
pub(crate) fn flat_key(body: &str, key: &str) -> Option<u64> {
    body.lines().find_map(|l| {
        let mut it = l.split_whitespace();
        (it.next()? == key).then(|| it.next()?.parse().ok())?
    })
}

fn write(path: &Path, value: &str) -> std::io::Result<()> {
    std::fs::write(path, value)
}

impl Cgroup {
    /// Create a child cgroup under this process's delegated cgroup with
    /// `memory.max = mem_bytes` (and `memory.swap.max`). Returns the cgroup
    /// and the `cgroup.procs` handle the child writes `0` to before exec.
    /// `Err` is the human reason the host cannot delegate memory.
    pub fn create(
        host: &CgroupHost,
        mem_bytes: u64,
        swap_bytes: Option<u64>,
    ) -> Result<(Self, File), CgroupError> {
        use CgroupError::{Failed, NotDelegated};
        let base = delegated_base(host)?;
        prepare(&base)?;
        let name = format!(
            "weft-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::SeqCst)
        );
        let dir = base.join(&name);
        std::fs::create_dir(&dir).map_err(|e| {
            let msg = format!("mkdir {}: {e}", dir.display());
            match e.kind() {
                std::io::ErrorKind::PermissionDenied
                | std::io::ErrorKind::NotFound
                | std::io::ErrorKind::ReadOnlyFilesystem => {
                    NotDelegated(format!("cgroup not delegated: {msg}"))
                }
                _ => Failed(msg),
            }
        })?;
        let cg = Self { dir };
        if let Err(e) = cg.configure(&base, mem_bytes, swap_bytes) {
            cg.remove_now();
            return Err(e);
        }
        let procs = OpenOptions::new()
            .write(true)
            .open(cg.dir.join("cgroup.procs"))
            .map_err(|e| {
                cg.remove_now();
                Failed(format!("open cgroup.procs: {e}"))
            })?;
        Ok((cg, procs))
    }

    /// Directory of this cgroup.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn configure(
        &self,
        base: &Path,
        mem_bytes: u64,
        swap_bytes: Option<u64>,
    ) -> Result<(), CgroupError> {
        let _ = base;
        if !has_controller(&self.dir.join("cgroup.controllers"), "memory") {
            return Err(CgroupError::NotDelegated(
                "memory controller is not available to this cgroup".into(),
            ));
        }
        write(&self.dir.join("memory.max"), &mem_bytes.to_string())
            .map_err(|e| CgroupError::Failed(format!("write memory.max: {e}")))?;
        // Swap accounting can be compiled out; the file is then absent and
        // there is no swap to cap.
        let swap = self.dir.join("memory.swap.max");
        if swap.exists() {
            write(&swap, &swap_bytes.unwrap_or(0).to_string())
                .map_err(|e| CgroupError::Failed(format!("write memory.swap.max: {e}")))?;
        }
        // Kill the whole cgroup together on OOM (best effort, kernel 4.19+).
        let _ = write(&self.dir.join("memory.oom.group"), "1");
        Ok(())
    }

    /// True when the kernel OOM-killed a process in this cgroup.
    pub fn oom_killed(&self) -> bool {
        std::fs::read_to_string(self.dir.join("memory.events"))
            .ok()
            .and_then(|b| flat_key(&b, "oom_kill"))
            .is_some_and(|n| n > 0)
    }

    /// Kill every process left in the cgroup (kernel 5.14+; a no-op before).
    pub fn kill_all(&self) {
        let _ = write(&self.dir.join("cgroup.kill"), "1");
    }

    fn remove_now(&self) {
        let _ = std::fs::remove_dir(&self.dir);
    }

    /// Remove the cgroup once its processes are gone, retrying briefly:
    /// exits are reaped asynchronously.
    pub async fn remove(&self) {
        self.kill_all();
        for _ in 0..40 {
            match std::fs::remove_dir(&self.dir) {
                Ok(()) => return,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
                Err(_) => tokio::time::sleep(std::time::Duration::from_millis(25)).await,
            }
        }
        tracing::warn!(dir = %self.dir.display(), "cgroup left behind (processes still exiting)");
    }

    /// Best-effort removal from `Drop`: kill, try once, and when the killed
    /// tasks have not exited yet retry briefly on a short-lived thread (the
    /// retry must not block the caller or need an async runtime).
    pub fn remove_best_effort(&self) {
        #[cfg(test)]
        BEST_EFFORT_CALLS.with(|n| n.set(n.get() + 1));
        self.kill_all();
        if std::fs::remove_dir(&self.dir).is_ok() {
            return;
        }
        let dir = self.dir.clone();
        let _ = std::thread::Builder::new()
            .name("cgroup-rmdir".into())
            .spawn(move || {
                for _ in 0..40 {
                    std::thread::sleep(std::time::Duration::from_millis(25));
                    match std::fs::remove_dir(&dir) {
                        Ok(()) => return,
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
                        Err(_) => {}
                    }
                }
                tracing::warn!(dir = %dir.display(), "cgroup left behind (processes still exiting)");
            });
    }
}

/// Per-base preparation: sweep stale instance cgroups of dead supervisors,
/// then make `base` a delegated parent that hands out the memory controller.
/// The outcome is cached with its reason. Nothing here sleeps: a failure that
/// can be transient (for example EBUSY because another process lives in
/// `base`) is retried by a later call, bounded by [`Setup`].
pub fn prepare(base: &Path) -> Result<(), CgroupError> {
    prepare_with(base, ENABLE_RETRY_AFTER)
}

fn prepare_with(base: &Path, retry_after: std::time::Duration) -> Result<(), CgroupError> {
    let mut guard = SETUP.lock().unwrap_or_else(|p| p.into_inner());
    let map = guard.get_or_insert_with(HashMap::new);
    if let Some(done) = map.get(base) {
        let retry = matches!(done.out, Err(CgroupError::Failed(_)))
            && done.tries <= ENABLE_RETRIES
            && done.at.elapsed() >= retry_after;
        if !retry {
            return done.out.clone();
        }
    }
    let tries = map.get(base).map_or(1, |d| d.tries + 1);
    sweep_stale(base);
    let out = enable_memory(base);
    map.insert(
        base.to_path_buf(),
        Setup { out: out.clone(), tries, at: std::time::Instant::now() },
    );
    out
}

/// Test seam: drop the cached outcome for `base` so the next launch retries.
#[cfg(test)]
pub(crate) fn forget_setup(base: &Path) {
    let mut guard = SETUP.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(map) = guard.as_mut() {
        map.remove(base);
    }
}

fn pid_alive(pid: u32) -> bool {
    Path::new("/proc").join(pid.to_string()).exists()
}

/// The supervisor pid in an instance cgroup name `weft-<pid>-<n>`.
fn instance_pid(name: &std::ffi::OsStr) -> Option<u32> {
    name.to_str()?
        .strip_prefix("weft-")?
        .split('-')
        .next()?
        .parse()
        .ok()
}

/// Remove empty `weft-<pid>-<n>` cgroups under `base` whose supervisor is gone
/// (a crash or a failed `Drop` removal leaves them). `rmdir` of a cgroup with
/// live tasks fails, so this cannot kill anything.
fn sweep_stale(base: &Path) {
    let Ok(rd) = std::fs::read_dir(base) else { return };
    let me = std::process::id();
    for e in rd.flatten() {
        let Some(pid) = instance_pid(&e.file_name()) else { continue };
        if pid != me && !pid_alive(pid) {
            let _ = std::fs::remove_dir(e.path());
        }
    }
}

/// What [`quiesce_leftovers`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Quiesced {
    /// Cgroups that had controllers enabled for their children and no longer do.
    pub changed: usize,
    /// Cgroups where that write failed (left as they were).
    pub failed: usize,
}

/// Deepest nesting [`quiesce_leftovers`] follows below an instance cgroup.
const QUIESCE_MAX_DEPTH: usize = 16;

/// Stop leftover instance cgroups from handing controllers to their children,
/// without touching a process.
///
/// A cog that survives a daemon exit (`KillMode=process` is deliberate: child
/// kernels and cogs live on) may enable a controller in its own
/// `weft-<pid>-<n>/cgroup.subtree_control`, for example a container runtime
/// that manages cgroups itself. systemd then cannot put the next daemon into
/// the unit cgroup: it has to clear `+memory` from the unit's
/// `subtree_control` first, and the kernel refuses that (EBUSY) while a child
/// still enables the controller. The unit fails with "Failed to spawn
/// executor: Device or resource busy" and the start limit keeps it down.
///
/// This walks the `weft-*` cgroups of dead supervisors, children first, and
/// writes `-<controller>` for every controller each one still enables.
/// Processes keep running where they are. It runs as `ExecStopPost=` of the
/// generated unit (systemd also runs it after a failed start, so it repairs a
/// controller the survivor enabled late) and writes nothing else. It needs the
/// same opt-in as the rest of the module.
pub fn quiesce_leftovers(host: &CgroupHost) -> Result<Quiesced, CgroupError> {
    let base = delegated_base(host)?;
    let me = std::process::id();
    let mut out = Quiesced::default();
    let rd = std::fs::read_dir(&base)
        .map_err(|e| CgroupError::Failed(format!("read {}: {e}", base.display())))?;
    for e in rd.flatten() {
        let Some(pid) = instance_pid(&e.file_name()) else { continue };
        if pid == me || pid_alive(pid) || !e.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        quiesce_tree(&e.path(), 0, &mut out);
    }
    Ok(out)
}

fn quiesce_tree(dir: &Path, depth: usize, out: &mut Quiesced) {
    if depth < QUIESCE_MAX_DEPTH
        && let Ok(rd) = std::fs::read_dir(dir)
    {
        for e in rd.flatten() {
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                quiesce_tree(&e.path(), depth + 1, out);
            }
        }
    }
    let ctl = dir.join("cgroup.subtree_control");
    let Ok(body) = std::fs::read_to_string(&ctl) else { return };
    let off: Vec<String> = body.split_whitespace().map(|c| format!("-{c}")).collect();
    if off.is_empty() {
        return;
    }
    match write(&ctl, &off.join(" ")) {
        Ok(()) => out.changed += 1,
        Err(_) => out.failed += 1,
    }
}

/// Make `base` a delegated parent that hands out the memory controller. The
/// no-internal-processes rule forbids enabling it while `base` holds
/// processes, so this process first moves into a `supervisor` leaf, the same
/// arrangement systemd's delegation guide describes. Called once per base via
/// [`prepare`].
fn enable_memory(base: &Path) -> Result<(), CgroupError> {
    use CgroupError::{Failed, NotDelegated};
    // Missing permission or path means "not ours to manage": the expected
    // fallback. Anything else (EBUSY, I/O) is a real failure.
    let classify = |e: &std::io::Error, msg: String| match e.kind() {
        std::io::ErrorKind::PermissionDenied
        | std::io::ErrorKind::NotFound
        | std::io::ErrorKind::ReadOnlyFilesystem => NotDelegated(msg),
        _ => Failed(msg),
    };
    let sub = base.join("cgroup.subtree_control");
    if has_controller(&sub, "memory") {
        return Ok(());
    }
    // The move cannot help when the parent does not hand out memory.
    if !has_controller(&base.join("cgroup.controllers"), "memory") {
        return Err(NotDelegated(format!(
            "memory controller is not available in {}",
            base.display()
        )));
    }
    let leaf = base.join("supervisor");
    let _ = std::fs::create_dir(&leaf);
    write(&leaf.join("cgroup.procs"), &std::process::id().to_string())
        .map_err(|e| classify(&e, format!("move supervisor into {}: {e}", leaf.display())))?;
    write(&sub, "+memory").map_err(|e| {
        // The write is EBUSY when another process still lives directly in
        // `base` (a model server, a shell): the unit must hold only weaver.
        // A plain constant of the platform errno, no sleep and no retry.
        let why = if e.raw_os_error() == Some(libc::EBUSY) {
            " (EBUSY: another process lives directly in this cgroup)"
        } else {
            ""
        };
        classify(&e, format!("enable memory in {}: {e}{why}", sub.display()))
    })
}

// Test seam: how many times `remove_best_effort` ran on this thread.
#[cfg(test)]
thread_local!(pub(crate) static BEST_EFFORT_CALLS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) });

#[cfg(test)]
#[path = "cgroup_tests.rs"]
mod tests;
