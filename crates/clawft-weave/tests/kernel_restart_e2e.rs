//! `weaver kernel restart` (SIGHUP re-exec) on a test-owned daemon: the same
//! pid comes back up and the socket answers again. Never touches the real
//! `~/.weftos` / `~/.clawft` and only signals the child it spawned.
#![cfg(unix)]
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Ceiling for a spawned weaver to start and serve. The debug weaver is ~170 MB
/// and on macOS its exec can sit in dyld/code-signature checks for tens of
/// seconds when many test processes start at once (nextest runs a whole crate
/// in parallel); the daemon itself boots in well under a second. A hang still
/// fails, just later.
const BOOT_WAIT: Duration = Duration::from_secs(300);

/// Short root: macOS limits a Unix socket path to ~104 bytes, and a worktree
/// checkout or the per-user temp dir overflows it.
fn dir() -> tempfile::TempDir {
    let root = PathBuf::from(std::env::var_os("HOME").expect("HOME")).join(".cache/wrs-e2e");
    std::fs::create_dir_all(&root).unwrap();
    tempfile::Builder::new().prefix("r").tempdir_in(root).unwrap()
}

fn listening_count(log: &Path) -> usize {
    std::fs::read_to_string(log)
        .map(|s| s.matches("Daemon listening on").count())
        .unwrap_or(0)
}

fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    let t = Instant::now();
    while t.elapsed() < BOOT_WAIT {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("timed out waiting for {what}");
}

/// SIGHUP a daemon this test spawned (never any other process).
fn hup(pid: u32) {
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), nix::sys::signal::Signal::SIGHUP).unwrap();
}

struct Kill(Child);
impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Start a foreground daemon under a throwaway HOME/runtime dir, SIGHUP it,
/// and check the re-exec comes back as the same pid with a working socket.
/// `extra` are added to `kernel start --foreground`; `rt_env` says whether
/// `WEFTOS_RUNTIME_DIR` is set (otherwise the runtime root resolves from
/// `cwd`/`HOME`, as a project-rooted or user daemon does); `sock_rel` is the
/// socket path relative to the throwaway HOME (or to the runtime dir).
fn restart_keeps_pid(extra: &[&str], rt_env: bool, project_cwd: bool, sock: impl Fn(&Path, &Path) -> PathBuf) {
    let root = dir();
    let home = root.path().join("h");
    let rt = root.path().join("rt");
    std::fs::create_dir_all(&home).unwrap();
    let cwd = if project_cwd {
        let p = home.join("work/app");
        std::fs::create_dir_all(p.join(".weftos/runtime")).unwrap();
        std::fs::write(p.join(".weftos/project.toml"), "").unwrap();
        p
    } else {
        root.path().to_path_buf()
    };
    let log = root.path().join("out.log");
    let out = std::fs::File::create(&log).unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_weaver"));
    cmd.args(["kernel", "start", "--foreground"])
        .args(extra)
        .env("HOME", &home)
        .env_remove("WEAVER_PROFILE")
        .env_remove("WEFTOS_RUNTIME_DIR")
        .env_remove("WEFTOS_REEXEC")
        .current_dir(&cwd)
        .stdin(Stdio::null())
        .stdout(out.try_clone().unwrap())
        .stderr(out);
    if rt_env {
        cmd.env("WEFTOS_RUNTIME_DIR", &rt);
    }
    let child = cmd.spawn().unwrap();
    let pid = child.id();
    let _guard = Kill(child);
    let sock = sock(&home, &rt);
    wait_for("first boot", || listening_count(&log) == 1 && UnixStream::connect(&sock).is_ok());

    hup(pid);

    wait_for("second boot", || {
        let t = std::fs::read_to_string(&log).unwrap_or_default();
        let dead = t.contains("another kernel owns")
            || t.contains("in use by another kernel")
            || t.contains("already running")
            || t.contains("re-exec failed");
        assert!(!dead, "the re-exec refused its own pid:\n{t}");
        listening_count(&log) == 2 && UnixStream::connect(&sock).is_ok()
    });
    let text = std::fs::read_to_string(&log).unwrap();
    assert!(!text.contains("another kernel owns"), "{text}");
    assert!(!text.contains("re-exec failed"), "{text}");
    // Same process (exec keeps the pid), still alive, pid file names it.
    assert!(nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None).is_ok());
    let pidfile = sock_dir(&sock).join("kernel.pid");
    assert_eq!(std::fs::read_to_string(pidfile).unwrap().trim(), pid.to_string());
}

