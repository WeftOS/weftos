//! Retention for landed worktrees (ADR-126 S2): which worktree stores have been landed long enough
//! to retire, what a store costs, and which archived objects have outlived their keep period.
//!
//! Everything here only reads. The daemon's `gc` verb acts on the answers, and only when told to
//! (a dry run is the default), because what it removes is recovery data.

use crate::overlay::{ObjectBackend, Result};
use crate::promote::is_promoted;

pub const DAY_MS: u64 = 24 * 3600 * 1000;
/// A landed worktree store is kept this long before `gc` offers to retire it.
pub const DEFAULT_LANDED_KEEP_DAYS: u64 = 14;
/// Default size past which a worktree store refuses further publishes (2 GiB).
pub const DEFAULT_WORKTREE_QUOTA: u64 = 2 << 30;

/// `Some(promoted_ms)` if the worktree landed at or before `cutoff_ms`. Not landed, or landed after
/// the cutoff, is `None`: an unlanded worktree is never a candidate, however old (it may hold the
/// only copy of someone's work; `leftovers` reports those).
pub fn landed_before<B: ObjectBackend>(b: &B, worktree_bucket: &str, cutoff_ms: u64) -> Result<Option<u64>> {
    Ok(is_promoted(b, worktree_bucket)?.map(|r| r.promoted_ms).filter(|ms| *ms <= cutoff_ms))
}

/// Objects and bytes in a bucket.
pub fn usage<B: ObjectBackend>(b: &B, bucket: &str) -> Result<(u64, u64)> {
    let l = b.list(bucket, "")?;
    Ok((l.len() as u64, l.iter().map(|m| m.size).sum()))
}

/// The timestamp in an archive key `<bucket>@<ts_ms>/<rest>` and its bucket, if it has that shape.
pub fn archive_key_parts(key: &str) -> Option<(&str, u64)> {
    let (head, _) = key.split_once('/')?;
    let (bucket, ts) = head.rsplit_once('@')?;
    Some((bucket, ts.parse().ok()?))
}

/// Archive keys written for buckets starting with `bucket_prefix` (a project's worktrees:
/// `w-<project>-`) at or before `cutoff_ms`. Keys of any other shape or bucket are left alone.
pub fn expired_archive_keys<B: ObjectBackend>(
    b: &B,
    archive_bucket: &str,
    bucket_prefix: &str,
    cutoff_ms: u64,
) -> Result<Vec<String>> {
    Ok(b.list(archive_bucket, bucket_prefix)?
        .into_iter()
        .filter(|m| archive_key_parts(&m.key).is_some_and(|(bk, ts)| bk.starts_with(bucket_prefix) && ts <= cutoff_ms))
        .map(|m| m.key)
        .collect())
}
