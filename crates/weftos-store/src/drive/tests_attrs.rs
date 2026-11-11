//! The execute attribute through refresh and conflicts: a mode-only change on the store (same
//! bytes, same etag and size) reaches a refreshed drive without a HEAD per object; a conflict's
//! remote copy and `Keep::Remote` carry the remote's attribute.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;
use crate::backend::{AdminBackend, BackendError, ObjectAttrs, ObjectBackend, ObjectMeta};
use crate::mem::MemBackend;

const B: &str = "b-attrs";
const EXEC: ObjectAttrs = ObjectAttrs { executable: true };
const PLAIN: ObjectAttrs = ObjectAttrs { executable: false };

/// The memory backend, counting `attrs` calls (the HEADs a refresh makes).
struct Counting {
    inner: MemBackend,
    attr_calls: AtomicUsize,
}

impl Counting {
    fn new() -> Self {
        let inner = MemBackend::new();
        inner.create_bucket(B, &[]).unwrap();
        Self { inner, attr_calls: AtomicUsize::new(0) }
    }
    fn calls(&self) -> usize {
        self.attr_calls.load(Ordering::SeqCst)
    }
    fn etag(&self, key: &str) -> String {
        self.inner.head(B, key).unwrap().etag
    }
}

type R<T> = std::result::Result<T, BackendError>;

impl ObjectBackend for Counting {
    fn put(&self, b: &str, k: &str, d: &[u8], n: bool) -> R<ObjectMeta> {
        self.inner.put(b, k, d, n)
    }
    fn put_if_match(&self, b: &str, k: &str, d: &[u8], e: &str) -> R<ObjectMeta> {
        self.inner.put_if_match(b, k, d, e)
    }
    fn put_attrs(&self, b: &str, k: &str, d: &[u8], n: bool, a: ObjectAttrs) -> R<ObjectMeta> {
        self.inner.put_attrs(b, k, d, n, a)
    }
    fn put_if_match_attrs(&self, b: &str, k: &str, d: &[u8], e: &str, a: ObjectAttrs) -> R<ObjectMeta> {
        self.inner.put_if_match_attrs(b, k, d, e, a)
    }
    fn attrs(&self, b: &str, k: &str) -> R<ObjectAttrs> {
        self.attr_calls.fetch_add(1, Ordering::SeqCst);
        self.inner.attrs(b, k)
    }
    fn delete_if_match(&self, b: &str, k: &str, e: &str) -> R<()> {
        self.inner.delete_if_match(b, k, e)
    }
    fn get(&self, b: &str, k: &str) -> R<Vec<u8>> {
        self.inner.get(b, k)
    }
    fn head(&self, b: &str, k: &str) -> R<ObjectMeta> {
        self.inner.head(b, k)
    }
    fn delete(&self, b: &str, k: &str) -> R<()> {
        self.inner.delete(b, k)
    }
    fn copy(&self, sb: &str, sk: &str, db: &str, dk: &str) -> R<ObjectMeta> {
        self.inner.copy(sb, sk, db, dk)
    }
    fn list(&self, b: &str, p: &str) -> R<Vec<ObjectMeta>> {
        self.inner.list(b, p)
    }
    fn list_stamped(&self, b: &str, p: &str) -> R<Vec<(ObjectMeta, String)>> {
        self.inner.list_stamped(b, p)
    }
}

fn mode(p: &Path) -> u32 {
    std::fs::symlink_metadata(p).unwrap().permissions().mode() & 0o777
}

fn chmod(p: &Path, m: u32) {
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)).unwrap();
}

/// Flip the stored attribute with the same bytes: the etag and size stay, only the stamp moves.
fn flip(b: &Counting, key: &str, bytes: &[u8], a: ObjectAttrs) {
    let e = b.etag(key);
    b.put_if_match_attrs(B, key, bytes, &e, a).unwrap();
}

