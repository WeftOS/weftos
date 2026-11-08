//! ADR-131 "materialise plus watch" prototype over the ADR-126 object store.
//!
//! A drive is a directory bound to one `(bucket, prefix)`. Every file in it is a real file on the
//! local file system. Beside the tree, `<dest>/.drive/` holds the state:
//!
//! - `cas/<hh>/<blake3>`: the content-addressed cache every placed file is cloned from;
//! - `index.json`: per path `(size, mtime, hash, etag)` as of the last sync, plus open conflicts;
//! - `conflicts.jsonl`: append-only record of every conflict (never resolved silently);
//! - `events.jsonl`: append-only record of what each sync placed, removed, put or deleted.
//!
//! Entry points: [`materialise`] (first fill, and refresh), [`Watcher`] / [`watch_once`]
//! (detect local changes after a quiet period), [`sync_up`], [`sync_down`], [`resolve`].
//!
//! Library only: the daemon loop of ADR-131 is meant to call these same functions.

mod cas;
mod conflict;
mod index;
mod materialise;
mod remote;
mod scan;
mod sync_up;
mod watch;

#[cfg(all(test, feature = "rustfs"))]
mod live_tests;
#[cfg(test)]
mod tests;

use std::path::Path;
use std::time::Duration;

use serde::Serialize;

use crate::backend::BackendError;
use crate::share::Limits;

pub use conflict::{ConflictKind, Keep, resolve};
pub use index::{Entry, Index, OpenConflict};
pub use materialise::{DownReport, materialise, materialise_with, sync_down, sync_down_with};
pub use scan::{Change, ChangeKind, Scan, scan};
pub use sync_up::{UpReport, sync_up, sync_up_with};
pub use watch::{WatchConfig, Watcher, watch_once};
#[cfg(feature = "drive-notify")]
pub use watch::watch_once_events;

/// Drive state lives here, directly under the drive root. A remote key whose first segment is
/// this name is refused, so a bucket can never overwrite the drive's own state.
pub const DRIVE_DIR: &str = ".drive";
/// Delete markers (ADR-126 section 4 style: one small object per deleted key, removed when the key
/// is written again) live under `<prefix>.drive-tombstones/`. Never materialised.
pub const TOMBSTONE_DIR: &str = ".drive-tombstones";
/// Temp files written next to their target and renamed into place.
pub(crate) const TMP_PREFIX: &str = ".drive.tmp.";
/// ADR-131 section 3.3: seal after 2 s of quiet.
pub const DEFAULT_QUIET: Duration = Duration::from_secs(2);

/// Errors of the drive layer.
#[derive(Debug, thiserror::Error)]
pub enum DriveError {
    #[error(transparent)]
    Backend(#[from] BackendError),
    #[error("io: {0}")]
    Io(String),
    #[error("json: {0}")]
    Json(String),
    #[error("{0}")]
    Invalid(String),
}

impl From<std::io::Error> for DriveError {
    fn from(e: std::io::Error) -> Self {
        DriveError::Io(e.to_string())
    }
}
impl From<serde_json::Error> for DriveError {
    fn from(e: serde_json::Error) -> Self {
        DriveError::Json(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, DriveError>;

/// Why a path was left out. Always reported, never silent (except the drive's own files).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipWhy {
    /// The existing secret-name denylist (`share::is_secret_path`).
    Secret,
    /// Not a usable relative path (traversal, empty segment, NUL, backslash, directory marker).
    BadPath,
    /// VCS control files and Windows-hostile names (`share::is_dangerous_path`).
    Dangerous,
    /// The drive's own state area or the daemon's control area of the bucket.
    Reserved,
    /// Over `Limits::max_file`.
    TooLarge,
    /// Over `Limits::max_files` or `Limits::max_total`.
    OverBudget,
    /// Two remote keys differ only by case; the first (in key order) wins.
    CaseCollision,
    /// A symlink; never followed, never uploaded.
    Symlink,
    /// Not a regular file or directory.
    NotAFile,
    /// The path changed while it was being read.
    Changed,
    /// Placing or reading it failed; see the detail.
    Io,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    pub path: String,
    pub why: SkipWhy,
    pub detail: String,
}

impl Skipped {
    pub(crate) fn new(path: &str, why: SkipWhy) -> Self {
        Self { path: path.to_owned(), why, detail: String::new() }
    }
    pub(crate) fn io(path: &str, e: impl std::fmt::Display) -> Self {
        Self { path: path.to_owned(), why: SkipWhy::Io, detail: e.to_string() }
    }
}

/// Knobs shared by the sync functions.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// The existing S2 ceilings (16 MiB per file, 20,000 files, 512 MiB).
    pub limits: Limits,
    /// Written into conflict records and tombstones. Default: the id stored in the index.
    pub device: Option<String>,
}

/// Normalise a prefix: empty, or ending in exactly one `/`.
pub(crate) fn norm_prefix(p: &str) -> String {
    let t = p.trim_matches('/');
    if t.is_empty() { String::new() } else { format!("{t}/") }
}

/// Is `rel` a path the drive may place or upload? Used for remote keys and local files alike.
pub(crate) fn check_rel(rel: &str) -> std::result::Result<(), SkipWhy> {
    if crate::overlay::validate_key(rel).is_err() {
        return Err(SkipWhy::BadPath);
    }
    let first = rel.split('/').next().unwrap_or("");
    if first == DRIVE_DIR || first == TOMBSTONE_DIR || rel.split('/').any(|s| s.starts_with(TMP_PREFIX)) {
        return Err(SkipWhy::Reserved);
    }
    if crate::share::is_dangerous_path(rel) {
        return Err(SkipWhy::Dangerous);
    }
    if crate::share::is_secret_path(rel) {
        return Err(SkipWhy::Secret);
    }
    Ok(())
}

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// Append one JSON line to a file under `<dest>/.drive/`.
pub(crate) fn append_jsonl(dest: &Path, name: &str, v: &impl Serialize) -> Result<()> {
    use std::io::Write;
    let mut line = serde_json::to_vec(v)?;
    line.push(b'\n');
    std::fs::create_dir_all(dest.join(DRIVE_DIR))?;
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(dest.join(DRIVE_DIR).join(name))?;
    f.write_all(&line)?;
    Ok(())
}

pub(crate) fn log_event(dest: &Path, op: &str, path: &str) {
    // The event log is a convenience for people; a failure to write it never fails a sync.
    let _ = append_jsonl(dest, "events.jsonl", &serde_json::json!({"ts_ms": now_ms(), "op": op, "path": path}));
}
