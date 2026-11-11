//! A backend that keeps buckets in a local directory. It is what the daemon uses when no
//! RustFS is configured: the same two traits, and the daemon is still the only door
//! (nothing listens), so the governed path is the enforcement.
//!
//! ```text
//! <root>/buckets/<bucket>/meta.json   tags
//! <root>/buckets/<bucket>/data/...    objects (one file per key)
//! <root>/buckets/<bucket>/tmp/        staging for atomic puts
//! <root>/keys.json                    access keys, enabled flag, policy
//! ```
//!
//! Limitations: a key that is a prefix of another (`a` and `a/b`) cannot coexist, because a
//! path is either a file or a directory. Keys that differ only by case (`A.txt`, `a.txt`) cannot
//! coexist either: they are one file on a case-insensitive volume, so a put of the second is
//! refused on every host and a get or delete of the second finds nothing.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::StoreError;
use crate::backend::{AccessKey, AdminBackend, BackendError, ObjectAttrs, ObjectBackend, ObjectMeta, check_bucket, check_key, etag_of};

static TMP: AtomicU64 = AtomicU64::new(0);

/// Write `bytes` to `path` through a temp file in the same directory, then rename.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    let err = |e: std::io::Error| StoreError::State(format!("{}: {e}", path.display()));
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d).map_err(err)?;
    }
    let tmp = path.with_extension(format!("tmp-{}-{}", std::process::id(), TMP.fetch_add(1, Ordering::Relaxed)));
    let mut f = std::fs::File::create(&tmp).map_err(err)?;
    f.write_all(bytes).and_then(|()| f.sync_all()).map_err(err)?;
    std::fs::rename(&tmp, path).map_err(err)
}

#[derive(Serialize, Deserialize, Default, Clone)]
struct KeyRec {
    principal: String,
    enabled: bool,
    policy: Option<Value>,
}

#[derive(Serialize, Deserialize, Default)]
struct Keys {
    keys: BTreeMap<String, KeyRec>,
}

/// Buckets as directories.
pub struct FsBackend {
    root: PathBuf,
    keys: Mutex<Keys>,
    /// Serialises writes in this process so a conditional put's compare and rename cannot interleave
    /// with another write. Another process on the same directory is not covered.
    wlock: Mutex<()>,
}

fn io(e: impl std::fmt::Display) -> BackendError {
    BackendError::Io(e.to_string())
}

impl FsBackend {
    /// Open (creating) the store under `root`. A broken `keys.json` is an error.
    pub fn open(root: &Path) -> Result<Self, StoreError> {
        std::fs::create_dir_all(root.join("buckets")).map_err(|e| StoreError::State(format!("{}: {e}", root.display())))?;
        let kp = root.join("keys.json");
        let keys = match std::fs::read(&kp) {
            Ok(b) => serde_json::from_slice(&b).map_err(|e| StoreError::State(format!("{}: {e}", kp.display())))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Keys::default(),
            Err(e) => return Err(StoreError::State(format!("{}: {e}", kp.display()))),
        };
        Ok(Self { root: root.to_owned(), keys: Mutex::new(keys), wlock: Mutex::new(()) })
    }

    fn bdir(&self, bucket: &str) -> Result<PathBuf, BackendError> {
        check_bucket(bucket)?;
        let d = self.root.join("buckets").join(bucket);
        if d.is_dir() { Ok(d) } else { Err(BackendError::NoBucket(bucket.into())) }
    }

    fn obj_path(&self, bucket: &str, key: &str) -> Result<PathBuf, BackendError> {
        check_key(key)?;
        let mut p = self.bdir(bucket)?.join("data");
        p.extend(key.split('/'));
        Ok(p)
    }

    /// The object's path, or `None` when an existing key differs from it only by case.
    fn path_unless_case_twin(&self, bucket: &str, key: &str) -> Result<Option<PathBuf>, BackendError> {
        let p = self.obj_path(bucket, key)?;
        let base = self.bdir(bucket)?.join("data");
        Ok(case_twin(&base, key)?.is_none().then_some(p))
    }

    fn save_keys(&self, k: &Keys) -> Result<(), BackendError> {
        write_atomic(&self.root.join("keys.json"), &serde_json::to_vec_pretty(k).map_err(io)?).map_err(io)
    }

