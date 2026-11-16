//! Classifying a unix-socket peer of the user daemon (ADR-103 A14, review S9).
//!
//! ADR-070's local-owner shortcut honours a literal scope (`"admin"`) from any
//! peer with the daemon's uid. A supervised project kernel runs under that
//! uid, so a compromised child (or a tool its agents run) could send
//! `auth: "admin"`, mint an owner token and reach `project.revoke`,
//! `governance.parent.push` and the user chain.
//!
//! Children run in their own process group (`process_group(0)`, so the
//! group id is the child's pid) and so does everything they spawn, unless it
//! changes group itself. A same-uid peer whose process group is a supervised
//! child's is a [`PeerClass::Child`]: its literal scopes are ignored (it is
//! treated as anonymous, never refused outright, because a child legitimately
//! calls `kernel.handshake`, `mesh.*` and `project.anchor.submit`, which the
//! client library stamps with `"admin"` and which need only `read`), and only
//! its project token is honoured. The owner's own CLI is not in any child's
//! group and is unaffected.
//!
//! A same-uid peer whose pid or group cannot be determined is also a
//! [`PeerClass::Child`] (fail closed). The Windows named-pipe path and the
//! legacy `handle_connection` wrapper do not classify and default to
//! [`PeerClass::Owner`].
//!
//! The process-group check alone is a narrowing: `setsid` and `setpgid` can
//! escape it. A sandboxed project child instead uses the separate child-only
//! parent socket, whose method ceiling does not depend on process group.

/// What kind of caller a unix-socket peer is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerClass {
    /// The daemon's own uid, outside every child's process group.
    Owner,
    /// A different uid, or credentials that could not be read (fail closed).
    OtherUid,
    /// Same uid, inside a supervised child's process group, or not provably
    /// outside one (unknown pid, unreadable group).
    Child,
}

impl PeerClass {
    /// May this peer use literal scope strings as credentials?
    pub fn is_owner(self) -> bool {
        self == Self::Owner
    }
}

/// Pure classification. `children` are the pids of the supervised children
/// (each a process-group leader); `pgid_of` looks up a peer's group.
pub fn classify(
    peer_uid: u32,
    peer_pid: Option<i32>,
    own_uid: u32,
    children: &[u32],
    pgid_of: impl Fn(i32) -> Option<i32>,
) -> PeerClass {
    if peer_uid != own_uid {
        return PeerClass::OtherUid;
    }
    // Fail closed: a same-uid peer whose pid is unknown, or whose process
    // group cannot be read (it exited and was reaped between connect and
    // accept: `ESRCH`), cannot be shown to be outside a child's group, so it
    // is treated like a child (literal scopes ignored; its token still works).
    // The owner's CLI is alive and waiting for its answer at accept.
    let Some(g) = peer_pid.and_then(pgid_of) else { return PeerClass::Child };
    if children.iter().any(|c| i64::from(*c) == i64::from(g)) { PeerClass::Child } else { PeerClass::Owner }
}

/// Pids of the supervised children (empty off the user daemon).
#[cfg(all(unix, feature = "exochain", feature = "placement"))]
fn supervised_pids() -> Vec<u32> {
    crate::project_supervisor::global().map_or_else(Vec::new, |s| s.launcher().supervised_pids())
}

#[cfg(not(all(unix, feature = "exochain", feature = "placement")))]
fn supervised_pids() -> Vec<u32> {
    Vec::new()
}

/// The aggregate-quota identity of a socket peer: the uid only. A pid is
/// free to mint (a new process, a fork), so keying on it would let a caller
/// escape the ceiling the same way reconnecting does; `_pid` is accepted for
/// call-site symmetry and diagnostics, never part of the bucket key.
pub fn peer_key(uid: u32, _pid: Option<i32>) -> String {
    format!("uid:{uid}")
}

/// The uid inside a [`peer_key`] (`uid:<uid>`), or `None` for any other shape.
pub fn uid_of_peer_key(key: &str) -> Option<u32> {
    key.strip_prefix("uid:")?.parse().ok()
}

/// Classify the peer of an accepted unix stream.
#[cfg(unix)]
pub fn classify_peer(cred: std::io::Result<tokio::net::unix::UCred>) -> PeerClass {
    let Ok(c) = cred else { return PeerClass::OtherUid };
    classify(
        c.uid(),
        c.pid(),
        nix::unistd::geteuid().as_raw(),
        &supervised_pids(),
        |pid| nix::unistd::getpgid(Some(nix::unistd::Pid::from_raw(pid))).ok().map(nix::unistd::Pid::as_raw),
    )
}

#[cfg(test)]
#[path = "child_peer_tests.rs"]
mod tests;
