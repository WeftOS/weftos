//! L1 follower view (ADR-130 L1 tier, ADR-126 §4 owner extension, board card f40828fb): lets any
//! node see unmerged worktrees *before* integration, and surfaces forgotten ones.
//!
//! A follower is read-only. It reads the project registry (`worktrees/<slug>.json`, written by the
//! daemon), and from there each worktree's manifest. Nothing here writes to a worktree bucket; the
//! only writes are to a local cache directory and, for the daemon-side helpers, the registry.
//!
//! What it gives a node:
//! - [`poll`]: what changed since this follower last looked (new, head moved, state changed, gone);
//! - [`search_docs`]: docs and planning across *all* unmerged worktrees;
//! - [`sync_docs`]: a local read-only mirror of one worktree's docs;
//! - [`leftovers`]: worktrees nobody is finishing (the "forgotten docs" failure this exists to end);
//! - [`overlaps`]: files two unmerged worktrees both changed differently, before anyone merges.

use crate::overlay::{ObjectBackend, Result, S2Error};
use crate::share::State;
use crate::share_fetch::{RegistryEntry, list_shared_lenient};
pub use crate::follower_browse::{browse_get, browse_ls, browse_registry};
pub use crate::follower_docs::{
    DocHit, SearchResult, remove_registry_entry, search_docs, sync_docs, sync_docs_from, verify_entry,
};
pub use crate::follower_report::{
    Leftover, LeftoverPolicy, LeftoverReport, Overlap, OverlapKind, OverlapReport, Why, leftovers, leftovers_report, overlaps,
    overlaps_from,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Seen {
    /// Etag of the registry object when it was read; with the digest it lets a poll skip the
    /// entry. `None` for state saved before the digest existed.
    #[serde(default)]
    pub etag: Option<String>,
    pub bucket: String,
    pub head_commit: String,
    pub state: State,
}

/// A follower's memory of what it has already reported. Persist it as JSON between polls.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FollowState {
    pub seen: BTreeMap<String, Seen>,
    /// Highest registry signature generation verified per worktree (`registry_sign::poll_verified`).
    /// Kept after a worktree leaves the registry, so a replayed older entry is still refused.
    #[serde(default)]
    pub generations: BTreeMap<String, u64>,
}

impl FollowState {
    /// Load from `path`. A missing file is a first run (empty state). An unreadable or corrupt
    /// file is an error: silently resetting would re-report every worktree as `New`.
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| S2Error::Invalid(format!("follow state {}: {e}", path.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(S2Error::Invalid(format!(
                "follow state {}: {e}",
                path.display()
            ))),
        }
    }

    /// Write to `path` through a temp file in the same directory, then rename, so a crash or a
    /// concurrent poll never leaves a truncated file.
    ///
    /// Invariant (WO-069 6g, G3): after `save` returns `Ok`, the generation on disk for every slug
    /// is at least the one this saver held, and no concurrent saves lower one that a completed save
    /// wrote. The generations are what refuse a replayed older signed entry, so a run with an older
    /// view that saves last must not take that protection away. The save writes the per-slug
    /// maximum of the file's `generations` and ours, under an exclusive `flock` on the state
    /// directory (std `File::lock`, no lock file is created) so the re-read and the rename are one
    /// step for every saver. Where the directory cannot be locked (Windows, a filesystem that refuses
    /// a directory lock) a sibling `.<name>.lock` file is locked instead (WO-069 lane Y). The locks
    /// are the fast path: a lock-free verify-and-repair pass always runs after the rename (it reads
    /// the file back and writes again if anything is lower), so the invariant also holds where no
    /// advisory lock works at all; see `follower_save.rs` for the argument and the one residual
    /// (two processes, one syscall wide). A file that cannot be parsed contributes nothing and is
    /// replaced (that is how an operator recovers it). If the file cannot be made stable after a
    /// bounded number of rounds, `save` returns an error and the next save merges again.
    ///
    /// Only the generations are merged. `seen` is last writer wins: two pollers sharing one file can
    /// overwrite each other's view, and the loser's next run re-reports, it never skips; give each
    /// its own state file. The in-memory `self` is not changed.
    pub fn save(&self, path: &Path) -> Result<()> {
        self.save_locked(path, true)
    }

