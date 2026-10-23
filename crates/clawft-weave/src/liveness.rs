//! Daemon liveness self-check and the systemd watchdog (card 1a919ae6).
//!
//! A wedged daemon must not sit dead for days (the 2026-10-15 hang kept its
//! mesh port listening for 8 days with no runtime progress). A task probes,
//! every round, the paths that hang took down:
//!
//! - `scheduler`: the probe task's own sleep overshoot (a starved runtime
//!   wakes it late);
//! - `blocking_pool`: a `spawn_blocking` round trip;
//! - `kernel_lock`: `kernel.read()`;
//! - `mesh_locks`: [`MeshRuntime::lock_probe`] on a blocking thread (every
//!   `peers` shard, the connection-id and discovery locks);
//! - `rpc`: a `kernel.liveness` call over the daemon's own socket (accept
//!   loop, gates and dispatch).
//!
//! Each probe is bounded by [`PROBE_TIMEOUT`]. A round where all pass stamps
//! `last_ok` and, under systemd with `WatchdogSec=`, sends `WATCHDOG=1`; a
//! wedged daemon stops sending it and systemd kills and restarts it. Without
//! `NOTIFY_SOCKET` / `WATCHDOG_USEC` (macOS, a manual start) nothing is sent
//! and only the self-check runs. `kernel.liveness` returns the last round;
//! `weaver kernel liveness` asks for it and fails when it is unhealthy,
//! stale, or the daemon does not answer.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use clawft_kernel::boot::Kernel;
use clawft_platform::NativePlatform;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

/// Round interval without a systemd watchdog.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(10);
/// Longest one probe may take before it counts as failed.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// Scheduler lag (sleep overshoot) that counts as a failed round.
pub const MAX_LAG: Duration = Duration::from_secs(5);
/// `last_ok` older than this many intervals reads as stale to the CLI.
pub const STALE_INTERVALS: u32 = 3;

type SharedKernel = Arc<tokio::sync::RwLock<Kernel<NativePlatform>>>;

/// One probe's outcome.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Probe {
    /// Probe name (see the module doc).
    pub name: String,
    /// Passed within [`PROBE_TIMEOUT`].
    pub ok: bool,
    /// How long it took, milliseconds.
    pub ms: u64,
    /// Why it failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// What `kernel.liveness` returns.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Report {
    /// Unix seconds of the last round where every probe passed (0: none).
    pub last_ok: u64,
    /// Unix seconds of the last round.
    pub last_round: u64,
    /// Rounds failed in a row.
    pub failures: u32,
    /// Seconds between rounds.
    pub interval_s: u64,
    /// Whether `WATCHDOG=1` goes to systemd.
    pub watchdog: bool,
    /// The last round's probes.
    pub probes: Vec<Probe>,
}

impl Report {
    /// Healthy: the last round passed and is recent enough at `now`.
    pub fn verdict(&self, now: u64) -> Result<(), String> {
        if self.last_ok == 0 {
            return Err("no liveness round has passed yet".into());
        }
        let stale = self.interval_s.max(1) * u64::from(STALE_INTERVALS);
        if now.saturating_sub(self.last_ok) > stale {
            return Err(format!("last good round {}s ago (stale after {stale}s)", now - self.last_ok));
        }
        match self.probes.iter().find(|p| !p.ok) {
            Some(p) => Err(format!("probe {} failed: {}", p.name, p.error.as_deref().unwrap_or("?"))),
            None => Ok(()),
        }
    }
}

/// Longest the boot keepalive feeds the watchdog before the liveness loop
/// takes over. A boot stuck past it stops feeding, and systemd restarts it.
pub const BOOT_GRACE: Duration = Duration::from_secs(300);

static REPORT: OnceLock<Mutex<Report>> = OnceLock::new();

/// The process's systemd watchdog interval, if it is the watched process.
fn own_watchdog() -> Option<Duration> {
    let env = |k| std::env::var(k).ok();
    watchdog_interval(env("WATCHDOG_USEC").as_deref(), env("WATCHDOG_PID").as_deref(), std::process::id())
        .filter(|_| std::env::var_os("NOTIFY_SOCKET").is_some())
}

