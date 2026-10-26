//! The host-touching parts of the device ledger (`device_owner`): glob
//! matching, the sysfs/filesystem resolver and the libc applier.

use std::path::Path;

use super::device_owner::{DeviceApplier, DeviceResolver, GrantOp};

/// True if `s` holds a glob character.
pub(super) fn wild(s: &str) -> bool {
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

/// The real node: the filesystem and sysfs.
pub struct SysResolver;

impl DeviceResolver for SysResolver {
    fn stat(&self, path: &str) -> Option<(u32, u32, u32)> {
        stat_owner(path)
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

/// `chown` and `chmod` through libc; needs root or `CAP_CHOWN`.
pub struct SystemApplier;

impl DeviceApplier for SystemApplier {
    fn apply(&self, ops: &[GrantOp]) -> Result<(), String> {
        for GrantOp::Chown { path, uid, gid, mode } in ops {
            let c = std::ffi::CString::new(path.as_str()).map_err(|e| e.to_string())?;
            // SAFETY: `c` is a NUL-terminated path that outlives both calls.
            let (a, b) = unsafe { (libc::chown(c.as_ptr(), *uid, *gid), libc::chmod(c.as_ptr(), *mode as libc::mode_t)) };
            if a != 0 || b != 0 {
                return Err(format!("{path}: {}", std::io::Error::last_os_error()));
            }
        }
        Ok(())
    }
}

/// `(uid, gid, mode)` of `path` on this host.
pub fn stat_owner(path: &str) -> Option<(u32, u32, u32)> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).ok().map(|m| (m.uid(), m.gid(), m.mode() & 0o7777))
}

