//! Durable, atomic file replacement using only the standard library.
//!
//! [`write_atomic`] replaces a file so that a reader (or a crash) sees either
//! the old contents or the new contents, never a mix. The steps are: create a
//! uniquely named sibling temp file, write the bytes, `sync_all` the file,
//! rename it over the target, then fsync the parent directory.
//!
//! # Durability by target
//!
//! | Target | File fsync | Directory fsync after rename |
//! |---|---|---|
//! | unix (Linux, macOS, BSD) | yes | yes |
//! | Windows | yes | no: a directory handle cannot be opened with `File::open` |
//! | `wasm32-wasip1` | yes | no: WASI p1 rejects `fd_sync` on a directory (EBADF) |
//!
//! Where the directory fsync is skipped, the rename is still atomic but its
//! directory entry may not survive a power loss.
//!
//! On macOS, std's `File::sync_all` issues `fcntl(F_FULLFSYNC)`, so the file
//! fsync reaches stable storage rather than only the drive cache.
//!
//! The temp name uses a process-wide counter and the wall-clock nanoseconds.
//! It deliberately avoids the process id, which is unsupported on
//! `wasm32-wasip1`.
//!
//! # Targets
//!
//! Not for `wasm32-unknown-unknown`: the temp name reads `SystemTime::now()`,
//! which panics there, and that target has no filesystem anyway.

use std::error::Error;
use std::ffi::OsString;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// The step of a durable write that failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Operation {
    /// The path has no file name component.
    ResolvePath,
    /// Creating the temp file.
    CreateTemp,
    /// Writing the bytes into the temp file.
    Write,
    /// Fsyncing the temp file.
    SyncFile,
    /// Renaming the temp file over the target.
    Rename,
    /// Opening or fsyncing the parent directory.
    SyncDir,
}

impl fmt::Display for Operation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::ResolvePath => "resolve path",
            Self::CreateTemp => "create temp file",
            Self::Write => "write",
            Self::SyncFile => "fsync file",
            Self::Rename => "rename",
            Self::SyncDir => "fsync directory",
        })
    }
}

/// A durable write failed. Carries the step and the path it was acting on.
#[derive(Debug)]
#[non_exhaustive]
pub struct DurableWriteError {
    operation: Operation,
    path: PathBuf,
    source: io::Error,
}

impl DurableWriteError {
    fn new(operation: Operation, path: &Path, source: io::Error) -> Self {
        Self { operation, path: path.to_path_buf(), source }
    }

    /// The step that failed.
    #[must_use]
    pub fn operation(&self) -> Operation {
        self.operation
    }

    /// The path the failing step acted on (the temp file, the target or the directory).
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl fmt::Display for DurableWriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}: {}", self.operation, self.path.display(), self.source)
    }
}

impl Error for DurableWriteError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.source)
    }
}

impl From<DurableWriteError> for io::Error {
    /// Keeps the original [`io::ErrorKind`] so callers that return `io::Result` can use `?`.
    fn from(e: DurableWriteError) -> Self {
        io::Error::new(e.source.kind(), e)
    }
}

/// Upper bound on temp names tried when `create_new` reports `AlreadyExists`.
///
/// Each name mixes a process-wide counter with the clock, so one retry is
/// almost always enough; the bound only stops a pathological loop.
const MAX_TEMP_ATTEMPTS: usize = 8;
const _: () = assert!(MAX_TEMP_ATTEMPTS > 0);

/// Build a unique sibling temp path for `path`.
fn temp_path(path: &Path) -> Result<PathBuf, DurableWriteError> {
    static NEXT_TMP: AtomicU64 = AtomicU64::new(0);
    let name = path.file_name().ok_or_else(|| {
        DurableWriteError::new(
            Operation::ResolvePath,
            path,
            io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"),
        )
    })?;
    let n = NEXT_TMP.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    let mut tmp_name = OsString::from(".");
    tmp_name.push(name);
    tmp_name.push(format!(".{n}.{nanos}.tmp"));
    Ok(path.with_file_name(tmp_name))
}

