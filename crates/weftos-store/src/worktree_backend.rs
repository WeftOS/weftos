//! Lets the S2 worktree modules run on any S1 [`crate::backend::ObjectBackend`] (local directory,
//! RustFS, in-memory). S1's backends report no modification time, so `modified_ms` is 0; S2 does
//! not need it (promotion conflicts compare etags against a recorded base, see `overlay.rs`).

use crate::backend::{BackendError, ObjectBackend as S1Backend};
use crate::overlay::{ObjectBackend, ObjectMeta, Result, S2Error};

fn meta(m: crate::backend::ObjectMeta) -> ObjectMeta {
    ObjectMeta {
        key: m.key,
        size: m.size,
        etag: m.etag,
        modified_ms: 0,
    }
}

fn err(e: BackendError) -> S2Error {
    S2Error::Backend(e.to_string())
}

impl<T: S1Backend + ?Sized> ObjectBackend for T {
    fn head(&self, bucket: &str, key: &str) -> Result<Option<ObjectMeta>> {
        match S1Backend::head(self, bucket, key) {
            Ok(m) => Ok(Some(meta(m))),
            Err(BackendError::NoObject(_)) => Ok(None),
            Err(e) => Err(err(e)),
        }
    }

    fn get(&self, bucket: &str, key: &str) -> Result<Option<Vec<u8>>> {
        match S1Backend::get(self, bucket, key) {
            Ok(d) => Ok(Some(d)),
            Err(BackendError::NoObject(_)) => Ok(None),
            Err(e) => Err(err(e)),
        }
    }

    fn put(&self, bucket: &str, key: &str, data: &[u8]) -> Result<ObjectMeta> {
        S1Backend::put(self, bucket, key, data, false)
            .map(meta)
            .map_err(err)
    }

    fn put_cas(&self, bucket: &str, key: &str, data: &[u8], expect: Option<&str>) -> Result<bool> {
        let r = match expect {
            Some(etag) => S1Backend::put_if_match(self, bucket, key, data, etag),
            None => S1Backend::put(self, bucket, key, data, true),
        };
        match r {
            Ok(_) => Ok(true),
            // lost the race: the object changed, vanished (a CAS needs an existing key), or appeared
            Err(BackendError::Stale(_) | BackendError::NoObject(_) | BackendError::Exists(_)) => Ok(false),
            Err(e) => Err(err(e)),
        }
    }

    fn delete(&self, bucket: &str, key: &str) -> Result<()> {
        match S1Backend::delete(self, bucket, key) {
            Ok(()) | Err(BackendError::NoObject(_)) => Ok(()),
            Err(e) => Err(err(e)),
        }
    }

    fn list(&self, bucket: &str, prefix: &str) -> Result<Vec<ObjectMeta>> {
        S1Backend::list(self, bucket, prefix)
            .map(|v| v.into_iter().map(meta).collect())
            .map_err(err)
    }

    fn copy(&self, sb: &str, sk: &str, db: &str, dk: &str) -> Result<ObjectMeta> {
        S1Backend::copy(self, sb, sk, db, dk).map(meta).map_err(err)
    }
}
