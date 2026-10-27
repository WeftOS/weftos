//! `weaver kernel stop` against a wedged, test-owned daemon (card 1a919ae6):
//! the stop completes in bounded time, finds the daemon by its run-dir lock
//! when the pid file is gone, and never deletes the pid file of a daemon
//! that is still alive. Never touches the real `~/.weftos` / `~/.clawft` and
//! only signals the child it spawned.
#![cfg(unix)]
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Short root: macOS limits a Unix socket path to ~104 bytes.
fn dir() -> tempfile::TempDir {
    let root = PathBuf::from(std::env::var_os("HOME").expect("HOME")).join(".cache/wrs-e2e");
    std::fs::create_dir_all(&root).unwrap();
    tempfile::Builder::new().prefix("s").tempdir_in(root).unwrap()
}

fn wait_for(what: &str, limit: Duration, mut f: impl FnMut() -> bool) {
    let t = Instant::now();
    while t.elapsed() < limit {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("timed out waiting for {what}");
}

/// A daemon this test started. A reaper thread waits on it, so a dead
/// daemon is not left a zombie that `kill -0` (and so `kernel stop`) would
/// still see as alive. Dropping the guard KILLs whatever is left.
struct Daemon {
    pid: u32,
    exited: mpsc::Receiver<()>,
    root: tempfile::TempDir,
}

impl Daemon {
    fn boot(extra_env: &[(&str, &str)]) -> Self {
        let root = dir();
        std::fs::create_dir_all(root.path().join("h")).unwrap();
        let out = std::fs::File::create(root.path().join("out.log")).unwrap();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_weaver"));
        cmd.args(["kernel", "start", "--foreground"]);
        env(&mut cmd, root.path());
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
        let mut child = cmd
            .stdin(Stdio::null())
            .stdout(out.try_clone().unwrap())
            .stderr(out)
            .spawn()
            .unwrap();
        let pid = child.id();
        let (tx, exited) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = child.wait();
            let _ = tx.send(());
        });
        let d = Self { pid, exited, root };
        let sock = d.rt().join("kernel.sock");
        // Generous: macOS scans a freshly linked binary on first launch, and
        // these tests start three daemons at once.
        wait_for("boot", Duration::from_secs(240), || {
            d.log().contains("Daemon listening on") && UnixStream::connect(&sock).is_ok()
        });
        wait_for("pid file", Duration::from_secs(10), || d.rt().join("kernel.pid").exists());
        d
    }

    fn rt(&self) -> PathBuf {
        self.root.path().join("rt")
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.root.path().join("out.log")).unwrap_or_default()
    }

    fn signal(&self, sig: &str) {
        assert!(Command::new("kill").args([sig, &self.pid.to_string()]).status().unwrap().success());
    }

    /// `weaver kernel <args>` against this daemon's run dir, timed.
    fn weaver(&self, args: &[&str]) -> (Output, Duration) {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_weaver"));
        cmd.arg("kernel").args(args);
        env(&mut cmd, self.root.path());
        let t = Instant::now();
        let out = cmd.stdin(Stdio::null()).output().unwrap();
        (out, t.elapsed())
    }

    fn gone_within(&self, limit: Duration) -> bool {
        self.exited.recv_timeout(limit).is_ok()
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = Command::new("kill").args(["-KILL", &self.pid.to_string()]).status();
    }
}

fn env(cmd: &mut Command, root: &Path) {
    cmd.env("HOME", root.join("h"))
        .env("WEFTOS_RUNTIME_DIR", root.join("rt"))
        .env_remove("WEAVER_PROFILE")
        .current_dir(root);
}

fn text(o: &Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
}

#[test]
fn stop_completes_when_a_shutdown_step_wedges_its_thread() {
    // Debug builds only: the hook blocks the shutdown thread for an hour,
    // like a sync lock held by a stuck task. Only the guard can end it.
    let d = Daemon::boot(&[("WEFTOS_TEST_WEDGE_SHUTDOWN", "1")]);
    let (out, took) = d.weaver(&["stop"]);
    assert!(out.status.success(), "{}\n--- daemon log ---\n{}", text(&out), d.log());
    assert!(took < Duration::from_secs(12), "stop took {took:?}");
    assert!(d.gone_within(Duration::from_secs(2)), "daemon still alive after stop");
    assert!(d.log().contains("shutdown exceeded its budget"), "{}", d.log());
    assert!(!d.rt().join("kernel.pid").exists(), "the exiting daemon removes its pid file");
}

