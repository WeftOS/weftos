//! An in-memory backend for tests. Besides the two backend traits it can answer
//! "would the backend itself refuse this key", which is how RustFS enforces a policy.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

use serde_json::Value;

use crate::backend::{AccessKey, AdminBackend, BackendError, ObjectAttrs, ObjectBackend, ObjectMeta, check_key, etag_of};

#[derive(Default)]
struct Bucket {
    tags: Vec<(String, String)>,
    objects: BTreeMap<String, Vec<u8>>,
    /// Keys whose current version was put executable.
    exec: BTreeSet<String>,
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

    /// The ids of the keys issued for a principal label.
    pub fn keys_of(&self, principal: &str) -> Vec<String> {
        self.lock().keys.iter().filter(|(_, k)| k.principal == principal).map(|(id, _)| id.clone()).collect()
    }

    /// Whether the key is enabled.
    pub fn key_enabled(&self, key_id: &str) -> Option<bool> {
        self.lock().keys.get(key_id).map(|k| k.enabled)
    }
}

fn set_exec(b: &mut Bucket, key: &str, attrs: ObjectAttrs) {
    if attrs.executable {
        b.exec.insert(key.to_owned());
    } else {
        b.exec.remove(key);
    }
}

fn meta(key: &str, data: &[u8]) -> ObjectMeta {
    ObjectMeta { key: key.to_owned(), size: data.len() as u64, etag: etag_of(data) }
}

impl ObjectBackend for MemBackend {
    fn put(&self, bucket: &str, key: &str, data: &[u8], if_none_match: bool) -> Result<ObjectMeta, BackendError> {
        self.put_attrs(bucket, key, data, if_none_match, ObjectAttrs::default())
    }

    fn put_attrs(&self, bucket: &str, key: &str, data: &[u8], if_none_match: bool, attrs: ObjectAttrs) -> Result<ObjectMeta, BackendError> {
        check_key(key)?;
        let mut g = self.lock();
        let b = g.buckets.get_mut(bucket).ok_or_else(|| BackendError::NoBucket(bucket.into()))?;
        if if_none_match && b.objects.contains_key(key) {
            return Err(BackendError::Exists(key.into()));
        }
        b.objects.insert(key.to_owned(), data.to_vec());
        set_exec(b, key, attrs);
        Ok(meta(key, data))
    }

    fn put_if_match(&self, bucket: &str, key: &str, data: &[u8], etag: &str) -> Result<ObjectMeta, BackendError> {
        self.put_if_match_attrs(bucket, key, data, etag, ObjectAttrs::default())
    }

    fn attrs(&self, bucket: &str, key: &str) -> Result<ObjectAttrs, BackendError> {
        let g = self.lock();
        let b = g.buckets.get(bucket).ok_or_else(|| BackendError::NoBucket(bucket.into()))?;
        if !b.objects.contains_key(key) {
            return Err(BackendError::NoObject(key.into()));
        }
        Ok(ObjectAttrs { executable: b.exec.contains(key) })
    }

    fn put_if_match_attrs(&self, bucket: &str, key: &str, data: &[u8], etag: &str, attrs: ObjectAttrs) -> Result<ObjectMeta, BackendError> {
        check_key(key)?;
        let mut g = self.lock();
        let b = g.buckets.get_mut(bucket).ok_or_else(|| BackendError::NoBucket(bucket.into()))?;
        match b.objects.get(key) {
            None => return Err(BackendError::NoObject(key.into())),
            Some(cur) if etag_of(cur) != etag.trim_matches('"') => return Err(BackendError::Stale(key.into())),
            Some(_) => {}
        }
        b.objects.insert(key.to_owned(), data.to_vec());
        set_exec(b, key, attrs);
        Ok(meta(key, data))
    }

    fn delete_if_match(&self, bucket: &str, key: &str, etag: &str) -> Result<(), BackendError> {
        let mut g = self.lock();
        let b = g.buckets.get_mut(bucket).ok_or_else(|| BackendError::NoBucket(bucket.into()))?;
        match b.objects.get(key) {
            None => return Err(BackendError::NoObject(key.into())),
            Some(cur) if etag_of(cur) != etag.trim_matches('"') => return Err(BackendError::Stale(key.into())),
            Some(_) => {}
        }
        b.objects.remove(key);
        b.exec.remove(key);
        Ok(())
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
        b.exec.remove(key);
        b.objects.remove(key).map(|_| ()).ok_or_else(|| BackendError::NoObject(key.into()))
    }

    fn copy(&self, sb: &str, sk: &str, db: &str, dk: &str) -> Result<ObjectMeta, BackendError> {
        let data = self.get(sb, sk)?;
        let attrs = self.attrs(sb, sk)?;
        self.put_attrs(db, dk, &data, false, attrs)
    }

