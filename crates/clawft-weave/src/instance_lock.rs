//! Single-instance guard for a runtime dir (ADR-103 P0b).
//!
//! The daemon holds an exclusive advisory lock on `<root>/kernel.lock` for
//! its whole lifetime, so two kernels can never share one runtime dir, and
//! the socket can be safely reclaimed once the lock is held: nobody else is
//! serving it.
//!
//! - **Unix**: `flock(LOCK_EX | LOCK_NB)`; the kernel drops it on any exit,
//!   including a crash, so a stale lock file never blocks a restart.
//! - **Windows**: the lock file is opened with no sharing, which fails while
//!   another process holds it.
//!
//! The holder's PID is written into the lock file for the error message.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use clawft_types::runtime_paths::RuntimePaths;

/// Why the lock could not be taken.
#[derive(Debug, thiserror::Error)]
pub enum LockError {
    /// Another live process holds the lock.
    #[error("another kernel owns {root} (pid {pid})")]
    Held {
        /// Runtime root the lock guards.
        root: String,
        /// Holder PID, or `?` when it could not be read.
        pid: String,
    },
    /// A restart re-exec could not take back its own lock: the lock file
    /// names this very pid (exec keeps the pid), so some other process still
    /// holds the descriptor it inherited. The kernel is not running.
    #[error(
        "restart could not re-take the lock on {root}: it is still held under this process's own \
         pid {pid} by a process that inherited the descriptor; the kernel is NOT running, start it again"
    )]
    SelfHeld {
        /// Runtime root the lock guards.
        root: String,
        /// This process's pid.
        pid: String,
    },
    /// Filesystem failure creating or opening the lock file.
    #[error("cannot take kernel lock {path}: {source}")]
    Io {
        /// Lock file path.
        path: PathBuf,
        /// Underlying error.
        source: std::io::Error,
    },
}

/// Write `path` (a `kernel.pid`) atomically: a temp file in the same
/// directory, then a rename, so a reader (the supervisor's adoption scan)
/// sees either no file or the whole pid, never an empty one.
pub fn write_pid_file(path: &Path, pid: u32) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = dir.join(format!(".kernel.pid.{}.tmp", std::process::id()));
    std::fs::write(&tmp, pid.to_string())?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// Held for the daemon's lifetime; dropping it releases the lock.
#[derive(Debug)]
pub struct InstanceLock {
    #[cfg(unix)]
    _guard: nix::fcntl::Flock<File>,
    #[cfg(not(unix))]
    _guard: File,
    path: PathBuf,
}

impl InstanceLock {
    /// Take the exclusive lock for `paths`, creating the runtime dir.
    pub fn acquire(paths: &RuntimePaths) -> Result<Self, LockError> {
        let path = paths.lock();
        let io_err = |source| LockError::Io {
            path: path.clone(),
            source,
        };
        std::fs::create_dir_all(paths.root()).map_err(io_err)?;
        let file = open_lock_file(&path);
        #[cfg(unix)]
        {
            use nix::errno::Errno;
            use nix::fcntl::{Flock, FlockArg};
            let file = file.map_err(io_err)?;
            match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
                Ok(mut guard) => {
                    record_pid(&mut guard);
                    Ok(Self {
                        _guard: guard,
                        path,
                    })
                }
                Err((_, Errno::EWOULDBLOCK)) => Err(held(paths, &path)),
                Err((_, errno)) => Err(io_err(std::io::Error::from(errno))),
            }
        }
        #[cfg(not(unix))]
        {
            match file {
                Ok(mut f) => {
                    record_pid(&mut f);
                    Ok(Self { _guard: f, path })
                }
                // ERROR_SHARING_VIOLATION (32) / ERROR_LOCK_VIOLATION (33).
                Err(e) if matches!(e.raw_os_error(), Some(32 | 33)) => Err(held(paths, &path)),
                Err(e) => Err(io_err(e)),
            }
        }
    }

    /// [`acquire`](Self::acquire) for a process that came up through the
    /// SIGHUP re-exec (same pid as its predecessor). The predecessor's lock
    /// is released by the exec, but a child it was spawning at that instant
    /// can briefly hold an inherited descriptor, so a lock that names this
    /// very pid is retried for `wait` before giving up with
    /// [`LockError::SelfHeld`]. A lock held by another pid fails at once.
    pub fn acquire_after_reexec(
        paths: &RuntimePaths,
        wait: std::time::Duration,
    ) -> Result<Self, LockError> {
        let start = std::time::Instant::now();
        loop {
            match Self::acquire(paths) {
                Err(LockError::Held { root, pid }) if pid == std::process::id().to_string() => {
                    if start.elapsed() >= wait {
                        return Err(LockError::SelfHeld { root, pid });
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                other => return other,
            }
        }
    }

    /// Lock file path.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(unix)]
fn open_lock_file(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
}

#[cfg(windows)]
fn open_lock_file(path: &Path) -> std::io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0)
        .open(path)
}

