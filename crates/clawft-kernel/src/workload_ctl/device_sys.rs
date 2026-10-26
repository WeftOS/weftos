//! The host-touching parts of the device ledger (`device_owner`): glob
//! matching, the sysfs/filesystem resolver and the libc applier.

use std::path::Path;

pub use crate::workload_runtime::cog_devices::{glob_match, globs_intersect, wild};

use super::device_node::{NodeId, node_id};
use super::device_owner::DeviceResolver;

/// The real node: the filesystem and sysfs.
pub struct SysResolver;

impl DeviceResolver for SysResolver {
    fn stat(&self, path: &str) -> Option<(u32, u32, u32)> {
        stat_owner(path)
    }

    fn canonical(&self, path: &str) -> String {
        std::fs::canonicalize(path).ok().and_then(|p| p.to_str().map(String::from)).unwrap_or_else(|| path.to_string())
    }

    fn node_id(&self, path: &str) -> Option<NodeId> {
        node_id(path)
    }

    fn netdevs(&self, pattern: &str) -> Vec<String> {
        let mut out: Vec<String> = std::fs::read_dir("/sys/class/net")
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| glob_match(pattern, n))
            .collect();
        out.sort();
        out
    }

    fn expand(&self, pattern: &str) -> Vec<String> {
        if !wild(pattern) {
            return Path::new(pattern).exists().then(|| pattern.to_string()).into_iter().collect();
        }
        let Some((dir, last)) = pattern.rsplit_once('/') else {
            return Vec::new();
        };
        let mut out: Vec<String> = std::fs::read_dir(if dir.is_empty() { "/" } else { dir })
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| glob_match(last, n))
            .map(|n| format!("{dir}/{n}"))
            .collect();
        out.sort();
        out
    }

    fn usb_nodes(&self, vid: u16, pid: u16) -> Vec<String> {
        let read = |p: &Path| std::fs::read_to_string(p).ok().map(|s| s.trim().to_string());
        let want = (format!("{vid:04x}"), format!("{pid:04x}"));
        let mut roots = Vec::new();
        let mut out = Vec::new();
        for e in std::fs::read_dir("/sys/bus/usb/devices").into_iter().flatten().flatten() {
            let d = e.path();
            if read(&d.join("idVendor")).as_deref() == Some(&want.0)
                && read(&d.join("idProduct")).as_deref() == Some(&want.1)
                && let Ok(real) = d.canonicalize()
            {
                if let (Some(b), Some(n)) = (read(&d.join("busnum")), read(&d.join("devnum"))) {
                    let (b, n) = (b.parse::<u32>().unwrap_or(0), n.parse::<u32>().unwrap_or(0));
                    out.push(format!("/dev/bus/usb/{b:03}/{n:03}"));
                }
                roots.push(real);
            }
        }
        for class in ["video4linux", "sound", "tty", "hidraw", "usbmisc"] {
            for e in std::fs::read_dir(Path::new("/sys/class").join(class)).into_iter().flatten().flatten() {
                let Ok(real) = e.path().join("device").canonicalize() else { continue };
                if !roots.iter().any(|r| real.starts_with(r)) {
                    continue;
                }
                let name = e.file_name().to_string_lossy().to_string();
                if class == "sound" {
                    let card = name.trim_start_matches("card");
                    let hit = |n: &str| n.contains(&format!("C{card}")) && n.starts_with(['p', 'c']);
                    out.extend(
                        self.expand("/dev/snd/*").into_iter().filter(|p| p.rsplit('/').next().is_some_and(hit)),
                    );
                } else if Path::new(&format!("/dev/{name}")).exists() {
                    out.push(format!("/dev/{name}"));
                }
            }
        }
        out.sort();
        out.dedup();
        out
    }
}

/// `(uid, gid, mode)` of `path` on this host.
pub fn stat_owner(path: &str) -> Option<(u32, u32, u32)> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).ok().map(|m| (m.uid(), m.gid(), m.mode() & 0o7777))
}