    fn list(&self, bucket: &str, prefix: &str) -> Result<Vec<ObjectMeta>, BackendError> {
        let g = self.lock();
        let b = g.buckets.get(bucket).ok_or_else(|| BackendError::NoBucket(bucket.into()))?;
        Ok(b.objects.iter().filter(|(k, _)| k.starts_with(prefix)).map(|(k, v)| meta(k, v)).collect())
    }

    /// The stamp is the execute attribute: it moves exactly when a mode-only change lands.
    fn list_stamped(&self, bucket: &str, prefix: &str) -> Result<Vec<(ObjectMeta, String)>, BackendError> {
        let g = self.lock();
        let b = g.buckets.get(bucket).ok_or_else(|| BackendError::NoBucket(bucket.into()))?;
        Ok(b.objects.iter().filter(|(k, _)| k.starts_with(prefix)).map(|(k, v)| (meta(k, v), if b.exec.contains(k) { "x" } else { "-" }.to_owned())).collect())
    }
}

impl AdminBackend for MemBackend {
    fn create_bucket(&self, bucket: &str, tags: &[(String, String)]) -> Result<(), BackendError> {
        let mut g = self.lock();
        if g.buckets.contains_key(bucket) {
            return Err(BackendError::BucketExists(bucket.into()));
        }
        g.buckets.insert(bucket.to_owned(), Bucket { tags: tags.to_vec(), objects: BTreeMap::new(), exec: BTreeSet::new() });
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_executable_attribute_follows_the_object() {
        let m = MemBackend::new();
        m.create_bucket("b-mem", &[]).unwrap();
        let x = ObjectAttrs { executable: true };
        m.put("b-mem", "plain", b"p", false).unwrap();
        assert_eq!(m.attrs("b-mem", "plain").unwrap(), ObjectAttrs::default());
        let v1 = m.put_attrs("b-mem", "run", b"r", true, x).unwrap();
        assert!(m.attrs("b-mem", "run").unwrap().executable);
        m.copy("b-mem", "run", "b-mem", "run2").unwrap();
        assert!(m.attrs("b-mem", "run2").unwrap().executable);
        assert!(matches!(m.put_attrs("b-mem", "run", b"x", true, ObjectAttrs::default()), Err(BackendError::Exists(_))), "a refused put changes nothing");
        assert!(m.attrs("b-mem", "run").unwrap().executable);
        m.put_if_match_attrs("b-mem", "run", b"r2", &v1.etag, ObjectAttrs::default()).unwrap();
        assert!(!m.attrs("b-mem", "run").unwrap().executable, "a replace without the attribute clears it");
        m.delete("b-mem", "run2").unwrap();
        m.put("b-mem", "run2", b"fresh", false).unwrap();
        assert!(!m.attrs("b-mem", "run2").unwrap().executable, "a deleted key does not keep it");
        assert!(matches!(m.attrs("b-mem", "none"), Err(BackendError::NoObject(_))));
    }

    #[test]
    fn a_streamed_put_keeps_the_execute_attribute_on_the_local_fallbacks_and_lists_a_stamp_that_follows_it() {
        use crate::backend::{MIN_PART, PutOpts};
        let m = MemBackend::new();
        m.create_bucket("b-mem", &[]).unwrap();
        let x = PutOpts { attrs: ObjectAttrs { executable: true }, ..Default::default() };
        // One part: put_locked is unsupported here, so the plain put path carries the attribute.
        m.put_stream("b-mem", "small", &mut std::io::Cursor::new(b"s".to_vec()), MIN_PART, &x).unwrap();
        assert!(m.attrs("b-mem", "small").unwrap().executable, "single-part stream");
        // More than one part: multipart is unsupported here, so the whole-body fallback does.
        m.put_stream("b-mem", "big", &mut std::io::Cursor::new(vec![7u8; MIN_PART + 1]), MIN_PART, &x).unwrap();
        assert!(m.attrs("b-mem", "big").unwrap().executable, "whole-body fallback");
        m.put_stream("b-mem", "plain", &mut std::io::Cursor::new(vec![7u8; MIN_PART + 1]), MIN_PART, &PutOpts::default()).unwrap();
        assert!(!m.attrs("b-mem", "plain").unwrap().executable);
        let st = |k: &str| m.list_stamped("b-mem", "").unwrap().into_iter().find(|(o, _)| o.key == k).unwrap().1;
        let (a, b) = (st("small"), st("plain"));
        assert_ne!(a, b, "the stamp differs between an executable and a plain object");
    }
}
