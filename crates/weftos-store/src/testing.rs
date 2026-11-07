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
