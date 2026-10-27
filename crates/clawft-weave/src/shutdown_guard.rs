//! Bounded daemon shutdown (card 1a919ae6).
//!
//! `weaver kernel stop` waits 10 s. A daemon whose runtime threads are
//! wedged (the 2026-10-13 incident: TERM taken, router gone, process still
//! holding the leaf listeners and the run dir) must not outlive that wait:
//!
//! - [`Shutdown::begin`] arms a guard on its own OS thread. It does not need
//!   the async runtime, so it fires even when every runtime thread is blocked.
//!   Past the budget it logs, removes `kernel.pid` and exits the process.
//! - [`Shutdown::step`] bounds and logs each shutdown step. A step that runs
//!   out of time is abandoned and the next one starts; the final step (the
//!   kernel: services, chain checkpoint) keeps [`KERNEL_RESERVE`] for itself.
//!
//! - [`arm_signal_escalation`] watches SIGTERM / SIGINT / SIGHUP from its own
//!   OS thread, outside tokio. A daemon whose runtime is too wedged to see the
//!   signal (so [`Shutdown::begin`] never runs) still exits within the budget
//!   of the first signal, and a SIGTERM that arrives while a shutdown is
//!   already under way ends the process at once.
//!
//! The exit code is 0 for a stop (a service manager must not restart what was
//! asked to stop) and 1 when a restart was asked for (so it brings it back).

use std::future::Future;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicIsize, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use tracing::{error, info, warn};

/// Whole graceful shutdown for a top-level daemon, inside the CLI's 10 s wait.
pub const SHUTDOWN_BUDGET: Duration = Duration::from_secs(7);
/// A nested instance stops its project kernels too, inside its owner's 15 s
/// graceful window.
pub const NESTED_SHUTDOWN_BUDGET: Duration = Duration::from_secs(13);
/// Time past the budget before the guard ends the process.
pub const HARD_EXIT_GRACE: Duration = Duration::from_secs(1);
/// Kept back from earlier steps for the final (kernel) step.
pub const KERNEL_RESERVE: Duration = Duration::from_secs(3);
/// Exit code of a hard exit that ends a restart (`kernel restart` / SIGHUP).
pub const HARD_EXIT_RESTART: i32 = 1;

/// Extra wait, past the in-runtime guard's own deadline, before the signal
/// watcher ends the process: the in-runtime guard normally fires first.
pub const SIGNAL_WATCH_SLACK: Duration = Duration::from_secs(1);

static STOP_SENTINEL: OnceLock<PathBuf> = OnceLock::new();
/// SIGTERM + SIGINT seen by the signal-handler action.
static TERMS: AtomicUsize = AtomicUsize::new(0);
/// SIGHUP seen by the signal-handler action.
static HUPS: AtomicUsize = AtomicUsize::new(0);
/// `TERMS` when [`Shutdown::begin`] ran; -1 before.
static TERMS_AT_BEGIN: AtomicIsize = AtomicIsize::new(-1);

/// The launchd "do not restart" sentinel a clean stop leaves (user profile).
/// The guard writes it too, so a hard exit is not relaunched by `KeepAlive`.
pub fn set_stop_sentinel(path: PathBuf) {
    let _ = STOP_SENTINEL.set(path);
}

/// What the guard does when it fires.
#[derive(Debug, Clone)]
pub struct HardExit {
    /// `kernel.pid`, removed just before the exit.
    pub pid_file: PathBuf,
    /// Process exit code.
    pub code: i32,
}

/// One shutdown in progress: a deadline and the guard behind it.
#[derive(Debug)]
pub struct Shutdown {
    started: Instant,
    deadline: Instant,
}

impl Shutdown {
    /// Start a shutdown of at most `budget`; the guard ends the process
    /// `HARD_EXIT_GRACE` after it.
    pub fn begin(budget: Duration, exit: HardExit) -> Self {
        TERMS_AT_BEGIN.store(TERMS.load(Ordering::SeqCst) as isize, Ordering::SeqCst);
        let s = Self::unguarded(budget);
        let at = s.deadline + HARD_EXIT_GRACE;
        let spawned = std::thread::Builder::new()
            .name("shutdown-guard".into())
            .spawn(move || {
                std::thread::sleep(at.saturating_duration_since(Instant::now()));
                hard_exit(&exit, &format!("shutdown exceeded its budget ({}s)", budget.as_secs()))
            });
        if let Err(e) = spawned {
            warn!(error = %e, "shutdown guard thread did not start; shutdown is unbounded");
        }
        info!(budget_ms = budget.as_millis() as u64, "daemon shutdown started");
        s
    }

    /// A deadline with no guard thread (tests).
    pub fn unguarded(budget: Duration) -> Self {
        let started = Instant::now();
        Self { started, deadline: started + budget }
    }