    fn with_keys<T>(&self, f: impl FnOnce(&mut Keys) -> Result<T, BackendError>) -> Result<T, BackendError> {
        let mut g = self.keys.lock().unwrap_or_else(|e| e.into_inner());
        let r = f(&mut g)?;
        self.save_keys(&g)?;
        Ok(r)
    }

    /// Whether the key exists and is enabled (for tests and `weaver store ls`).
    pub fn key_enabled(&self, key_id: &str) -> Option<bool> {
        self.keys.lock().unwrap_or_else(|e| e.into_inner()).keys.get(key_id).map(|k| k.enabled)
    }

    /// The policy attached to a key.
    pub fn policy_of(&self, key_id: &str) -> Option<Value> {
        self.keys.lock().unwrap_or_else(|e| e.into_inner()).keys.get(key_id).and_then(|k| k.policy.clone())
    }
}

/// An existing path under `base` that spells some segment of `key` differently only by case
/// (review F20). On a case-insensitive volume, such as the default macOS one, that existing path is
/// the same file as `key`; on a case-sensitive one it is a different file. Either way the two keys
/// are refused so that a store reads the same on every host. Names that differ only by Unicode
/// normalisation are not detected.
fn case_twin(base: &Path, key: &str) -> Result<Option<String>, BackendError> {
    let mut dir = base.to_path_buf();
    for seg in key.split('/') {
        let (mut exact, mut twin) = (false, None);
        match std::fs::read_dir(&dir) {
            Ok(rd) => {
                for ent in rd {
                    let name = ent.map_err(io)?.file_name().to_string_lossy().into_owned();
                    if name == seg {
                        exact = true;
                    } else if name.to_lowercase() == seg.to_lowercase() {
                        twin = Some(name);
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound || e.kind() == std::io::ErrorKind::NotADirectory => return Ok(None),
            Err(e) => return Err(io(e)),
        }
        match (exact, twin) {
            (true, _) => dir.push(seg),
            (false, Some(t)) => return Ok(Some(t)),
            (false, None) => return Ok(None),
        }
    }
    Ok(None)
}

fn walk(dir: &Path, rel: &str, out: &mut Vec<ObjectMeta>) -> Result<(), BackendError> {
    let rd = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(io(e)),
    };
    for ent in rd {
        let ent = ent.map_err(io)?;
        let name = ent.file_name().to_string_lossy().into_owned();
        let key = if rel.is_empty() { name } else { format!("{rel}/{name}") };
        let ft = ent.file_type().map_err(io)?;
        if ft.is_dir() {
            walk(&ent.path(), &key, out)?;
        } else if ft.is_file() {
            let data = std::fs::read(ent.path()).map_err(io)?;
            out.push(ObjectMeta { key, size: data.len() as u64, etag: etag_of(&data) });
        }
    }
    Ok(())
}

impl FsBackend {
    fn wguard(&self) -> std::sync::MutexGuard<'_, ()> {
        self.wlock.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The executable attribute is the data file's own execute bits (0755 when set; otherwise the
    /// default mode, as before attributes existed). A put makes a new file, so it replaces the old attribute.
    fn put_unlocked(&self, bucket: &str, key: &str, data: &[u8], if_none_match: bool, attrs: ObjectAttrs) -> Result<ObjectMeta, BackendError> {
        let Some(dst) = self.path_unless_case_twin(bucket, key)? else {
            return Err(BackendError::BadKey(format!("{key:?}: differs only by case from an existing key in {bucket}")));
        };
        let tmpdir = self.bdir(bucket)?.join("tmp");
        std::fs::create_dir_all(&tmpdir).map_err(io)?;
        if let Some(p) = dst.parent() {
            std::fs::create_dir_all(p).map_err(io)?;
        }
        let tmp = tmpdir.join(format!("{}-{}", std::process::id(), TMP.fetch_add(1, Ordering::Relaxed)));
        let mut f = std::fs::File::create(&tmp).map_err(io)?;
        f.write_all(data).and_then(|()| f.sync_all()).map_err(io)?;
        if attrs.executable {
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(std::fs::Permissions::from_mode(0o755)).map_err(io)?;
        }
        let res = if if_none_match {
            // A hard link fails if the target exists: an atomic create-if-absent.
            let r = std::fs::hard_link(&tmp, &dst);
            let _ = std::fs::remove_file(&tmp);
            r.map_err(|e| if e.kind() == std::io::ErrorKind::AlreadyExists { BackendError::Exists(key.into()) } else { io(e) })
        } else {
            std::fs::rename(&tmp, &dst).map_err(io)
        };
        if res.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        res?;
        Ok(ObjectMeta { key: key.to_owned(), size: data.len() as u64, etag: etag_of(data) })
    }

    fn delete_unlocked(&self, bucket: &str, key: &str) -> Result<(), BackendError> {
        let Some(p) = self.path_unless_case_twin(bucket, key)? else { return Err(BackendError::NoObject(key.into())) };
        std::fs::remove_file(&p).map_err(|e| if e.kind() == std::io::ErrorKind::NotFound { BackendError::NoObject(key.into()) } else { io(e) })?;
        // Prune now-empty parent directories up to the bucket's data dir.
        let stop = self.bdir(bucket)?.join("data");
        let mut d = p.parent().map(Path::to_path_buf);
        while let Some(dir) = d {
            if dir == stop || std::fs::remove_dir(&dir).is_err() {
                break;
            }
            d = dir.parent().map(Path::to_path_buf);
        }
        Ok(())
    }

    fn check_etag(&self, bucket: &str, key: &str, etag: &str) -> Result<(), BackendError> {
        if self.head(bucket, key)?.etag == etag.trim_matches('"') { Ok(()) } else { Err(BackendError::Stale(key.into())) }
    }
}

impl ObjectBackend for FsBackend {
    fn put(&self, bucket: &str, key: &str, data: &[u8], if_none_match: bool) -> Result<ObjectMeta, BackendError> {
        self.put_attrs(bucket, key, data, if_none_match, ObjectAttrs::default())
    }

    fn put_attrs(&self, bucket: &str, key: &str, data: &[u8], if_none_match: bool, attrs: ObjectAttrs) -> Result<ObjectMeta, BackendError> {
        let _g = self.wguard();
        self.put_unlocked(bucket, key, data, if_none_match, attrs)
    }

    fn put_if_match(&self, bucket: &str, key: &str, data: &[u8], etag: &str) -> Result<ObjectMeta, BackendError> {
        self.put_if_match_attrs(bucket, key, data, etag, ObjectAttrs::default())
    }

    fn put_if_match_attrs(&self, bucket: &str, key: &str, data: &[u8], etag: &str, attrs: ObjectAttrs) -> Result<ObjectMeta, BackendError> {
        let _g = self.wguard();
        self.check_etag(bucket, key, etag)?;
        self.put_unlocked(bucket, key, data, false, attrs)
    }

    fn attrs(&self, bucket: &str, key: &str) -> Result<ObjectAttrs, BackendError> {
        use std::os::unix::fs::PermissionsExt;
        let Some(p) = self.path_unless_case_twin(bucket, key)? else { return Err(BackendError::NoObject(key.into())) };
        match std::fs::metadata(&p) {
            Ok(m) if m.is_file() => Ok(ObjectAttrs { executable: m.permissions().mode() & 0o111 != 0 }),
            Ok(_) => Err(BackendError::NoObject(key.into())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(BackendError::NoObject(key.into())),
            Err(e) => Err(io(e)),
        }
    }

    fn delete_if_match(&self, bucket: &str, key: &str, etag: &str) -> Result<(), BackendError> {
        let _g = self.wguard();
        self.check_etag(bucket, key, etag)?;
        self.delete_unlocked(bucket, key)
    }

    fn get(&self, bucket: &str, key: &str) -> Result<Vec<u8>, BackendError> {
        let Some(p) = self.path_unless_case_twin(bucket, key)? else { return Err(BackendError::NoObject(key.into())) };
        std::fs::read(&p).map_err(|e| if e.kind() == std::io::ErrorKind::NotFound || p.is_dir() { BackendError::NoObject(key.into()) } else { io(e) })
    }

    fn head(&self, bucket: &str, key: &str) -> Result<ObjectMeta, BackendError> {
        self.get(bucket, key).map(|d| ObjectMeta { key: key.to_owned(), size: d.len() as u64, etag: etag_of(&d) })
    }

    fn delete(&self, bucket: &str, key: &str) -> Result<(), BackendError> {
        let _g = self.wguard();
        self.delete_unlocked(bucket, key)
    }

    fn copy(&self, sb: &str, sk: &str, db: &str, dk: &str) -> Result<ObjectMeta, BackendError> {
        let data = self.get(sb, sk)?;
        let attrs = self.attrs(sb, sk)?;
        self.put_attrs(db, dk, &data, false, attrs)
    }

    fn list(&self, bucket: &str, prefix: &str) -> Result<Vec<ObjectMeta>, BackendError> {
        let data = self.bdir(bucket)?.join("data");
        let mut out = Vec::new();
        walk(&data, "", &mut out)?;
        out.retain(|m| m.key.starts_with(prefix));
        out.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(out)
    }

    /// The stamp is the data file's mtime (ns) and its execute attribute: a put makes a new file,
    /// so it moves on a mode-only put too.
    fn list_stamped(&self, bucket: &str, prefix: &str) -> Result<Vec<(ObjectMeta, String)>, BackendError> {
        use std::os::unix::fs::PermissionsExt;
        let data = self.bdir(bucket)?.join("data");
        let mut out = Vec::new();
        for m in self.list(bucket, prefix)? {
            let mut p = data.clone();
            p.extend(m.key.split('/'));
            let stamp = match std::fs::metadata(&p) {
                Ok(md) => {
                    let ns = md.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos());
                    format!("{ns}:{}", md.permissions().mode() & 0o111 != 0)
                }
                Err(_) => String::new(),
            };
            out.push((m, stamp));
        }
        Ok(out)
    }
}

impl AdminBackend for FsBackend {
    fn create_bucket(&self, bucket: &str, tags: &[(String, String)]) -> Result<(), BackendError> {
        check_bucket(bucket)?;
        let d = self.root.join("buckets").join(bucket);
        match std::fs::create_dir(&d) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Err(BackendError::BucketExists(bucket.into())),
            Err(e) => return Err(io(e)),
        }
        std::fs::create_dir_all(d.join("data")).map_err(io)?;
        write_atomic(&d.join("meta.json"), &serde_json::to_vec_pretty(tags).map_err(io)?).map_err(io)
    }

    fn delete_bucket(&self, bucket: &str) -> Result<(), BackendError> {
        let d = self.bdir(bucket)?;
        let mut objs = Vec::new();
        walk(&d.join("data"), "", &mut objs)?;
        if !objs.is_empty() {
            return Err(BackendError::Io(format!("bucket {bucket} is not empty")));
        }
        std::fs::remove_dir_all(&d).map_err(io)
    }

    fn bucket_exists(&self, bucket: &str) -> Result<bool, BackendError> {
        check_bucket(bucket)?;
        Ok(self.root.join("buckets").join(bucket).is_dir())
    }

    fn buckets(&self) -> Result<Vec<crate::backend::BucketTags>, BackendError> {
        let mut out = Vec::new();
        for ent in std::fs::read_dir(self.root.join("buckets")).map_err(io)? {
            let ent = ent.map_err(io)?;
            let tags = std::fs::read(ent.path().join("meta.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
            out.push((ent.file_name().to_string_lossy().into_owned(), tags));
        }
        out.sort();
        Ok(out)
    }

    fn create_access_key(&self, principal: &str) -> Result<AccessKey, BackendError> {
        let id = format!("AK{}", &blake3::hash(principal.as_bytes()).to_hex()[..18]).to_uppercase();
        self.with_keys(|k| {
            k.keys.entry(id.clone()).or_insert_with(|| KeyRec { principal: principal.to_owned(), enabled: true, policy: None });
            Ok(())
        })?;
        Ok(AccessKey { id })
    }

    fn set_key_enabled(&self, key_id: &str, enabled: bool) -> Result<(), BackendError> {
        self.with_keys(|k| {
            k.keys.get_mut(key_id).ok_or_else(|| BackendError::Forbidden(format!("unknown access key {key_id}")))?.enabled = enabled;
            Ok(())
        })
    }

    fn put_policy(&self, key_id: &str, policy: &Value) -> Result<(), BackendError> {
        self.with_keys(|k| {
            k.keys.get_mut(key_id).ok_or_else(|| BackendError::Forbidden(format!("unknown access key {key_id}")))?.policy = Some(policy.clone());
            Ok(())
        })
    }

    fn delete_access_key(&self, key_id: &str) -> Result<(), BackendError> {
        self.with_keys(|k| {
            k.keys.remove(key_id);
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn be() -> (tempfile::TempDir, FsBackend) {
        let d = tempfile::tempdir().unwrap();
        let b = FsBackend::open(d.path()).unwrap();
        (d, b)
    }

    #[test]
    fn put_get_list_copy_delete_and_conditional_put() {
        let (_d, b) = be();
        b.create_bucket("n-one", &[("kind".into(), "node".into())]).unwrap();
        b.create_bucket("p-two", &[]).unwrap();
        assert!(matches!(b.create_bucket("n-one", &[]), Err(BackendError::BucketExists(_))));
        b.put("n-one", "a/b.txt", b"hello", false).unwrap();
        assert_eq!(b.get("n-one", "a/b.txt").unwrap(), b"hello");
        assert!(matches!(b.put("n-one", "a/b.txt", b"x", true), Err(BackendError::Exists(_))), "If-None-Match: * is 412 on an existing key");
        assert_eq!(b.get("n-one", "a/b.txt").unwrap(), b"hello", "the refused put changed nothing");
        b.put("n-one", "c.txt", b"x", true).unwrap();
        let l = b.list("n-one", "a/").unwrap();
        assert_eq!(l.len(), 1);
        assert_eq!(l[0].size, 5);
        b.copy("n-one", "a/b.txt", "p-two", "copied").unwrap();
        assert_eq!(b.get("p-two", "copied").unwrap(), b"hello");
        b.delete("n-one", "a/b.txt").unwrap();
        assert!(matches!(b.get("n-one", "a/b.txt"), Err(BackendError::NoObject(_))));
        assert!(b.list("n-one", "").unwrap().iter().all(|m| m.key == "c.txt"));
        assert!(b.delete_bucket("p-two").is_err(), "not empty");
        b.delete("p-two", "copied").unwrap();
        b.delete_bucket("p-two").unwrap();
        assert_eq!(b.buckets().unwrap().len(), 1);
    }

    #[test]
    fn the_executable_attribute_is_the_data_files_execute_bits() {
        use std::os::unix::fs::PermissionsExt;
        let (d, b) = be();
        b.create_bucket("n-one", &[]).unwrap();
        let x = ObjectAttrs { executable: true };
        b.put("n-one", "plain", b"p", false).unwrap();
        assert_eq!(b.attrs("n-one", "plain").unwrap(), ObjectAttrs::default(), "a plain put has no attribute");
        let v1 = b.put_attrs("n-one", "bin/run", b"#!/bin/sh\n", true, x).unwrap();
        assert!(b.attrs("n-one", "bin/run").unwrap().executable);
        assert_eq!(b.get("n-one", "bin/run").unwrap(), b"#!/bin/sh\n");
        let on_disk = std::fs::metadata(d.path().join("buckets/n-one/data/bin/run")).unwrap().permissions().mode() & 0o777;
        assert_eq!(on_disk, 0o755);
        // A copy keeps it; a replace without it clears it; a conditional replace sets it.
        b.copy("n-one", "bin/run", "n-one", "bin/run2").unwrap();
        assert!(b.attrs("n-one", "bin/run2").unwrap().executable);
        let v2 = b.put_if_match_attrs("n-one", "bin/run", b"#!/bin/sh\necho\n", &v1.etag, ObjectAttrs::default()).unwrap();
        assert!(!b.attrs("n-one", "bin/run").unwrap().executable);
        b.put_if_match_attrs("n-one", "bin/run", b"#!/bin/sh\n", &v2.etag, x).unwrap();
        assert!(b.attrs("n-one", "bin/run").unwrap().executable);
        assert!(matches!(b.attrs("n-one", "nope"), Err(BackendError::NoObject(_))));
        // Listing and etags are unaffected by the mode.
        assert_eq!(b.list("n-one", "bin/").unwrap().len(), 2);
    }

    #[test]
    fn put_if_match_swaps_only_the_object_that_was_read() {
        let (_d, b) = be();
        b.create_bucket("n-one", &[]).unwrap();
        assert!(matches!(b.put_if_match("n-one", "k", b"x", "e"), Err(BackendError::NoObject(_))), "a missing key is not a match");
        let v1 = b.put("n-one", "k", b"one", false).unwrap();
        let v2 = b.put_if_match("n-one", "k", b"two", &v1.etag).unwrap();
        assert_ne!(v1.etag, v2.etag);
        assert!(matches!(b.put_if_match("n-one", "k", b"three", &v1.etag), Err(BackendError::Stale(_))), "the old etag lost the race");
        assert_eq!(b.get("n-one", "k").unwrap(), b"two", "the refused swap changed nothing");
        b.put_if_match("n-one", "k", b"three", &format!("\"{}\"", v2.etag)).unwrap();
        assert_eq!(b.get("n-one", "k").unwrap(), b"three", "a quoted etag works too");
    }

    #[test]
    fn traversal_and_odd_keys_never_leave_the_bucket() {
        let (d, b) = be();
        b.create_bucket("n-one", &[]).unwrap();
        for k in ["../escape", "a/../../escape", "/abs", "a//b", "a/./b", "", "a\\b", "a\0b", "a/"] {
            assert!(matches!(b.put("n-one", k, b"x", false), Err(BackendError::BadKey(_))), "{k:?}");
        }
        for bad in ["../x", "N-ONE", "a/b", "ab", ".hidden"] {
            assert!(b.create_bucket(bad, &[]).is_err(), "{bad}");
        }
        assert!(!d.path().join("escape").exists());
    }

    #[test]
    fn keys_and_policies_survive_a_reopen() {
        let (d, b) = be();
        let k = b.create_access_key("cog:c1").unwrap();
        assert_eq!(b.create_access_key("cog:c1").unwrap(), k, "idempotent per principal");
        b.put_policy(&k.id, &serde_json::json!({"Statement": []})).unwrap();
        b.set_key_enabled(&k.id, false).unwrap();
        let b2 = FsBackend::open(d.path()).unwrap();
        assert_eq!(b2.key_enabled(&k.id), Some(false));
        assert!(b2.policy_of(&k.id).is_some());
        assert!(b2.set_key_enabled("nope", true).is_err());
    }

    #[test]
    fn a_broken_keys_file_is_an_error_not_an_empty_table() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("keys.json"), b"{nope").unwrap();
        assert!(FsBackend::open(d.path()).is_err());
    }

    #[test]
    fn deleting_the_last_object_prunes_empty_directories() {
        let (d, b) = be();
        b.create_bucket("n-one", &[]).unwrap();
        b.put("n-one", "x/y/z", b"1", false).unwrap();
        b.delete("n-one", "x/y/z").unwrap();
        assert!(!d.path().join("buckets/n-one/data/x").exists());
        assert!(d.path().join("buckets/n-one/data").exists());
    }

    #[test]
    fn keys_that_differ_only_by_case_are_refused_not_merged() {
        // The default macOS volume is case-insensitive, where `A.txt` and `a.txt` would be one
        // file. The same rule applies everywhere so a store reads the same on any host.
        let (_d, b) = be();
        b.create_bucket("n-one", &[]).unwrap();
        b.put("n-one", "Docs/A.txt", b"first", false).unwrap();
        for (k, cond) in [("Docs/a.txt", false), ("Docs/a.txt", true), ("docs/A.txt", false), ("DOCS/A.TXT", true)] {
            let e = b.put("n-one", k, b"second", cond).unwrap_err();
            assert!(matches!(e, BackendError::BadKey(_)), "{k} {cond}: {e:?}");
        }
        assert_eq!(b.get("n-one", "Docs/A.txt").unwrap(), b"first", "the first object is untouched");
        for k in ["Docs/a.txt", "docs/A.txt"] {
            assert!(matches!(b.get("n-one", k), Err(BackendError::NoObject(_))), "{k} must not read the other case's object");
            assert!(matches!(b.head("n-one", k), Err(BackendError::NoObject(_))), "{k}");
            assert!(matches!(b.delete("n-one", k), Err(BackendError::NoObject(_))), "{k} must not delete the other case's object");
        }
        assert_eq!(b.get("n-one", "Docs/A.txt").unwrap(), b"first");
        // overwriting the exact key, and different names, still work
        b.put("n-one", "Docs/A.txt", b"again", false).unwrap();
        b.put("n-one", "Docs/b.txt", b"b", false).unwrap();
        assert_eq!(b.get("n-one", "Docs/A.txt").unwrap(), b"again");
        assert_eq!(b.list("n-one", "").unwrap().len(), 2);
        // once the key is gone its case is free again
        b.delete("n-one", "Docs/A.txt").unwrap();
        b.put("n-one", "Docs/a.txt", b"lower", false).unwrap();
        assert_eq!(b.get("n-one", "Docs/a.txt").unwrap(), b"lower");
    }
}
