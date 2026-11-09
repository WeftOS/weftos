//! Hardening scenario 3: the watch loop dies in the middle of a batch.
//!
//! The loop (`watch_once`, then `sync_up`) runs on its own thread against a wrapper backend that
//! panics right after the Nth data write lands. The panic unwinds through `sync_up_with` without
//! reaching its index save and without any cleanup, which is the state a killed process leaves: the
//! store holds the writes that completed and `index.json` is still the one from before the batch.
//!
//! A real SIGKILL of a re-executed test binary was built and passed on the file backend and on
//! RustFS (commit 7f4eac450 and cbabe7727 hold it), but `scripts/diff-gate.sh` rejects new
//! `std::process::Command` use, so the committed version simulates the death in-process.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::tests_hardening::*;
use super::*;
use crate::backend::{ObjectBackend, ObjectMeta};

/// A backend wrapper that logs every successful write and can die (panic) right after the Nth
/// data write lands. Tombstone writes are logged but not counted.
pub(super) struct Probe<'a, B: ObjectBackend + ?Sized> {
    inner: &'a B,
    die_after: Option<usize>,
    log: Mutex<Vec<String>>,
}

impl<'a, B: ObjectBackend + ?Sized> Probe<'a, B> {
    pub(super) fn new(inner: &'a B, die_after: Option<usize>) -> Self {
        Self { inner, die_after, log: Mutex::new(vec![]) }
    }
    /// Successful writes, sorted. A delete of a tombstone key is dropped: a create always tries to
    /// clear its marker, which RustFS answers 204 even when there was none, while the file and
    /// memory backends answer "no such object".
    fn writes(&self) -> Vec<String> {
        let mut v: Vec<String> = self.log.lock().unwrap().iter().filter(|e| !(e.starts_with("del:") && e.contains(TOMBSTONE_DIR))).cloned().collect();
        v.sort();
        v
    }
    fn note(&self, what: &str, key: &str) {
        let n = {
            let mut l = self.log.lock().unwrap();
            l.push(format!("{what}:{key}"));
            l.iter().filter(|e| !e.contains(TOMBSTONE_DIR)).count()
        };
        if self.die_after == Some(n) && !key.contains(TOMBSTONE_DIR) {
            panic!("simulated death after data write {n}: {what} {key}");
        }
    }
}

impl<B: ObjectBackend + ?Sized> ObjectBackend for Probe<'_, B> {
    fn put(&self, b: &str, k: &str, d: &[u8], n: bool) -> std::result::Result<ObjectMeta, BackendError> {
        let r = self.inner.put(b, k, d, n)?;
        self.note("put", k);
        Ok(r)
    }
    fn put_if_match(&self, b: &str, k: &str, d: &[u8], e: &str) -> std::result::Result<ObjectMeta, BackendError> {
        let r = self.inner.put_if_match(b, k, d, e)?;
        self.note("put", k);
        Ok(r)
    }
    fn delete_if_match(&self, b: &str, k: &str, e: &str) -> std::result::Result<(), BackendError> {
        self.inner.delete_if_match(b, k, e)?;
        self.note("del", k);
        Ok(())
    }
    fn get(&self, b: &str, k: &str) -> std::result::Result<Vec<u8>, BackendError> {
        self.inner.get(b, k)
    }
    fn head(&self, b: &str, k: &str) -> std::result::Result<ObjectMeta, BackendError> {
        self.inner.head(b, k)
    }
    fn delete(&self, b: &str, k: &str) -> std::result::Result<(), BackendError> {
        self.inner.delete(b, k)?;
        self.note("del", k);
        Ok(())
    }
    fn copy(&self, sb: &str, sk: &str, db: &str, dk: &str) -> std::result::Result<ObjectMeta, BackendError> {
        self.inner.copy(sb, sk, db, dk)
    }
    fn list(&self, b: &str, p: &str) -> std::result::Result<Vec<ObjectMeta>, BackendError> {
        self.inner.list(b, p)
    }
}