#[cfg(not(any(unix, windows)))]
fn open_lock_file(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
}

fn record_pid(file: &mut File) {
    use std::io::Seek;
    // Best effort: the lock, not the PID text, is the guarantee.
    let _ = file.set_len(0);
    let _ = file.rewind();
    let _ = write!(file, "{}", std::process::id());
    let _ = file.flush();
}

fn held(paths: &RuntimePaths, lock: &Path) -> LockError {
    // The lock file is truncated only by its holder, so it carries the
    // holder's PID; the pid file is the fallback (and the only source on
    // Windows, where the locked file cannot be read).
    let read_pid = |p: &Path| {
        let mut s = String::new();
        File::open(p).ok()?.read_to_string(&mut s).ok()?;
        s.trim().parse::<u32>().ok()
    };
    let pid = read_pid(lock)
        .or_else(|| read_pid(&paths.pid()))
        .map_or_else(|| "?".to_string(), |p| p.to_string());
    LockError::Held {
        root: paths.root().display().to_string(),
        pid,
    }
}

/// The pid of the process holding `<root>/kernel.lock`, read without a pid
/// file (`weaver kernel stop` when `kernel.pid` is gone but a daemon is not).
///
/// Only a held lock names its holder: the file is rewritten by each holder
/// on acquisition, so a leftover one names a dead (or reused) pid. Whether it
/// is held is probed with a non-blocking shared lock, released at once; a
/// kernel taking the lock in that same instant would see it held and refuse
/// to boot (exit 78), which only a concurrent start and stop can cause.
/// `Err` says why there is no holder.
#[cfg(unix)]
pub fn holder(paths: &RuntimePaths) -> Result<u32, String> {
    use nix::errno::Errno;
    use nix::fcntl::{Flock, FlockArg};
    let path = paths.lock();
    let mut file = match File::open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(format!("no lock file at {}", path.display()));
        }
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    let mut text = String::new();
    file.read_to_string(&mut text)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    match Flock::lock(file, FlockArg::LockSharedNonblock) {
        Ok(_free) => Err(format!("{} is not held by any process", path.display())),
        Err((_, Errno::EWOULDBLOCK)) => text.trim().parse::<u32>().map_err(|_| {
            format!("{} is held but names no pid ({:?})", path.display(), text.trim())
        }),
        Err((_, errno)) => Err(format!("cannot probe {}: {errno}", path.display())),
    }
}

/// With the instance lock held, make the socket path bindable.
///
/// A socket file that refuses connections (or vanished) is stale: nobody
/// serves it, so it is unlinked. Permission errors and other failures are
/// never treated as stale. One that accepts connections belongs to a
/// server that does not hold our lock; it is never taken over.
#[cfg(unix)]
fn refused(msg: String) -> anyhow::Error {
    anyhow::Error::new(crate::boot_refusal::BootRefused(msg))
}