/// Replace `path` with `bytes`, atomically and durably.
///
/// The parent directory must exist. The temp file is created next to the
/// target with `create_new`, so concurrent writers never share one, and is
/// removed on any error. If the chosen temp name already exists (another
/// writer's file) it is left untouched and a fresh name is tried, a
/// bounded number of times. See the crate docs for the per-target durability.
///
/// # Examples
///
/// ```
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let dir = std::env::temp_dir().join(format!("weftos-fsutil-doc-{}", std::process::id()));
/// std::fs::create_dir_all(&dir)?;
/// let target = dir.join("state.bin");
/// weftos_fsutil::write_atomic(&target, b"hello")?;
/// assert_eq!(std::fs::read(&target)?, b"hello");
/// std::fs::remove_dir_all(&dir)?;
/// # Ok(())
/// # }
/// ```
///
/// # Errors
///
/// Returns a [`DurableWriteError`] naming the failing [`Operation`] and path.
/// If the rename succeeded but the directory fsync failed (unix only), the new
/// contents are in place but not yet guaranteed durable.
///
/// If every temp name tried collides with an existing file, the call gives up
/// after a fixed small number of attempts with [`Operation::CreateTemp`] and an
/// [`io::ErrorKind::AlreadyExists`] cause; the colliding files are left alone.
/// Only `AlreadyExists` is retried: any other create error (including a
/// Windows `PermissionDenied` on a delete-pending name) is returned at once.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), DurableWriteError> {
    write_atomic_with(path, bytes, || temp_path(path), fsync_dir)
}

