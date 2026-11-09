//! What holds the bytes: a data plane and a control plane (ADR-126 sections 3 and 7).

use serde_json::Value;

/// Backend failures.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BackendError {
    #[error("no such bucket {0}")]
    NoBucket(String),
    #[error("bucket {0} already exists")]
    BucketExists(String),
    #[error("no such object {0}")]
    NoObject(String),
    /// A conditional put found the key already there (HTTP 412).
    #[error("object {0} already exists")]
    Exists(String),
    /// A conditional put found a different object than the one the caller read (HTTP 412 on
    /// `If-Match`): someone else wrote since. Read again and retry.
    #[error("object {0} changed since it was read (etag mismatch)")]
    Stale(String),
    /// The credential or its policy refuses this (HTTP 403).
    #[error("forbidden: {0}")]
    Forbidden(String),
    /// The backend cannot do this (Object Lock on the local backends).
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// The backend could not be reached, or answered 5xx/429/408: nothing says the request was
    /// refused, so the same call may work later. Already retried (see `Retry`) before it surfaces.
    #[error("backend unavailable: {0}")]
    Unavailable(String),
    #[error("bad object key: {0}")]
    BadKey(String),
    #[error("backend: {0}")]
    Io(String),
}

impl BackendError {
    /// Whether trying again later might succeed (the backend, not the request, was the problem).
    pub fn is_transient(&self) -> bool {
        matches!(self, Self::Unavailable(_))
    }
}

/// Read until `n` bytes or the end.
fn read_up_to(src: &mut dyn std::io::Read, n: usize) -> Result<Vec<u8>, BackendError> {
    let mut buf = Vec::with_capacity(n.min(16 * 1024 * 1024));
    use std::io::Read;
    src.take(n as u64).read_to_end(&mut buf).map_err(|e| BackendError::Io(e.to_string()))?;
    Ok(buf)
}

/// Size and identity of one object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectMeta {
    pub key: String,
    pub size: u64,
    /// BLAKE3 hex of the content.
    pub etag: String,
}

/// One stored version of a key in a versioned (locked) bucket, or a delete marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectVersion {
    pub key: String,
    /// The version id; `"null"` for an object written before versioning was on.
    pub version_id: String,
    /// The newest entry for its key.
    pub is_latest: bool,
    /// A delete marker (a plain delete in a versioned bucket): no content, size 0.
    pub is_delete_marker: bool,
    pub size: u64,
    /// The backend's etag for this version (empty for a delete marker).
    pub etag: String,
    /// When RustFS stored it, as the ISO-8601 text it sent.
    pub last_modified: String,
}

/// S3 Object Lock retention mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockMode {
    /// Cannot be shortened or removed before the date, except by a caller who asks to bypass it
    /// (`s3:BypassGovernanceRetention`).
    Governance,
    /// Cannot be shortened or removed by anyone, root included, until the date.
    Compliance,
}

impl LockMode {
    /// The header value.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Governance => "GOVERNANCE",
            Self::Compliance => "COMPLIANCE",
        }
    }
}

/// Keep this version until `until_unix_s`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retention {
    pub mode: LockMode,
    /// Seconds since the Unix epoch; must be in the future when the object is put.
    pub until_unix_s: u64,
}

/// What a locked put asks for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PutOpts {
    /// Refuse an existing key (`If-None-Match: *`).
    pub if_none_match: bool,
    pub retention: Option<Retention>,
    /// Put a legal hold on the version (no end date; lifted by `put_legal_hold(false)`).
    pub legal_hold: bool,
}

/// A put into a versioned (locked) bucket: the object and the version that was written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockedPut {
    pub meta: ObjectMeta,
    pub version_id: Option<String>,
}

/// A bucket's default retention (applied to a put that carries no retention headers).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DefaultRetention {
    pub mode: LockMode,
    /// Exactly one of `days` and `years` is set.
    pub days: Option<u32>,
    pub years: Option<u32>,
}

/// A bucket's Object Lock configuration, as read back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LockConfig {
    /// The bucket was made with Object Lock.
    pub enabled: bool,
    pub default: Option<DefaultRetention>,
}