/// 3: the watch loop dies in the middle of a batch. The batch, in path order, is: modify f0, modify
/// f1, delete f2, delete f3, create n0, create n1. `die_after` is how many data writes the loop
/// completes before it dies: 3 stops after f2's delete (before its tombstone), 5 stops after the
/// create of n0 (before the index is saved).
pub(super) fn crash_mid_batch<B: ObjectBackend + ?Sized>(be: &B, bk: &str, die_after: usize) {
    assert!(die_after == 3 || die_after == 5);
    let late = die_after == 5;
    let pre = if late { "k2" } else { "k1" };
    let r = Rig::new(be, bk, pre);
    for i in 0..4 {
        r.seed(&format!("f{i}.txt"), &format!("orig f{i}"));
    }
    let a = r.join("a");
    let (e0, e1) = (r.etag("f0.txt"), r.etag("f1.txt"));
    wr(&a, "f0.txt", "f0 edited");
    wr(&a, "f1.txt", "f1 edited");
    std::fs::remove_file(a.join("f2.txt")).unwrap();
    std::fs::remove_file(a.join("f3.txt")).unwrap();
    wr(&a, "n0.txt", "new 0");
    wr(&a, "n1.txt", "new 1");

    // The loop runs watch then sync_up on its own thread and dies after its Nth data write.
    let cfg = WatchConfig { quiet: Duration::from_millis(100), poll: Duration::from_millis(20), ..Default::default() };
    let died = std::thread::scope(|sc| {
        sc.spawn(|| {
            let batch = watch_once(&a, &cfg, Duration::from_secs(30)).unwrap().expect("a batch");
            assert_eq!(batch.len(), 6);
            sync_up(&Probe::new(be, Some(die_after)), &a)
        })
        .join()
    });
    assert!(died.is_err(), "the loop was meant to die mid-batch");

    // What the crash left: the writes before the death landed, the rest did not, and the index
    // still describes the tree before the batch.
    assert_eq!((r.text("f0.txt").as_deref(), r.text("f1.txt").as_deref()), (Some("f0 edited"), Some("f1 edited")));
    assert!(r.text("f2.txt").is_none() && r.text("n1.txt").is_none());
    assert_eq!((r.text("f3.txt").is_none(), r.text("n0.txt").is_some()), (late, late));
    assert_eq!(r.tomb("f2.txt").is_some(), late, "with 3 writes the death came before f2's tombstone");
    let ix = Index::load(&a).unwrap().expect("index.json is intact (renamed into place, never half-written)");
    assert!(ix.entries["f0.txt"].etag == e0 && ix.entries["f1.txt"].etag == e1 && ix.entries.contains_key("f2.txt"));
    let (landed0, landed1) = (r.etag("f0.txt"), r.etag("f1.txt"));
    // Debris a killed run can leave, planted (a death cannot be aimed at these windows): a
    // temp beside a target, one in a subdirectory, a half-written index temp, a CAS temp.
    wr(&a, ".drive.tmp.01KILLED", "partial placement");
    wr(&a, "sub/.drive.tmp.01KILLED", "partial placement");
    std::fs::write(a.join(DRIVE_DIR).join(".drive.tmp.index"), "{\"schema\":1,\"bucket\":\"half").unwrap();
    std::fs::create_dir_all(a.join(DRIVE_DIR).join("cas/ab")).unwrap();
    std::fs::write(a.join(DRIVE_DIR).join("cas/ab/.drive.tmp.01KILLED"), "partial").unwrap();

    // Restart: watch picks the whole batch up from the scan against the index.
    let mut w = Watcher::new(&a, WatchConfig::default()).unwrap();
    let t = Instant::now();
    assert!(w.poll(t).unwrap().is_none());
    let ch = w.poll(t + QUIET_PLUS).unwrap().expect("pending changes found after restart");
    let paths: Vec<_> = ch.iter().map(|c| c.path.as_str()).collect();
    assert_eq!(paths, ["f0.txt", "f1.txt", "f2.txt", "f3.txt", "n0.txt", "n1.txt"], "temp debris is not a change");
    let probe = Probe::new(be, None);
    let up = sync_up(&probe, &a).unwrap();
    // Late: n0 had landed (a create-only put finds the same bytes and adopts), f2 and f3 were
    // already deleted (the delete finds no object and forgets it).
    let (put, deleted, expect): (&[&str], &[&str], &[&str]) = if late {
        (&["n1.txt"], &[], &["put:n1.txt"])
    } else {
        (&["n0.txt", "n1.txt"], &["f3.txt"], &["del:f3.txt", "put:.drive-tombstones/f3.txt", "put:n0.txt", "put:n1.txt"])
    };
    assert_eq!((up.put.clone(), up.deleted.clone()), (put.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>(), deleted.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>()), "{up:?}");
    assert!(up.conflicts.is_empty() && up.skipped.is_empty(), "{up:?}");
    let expect: Vec<String> = expect.iter().map(|e| e.replacen(':', &format!(":{pre}/"), 1)).collect();
    assert_eq!(probe.writes(), expect, "nothing that landed before the crash is uploaded again");
    assert_eq!((r.etag("f0.txt"), r.etag("f1.txt")), (landed0, landed1));
    // Index consistent: matches the store, nothing pending, the half-written temp is gone.
    let ix = Index::load(&a).unwrap().unwrap();
    assert_eq!((&ix.entries["f0.txt"].etag, &ix.entries["f1.txt"].etag), (&r.etag("f0.txt"), &r.etag("f1.txt")));
    assert!(!ix.entries.contains_key("f2.txt") && !ix.entries.contains_key("f3.txt") && ix.entries.contains_key("n1.txt"));
    assert!(ix.conflicts.is_empty() && scan(&a, &ix, &Default::default()).unwrap().changes.is_empty());
    assert!(!a.join(DRIVE_DIR).join(".drive.tmp.index").exists());
    // Stale temps are ignored (never uploaded), not cleaned.
    assert!(a.join(".drive.tmp.01KILLED").exists());
    assert!(be.list(bk, &format!("{pre}/")).unwrap().iter().all(|m| !m.key.contains(TMP_PREFIX)));
    // A second device sees one consistent state. f2's tombstone was lost to the crash window.
    let b = r.join("b");
    assert_eq!((rd(&b, "f0.txt"), rd(&b, "n1.txt")), ("f0 edited".to_owned(), "new 1".to_owned()));
    assert!(!b.join("f2.txt").exists() && !b.join("f3.txt").exists());
    assert_eq!((r.tomb("f2.txt").is_some(), r.tomb("f3.txt").is_some()), (late, true));
    let dn = sync_down(be, &a).unwrap();
    assert!(dn.conflicts.is_empty() && dn.created.is_empty() && dn.updated.is_empty() && dn.removed.is_empty());
}

#[test]
fn a_watch_loop_that_dies_mid_batch_restarts_from_the_scan_without_double_uploads() {
    let be = mem();
    crash_mid_batch(&be, BK, 3);
}

#[test]
fn a_watch_loop_that_dies_after_a_create_landed_adopts_it_on_restart() {
    let be = mem();
    crash_mid_batch(&be, BK, 5);
}
