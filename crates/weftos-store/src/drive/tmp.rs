//! Temp files (`.drive.tmp.*`): who is writing one right now, and removing the ones a dead run left.
//!
//! Every writer registers its temp path in [`Live`] for as long as it may exist. The cleanup that
//! runs when a drive is opened skips registered paths and anything younger than the age gate, so
//! it can never delete a file another thread of this process is still writing.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use super::TMP_PREFIX;

/// Default for `Options::stale_tmp_age`: a temp untouched for this long (by mtime) belongs to a run
/// that died. A drive file is at most `Limits::max_file` (16 MiB by default), so a single write
/// takes seconds at worst; 10 minutes is far above that. The number is an owner choice.
pub const DEFAULT_STALE_TMP_AGE: Duration = Duration::from_secs(10 * 60);

fn registry() -> &'static Mutex<HashSet<PathBuf>> {
    static R: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Marks a temp path as in use by this process until dropped (also on unwind).
pub(crate) struct Live(PathBuf);

impl Live {
    pub(crate) fn new(path: &Path) -> Self {
        registry()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(path.to_owned());
        Self(path.to_owned())
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        registry()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.0);
    }
}

fn is_live(p: &Path) -> bool {
    registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(p)
}

/// Remove regular files named `.drive.tmp.*` under `root` that are at least `age` old and not live.
/// Walks real directories only: a symlink is never followed and never removed, so the walk cannot
/// leave `root`. Returns how many files were removed; per-entry errors are skipped.
pub(crate) fn remove_stale(root: &Path, age: Duration) -> usize {
    let now = SystemTime::now();
    let mut removed = 0;
    let mut stack = vec![root.to_owned()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for ent in rd.flatten() {
            let Ok(ft) = ent.file_type() else { continue };
            let path = ent.path();
            if ft.is_dir() {
                stack.push(path);
            } else if ft.is_file() && ent.file_name().to_string_lossy().starts_with(TMP_PREFIX) {
                let old = ent
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| now.duration_since(t).ok())
                    .is_some_and(|d| d >= age);
                if old && !is_live(&path) && std::fs::remove_file(&path).is_ok() {
                    removed += 1;
                }
            }
        }
    }
    removed
}