    /// [`Self::save`] with the directory lock switchable, the seam the lock tests use to take the
    /// path a platform without one takes.
    pub(crate) fn save_locked(&self, path: &Path, dir_lock: bool) -> Result<()> {
        let io =
            |e: std::io::Error| S2Error::Invalid(format!("follow state {}: {e}", path.display()));
        let dir = path
            .parent()
            .filter(|d| !d.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        std::fs::create_dir_all(dir).map_err(io)?;
        let _lock = lock_state(dir, path, dir_lock);
        self.save_repairing(path, dir, &io)
    }

    /// Write `self` to a fresh temp file beside `path` and return its path (see [`Self::commit_temp`]).
    fn stage_temp(&self, path: &Path, dir: &Path, io: &dyn Fn(std::io::Error) -> S2Error) -> Result<std::path::PathBuf> {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("follow");
        // Unique per call, not per process: the daemon serves overlapping `follow` calls on the
        // same state file from threads of one process, and a shared temp name would let one call
        // rename the other's file away.
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = dir.join(format!(".{name}.{}.{seq}.tmp", std::process::id()));
        let json = serde_json::to_vec_pretty(self).map_err(|e| S2Error::Invalid(e.to_string()))?;
        #[cfg(test)]
        crate::follower_seam::count_write();
        std::fs::write(&tmp, json).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            io(e)
        })?;
        Ok(tmp)
    }

    /// Rename a staged temp file over `path`.
    fn commit_temp(tmp: &Path, path: &Path, io: &dyn Fn(std::io::Error) -> S2Error) -> Result<()> {
        std::fs::rename(tmp, path).map_err(|e| {
            let _ = std::fs::remove_file(tmp);
            io(e)
        })
    }
}

/// An exclusive advisory lock on `dir`, held until the returned handle drops. `flock` on a directory
/// handle works on Linux and macOS and creates no file. `None` when the directory cannot be opened
/// or locked; [`lock_state`] then locks a file instead.
fn lock_dir(dir: &Path) -> Option<std::fs::File> {
    let f = std::fs::File::open(dir).ok()?;
    f.lock().ok()?;
    Some(f)
}

/// The lock `save` holds across its read-merge-rename: the directory itself where that works, else
/// (WO-069 lane Y, item 4) a sibling `.<name>.lock` file. A directory cannot be opened as a file on
/// Windows (inferred, not run there) and some filesystems refuse a directory lock; without the
/// fallback the merge ran unlocked there and a racing save could lower a generation. The lock file
/// is left in place, empty: the OS drops the lock with its holder, so it is never stale, and
/// removing it after use would let two savers lock two different files. `None` only on a
/// filesystem with no advisory locks at all; the merge then runs unlocked, as before.
fn lock_state(dir: &Path, path: &Path, dir_lock: bool) -> Option<std::fs::File> {
    #[cfg(test)]
    if crate::follower_seam::no_locks() {
        return None;
    }
    if dir_lock && let Some(f) = lock_dir(dir) {
        return Some(f);
    }
    let name = path.file_name()?.to_str()?;
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join(format!(".{name}.lock")))
        .ok()?;
    f.lock().ok()?;
    Some(f)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    New(RegistryEntry),
    HeadMoved {
        entry: RegistryEntry,
        from: String,
    },
    StateChanged {
        entry: RegistryEntry,
        from: State,
    },
    /// Left the registry: merged and cleaned up, or removed by the daemon.
    Gone {
        slug: String,
    },
    /// The registry entry is corrupt or from a newer schema; reported every poll until fixed.
    Unreadable {
        slug: String,
    },
}

