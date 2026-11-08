//! Detect local creates, modifies and deletes, and report them once the tree has been quiet.
//!
//! The baseline is a scan against the index (no new dependency). A change is reported only after
//! the scan result has stopped changing for `quiet` (ADR-131 section 3.3: seal after 2 s of quiet),
//! and reported once: until the index is updated by a sync, the same batch is not reported again.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use super::index::Index;
use super::scan::{Change, ChangeKind, scan};
use super::{DEFAULT_QUIET, DRIVE_DIR, DriveError, Result};
use crate::share::Limits;

#[derive(Debug, Clone)]
pub struct WatchConfig {
    /// How long the tree must be unchanged before a batch is reported.
    pub quiet: Duration,
    /// Scan interval for [`watch_once`].
    pub poll: Duration,
    pub limits: Limits,
}

impl Default for WatchConfig {
    fn default() -> Self {
        Self { quiet: DEFAULT_QUIET, poll: Duration::from_millis(200), limits: Limits::default() }
    }
}

type Sig = BTreeMap<String, (ChangeKind, u64, u64)>;

pub struct Watcher {
    dest: PathBuf,
    cfg: WatchConfig,
    index: Index,
    index_stamp: Option<(u64, SystemTime)>,
    last_sig: Sig,
    last_activity: Option<Instant>,
    reported: Option<Sig>,
}

fn stamp(dest: &Path) -> Option<(u64, SystemTime)> {
    let md = std::fs::metadata(dest.join(DRIVE_DIR).join("index.json")).ok()?;
    Some((md.len(), md.modified().ok()?))
}

impl Watcher {
    pub fn new(dest: &Path, cfg: WatchConfig) -> Result<Self> {
        let index = Index::load(dest)?.ok_or_else(|| DriveError::Invalid(format!("{} is not a drive", dest.display())))?;
        Ok(Self {
            dest: dest.to_owned(),
            cfg,
            index,
            index_stamp: stamp(dest),
            last_sig: Sig::new(),
            last_activity: None,
            reported: None,
        })
    }

    /// One scan at time `now`. Returns the pending changes once they have been quiet for
    /// `quiet`, and `None` before that or when nothing changed. `now` is a parameter so tests
    /// control time.
    pub fn poll(&mut self, now: Instant) -> Result<Option<Vec<Change>>> {
        let st = stamp(&self.dest);
        if st != self.index_stamp {
            // A sync rewrote the index: start from what it says.
            self.index = Index::load(&self.dest)?.ok_or_else(|| DriveError::Invalid("index vanished".into()))?;
            self.index_stamp = st;
        }
        let s = scan(&self.dest, &self.index, &self.cfg.limits)?;
        for (p, size, mt) in &s.touched {
            self.index.touch(p, *size, *mt);
        }
        let sig: Sig = s.changes.iter().map(|c| (c.path.clone(), (c.kind, c.size, c.mtime_ns))).collect();
        if sig != self.last_sig {
            self.last_sig = sig.clone();
            self.last_activity = Some(now);
            self.reported = None;
        }
        let Some(since) = self.last_activity.filter(|_| !sig.is_empty()) else { return Ok(None) };
        if now.saturating_duration_since(since) >= self.cfg.quiet && self.reported.as_ref() != Some(&sig) {
            self.reported = Some(sig);
            return Ok(Some(s.changes));
        }
        Ok(None)
    }
}

impl Watcher {
    /// How long a caller may sleep before the next poll: the poll interval, or less when a batch is
    /// pending and its quiet period ends sooner.
    pub fn next_wake(&self, now: Instant) -> Duration {
        let pending = !self.last_sig.is_empty() && self.reported.as_ref() != Some(&self.last_sig);
        match self.last_activity.filter(|_| pending) {
            Some(since) => {
                let left = self.cfg.quiet.saturating_sub(now.saturating_duration_since(since));
                left.max(Duration::from_millis(10)).min(self.cfg.poll)
            }
            None => self.cfg.poll,
        }
    }
}

/// Block, scanning every `cfg.poll`, until a quiet batch of changes exists or `timeout` passes.
pub fn watch_once(dest: &Path, cfg: &WatchConfig, timeout: Duration) -> Result<Option<Vec<Change>>> {
    let mut w = Watcher::new(dest, cfg.clone())?;
    let start = Instant::now();
    loop {
        let now = Instant::now();
        if let Some(c) = w.poll(now)? {
            return Ok(Some(c));
        }
        if now.saturating_duration_since(start) >= timeout {
            return Ok(None);
        }
        std::thread::sleep(w.next_wake(Instant::now()));
    }
}

/// Like [`watch_once`], but wakes on file-system events (FSEvents, inotify) instead of sleeping the
/// whole poll interval, so a change is noticed at once and the quiet period starts at the event.
/// The scan is still what decides; events only say "look now". `cfg.poll` stays the fallback wake
/// for a dropped or coalesced event.
#[cfg(feature = "drive-notify")]
pub fn watch_once_events(dest: &Path, cfg: &WatchConfig, timeout: Duration) -> Result<Option<Vec<Change>>> {
    use notify::{RecursiveMode, Watcher as _};
    let (tx, rx) = std::sync::mpsc::channel();
    let mut nw = notify::recommended_watcher(move |_ev: notify::Result<notify::Event>| {
        let _ = tx.send(());
    })
    .map_err(|e| DriveError::Io(format!("file watcher: {e}")))?;
    nw.watch(dest, RecursiveMode::Recursive).map_err(|e| DriveError::Io(format!("file watcher: {e}")))?;
    let mut w = Watcher::new(dest, cfg.clone())?;
    let start = Instant::now();
    loop {
        let now = Instant::now();
        if let Some(c) = w.poll(now)? {
            return Ok(Some(c));
        }
        if now.saturating_duration_since(start) >= timeout {
            return Ok(None);
        }
        // Event or timer, whichever first; then drain the burst.
        let _ = rx.recv_timeout(w.next_wake(now));
        while rx.try_recv().is_ok() {}
    }
}
