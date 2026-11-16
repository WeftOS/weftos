//! WO-069 J1: a runtime dir whose daemon sockets overflow `sun_path` is
//! refused up front, with the path named, instead of dying after
//! "Daemon listening" with a bare `path must be shorter than SUN_LEN`.
//! Spawns the real `weaver kernel start --foreground --profile user` under a
//! throwaway HOME / runtime dir; never touches `~/.weftos`.
#![cfg(unix)]
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// `sun_path` size here, NUL included (104 macOS/BSD, 108 Linux).
const LIMIT: usize = if cfg!(any(target_os = "linux", target_os = "android")) { 108 } else { 104 };
/// Longest suffix under the runtime dir the user daemon binds, with its `/`.
const LONGEST_SUFFIX: usize = "/child-ipc/child.sock".len();
/// Longest runtime dir whose every socket fits (NUL excluded from the path).
const MAX_ROOT: usize = LIMIT - 1 - LONGEST_SUFFIX;

/// Generous ceiling: a fresh debug weaver can sit in macOS code-signature
/// scans for tens of seconds before it runs.
const WAIT: Duration = Duration::from_secs(300);

struct Kill(Child);
impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn dir() -> tempfile::TempDir {
    let root = PathBuf::from(std::env::var_os("HOME").expect("HOME")).join(".cache/wrs-e2e");
    std::fs::create_dir_all(&root).unwrap();
    tempfile::Builder::new().prefix("j").tempdir_in(root).unwrap()
}

/// `base`/`pppp..` with the whole path exactly `bytes` long.
fn runtime_dir_of(base: &Path, bytes: usize) -> PathBuf {
    let used = base.as_os_str().len() + 1;
    assert!(bytes > used, "the temp dir {} is too long for this test", base.display());
    let p = base.join("p".repeat(bytes - used));
    assert_eq!(p.as_os_str().len(), bytes);
    p
}

fn start(root: &Path, rt: &Path, log: &Path) -> Child {
    let home = root.join("h");
    std::fs::create_dir_all(&home).unwrap();
    let out = std::fs::File::create(log).unwrap();
    Command::new(env!("CARGO_BIN_EXE_weaver"))
        .args(["kernel", "start", "--foreground", "--profile", "user"])
        .env("HOME", &home)
        .env("WEFTOS_RUNTIME_DIR", rt)
        .env_remove("WEAVER_PROFILE")
        .env_remove("WEFTOS_REEXEC")
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(out.try_clone().unwrap())
        .stderr(out)
        .spawn()
        .unwrap()
}

#[test]
fn an_over_long_runtime_dir_is_refused_before_anything_is_created_or_announced() {
    let root = dir();
    // One byte too long for the user daemon's `child-ipc/child.sock`; its
    // `kernel.sock` still fits, which is why the old daemon got as far as
    // announcing it was listening.
    let rt = runtime_dir_of(root.path(), MAX_ROOT + 1);
    let log = root.path().join("out.log");
    let mut child = start(root.path(), &rt, &log);
    let t = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        if t.elapsed() > WAIT {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the daemon did not exit: {}", std::fs::read_to_string(&log).unwrap_or_default());
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let text = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(!status.success(), "must exit non-zero:\n{text}");
    assert!(!text.contains("Daemon listening"), "announced listening before failing:\n{text}");
    let child_sock = rt.join("child-ipc/child.sock");
    assert!(text.contains(&child_sock.display().to_string()), "must name the path:\n{text}");
    assert!(text.contains(&format!("{} bytes", MAX_ROOT + 1 + LONGEST_SUFFIX)), "path length:\n{text}");
    assert!(text.contains(&format!("{} bytes", LIMIT - 1)), "limit:\n{text}");
    assert!(
        text.contains(&format!("at most {MAX_ROOT} bytes")) && text.contains("WEFTOS_RUNTIME_DIR"),
        "must say what to do:\n{text}"
    );
    assert!(!text.contains("SUN_LEN"), "the anonymous OS error must not be what the operator sees:\n{text}");
    assert!(!rt.exists(), "nothing may be created under the runtime dir: {}", rt.display());
}

#[test]
fn a_runtime_dir_just_short_enough_starts_as_before() {
    let root = dir();
    let rt = runtime_dir_of(root.path(), MAX_ROOT);
    let log = root.path().join("out.log");
    let child = Kill(start(root.path(), &rt, &log));
    let t = Instant::now();
    let sock = rt.join("kernel.sock");
    let child_sock = rt.join("child-ipc/child.sock");
    while t.elapsed() < WAIT {
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        if text.contains("Daemon listening on") && UnixStream::connect(&sock).is_ok() && child_sock.exists() {
            let text = std::fs::read_to_string(&log).unwrap_or_default();
            assert!(!text.contains("too long"), "{text}");
            drop(child);
            return;
        }
        assert!(!text.contains("use a shorter runtime directory"), "refused a dir that fits:\n{text}");
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("never came up:\n{}", std::fs::read_to_string(&log).unwrap_or_default());
}