#[test]
fn a_mode_only_change_on_the_store_reaches_a_refreshed_drive() {
    let b = Counting::new();
    b.put_attrs(B, "p/x.sh", b"#!/bin/sh\n", false, PLAIN).unwrap();
    let d = tempfile::tempdir().unwrap();
    materialise(&b, B, "p", d.path()).unwrap();
    assert_eq!(mode(&d.path().join("x.sh")) & 0o111, 0);

    flip(&b, "p/x.sh", b"#!/bin/sh\n", EXEC);
    let r = sync_down(&b, d.path()).unwrap();
    assert_eq!(r.fetched, 0, "{r:?}");
    assert_eq!(r.mode_updated, ["x.sh"], "{r:?}");
    assert_eq!(r.updated, ["x.sh"], "{r:?}");
    assert_eq!(mode(&d.path().join("x.sh")), 0o755);
    assert!(Index::load(d.path()).unwrap().unwrap().entries["x.sh"].exec);
    assert!(sync_up(&b, d.path()).unwrap().put.is_empty(), "applying the store's mode is not a local change");

    // And back: clearing the attribute clears the execute bits.
    flip(&b, "p/x.sh", b"#!/bin/sh\n", PLAIN);
    let r = sync_down(&b, d.path()).unwrap();
    assert_eq!(r.mode_updated, ["x.sh"], "{r:?}");
    assert_eq!(mode(&d.path().join("x.sh")) & 0o111, 0);
    assert!(!Index::load(d.path()).unwrap().unwrap().entries["x.sh"].exec);
    assert!(sync_up(&b, d.path()).unwrap().put.is_empty());
}

#[test]
fn a_refresh_asks_for_attributes_only_where_the_listing_stamp_moved() {
    let b = Counting::new();
    for i in 0..20 {
        b.put_attrs(B, &format!("p/f{i:02}.txt"), format!("file {i}").as_bytes(), false, PLAIN).unwrap();
    }
    let d = tempfile::tempdir().unwrap();
    materialise(&b, B, "p", d.path()).unwrap();
    let before = b.calls();

    let r = sync_down(&b, d.path()).unwrap();
    assert_eq!((r.unchanged, b.calls() - before), (20, 0), "nothing changed: no HEAD at all ({r:?})");

    flip(&b, "p/f07.txt", b"file 7", EXEC);
    let r = sync_down(&b, d.path()).unwrap();
    assert_eq!(r.mode_updated, ["f07.txt"], "{r:?}");
    assert_eq!(r.unchanged, 19, "{r:?}");
    assert_eq!(b.calls() - before, 1, "one HEAD, for the one object whose stamp moved");

    let r = sync_down(&b, d.path()).unwrap();
    assert_eq!((r.unchanged, b.calls() - before), (20, 1), "the stamp was recorded: no HEAD again ({r:?})");
}

#[test]
fn a_file_this_device_uploaded_is_checked_once_then_not_again() {
    let b = Counting::new();
    b.put(B, "p/seed.txt", b"seed", false).unwrap();
    let d = tempfile::tempdir().unwrap();
    materialise(&b, B, "p", d.path()).unwrap();
    std::fs::write(d.path().join("run.sh"), "#!/bin/sh\n").unwrap();
    chmod(&d.path().join("run.sh"), 0o755);
    assert_eq!(sync_up(&b, d.path()).unwrap().put, ["run.sh"]);
    let before = b.calls();
    // The put's answer carries no stamp, so the first refresh looks at this one object ...
    let r = sync_down(&b, d.path()).unwrap();
    assert_eq!((r.unchanged, r.mode_updated.len(), b.calls() - before), (2, 0, 1), "{r:?}");
    // ... and the second has nothing to ask.
    sync_down(&b, d.path()).unwrap();
    assert_eq!(b.calls() - before, 1);
    assert_eq!(mode(&d.path().join("run.sh")), 0o755, "an executable is left alone");
}

#[test]
fn a_remote_mode_change_does_not_overwrite_a_local_edit_or_a_local_chmod() {
    let b = Counting::new();
    b.put_attrs(B, "p/edit.sh", b"one\n", false, PLAIN).unwrap();
    b.put_attrs(B, "p/mine.sh", b"two\n", false, PLAIN).unwrap();
    let d = tempfile::tempdir().unwrap();
    materialise(&b, B, "p", d.path()).unwrap();
    std::fs::write(d.path().join("edit.sh"), "one edited\n").unwrap();
    chmod(&d.path().join("mine.sh"), 0o700);
    flip(&b, "p/edit.sh", b"one\n", EXEC);
    flip(&b, "p/mine.sh", b"two\n", EXEC);
    let r = sync_down(&b, d.path()).unwrap();
    assert!(r.conflicts.is_empty() && r.mode_updated.is_empty(), "{r:?}");
    assert_eq!(std::fs::read_to_string(d.path().join("edit.sh")).unwrap(), "one edited\n");
    // The local chmod equals the store's attribute: adopted, nothing to push for it.
    let ix = Index::load(d.path()).unwrap().unwrap();
    assert!(ix.entries["mine.sh"].exec);
    let up = sync_up(&b, d.path()).unwrap();
    assert_eq!(up.put, ["edit.sh"], "only the edit goes out ({up:?})");
    // Refreshing again asks nothing: both stamps were recorded.
    let c = b.calls();
    sync_down(&b, d.path()).unwrap();
    assert_eq!(b.calls(), c + 1, "only the just-uploaded edit.sh is looked at, once");
}

