//! Push local changes to the prefix.
//!
//! Create: a create-only put (`if_none_match`), so a concurrent remote create is never overwritten.
//! Modify: the remote etag must still be the one this file last synced against, else conflict.
//! Delete: the remote object goes and a delete marker is written (ADR-126 section 4 style), unless
//! the remote changed since, which is a conflict.
//!
//! The Modify and Delete checks are head-then-write, not atomic: the store traits have no
//! conditional put on an etag yet (see the S3 notes). A writer that lands inside that window wins
//! silently; the create path has no such window.

use std::path::Path;

use super::cas::hash_of;
use super::conflict::{ConflictKind, record};
use super::index::{Entry, Index};
use super::remote::{clear_tombstone, read_tombstone, write_tombstone};
use super::scan::{Change, ChangeKind, scan};
use super::{DriveError, Options, Result, SkipWhy, Skipped, log_event};
use crate::backend::{BackendError, ObjectBackend};
use crate::share::{Skip, looks_like_private_key, read_regular};

#[derive(Debug, Default, Clone)]
pub struct UpReport {
    pub put: Vec<String>,
    /// Remote objects deleted for local deletes.
    pub deleted: Vec<String>,
    /// Conflicts recorded by this run.
    pub conflicts: Vec<String>,
    /// Paths already in conflict, left alone.
    pub open_conflicts: usize,
    pub bytes_put: u64,
    pub skipped: Vec<Skipped>,
}

pub fn sync_up<B: ObjectBackend + ?Sized>(backend: &B, dest: &Path) -> Result<UpReport> {
    sync_up_with(backend, dest, &Options::default())
}

pub fn sync_up_with<B: ObjectBackend + ?Sized>(backend: &B, dest: &Path, opts: &Options) -> Result<UpReport> {
    let mut index = Index::load(dest)?.ok_or_else(|| DriveError::Invalid(format!("{} is not a drive", dest.display())))?;
    let mut rep = UpReport::default();
    let r = up(backend, dest, &mut index, opts, &mut rep);
    let saved = index.save(dest);
    r?;
    saved?;
    Ok(rep)
}

/// The bytes to upload, or a recorded reason not to.
fn read_for_upload(dest: &Path, ch: &Change, max: u64, rep: &mut UpReport) -> Option<Vec<u8>> {
    let why = match read_regular(&dest.join(&ch.path), max) {
        Ok(d) if looks_like_private_key(&d) => SkipWhy::Secret,
        Ok(d) => return Some(d),
        Err(Some(Skip::TooLarge)) => SkipWhy::TooLarge,
        Err(Some(Skip::Symlink)) => SkipWhy::Symlink,
        Err(Some(_)) => SkipWhy::Changed,
        Err(None) => SkipWhy::Io,
    };
    rep.skipped.push(Skipped::new(&ch.path, why));
    None
}