    /// Run one step for at most `cap`, and never into the time kept for the
    /// kernel step. `None`: it ran out of time and was abandoned.
    pub async fn step<T>(&self, name: &str, cap: Duration, fut: impl Future<Output = T>) -> Option<T> {
        let left = self.remaining().saturating_sub(KERNEL_RESERVE);
        self.run(name, cap.min(left), fut).await
    }

    /// The last step: whatever time is left.
    pub async fn final_step<T>(&self, name: &str, fut: impl Future<Output = T>) -> Option<T> {
        self.run(name, self.remaining(), fut).await
    }

    /// Time left before the deadline.
    pub fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }

    async fn run<T>(&self, name: &str, limit: Duration, fut: impl Future<Output = T>) -> Option<T> {
        let t = Instant::now();
        info!(step = name, limit_ms = limit.as_millis() as u64, "shutdown step");
        match tokio::time::timeout(limit, fut).await {
            Ok(v) => {
                info!(step = name, elapsed_ms = t.elapsed().as_millis() as u64, "shutdown step done");
                Some(v)
            }
            Err(_) => {
                warn!(
                    step = name,
                    limit_ms = limit.as_millis() as u64,
                    since_start_ms = self.started.elapsed().as_millis() as u64,
                    "shutdown step timed out; abandoned"
                );
                None
            }
        }
    }
}

