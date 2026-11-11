//! The remote side: a validated listing of the prefix, delete markers, and fetch into the CAS.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use super::cas::{Cas, hash_of, valid_hash};
use super::{DriveError, Result, SkipWhy, Skipped, TOMBSTONE_DIR, check_rel, now_ms};
use crate::backend::{BackendError, ObjectBackend, ObjectMeta};
use crate::share::Limits;

pub(crate) struct Listing {
    /// Accepted objects by path relative to the prefix.
    pub objects: BTreeMap<String, ObjectMeta>,
    /// The store's modification stamp for each accepted object (empty when the backend has none).
    pub stamps: BTreeMap<String, String>,
    /// Every relative path the store has, accepted or not. A path missing here is deleted.
    pub seen: BTreeSet<String>,
    pub skipped: Vec<Skipped>,
}

/// The daemon's control area of a bucket: never part of a drive (writes there are denied by the
/// policy in `policy.rs`; reads would only mirror registry state into a working tree).
fn control_area(bucket: &str, rel: &str) -> bool {
    bucket.starts_with("p-") && rel.starts_with("worktrees/")
}

pub(crate) fn list<B: ObjectBackend + ?Sized>(b: &B, bucket: &str, prefix: &str, limits: &Limits) -> Result<Listing> {
    let mut metas = b.list_stamped(bucket, prefix)?;
    metas.sort_by(|x, y| x.0.key.cmp(&y.0.key));
    let mut out = Listing { objects: BTreeMap::new(), stamps: BTreeMap::new(), seen: BTreeSet::new(), skipped: Vec::new() };
    let mut lower: BTreeSet<String> = BTreeSet::new();
    let mut total = 0u64;
    for (m, stamp) in metas {
        let Some(rel) = m.key.strip_prefix(prefix).filter(|r| !r.is_empty()) else { continue };
        if rel.split('/').next() == Some(TOMBSTONE_DIR) {
            continue;
        }
        out.seen.insert(rel.to_owned());
        if control_area(bucket, rel) {
            out.skipped.push(Skipped::new(rel, SkipWhy::Reserved));
            continue;
        }
        if let Err(why) = check_rel(rel) {
            out.skipped.push(Skipped::new(rel, why));
            continue;
        }
        if m.size > limits.max_file {
            out.skipped.push(Skipped::new(rel, SkipWhy::TooLarge));
            continue;
        }
        // APFS and NTFS are case-insensitive by default: two keys that differ only by case would
        // be one file. Keep the first, report the rest.
        if !lower.insert(rel.to_lowercase()) {
            out.skipped.push(Skipped::new(rel, SkipWhy::CaseCollision));
            continue;
        }
        if out.objects.len() >= limits.max_files || total + m.size > limits.max_total {
            lower.remove(&rel.to_lowercase());
            out.skipped.push(Skipped::new(rel, SkipWhy::OverBudget));
            continue;
        }
        total += m.size;
        out.stamps.insert(rel.to_owned(), stamp);
        out.objects.insert(rel.to_owned(), m);
    }
    Ok(out)
}

/// Whether the object is executable (its attribute); a key that is gone reads as not executable.
/// One HEAD: callers use it only where the answer is needed (placing, a conflict, a stamp that moved).
pub(crate) fn attr_exec<B: ObjectBackend + ?Sized>(b: &B, bucket: &str, key: &str) -> Result<bool> {
    match b.attrs(bucket, key) {
        Ok(a) => Ok(a.executable),
        Err(BackendError::NoObject(_)) => Ok(false),
        Err(e) => Err(e.into()),
    }
}

pub(crate) fn tombstone_key(prefix: &str, rel: &str) -> String {
    format!("{prefix}{TOMBSTONE_DIR}/{rel}")
}

pub(crate) fn write_tombstone<B: ObjectBackend + ?Sized>(
    b: &B,
    bucket: &str,
    prefix: &str,
    rel: &str,
    device: &str,
    deleted_etag: &str,
) -> Result<()> {
    let ts = now_ms();
    let body = serde_json::json!({"device": device, "ts_ms": ts, "etag": deleted_etag}).to_string();
    let key = tombstone_key(prefix, rel);
    // Create-only first: a tombstone that is already there is never overwritten blind.
    match b.put(bucket, &key, body.as_bytes(), true) {
        Ok(_) => return Ok(()),
        Err(BackendError::Exists(_)) => {}
        Err(e) => return Err(e.into()),
    }
    // One is there. A newer one (this writer is stale, e.g. a skewed clock) or one for the same
    // deleted version stays. An older one is replaced only if still the version we looked at.
    let (Ok(cur), Some(old)) = (b.head(bucket, &key), read_tombstone(b, bucket, prefix, rel)) else { return Ok(()) };
    let newer = old["ts_ms"].as_u64().is_some_and(|t| t > ts);
    if newer || old["etag"].as_str() == Some(deleted_etag) {
        return Ok(());
    }
    match b.put_if_match(bucket, &key, body.as_bytes(), &cur.etag) {
        Ok(_) | Err(BackendError::Stale(_) | BackendError::NoObject(_) | BackendError::Unsupported(_)) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Revive: the key is written again, so its marker goes.
pub(crate) fn clear_tombstone<B: ObjectBackend + ?Sized>(b: &B, bucket: &str, prefix: &str, rel: &str) -> Result<()> {
    match b.delete(bucket, &tombstone_key(prefix, rel)) {
        Ok(()) | Err(BackendError::NoObject(_)) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

pub(crate) fn read_tombstone<B: ObjectBackend + ?Sized>(b: &B, bucket: &str, prefix: &str, rel: &str) -> Option<serde_json::Value> {
    let d = b.get(bucket, &tombstone_key(prefix, rel)).ok()?;
    serde_json::from_slice(&d).ok()
}

/// Make the object's bytes present in the CAS and return their hash. Returns the number of bytes
/// downloaded (0 when the CAS already had them). `known` maps an etag to a hash this device has
/// already seen for it, so identical objects are fetched once.
pub(crate) fn fetch_to_cas<B: ObjectBackend + ?Sized>(
    b: &B,
    bucket: &str,
    key: &str,
    meta: &ObjectMeta,
    cas: &Cas,
    known: &HashMap<String, String>,
) -> Result<(String, u64)> {
    if let Some(h) = known.get(&meta.etag).filter(|h| cas.has(h)) {
        return Ok((h.clone(), 0));
    }
    let data = b.get(bucket, key)?;
    if data.len() as u64 != meta.size {
        return Err(DriveError::Io(format!("{key}: listing said {} bytes, got {}", meta.size, data.len())));
    }
    let hash = hash_of(&data);
    // The built-in backends use the BLAKE3 as the etag, so a mismatch is corruption in transit or
    // at rest. Other stores use their own etags, which cannot be checked here.
    if valid_hash(&meta.etag) && meta.etag != hash {
        return Err(DriveError::Invalid(format!("{key}: etag {} does not match content {hash}", meta.etag)));
    }
    cas.put(&hash, &data)?;
    Ok((hash, data.len() as u64))
}
