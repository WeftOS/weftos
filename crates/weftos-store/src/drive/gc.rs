//! Tombstone collection (prototype, default off): drop delete markers older than a retention window.
//!
//! A marker at `<prefix>.drive-tombstones/<path>` names the deleter and records when. Nothing in
//! the sync rules needs it to *detect* a delete (a path missing from the listing is the signal);
//! it only supplies the deleter's name in a conflict record and the newer-marker rule of
//! `write_tombstone`. Collecting an old one therefore costs a name, nothing else. See
//! `docs/research/adr126-s3-tombstone-collection-2026-11-08.md`.
//!
//! The age is read from the marker body (`ts_ms`), because [`ObjectMeta`] carries no mtime. A body
//! without a usable `ts_ms` is never collected. A marker is deleted with `delete_if_match` on the
//! etag of the listing, so one replaced since the listing survives; a backend with no conditional
//! delete collects nothing (never a blind delete).

use std::time::Duration;

use super::{Options, Result, TOMBSTONE_DIR, norm_prefix};
use crate::backend::{BackendError, ObjectBackend};

/// What one collection pass did. Counts are exact; `collected` names the markers removed.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct GcReport {
    /// Markers the store listed under the tombstone prefix.
    pub listed: usize,
    /// Markers whose body was read (at most `Options::tombstone_gc_cap`).
    pub examined: usize,
    /// Younger than the retention window.
    pub kept_young: usize,
    /// Body unreadable or without `ts_ms`: kept, never guessed at.
    pub undated: usize,
    /// Replaced since the listing (`If-Match` refused): kept.
    pub raced: usize,
    /// Already gone when the delete arrived.
    pub vanished: usize,
    /// The backend has no conditional delete: kept.
    pub unsupported: usize,
    /// Paths (relative to the drive prefix) whose markers were deleted.
    pub collected: Vec<String>,
    /// More markers were listed than the cap allowed to be examined.
    pub capped: bool,
}

/// Default retention. A PLACEHOLDER: the owner chooses the real number (see the design note).
pub const PLACEHOLDER_RETENTION: Duration = Duration::from_secs(7 * 24 * 3600);
/// Default cap on markers examined (one `get` each) per pass.
pub const DEFAULT_GC_CAP: usize = 500;

/// One pass over `<prefix>.drive-tombstones/`. Idempotent. Does nothing, and lists nothing, unless
/// `opts.collect_tombstones` is set. `now_ms` is the caller's clock, so tests can place it.
pub fn collect_tombstones<B: ObjectBackend + ?Sized>(
    b: &B,
    bucket: &str,
    prefix: &str,
    opts: &Options,
    now_ms: u64,
) -> Result<GcReport> {
    let mut rep = GcReport::default();
    if !opts.collect_tombstones {
        return Ok(rep);
    }
    let prefix = norm_prefix(prefix);
    let dir = format!("{prefix}{TOMBSTONE_DIR}/");
    let cutoff = now_ms.saturating_sub(opts.tombstone_retention.as_millis() as u64);
    let mut metas = b.list(bucket, &dir)?;
    metas.sort_by(|x, y| x.key.cmp(&y.key));
    for m in metas {
        let Some(rel) = m.key.strip_prefix(&dir).filter(|r| !r.is_empty()) else { continue };
        rep.listed += 1;
        if rep.examined >= opts.tombstone_gc_cap {
            rep.capped = true;
            continue;
        }
        rep.examined += 1;
        let ts = b
            .get(bucket, &m.key)
            .ok()
            .and_then(|d| serde_json::from_slice::<serde_json::Value>(&d).ok())
            .and_then(|v| v["ts_ms"].as_u64());
        let Some(ts) = ts else {
            rep.undated += 1;
            continue;
        };
        if ts > cutoff {
            rep.kept_young += 1;
            continue;
        }
        match b.delete_if_match(bucket, &m.key, &m.etag) {
            Ok(()) => rep.collected.push(rel.to_owned()),
            Err(BackendError::PreconditionFailed(_)) => rep.raced += 1,
            Err(BackendError::NoObject(_)) => rep.vanished += 1,
            Err(BackendError::Unsupported) => rep.unsupported += 1,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(rep)
}