#[cfg(unix)]
pub async fn reclaim_stale_socket(paths: &RuntimePaths) -> anyhow::Result<()> {
    use clawft_rpc::probe::{SocketState, probe_socket};
    let socket = paths.socket();
    if !socket.exists() {
        return Ok(());
    }
    match probe_socket(&socket).await {
        SocketState::Reachable => Err(anyhow::Error::new(crate::boot_refusal::AlreadyRunning(
            format!(
                "daemon already running (socket accepts connections: {}); \
                 it does not hold {} - refusing to take over its socket",
                socket.display(),
                paths.lock().display()
            ),
        ))),
        // ECONNREFUSED / ENOENT: nobody serves it, so it is stale.
        SocketState::Stale | SocketState::NoSocketFile => {
            std::fs::remove_file(&socket)?;
            tracing::warn!(socket = %socket.display(), "removed stale socket file");
            Ok(())
        }
        // Not ours to judge: another user's socket, or an unexpected error.
        SocketState::PermissionDenied => Err(refused(format!(
            "cannot probe {}: permission denied; it belongs to another user, leaving it alone",
            socket.display()
        ))),
        SocketState::Other(e) => anyhow::bail!(
            "cannot tell whether {} is stale ({e}); leaving it alone",
            socket.display()
        ),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn second_acquire_fails_with_holder_pid_then_succeeds_after_drop() {
        let dir = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::at(dir.path());
        let first = InstanceLock::acquire(&paths).expect("first lock");
        assert_eq!(first.path(), dir.path().join("kernel.lock"));

        let err = InstanceLock::acquire(&paths).expect_err("second must fail");
        let msg = err.to_string();
        assert!(msg.contains("another kernel owns"), "{msg}");
        assert!(msg.contains(&dir.path().display().to_string()), "{msg}");
        assert!(
            msg.contains(&format!("pid {}", std::process::id())),
            "{msg}"
        );

        let err = anyhow::Error::new(err);
        assert_eq!(crate::boot_refusal::exit_code(&err), 78, "a held lock is a refusal");

        drop(first);
        InstanceLock::acquire(&paths).expect("lock free after drop");
    }

    #[test]
    fn reexec_lock_naming_our_own_pid_is_retried_then_refused_clearly() {
        let dir = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::at(dir.path());
        // The lock file names this pid, as after an exec that keeps it.
        let holder = InstanceLock::acquire(&paths).unwrap();
        let t = std::time::Instant::now();
        let err = InstanceLock::acquire_after_reexec(&paths, std::time::Duration::from_millis(300))
            .expect_err("still held");
        assert!(t.elapsed() >= std::time::Duration::from_millis(300), "retried first");
        assert!(matches!(err, LockError::SelfHeld { .. }), "{err}");
        assert!(err.to_string().contains("NOT running"), "{err}");
        assert_eq!(crate::boot_refusal::exit_code(&anyhow::Error::new(err)), 78);
        // Released while retrying: the re-exec comes up.
        let p2 = paths.clone();
        let t = std::thread::spawn(move || {
            InstanceLock::acquire_after_reexec(&p2, std::time::Duration::from_secs(5)).map(|_| ())
        });
        std::thread::sleep(std::time::Duration::from_millis(250));
        drop(holder);
        t.join().unwrap().expect("acquired once the descriptor closed");
    }

    #[tokio::test]
    async fn stale_socket_is_unlinked_and_live_socket_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::at(dir.path());
        // No socket file: nothing to do.
        reclaim_stale_socket(&paths).await.unwrap();

        // Dead listener left its file behind: reclaimed.
        let dead = std::os::unix::net::UnixListener::bind(paths.socket()).unwrap();
        drop(dead);
        assert!(paths.socket().exists());
        reclaim_stale_socket(&paths).await.unwrap();
        assert!(!paths.socket().exists());

        // Live listener: refused, and its socket file is left alone.
        let _live = tokio::net::UnixListener::bind(paths.socket()).unwrap();
        let err = reclaim_stale_socket(&paths).await.unwrap_err();
        assert!(err.to_string().contains("already running"), "{err}");
        assert_eq!(crate::boot_refusal::exit_code(&err), 78, "a live daemon is a refusal");
        assert!(paths.socket().exists());
    }

    #[test]
    fn holder_names_the_live_lock_owner_and_refuses_otherwise() {
        let dir = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::at(dir.path());
        let none = holder(&paths).unwrap_err();
        assert!(none.contains("no lock file"), "{none}");

        let lock = InstanceLock::acquire(&paths).unwrap();
        assert_eq!(holder(&paths), Ok(std::process::id()));
        // Probing does not take the lock away from its owner.
        assert!(InstanceLock::acquire(&paths).is_err());
        drop(lock);

        // A leftover file names a pid, but nobody holds it.
        let free = holder(&paths).unwrap_err();
        assert!(free.contains("not held"), "{free}");
        InstanceLock::acquire(&paths).expect("the probe left the lock free");
    }

    #[test]
    fn stale_lock_file_without_holder_does_not_block() {
        let dir = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::at(dir.path());
        std::fs::write(paths.lock(), "99999999").unwrap();
        InstanceLock::acquire(&paths).expect("a leftover file is not a lock");
    }
}