/// Feed the watchdog while the daemon boots (chain replay, service start),
/// until [`start`] runs or [`BOOT_GRACE`] passes. `WatchdogSec=` counts from
/// unit start, and boot may take longer than one interval. Only a runtime
/// that schedules this task feeds it. No-op without a systemd watchdog.
pub fn boot_keepalive() {
    let Some(w) = own_watchdog() else { return };
    let every = round_interval(Some(w));
    tokio::spawn(async move {
        let t = Instant::now();
        while REPORT.get().is_none() && t.elapsed() < BOOT_GRACE {
            sd_notify("WATCHDOG=1");
            tokio::time::sleep(every).await;
        }
        if REPORT.get().is_none() {
            warn!(grace_s = BOOT_GRACE.as_secs(), "boot did not finish in time; watchdog no longer fed");
        }
    });
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `kernel.liveness`: the last round. A daemon that has not started the
/// loop says so (an error), never an empty report.
pub fn rpc() -> clawft_rpc::Response {
    match REPORT.get() {
        Some(r) => {
            let r = r.lock().unwrap_or_else(|p| p.into_inner()).clone();
            match serde_json::to_value(r) {
                Ok(v) => clawft_rpc::Response::success(v),
                Err(e) => clawft_rpc::Response::error(format!("encode failed: {e}")),
            }
        }
        None => clawft_rpc::Response::error("liveness check not running in this daemon"),
    }
}

/// The systemd watchdog interval for this process: `WATCHDOG_USEC`, when
/// `WATCHDOG_PID` is absent or names us (a child that inherited the
/// variables is not the watched process).
pub fn watchdog_interval(usec: Option<&str>, pid: Option<&str>, own_pid: u32) -> Option<Duration> {
    if pid.is_some_and(|p| p.trim() != own_pid.to_string()) {
        return None;
    }
    let us: u64 = usec?.trim().parse().ok()?;
    (us > 0).then(|| Duration::from_micros(us))
}

/// Seconds between rounds: a third of the watchdog interval, at most
/// [`DEFAULT_INTERVAL`].
pub fn round_interval(watchdog: Option<Duration>) -> Duration {
    watchdog.map_or(DEFAULT_INTERVAL, |w| (w / 3).clamp(Duration::from_secs(1), DEFAULT_INTERVAL))
}

/// Send one `sd_notify` datagram. No-op without `NOTIFY_SOCKET`.
#[cfg(unix)]
pub fn sd_notify(state: &str) {
    let Some(sock) = std::env::var_os("NOTIFY_SOCKET") else { return };
    let s = std::os::unix::net::UnixDatagram::unbound();
    let sent = s.and_then(|s| {
        let path = sock.to_string_lossy();
        #[cfg(target_os = "linux")]
        if let Some(name) = path.strip_prefix('@') {
            use std::os::linux::net::SocketAddrExt;
            let addr = std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes())?;
            return s.send_to_addr(state.as_bytes(), &addr);
        }
        s.send_to(state.as_bytes(), PathBuf::from(path.as_ref()))
    });
    if let Err(e) = sent {
        warn!(error = %e, "sd_notify failed");
    }
}

#[cfg(not(unix))]
pub fn sd_notify(_state: &str) {}

async fn timed<F: std::future::Future<Output = Result<(), String>>>(name: &str, f: F) -> Probe {
    let t = Instant::now();
    let r = tokio::time::timeout(PROBE_TIMEOUT, f)
        .await
        .unwrap_or_else(|_| Err(format!("no answer in {}s", PROBE_TIMEOUT.as_secs())));
    Probe { name: name.into(), ok: r.is_ok(), ms: t.elapsed().as_millis() as u64, error: r.err() }
}

/// What a round probes. The daemon gives its kernel (the mesh runtime is
/// found through it) and its RPC socket; `weaver mesh serve` gives only its
/// mesh runtime.
#[derive(Default)]
pub struct Targets {
    /// The daemon's kernel (`kernel_lock`, and `mesh_locks` through it).
    pub kernel: Option<SharedKernel>,
    /// The daemon's RPC socket (`rpc`).
    pub socket: Option<PathBuf>,
    /// A mesh runtime held directly (`mesh_locks`).
    #[cfg(feature = "mesh")]
    pub mesh: Option<Arc<clawft_kernel::mesh_runtime::MeshRuntime>>,
}

