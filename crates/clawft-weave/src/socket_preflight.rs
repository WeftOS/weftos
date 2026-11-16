//! Up-front check of every unix socket path the daemon will use (WO-069 J1).
//!
//! A runtime dir long enough that a socket under it overflows `sun_path` used
//! to fail late and anonymously: the daemon bound `kernel.sock`, printed
//! "Daemon listening", then died on `child-ipc/child.sock` with `path must be
//! shorter than SUN_LEN`. [`preflight`] runs before the daemon creates, locks
//! or binds anything, names the offending path, its length and the limit, and
//! says what to set. The limits and the byte counting live in
//! [`clawft_types::socket_path`].

use std::path::{Path, PathBuf};

use clawft_types::runtime_paths::RuntimePaths;
use clawft_types::socket_path::{self, PathTooLong};

/// Every socket the daemon binds under `paths`: the kernel socket, the cogbus
/// socket, and (user profile) the child-only endpoint.
pub fn daemon_socket_paths(paths: &RuntimePaths, user_profile: bool) -> Vec<PathBuf> {
    let mut v = vec![paths.socket(), paths.root().join(weftos_cogbus::SOCKET_NAME)];
    if user_profile {
        v.push(crate::user_daemon::child_socket_path(paths.root()));
    }
    v
}

/// The socket paths relative to the runtime dir (`kernel.sock`,
/// `child-ipc/child.sock`), derived from [`daemon_socket_paths`].
fn suffixes(paths: &RuntimePaths, user_profile: bool) -> Vec<String> {
    daemon_socket_paths(paths, user_profile)
        .iter()
        .filter_map(|p| p.strip_prefix(paths.root()).ok())
        .map(|s| s.to_string_lossy().into_owned())
        .collect()
}

/// Longest runtime dir (bytes) for which every daemon socket fits `limit`.
pub fn max_runtime_dir_bytes(paths: &RuntimePaths, user_profile: bool, limit: usize) -> usize {
    let s = suffixes(paths, user_profile);
    socket_path::max_root_bytes(s.iter().map(String::as_str), limit)
}

/// The operator-facing message for `e`, naming the fix.
fn message(e: &PathTooLong, paths: &RuntimePaths, user_profile: bool, limit: usize) -> String {
    let max = max_runtime_dir_bytes(paths, user_profile, limit);
    format!(
        "{e}\n  nothing was started: a unix socket path may be at most {} bytes on this platform.\n  \
         use a shorter runtime directory: set WEFTOS_RUNTIME_DIR to a path of at most {max} bytes \
         (the current one, {}, is {} bytes)",
        socket_path::max_path_bytes(limit),
        paths.root().display(),
        socket_path::path_bytes(paths.root()),
    )
}

/// [`preflight`] against an explicit `sun_path` size (both platform limits are
/// testable on any host).
pub fn preflight_with(paths: &RuntimePaths, user_profile: bool, limit: usize) -> anyhow::Result<()> {
    let all = daemon_socket_paths(paths, user_profile);
    let refs: Vec<&Path> = all.iter().map(PathBuf::as_path).collect();
    socket_path::check_all_with(refs, limit)
        .map_err(|e| anyhow::Error::msg(message(&e, paths, user_profile, limit)))
}

/// Check every socket path this daemon will use against the platform limit,
/// before anything is created, locked or bound. One error, naming the path.
pub fn preflight(paths: &RuntimePaths) -> anyhow::Result<()> {
    preflight_with(paths, crate::user_daemon::is_active(), socket_path::PLATFORM_LIMIT)
}

#[cfg(test)]
#[path = "socket_preflight_tests.rs"]
mod tests;
