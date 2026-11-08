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
    /// Delete one version. A locked version is refused ([`BackendError::Forbidden`]) until its
    /// retention ends or its legal hold is lifted; `bypass_governance` overrides only `Governance`.
    fn delete_version(&self, _bucket: &str, _key: &str, _version_id: &str, _bypass_governance: bool) -> Result<(), BackendError> {
        Err(BackendError::Unsupported("object lock".into()))
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
    fn delete(&self, bucket: &str, key: &str) -> Result<(), BackendError>;
    /// Copy within or across buckets.
    fn copy(&self, src_bucket: &str, src_key: &str, dst_bucket: &str, dst_key: &str) -> Result<ObjectMeta, BackendError>;
    /// Objects whose key starts with `prefix`, sorted by key.
    fn list(&self, bucket: &str, prefix: &str) -> Result<Vec<ObjectMeta>, BackendError>;
}

/// Buckets, access keys and policies (the RustFS admin API).
pub trait AdminBackend: Send + Sync {
    fn create_bucket(&self, bucket: &str, tags: &[(String, String)]) -> Result<(), BackendError>;
    /// Create a bucket with Object Lock enabled (which also turns versioning on). It can only be
    /// chosen at creation. Local backends answer [`BackendError::Unsupported`].
    /// Replace the bucket's lifecycle rules with `rules` (an empty list removes them all).
    fn put_lifecycle(&self, _bucket: &str, _rules: &[LifecycleRule]) -> Result<(), BackendError> {
        Err(BackendError::Unsupported("lifecycle rules".into()))
    }
    /// The bucket's lifecycle rules (none when it has none).
    fn get_lifecycle(&self, _bucket: &str) -> Result<Vec<LifecycleRule>, BackendError> {
        Err(BackendError::Unsupported("lifecycle rules".into()))
    }
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

/// An object key every backend accepts: non-empty, at most 512 bytes, no NUL or backslash,
/// no empty, `.` or `..` segments, no leading or trailing `/`.
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
    Ok(())
}