/// Compare the registry with what this follower last saw, report differences, and remember them.
/// A head move and a state change in the same poll are both reported. A slug whose bucket changed
/// is reported as `Gone` then `New` (the worktree was recreated). A corrupt entry is reported as
/// `Unreadable` and does not blind the rest; its previous state is kept.
///
/// If the registry lists nothing but this follower knows of worktrees, that is more likely a wrong
/// or unavailable project bucket than every worktree vanishing, so it is an error unless
/// `allow_all_gone` is set (see [`poll_confirmed`]).
pub fn poll<B: ObjectBackend>(
    b: &B,
    project_bucket: &str,
    state: &mut FollowState,
) -> Result<Vec<Change>> {
    poll_with(b, project_bucket, state, false)
}

/// [`poll`] that accepts an empty registry as real.
pub fn poll_confirmed<B: ObjectBackend>(
    b: &B,
    project_bucket: &str,
    state: &mut FollowState,
) -> Result<Vec<Change>> {
    poll_with(b, project_bucket, state, true)
}

fn poll_with<B: ObjectBackend>(
    b: &B,
    project_bucket: &str,
    state: &mut FollowState,
    allow_all_gone: bool,
) -> Result<Vec<Change>> {
    let (entries, bad) = list_shared_lenient(b, project_bucket)?;
    let total = entries.len() + bad.len();
    poll_core(state, entries.into_iter().map(|e| (e, None)).collect(), bad, Vec::new(), total, allow_all_gone)
}

/// [`poll`] for a backend that can be read from several threads: the registry entries are fetched
/// `jobs` at a time, and with `use_digest` the daemon-written digest replaces the listing, so only
/// entries whose etag changed since this follower last looked are fetched at all. Same results as
/// [`poll`]; a missing digest falls back to listing.
pub fn poll_fast<B: ObjectBackend + Sync>(
    b: &B,
    project_bucket: &str,
    state: &mut FollowState,
    allow_all_gone: bool,
    use_digest: bool,
    jobs: usize,
) -> Result<Vec<Change>> {
    poll_fast_cached(b, project_bucket, state, allow_all_gone, use_digest, jobs, None)
}

/// [`poll_fast`] that also tells a browse [`crate::follower_cache::ManifestCache`] which registry
/// entry etags it just saw, so a moved or removed worktree drops its cached manifest at once.
pub fn poll_fast_cached<B: ObjectBackend + Sync>(
    b: &B,
    project_bucket: &str,
    state: &mut FollowState,
    allow_all_gone: bool,
    use_digest: bool,
    jobs: usize,
    cache: Option<&mut crate::follower_cache::ManifestCache>,
) -> Result<Vec<Change>> {
    match cache {
        Some(c) => poll_fast_observed(b, project_bucket, state, allow_all_gone, use_digest, jobs, &mut |cur| {
            c.observe_registry(project_bucket, cur);
        }),
        None => poll_fast_observed(b, project_bucket, state, allow_all_gone, use_digest, jobs, &mut |_| {}),
    }
}

