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
//! ```

use serde::Deserialize;

use super::types::RuntimeError;

/// Most `[[devices]]` tables accepted.
pub const MAX_DEVICES: usize = 8;
const MAX_PATH: usize = 108;

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
}

impl DeviceSel {
    /// The form shown in listings and refusals.
    pub fn label(&self) -> String {
        match self {
            DeviceSel::Path(p) | DeviceSel::Socket(p) => p.clone(),
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
    access: Option<String>,
}

fn bad(msg: impl Into<String>) -> RuntimeError {
    RuntimeError::InvalidConfig(msg.into())
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
        let sel = match (r.path, r.usb, r.socket) {
            (Some(p), None, None) => {
                plain_path(i, "path", &p, true)?;
                if !p.starts_with("/dev/") {
                    return Err(bad(format!("devices[{i}]: path {p:?} must be under /dev/")));
                }
                DeviceSel::Path(p)
            }
            (None, Some(u), None) => {
                let (v, p) = u.split_once(':').unwrap_or_default();
                match (hex4(v), hex4(p)) {
                    (Some(vid), Some(pid)) => DeviceSel::Usb { vid, pid },
                    _ => return Err(bad(format!("devices[{i}]: usb {u:?} must be vid:pid, 4 hex digits each"))),
                }
            }
            (None, None, Some(s)) => {
                plain_path(i, "socket", &s, false)?;
                DeviceSel::Socket(s)
            }
            _ => return Err(bad(format!("devices[{i}]: give exactly one of path, usb, socket"))),
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