fn sock_dir(sock: &Path) -> &Path {
    sock.parent().unwrap()
}

#[test]
fn sighup_restart_comes_back_with_the_same_pid() {
    restart_keeps_pid(&[], true, false, |_, rt| rt.join("kernel.sock"));
}

/// The user daemon (`--profile user`): launchd and systemd start it this way.
#[test]
fn sighup_restart_of_the_user_profile_daemon_comes_back_with_the_same_pid() {
    restart_keeps_pid(&["--profile", "user"], false, false, |home, _| home.join(".weftos/run/kernel.sock"));
}

/// A project-rooted daemon: no runtime dir in the environment, the root
/// resolves from the project the daemon was started in.
#[test]
fn sighup_restart_of_a_project_rooted_daemon_comes_back_with_the_same_pid() {
    restart_keeps_pid(&["--legacy-project-daemon"], false, true, |home, _| {
        home.join("work/app/.weftos/runtime/kernel.sock")
    });
}

/// A process that inherited the chain-lock descriptor across the re-exec
/// still holds `chain.lock`, and the file names the daemon's own pid (exec
/// keeps it). The re-exec must wait for that descriptor to close instead of
/// refusing with "in use by another kernel (pid <its own pid>)".
#[test]
fn sighup_restart_waits_for_an_inherited_chain_lock_under_its_own_pid() {
    use nix::fcntl::{Flock, FlockArg};
    use std::io::Write;
    let root = dir();
    let home = root.path().join("h");
    let rt = root.path().join("rt");
    std::fs::create_dir_all(&home).unwrap();
    let log = root.path().join("out.log");
    let out = std::fs::File::create(&log).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_weaver"))
        .args(["kernel", "start", "--foreground"])
        .env("HOME", &home)
        .env("WEFTOS_RUNTIME_DIR", &rt)
        .env_remove("WEAVER_PROFILE")
        .env_remove("WEFTOS_REEXEC")
        .current_dir(root.path())
        .stdin(Stdio::null())
        .stdout(out.try_clone().unwrap())
        .stderr(out)
        .spawn()
        .unwrap();
    let pid = child.id();
    let _guard = Kill(child);
    let sock = rt.join("kernel.sock");
    wait_for("first boot", || listening_count(&log) == 1 && UnixStream::connect(&sock).is_ok());
    let lock_path = rt.join("chain.lock");
    assert!(lock_path.exists(), "no chain.lock in {}", rt.display());

    hup(pid);

    // Stand in for the inherited descriptor: take chain.lock the instant the
    // old image releases it (at exec), write the daemon's pid into it, and
    // hold it for a while.
    let mut f = {
        let t = Instant::now();
        loop {
            let file = std::fs::OpenOptions::new().read(true).write(true).open(&lock_path).unwrap();
            match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
                Ok(g) => break g,
                Err(_) => {
                    assert!(t.elapsed() < Duration::from_secs(60), "the old image never released chain.lock");
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        }
    };
    f.set_len(0).unwrap();
    write!(f, "{pid}").unwrap();
    f.flush().unwrap();
    std::thread::sleep(Duration::from_millis(2500));
    drop(f); // closes the descriptor, releasing the lock

    wait_for("second boot", || {
        let t = std::fs::read_to_string(&log).unwrap_or_default();
        assert!(!t.contains("in use by another kernel"), "the re-exec refused its own pid:\n{t}");
        assert!(!t.contains("the kernel is NOT running"), "{t}");
        listening_count(&log) == 2 && UnixStream::connect(&sock).is_ok()
    });
    assert_eq!(std::fs::read_to_string(rt.join("kernel.pid")).unwrap().trim(), pid.to_string());
}
