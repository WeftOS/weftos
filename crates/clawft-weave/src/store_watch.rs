//! The daemon's tail of RustFS's own JSON log (ADR-126 S1), so that requests RustFS refused reach
//! the activity log and, in a run, suspend the key: `s3_authorization_denied` (a known key refused
//! by policy) and `SignatureDoesNotMatch` / `InvalidAccessKeyId` (refused before the caller was
//! known). The audit and notification posts have their own receiver, `store_ingest`.
//!
//! - **Off unless `WEFTOS_STORE_RUSTFS_LOG` names the file** RustFS writes (its stdout/stderr
//!   redirected to a file, or its log directory's `rustfs.log`). Without it, or without the RustFS
//!   backend, nothing runs; the daemon is unaffected.
//! - Starts at the end of the file (a restart does not replay old refusals into the rule), follows
//!   it across truncation and rotation, and reads only complete lines.
//! - `WEFTOS_STORE_DENIAL_RULE` sets the suspension rule: `off`, or `<max>/<seconds>` (default 20/60).

use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use weftos_store::StoreService;
use weftos_store::service::DenialRule;

/// The longest line kept; a longer one is skipped whole (RustFS lines are well under this).
const MAX_LINE: usize = 64 * 1024;
/// The most read in one poll, so a huge backlog is worked off over several polls.
const MAX_POLL: u64 = 4 * 1024 * 1024;

/// Reads the complete lines appended to a file since the last poll.
pub struct LogTail {
    path: PathBuf,
    pos: u64,
    ino: Option<u64>,
    started: bool,
    from_start: bool,
    /// The tail of a line that has not ended yet.
    partial: Vec<u8>,
    skipping: bool,
}

impl LogTail {
    /// Follow `path` from its current end.
    pub fn new(path: &Path) -> Self {
        Self { path: path.to_owned(), pos: 0, ino: None, started: false, from_start: false, partial: Vec::new(), skipping: false }
    }

    /// Follow `path` from its beginning (tests, and a file known to be new).
    pub fn from_start(path: &Path) -> Self {
        Self { from_start: true, ..Self::new(path) }
    }

    /// Hand every new complete line to `f`. A missing file is not an error (RustFS may not have
    /// written yet); a file that shrank or was replaced is read again from its start.
    pub fn poll(&mut self, mut f: impl FnMut(&str)) -> std::io::Result<usize> {
        let mut file = match std::fs::File::open(&self.path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(e),
        };
        let md = file.metadata()?;
        if !self.started {
            self.started = true;
            self.pos = if self.from_start { 0 } else { md.len() };
            self.ino = Some(md.ino());
        } else if self.ino != Some(md.ino()) || md.len() < self.pos {
            self.pos = 0;
            self.ino = Some(md.ino());
            self.partial.clear();
            self.skipping = false;
        }
        if md.len() <= self.pos {
            return Ok(0);
        }
        file.seek(SeekFrom::Start(self.pos))?;
        let mut buf = Vec::new();
        file.take(MAX_POLL).read_to_end(&mut buf)?;
        self.pos += buf.len() as u64;
        let mut n = 0;
        for chunk in buf.split_inclusive(|b| *b == b'\n') {
            let ended = chunk.last() == Some(&b'\n');
            if self.skipping {
                if ended {
                    self.skipping = false;
                }
                continue;
            }
            self.partial.extend_from_slice(chunk);
            if self.partial.len() > MAX_LINE {
                self.partial.clear();
                self.skipping = !ended;
                continue;
            }
            if ended {
                let line = String::from_utf8_lossy(&self.partial).trim_end().to_owned();
                self.partial.clear();
                if !line.is_empty() {
                    f(&line);
                    n += 1;
                }
            }
        }
        Ok(n)
    }
}

/// Poll once and give each line to the service. Returns how many lines it took as records.
pub fn pump(tail: &mut LogTail, svc: &StoreService) -> std::io::Result<usize> {
    let mut taken = 0;
    tail.poll(|l| taken += usize::from(svc.ingest_log_line(l)))?;
    Ok(taken)
}

/// The suspension rule from `WEFTOS_STORE_DENIAL_RULE`: unset is the default, `off` is none,
/// `<max>/<seconds>` names it.
pub fn denial_rule_from(v: Option<&str>) -> Result<Option<DenialRule>, String> {
    let Some(v) = v.map(str::trim).filter(|v| !v.is_empty()) else { return Ok(Some(DenialRule::default())) };
    if v.eq_ignore_ascii_case("off") {
        return Ok(None);
    }
    let bad = || format!("WEFTOS_STORE_DENIAL_RULE={v:?}: want `off` or `<max>/<seconds>`, for example 20/60");
    let (m, s) = v.split_once('/').ok_or_else(bad)?;
    let (max, secs): (u32, u64) = (m.trim().parse().map_err(|_| bad())?, s.trim().parse().map_err(|_| bad())?);
    if max == 0 || secs == 0 {
        return Err(bad());
    }
    Ok(Some(DenialRule { max, window_ms: secs.saturating_mul(1000) }))
}

/// Apply `WEFTOS_STORE_DENIAL_RULE`, and start the tail if `WEFTOS_STORE_RUSTFS_LOG` is set. A bad
/// rule value is reported and the default stays.
pub fn start_from_env(svc: &Arc<StoreService>) {
    match denial_rule_from(std::env::var("WEFTOS_STORE_DENIAL_RULE").ok().as_deref()) {
        Ok(rule) => svc.set_denial_rule(rule),
        Err(e) => tracing::warn!(error = %e, "denial rule not changed"),
    }
    let Some(path) = std::env::var_os("WEFTOS_STORE_RUSTFS_LOG").filter(|v| !v.is_empty()) else { return };
    spawn_tail(svc, PathBuf::from(path), Duration::from_secs(1));
}

/// Poll `path` every `every` for as long as the service lives.
pub fn spawn_tail(svc: &Arc<StoreService>, path: PathBuf, every: Duration) {
    let weak = Arc::downgrade(svc);
    let started = std::thread::Builder::new().name("store-rustfs-log".into()).spawn(move || {
        let mut tail = LogTail::new(&path);
        let mut failing = false;
        loop {
            std::thread::sleep(every);
            let Some(svc) = weak.upgrade() else { return };
            match pump(&mut tail, &svc) {
                Ok(_) => failing = false,
                Err(e) if !failing => {
                    failing = true;
                    tracing::warn!(error = %e, file = %path.display(), "RustFS log not readable");
                }
                Err(_) => {}
            }
        }
    });
    match started {
        Ok(_) => tracing::info!("store: following the RustFS log for refusals"),
        Err(e) => tracing::warn!(error = %e, "RustFS log tail not started"),
    }
}

#[cfg(test)]
#[path = "store_watch_tests.rs"]
mod tests;
