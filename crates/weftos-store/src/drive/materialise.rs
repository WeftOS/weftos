//! Materialise a prefix into a local tree, and refresh it (`sync_down`).
//!
//! First fill and refresh are the same algorithm: list the prefix, fetch only objects whose etag
//! or size differs from the index, place them from the CAS, and apply remote deletes. A remote
//! change over a locally modified file is a conflict, not an overwrite.

use std::collections::HashMap;
use std::path::Path;

use super::cas::{Cas, prune_empty_parents};
use super::conflict::{ConflictKind, record};
use super::index::{Entry, Index, Local, is_exec, local_state, mtime_ns};
use super::remote::{self, attr_exec, fetch_to_cas, read_tombstone};
use super::tmp::{Live, remove_stale};
use super::{DRIVE_DIR, DriveError, Options, Result, Skipped, TMP_PREFIX, log_event, norm_prefix};
use crate::backend::{BackendError, ObjectBackend, ObjectMeta};

#[derive(Debug, Default, Clone)]
pub struct DownReport {
    pub created: Vec<String>,
    pub updated: Vec<String>,
    /// The subset of `updated` where only the execute attribute changed on the store (same bytes);
    /// the local file's mode was changed in place.
    pub mode_updated: Vec<String>,
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
    /// Stale `.drive.tmp.*` files (older than `Options::stale_tmp_age`) removed at open.
    pub stale_tmp_removed: usize,
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
    let mut rep = DownReport { stale_tmp_removed: remove_stale(dest, opts.stale_tmp_age), ..Default::default() };
    let r = apply_remote(backend, dest, index, opts, &mut rep);
    // The index is saved even when a fetch failed halfway: what was placed is recorded.
    let saved = index.save(dest);
    r?;
    saved?;
    Ok(rep)
}

