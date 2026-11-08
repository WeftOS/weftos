//! [`ReleaseBackend`] over `weftos-store`'s [`ObjectBackend`] (ADR-126 S1), so a release store
//! lives in the RustFS bucket S1 maps its name to.
//!
//! The mapping is exact: `put_if_absent` is `put(.., if_none_match = true)` (412 is
//! `BackendError::Exists`), `get` and `list` are GetObject and ListObjectsV2, `delete` is DeleteObject.
//! S1's transport is blocking, so each call runs on tokio's blocking pool.

use std::sync::Arc;

use weftos_store::{BackendError as StoreErr, ObjectBackend, StoreName};

use crate::backend::{BackendError, DeletableBackend, PutOutcome, ReleaseBackend};
use crate::name::ReleaseStoreName;

/// One release store's bucket on an S1 object backend.
///
/// Hand CI an adapter built on a credential whose policy lacks DeleteObject and `tombstones/*` put
/// (ADR-126 section 3); the sweeper gets its own, wider one. Both use this same type.
pub struct StoreBackend<O: ObjectBackend + 'static> {
    objects: Arc<O>,
    bucket: String,
}

impl<O: ObjectBackend + 'static> StoreBackend<O> {
    /// Use an explicit bucket.
    pub fn new(objects: Arc<O>, bucket: impl Into<String>) -> Self {
        Self { objects, bucket: bucket.into() }
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
        let (o, b) = (self.objects.clone(), self.bucket.clone());
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
    async fn delete(&self, key: &str) -> Result<(), BackendError> {
        let k = key.to_string();
        match self.blocking(move |o, b| o.delete(b, &k)).await {
            Ok(()) | Err(StoreErr::NoObject(_)) => Ok(()),
            Err(e) => Err(err(e)),
        }
    }
}
