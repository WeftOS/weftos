//! What a path on this host actually is, and the checked way to hand it to a cog's uid.
//!
//! A device claim names a path, but the path is only a name: `/dev/v4l/by-id/...` is a symlink to
//! `/dev/video0`, and a socket path can be replaced by a symlink to anything. So the ledger
//! works with [`NodeId`], the kind of node and its identity (major:minor for a device,
//! device and inode for a socket), refuses the kinds and devices no cog may own, and the
//! applier verifies the node is still the one that was planned and never follows a symlink.

use std::os::unix::fs::{FileTypeExt, MetadataExt};

use super::device_owner::{DeviceApplier, GrantOp};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NodeKind {
    Char,
    Block,
    Socket,
    Dir,
    File,
    Symlink,
    Other,
}

/// A node's kind and identity: `(major, minor)` for devices, `(st_dev, st_ino)` for the rest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId {
    pub kind: NodeKind,
    pub a: u64,
    pub b: u64,
}

impl NodeId {
    pub fn char_dev(major: u32, minor: u32) -> Self {
        Self { kind: NodeKind::Char, a: major as u64, b: minor as u64 }
    }

    pub fn is_device(&self) -> bool {
        matches!(self.kind, NodeKind::Char | NodeKind::Block)
    }
}

/// Identity of `path` itself (not of what a symlink points at).
pub fn node_id(path: &str) -> Option<NodeId> {
    let m = std::fs::symlink_metadata(path).ok()?;
    Some(id_of(&m))
}

fn id_of(m: &std::fs::Metadata) -> NodeId {
    let t = m.file_type();
    let dev = |kind| NodeId { kind, a: libc::major(m.rdev() as _) as u64, b: libc::minor(m.rdev() as _) as u64 };
    if t.is_char_device() {
        dev(NodeKind::Char)
    } else if t.is_block_device() {
        dev(NodeKind::Block)
    } else {
        let kind = if t.is_socket() {
            NodeKind::Socket
        } else if t.is_dir() {
            NodeKind::Dir
        } else if t.is_file() {
            NodeKind::File
        } else if t.is_symlink() {
            NodeKind::Symlink
        } else {
            NodeKind::Other
        };
        NodeId { kind, a: m.dev(), b: m.ino() }
    }
}

/// Why no cog may own this node, if so: any block device, and the character devices that are
/// memory, ports, consoles, terminals, loop control or the watchdog.
pub fn sensitive(id: &NodeId) -> Option<&'static str> {
    match (id.kind, id.a, id.b) {
        (NodeKind::Block, _, _) => Some("a block device"),
        (NodeKind::Char, 1, 1 | 2 | 4) => Some("raw memory or I/O ports"),
        (NodeKind::Char, 4, m) if m < 64 => Some("a virtual console"),
        (NodeKind::Char, 5, 0..=2) => Some("the controlling terminal, console or ptmx"),
        (NodeKind::Char, 7, _) => Some("a virtual console screen"),
        (NodeKind::Char, 10, 130 | 237) => Some("the watchdog or loop control"),
        (NodeKind::Char, 136..=143, _) => Some("a pseudo-terminal"),
        _ => None,
    }
}

/// Check the selector's kind: a path selector names a character device, a socket selector a
/// socket. `Err` says what it found.
pub fn kind_matches(socket: bool, id: &NodeId) -> Result<(), String> {
    let want = if socket { NodeKind::Socket } else { NodeKind::Char };
    if id.kind == want {
        Ok(())
    } else {
        Err(format!("it is {:?}, not {want:?}", id.kind).to_lowercase())
    }
}

/// `chown` and `chmod` that cannot be steered: the node is opened `O_PATH|O_NOFOLLOW`, compared
/// with what was planned, and changed through that descriptor, so a symlink swapped in between
/// is refused rather than followed. Needs root or `CAP_CHOWN`.
pub struct SystemApplier;

impl DeviceApplier for SystemApplier {
    fn apply(&self, ops: &[GrantOp]) -> Result<(), String> {
        ops.iter().try_for_each(apply_one)
    }
}

/// Verify `path` is `expect` (when known) and not a symlink.
pub fn verify(path: &str, expect: Option<NodeId>) -> Result<(), String> {
    let m = std::fs::symlink_metadata(path).map_err(|e| format!("{path}: {e}"))?;
    if m.file_type().is_symlink() {
        return Err(format!("{path} is a symlink; it is not followed"));
    }
    match expect {
        Some(e) if id_of(&m) != e => Err(format!("{path} is not the node that was planned (it changed)")),
        _ => Ok(()),
    }
}