#[test]
fn a_conflicts_remote_copy_and_keep_remote_carry_the_execute_attribute() {
    let b = Counting::new();
    b.put_attrs(B, "p/run", b"v1\n", false, PLAIN).unwrap();
    b.put_attrs(B, "p/plain", b"v1\n", false, PLAIN).unwrap();
    let d = tempfile::tempdir().unwrap();
    materialise(&b, B, "p", d.path()).unwrap();
    // Both sides edit both files; the store's `run` is executable, `plain` is not.
    for f in ["run", "plain"] {
        std::fs::write(d.path().join(f), "local\n").unwrap();
    }
    let e = b.etag("p/run");
    b.put_if_match_attrs(B, "p/run", b"remote\n", &e, EXEC).unwrap();
    let e = b.etag("p/plain");
    b.put_if_match_attrs(B, "p/plain", b"remote\n", &e, PLAIN).unwrap();
    let r = sync_down(&b, d.path()).unwrap();
    assert_eq!(r.conflicts.len(), 2, "{r:?}");
    let ix = Index::load(d.path()).unwrap().unwrap();
    let copy = |p: &str| ix.conflicts[p].remote_copy.clone().unwrap();
    assert_eq!(mode(&d.path().join(copy("run"))), 0o755, "the remote copy of an executable is executable");
    assert_eq!(mode(&d.path().join(copy("plain"))) & 0o111, 0);

    resolve(d.path(), "run", Keep::Remote).unwrap();
    resolve(d.path(), "plain", Keep::Remote).unwrap();
    assert_eq!(mode(&d.path().join("run")), 0o755, "Keep::Remote on an executable keeps it executable");
    assert_eq!(std::fs::read_to_string(d.path().join("run")).unwrap(), "remote\n");
    assert_eq!(mode(&d.path().join("plain")) & 0o111, 0);
    let ix = Index::load(d.path()).unwrap().unwrap();
    assert!(ix.entries["run"].exec && !ix.entries["plain"].exec);
    assert!(sync_up(&b, d.path()).unwrap().put.is_empty(), "the resolved files match the store: nothing to push");
    assert!(b.inner.attrs(B, "p/run").unwrap().executable, "the store's attribute was not lost");
}

#[test]
fn a_create_conflict_found_by_sync_up_carries_the_remote_attribute() {
    let b = Counting::new();
    let d = tempfile::tempdir().unwrap();
    materialise(&b, B, "p", d.path()).unwrap();
    std::fs::write(d.path().join("tool"), "mine\n").unwrap();
    b.put_attrs(B, "p/tool", b"theirs\n", false, EXEC).unwrap();
    let up = sync_up(&b, d.path()).unwrap();
    assert_eq!(up.conflicts, ["tool"], "{up:?}");
    let ix = Index::load(d.path()).unwrap().unwrap();
    let copy = ix.conflicts["tool"].remote_copy.clone().unwrap();
    assert_eq!(mode(&d.path().join(&copy)), 0o755);
    assert!(ix.conflicts["tool"].remote_exec);
    resolve(d.path(), "tool", Keep::Remote).unwrap();
    assert_eq!(mode(&d.path().join("tool")), 0o755);
}

#[test]
fn a_conflict_record_and_an_entry_from_before_these_fields_still_load() {
    let c: OpenConflict = serde_json::from_str(r#"{"kind":"both_modified","remote_copy":"a.remote-1","remote_etag":"e","at_ms":1}"#).unwrap();
    assert!(!c.remote_exec);
    let e: Entry = serde_json::from_str(r#"{"size":1,"mtime_ns":2,"hash":"","etag":"e"}"#).unwrap();
    assert!(e.stamp.is_empty() && !e.exec);
    let plain = serde_json::to_string(&e).unwrap();
    assert!(!plain.contains("stamp") && !plain.contains("exec"), "unset fields are not written: {plain}");
}
