//! `[[devices]]` in `cog.toml` (Ventuno cog plan 4.1, 4.7; ADR-122 section 4b):
//! the devices a device-owner cog opens. Exactly one cog per node may own a
//! given device; the host's device ledger (`workload_ctl::device_owner`)
//! enforces that.
//!
//! ```toml
//! [[devices]]
//! path = "/dev/video*"          # a device node or a glob in its last component
//!
//! [[devices]]
//! usb = "1bcf:0b15"            # a USB vid:pid; resolved to nodes at placement
//!
//! [[devices]]
//! socket = "/run/arduino-router.sock"
//! access = "rw"                # rw (default) or ro
//!
//! [[devices]]
//! netdev = "can*"              # a network interface (SocketCAN), by name or glob
//! ```

use serde::Deserialize;

use super::types::RuntimeError;

/// Most `[[devices]]` tables accepted.
pub const MAX_DEVICES: usize = 8;
const MAX_PATH: usize = 108;

/// True if `s` holds a glob character.
pub fn wild(s: &str) -> bool {
    s.contains(['*', '?'])
}

/// True if glob `pattern` (`*` any run, `?` one char) matches all of `text`.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let (p, t) = (pattern.as_bytes(), text.as_bytes());
    let (mut i, mut j, mut star, mut mark) = (0, 0, None, 0);
    while j < t.len() {
        if i < p.len() && (p[i] == b'?' || p[i] == t[j]) {
            i += 1;
            j += 1;
        } else if i < p.len() && p[i] == b'*' {
            star = Some(i);
            mark = j;
            i += 1;
        } else if let Some(s) = star {
            i = s + 1;
            mark += 1;
            j = mark;
        } else {
            return false;
        }
    }
    p[i..].iter().all(|c| *c == b'*')
}

/// True if some string matches both globs.
pub fn globs_intersect(a: &str, b: &str) -> bool {
    fn go(a: &[u8], b: &[u8], seen: &mut Vec<Vec<Option<bool>>>, i: usize, j: usize) -> bool {
        if let Some(r) = seen[i][j] {
            return r;
        }
        let r = match (a.get(i), b.get(j)) {
            (None, None) => true,
            (Some(b'*'), _) => go(a, b, seen, i + 1, j) || (j < b.len() && go(a, b, seen, i, j + 1)),
            (_, Some(b'*')) => go(a, b, seen, i, j + 1) || (i < a.len() && go(a, b, seen, i + 1, j)),
            (Some(x), Some(y)) => (*x == b'?' || *y == b'?' || x == y) && go(a, b, seen, i + 1, j + 1),
            _ => false,
        };
        seen[i][j] = Some(r);
        r
    }
    let mut seen = vec![vec![None; b.len() + 1]; a.len() + 1];
    go(a.as_bytes(), b.as_bytes(), &mut seen, 0, 0)
}


/// What a `[[devices]]` table selects.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum DeviceSel {
    /// A node under `/dev`; a `*` or `?` is allowed in the last component only.
    Path(String),
    /// A USB device by vendor and product id.
    Usb {
        /// Vendor id.
        vid: u16,
        /// Product id.
        pid: u16,
    },
    /// A Unix socket owned by a system service (the MCU router).
    Socket(String),
    /// A network interface by name or glob (`can0`, `can*`), at most 15 characters. The host
    /// hands the owner the interface names (no leading `/`) in `WEFTOS_DEVICES`.
    Netdev(String),
}

impl DeviceSel {
    /// The form shown in listings and refusals.
    pub fn label(&self) -> String {
        match self {
            DeviceSel::Path(p) | DeviceSel::Socket(p) => p.clone(),
            DeviceSel::Netdev(n) => format!("net:{n}"),
            DeviceSel::Usb { vid, pid } => format!("usb:{vid:04x}:{pid:04x}"),
        }
    }
}

/// One `[[devices]]` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceDecl {
    /// What it selects.
    pub sel: DeviceSel,
    /// The owner needs write access too (default true).
    pub write: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawDevice {
    path: Option<String>,
    usb: Option<String>,
    socket: Option<String>,
    netdev: Option<String>,
    access: Option<String>,
}

fn bad(msg: impl Into<String>) -> RuntimeError {
    RuntimeError::InvalidConfig(msg.into())
}

/// `/dev` nodes a cog may never own: raw memory and ports, disks and block devices, consoles and
/// terminals, loop and device-mapper control, the watchdog and the CPU MSRs. A selector that
/// can name any of them (a glob that intersects one) is refused when `cog.toml` is parsed.
pub const DENIED_DEV: &[&str] = &[
    "/dev/mem", "/dev/kmem", "/dev/port", "/dev/kcore", "/dev/nvram", "/dev/cpu/*", "/dev/cpu_dma_latency",
    "/dev/sd*", "/dev/hd*", "/dev/vd*", "/dev/xvd*", "/dev/nvme*", "/dev/mmcblk*", "/dev/dm-*", "/dev/md*",
    "/dev/loop*", "/dev/ram*", "/dev/zram*", "/dev/disk/*", "/dev/block/*", "/dev/mapper/*",
    "/dev/tty", "/dev/console", "/dev/ptmx", "/dev/pts/*", "/dev/vcs*", "/dev/tty?", "/dev/tty1?",
    "/dev/tty2?", "/dev/tty3?", "/dev/tty4?", "/dev/tty5?", "/dev/tty6?", "/dev/watchdog*", "/dev/kmsg",
];