/// One bucket lifecycle rule: what the backend does to objects by age, so nobody has to sweep.
/// At least one action is required. Object Lock still wins: an expiry never removes a version
/// whose retention or legal hold has not ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LifecycleRule {
    /// Unique within the bucket, 1..=255 characters.
    pub id: String,
    /// Keys starting with this (empty: the whole bucket).
    pub prefix: String,
    pub enabled: bool,
    /// Expire the current version this many days after it was written (in a versioned bucket this
    /// adds a delete marker).
    pub expire_days: Option<u32>,
    /// Remove a version this many days after a newer one replaced it.
    pub noncurrent_days: Option<u32>,
    /// Abort a multipart upload not completed this many days after it started.
    pub abort_multipart_days: Option<u32>,
}

impl LifecycleRule {
    /// Check the rule before it is sent.
    pub fn validate(&self) -> Result<(), BackendError> {
        let bad = |m: &str| Err(BackendError::BadKey(format!("lifecycle rule {:?}: {m}", self.id)));
        if self.id.is_empty() || self.id.len() > 255 {
            return bad("id must be 1..=255 characters");
        }
        if self.expire_days.is_none() && self.noncurrent_days.is_none() && self.abort_multipart_days.is_none() {
            return bad("needs at least one action");
        }
        if [self.expire_days, self.noncurrent_days, self.abort_multipart_days].into_iter().flatten().any(|d| d == 0) {
            return bad("days must be at least 1");
        }
        Ok(())
    }
}

/// One uploaded part of a multipart upload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part {
    /// 1..=10000.
    pub number: u32,
    pub etag: String,
}

/// Smallest part S3 accepts (the last part may be smaller).
pub const MIN_PART: usize = 5 * 1024 * 1024;
/// Part size [`ObjectBackend::put_stream`] uses by default.
pub const DEFAULT_PART: usize = 8 * 1024 * 1024;
/// Largest object a backend without multipart will take through `put_stream` (it holds it in memory).
pub const LOCAL_MAX: usize = 512 * 1024 * 1024;
/// Most parts one upload can have.
pub const MAX_PARTS: u32 = 10_000;

/// An access key the backend issued for one principal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessKey {
    pub id: String,
}

/// A bucket name and its tags.
pub type BucketTags = (String, Vec<(String, String)>);