/// Place a blob at `rel` through a temp file in the same directory. `exec` makes the file 0755
/// (the CAS blob is shared by every path with these bytes, so the mode is set on the clone).
fn place_file(dest: &Path, cas: &Cas, rel: &str, hash: &str, exec: bool) -> Result<(u64, u64)> {
    let target = crate::share::safe_join(dest, rel).map_err(|e| DriveError::Invalid(e.to_string()))?;
    let dir = target.parent().ok_or_else(|| DriveError::Io(format!("{rel}: no parent")))?;
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!("{TMP_PREFIX}{}", ulid::Ulid::new()));
    let _live = Live::new(&tmp);
    let res = cas.clone_to(hash, &tmp).and_then(|()| {
        if exec {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
        }
        std::fs::rename(&tmp, &target).map_err(Into::into)
    });
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
        let stamp = listing.stamps.get(rel).map_or("", String::as_str);
        if let Some(e) = entry.as_ref().filter(|e| e.etag == meta.etag && e.size == meta.size) {
            // Same bytes. A mode-only change on the store (a put of the same bytes with another
            // execute attribute) keeps the etag and size, and shows only in the listing's stamp:
            // ask for the object's attributes only where the stamp moved (or is not known yet),
            // never one HEAD per object on every refresh.
            if stamp.is_empty() || e.stamp == stamp {
                rep.unchanged += 1;
            } else {
                mode_check(b, dest, index, rel, stamp, max, rep)?;
            }
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
                // One HEAD for the attributes, only for an object that is placed (not for the
                // unchanged ones; a mode-only change on the store is found by `mode_check`).
                let exec = attr_exec(b, &bucket, &key)?;
                place(dest, &cas, index, rel, &hash, meta, stamp, entry.is_none(), exec, rep);
            }
            (true, Local::Missing) => {
                let hash = fetch(rep)?;
                let bytes = cas.read(&hash)?;
                let rexec = attr_exec(b, &bucket, &key)?;
                record(dest, index, rel, ConflictKind::RemoteModifiedLocalDeleted, Some((&meta.etag, &bytes, rexec)), None, "")?;
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
                        Entry { size: md.len(), mtime_ns: mtime_ns(&md), hash, etag: meta.etag.clone(), exec: is_exec(&md), stamp: String::new() },
                    );
                    rep.unchanged += 1;
                    continue;
                }
                let bytes = cas.read(&hash)?;
                let rexec = attr_exec(b, &bucket, &key)?;
                record(dest, index, rel, ConflictKind::BothModified, Some((&meta.etag, &bytes, rexec)), Some(&lh), "")?;
                rep.conflicts.push(rel.clone());
            }
            (false, Local::Untracked { hash: lh }) => {
                let hash = fetch(rep)?;
                if hash == lh {
                    // Same bytes on both sides: adopt the file, no conflict.
                    let md = std::fs::metadata(dest.join(rel))?;
                    index.entries.insert(
                        rel.clone(),
                        Entry { size: md.len(), mtime_ns: mtime_ns(&md), hash, etag: meta.etag.clone(), exec: is_exec(&md), stamp: String::new() },
                    );
                } else {
                    let bytes = cas.read(&hash)?;
                    let rexec = attr_exec(b, &bucket, &key)?;
                    record(dest, index, rel, ConflictKind::BothCreated, Some((&meta.etag, &bytes, rexec)), Some(&lh), "")?;
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
fn place(dest: &Path, cas: &Cas, index: &mut Index, rel: &str, hash: &str, meta: &ObjectMeta, stamp: &str, created: bool, exec: bool, rep: &mut DownReport) {
    match place_file(dest, cas, rel, hash, exec) {
        Ok((size, mt)) => {
            index.entries.insert(rel.to_owned(), Entry { size, mtime_ns: mt, hash: hash.to_owned(), etag: meta.etag.clone(), exec, stamp: stamp.to_owned() });
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

/// The listing says this object has the bytes the index holds but a different modification stamp:
/// read its attributes (one HEAD) and, if the execute attribute changed, apply it to the file in
/// place. A file the user has edited, deleted or chmod-ed since the last sync is left alone (the
/// stamp is still recorded, so there is no HEAD on the next refresh); a local mode change then
/// goes out with the next `sync_up`, last writer wins.
fn mode_check<B: ObjectBackend + ?Sized>(
    b: &B,
    dest: &Path,
    index: &mut Index,
    rel: &str,
    stamp: &str,
    max: u64,
    rep: &mut DownReport,
) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let key = index.key(rel);
    let remote_exec = match b.attrs(&index.bucket, &key) {
        Ok(a) => a.executable,
        // Gone between the listing and now: the next refresh sees the delete.
        Err(BackendError::NoObject(_)) => {
            rep.unchanged += 1;
            return Ok(());
        }
        Err(e) => return Err(e.into()),
    };
    let Some(e) = index.entries.get(rel).cloned() else { return Ok(()) };
    let mut next = Entry { stamp: stamp.to_owned(), ..e.clone() };
    if remote_exec == e.exec {
        index.entries.insert(rel.to_owned(), next);
        rep.unchanged += 1;
        return Ok(());
    }
    let path = crate::share::safe_join(dest, rel).map_err(|x| DriveError::Invalid(x.to_string()))?;
    match (local_state(dest, rel, Some(&e), max), std::fs::symlink_metadata(&path)) {
        (Ok(Local::Unchanged), Ok(md)) if md.is_file() => {
            let mode = md.permissions().mode();
            if is_exec(&md) == e.exec {
                // Untouched locally: apply the store's attribute in place (same inode, so a
                // hard-linked view base follows).
                let new_mode = if remote_exec { 0o755 } else { mode & !0o111 };
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(new_mode))?;
                next.exec = remote_exec;
                index.entries.insert(rel.to_owned(), next);
                rep.updated.push(rel.to_owned());
                rep.mode_updated.push(rel.to_owned());
                log_event(dest, "mode", rel);
                return Ok(());
            }
            // The local mode was changed too: the same value is adopted, a different one stays
            // pending and is pushed by the next sync_up.
            if is_exec(&md) == remote_exec {
                next.exec = remote_exec;
            }
            index.entries.insert(rel.to_owned(), next);
            rep.unchanged += 1;
        }
        (Ok(Local::NotFile), _) | (Err(_), _) => {
            rep.skipped.push(Skipped::io(rel, "cannot apply the store's execute attribute: not a regular file here"));
        }
        // Edited or deleted locally: leave it; the local change is pushed (or conflicts) later.
        _ => {
            index.entries.insert(rel.to_owned(), next);
            rep.unchanged += 1;
        }
    }
    Ok(())
}
