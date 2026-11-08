//! Materialise a prefix into a local tree, and refresh it (`sync_down`).
//!
//! First fill and refresh are the same algorithm: list the prefix, fetch only objects whose etag
//! or size differs from the index, place them from the CAS, and apply remote deletes. A remote
//! change over a locally modified file is a conflict, not an overwrite.

use std::collections::HashMap;
use std::path::Path;

use super::cas::{Cas, prune_empty_parents};
use super::conflict::{ConflictKind, record};
use super::index::{Entry, Index, Local, local_state, mtime_ns};
use super::remote::{self, fetch_to_cas, read_tombstone};
use super::{DRIVE_DIR, DriveError, Options, Result, Skipped, TMP_PREFIX, log_event, norm_prefix};
use crate::backend::{ObjectBackend, ObjectMeta};

#[derive(Debug, Default, Clone)]
pub struct DownReport {
    pub created: Vec<String>,
    pub updated: Vec<String>,
    /// Remote deletes applied to unmodified local files.
    pub removed: Vec<String>,
    /// Conflicts recorded by this run.
    pub conflicts: Vec<String>,
    /// Paths already in conflict from an earlier run, left alone.
    pub open_conflicts: usize,
    pub unchanged: usize,
    /// Objects downloaded (GETs).
    pub fetched: usize,
    pub bytes_fetched: u64,
    pub skipped: Vec<Skipped>,
}

/// Bind `dest` to `(bucket, prefix)` and bring it up to date. Safe to run again: the second run is
/// a refresh that fetches only what changed.
pub fn materialise<B: ObjectBackend + ?Sized>(backend: &B, bucket: &str, prefix: &str, dest: &Path) -> Result<DownReport> {
    materialise_with(backend, bucket, prefix, dest, &Options::default())
}

pub fn materialise_with<B: ObjectBackend + ?Sized>(
    backend: &B,
    bucket: &str,
    prefix: &str,
    dest: &Path,
    opts: &Options,
) -> Result<DownReport> {
    if bucket.is_empty() {
        return Err(DriveError::Invalid("empty bucket".into()));
    }
    let prefix = norm_prefix(prefix);
    std::fs::create_dir_all(dest.join(DRIVE_DIR))?;
    let mut index = match Index::load(dest)? {
        Some(ix) if ix.bucket == bucket && ix.prefix == prefix => ix,
        Some(ix) => {
            return Err(DriveError::Invalid(format!(
                "{} is already bound to {}/{}, not {bucket}/{prefix}",
                dest.display(),
                ix.bucket,
                ix.prefix
            )));
        }
        None => Index::new(bucket, &prefix, opts.device.as_deref()),
    };
    run_down(backend, dest, &mut index, opts)
}

/// Apply remote changes to an existing drive.
pub fn sync_down<B: ObjectBackend + ?Sized>(backend: &B, dest: &Path) -> Result<DownReport> {
    sync_down_with(backend, dest, &Options::default())
}

pub fn sync_down_with<B: ObjectBackend + ?Sized>(backend: &B, dest: &Path, opts: &Options) -> Result<DownReport> {
    let mut index = Index::load(dest)?.ok_or_else(|| DriveError::Invalid(format!("{} is not a drive", dest.display())))?;
    run_down(backend, dest, &mut index, opts)
}

fn run_down<B: ObjectBackend + ?Sized>(backend: &B, dest: &Path, index: &mut Index, opts: &Options) -> Result<DownReport> {
    let mut rep = DownReport::default();
    let r = apply_remote(backend, dest, index, opts, &mut rep);
    // The index is saved even when a fetch failed halfway: what was placed is recorded.
    let saved = index.save(dest);
    r?;
    saved?;
    Ok(rep)
}

/// Place a blob at `rel` through a temp file in the same directory.
fn place_file(dest: &Path, cas: &Cas, rel: &str, hash: &str) -> Result<(u64, u64)> {
    let target = crate::share::safe_join(dest, rel).map_err(|e| DriveError::Invalid(e.to_string()))?;
    let dir = target.parent().ok_or_else(|| DriveError::Io(format!("{rel}: no parent")))?;
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!("{TMP_PREFIX}{}", ulid::Ulid::new()));
    let res = cas.clone_to(hash, &tmp).and_then(|()| std::fs::rename(&tmp, &target).map_err(Into::into));
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res?;
    let md = std::fs::metadata(&target)?;
    Ok((md.len(), mtime_ns(&md)))
}