/// Where a socket may live: the system runtime directories. Sockets of the container runtimes
/// and of systemd and D-Bus are refused inside them.
pub const SOCKET_DIRS: &[&str] = &["/run/", "/var/run/"];
const DENIED_SOCKET: &[&str] = &[
    "/run/docker*", "/var/run/docker*", "/run/containerd*", "/var/run/containerd*", "/run/podman*", "/var/run/podman*",
    "/run/systemd/*", "/var/run/systemd/*", "/run/dbus/*", "/var/run/dbus/*", "/run/user/*", "/var/run/user/*",
    "/run/crio*", "/run/snapd*",
];

fn denied(i: usize, key: &str, p: &str, list: &[&str]) -> Result<(), RuntimeError> {
    match list.iter().find(|d| globs_intersect(p, d)) {
        Some(d) => Err(bad(format!("devices[{i}]: {key} {p:?} can name {d}, which no cog may own"))),
        None => Ok(()),
    }
}

/// A glob in the last component needs at least three literal characters, so `/dev/*` and
/// `/dev/s*` are not selectors.
fn enough_literal(i: usize, p: &str) -> Result<(), RuntimeError> {
    let last = p.rsplit('/').next().unwrap_or_default();
    if wild(last) && last.chars().filter(|c| !matches!(c, '*' | '?')).count() < 3 {
        return Err(bad(format!("devices[{i}]: path {p:?} needs at least three literal characters in its last component")));
    }
    Ok(())
}

fn plain_path(i: usize, key: &str, p: &str, globs: bool) -> Result<(), RuntimeError> {
    let chars_ok = p.bytes().all(|b| {
        b.is_ascii_alphanumeric() || b"/_.-".contains(&b) || (globs && b"*?".contains(&b))
    });
    let segs_ok = p.split('/').skip(1).all(|s| s != ".." && s != "." && !s.is_empty());
    let glob_dirs = p.rsplit_once('/').is_some_and(|(dir, _)| !dir.contains(['*', '?']));
    if p.len() > MAX_PATH || !p.starts_with('/') || !chars_ok || !segs_ok {
        return Err(bad(format!(
            "devices[{i}]: {key} {p:?} must be a plain absolute path of at most {MAX_PATH} chars"
        )));
    }
    if !glob_dirs {
        return Err(bad(format!(
            "devices[{i}]: {key} {p:?} may use * or ? in the last component only"
        )));
    }
    Ok(())
}

fn hex4(s: &str) -> Option<u16> {
    (s.len() == 4).then(|| u16::from_str_radix(s, 16).ok()).flatten()
}

/// Validate `[[devices]]`.
pub(super) fn parse_devices(raw: Vec<RawDevice>) -> Result<Vec<DeviceDecl>, RuntimeError> {
    if raw.len() > MAX_DEVICES {
        return Err(bad(format!("more than {MAX_DEVICES} [[devices]]")));
    }
    let mut out: Vec<DeviceDecl> = Vec::with_capacity(raw.len());
    for (i, r) in raw.into_iter().enumerate() {
        let sel = match (r.path, r.usb, r.socket, r.netdev) {
            (Some(p), None, None, None) => {
                plain_path(i, "path", &p, true)?;
                if !p.starts_with("/dev/") {
                    return Err(bad(format!("devices[{i}]: path {p:?} must be under /dev/")));
                }
                enough_literal(i, &p)?;
                denied(i, "path", &p, DENIED_DEV)?;
                DeviceSel::Path(p)
            }
            (None, Some(u), None, None) => {
                let (v, p) = u.split_once(':').unwrap_or_default();
                match (hex4(v), hex4(p)) {
                    (Some(vid), Some(pid)) => DeviceSel::Usb { vid, pid },
                    _ => return Err(bad(format!("devices[{i}]: usb {u:?} must be vid:pid, 4 hex digits each"))),
                }
            }
            (None, None, Some(s), None) => {
                plain_path(i, "socket", &s, false)?;
                if !SOCKET_DIRS.iter().any(|d| s.starts_with(d) && s.len() > d.len()) {
                    return Err(bad(format!("devices[{i}]: socket {s:?} must be under /run/ or /var/run/")));
                }
                denied(i, "socket", &s, DENIED_SOCKET)?;
                DeviceSel::Socket(s)
            }
            (None, None, None, Some(n)) => {
                let ok = !n.is_empty()
                    && n.len() <= 15
                    && n.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_-*?".contains(&b));
                if !ok {
                    return Err(bad(format!("devices[{i}]: netdev {n:?} must be 1..=15 chars of [a-z0-9_-] with * or ?")));
                }
                DeviceSel::Netdev(n)
            }
            _ => return Err(bad(format!("devices[{i}]: give exactly one of path, usb, socket, netdev"))),
        };
        let write = match r.access.as_deref() {
            None | Some("rw") => true,
            Some("ro") => false,
            Some(a) => return Err(bad(format!("devices[{i}]: access {a:?} must be rw or ro"))),
        };
        if out.iter().any(|d| d.sel == sel) {
            return Err(bad(format!("devices[{i}]: {} listed twice", sel.label())));
        }
        out.push(DeviceDecl { sel, write });
    }
    Ok(out)
}