#[cfg(feature = "mesh")]
async fn mesh_locks(mesh: Arc<clawft_kernel::mesh_runtime::MeshRuntime>) -> Probe {
    timed("mesh_locks", async move {
        tokio::task::spawn_blocking(move || mesh.lock_probe()).await.map_err(|e| e.to_string())
    })
    .await
}

/// One round of probes. `lag` is the scheduler's overshoot on the last sleep.
async fn round(t: &Targets, lag: Duration) -> Vec<Probe> {
    let mut out = vec![Probe {
        name: "scheduler".into(),
        ok: lag <= MAX_LAG,
        ms: lag.as_millis() as u64,
        error: (lag > MAX_LAG).then(|| format!("woke {}ms late", lag.as_millis())),
    }];
    out.push(
        timed("blocking_pool", async {
            tokio::task::spawn_blocking(|| ()).await.map_err(|e| e.to_string())
        })
        .await,
    );
    #[cfg(feature = "mesh")]
    let mut mesh = t.mesh.clone();
    if let Some(kernel) = &t.kernel {
        out.push(
            timed("kernel_lock", async {
                let k = kernel.read().await;
                #[cfg(feature = "mesh")]
                if mesh.is_none() {
                    mesh = k.a2a_router().mesh_runtime().cloned();
                }
                drop(k);
                Ok(())
            })
            .await,
        );
    }
    #[cfg(feature = "mesh")]
    if let Some(m) = mesh {
        out.push(mesh_locks(m).await);
    }
    #[cfg(unix)]
    if let Some(socket) = &t.socket {
        out.push(
            timed("rpc", async {
                let mut c = clawft_rpc::DaemonClient::connect_path(socket)
                    .await
                    .ok_or_else(|| format!("cannot connect to {}", socket.display()))?;
                // `kernel.liveness`, not `ping`: it is allowed under every
                // outside-project policy, `deny_all` included.
                let r = c.simple_call("kernel.liveness").await.map_err(|e| e.to_string())?;
                if r.ok { Ok(()) } else { Err(r.error.unwrap_or_else(|| "kernel.liveness refused".into())) }
            })
            .await,
        );
    }
    out
}

/// Start the loop (once per process). The daemon calls it after its RPC
/// socket is bound. The loop ends when `shutdown` turns true or its sender
/// is dropped.
pub fn start(targets: Targets, mut shutdown: tokio::sync::watch::Receiver<bool>) {
    if REPORT.set(Mutex::new(Report::default())).is_err() {
        return;
    }
    let watchdog = own_watchdog();
    let every = round_interval(watchdog);
    info!(interval_s = every.as_secs(), watchdog = watchdog.is_some(), "liveness check started");
    if watchdog.is_some() {
        sd_notify("READY=1");
    }
    tokio::spawn(async move {
        let mut lag = Duration::ZERO;
        loop {
            let probes = round(&targets, lag).await;
            let ok = probes.iter().all(|p| p.ok);
            let now = now_secs();
            {
                let mut r = REPORT.get().expect("set above").lock().unwrap_or_else(|p| p.into_inner());
                r.last_round = now;
                r.interval_s = every.as_secs();
                r.watchdog = watchdog.is_some();
                if ok {
                    r.last_ok = now;
                    r.failures = 0;
                } else {
                    r.failures += 1;
                }
                r.probes = probes.clone();
            }
            if ok {
                if watchdog.is_some() {
                    sd_notify("WATCHDOG=1");
                }
            } else {
                let failed: Vec<_> = probes.iter().filter(|p| !p.ok).map(|p| p.name.as_str()).collect();
                warn!(?failed, "liveness round failed; watchdog not fed");
            }
            let t = Instant::now();
            tokio::select! {
                _ = tokio::time::sleep(every) => {}
                r = shutdown.changed() => {
                    if r.is_err() || *shutdown.borrow() {
                        return;
                    }
                }
            }
            lag = t.elapsed().saturating_sub(every);
        }
    });
}

/// How long `weaver kernel liveness` waits for the daemon to answer.
pub const CLI_TIMEOUT: Duration = Duration::from_secs(10);

