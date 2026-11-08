//! [`ReleaseBackend`] over `weftos-store`'s [`ObjectBackend`] (ADR-126 S1), so a release store
//! lives in the RustFS bucket S1 maps its name to.
//!
//! The mapping is exact: `put_if_absent` is `put(.., if_none_match = true)` (412 is
//! `BackendError::Exists`), `get` and `list` are GetObject and ListObjectsV2, `delete` is DeleteObject.
//! S1's transport is blocking, so each call runs on tokio's blocking pool.

use std::sync::Arc;

use weftos_store::backend::{LockMode as StoreMode, PutOpts, Retention};
use weftos_store::{AdminBackend, BackendError as StoreErr, ObjectBackend, StoreName};

use crate::backend::{BackendError, DeletableBackend, Lock, LockMode, PutOutcome, ReleaseBackend};
use crate::name::ReleaseStoreName;

/// One release store's bucket on an S1 object backend.
///
/// Hand CI an adapter built on a credential whose policy lacks DeleteObject and `tombstones/*` put
/// (ADR-126 section 3); the sweeper gets its own, wider one. Both use this same type.
pub struct StoreBackend<O: ObjectBackend + 'static> {
    /// `None` only while dropping.
    objects: Option<Arc<O>>,
    bucket: String,
}

/// S1's RustFS transport owns a blocking HTTP client, and tokio panics if one is dropped on an
/// async thread. Hand the last reference to the blocking pool instead.
impl<O: ObjectBackend + 'static> Drop for StoreBackend<O> {
    fn drop(&mut self) {
        if let Some(o) = self.objects.take() {
            match tokio::runtime::Handle::try_current() {
                Ok(h) => {
                    drop(h.spawn_blocking(move || drop(o)));
                }
                Err(_) => drop(o),
            }
        }
    }
}

impl<O: ObjectBackend + 'static> StoreBackend<O> {
    /// Use an explicit bucket.
    pub fn new(objects: Arc<O>, bucket: impl Into<String>) -> Self {
        Self { objects: Some(objects), bucket: bucket.into() }
    }

    /// Use the bucket S1 assigns to this release store's name.
    pub fn for_release(objects: Arc<O>, store: &ReleaseStoreName) -> Result<Self, BackendError> {
        let name = StoreName::parse(&store.uri()).map_err(|e| BackendError(e.to_string()))?;
        Ok(Self::new(objects, name.bucket()))
    }

    pub fn bucket(&self) -> &str {
        &self.bucket
    }

    async fn blocking<T: Send + 'static>(
        &self,
        f: impl FnOnce(&O, &str) -> Result<T, StoreErr> + Send + 'static,
    ) -> Result<T, StoreErr> {
        let o = self.objects.clone().expect("present until dropped");
        let b = self.bucket.clone();
        tokio::task::spawn_blocking(move || f(&o, &b))
            .await
            .map_err(|e| StoreErr::Io(format!("blocking task: {e}")))?
    }
}

fn err(e: StoreErr) -> BackendError {
    BackendError(e.to_string())
}

impl<O: ObjectBackend + 'static> ReleaseBackend for StoreBackend<O> {
    async fn put_if_absent(&self, key: &str, bytes: &[u8]) -> Result<PutOutcome, BackendError> {
        let (k, data) = (key.to_string(), bytes.to_vec());
        match self.blocking(move |o, b| o.put(b, &k, &data, true)).await {
            Ok(_) => Ok(PutOutcome::Created),
            Err(StoreErr::Exists(_)) => Ok(PutOutcome::Exists),
            Err(e) => Err(err(e)),
        }
    }

    async fn put_if_absent_locked(&self, key: &str, bytes: &[u8], lock: &Lock) -> Result<PutOutcome, BackendError> {
        let (k, data) = (key.to_string(), bytes.to_vec());
        let opts = PutOpts {
            if_none_match: true,
            retention: Some(Retention {
                mode: match lock.mode {
                    LockMode::Governance => StoreMode::Governance,
                    LockMode::Compliance => StoreMode::Compliance,
                },
                until_unix_s: lock.until,
            }),
            legal_hold: false,
        };
        match self.blocking(move |o, b| o.put_locked(b, &k, &data, &opts)).await {
            Ok(_) => Ok(PutOutcome::Created),
            Err(StoreErr::Exists(_)) => Ok(PutOutcome::Exists),
            Err(e) => Err(err(e)),
        }
    }

    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, BackendError> {
        let k = key.to_string();
        match self.blocking(move |o, b| o.get(b, &k)).await {
            Ok(v) => Ok(Some(v)),
            Err(StoreErr::NoObject(_)) => Ok(None),
            Err(e) => Err(err(e)),
        }
    }

    async fn list(&self, prefix: &str) -> Result<Vec<String>, BackendError> {
        let p = prefix.to_string();
        let metas = self.blocking(move |o, b| o.list(b, &p)).await.map_err(err)?;
        Ok(metas.into_iter().map(|m| m.key).collect())
    }
}

