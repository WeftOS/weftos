//! Orphaned worktree stores (WO-069 lane E, prototype on `w2/wo069-e-orphan-gc`; design note section 7).
//!
//! A worktree removed without landing (`git worktree remove`, the normal end of a research lane)
//! leaves its store, bundle and registry entry behind. The sweep notices the local path is gone and
//! tells the daemon when it first went missing. This module is the policy and the two daemon-side
//! actions, both generic over [`ObjectBackend`] and both driven by a caller-supplied clock:
//!
//! - [`mark_orphan`]: after `orphan_after_ms` of a missing path, the registry entry becomes state
//!   `orphan`. Only an `open`, unlanded store may become one.
//! - [`retire_orphan`]: after `archive_after_ms` as an orphan, every object of the worktree's bucket
//!   is copied to the archive, verified, deleted, the bucket dropped, and the registry entry removed.
//!   Nothing outside that worktree's own bucket, its registry entry, that entry's signature object
//!   and the registry digest is written or deleted.
//!
//! A republish (the worktree came back) re-derives the entry from the worktree's own bucket, so the
//! orphan state clears itself. No function here reads the wall clock.

use serde::{Deserialize, Serialize};

use crate::overlay::{ObjectBackend, S2Error, validate_key};
use crate::promote::{ArchiveTarget, is_promoted};
use crate::registry_sign::SIG_PREFIX;
use crate::share::{State, read_handoff};
use crate::share_fetch::{REGISTRY_PREFIX, RegistryEntry, expected_worktree_bucket, list_shared_lenient, write_registry_entry};

const HOUR_MS: u64 = 3_600_000;
const DAY_MS: u64 = 24 * HOUR_MS;

/// Hours a registered worktree's path must stay missing before it is marked orphan.
pub const ENV_ORPHAN_AFTER_HOURS: &str = "WEFTOS_WORKTREE_ORPHAN_AFTER_HOURS";
/// Days an orphan store is kept (recoverable in the store) before `gc --confirmed` archives and deletes it.
pub const ENV_ARCHIVE_AFTER_DAYS: &str = "WEFTOS_WORKTREE_ORPHAN_ARCHIVE_AFTER_DAYS";

/// The proposed policy (design note section 10, question 5). Reversible: set the env names above,
/// or leave `gc` without `--confirmed` (a dry run) and nothing is ever deleted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct OrphanPolicy {
    /// Path missing at least this long before marking (default 24 h).
    pub orphan_after_ms: u64,
    /// Orphan for at least this long before archive and delete (default 14 d), counted from marking.
    pub archive_after_ms: u64,
}

impl Default for OrphanPolicy {
    fn default() -> Self {
        Self { orphan_after_ms: DAY_MS, archive_after_ms: 14 * DAY_MS }
    }
}

impl OrphanPolicy {
    /// Zero would mark on the first sweep or delete the moment a store is marked: refused.
    pub fn validate(&self) -> Result<(), S2Error> {
        if self.orphan_after_ms == 0 || self.archive_after_ms == 0 {
            return Err(S2Error::Invalid("orphan policy: the grace and the archive delay must both be above zero".into()));
        }
        Ok(())
    }

    /// Defaults, overridden by `get(ENV_*)` (whole numbers of hours and days). A bad value is an error.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self, S2Error> {
        let mut p = Self::default();
        let num = |name: &str| -> Result<Option<u64>, S2Error> {
            match get(name).filter(|v| !v.trim().is_empty()) {
                None => Ok(None),
                Some(v) => v.trim().parse::<u64>().map(Some).map_err(|_| S2Error::Invalid(format!("{name} must be a whole number, got {v:?}"))),
            }
        };
        if let Some(h) = num(ENV_ORPHAN_AFTER_HOURS)? {
            p.orphan_after_ms = h.saturating_mul(HOUR_MS);
        }
        if let Some(d) = num(ENV_ARCHIVE_AFTER_DAYS)? {
            p.archive_after_ms = d.saturating_mul(DAY_MS);
        }
        p.validate()?;
        Ok(p)
    }

    pub fn from_env() -> Result<Self, S2Error> {
        Self::from_lookup(|k| std::env::var(k).ok())
    }
}

