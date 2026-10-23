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
//! The exit code is 0 for a stop (a service manager must not restart what was
//! asked to stop) and 1 when a restart was asked for (so it brings it back).

use std::future::Future;
use std::path::PathBuf;
use std::sync::OnceLock;
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

static STOP_SENTINEL: OnceLock<PathBuf> = OnceLock::new();

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
        let s = Self::unguarded(budget);
        let at = s.deadline + HARD_EXIT_GRACE;
        let spawned = std::thread::Builder::new()
            .name("shutdown-guard".into())
            .spawn(move || {
                std::thread::sleep(at.saturating_duration_since(Instant::now()));
                hard_exit(&exit, budget)
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

fn hard_exit(exit: &HardExit, budget: Duration) -> ! {
    error!(
        budget_s = budget.as_secs(),
        code = exit.code,
        "shutdown exceeded its budget (a wedged task or lock); exiting now"
    );
    eprintln!("weaver: shutdown exceeded {}s; exiting", budget.as_secs());
    if exit.code == 0
        && let Some(s) = STOP_SENTINEL.get()
    {
        crate::boot_refusal::write_refused(s, "stopped (shutdown exceeded its budget)");
    }
    let _ = std::fs::remove_file(&exit.pid_file);
    std::process::exit(exit.code)
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

    #[tokio::test]
    async fn the_final_step_gets_only_what_is_left() {
        let s = Shutdown::unguarded(Duration::from_millis(200));
        let t = Instant::now();
        assert!(s.final_step("kernel", std::future::pending::<()>()).await.is_none());
        assert!(t.elapsed() < Duration::from_secs(1));
    }
}
