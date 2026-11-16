//! Unix socket path length limit (WO-069 lane J1).
//!
//! `sockaddr_un.sun_path` is a fixed array: 104 bytes on macOS and the BSDs,
//! 108 on Linux, and the NUL terminator counts against it. A longer path makes
//! `bind(2)`/`connect(2)` fail with the bare `path must be shorter than
//! SUN_LEN`, which names nothing. This module is the one place that knows the
//! limit, so a daemon can check every socket path it will use before it binds
//! or announces anything, and a client can say what is wrong with the path it
//! was given.
//!
//! The functions are pure (no I/O) and measure exactly the bytes the caller
//! would pass to `bind`/`connect`: a relative path is measured as written, not
//! after resolving it against the working directory.

use std::path::{Path, PathBuf};

/// `sun_path` size on macOS and the BSDs, NUL included.
pub const LIMIT_MACOS: usize = 104;
/// `sun_path` size on Linux, NUL included.
pub const LIMIT_LINUX: usize = 108;

/// `sun_path` size of the platform this crate is built for. Named constants
/// rather than `size_of` of the libc field, which safe code cannot name.
/// Unknown unix targets get the smaller (BSD) limit, so the check errs safe.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub const PLATFORM_LIMIT: usize = LIMIT_LINUX;
/// `sun_path` size of the platform this crate is built for.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub const PLATFORM_LIMIT: usize = LIMIT_MACOS;

/// A socket path that does not fit in `sun_path`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathTooLong {
    /// The offending path, as it would be passed to `bind`/`connect`.
    pub path: PathBuf,
    /// Its length in bytes (not chars, not counting the NUL).
    pub bytes: usize,
    /// The `sun_path` size it was checked against, NUL included.
    pub limit: usize,
}

impl PathTooLong {
    /// Longest path that fits: the limit minus the terminator.
    pub fn max_bytes(&self) -> usize {
        max_path_bytes(self.limit)
    }
}

impl std::fmt::Display for PathTooLong {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "unix socket path is {} bytes, over the platform limit of {} bytes ({} bytes including the \
             terminator): {}",
            self.bytes,
            self.max_bytes(),
            self.limit,
            self.path.display()
        )
    }
}

impl std::error::Error for PathTooLong {}

/// Longest socket path in bytes for a `sun_path` of `limit` (NUL excluded).
pub fn max_path_bytes(limit: usize) -> usize {
    limit.saturating_sub(1)
}

/// `path` length in bytes as the OS sees it (multi-byte UTF-8 counts bytes).
pub fn path_bytes(path: &Path) -> usize {
    path.as_os_str().as_encoded_bytes().len()
}

/// Check one socket path against a `sun_path` size of `limit`.
pub fn check_with(path: &Path, limit: usize) -> Result<(), PathTooLong> {
    let bytes = path_bytes(path);
    if bytes > max_path_bytes(limit) {
        return Err(PathTooLong { path: path.to_path_buf(), bytes, limit });
    }
    Ok(())
}

/// [`check_with`] against this platform's limit.
pub fn check(path: &Path) -> Result<(), PathTooLong> {
    check_with(path, PLATFORM_LIMIT)
}

/// Check every path, reporting the longest offender (the one that names the
/// worst case), or `Ok` when all fit.
pub fn check_all_with<'a>(
    paths: impl IntoIterator<Item = &'a Path>,
    limit: usize,
) -> Result<(), PathTooLong> {
    let mut worst: Option<PathTooLong> = None;
    for p in paths {
        if let Err(e) = check_with(p, limit)
            && worst.as_ref().is_none_or(|w| e.bytes > w.bytes)
        {
            worst = Some(e);
        }
    }
    worst.map_or(Ok(()), Err)
}

/// Longest runtime dir (bytes) for which every `suffix` (relative to the
/// runtime dir, e.g. `kernel.sock`, `child-ipc/child.sock`) still fits: the
/// path limit minus the longest suffix and its joining `/`.
pub fn max_root_bytes<'a>(suffixes: impl IntoIterator<Item = &'a str>, limit: usize) -> usize {
    let longest = suffixes.into_iter().map(str::len).max().unwrap_or(0);
    max_path_bytes(limit).saturating_sub(longest + 1)
}

#[cfg(test)]
#[path = "socket_path_tests.rs"]
mod tests;