/// Test seam: `write_atomic` with the temp-name generator injected, so a test
/// can force a name collision. Private; the public API is unchanged.
///
/// A temp file is removed on error only if this call created it: a name that
/// `create_new` reports as `AlreadyExists` belongs to another writer and is
/// left alone, and a fresh name is tried, up to [`MAX_TEMP_ATTEMPTS`] times.
fn write_atomic_with(
    path: &Path,
    bytes: &[u8],
    mut next_tmp: impl FnMut() -> Result<PathBuf, DurableWriteError>,
    sync_dir: impl FnOnce(&Path) -> Result<(), DurableWriteError>,
) -> Result<(), DurableWriteError> {
    let mut attempt = 0;
    loop {
        attempt += 1;
        let tmp = next_tmp()?;
        match OpenOptions::new().write(true).create_new(true).open(&tmp) {
            Ok(file) => {
                // The temp file is ours from here until the rename succeeds.
                if let Err(e) = write_and_rename(file, &tmp, path, bytes) {
                    // Best effort: remove only the file this call created.
                    let _ = std::fs::remove_file(&tmp);
                    return Err(e);
                }
                break;
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists && attempt < MAX_TEMP_ATTEMPTS => {}
            Err(e) => return Err(DurableWriteError::new(Operation::CreateTemp, &tmp, e)),
        }
    }
    // After the rename the temp name is no longer ours: another writer may
    // already have reused it, so a directory-fsync failure must not clean it up.
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    sync_dir(dir)
}

/// Write, fsync and rename the already-created temp file. Does not touch the
/// directory entry's durability; the caller does that after the rename.
fn write_and_rename(
    mut file: File,
    tmp: &Path,
    path: &Path,
    bytes: &[u8],
) -> Result<(), DurableWriteError> {
    file.write_all(bytes)
        .map_err(|e| DurableWriteError::new(Operation::Write, tmp, e))?;
    file.sync_all()
        .map_err(|e| DurableWriteError::new(Operation::SyncFile, tmp, e))?;
    drop(file);
    std::fs::rename(tmp, path).map_err(|e| DurableWriteError::new(Operation::Rename, path, e))
}

/// Fsync a directory so a rename or create inside it survives a power loss.
///
/// On Windows and `wasm32-wasip1` this does nothing and returns `Ok(())`; see
/// the crate docs.
///
/// # Errors
///
/// Returns a [`DurableWriteError`] with [`Operation::SyncDir`] if the directory
/// cannot be opened or fsynced (some filesystems, such as exFAT or SMB shares,
/// refuse a directory fsync).
pub fn fsync_dir(dir: &Path) -> Result<(), DurableWriteError> {
    #[cfg(unix)]
    {
        File::open(dir)
            .and_then(|d| d.sync_all())
            .map_err(|e| DurableWriteError::new(Operation::SyncDir, dir, e))
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

const _: () = {
    const fn assert_traits<T: Error + Send + Sync + 'static>() {}
    assert_traits::<DurableWriteError>();
};

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "weftos-fsutil-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos())
        ));
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    fn entries(dir: &Path) -> Vec<OsString> {
        let mut v: Vec<_> = std::fs::read_dir(dir)
            .expect("read dir")
            .map(|e| e.expect("entry").file_name())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn round_trip_and_replace() {
        let dir = scratch("rt");
        let path = dir.join("f.bin");
        write_atomic(&path, b"first").expect("first write");
        assert_eq!(std::fs::read(&path).expect("read"), b"first");
        write_atomic(&path, b"second, longer").expect("second write");
        assert_eq!(std::fs::read(&path).expect("read"), b"second, longer");
        write_atomic(&path, b"").expect("empty write");
        assert_eq!(std::fs::read(&path).expect("read"), b"");
        assert_eq!(entries(&dir), vec![OsString::from("f.bin")]);
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn relative_path_with_no_parent_works() {
        // Runs in the crate directory; the file name is unique and removed after.
        let name = format!("fsutil-rel-{}.tmp-test", std::process::id());
        write_atomic(Path::new(&name), b"x").expect("write relative");
        assert_eq!(std::fs::read(&name).expect("read"), b"x");
        std::fs::remove_file(&name).expect("cleanup");
    }

    #[test]
    fn failed_rename_leaves_no_temp_file_and_names_the_step() {
        let dir = scratch("fail-rename");
        let path = dir.join("target");
        std::fs::create_dir(&path).expect("dir in the way");
        let err = write_atomic(&path, b"data").expect_err("rename onto a directory fails");
        assert_eq!(err.operation(), Operation::Rename);
        assert_eq!(err.path(), path.as_path());
        assert!(err.source().is_some());
        assert_eq!(entries(&dir), vec![OsString::from("target")]);
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn missing_parent_is_a_create_temp_error() {
        let dir = scratch("fail-parent");
        let path = dir.join("absent").join("f");
        let err = write_atomic(&path, b"data").expect_err("parent is missing");
        assert_eq!(err.operation(), Operation::CreateTemp);
        let io_err: io::Error = err.into();
        assert_eq!(io_err.kind(), io::ErrorKind::NotFound);
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn path_without_file_name_is_rejected() {
        let err = write_atomic(Path::new("/"), b"x").expect_err("no file name");
        assert_eq!(err.operation(), Operation::ResolvePath);
    }

    #[test]
    fn temp_names_are_unique_under_concurrency() {
        let path = Path::new("/some/dir/target.bin");
        let handles: Vec<_> = (0..8)
            .map(|_| {
                std::thread::spawn(move || {
                    (0..500)
                        .map(|_| temp_path(path).expect("temp path"))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        let mut seen = HashSet::new();
        for h in handles {
            for p in h.join().expect("thread") {
                assert_eq!(p.parent(), path.parent(), "temp file stays in the target directory");
                assert!(seen.insert(p), "duplicate temp name");
            }
        }
        assert_eq!(seen.len(), 4000);
    }

    #[test]
    fn concurrent_writers_to_one_target_all_succeed_and_leave_one_file() {
        let dir = scratch("conc");
        let path = Arc::new(dir.join("shared.bin"));
        let handles: Vec<_> = (0..8u8)
            .map(|i| {
                let path = Arc::clone(&path);
                std::thread::spawn(move || {
                    for _ in 0..25 {
                        write_atomic(&path, &vec![i; 4096]).expect("concurrent write");
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().expect("writer thread");
        }
        let body = std::fs::read(&*path).expect("read");
        assert_eq!(body.len(), 4096);
        assert!(body.iter().all(|b| *b == body[0]), "winner is one writer's whole payload");
        assert_eq!(entries(&dir), vec![OsString::from("shared.bin")]);
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    /// A reader polling during repeated replacement only ever sees one whole payload.
    #[test]
    fn target_is_never_partially_written() {
        let dir = scratch("atomic");
        let path = Arc::new(dir.join("t.bin"));
        let a = vec![0xAAu8; 1 << 20];
        let b = vec![0x55u8; (1 << 20) + 13];
        write_atomic(&path, &a).expect("seed");
        let stop = Arc::new(AtomicBool::new(false));
        let reader = {
            let (path, stop, a, b) = (Arc::clone(&path), Arc::clone(&stop), a.clone(), b.clone());
            std::thread::spawn(move || {
                let mut reads = 0u32;
                while !stop.load(Ordering::Relaxed) {
                    let got = std::fs::read(&*path).expect("target always readable");
                    assert!(got == a || got == b, "torn read of {} bytes", got.len());
                    reads += 1;
                }
                reads
            })
        };
        for i in 0..40 {
            write_atomic(&path, if i % 2 == 0 { &b } else { &a }).expect("replace");
        }
        stop.store(true, Ordering::Relaxed);
        assert!(reader.join().expect("reader thread") > 0);
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    /// Run `write_atomic_with` over a generator that yields `names` in order
    /// (then repeats the last), returning the result and how often it was asked.
    fn run_with_names(
        path: &Path,
        names: &[PathBuf],
    ) -> (Result<(), DurableWriteError>, usize) {
        let mut calls = 0usize;
        let result = write_atomic_with(path, b"new", || {
            let i = calls.min(names.len() - 1);
            calls += 1;
            Ok(names[i].clone())
        }, fsync_dir);
        (result, calls)
    }

    /// A temp name already taken by another writer is skipped, never removed.
    #[test]
    fn temp_name_collision_retries_and_spares_the_other_writers_file() {
        let dir = scratch("collide");
        let path = dir.join("t.bin");
        let theirs = dir.join(".t.bin.theirs.tmp");
        std::fs::write(&theirs, b"other writer").expect("plant their temp file");
        let fresh = dir.join(".t.bin.fresh.tmp");
        let (result, calls) = run_with_names(&path, &[theirs.clone(), fresh]);
        result.expect("retry with a fresh name succeeds");
        assert_eq!(calls, 2);
        assert_eq!(std::fs::read(&path).expect("read target"), b"new");
        assert_eq!(std::fs::read(&theirs).expect("their file survives"), b"other writer");
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    /// Persistent collisions stop at the bound with the typed CreateTemp error
    /// carrying AlreadyExists, and still never touch the other writer's file.
    #[test]
    fn exhausted_collisions_refuse_with_already_exists_and_spare_the_file() {
        let dir = scratch("collide-all");
        let path = dir.join("t.bin");
        let theirs = dir.join(".t.bin.theirs.tmp");
        std::fs::write(&theirs, b"other writer").expect("plant their temp file");
        let (result, calls) = run_with_names(&path, std::slice::from_ref(&theirs));
        let err = result.expect_err("every name collides");
        assert_eq!(err.operation(), Operation::CreateTemp);
        assert_eq!(err.path(), theirs.as_path());
        let io_err: io::Error = err.into();
        assert_eq!(io_err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(calls, MAX_TEMP_ATTEMPTS);
        assert!(!path.exists(), "target untouched");
        assert_eq!(std::fs::read(&theirs).expect("their file survives"), b"other writer");
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    /// Once the rename has succeeded the temp name is no longer this call's: a
    /// later directory-fsync failure must not delete a file another writer has
    /// since created under that name.
    #[test]
    fn dir_sync_failure_after_rename_spares_a_file_now_at_the_temp_name() {
        let dir = scratch("syncdir-fail");
        let path = dir.join("t.bin");
        let tmp = dir.join(".t.bin.mine.tmp");
        let tmp_for_sync = tmp.clone();
        let result = write_atomic_with(
            &path,
            b"new",
            || Ok(tmp.clone()),
            |d| {
                // Simulate another writer taking the now-free temp name.
                std::fs::write(&tmp_for_sync, b"other writer").expect("plant");
                Err(DurableWriteError::new(
                    Operation::SyncDir,
                    d,
                    io::Error::other("forced dir fsync failure"),
                ))
            },
        );
        let err = result.expect_err("dir fsync failed");
        assert_eq!(err.operation(), Operation::SyncDir);
        assert_eq!(std::fs::read(&path).expect("new contents in place"), b"new");
        assert_eq!(std::fs::read(&tmp).expect("their file survives"), b"other writer");
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[cfg(unix)]
    #[test]
    fn fsync_dir_reports_a_missing_directory() {
        let err = fsync_dir(Path::new("/definitely/not/a/dir")).expect_err("missing dir");
        assert_eq!(err.operation(), Operation::SyncDir);
        assert!(fsync_dir(&std::env::temp_dir()).is_ok());
    }
}