fn up<B: ObjectBackend + ?Sized>(b: &B, dest: &Path, index: &mut Index, opts: &Options, rep: &mut UpReport) -> Result<()> {
    let s = scan(dest, index, &opts.limits)?;
    for (p, size, mt) in &s.touched {
        index.touch(p, *size, *mt);
    }
    rep.skipped.extend(s.skipped);
    rep.open_conflicts = index.conflicts.len();
    let (bucket, prefix, device) = (index.bucket.clone(), index.prefix.clone(), index.device.clone());
    for ch in s.changes {
        let key = index.key(&ch.path);
        if ch.kind != ChangeKind::Delete && rep.put.len() >= opts.limits.max_files {
            rep.skipped.push(Skipped::new(&ch.path, SkipWhy::OverBudget));
            continue;
        }
        match ch.kind {
            ChangeKind::Create => {
                let Some(data) = read_for_upload(dest, &ch, opts.limits.max_file, rep) else { continue };
                let hash = hash_of(&data);
                match b.put(&bucket, &key, &data, true) {
                    Ok(m) => {
                        uploaded(dest, index, &ch, &data, hash, m.etag, rep);
                        clear_tombstone(b, &bucket, &prefix, &ch.path)?;
                    }
                    Err(BackendError::Exists(_)) => {
                        // Someone created it first. Same bytes: adopt. Different: conflict.
                        let m = b.head(&bucket, &key)?;
                        let rb = b.get(&bucket, &key)?;
                        if hash_of(&rb) == hash {
                            index.entries.insert(ch.path.clone(), Entry { size: data.len() as u64, mtime_ns: ch.mtime_ns, hash, etag: m.etag });
                        } else {
                            record(dest, index, &ch.path, ConflictKind::BothCreated, Some((&m.etag, &rb)), Some(&hash), "")?;
                            rep.conflicts.push(ch.path.clone());
                        }
                    }
                    Err(e) => return Err(e.into()),
                }
            }
            ChangeKind::Modify => {
                let Some(entry) = index.entries.get(&ch.path).cloned() else { continue };
                let Some(data) = read_for_upload(dest, &ch, opts.limits.max_file, rep) else { continue };
                let hash = hash_of(&data);
                match b.head(&bucket, &key) {
                    Err(BackendError::NoObject(_)) => {
                        let who = read_tombstone(b, &bucket, &prefix, &ch.path)
                            .and_then(|t| t["device"].as_str().map(|d| format!("remote deleted by {d}")))
                            .unwrap_or_default();
                        record(dest, index, &ch.path, ConflictKind::RemoteDeletedLocalModified, None, Some(&hash), &who)?;
                        rep.conflicts.push(ch.path.clone());
                    }
                    Err(e) => return Err(e.into()),
                    Ok(m) if m.etag != entry.etag => {
                        let rb = b.get(&bucket, &key)?;
                        if hash_of(&rb) == hash {
                            index.entries.insert(ch.path.clone(), Entry { size: data.len() as u64, mtime_ns: ch.mtime_ns, hash, etag: m.etag });
                        } else {
                            record(dest, index, &ch.path, ConflictKind::BothModified, Some((&m.etag, &rb)), Some(&hash), "")?;
                            rep.conflicts.push(ch.path.clone());
                        }
                    }
                    Ok(_) => {
                        let m = b.put(&bucket, &key, &data, false)?;
                        uploaded(dest, index, &ch, &data, hash, m.etag, rep);
                    }
                }
            }
            ChangeKind::Delete => {
                let Some(entry) = index.entries.get(&ch.path).cloned() else { continue };
                match b.head(&bucket, &key) {
                    Err(BackendError::NoObject(_)) => {
                        index.entries.remove(&ch.path);
                        log_event(dest, "forget", &ch.path);
                    }
                    Err(e) => return Err(e.into()),
                    Ok(m) if m.etag != entry.etag => {
                        let rb = b.get(&bucket, &key)?;
                        record(dest, index, &ch.path, ConflictKind::RemoteModifiedLocalDeleted, Some((&m.etag, &rb)), None, "")?;
                        rep.conflicts.push(ch.path.clone());
                    }
                    Ok(_) => {
                        match b.delete(&bucket, &key) {
                            Ok(()) | Err(BackendError::NoObject(_)) => {}
                            Err(e) => return Err(e.into()),
                        }
                        write_tombstone(b, &bucket, &prefix, &ch.path, &device, &entry.etag)?;
                        index.entries.remove(&ch.path);
                        rep.deleted.push(ch.path.clone());
                        log_event(dest, "delete", &ch.path);
                    }
                }
            }
        }
    }
    Ok(())
}

fn uploaded(dest: &Path, index: &mut Index, ch: &Change, data: &[u8], hash: String, etag: String, rep: &mut UpReport) {
    index.entries.insert(ch.path.clone(), Entry { size: data.len() as u64, mtime_ns: ch.mtime_ns, hash, etag });
    rep.put.push(ch.path.clone());
    rep.bytes_put += data.len() as u64;
    log_event(dest, "put", &ch.path);
}