/// Object operations on a bucket.
pub trait ObjectBackend: Send + Sync {
    /// Store an object. With `if_none_match`, refuse an existing key ([`BackendError::Exists`]).
    fn put(&self, bucket: &str, key: &str, data: &[u8], if_none_match: bool) -> Result<ObjectMeta, BackendError>;
    /// Replace an object only if its current etag is `etag` (a compare-and-swap, S3 `If-Match`).
    /// A different etag is [`BackendError::Stale`], a missing key is [`BackendError::NoObject`].
    /// Take the etag from the last `put` or `head` of this backend: etags are opaque and differ
    /// between backends (BLAKE3 for the local ones, MD5 for RustFS).
    fn put_if_match(&self, bucket: &str, key: &str, data: &[u8], etag: &str) -> Result<ObjectMeta, BackendError>;
    /// Put with Object Lock headers (retention and/or legal hold). Needs a bucket made with
    /// [`AdminBackend::create_locked_bucket`]; others answer [`BackendError::Unsupported`].
    fn put_locked(&self, _bucket: &str, _key: &str, _data: &[u8], _opts: &PutOpts) -> Result<LockedPut, BackendError> {
        Err(BackendError::Unsupported("object lock".into()))
    }
    /// Start a multipart upload (carrying the lock headers of `opts`, if any); returns its id.
    /// `opts.if_none_match` is applied when the upload is completed.
    fn create_multipart(&self, _bucket: &str, _key: &str, _opts: &PutOpts) -> Result<String, BackendError> {
        Err(BackendError::Unsupported("multipart upload".into()))
    }
    /// Upload one part (5 MiB or more unless it is the last); returns its etag. Repeating a part
    /// number replaces it, so a failed part can be sent again.
    fn upload_part(&self, _bucket: &str, _key: &str, _upload_id: &str, _number: u32, _data: &[u8]) -> Result<String, BackendError> {
        Err(BackendError::Unsupported("multipart upload".into()))
    }
    /// Finish the upload from its parts, in order. Nothing is visible before this succeeds.
    fn complete_multipart(&self, _bucket: &str, _key: &str, _upload_id: &str, _parts: &[Part], _opts: &PutOpts) -> Result<LockedPut, BackendError> {
        Err(BackendError::Unsupported("multipart upload".into()))
    }
    /// Throw an unfinished upload away.
    fn abort_multipart(&self, _bucket: &str, _key: &str, _upload_id: &str) -> Result<(), BackendError> {
        Err(BackendError::Unsupported("multipart upload".into()))
    }
    /// Store an object read from `src` without holding it all in memory: parts of `part_size`
    /// bytes (at least [`MIN_PART`]) go up as they are read, so only one part is in memory. An
    /// object smaller than one part is a single put. A failure aborts the upload; nothing is left
    /// visible. Backends without multipart (the local ones) read it all and put it once, which
    /// keeps the old size limit, and refuse lock options.
    fn put_stream(&self, bucket: &str, key: &str, src: &mut dyn std::io::Read, part_size: usize, opts: &PutOpts) -> Result<LockedPut, BackendError> {
        // Between the smallest part S3 takes and 1 GiB: a bigger one only holds memory.
        let part_size = part_size.clamp(MIN_PART, 1024 * 1024 * 1024);
        let first = read_up_to(src, part_size)?;
        if first.len() < part_size {
            // Fits in one part: one request, lock headers and all.
            return match self.put_locked(bucket, key, &first, opts) {
                Err(BackendError::Unsupported(_)) if opts.retention.is_none() && !opts.legal_hold => {
                    let meta = self.put(bucket, key, &first, opts.if_none_match)?;
                    Ok(LockedPut { meta, version_id: None })
                }
                other => other,
            };
        }
        let id = match self.create_multipart(bucket, key, opts) {
            Ok(id) => id,
            Err(BackendError::Unsupported(_)) if opts.retention.is_none() && !opts.legal_hold => {
                // The local backends hold the object in memory: the old whole-body limit applies.
                let mut all = first;
                let room = (LOCAL_MAX as u64).saturating_sub(all.len() as u64);
                use std::io::Read as _;
                (&mut *src).take(room + 1).read_to_end(&mut all).map_err(|e| BackendError::Io(e.to_string()))?;
                if all.len() > LOCAL_MAX {
                    return Err(BackendError::BadKey(format!("over {} MiB: this backend has no multipart upload", LOCAL_MAX / (1024 * 1024))));
                }
                let meta = self.put(bucket, key, &all, opts.if_none_match)?;
                return Ok(LockedPut { meta, version_id: None });
            }
            Err(e) => return Err(e),
        };
        let run = || -> Result<LockedPut, BackendError> {
            let (mut parts, mut total, mut buf, mut n) = (Vec::new(), 0u64, first, 1u32);
            loop {
                if n > MAX_PARTS {
                    return Err(BackendError::BadKey(format!("more than {MAX_PARTS} parts: raise part_size")));
                }
                total += buf.len() as u64;
                parts.push(Part { number: n, etag: self.upload_part(bucket, key, &id, n, &buf)? });
                if buf.len() < part_size {
                    break;
                }
                buf = read_up_to(src, part_size)?;
                if buf.is_empty() {
                    break;
                }
                n += 1;
            }
            let mut done = self.complete_multipart(bucket, key, &id, &parts, opts)?;
            done.meta.size = total;
            Ok(done)
        };
        match run() {
            Ok(v) => Ok(v),
            Err(e) => {
                let _ = self.abort_multipart(bucket, key, &id);
                Err(e)
            }
        }
    }
    /// Delete one version. A locked version is refused ([`BackendError::Forbidden`]) until its
    /// retention ends or its legal hold is lifted; `bypass_governance` overrides only `Governance`.
    fn delete_version(&self, _bucket: &str, _key: &str, _version_id: &str, _bypass_governance: bool) -> Result<(), BackendError> {
        Err(BackendError::Unsupported("object lock".into()))
    }
    /// The retention on one version (`None` if it has none).
    fn get_retention(&self, _bucket: &str, _key: &str, _version_id: &str) -> Result<Option<Retention>, BackendError> {
        Err(BackendError::Unsupported("object lock".into()))
    }
    /// Set the retention of one version, for example to stamp it after the upload is committed.
    /// RustFS refuses to shorten it (and, under `Compliance`, to change it down in any way);
    /// `bypass_governance` lets a `Governance` one be shortened. Prefer [`Self::extend_retention`].
    fn put_retention(&self, _bucket: &str, _key: &str, _version_id: &str, _r: &Retention, _bypass_governance: bool) -> Result<(), BackendError> {
        Err(BackendError::Unsupported("object lock".into()))
    }
    /// Move a version's retention date later, keeping its mode (or setting `mode` where it had
    /// none). A date that is not later than the current one is refused before anything is sent,
    /// so this can never shorten a hold. Returns the retention now in force.
    fn extend_retention(&self, bucket: &str, key: &str, version_id: &str, until_unix_s: u64, mode: LockMode) -> Result<Retention, BackendError> {
        let new = match self.get_retention(bucket, key, version_id)? {
            Some(cur) if until_unix_s <= cur.until_unix_s => {
                return Err(BackendError::BadKey(format!("retain-until {until_unix_s} does not extend the current {}", cur.until_unix_s)));
            }
            Some(cur) => Retention { mode: cur.mode, until_unix_s },
            None => Retention { mode, until_unix_s },
        };
        self.put_retention(bucket, key, version_id, &new, false)?;
        Ok(new)
    }
    /// Read a bucket's Object Lock configuration. A bucket made without lock is `enabled: false`.
    fn get_lock_config(&self, _bucket: &str) -> Result<LockConfig, BackendError> {
        Err(BackendError::Unsupported("object lock".into()))
    }
    /// Set a locked bucket's default retention (what a put with no retention headers gets).
    fn put_default_retention(&self, _bucket: &str, _d: &DefaultRetention) -> Result<(), BackendError> {
        Err(BackendError::Unsupported("object lock".into()))
    }
    /// Turn a legal hold on or off for one version.
    fn put_legal_hold(&self, _bucket: &str, _key: &str, _version_id: &str, _on: bool) -> Result<(), BackendError> {
        Err(BackendError::Unsupported("object lock".into()))
    }
    /// Every stored version and delete marker whose key starts with `prefix`, sorted by key and
    /// newest first within a key. A key with more than one non-marker version was replaced; a
    /// newest entry that is a delete marker was deleted. Backends that keep one copy per key
    /// answer [`BackendError::Unsupported`].
    fn list_versions(&self, _bucket: &str, _prefix: &str) -> Result<Vec<ObjectVersion>, BackendError> {
        Err(BackendError::Unsupported("object versions".into()))
    }
    fn get(&self, bucket: &str, key: &str) -> Result<Vec<u8>, BackendError>;
    fn head(&self, bucket: &str, key: &str) -> Result<ObjectMeta, BackendError>;
    /// Delete an object only if its current etag is `etag` (HTTP `If-Match`). Same errors as
    /// [`Self::put_if_match`]. The default says [`BackendError::Unsupported`]; the drive falls back
    /// to head-then-delete.
    fn delete_if_match(&self, _bucket: &str, _key: &str, _etag: &str) -> Result<(), BackendError> {
        Err(BackendError::Unsupported("conditional delete".into()))
    }
    fn delete(&self, bucket: &str, key: &str) -> Result<(), BackendError>;
    /// Copy within or across buckets.
    fn copy(&self, src_bucket: &str, src_key: &str, dst_bucket: &str, dst_key: &str) -> Result<ObjectMeta, BackendError>;
    /// Objects whose key starts with `prefix`, sorted by key.
    fn list(&self, bucket: &str, prefix: &str) -> Result<Vec<ObjectMeta>, BackendError>;
}