fn apply_remote<B: ObjectBackend + ?Sized>(
    b: &B,
    dest: &Path,
    index: &mut Index,
    opts: &Options,
    rep: &mut DownReport,
) -> Result<()> {
    let max = opts.limits.max_file;
    let bucket = index.bucket.clone();
    let listing = remote::list(b, &bucket, &index.prefix, &opts.limits)?;
    rep.skipped.extend(listing.skipped);
    let cas = Cas::new(dest);
    let mut known: HashMap<String, String> =
        index.entries.values().filter(|e| !e.hash.is_empty()).map(|e| (e.etag.clone(), e.hash.clone())).collect();

    for (rel, meta) in &listing.objects {
        if index.conflicts.contains_key(rel) {
            rep.open_conflicts += 1;
            continue;
        }
        let entry = index.entries.get(rel).cloned();
        if entry.as_ref().is_some_and(|e| e.etag == meta.etag && e.size == meta.size) {
            rep.unchanged += 1;
            continue;
        }
        let local = match local_state(dest, rel, entry.as_ref(), max) {
            Ok(l) => l,
            Err(e) => {
                rep.skipped.push(Skipped::io(rel, e));
                continue;
            }
        };
        let key = index.key(rel);
        let mut fetch = |rep: &mut DownReport| -> Result<String> {
            let (hash, n) = fetch_to_cas(b, &bucket, &key, meta, &cas, &known)?;
            if n > 0 {
                rep.fetched += 1;
                rep.bytes_fetched += n;
            }
            known.insert(meta.etag.clone(), hash.clone());
            Ok(hash)
        };
        match (entry.is_some(), local) {
            (_, Local::NotFile) => rep.skipped.push(Skipped::io(rel, "a directory, symlink or oversized file is in the way")),
            (true, Local::Unchanged) | (false, Local::Missing) => {
                let hash = fetch(rep)?;
                place(dest, &cas, index, rel, &hash, meta, entry.is_none(), rep);
            }
            (true, Local::Missing) => {
                let hash = fetch(rep)?;
                let bytes = cas.read(&hash)?;
                record(dest, index, rel, ConflictKind::RemoteModifiedLocalDeleted, Some((&meta.etag, &bytes)), None, "")?;
                rep.conflicts.push(rel.clone());
            }
            (true, Local::Modified { hash: lh }) => {
                let hash = fetch(rep)?;
                if hash == lh {
                    // Both sides hold the same bytes: a pull that placed this file and died before
                    // saving the index, or the same edit made twice. Adopt, as the create case does.
                    let md = std::fs::metadata(dest.join(rel))?;
                    index.entries.insert(
                        rel.clone(),
                        Entry { size: md.len(), mtime_ns: mtime_ns(&md), hash, etag: meta.etag.clone() },
                    );
                    rep.unchanged += 1;
                    continue;
                }
                let bytes = cas.read(&hash)?;
                record(dest, index, rel, ConflictKind::BothModified, Some((&meta.etag, &bytes)), Some(&lh), "")?;
                rep.conflicts.push(rel.clone());
            }
            (false, Local::Untracked { hash: lh }) => {
                let hash = fetch(rep)?;
                if hash == lh {
                    // Same bytes on both sides: adopt the file, no conflict.
                    let md = std::fs::metadata(dest.join(rel))?;
                    index.entries.insert(
                        rel.clone(),
                        Entry { size: md.len(), mtime_ns: mtime_ns(&md), hash, etag: meta.etag.clone() },
                    );
                } else {
                    let bytes = cas.read(&hash)?;
                    record(dest, index, rel, ConflictKind::BothCreated, Some((&meta.etag, &bytes)), Some(&lh), "")?;
                    rep.conflicts.push(rel.clone());
                }
            }
            _ => {}
        }
    }

    // Remote deletes: in the index, gone from the store.
    let gone: Vec<String> = index.entries.keys().filter(|k| !listing.seen.contains(*k)).cloned().collect();
    for rel in gone {
        if index.conflicts.contains_key(&rel) {
            rep.open_conflicts += 1;
            continue;
        }
        let entry = index.entries[&rel].clone();
        match local_state(dest, &rel, Some(&entry), max) {
            Ok(Local::Missing) => {
                // Deleted on both sides.
                index.entries.remove(&rel);
                log_event(dest, "forget", &rel);
            }
            Ok(Local::Unchanged) => match std::fs::remove_file(dest.join(&rel)) {
                Ok(()) => {
                    prune_empty_parents(dest, &rel);
                    index.entries.remove(&rel);
                    rep.removed.push(rel.clone());
                    log_event(dest, "remove", &rel);
                }
                Err(e) => rep.skipped.push(Skipped::io(&rel, e)),
            },
            Ok(Local::Modified { hash }) => {
                let who = read_tombstone(b, &bucket, &index.prefix, &rel)
                    .and_then(|t| t["device"].as_str().map(|d| format!("remote deleted by {d}")))
                    .unwrap_or_default();
                record(dest, index, &rel, ConflictKind::RemoteDeletedLocalModified, None, Some(&hash), &who)?;
                rep.conflicts.push(rel);
            }
            Ok(Local::NotFile | Local::Untracked { .. }) => {}
            Err(e) => rep.skipped.push(Skipped::io(&rel, e)),
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn place(dest: &Path, cas: &Cas, index: &mut Index, rel: &str, hash: &str, meta: &ObjectMeta, created: bool, rep: &mut DownReport) {
    match place_file(dest, cas, rel, hash) {
        Ok((size, mt)) => {
            index.entries.insert(rel.to_owned(), Entry { size, mtime_ns: mt, hash: hash.to_owned(), etag: meta.etag.clone() });
            if created {
                rep.created.push(rel.to_owned());
                log_event(dest, "place", rel);
            } else {
                rep.updated.push(rel.to_owned());
                log_event(dest, "update", rel);
            }
        }
        Err(e) => rep.skipped.push(Skipped::io(rel, e)),
    }
}
