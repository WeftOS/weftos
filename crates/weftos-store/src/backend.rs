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
    #[error("bad object key: {0}")]
    BadKey(String),
    #[error("backend: {0}")]
    Io(String),
}

/// Size and identity of one object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectMeta {
    pub key: String,
    pub size: u64,
    /// BLAKE3 hex of the content.
    pub etag: String,
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