impl<O: ObjectBackend + 'static> DeletableBackend for StoreBackend<O> {
    async fn copy(&self, src: &str, dst: &str) -> Result<(), BackendError> {
        let (s, d) = (src.to_string(), dst.to_string());
        self.blocking(move |o, b| o.copy(b, &s, b, &d).map(|_| ())).await.map_err(err)
    }

    async fn delete(&self, key: &str) -> Result<(), BackendError> {
        let k = key.to_string();
        match self.blocking(move |o, b| o.delete(b, &k)).await {
            Ok(()) | Err(StoreErr::NoObject(_)) => Ok(()),
            Err(e) => Err(err(e)),
        }
    }
}

/// Make the bucket for a release store with Object Lock on (versioning comes with it). The choice
/// is only available at creation, so a release store that is meant to be locked must be made this
/// way. S1's service does the same through `create_locked_store` when it owns the registry; this is
/// for provisioning scripts and tests that hold an admin backend.
pub fn create_locked_release_bucket<A: AdminBackend>(admin: &A, store: &ReleaseStoreName) -> Result<String, BackendError> {
    let name = StoreName::parse(&store.uri()).map_err(|e| BackendError(e.to_string()))?;
    let mut tags = name.tags();
    tags.push(("object-lock".into(), "on".into()));
    admin.create_locked_bucket(&name.bucket(), &tags).map_err(err)?;
    Ok(name.bucket())
}

/// IAM policy for a CI key that writes straight to a release bucket, as `weaver store release publish`
/// does. Measured on RustFS 1.0.4 (`tests/store_ci_key.rs`):
///
/// - `s3:PutObject` alone cannot send Object Lock headers; the put also needs `s3:PutObjectRetention`.
/// - The Deny refuses any put that carries no lock mode (condition `Null s3:object-lock-mode`), so every
///   version this key writes is locked. RustFS IAM cannot require `If-None-Match`.
/// - **It cannot stop this key replacing a key it can write**: an unconditional put with lock headers
///   makes a new version that reads as current (the locked old version survives, readable by version id).
///   The publisher's own `If-None-Match` is what protects against that, and S1's daemon-mediated append
///   role (read-only at RustFS) is the only way to make write-once a server-side fact. Use this role only
///   where CI is trusted not to do that.
/// - There is no DeleteObject, so deletion stays with the sweeper.
pub fn direct_ci_policy(bucket: &str) -> serde_json::Value {
    serde_json::json!({
        "Version": "2012-10-17",
        "Statement": [
            {
                "Effect": "Allow",
                "Action": ["s3:GetObject", "s3:ListBucket", "s3:GetBucketLocation", "s3:PutObject", "s3:PutObjectRetention"],
                "Resource": [format!("arn:aws:s3:::{bucket}"), format!("arn:aws:s3:::{bucket}/*")]
            },
            {
                "Effect": "Deny",
                "Action": ["s3:PutObject"],
                "Resource": [format!("arn:aws:s3:::{bucket}/*")],
                "Condition": {"Null": {"s3:object-lock-mode": "true"}}
            }
        ]
    })
}
