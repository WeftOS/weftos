//! Hardening scenario 3: the watch loop is killed (SIGKILL) in the middle of a batch.
//!
//! The test re-executes its own test binary as a child (`crash_child_entry`) that runs the watch
//! and `sync_up` against a shared store, parks after the Nth data write, and is killed by the
//! parent. Runs here on the file-system backend (two processes can share a directory) and, ignored,
//! on a real RustFS from `live_hardening.rs`.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::*;
use super::tests_hardening::*;
use crate::backend::{AdminBackend, ObjectBackend, ObjectMeta};

/// A backend wrapper that logs every successful write and can park the process after the Nth
/// data write (tombstone writes are logged but not counted), so a parent can SIGKILL it there.
pub(super) struct Probe<'a, B: ObjectBackend + ?Sized> {
    inner: &'a B,
    park_after: Option<usize>,
    log: Mutex<Vec<String>>,
}

impl<'a, B: ObjectBackend + ?Sized> Probe<'a, B> {
    pub(super) fn new(inner: &'a B, park_after: Option<usize>) -> Self {
        Self { inner, park_after, log: Mutex::new(vec![]) }
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
        if self.park_after == Some(n) && !key.contains(TOMBSTONE_DIR) {
            println!("PARKED");
            let _ = std::io::stdout().flush();
            loop {
                std::thread::sleep(Duration::from_secs(3600));
            }
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

/// 3: the watch loop dies in the middle of a batch. `child_kind` tells the child process how to
/// open the same store (`fs:<root>` or `rustfs`). The batch, in path order, is: modify f0, modify
/// f1, delete f2, delete f3, create n0, create n1. `park_after` is how many data writes the child
/// completes before the kill: 3 stops after f2's delete (before its tombstone), 5 stops after the
/// create of n0 (before the index is saved).
pub(super) fn crash_mid_batch<B: ObjectBackend + ?Sized>(be: &B, bk: &str, child_kind: &str, park_after: usize) {
    assert!(park_after == 3 || park_after == 5);
    let late = park_after == 5;
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

    // The child runs watch then sync_up and parks after its Nth data write. The parent SIGKILLs it.
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "drive::tests_crash::crash_child_entry", "--nocapture", "--test-threads=1"])
        .env("DRIVE_CRASH_CHILD", format!("{child_kind}|{}|{park_after}", a.display()))
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let out = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for l in std::io::BufReader::new(out).lines().map_while(std::result::Result::ok) {
            let _ = tx.send(l);
        }
    });
    loop {
        match rx.recv_timeout(Duration::from_secs(90)) {
            Ok(l) if l.trim() == "PARKED" => break,
            Ok(l) if l.trim() == "COMPLETED" => panic!("the child finished before the kill point"),
            Ok(_) => {}
            Err(_) => {
                let _ = child.kill();
                panic!("the child never reached its kill point");
            }
        }
    }
    child.kill().unwrap(); // SIGKILL on unix
    let st = child.wait().unwrap();
    assert!(!st.success(), "killed, not exited: {st:?}");

    // What the crash left: the writes before the kill landed, the rest did not, and the index
    // still describes the tree before the batch.
    assert_eq!((r.text("f0.txt").as_deref(), r.text("f1.txt").as_deref()), (Some("f0 edited"), Some("f1 edited")));
    assert!(r.text("f2.txt").is_none() && r.text("n1.txt").is_none());
    assert_eq!((r.text("f3.txt").is_none(), r.text("n0.txt").is_some()), (late, late));
    assert_eq!(r.tomb("f2.txt").is_some(), late, "with 3 writes the kill came before f2's tombstone");
    let ix = Index::load(&a).unwrap().expect("index.json is intact (renamed into place, never half-written)");
    assert!(ix.entries["f0.txt"].etag == e0 && ix.entries["f1.txt"].etag == e1 && ix.entries.contains_key("f2.txt"));
    let (landed0, landed1) = (r.etag("f0.txt"), r.etag("f1.txt"));
    // Debris a killed run can leave, planted (a SIGKILL cannot be aimed at these windows): a
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

/// The child half of `crash_mid_batch`. With `DRIVE_CRASH_CHILD` unset it does nothing, so a
/// normal test run passes it. Spec: `<kind>|<drive dir>|<park after N data writes>`.
#[test]
fn crash_child_entry() {
    let Ok(spec) = std::env::var("DRIVE_CRASH_CHILD") else { return };
    let mut it = spec.splitn(3, '|');
    let (kind, dest, n) = (it.next().unwrap(), PathBuf::from(it.next().unwrap()), it.next().unwrap().parse().unwrap());
    let _keep;
    let be: Box<dyn ObjectBackend> = if let Some(root) = kind.strip_prefix("fs:") {
        Box::new(crate::fs::FsBackend::open(Path::new(root)).unwrap())
    } else {
        #[cfg(feature = "rustfs")]
        {
            let envf = std::env::var("WEFTOS_STORE_TEST_RUSTFS_ENV").unwrap();
            let sd = tempfile::tempdir().unwrap();
            let cfg = crate::rustfs::RustfsConfig::from_env_file(Path::new(&envf), sd.path().join("secrets")).unwrap();
            _keep = sd;
            Box::new(crate::rustfs::RustfsBackend::new(cfg).unwrap())
        }
        #[cfg(not(feature = "rustfs"))]
        panic!("kind {kind:?} needs the rustfs feature")
    };
    let cfg = WatchConfig { quiet: Duration::from_millis(100), poll: Duration::from_millis(20), ..Default::default() };
    let batch = watch_once(&dest, &cfg, Duration::from_secs(30)).unwrap().expect("a batch");
    println!("BATCH {}", batch.len());
    let probe = Probe::new(&*be, Some(n));
    sync_up(&probe, &dest).unwrap();
    println!("COMPLETED");
}

#[test]
fn a_killed_watch_loop_restarts_from_the_scan_without_double_uploads() {
    let root = tempfile::tempdir().unwrap();
    let store = root.path().join("store");
    let be = crate::fs::FsBackend::open(&store).unwrap();
    be.create_bucket(BK, &[]).unwrap();
    crash_mid_batch(&be, BK, &format!("fs:{}", store.display()), 3);
}

#[test]
fn a_watch_loop_killed_after_a_create_landed_adopts_it_on_restart() {
    let root = tempfile::tempdir().unwrap();
    let store = root.path().join("store");
    let be = crate::fs::FsBackend::open(&store).unwrap();
    be.create_bucket(BK, &[]).unwrap();
    crash_mid_batch(&be, BK, &format!("fs:{}", store.display()), 5);
}