/// Buckets, access keys and policies (the RustFS admin API).
pub trait AdminBackend: Send + Sync {
    fn create_bucket(&self, bucket: &str, tags: &[(String, String)]) -> Result<(), BackendError>;
    /// Replace the bucket's lifecycle rules with `rules` (an empty list removes them all).
    fn put_lifecycle(&self, _bucket: &str, _rules: &[LifecycleRule]) -> Result<(), BackendError> {
        Err(BackendError::Unsupported("lifecycle rules".into()))
    }
    /// The bucket's lifecycle rules (none when it has none).
    fn get_lifecycle(&self, _bucket: &str) -> Result<Vec<LifecycleRule>, BackendError> {
        Err(BackendError::Unsupported("lifecycle rules".into()))
    }
    /// Create a bucket with Object Lock enabled (which also turns versioning on). It can only be
    /// chosen at creation. Local backends answer [`BackendError::Unsupported`].
    fn create_locked_bucket(&self, _bucket: &str, _tags: &[(String, String)]) -> Result<(), BackendError> {
        Err(BackendError::Unsupported("object lock".into()))
    }
    /// Delete an empty bucket.
    fn delete_bucket(&self, bucket: &str) -> Result<(), BackendError>;
    fn bucket_exists(&self, bucket: &str) -> Result<bool, BackendError>;
    /// Every bucket and its tags.
    fn buckets(&self) -> Result<Vec<BucketTags>, BackendError>;
    /// Create (or return the existing) access key for a principal label.
    fn create_access_key(&self, principal: &str) -> Result<AccessKey, BackendError>;
    /// Enable or disable a key. A disabled key's next request is refused.
    fn set_key_enabled(&self, key_id: &str, enabled: bool) -> Result<(), BackendError>;
    /// Replace the IAM policy attached to a key.
    fn put_policy(&self, key_id: &str, policy: &Value) -> Result<(), BackendError>;
    fn delete_access_key(&self, key_id: &str) -> Result<(), BackendError>;
}