/// End the process now. Files first and logging last, each without waiting on
/// a lock a wedged thread may hold: stderr is written raw, and the tracing
/// line gets a moment on a thread of its own.
fn hard_exit(exit: &HardExit, why: &str) -> ! {
    if exit.code == 0
        && let Some(s) = STOP_SENTINEL.get()
    {
        crate::boot_refusal::write_refused(s, &format!("stopped ({why})"));
    }
    let _ = std::fs::remove_file(&exit.pid_file);
    raw_stderr(&format!("weaver: {why}; exiting\n"));
    let (code, line) = (exit.code, why.to_string());
    let logged = std::thread::Builder::new().name("shutdown-log".into()).spawn(move || {
        error!(code, reason = %line, "hard exit: shutdown exceeded its budget (a wedged task or lock); exiting now");
    });
    if let Ok(h) = logged {
        let t = Instant::now();
        while !h.is_finished() && t.elapsed() < Duration::from_millis(300) {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    std::process::exit(exit.code)
}

fn raw_stderr(msg: &str) {
    #[cfg(unix)]
    // SAFETY: write(2) on fd 2 with a valid buffer; the result is ignored.
    unsafe {
        let _ = libc::write(2, msg.as_ptr().cast(), msg.len());
    }
    #[cfg(not(unix))]
    eprint!("{msg}");
}

/// What the signal watcher does with the counts it sees.
#[derive(Debug, PartialEq, Eq)]
#[cfg_attr(not(unix), allow(dead_code))]
enum Verdict {
    Wait,
    /// Exit now with this code, for this reason.
    Exit(i32, &'static str),
}

#[cfg_attr(not(unix), allow(dead_code))]
fn verdict(terms: usize, hups: usize, terms_at_begin: Option<usize>, since_first: Option<Duration>, limit: Duration) -> Verdict {
    let code = if terms > 0 { 0 } else { HARD_EXIT_RESTART };
    if terms_at_begin.is_some_and(|at| terms > at) {
        return Verdict::Exit(0, "SIGTERM during shutdown");
    }
    if terms > 0 && terms + hups >= 2 {
        return Verdict::Exit(0, "second signal before shutdown began (the runtime did not act on the first)");
    }
    if since_first.is_some_and(|d| d >= limit) {
        return Verdict::Exit(code, "no exit within the shutdown budget of the first signal");
    }
    Verdict::Wait
}

/// Count SIGTERM / SIGINT / SIGHUP in the signal handler itself and act on
/// the counts from an OS thread, so neither depends on the async runtime
/// (card 1a919ae6: a daemon whose runtime threads were wedged took TERM and
/// never acted on it). tokio's own handlers keep running beside these and
/// drive the graceful path. `budget` is the shutdown budget the daemon uses.
#[cfg(unix)]
pub fn arm_signal_escalation(budget: Duration, pid_file: PathBuf) {
    static ARMED: OnceLock<()> = OnceLock::new();
    if ARMED.set(()).is_err() {
        return;
    }
    for (sig, counter) in [(libc::SIGTERM, &TERMS), (libc::SIGINT, &TERMS), (libc::SIGHUP, &HUPS)] {
        // SAFETY: the action only increments an atomic, which is
        // async-signal-safe.
        let r = unsafe {
            signal_hook_registry::register(sig, move || {
                counter.fetch_add(1, Ordering::SeqCst);
            })
        };
        if let Err(e) = r {
            warn!(signal = sig, error = %e, "signal escalation not armed for this signal");
        }
    }
    let limit = budget + HARD_EXIT_GRACE + SIGNAL_WATCH_SLACK;
    let spawned = std::thread::Builder::new().name("signal-escalation".into()).spawn(move || {
        let mut first: Option<Instant> = None;
        loop {
            std::thread::sleep(Duration::from_millis(50));
            let (terms, hups) = (TERMS.load(Ordering::SeqCst), HUPS.load(Ordering::SeqCst));
            if first.is_none() && terms + hups > 0 {
                first = Some(Instant::now());
            }
            let at = usize::try_from(TERMS_AT_BEGIN.load(Ordering::SeqCst)).ok();
            if let Verdict::Exit(code, why) = verdict(terms, hups, at, first.map(|t| t.elapsed()), limit) {
                hard_exit(&HardExit { pid_file: pid_file.clone(), code }, why);
            }
        }
    });
    if let Err(e) = spawned {
        warn!(error = %e, "signal escalation thread did not start");
    }
}

/// Debug builds only: `WEFTOS_TEST_WEDGE_SHUTDOWN=1` makes the first
/// shutdown step block its thread for an hour, the way a sync lock held by a
/// stuck task blocks a runtime thread, so a test can prove the guard ends the
/// process anyway. Release builds ignore the variable.
pub fn test_wedge() {
    #[cfg(debug_assertions)]
    if std::env::var_os("WEFTOS_TEST_WEDGE_SHUTDOWN").is_some_and(|v| v == "1") {
        warn!("WEFTOS_TEST_WEDGE_SHUTDOWN set: blocking this thread (test hook)");
        std::thread::sleep(Duration::from_secs(3600));
    }
}

/// Debug builds only: `WEFTOS_TEST_WEDGE_SIGNALS=1` blocks the daemon's main
/// task for an hour just before it waits for a signal, so tokio never acts
/// on SIGTERM. Only [`arm_signal_escalation`] can end the process then.
/// Release builds ignore the variable.
pub fn test_wedge_before_signals() {
    #[cfg(debug_assertions)]
    if std::env::var_os("WEFTOS_TEST_WEDGE_SIGNALS").is_some_and(|v| v == "1") {
        warn!("WEFTOS_TEST_WEDGE_SIGNALS set: blocking the signal wait (test hook)");
        std::thread::sleep(Duration::from_secs(3600));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_step_that_never_finishes_is_abandoned_at_its_cap() {
        let s = Shutdown::unguarded(Duration::from_secs(30));
        let t = Instant::now();
        let r = s.step("stuck", Duration::from_millis(100), std::future::pending::<()>()).await;
        assert!(r.is_none());
        assert!(t.elapsed() < Duration::from_secs(2));
        assert_eq!(s.step("quick", Duration::from_secs(1), async { 7 }).await, Some(7));
    }

    #[tokio::test]
    async fn early_steps_leave_the_kernel_reserve() {
        // A 4 s budget leaves 1 s for early steps, whatever their cap says.
        let s = Shutdown::unguarded(KERNEL_RESERVE + Duration::from_secs(1));
        let t = Instant::now();
        assert!(s.step("long", Duration::from_secs(60), std::future::pending::<()>()).await.is_none());
        let spent = t.elapsed();
        assert!(spent >= Duration::from_millis(900) && spent < Duration::from_secs(2), "{spent:?}");
        // The final step still has about the reserve.
        assert!(s.remaining() >= KERNEL_RESERVE - Duration::from_millis(200));
    }

    #[test]
    fn signal_verdicts() {
        let lim = Duration::from_secs(9);
        let s = |d: u64| Some(Duration::from_secs(d));
        assert_eq!(verdict(0, 0, None, None, lim), Verdict::Wait);
        // One TERM: the graceful path gets the whole budget.
        assert_eq!(verdict(1, 0, None, s(8), lim), Verdict::Wait);
        assert_eq!(verdict(1, 0, Some(1), s(8), lim), Verdict::Wait);
        // ...but not longer, even if the runtime never began a shutdown.
        assert!(matches!(verdict(1, 0, None, s(9), lim), Verdict::Exit(0, _)));
        // A wedged restart exits 1 so the service manager brings it back.
        assert!(matches!(verdict(0, 1, None, s(9), lim), Verdict::Exit(1, _)));
        // A TERM while a shutdown is under way ends it now.
        assert!(matches!(verdict(2, 0, Some(1), s(1), lim), Verdict::Exit(0, _)));
        assert!(matches!(verdict(1, 0, Some(0), s(1), lim), Verdict::Exit(0, _)));
        // A second signal the runtime never acted on ends it now.
        assert!(matches!(verdict(2, 0, None, s(1), lim), Verdict::Exit(0, _)));
        assert!(matches!(verdict(1, 1, None, s(1), lim), Verdict::Exit(0, _)));
        // Two HUPs are left to the deadline.
        assert_eq!(verdict(0, 2, None, s(1), lim), Verdict::Wait);
    }

    #[tokio::test]
    async fn the_final_step_gets_only_what_is_left() {
        let s = Shutdown::unguarded(Duration::from_millis(200));
        let t = Instant::now();
        assert!(s.final_step("kernel", std::future::pending::<()>()).await.is_none());
        assert!(t.elapsed() < Duration::from_secs(1));
    }
}