fn apply_one(op: &GrantOp) -> Result<(), String> {
    let GrantOp::Chown { path, uid, gid, mode, expect } = op;
    verify(path, *expect)?;
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
        let c = std::ffi::CString::new(path.as_str()).map_err(|e| e.to_string())?;
        // SAFETY: a plain open of a NUL-terminated path; the result is checked and owned at once.
        let fd = unsafe { libc::open(c.as_ptr(), libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(format!("{path}: {}", std::io::Error::last_os_error()));
        }
        // SAFETY: `fd` was just returned to us and nothing else owns it.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        // The descriptor, not the name, is what is verified and changed from here on.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: `st` is a live stat buffer; fstat on an O_PATH descriptor is allowed.
        if unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } != 0 {
            return Err(format!("{path}: {}", std::io::Error::last_os_error()));
        }
        let fmt = st.st_mode & libc::S_IFMT;
        if fmt == libc::S_IFLNK {
            return Err(format!("{path} is a symlink; it is not followed"));
        }
        let magic = std::ffi::CString::new(format!("/proc/self/fd/{}", fd.as_raw_fd())).map_err(|e| e.to_string())?;
        // SAFETY: `magic` names our own open descriptor, which outlives both calls.
        let (a, b) = unsafe { (libc::chown(magic.as_ptr(), *uid, *gid), libc::chmod(magic.as_ptr(), *mode as libc::mode_t)) };
        if a != 0 || b != 0 {
            return Err(format!("{path}: {}", std::io::Error::last_os_error()));
        }
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let c = std::ffi::CString::new(path.as_str()).map_err(|e| e.to_string())?;
        // SAFETY: `c` is a NUL-terminated path that outlives both calls; lchown does not follow.
        let (a, b) = unsafe { (libc::lchown(c.as_ptr(), *uid, *gid), libc::chmod(c.as_ptr(), *mode as libc::mode_t)) };
        if a != 0 || b != 0 {
            return Err(format!("{path}: {}", std::io::Error::last_os_error()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    fn dir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("devnode-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn kinds_and_sensitive_nodes() {
        assert!(sensitive(&NodeId { kind: NodeKind::Block, a: 8, b: 0 }).is_some());
        for (a, b) in [(1, 1), (1, 2), (1, 4), (4, 1), (4, 63), (5, 0), (5, 1), (5, 2), (7, 0), (10, 130), (10, 237), (136, 0), (143, 9)] {
            assert!(sensitive(&NodeId::char_dev(a, b)).is_some(), "{a}:{b}");
        }
        for (a, b) in [(81, 0), (116, 3), (4, 64), (188, 0), (1, 3), (5, 3)] {
            assert!(sensitive(&NodeId::char_dev(a, b)).is_none(), "{a}:{b} (video, sound, ttyS0, ttyUSB0, null, ...)");
        }
        assert!(kind_matches(false, &NodeId::char_dev(81, 0)).is_ok());
        assert!(kind_matches(true, &NodeId::char_dev(81, 0)).unwrap_err().contains("char"));
        assert!(kind_matches(false, &NodeId { kind: NodeKind::Socket, a: 1, b: 2 }).is_err());
    }

    #[test]
    fn a_socket_is_a_socket_and_a_symlink_to_it_is_a_symlink() {
        let d = dir("kinds");
        let sock = d.join("s.sock");
        let _l = UnixListener::bind(&sock).unwrap();
        let link = d.join("l.sock");
        std::os::unix::fs::symlink(&sock, &link).unwrap();
        assert_eq!(node_id(sock.to_str().unwrap()).unwrap().kind, NodeKind::Socket);
        assert_eq!(node_id(link.to_str().unwrap()).unwrap().kind, NodeKind::Symlink, "the link itself, not its target");
        assert!(node_id("/nonexistent/x").is_none());
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn the_applier_refuses_a_symlink_a_changed_node_and_a_missing_one() {
        let d = dir("apply");
        let sock = d.join("s.sock");
        let _l = UnixListener::bind(&sock).unwrap();
        let victim = d.join("victim");
        std::fs::write(&victim, "x").unwrap();
        let link = d.join("evil.sock");
        std::os::unix::fs::symlink(&victim, &link).unwrap();
        let me = unsafe { (libc::geteuid(), libc::getegid()) };
        let op = |p: &std::path::Path, expect| GrantOp::Chown { path: p.to_str().unwrap().into(), uid: me.0, gid: me.1, mode: 0o600, expect };
        let planned = node_id(sock.to_str().unwrap());
        // The planned socket is accepted (chown to ourselves needs no privilege).
        SystemApplier.apply(&[op(&sock, planned)]).unwrap();
        // A symlink in its place is refused, and what it pointed at is untouched.
        let before = std::fs::metadata(&victim).unwrap().mode();
        let e = SystemApplier.apply(&[op(&link, planned)]).unwrap_err();
        assert!(e.contains("symlink"), "{e}");
        assert_eq!(std::fs::metadata(&victim).unwrap().mode(), before);
        // A different node under the planned name is refused: it is not what was planned.
        let e = SystemApplier.apply(&[op(&victim, planned)]).unwrap_err();
        assert!(e.contains("changed"), "{e}");
        assert!(SystemApplier.apply(&[op(&d.join("gone"), None)]).is_err());
        std::fs::remove_dir_all(&d).ok();
    }
}