/// The etag used by the built-in backends.
pub fn etag_of(data: &[u8]) -> String {
    blake3::hash(data).to_hex().to_string()
}

/// The longest one `/`-separated segment of a key, in bytes (measured on RustFS 1.0.4: 255 is
/// accepted, 256 is `InvalidArgument`).
pub const MAX_SEGMENT: usize = 255;

/// A bucket name this crate will put in a request path: 3 to 63 of `a-z`, `0-9`, `-`, not starting
/// with `-`. Bucket strings come from store names and from a registry; one with `/`, `..` or `%`
/// in it must never reach a URL (`w-x/../p-y` would address another bucket).
pub fn check_bucket(b: &str) -> Result<(), BackendError> {
    if (3..=63).contains(&b.len()) && b.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-') && !b.starts_with('-') {
        Ok(())
    } else {
        Err(BackendError::BadKey(format!("bucket {b:?}: 3 to 63 of a-z, 0-9, '-'")))
    }
}

/// An object key every backend accepts: non-empty, at most 512 bytes, no NUL or backslash,
/// no empty, `.` or `..` segments, no leading or trailing `/`, no segment over [`MAX_SEGMENT`] bytes.
pub fn check_key(key: &str) -> Result<(), BackendError> {
    let bad = |why: &str| Err(BackendError::BadKey(format!("{key:?}: {why}")));
    if key.is_empty() || key.len() > 512 {
        return bad("1 to 512 bytes");
    }
    if key.bytes().any(|b| b == 0 || b == b'\\') {
        return bad("no NUL or backslash");
    }
    if key.split('/').any(|s| s.is_empty() || s == "." || s == "..") {
        return bad("no empty, '.' or '..' segments");
    }
    // RustFS (and every file system under it) refuses one path segment over 255 bytes with an
    // opaque InvalidArgument, below the total limit: say so here, where the cause is visible.
    if key.split('/').any(|s| s.len() > MAX_SEGMENT) {
        return bad("no segment over 255 bytes");
    }
    Ok(())
}

#[cfg(test)]
#[path = "backend_tests.rs"]
mod tests;