/// What the registry remembers about an orphan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrphanInfo {
    /// When the daemon marked it (the clock for the archive delay).
    pub orphaned_at_ms: u64,
    /// The last head the worktree published.
    pub last_head: String,
    /// When the sweep first saw the local path missing.
    pub missing_since_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    NoEntry,
    /// Not `open`: claimed, handed off, merged, archived. Never orphaned, never deleted.
    NotOpen(State),
    /// The worktree's own bucket says it landed or is no longer open.
    Landed,
    /// The path has not been missing long enough.
    TooSoon { missing_ms: u64, grace_ms: u64 },
    FutureSince,
    /// Retire only: the entry is not an orphan yet, or not one for long enough.
    NotOrphan,
    NotDue { orphan_ms: u64, needed_ms: u64 },
    /// The entry's bucket is not the one this slug must have, or is a shared bucket.
    WrongBucket(String),
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let h = |ms: &u64| ms / HOUR_MS;
        match self {
            Refusal::NoEntry => write!(f, "no registry entry for this worktree (never published)"),
            Refusal::NotOpen(s) => write!(f, "refusing: the worktree is {s:?}, only an open one can be orphaned"),
            Refusal::Landed => write!(f, "refusing: the worktree's own record says it is claimed, landed or archived"),
            Refusal::TooSoon { missing_ms, grace_ms } => write!(f, "path missing {} h, grace is {} h", h(missing_ms), h(grace_ms)),
            Refusal::FutureSince => write!(f, "refusing: missing-since is in the future"),
            Refusal::NotOrphan => write!(f, "refusing: not an orphan"),
            Refusal::NotDue { orphan_ms, needed_ms } => write!(f, "orphan {} h, archive needs {} h", h(orphan_ms), h(needed_ms)),
            Refusal::WrongBucket(b) => write!(f, "refusing: bucket {b} is not this worktree's"),
        }
    }
}

#[derive(Debug)]
pub enum OrphanError {
    Refused(Refusal),
    Store(S2Error),
}
impl std::fmt::Display for OrphanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OrphanError::Refused(r) => r.fmt(f),
            OrphanError::Store(e) => e.fmt(f),
        }
    }
}
impl std::error::Error for OrphanError {}
impl From<S2Error> for OrphanError {
    fn from(e: S2Error) -> Self {
        OrphanError::Store(e)
    }
}
impl From<Refusal> for OrphanError {
    fn from(r: Refusal) -> Self {
        OrphanError::Refused(r)
    }
}

fn read_entry<B: ObjectBackend>(b: &B, project_bucket: &str, slug: &str) -> Result<Option<RegistryEntry>, OrphanError> {
    validate_key(slug)?;
    let Some(bytes) = b.get(project_bucket, &format!("{REGISTRY_PREFIX}{slug}.json"))? else { return Ok(None) };
    let e: RegistryEntry = serde_json::from_slice(&bytes).map_err(S2Error::from)?;
    if e.slug != slug || expected_worktree_bucket(project_bucket, slug).is_some_and(|x| x != e.bucket) {
        return Err(Refusal::WrongBucket(e.bucket).into());
    }
    Ok(Some(e))
}

/// The worktree's own record must agree the store is still open and unlanded.
fn check_open<B: ObjectBackend>(b: &B, bucket: &str) -> Result<(), OrphanError> {
    if is_promoted(b, bucket)?.is_some() || read_handoff(b, bucket)?.is_some_and(|h| h.state != State::Open) {
        return Err(Refusal::Landed.into());
    }
    Ok(())
}

/// Mark `slug` orphan, writing the entry with `write` (so the daemon can sign it). Returns the entry
/// as it now stands. Already an orphan: returned unchanged, nothing written (the sweep calls this
/// on every pass). A refusal writes nothing.
///
/// A publish racing this call can re-derive the entry between the read and the write; the daemon is
/// the only writer of the registry, so that race is one daemon handling two requests at once.
pub fn mark_orphan_with<B: ObjectBackend>(
    b: &B,
    project_bucket: &str,
    slug: &str,
    now_ms: u64,
    since_ms: u64,
    policy: &OrphanPolicy,
    write: &dyn Fn(&RegistryEntry) -> Result<(), S2Error>,
) -> Result<RegistryEntry, OrphanError> {
    policy.validate()?;
    let e = read_entry(b, project_bucket, slug)?.ok_or(Refusal::NoEntry)?;
    if e.state == State::Orphan {
        return Ok(e);
    }
    if e.state != State::Open {
        return Err(Refusal::NotOpen(e.state).into());
    }
    check_open(b, &e.bucket)?;
    if since_ms > now_ms {
        return Err(Refusal::FutureSince.into());
    }
    let missing_ms = now_ms - since_ms;
    if missing_ms < policy.orphan_after_ms {
        return Err(Refusal::TooSoon { missing_ms, grace_ms: policy.orphan_after_ms }.into());
    }
    let marked = RegistryEntry {
        state: State::Orphan,
        updated_ms: now_ms.max(e.updated_ms),
        orphan: Some(OrphanInfo { orphaned_at_ms: now_ms, last_head: e.head_commit.clone(), missing_since_ms: since_ms }),
        ..e
    };
    write(&marked)?;
    Ok(marked)
}

/// [`mark_orphan_with`] writing an unsigned entry (and its digest).
pub fn mark_orphan<B: ObjectBackend>(
    b: &B,
    project_bucket: &str,
    slug: &str,
    now_ms: u64,
    since_ms: u64,
    policy: &OrphanPolicy,
) -> Result<RegistryEntry, OrphanError> {
    mark_orphan_with(b, project_bucket, slug, now_ms, since_ms, policy, &|e| write_registry_entry(b, project_bucket, e))
}

/// One orphan as `gc` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OrphanCandidate {
    pub slug: String,
    pub bucket: String,
    pub last_head: String,
    pub orphaned_at_ms: u64,
    pub missing_since_ms: u64,
    pub orphan_ms: u64,
    /// Old enough to archive and delete under the policy.
    pub due: bool,
    /// `0` when due.
    pub due_in_ms: u64,
    pub objects: u64,
    pub bytes: u64,
}

