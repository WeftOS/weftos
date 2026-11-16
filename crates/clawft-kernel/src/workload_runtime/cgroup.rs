//! cgroup v2 memory enforcement for native workloads (card 59b07953).
//!
//! Where the host process has a writable (delegated) cgroup v2 subtree, each
//! supervised instance runs in its own child cgroup with `memory.max` set to
//! `ram_mb`, so the kernel enforces the limit without polling and an OOM kill
//! inside the cgroup is reported as `killed_for_memory`. Hosts without
//! delegation fall back to the RSS watchdog and `RLIMIT_DATA` of
//! `supervise.rs`, with the reason logged once.
//!
//! Plain file I/O on the cgroup filesystem only: no new crates, no raw
//! syscalls, no new processes. The child is moved into its cgroup before
//! exec by writing `0` to a `cgroup.procs` handle opened by the parent.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

/// The usual cgroup v2 mount point.
pub const CGROUP_ROOT: &str = "/sys/fs/cgroup";

/// Where the host finds cgroup v2, injectable so tests can use a fake tree.
#[derive(Debug, Clone)]
pub struct CgroupHost {
    /// cgroup v2 mount point.
    pub root: PathBuf,
    /// File naming this process's cgroup (`/proc/self/cgroup`).
    pub self_cgroup: PathBuf,
}

impl CgroupHost {
    /// The running system.
    pub fn system() -> Self {
        Self {
            root: CGROUP_ROOT.into(),
            self_cgroup: "/proc/self/cgroup".into(),
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
/// Serialises the one-time "make my cgroup a delegated parent" step.
static SETUP: Mutex<()> = Mutex::new(());

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
    ) -> Result<(Self, File), String> {
        if !host.root.join("cgroup.controllers").exists() {
            return Err(format!("{} is not a cgroup v2 mount", host.root.display()));
        }
        let own = std::fs::read_to_string(&host.self_cgroup)
            .map_err(|e| format!("read {}: {e}", host.self_cgroup.display()))?;
        let rel = parse_self_cgroup(&own).ok_or("no unified (cgroup v2) hierarchy entry")?;
        // After the one-time move this process lives in `<base>/supervisor`;
        // the delegated parent is still `<base>`.
        let rel = rel.strip_suffix("/supervisor").unwrap_or(rel);
        let base = host.root.join(rel.trim_start_matches('/'));
        let name = format!(
            "weft-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::SeqCst)
        );
        let dir = base.join(&name);
        std::fs::create_dir(&dir)
            .map_err(|e| format!("cgroup not delegated: mkdir {}: {e}", dir.display()))?;
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
                format!("open cgroup.procs: {e}")
            })?;
        Ok((cg, procs))
    }

    fn configure(
        &self,
        base: &Path,
        mem_bytes: u64,
        swap_bytes: Option<u64>,
    ) -> Result<(), String> {
        if !has_controller(&self.dir.join("cgroup.controllers"), "memory") {
            enable_memory(base)?;
            if !has_controller(&self.dir.join("cgroup.controllers"), "memory") {
                return Err("memory controller is not available to this cgroup".into());
            }
        }
        write(&self.dir.join("memory.max"), &mem_bytes.to_string())
            .map_err(|e| format!("write memory.max: {e}"))?;
        // Swap accounting can be compiled out; the file is then absent and
        // there is no swap to cap.
        let swap = self.dir.join("memory.swap.max");
        if swap.exists() {
            write(&swap, &swap_bytes.unwrap_or(0).to_string())
                .map_err(|e| format!("write memory.swap.max: {e}"))?;
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

    /// Best-effort removal from `Drop`.
    pub fn remove_best_effort(&self) {
        self.kill_all();
        self.remove_now();
    }
}

/// Make `base` a delegated parent that hands out the memory controller. The
/// no-internal-processes rule forbids enabling it while `base` holds
/// processes, so this process first moves into a `supervisor` leaf, the same
/// arrangement systemd's delegation guide describes. Runs at most once per
/// need; later spawns find the controller already enabled.
fn enable_memory(base: &Path) -> Result<(), String> {
    let _g = SETUP.lock().unwrap_or_else(|p| p.into_inner());
    let sub = base.join("cgroup.subtree_control");
    if has_controller(&sub, "memory") {
        return Ok(());
    }
    let leaf = base.join("supervisor");
    let _ = std::fs::create_dir(&leaf);
    write(&leaf.join("cgroup.procs"), &std::process::id().to_string())
        .map_err(|e| format!("move supervisor into {}: {e}", leaf.display()))?;
    // A child another spawn forked a moment ago can still be in `base`
    // until its own pre-exec move runs, which makes the write EBUSY.
    let mut last = None;
    for _ in 0..40 {
        match write(&sub, "+memory") {
            Ok(()) => return Ok(()),
            Err(e) if e.raw_os_error() == Some(16) => {
                last = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            Err(e) => return Err(format!("enable memory in {}: {e}", sub.display())),
        }
    }
    Err(format!(
        "enable memory in {}: {}",
        sub.display(),
        last.map(|e| e.to_string()).unwrap_or_default()
    ))
}

#[cfg(test)]
#[path = "cgroup_tests.rs"]
mod tests;