/// `weaver kernel liveness`: print the report; `Err` (exit 1) when the
/// daemon does not answer within [`CLI_TIMEOUT`] or its verdict fails. A
/// launchd or cron probe can run it and `weaver kernel stop --force` on
/// failure.
pub async fn cli(json: bool) -> anyhow::Result<()> {
    let ask = async {
        let mut c = clawft_rpc::DaemonClient::connect()
            .await
            .ok_or_else(|| anyhow::anyhow!("no daemon answers at {}", crate::protocol::socket_path().display()))?;
        c.simple_call("kernel.liveness").await
    };
    let resp = tokio::time::timeout(CLI_TIMEOUT, ask).await.map_err(|_| {
        anyhow::anyhow!("daemon did not answer within {}s: it may be wedged", CLI_TIMEOUT.as_secs())
    })??;
    if !resp.ok {
        anyhow::bail!("{}", resp.error.unwrap_or_else(|| "kernel.liveness refused".into()));
    }
    let report: Report = serde_json::from_value(resp.result.unwrap_or_default())?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        for p in &report.probes {
            let mark = if p.ok { "ok  " } else { "FAIL" };
            println!("{mark} {:<14} {:>6} ms {}", p.name, p.ms, p.error.as_deref().unwrap_or(""));
        }
        println!(
            "watchdog: {}  interval: {}s  failures in a row: {}",
            if report.watchdog { "systemd" } else { "off" },
            report.interval_s,
            report.failures
        );
    }
    report.verdict(now_secs()).map_err(|e| anyhow::anyhow!("unhealthy: {e}"))?;
    if !json {
        println!("live");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(name: &str, ok: bool) -> Probe {
        Probe { name: name.into(), ok, ms: 1, error: (!ok).then(|| "stuck".into()) }
    }

    #[test]
    fn watchdog_is_ours_only_when_the_pid_matches_or_is_absent() {
        assert_eq!(watchdog_interval(Some("30000000"), None, 7), Some(Duration::from_secs(30)));
        assert_eq!(watchdog_interval(Some("30000000"), Some("7"), 7), Some(Duration::from_secs(30)));
        assert_eq!(watchdog_interval(Some("30000000"), Some("8"), 7), None, "inherited by a child");
        assert_eq!(watchdog_interval(None, None, 7), None);
        assert_eq!(watchdog_interval(Some("0"), None, 7), None);
        assert_eq!(watchdog_interval(Some("soon"), None, 7), None);
    }

    #[test]
    fn rounds_run_three_times_per_watchdog_interval_at_most_every_ten_seconds() {
        assert_eq!(round_interval(None), DEFAULT_INTERVAL);
        assert_eq!(round_interval(Some(Duration::from_secs(60))), DEFAULT_INTERVAL);
        assert_eq!(round_interval(Some(Duration::from_secs(15))), Duration::from_secs(5));
        assert_eq!(round_interval(Some(Duration::from_millis(900))), Duration::from_secs(1));
    }

    #[test]
    fn the_verdict_refuses_a_never_passed_stale_or_failing_report() {
        let now = 1_000;
        let good = Report { last_ok: now - 5, last_round: now - 5, interval_s: 10, probes: vec![probe("rpc", true)], ..Default::default() };
        assert_eq!(good.verdict(now), Ok(()));
        let never = Report::default();
        assert!(never.verdict(now).unwrap_err().contains("no liveness round"));
        let stale = Report { last_ok: now - 31, ..good.clone() };
        assert!(stale.verdict(now).unwrap_err().contains("stale"));
        let failing = Report { probes: vec![probe("mesh_locks", false)], ..good };
        assert!(failing.verdict(now).unwrap_err().contains("mesh_locks"));
    }

    #[cfg(unix)]
    #[test]
    fn sd_notify_sends_the_state_to_the_notify_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("n.sock");
        let rx = std::os::unix::net::UnixDatagram::bind(&path).unwrap();
        // The variable is read only here and in `start`; no other test sets it.
        unsafe { std::env::set_var("NOTIFY_SOCKET", &path) };
        sd_notify("WATCHDOG=1");
        unsafe { std::env::remove_var("NOTIFY_SOCKET") };
        let mut buf = [0u8; 64];
        rx.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let n = rx.recv(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"WATCHDOG=1");
    }
}
