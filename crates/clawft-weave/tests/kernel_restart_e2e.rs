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

struct Kill(Child);
impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn sighup_restart_comes_back_with_the_same_pid() {
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

    let status = Command::new("kill").args(["-HUP", &pid.to_string()]).status().unwrap();
    assert!(status.success());

    wait_for("second boot", || listening_count(&log) == 2 && UnixStream::connect(&sock).is_ok());
    let text = std::fs::read_to_string(&log).unwrap();
    assert!(!text.contains("another kernel owns"), "{text}");
    assert!(!text.contains("re-exec failed"), "{text}");
    // Same process (exec keeps the pid), still alive, pid file names it.
    assert!(Command::new("kill").args(["-0", &pid.to_string()]).status().unwrap().success());
    assert_eq!(std::fs::read_to_string(rt.join("kernel.pid")).unwrap().trim(), pid.to_string());
}
