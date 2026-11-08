//! ADR-103 Phase 0 R7: the legacy-chain adoption guard exercised by REAL
//! boots of the `weaver` binary.
//!
//! The guard only runs when no `WEFTOS_RUNTIME_DIR` is set (a set variable is
//! `RootSource::Env` and skips it), so this test boots with a cleared
//! environment, a fake `HOME` and the working directory inside it: the
//! resolved root is `<fake home>/.clawft`, the legacy root. Nothing under
//! the real home or runtime root is read or written.
//!
//! Sequence: a clean first boot creates a genuine chain; deleting its
//! `chain.lock` and ageing it turns it into what a lock-unaware older daemon
//! left behind; a plain boot must refuse (exit 78, nothing started), and
//! `--adopt-legacy-chain` must adopt it (a `chain.lock` appears and the
//! kernel serves).
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

const WEAVER: &str = env!("CARGO_BIN_EXE_weaver");
const EX_CONFIG: i32 = 78;

/// Ceiling for a spawned weaver to start and serve. The debug weaver is ~170 MB
/// and on macOS its exec can sit in dyld/code-signature checks for tens of
/// seconds when many test processes start at once (nextest runs a whole crate
/// in parallel); the daemon itself boots in well under a second. A hang still
/// fails, just later.
const BOOT_WAIT: Duration = Duration::from_secs(300);

/// A spawned weaver that is killed and reaped if the test panics.
struct Kid(Option<Child>);

impl Kid {
    fn get(&mut self) -> &mut Child {
        self.0.as_mut().expect("child already taken")
    }

    fn take(mut self) -> Child {
        self.0.take().expect("child already taken")
    }
}

impl Drop for Kid {
    fn drop(&mut self) {
        if let Some(c) = self.0.as_mut() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

struct Home {
    _t: tempfile::TempDir,
    home: PathBuf,
}

impl Home {
    fn new() -> Self {
        // Short: `<home>/.clawft/kernel.sock` must fit macOS's 104-byte sun_path.
        let t = tempfile::Builder::new().prefix("wlab").tempdir_in("/tmp").unwrap();
        let home = t.path().join("h");
        std::fs::create_dir_all(&home).unwrap();
        Self { home: home.canonicalize().unwrap(), _t: t }
    }

    fn legacy(&self) -> PathBuf {
        self.home.join(".clawft")
    }

    /// `weaver kernel start --foreground [extra]` as a cleared-env child whose
    /// stderr goes to a file (read back with [`Self::log`]).
    fn start(&self, extra: &[&str]) -> Kid {
        let log = std::fs::File::create(self.home.join("boot.log")).unwrap();
        Kid(Some(Command::new(WEAVER)
            .args(["kernel", "start", "--foreground"])
            .args(extra)
            .current_dir(&self.home)
            .env_clear()
            .env("HOME", &self.home)
            .env("PATH", "/usr/bin:/bin")
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .expect("start weaver")))
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.home.join("boot.log")).unwrap_or_default()
    }

    fn wait_serving(&self, kid: &mut Kid) {
        let child = kid.get();
        let sock = self.legacy().join("kernel.sock");
        let end = Instant::now() + BOOT_WAIT;
        while !sock.exists() {
            assert!(child.try_wait().unwrap().is_none(), "exited before serving:\n{}", self.log());
            assert!(Instant::now() < end, "never served:\n{}", self.log());
            std::thread::sleep(Duration::from_millis(100));
        }
        // Past the first checkpoint interval is not needed: shutdown saves.
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// SIGTERM our own child (a graceful shutdown that checkpoints the chain) and
/// wait for it; escalate only if it will not exit.
fn stop(kid: Kid, log: &Path) {
    let mut child = kid.take();
    kill(Pid::from_raw(child.id() as i32), Signal::SIGTERM).unwrap();
    let end = Instant::now() + Duration::from_secs(90);
    loop {
        if let Some(st) = child.try_wait().unwrap() {
            assert!(st.success(), "unclean shutdown: {st}\n{}", std::fs::read_to_string(log).unwrap_or_default());
            return;
        }
        if Instant::now() >= end {
            let _ = child.kill();
            let _ = child.wait();
            panic!("kernel did not shut down");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn age(path: &Path) {
    let old = SystemTime::now() - Duration::from_secs(3600);
    std::fs::File::options().write(true).open(path).unwrap().set_modified(old).unwrap();
}

#[test]
fn a_lock_unaware_legacy_chain_is_refused_until_adopted_explicitly() {
    let h = Home::new();

    // 1. A genuine chain, written by a clean boot and shutdown.
    let mut first = h.start(&[]);
    h.wait_serving(&mut first);
    stop(first, &h.home.join("boot.log"));
    assert!(!h.legacy().join("kernel.sock").exists(), "the first kernel is gone: its socket cannot satisfy step 4");
    let rvf = h.legacy().join("chain.rvf");
    assert!(rvf.exists(), "the first boot left a chain: {:?}", std::fs::read_dir(h.legacy()).unwrap().flatten().map(|e| e.file_name()).collect::<Vec<_>>());

    // 2. Make it what an older, lock-unaware daemon left behind.
    std::fs::remove_file(h.legacy().join("chain.lock")).unwrap();
    for f in ["chain.rvf", "chain.json", "chain.tree.json"] {
        let p = h.legacy().join(f);
        if p.exists() {
            age(&p);
        }
    }

    // 3. A plain boot refuses with the permanent exit code and starts nothing.
    let mut refused = h.start(&[]);
    let end = Instant::now() + BOOT_WAIT;
    let status = loop {
        if let Some(st) = refused.get().try_wait().unwrap() {
            break st;
        }
        assert!(Instant::now() < end, "the refused boot kept running:\n{}", h.log());
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_eq!(status.code(), Some(EX_CONFIG), "{}", h.log());
    let log = h.log();
    assert!(log.contains("never been used by a lock-aware kernel"), "{log}");
    assert!(log.contains("--adopt-legacy-chain"), "{log}");
    assert!(!h.legacy().join("chain.lock").exists(), "a refused boot must not take the chain");

    // 4. The explicit flag adopts it: the kernel serves and the chain is locked.
    let mut adopted = h.start(&["--adopt-legacy-chain"]);
    h.wait_serving(&mut adopted);
    assert!(h.legacy().join("chain.lock").exists(), "adoption takes chain.lock");
    assert!(!h.log().contains("never been used by a lock-aware kernel"), "{}", h.log());
    stop(adopted, &h.home.join("boot.log"));
}
