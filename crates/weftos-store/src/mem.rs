//! An in-memory backend for tests. Besides the two backend traits it can answer
//! "would the backend itself refuse this key", which is how RustFS enforces a policy.

use std::collections::BTreeMap;
use std::sync::Mutex;

use serde_json::Value;

use crate::backend::{AccessKey, AdminBackend, BackendError, ObjectBackend, ObjectMeta, check_key, etag_of};

#[derive(Default)]
struct Bucket {
    tags: Vec<(String, String)>,
    objects: BTreeMap<String, Vec<u8>>,
}

struct KeyState {
    principal: String,
    enabled: bool,
    policy: Option<Value>,
}

#[derive(Default)]
struct State {
    buckets: BTreeMap<String, Bucket>,
    keys: BTreeMap<String, KeyState>,
}

/// In-memory buckets, keys and policies.
#[derive(Default)]
pub struct MemBackend {
    st: Mutex<State>,
}

impl MemBackend {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// What the backend does with a request signed by `key_id`: refused (403) if the key is
    /// unknown or disabled, or its policy has no statement allowing `action` on `bucket`.
    pub fn authorize(&self, key_id: &str, bucket: &str, action: &str) -> Result<(), BackendError> {
        let g = self.lock();
        let k = g.keys.get(key_id).ok_or_else(|| BackendError::Forbidden(format!("unknown access key {key_id}")))?;
        if !k.enabled {
            return Err(BackendError::Forbidden(format!("access key {key_id} is disabled")));
        }
        let arn = format!("arn:aws:s3:::{bucket}");
        let ok = k.policy.as_ref().and_then(|p| p["Statement"].as_array()).is_some_and(|st| {
            st.iter().any(|s| {
                s["Effect"] == "Allow"
                    && s["Action"].as_array().is_some_and(|a| a.iter().any(|x| x == action))
                    && s["Resource"].as_array().is_some_and(|r| r.iter().any(|x| x.as_str().is_some_and(|x| x == arn || x.starts_with(&format!("{arn}/")))))
            })
        });
        if ok { Ok(()) } else { Err(BackendError::Forbidden(format!("access key {key_id}: {action} on {bucket} is not allowed by its policy"))) }
    }

    /// The policy currently attached to a key.
    pub fn policy_of(&self, key_id: &str) -> Option<Value> {
        self.lock().keys.get(key_id).and_then(|k| k.policy.clone())
    }

    /// True if `key` is not in `bucket` (tests).
    pub fn get_missing_check(&self, bucket: &str, key: &str) -> bool {
        self.get(bucket, key).is_err()
    }

    /// Whether the key is enabled.
    pub fn key_enabled(&self, key_id: &str) -> Option<bool> {
        self.lock().keys.get(key_id).map(|k| k.enabled)
    }
}

fn meta(key: &str, data: &[u8]) -> ObjectMeta {
    ObjectMeta { key: key.to_owned(), size: data.len() as u64, etag: etag_of(data) }
}

impl ObjectBackend for MemBackend {
    fn put(&self, bucket: &str, key: &str, data: &[u8], if_none_match: bool) -> Result<ObjectMeta, BackendError> {
        check_key(key)?;
        let mut g = self.lock();
        let b = g.buckets.get_mut(bucket).ok_or_else(|| BackendError::NoBucket(bucket.into()))?;
        if if_none_match && b.objects.contains_key(key) {
            return Err(BackendError::Exists(key.into()));
        }
        b.objects.insert(key.to_owned(), data.to_vec());
        Ok(meta(key, data))
    }

    fn get(&self, bucket: &str, key: &str) -> Result<Vec<u8>, BackendError> {
        let g = self.lock();
        let b = g.buckets.get(bucket).ok_or_else(|| BackendError::NoBucket(bucket.into()))?;
        b.objects.get(key).cloned().ok_or_else(|| BackendError::NoObject(key.into()))
    }

    fn head(&self, bucket: &str, key: &str) -> Result<ObjectMeta, BackendError> {
        self.get(bucket, key).map(|d| meta(key, &d))
    }

    fn delete(&self, bucket: &str, key: &str) -> Result<(), BackendError> {
        let mut g = self.lock();
        let b = g.buckets.get_mut(bucket).ok_or_else(|| BackendError::NoBucket(bucket.into()))?;
        b.objects.remove(key).map(|_| ()).ok_or_else(|| BackendError::NoObject(key.into()))
    }

    fn copy(&self, sb: &str, sk: &str, db: &str, dk: &str) -> Result<ObjectMeta, BackendError> {
        let data = self.get(sb, sk)?;
        self.put(db, dk, &data, false)
    }

    fn list(&self, bucket: &str, prefix: &str) -> Result<Vec<ObjectMeta>, BackendError> {
        let g = self.lock();
        let b = g.buckets.get(bucket).ok_or_else(|| BackendError::NoBucket(bucket.into()))?;
        Ok(b.objects.iter().filter(|(k, _)| k.starts_with(prefix)).map(|(k, v)| meta(k, v)).collect())
    }
}

impl AdminBackend for MemBackend {
    fn create_bucket(&self, bucket: &str, tags: &[(String, String)]) -> Result<(), BackendError> {
        let mut g = self.lock();
        if g.buckets.contains_key(bucket) {
            return Err(BackendError::BucketExists(bucket.into()));
        }
        g.buckets.insert(bucket.to_owned(), Bucket { tags: tags.to_vec(), objects: BTreeMap::new() });
        Ok(())
    }

    fn delete_bucket(&self, bucket: &str) -> Result<(), BackendError> {
        let mut g = self.lock();
        match g.buckets.get(bucket) {
            None => Err(BackendError::NoBucket(bucket.into())),
            Some(b) if !b.objects.is_empty() => Err(BackendError::Io(format!("bucket {bucket} is not empty"))),
            Some(_) => {
                g.buckets.remove(bucket);
                Ok(())
            }
        }
    }

    fn bucket_exists(&self, bucket: &str) -> Result<bool, BackendError> {
        Ok(self.lock().buckets.contains_key(bucket))
    }

    fn buckets(&self) -> Result<Vec<crate::backend::BucketTags>, BackendError> {
        Ok(self.lock().buckets.iter().map(|(n, b)| (n.clone(), b.tags.clone())).collect())
    }

    fn create_access_key(&self, principal: &str) -> Result<AccessKey, BackendError> {
        let id = format!("AK{}", &blake3::hash(principal.as_bytes()).to_hex()[..18]).to_uppercase();
        self.lock().keys.entry(id.clone()).or_insert_with(|| KeyState { principal: principal.to_owned(), enabled: true, policy: None });
        Ok(AccessKey { id })
    }

    fn set_key_enabled(&self, key_id: &str, enabled: bool) -> Result<(), BackendError> {
        let mut g = self.lock();
        let k = g.keys.get_mut(key_id).ok_or_else(|| BackendError::Forbidden(format!("unknown access key {key_id}")))?;
        k.enabled = enabled;
        let _ = &k.principal;
        Ok(())
    }

    fn put_policy(&self, key_id: &str, policy: &Value) -> Result<(), BackendError> {
        let mut g = self.lock();
        let k = g.keys.get_mut(key_id).ok_or_else(|| BackendError::Forbidden(format!("unknown access key {key_id}")))?;
        k.policy = Some(policy.clone());
        Ok(())
    }

    fn delete_access_key(&self, key_id: &str) -> Result<(), BackendError> {
        self.lock().keys.remove(key_id);
        Ok(())
    }
}
