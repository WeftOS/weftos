//! In-memory [`ObjectBackend`] for compartmentalized tests of the S2 paths. Not a store.

use crate::overlay::{ObjectBackend, ObjectMeta, Result};
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;

type Objects = BTreeMap<(String, String), (Vec<u8>, u64)>;

#[derive(Default)]
pub struct MemBackend {
    objects: RefCell<Objects>,
    clock: Cell<u64>,
}

impl MemBackend {
    pub fn new() -> Self {
        Self::default()
    }
    /// Set the clock used as `modified_ms` for later writes.
    pub fn set_clock(&self, ms: u64) {
        self.clock.set(ms);
    }
    fn etag(data: &[u8]) -> String {
        // FNV-1a: deterministic, good enough to tell test payloads apart.
        let mut h: u64 = 0xcbf29ce484222325;
        for b in data {
            h ^= *b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        format!("{h:016x}-{}", data.len())
    }
    fn meta(key: &str, data: &[u8], ms: u64) -> ObjectMeta {
        ObjectMeta {
            key: key.to_string(),
            size: data.len() as u64,
            etag: Self::etag(data),
            modified_ms: ms,
        }
    }
    pub fn len(&self, bucket: &str) -> usize {
        self.objects
            .borrow()
            .keys()
            .filter(|(b, _)| b == bucket)
            .count()
    }
    pub fn is_empty(&self, bucket: &str) -> bool {
        self.len(bucket) == 0
    }
}

impl ObjectBackend for MemBackend {
    fn head(&self, bucket: &str, key: &str) -> Result<Option<ObjectMeta>> {
        Ok(self
            .objects
            .borrow()
            .get(&(bucket.into(), key.into()))
            .map(|(d, ms)| Self::meta(key, d, *ms)))
    }
    fn get(&self, bucket: &str, key: &str) -> Result<Option<Vec<u8>>> {
        Ok(self
            .objects
            .borrow()
            .get(&(bucket.into(), key.into()))
            .map(|(d, _)| d.clone()))
    }
    fn put(&self, bucket: &str, key: &str, data: &[u8]) -> Result<ObjectMeta> {
        let ms = self.clock.get();
        self.objects
            .borrow_mut()
            .insert((bucket.into(), key.into()), (data.to_vec(), ms));
        Ok(Self::meta(key, data, ms))
    }
    fn delete(&self, bucket: &str, key: &str) -> Result<()> {
        self.objects
            .borrow_mut()
            .remove(&(bucket.into(), key.into()));
        Ok(())
    }
    fn list(&self, bucket: &str, prefix: &str) -> Result<Vec<ObjectMeta>> {
        Ok(self
            .objects
            .borrow()
            .iter()
            .filter(|((b, k), _)| b == bucket && k.starts_with(prefix))
            .map(|((_, k), (d, ms))| Self::meta(k, d, *ms))
            .collect())
    }
}

/// Calls and bytes seen by a [`CountingBackend`]. Every backend-trait method is its own kind:
/// `put_cas` is one call (the conditional create or replace as the real backend sees it), not a HEAD
/// plus a PUT, and `delete` is counted. The trait has no paged listing, so `list` counts one call per
/// `list()` (a real S3 listing of N objects is ceil(N / 1000) wire pages; that is not visible here).
/// `copy` is its own kind and is forwarded to the inner backend's `copy` (server-side on S1, as on
/// the wire), not the default get-then-put.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Counts {
    pub get: u64,
    pub head: u64,
    pub list: u64,
    pub put: u64,
    pub put_cas: u64,
    pub delete: u64,
    pub copy: u64,
    pub bytes_read: u64,
}

impl Counts {
    /// All backend calls, any kind.
    pub fn total(&self) -> u64 {
        self.get + self.head + self.list + self.put + self.put_cas + self.delete + self.copy
    }
}

/// A spy around any [`ObjectBackend`]: counts calls and bytes returned by `get`. Atomic counters,
/// so it also wraps a backend that is read from several threads. Test support; compiled always (the
/// module is not feature-gated) and not part of any product path.
pub struct CountingBackend<B> {
    pub inner: B,
    get: std::sync::atomic::AtomicU64,
    head: std::sync::atomic::AtomicU64,
    list: std::sync::atomic::AtomicU64,
    put: std::sync::atomic::AtomicU64,
    put_cas: std::sync::atomic::AtomicU64,
    delete: std::sync::atomic::AtomicU64,
    copy: std::sync::atomic::AtomicU64,
    bytes: std::sync::atomic::AtomicU64,
}

impl<B> CountingBackend<B> {
    pub fn new(inner: B) -> Self {
        let z = || std::sync::atomic::AtomicU64::new(0);
        Self { inner, get: z(), head: z(), list: z(), put: z(), put_cas: z(), delete: z(), copy: z(), bytes: z() }
    }
    pub fn counts(&self) -> Counts {
        use std::sync::atomic::Ordering::Relaxed;
        Counts {
            get: self.get.load(Relaxed),
            head: self.head.load(Relaxed),
            list: self.list.load(Relaxed),
            put: self.put.load(Relaxed),
            put_cas: self.put_cas.load(Relaxed),
            delete: self.delete.load(Relaxed),
            copy: self.copy.load(Relaxed),
            bytes_read: self.bytes.load(Relaxed),
        }
    }
    pub fn reset(&self) {
        use std::sync::atomic::Ordering::Relaxed;
        for a in [&self.get, &self.head, &self.list, &self.put, &self.put_cas, &self.delete, &self.copy, &self.bytes] {
            a.store(0, Relaxed);
        }
    }
}

impl<B: ObjectBackend> ObjectBackend for CountingBackend<B> {
    fn head(&self, bucket: &str, key: &str) -> Result<Option<ObjectMeta>> {
        self.head.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.inner.head(bucket, key)
    }
    fn get(&self, bucket: &str, key: &str) -> Result<Option<Vec<u8>>> {
        self.get.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let r = self.inner.get(bucket, key)?;
        self.bytes.fetch_add(r.as_ref().map_or(0, |d| d.len() as u64), std::sync::atomic::Ordering::Relaxed);
        Ok(r)
    }
    fn put(&self, bucket: &str, key: &str, data: &[u8]) -> Result<ObjectMeta> {
        self.put.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.inner.put(bucket, key, data)
    }
    fn delete(&self, bucket: &str, key: &str) -> Result<()> {
        self.delete.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.inner.delete(bucket, key)
    }
    fn list(&self, bucket: &str, prefix: &str) -> Result<Vec<ObjectMeta>> {
        self.list.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.inner.list(bucket, prefix)
    }
    fn put_cas(&self, bucket: &str, key: &str, data: &[u8], expect: Option<&str>) -> Result<bool> {
        self.put_cas.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.inner.put_cas(bucket, key, data, expect)
    }
    fn copy(&self, src_bucket: &str, src_key: &str, dst_bucket: &str, dst_key: &str) -> Result<ObjectMeta> {
        self.copy.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.inner.copy(src_bucket, src_key, dst_bucket, dst_key)
    }
}