/// [`poll_fast_cached`] for a cache the caller shares behind a lock: `observe` gets the registry's
/// `slug -> entry etag` after the reads are done, so the lock is taken only for the observation.
pub fn poll_fast_observed<B: ObjectBackend + Sync>(
    b: &B,
    project_bucket: &str,
    state: &mut FollowState,
    allow_all_gone: bool,
    use_digest: bool,
    jobs: usize,
    observe: &mut dyn FnMut(&BTreeMap<String, String>),
) -> Result<Vec<Change>> {
    let mut digest = use_digest.then(|| crate::share_fetch::read_digest(b, project_bucket)).transpose()?.flatten();
    // The digest is a hint. A follower that knows a worktree the digest does not list does not
    // report it gone on the hint's word (a lost or half-written digest would): it falls back to the
    // listing, which is the truth. The digest-hit poll where nothing known went missing is unchanged.
    if digest.as_ref().is_some_and(|d| state.seen.keys().any(|k| !d.entries.contains_key(k))) {
        digest = None;
    }
    let mut current = match digest {
        Some(d) => d.entries,
        None => crate::share_fetch::registry_listing(b, project_bucket)?,
    };
    let (mut want, mut carried) = (Vec::new(), Vec::new());
    for (slug, etag) in &current {
        match state.seen.get(slug) {
            Some(s) if s.etag.as_ref() == Some(etag) => carried.push((slug.clone(), s.clone())),
            _ => want.push(slug.clone()),
        }
    }
    let (fresh, bad, missing) = crate::share_fetch::fetch_entries_split(b, project_bucket, &want, jobs)?;
    // A slug the digest names but whose object does not exist was removed: not unreadable, and not
    // reported at all to a follower that never knew it (one that did know it sees `Gone`).
    for slug in &missing {
        current.remove(slug);
    }
    observe(&current);
    let fresh = fresh.into_iter().map(|e| { let t = current.get(&e.slug).cloned(); (e, t) }).collect();
    poll_core(state, fresh, bad, carried, current.len(), allow_all_gone)
}

fn poll_core(
    state: &mut FollowState,
    fresh: Vec<(RegistryEntry, Option<String>)>,
    bad: Vec<String>,
    carried: Vec<(String, Seen)>,
    total: usize,
    allow_all_gone: bool,
) -> Result<Vec<Change>> {
    if total == 0 && !state.seen.is_empty() && !allow_all_gone {
        return Err(S2Error::Invalid(format!(
            "registry is empty but {} worktree(s) were known; wrong project bucket? (poll_confirmed accepts it)",
            state.seen.len()
        )));
    }
    let mut out = Vec::new();
    let mut now_seen: BTreeMap<String, Seen> = carried.into_iter().collect();
    for (e, etag) in fresh {
        let seen = state.seen.get(&e.slug);
        match seen {
            Some(s) if s.bucket != e.bucket => {
                out.push(Change::Gone {
                    slug: e.slug.clone(),
                });
                out.push(Change::New(e.clone()));
            }
            None => out.push(Change::New(e.clone())),
            Some(s) => {
                if s.head_commit != e.head_commit {
                    out.push(Change::HeadMoved {
                        from: s.head_commit.clone(),
                        entry: e.clone(),
                    });
                }
                if s.state != e.state {
                    out.push(Change::StateChanged {
                        from: s.state,
                        entry: e.clone(),
                    });
                }
            }
        }
        now_seen.insert(
            e.slug.clone(),
            Seen {
                etag,
                bucket: e.bucket,
                head_commit: e.head_commit,
                state: e.state,
            },
        );
    }
    for slug in bad {
        if let Some(prev) = state.seen.get(&slug) {
            // Keep what was known, but forget the etag so the entry is fetched (and reported) again.
            now_seen.insert(slug.clone(), Seen { etag: None, ..prev.clone() });
        }
        out.push(Change::Unreadable { slug });
    }
    for slug in state.seen.keys() {
        if !now_seen.contains_key(slug) {
            out.push(Change::Gone { slug: slug.clone() });
        }
    }
    state.seen = now_seen;
    Ok(out)
}

/// The comparison half of [`poll`], on an already-listed registry (`registry_sign::poll_verified`
/// lists through a trust filter and shares this).
pub(crate) fn diff_poll(
    entries: Vec<RegistryEntry>,
    bad: Vec<String>,
    state: &mut FollowState,
    allow_all_gone: bool,
) -> Result<Vec<Change>> {
    let total = entries.len() + bad.len();
    poll_core(state, entries.into_iter().map(|e| (e, None)).collect(), bad, Vec::new(), total, allow_all_gone)
}

#[path = "follower_save.rs"]
mod save;
#[cfg(test)]
#[path = "follower_lock_tests.rs"]
mod lock_tests;
#[cfg(test)]
#[path = "follower_repair_tests.rs"]
mod repair_tests;
