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
    /// A conditional put or delete found a different etag than the caller expected (HTTP 412).
    #[error("object {0} changed: etag precondition failed")]
    PreconditionFailed(String),
    /// This backend has no conditional write. Callers fall back to head-then-write.
    #[error("conditional write unsupported by this backend")]
    Unsupported,
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
    fn get(&self, bucket: &str, key: &str) -> Result<Vec<u8>, BackendError>;
    fn head(&self, bucket: &str, key: &str) -> Result<ObjectMeta, BackendError>;
    /// Store an object only if the key's current etag is `etag` (HTTP `If-Match`). A different
    /// etag is [`BackendError::PreconditionFailed`], a missing key is [`BackendError::NoObject`].
    /// The default says [`BackendError::Unsupported`], so existing backends compile unchanged.
    fn put_if_match(&self, _bucket: &str, _key: &str, _data: &[u8], _etag: &str) -> Result<ObjectMeta, BackendError> {
        Err(BackendError::Unsupported)
    }
    /// Delete an object only if its current etag is `etag`. Same errors as [`Self::put_if_match`].
    fn delete_if_match(&self, _bucket: &str, _key: &str, _etag: &str) -> Result<(), BackendError> {
        Err(BackendError::Unsupported)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The contract every backend with a conditional write must meet.
    fn conditional_contract<B: ObjectBackend + AdminBackend>(b: &B) {
        b.create_bucket("b-cond", &[]).unwrap();
        let v1 = b.put("b-cond", "k", b"one", false).unwrap();
        // Right etag: written, new etag returned.
        let v2 = b.put_if_match("b-cond", "k", b"two", &v1.etag).unwrap();
        assert_eq!(v2.etag, etag_of(b"two"));
        assert_eq!(b.get("b-cond", "k").unwrap(), b"two");
        // Stale etag: refused, object untouched.
        assert!(matches!(b.put_if_match("b-cond", "k", b"three", &v1.etag), Err(BackendError::PreconditionFailed(_))));
        assert_eq!(b.get("b-cond", "k").unwrap(), b"two");
        // Missing key: NoObject, nothing created.
        assert!(matches!(b.put_if_match("b-cond", "gone", b"x", &v1.etag), Err(BackendError::NoObject(_))));
        assert!(b.get("b-cond", "gone").is_err());
        // Conditional delete: stale refused and untouched, right one deletes.
        assert!(matches!(b.delete_if_match("b-cond", "k", &v1.etag), Err(BackendError::PreconditionFailed(_))));
        assert_eq!(b.get("b-cond", "k").unwrap(), b"two");
        b.delete_if_match("b-cond", "k", &v2.etag).unwrap();
        assert!(matches!(b.get("b-cond", "k"), Err(BackendError::NoObject(_))));
        assert!(matches!(b.delete_if_match("b-cond", "k", &v2.etag), Err(BackendError::NoObject(_))));
    }

    #[test]
    fn mem_backend_meets_the_conditional_contract() {
        conditional_contract(&crate::mem::MemBackend::new());
    }

    #[test]
    fn fs_backend_meets_the_conditional_contract() {
        let d = tempfile::tempdir().unwrap();
        conditional_contract(&crate::fs::FsBackend::open(d.path()).unwrap());
    }

    #[test]
    fn a_backend_with_no_conditional_write_says_unsupported() {
        struct Bare;
        impl ObjectBackend for Bare {
            fn put(&self, _: &str, k: &str, d: &[u8], _: bool) -> Result<ObjectMeta, BackendError> {
                Ok(ObjectMeta { key: k.into(), size: d.len() as u64, etag: etag_of(d) })
            }
            fn get(&self, _: &str, _: &str) -> Result<Vec<u8>, BackendError> { Err(BackendError::Unsupported) }
            fn head(&self, _: &str, _: &str) -> Result<ObjectMeta, BackendError> { Err(BackendError::Unsupported) }
            fn delete(&self, _: &str, _: &str) -> Result<(), BackendError> { Ok(()) }
            fn copy(&self, _: &str, _: &str, _: &str, _: &str) -> Result<ObjectMeta, BackendError> { Err(BackendError::Unsupported) }
            fn list(&self, _: &str, _: &str) -> Result<Vec<ObjectMeta>, BackendError> { Ok(vec![]) }
        }
        assert_eq!(Bare.put_if_match("b", "k", b"x", "e"), Err(BackendError::Unsupported));
        assert_eq!(Bare.delete_if_match("b", "k", "e"), Err(BackendError::Unsupported));
    }
}
