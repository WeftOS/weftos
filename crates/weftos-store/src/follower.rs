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

use crate::overlay::{ObjectBackend, Result, S2Error, validate_key};
use crate::promote::read_meta;
use crate::share::{CLAIM_TTL_MS, ChangeKind, State, read_manifest, safe_join};
use crate::share_fetch::{
    FetchReport, REGISTRY_PREFIX, RegistryEntry, Select, fetch_tree, list_shared_lenient,
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
    /// concurrent poll never leaves a truncated file. Two pollers sharing one file can still
    /// overwrite each other's last write; give each its own state file.
    pub fn save(&self, path: &Path) -> Result<()> {
        let io =
            |e: std::io::Error| S2Error::Invalid(format!("follow state {}: {e}", path.display()));
        let dir = path
            .parent()
            .filter(|d| !d.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        std::fs::create_dir_all(dir).map_err(io)?;
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
        std::fs::write(&tmp, json).map_err(io)?;
        std::fs::rename(&tmp, path).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            io(e)
        })
    }
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
    let current = match use_digest.then(|| crate::share_fetch::read_digest(b, project_bucket)).transpose()?.flatten() {
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
    let (fresh, bad) = crate::share_fetch::fetch_entries(b, project_bucket, &want, jobs)?;
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

/// A registry entry is a pointer, not authority: before reading a bucket on its say-so, check the
/// slug is well-formed and the bucket's own `.wt/meta.json` names the same slug. This stops an
/// entry from redirecting a follower (which holds its own read grants) at someone else's bucket.
pub fn verify_entry<B: ObjectBackend>(b: &B, e: &RegistryEntry) -> Result<()> {
    let ok_slug = !e.slug.is_empty()
        && e.slug.len() <= 48
        && e.slug
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        && !e.slug.starts_with('-');
    if !ok_slug {
        return Err(S2Error::Invalid(format!(
            "registry slug {:?} is not a valid slug",
            e.slug
        )));
    }
    match read_meta(b, &e.bucket)? {
        Some(m) if m.slug == e.slug && bucket_project(&e.bucket).is_none_or(|p| p.eq_ignore_ascii_case(&m.project)) => Ok(()),
        Some(m) if m.slug == e.slug => Err(S2Error::Invalid(format!(
            "registry entry {} points at a bucket whose metadata names project {}",
            e.slug, m.project
        ))),
        Some(m) => Err(S2Error::Invalid(format!(
            "registry entry {} points at bucket of worktree {}",
            e.slug, m.slug
        ))),
        None => Err(S2Error::Invalid(format!(
            "bucket {} has no worktree metadata; refusing to follow {}",
            e.bucket, e.slug
        ))),
    }
}

/// The project ULID in a worktree bucket name `w-<ulid>-<h12>`, if it has that shape.
fn bucket_project(bucket: &str) -> Option<&str> {
    let rest = bucket.strip_prefix("w-")?;
    (rest.len() > 27 && rest.as_bytes()[26] == b'-').then(|| &rest[..26])
}

/// Daemon-side: drop a registry entry once its worktree bucket is promoted and deleted.
pub fn remove_registry_entry<B: ObjectBackend>(
    b: &B,
    project_bucket: &str,
    slug: &str,
) -> Result<()> {
    validate_key(slug)?;
    b.delete(project_bucket, &format!("{REGISTRY_PREFIX}{slug}.json"))?;
    let _ = crate::share_fetch::write_digest(b, project_bucket); // a hint
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocHit {
    pub slug: String,
    pub path: String,
    pub title: Option<String>,
    pub state: State,
}

#[derive(Debug, Default)]
pub struct SearchResult {
    pub hits: Vec<DocHit>,
    /// Worktrees whose manifest could not be read; the search is partial for these.
    pub unreadable: Vec<String>,
}

/// Search docs and planning (path or title, case-insensitive) across unmerged worktrees.
/// Merged and archived ones are skipped. Hits are ordered by slug, then path.
pub fn search_docs<B: ObjectBackend>(
    b: &B,
    entries: &[RegistryEntry],
    query: &str,
) -> SearchResult {
    let mut res = SearchResult::default();
    for e in entries
        .iter()
        .filter(|e| !matches!(e.state, State::Merged | State::Archived))
    {
        match verify_entry(b, e).and_then(|_| read_manifest(b, &e.bucket)) {
            Ok(Some(m)) => {
                for d in crate::share_fetch::find_docs(&m, query) {
                    res.hits.push(DocHit {
                        slug: e.slug.clone(),
                        path: d.path.clone(),
                        title: d.title.clone(),
                        state: e.state,
                    });
                }
            }
            _ => res.unreadable.push(e.slug.clone()),
        }
    }
    res.hits
        .sort_by(|a, c| a.slug.cmp(&c.slug).then_with(|| a.path.cmp(&c.path)));
    res
}

const INDEX: &str = ".wt-index.json";

/// Mirror one worktree's docs under `cache/<slug>/`, read-only, removing docs that left the tree.
/// The cache is the follower's own: local edits in it are overwritten.
pub fn sync_docs<B: ObjectBackend>(b: &B, e: &RegistryEntry, cache: &Path) -> Result<FetchReport> {
    verify_entry(b, e)?;
    // safe_join refuses a symlinked `cache/<slug>` as well as traversal.
    let dest = safe_join(cache, &e.slug)?;
    std::fs::create_dir_all(&dest)?;
    let m = read_manifest(b, &e.bucket)?
        .ok_or_else(|| S2Error::Invalid("worktree has no published tree".into()))?;
    let want: std::collections::BTreeSet<String> = m.docs.iter().map(|d| d.path.clone()).collect();
    let idx = safe_join(&dest, INDEX)?;
    let old: Vec<String> = std::fs::read(&idx)
        .ok()
        .and_then(|d| serde_json::from_slice(&d).ok())
        .unwrap_or_default();
    // Index first, as old + new, so a failed fetch below still leaves every file it may have
    // written (or that was there) tracked and prunable on the next run.
    let mut tracked: std::collections::BTreeSet<String> = old.iter().cloned().collect();
    tracked.extend(want.iter().cloned());
    std::fs::write(&idx, serde_json::to_vec(&tracked)?)?;
    for p in old.iter().filter(|p| !want.contains(p.as_str())) {
        if let Ok(f) = safe_join(&dest, p) {
            let _ = std::fs::remove_file(f);
        }
    }
    let rep = fetch_tree(b, &e.bucket, &dest, Select::DocsOnly, true)?;
    let keep: Vec<&String> = want.iter().collect();
    std::fs::write(&idx, serde_json::to_vec(&keep)?)?;
    #[cfg(unix)]
    for p in &rep.written {
        let f = safe_join(&dest, p)?;
        let mut perm = std::fs::metadata(&f)?.permissions();
        perm.set_readonly(true);
        std::fs::set_permissions(&f, perm)?;
    }
    Ok(rep)
}

#[derive(Debug, Clone, Copy)]
pub struct LeftoverPolicy {
    /// An `Open` worktree with no update this long is forgotten work.
    pub open_idle_ms: u64,
    /// A `Handoff` that nobody claimed this long needs a nudge. Also the limit for a `Merged` or
    /// `Archived` entry still in the registry (cleanup did not finish).
    pub unclaimed_ms: u64,
    /// A timestamp this far ahead of the follower's clock is reported as skew, not trusted.
    pub skew_ms: u64,
}
impl Default for LeftoverPolicy {
    fn default() -> Self {
        Self {
            open_idle_ms: 3 * 24 * 3600 * 1000,
            unclaimed_ms: 24 * 3600 * 1000,
            skew_ms: 3600 * 1000,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    ForgottenOpen,
    UnclaimedHandoff,
    AbandonedClaim,
    /// Merged or archived but never removed from the registry.
    StuckCleanup,
    /// `updated_ms` is in the future: the writer's clock is ahead, so idle time is unknown.
    FutureTimestamp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Leftover {
    pub entry: RegistryEntry,
    pub idle_ms: u64,
    pub why: Why,
}

/// Unmerged worktrees nobody is finishing, oldest first. `now_ms` and the registry's `updated_ms`
/// come from different machines' clocks, so thresholds should be generous (days, not minutes).
pub fn leftovers(entries: &[RegistryEntry], now_ms: u64, p: &LeftoverPolicy) -> Vec<Leftover> {
    let mut v: Vec<_> = entries
        .iter()
        .filter_map(|e| {
            let idle = now_ms.saturating_sub(e.updated_ms);
            if e.updated_ms > now_ms.saturating_add(p.skew_ms)
                && !matches!(e.state, State::Merged | State::Archived)
            {
                return Some(Leftover {
                    entry: e.clone(),
                    idle_ms: 0,
                    why: Why::FutureTimestamp,
                });
            }
            let why = match e.state {
                State::Merged | State::Archived if idle >= p.unclaimed_ms => Why::StuckCleanup,
                State::Open if idle >= p.open_idle_ms => Why::ForgottenOpen,
                State::Handoff if idle >= p.unclaimed_ms => Why::UnclaimedHandoff,
                State::Claimed if idle >= CLAIM_TTL_MS => Why::AbandonedClaim,
                _ => return None,
            };
            Some(Leftover {
                entry: e.clone(),
                idle_ms: idle,
                why,
            })
        })
        .collect();
    v.sort_by(|a, c| {
        c.idle_ms
            .cmp(&a.idle_ms)
            .then_with(|| a.entry.slug.cmp(&c.entry.slug))
    });
    v
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Overlap {
    pub path: String,
    pub in_a: String,
    pub in_b: String,
    pub kind: OverlapKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverlapKind {
    /// Both trees hold the file with different content.
    BothChanged,
    /// One worktree deleted the file; the other changed it. `deleted_in` is the deleting slug.
    ModifyDelete { deleted_in: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlapReport {
    /// Both trees branch from the same base commit. If `false`, a path can differ only because
    /// the base moved, so treat `overlaps` as a superset of the real conflicts.
    pub same_base: bool,
    /// The base commits of the two trees (`a` then `b`), so a caller can discount the report
    /// when they differ: on different bases a path can differ only because the base moved.
    pub base_a: String,
    pub base_b: String,
    /// Files in both trees with different content, and files one worktree deleted that the other
    /// changed (needs `changes` in both manifests; a rename counts as delete + add). Absence is not
    /// proof of no conflict: only paths, never hunks, are compared.
    pub overlaps: Vec<Overlap>,
}

/// Files that both unmerged worktrees contain with *different* content, before either is merged.
pub fn overlaps<B: ObjectBackend>(
    b: &B,
    a: &RegistryEntry,
    c: &RegistryEntry,
) -> Result<OverlapReport> {
    if a.bucket == c.bucket {
        return Err(S2Error::Invalid(
            "both entries point at the same bucket".into(),
        ));
    }
    verify_entry(b, a)?;
    verify_entry(b, c)?;
    let ma = read_manifest(b, &a.bucket)?
        .ok_or_else(|| S2Error::Invalid(format!("{} has no published tree", a.slug)))?;
    let mc = read_manifest(b, &c.bucket)?
        .ok_or_else(|| S2Error::Invalid(format!("{} has no published tree", c.slug)))?;
    let other: BTreeMap<&str, &str> = mc
        .files
        .iter()
        .map(|f| (f.path.as_str(), f.sha256.as_str()))
        .collect();
    let mut v: Vec<_> = ma
        .files
        .iter()
        .filter(|f| other.get(f.path.as_str()).is_some_and(|s| *s != f.sha256))
        .map(|f| Overlap {
            path: f.path.clone(),
            in_a: a.slug.clone(),
            in_b: c.slug.clone(),
            kind: OverlapKind::BothChanged,
        })
        .collect();
    for (del, del_slug, chg) in [(&ma, &a.slug, &mc), (&mc, &c.slug, &ma)] {
        for d in del.changes.iter().filter(|x| x.kind == ChangeKind::Deleted) {
            let touched = chg
                .changes
                .iter()
                .any(|x| x.path == d.path && x.kind != ChangeKind::Deleted);
            if touched {
                v.push(Overlap {
                    path: d.path.clone(),
                    in_a: a.slug.clone(),
                    in_b: c.slug.clone(),
                    kind: OverlapKind::ModifyDelete {
                        deleted_in: del_slug.clone(),
                    },
                });
            }
        }
    }
    v.sort_by(|x, y| x.path.cmp(&y.path));
    Ok(OverlapReport {
        same_base: ma.base_commit == mc.base_commit,
        base_a: ma.base_commit.clone(),
        base_b: mc.base_commit.clone(),
        overlaps: v,
    })
}