/// Bucket gone is "nothing in it" (a retry after a half-finished retire).
fn list_or_empty<B: ObjectBackend>(b: &B, bucket: &str) -> Result<Vec<crate::overlay::ObjectMeta>, S2Error> {
    match b.list(bucket, "") {
        Err(S2Error::Backend(m)) if m.contains("no such bucket") => Ok(vec![]),
        r => r,
    }
}

/// Every orphan in the project's registry with its age and size. Reads only.
pub fn plan_orphans<B: ObjectBackend>(b: &B, project_bucket: &str, now_ms: u64, policy: &OrphanPolicy) -> Result<Vec<OrphanCandidate>, S2Error> {
    policy.validate()?;
    plan_orphans_from(b, list_shared_lenient(b, project_bucket)?.0, now_ms, policy)
}

/// [`plan_orphans`] on registry entries the caller already read (a display plan, WO-069 X2). An act
/// (a confirmed gc) must plan from a fresh read, never from cached entries.
pub fn plan_orphans_from<B: ObjectBackend>(b: &B, entries: Vec<RegistryEntry>, now_ms: u64, policy: &OrphanPolicy) -> Result<Vec<OrphanCandidate>, S2Error> {
    policy.validate()?;
    let mut out = Vec::new();
    for e in entries {
        let (State::Orphan, Some(o)) = (e.state, e.orphan.as_ref()) else { continue };
        let orphan_ms = now_ms.saturating_sub(o.orphaned_at_ms);
        let objs = list_or_empty(b, &e.bucket)?;
        out.push(OrphanCandidate {
            slug: e.slug.clone(),
            bucket: e.bucket.clone(),
            last_head: o.last_head.clone(),
            orphaned_at_ms: o.orphaned_at_ms,
            missing_since_ms: o.missing_since_ms,
            orphan_ms,
            due: orphan_ms >= policy.archive_after_ms,
            due_in_ms: policy.archive_after_ms.saturating_sub(orphan_ms),
            objects: objs.len() as u64,
            bytes: objs.iter().map(|m| m.size).sum(),
        });
    }
    out.sort_by(|a, c| a.slug.cmp(&c.slug));
    Ok(out)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Retired {
    pub slug: String,
    pub bucket: String,
    pub archived: u64,
    pub bytes: u64,
}

/// Archive then delete one due orphan. Every re-check runs again here (the entry is still an
/// orphan, old enough, the worktree's own record still open), because a dry run's answer can be
/// stale. Order: copy every object to the archive, verify each copy by size, delete the objects,
/// call `drop_bucket` (the daemon removes the now empty bucket and forgets the store), then remove
/// the registry entry. A failure at any step stops there with everything still recoverable: until
/// the verified copies exist nothing is deleted, and a rerun resumes (an already emptied or missing
/// bucket counts as empty).
pub fn retire_orphan<B: ObjectBackend>(
    b: &B,
    project_bucket: &str,
    slug: &str,
    now_ms: u64,
    policy: &OrphanPolicy,
    archive: &ArchiveTarget<'_>,
    drop_bucket: &dyn Fn(&str) -> Result<(), S2Error>,
) -> Result<Retired, OrphanError> {
    policy.validate()?;
    validate_key(archive.prefix.trim_end_matches('/'))?;
    let e = read_entry(b, project_bucket, slug)?.ok_or(Refusal::NoEntry)?;
    let (State::Orphan, Some(o)) = (e.state, e.orphan.as_ref()) else { return Err(Refusal::NotOrphan.into()) };
    let orphan_ms = now_ms.saturating_sub(o.orphaned_at_ms);
    if orphan_ms < policy.archive_after_ms {
        return Err(Refusal::NotDue { orphan_ms, needed_ms: policy.archive_after_ms }.into());
    }
    if e.bucket == project_bucket || e.bucket == archive.bucket {
        return Err(Refusal::WrongBucket(e.bucket).into());
    }
    check_open(b, &e.bucket)?;
    let objs = list_or_empty(b, &e.bucket)?;
    for m in &objs {
        let to = format!("{}{}", archive.prefix, m.key);
        b.copy(&e.bucket, &m.key, archive.bucket, &to)?;
        if b.head(archive.bucket, &to)?.map(|h| h.size) != Some(m.size) {
            return Err(S2Error::Backend(format!("archive copy of {} did not verify; nothing was deleted", m.key)).into());
        }
    }
    for m in &objs {
        b.delete(&e.bucket, &m.key)?;
    }
    drop_bucket(&e.bucket)?;
    crate::follower::remove_registry_entry(b, project_bucket, slug)?; // entry, then the digest
    let sig = format!("{SIG_PREFIX}{slug}.json");
    if b.head(project_bucket, &sig)?.is_some() {
        b.delete(project_bucket, &sig)?;
    }
    Ok(Retired { slug: slug.into(), bucket: e.bucket, archived: objs.len() as u64, bytes: objs.iter().map(|m| m.size).sum() })
}