#[test]
fn sigterm_ends_a_daemon_whose_runtime_never_acts_on_it() {
    // Debug builds only: the hook blocks the daemon's main task before it
    // waits for a signal, so tokio takes TERM and nothing acts on it (the
    // 2026-10-13 "SIGTERM ignored"). Only the signal watcher can end it.
    let d = Daemon::boot(&[("WEFTOS_TEST_WEDGE_SIGNALS", "1")]);
    wait_for("the signal wedge", Duration::from_secs(10), || d.log().contains("WEFTOS_TEST_WEDGE_SIGNALS set"));
    let t = Instant::now();
    d.signal("-TERM");
    assert!(d.gone_within(Duration::from_secs(12)), "TERM ignored\n--- daemon log ---\n{}", d.log());
    assert!(t.elapsed() < Duration::from_secs(12), "{:?}", t.elapsed());
    assert!(d.log().contains("no exit within the shutdown budget of the first signal"), "{}", d.log());
    assert!(!d.rt().join("kernel.pid").exists(), "the exiting daemon removes its pid file");
}

#[test]
fn a_second_sigterm_during_a_stuck_shutdown_exits_at_once() {
    let d = Daemon::boot(&[("WEFTOS_TEST_WEDGE_SHUTDOWN", "1")]);
    d.signal("-TERM");
    wait_for("the wedged shutdown", Duration::from_secs(5), || d.log().contains("WEFTOS_TEST_WEDGE_SHUTDOWN set"));
    let t = Instant::now();
    d.signal("-TERM");
    assert!(d.gone_within(Duration::from_secs(3)), "second TERM ignored\n--- daemon log ---\n{}", d.log());
    // Well inside the 8 s the budget guard alone would take.
    assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
    assert!(d.log().contains("SIGTERM during shutdown"), "{}", d.log());
    assert!(!d.rt().join("kernel.pid").exists(), "the exiting daemon removes its pid file");
}

#[test]
fn stop_force_finds_a_wedged_daemon_by_its_lock_without_a_pid_file() {
    let d = Daemon::boot(&[]);
    // The 2026-10-13 state: no pid file, and TERM is not acted on.
    std::fs::remove_file(d.rt().join("kernel.pid")).unwrap();
    d.signal("-STOP");
    let (out, took) = d.weaver(&["stop", "--force"]);
    assert!(out.status.success(), "{}", text(&out));
    let said = text(&out);
    assert!(said.contains("holds the run-dir lock"), "{said}");
    assert!(said.contains("SIGKILL"), "{said}");
    assert!(took < Duration::from_secs(20), "stop --force took {took:?}");
    assert!(d.gone_within(Duration::from_secs(2)), "daemon survived stop --force");
}

#[test]
fn plain_stop_of_a_wedged_daemon_fails_and_keeps_its_pid_file() {
    let d = Daemon::boot(&[]);
    // Live: the self-check passes (its first round runs right after boot).
    wait_for("a passing liveness round", Duration::from_secs(30), || d.weaver(&["liveness"]).0.status.success());
    d.signal("-STOP");
    // Wedged: the self-check says so instead of hanging.
    let (out, took) = d.weaver(&["liveness"]);
    assert!(!out.status.success(), "a stopped daemon passed liveness: {}", text(&out));
    assert!(text(&out).contains("did not answer"), "{}", text(&out));
    assert!(took < Duration::from_secs(15), "liveness took {took:?}");
    let (out, _) = d.weaver(&["stop"]);
    assert!(!out.status.success(), "a daemon that did not exit is not stopped: {}", text(&out));
    assert!(text(&out).contains("--force"), "{}", text(&out));
    assert!(d.rt().join("kernel.pid").exists(), "a live daemon keeps its pid file");
    // And the follow-up the message suggests works.
    let (out, _) = d.weaver(&["stop", "--force"]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(d.gone_within(Duration::from_secs(2)));
}
