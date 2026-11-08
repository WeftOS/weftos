use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;

use super::*;
use crate::audit::MemSink;
use crate::backend::{AdminBackend, ObjectBackend};
use crate::mem::MemBackend;
use crate::service::{Config, Parties, StoreService, archive_key};
use crate::{ChainSink, ExposureGate, StoreName};

const DAY: u64 = 86_400_000;
const PROJECT: &str = "01HZX3K9Q5M2N7P8R4T6V0WABC";

struct Open;
impl ExposureGate for Open {
    fn register(&self, _: &str, _: &str) {}
    fn check(&self, _: &str) -> Result<(), String> {
        Ok(())
    }
}
#[derive(Default)]
struct Chain(Mutex<Vec<(String, Value)>>);
impl ChainSink for Chain {
    fn record(&self, k: &str, v: Value) {
        self.0.lock().unwrap().push((k.to_owned(), v));
    }
}

static NOW: AtomicU64 = AtomicU64::new(0);
// These tests share one clock, so they take turns.
static TURN: Mutex<()> = Mutex::new(());
fn clock() -> u64 {
    NOW.load(Ordering::SeqCst)
}

struct Rig {
    svc: StoreService,
    be: Arc<MemBackend>,
    chain: Arc<Chain>,
    _turn: std::sync::MutexGuard<'static, ()>,
}
use std::sync::Arc;

fn rig(day: u64) -> Rig {
    let turn = TURN.lock().unwrap_or_else(|e| e.into_inner());
    NOW.store(day * DAY, Ordering::SeqCst);
    let be = Arc::new(MemBackend::new());
    let chain = Arc::new(Chain::default());
    let svc = StoreService::open(Config { node_id: "pg-1".into(), state_path: None, clock }, be.clone(), be.clone(), Arc::new(Open), Arc::new(MemSink::default()), chain.clone()).unwrap();
    Rig { svc, be, chain, _turn: turn }
}

fn put_set(r: &Rig, bucket: &str, archived_day: u64, keys: &[&str]) {
    let _ = AdminBackend::create_bucket(&*r.be, ARCHIVE_BUCKET, &[]);
    for k in keys {
        r.be.put(ARCHIVE_BUCKET, &archive_key(bucket, archived_day * DAY, k), k.as_bytes(), false).unwrap();
    }
}

fn left(r: &Rig) -> Vec<String> {
    r.be.list(ARCHIVE_BUCKET, "").unwrap().into_iter().map(|m| m.key).collect()
}

const KEEP30: ArchiveKeep = ArchiveKeep { node_days: Some(30), project_days: Some(30), release_days: Some(30) };

#[test]
fn by_default_nothing_expires_however_old() {
    let r = rig(1000);
    put_set(&r, "n-old", 1, &["a"]);
    put_set(&r, "p-old", 1, &["a"]);
    put_set(&r, "r-old", 1, &["a"]);
    let rep = r.svc.archive_gc(&ArchiveKeep::default(), true, "op").unwrap();
    assert_eq!((rep.expired.len(), rep.kept, rep.deleted_objects), (0, 3, 0));
    assert_eq!(left(&r).len(), 3);
    assert!(r.chain.0.lock().unwrap().is_empty(), "nothing done, nothing recorded");
}

#[test]
fn a_set_past_its_kinds_keep_time_expires_whole_and_a_younger_one_stays() {
    let r = rig(100);
    put_set(&r, "n-gone", 60, &["d/a", "d/b", "c"]); // 40 days old
    put_set(&r, "n-fresh", 90, &["d/a"]); // 10 days
    put_set(&r, "p-gone", 69, &["x"]); // 31 days
    put_set(&r, "p-edge", 70, &["x"]); // exactly 30 days: expired (>=)
    put_set(&r, "r-keep", 95, &["x"]);
    let keep = ArchiveKeep { node_days: Some(30), project_days: Some(30), release_days: Some(30) };
    let dry = r.svc.archive_gc(&keep, false, "op").unwrap();
    assert!(dry.dry_run);
    let names: Vec<(&str, u64, u64)> = dry.expired.iter().map(|s| (s.bucket.as_str(), s.objects, s.age_days)).collect();
    assert_eq!(names, vec![("n-gone", 3, 40), ("p-edge", 1, 30), ("p-gone", 1, 31)]);
    assert_eq!(dry.deleted_objects, 0);
    assert_eq!(left(&r).len(), 7, "a dry run deletes nothing");
    let done = r.svc.archive_gc(&keep, true, "op").unwrap();
    assert_eq!((done.deleted_objects, done.kept, done.dry_run), (5, 2, false));
    let rest = left(&r);
    assert_eq!(rest.len(), 2);
    assert!(rest.iter().all(|k| k.starts_with("n-fresh@") || k.starts_with("r-keep@")), "{rest:?}");
    assert_eq!(r.chain.0.lock().unwrap().iter().filter(|(k, _)| k == "store.archive.gc").count(), 1);
    // A second run finds nothing more and records nothing more.
    assert_eq!(r.svc.archive_gc(&keep, true, "op").unwrap().deleted_objects, 0);
    assert_eq!(r.chain.0.lock().unwrap().iter().filter(|(k, _)| k == "store.archive.gc").count(), 1);
}

#[test]
fn each_kind_has_its_own_limit() {
    let r = rig(100);
    put_set(&r, "n-a", 1, &["x"]);
    put_set(&r, "p-a", 1, &["x"]);
    put_set(&r, "r-a", 1, &["x"]);
    let rep = r.svc.archive_gc(&ArchiveKeep { node_days: Some(10), ..Default::default() }, true, "op").unwrap();
    assert_eq!(rep.expired.len(), 1);
    assert_eq!(rep.expired[0].kind, "node");
    assert_eq!(left(&r).len(), 2, "projects and releases have no limit set");
}

#[test]
fn worktree_archives_and_things_that_are_not_archive_keys_are_left_alone() {
    let r = rig(1000);
    put_set(&r, "w-slug", 1, &["x"]);
    put_set(&r, "z-unknown", 1, &["x"]);
    r.be.put(ARCHIVE_BUCKET, "no-slash-key", b"x", false).unwrap();
    r.be.put(ARCHIVE_BUCKET, "n-a@notanumber/k", b"x", false).unwrap();
    r.be.put(ARCHIVE_BUCKET, "@5/k", b"x", false).unwrap();
    let rep = r.svc.archive_gc(&KEEP30, true, "op").unwrap();
    assert_eq!((rep.expired.len(), rep.deleted_objects), (0, 0));
    assert_eq!(rep.not_ours, 5);
    assert_eq!(left(&r).len(), 5);
}

#[test]
fn an_at_sign_in_the_object_key_does_not_confuse_the_set() {
    let r = rig(100);
    put_set(&r, "p-x", 1, &["dir@2/file@3.txt", "a@b"]);
    let rep = r.svc.archive_gc(&KEEP30, true, "op").unwrap();
    assert_eq!((rep.expired.len(), rep.expired[0].objects, rep.deleted_objects), (1, 2, 2));
    assert!(left(&r).is_empty());
}

#[test]
fn zero_days_is_refused_and_an_archive_that_was_never_made_is_fine() {
    let r = rig(100);
    assert!(matches!(r.svc.archive_gc(&ArchiveKeep { project_days: Some(0), ..Default::default() }, true, "op"), Err(StoreError::Refused(_))));
    let rep = r.svc.archive_gc(&KEEP30, true, "op").unwrap();
    assert_eq!((rep.expired.len(), rep.kept, rep.deleted_objects), (0, 0, 0), "no archive bucket yet");
}

#[test]
fn an_archive_still_being_written_is_never_deleted_but_a_finished_one_of_the_same_age_is() {
    let r = rig(10);
    let p = StoreName::project("weft", PROJECT).unwrap();
    r.svc.create_store(&p, "daemon", Parties::default()).unwrap();
    r.svc.grant(&p, crate::Principal::parse("member:m").unwrap(), crate::Access::Write, "op").unwrap();
    r.svc.put(&crate::service::Caller::local(crate::Principal::parse("member:m").unwrap()), &p, "k", b"x", false).unwrap();
    // Archive at day 10 by hand-marking the record as cut short, as a crash would leave it.
    r.svc.archive(&p, false, "op").unwrap();
    {
        let mut g = r.svc.lock();
        let rec = g.stores.get_mut(&p.to_string()).unwrap();
        rec.archive_done = false;
    }
    NOW.store(60 * DAY, Ordering::SeqCst);
    let rep = r.svc.archive_gc(&KEEP30, true, "op").unwrap();
    assert!(rep.expired.is_empty(), "{rep:?}");
    assert_eq!(rep.in_progress.len(), 1);
    assert_eq!(rep.in_progress[0].bucket, p.bucket());
    // Finished: now it goes.
    {
        let mut g = r.svc.lock();
        g.stores.get_mut(&p.to_string()).unwrap().archive_done = true;
    }
    let rep = r.svc.archive_gc(&KEEP30, true, "op").unwrap();
    assert_eq!((rep.expired.len(), rep.in_progress.len()), (1, 0));
}

#[test]
fn a_real_archive_of_a_project_expires_after_its_time_and_the_store_record_is_unaffected() {
    let r = rig(10);
    let p = StoreName::project("weft", PROJECT).unwrap();
    r.svc.create_store(&p, "daemon", Parties::default()).unwrap();
    let who = crate::service::Caller::local(crate::Principal::parse("member:m").unwrap());
    r.svc.grant(&p, crate::Principal::parse("member:m").unwrap(), crate::Access::Write, "op").unwrap();
    r.svc.put(&who, &p, "a/b", b"data", false).unwrap();
    assert_eq!(r.svc.archive(&p, false, "op").unwrap(), 1);
    NOW.store(20 * DAY, Ordering::SeqCst);
    assert_eq!(r.svc.archive_gc(&KEEP30, true, "op").unwrap().deleted_objects, 0, "10 days old, kept");
    NOW.store(45 * DAY, Ordering::SeqCst);
    let rep = r.svc.archive_gc(&KEEP30, true, "op").unwrap();
    assert_eq!((rep.deleted_objects, rep.expired[0].kind.as_str()), (1, "project"));
    assert!(left(&r).is_empty());
    assert!(r.svc.has_store(&p), "the store record is not the archive's to remove");
    assert_eq!(r.be.get(&p.bucket(), "a/b").unwrap(), b"data", "and the live bucket is untouched");
}
