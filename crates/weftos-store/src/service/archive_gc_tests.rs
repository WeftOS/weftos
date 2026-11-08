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

fn project(r: &Rig, ulid: &str) -> StoreName {
    let p = StoreName::project("weft", ulid).unwrap();
    r.svc.create_store(&p, "daemon", Parties::default()).unwrap();
    p
}

const P2: &str = "01HZX3K9Q5M2N7P8R4T6V0WXYZ";

#[test]
fn a_per_store_override_beats_the_kind_limit_both_ways() {
    let r = rig(100);
    let short = project(&r, PROJECT);
    let long = project(&r, P2);
    put_set(&r, &short.bucket(), 90, &["x"]); // 10 days old
    put_set(&r, &long.bucket(), 1, &["x"]); // 99 days old
    put_set(&r, "p-other", 1, &["x"]); // no record, 99 days: kind limit
    r.svc.set_archive_keep(&short, Some(KeepFor::Days(5)), "op").unwrap();
    r.svc.set_archive_keep(&long, Some(KeepFor::Forever), "op").unwrap();
    let rep = r.svc.archive_gc(&KEEP30, true, "op").unwrap();
    let gone: Vec<&str> = rep.expired.iter().map(|s| s.bucket.as_str()).collect();
    assert!(gone.contains(&short.bucket().as_str()), "10 days old but its own limit is 5: {gone:?}");
    assert!(gone.contains(&"p-other"), "no override, the kind's 30 days: {gone:?}");
    assert!(!gone.contains(&long.bucket().as_str()), "forever beats the kind's 30 days: {gone:?}");
    assert_eq!(left(&r).len(), 1);
}

#[test]
fn an_override_works_when_the_kind_has_no_limit_and_clearing_it_goes_back() {
    let r = rig(100);
    let p = project(&r, PROJECT);
    put_set(&r, &p.bucket(), 1, &["x"]);
    r.svc.set_archive_keep(&p, Some(KeepFor::Days(10)), "op").unwrap();
    assert_eq!(r.svc.archive_keep(&p).unwrap(), Some(KeepFor::Days(10)));
    let dry = r.svc.archive_gc(&ArchiveKeep::default(), false, "op").unwrap();
    assert_eq!(dry.expired.len(), 1, "the override alone makes it expire");
    r.svc.set_archive_keep(&p, None, "op").unwrap();
    let rep = r.svc.archive_gc(&ArchiveKeep::default(), true, "op").unwrap();
    assert_eq!((rep.expired.len(), rep.kept, rep.deleted_objects), (0, 1, 0));
}

#[test]
fn an_override_is_refused_for_zero_days_unknown_stores_and_worktrees_and_is_recorded_and_persisted() {
    let r = rig(100);
    let p = project(&r, PROJECT);
    assert!(matches!(r.svc.set_archive_keep(&p, Some(KeepFor::Days(0)), "op"), Err(StoreError::Refused(_))));
    let ghost = StoreName::project("weft", P2).unwrap();
    assert!(matches!(r.svc.set_archive_keep(&ghost, Some(KeepFor::Forever), "op"), Err(StoreError::UnknownStore(_))));
    let w = StoreName::worktree("weft", PROJECT, "main").unwrap();
    r.svc.create_store(&w, "daemon", Parties::default()).unwrap();
    assert!(matches!(r.svc.set_archive_keep(&w, Some(KeepFor::Days(3)), "op"), Err(StoreError::Refused(_))), "worktree archives are S2's");
    r.svc.set_archive_keep(&p, Some(KeepFor::Days(7)), "op").unwrap();
    let ev: Vec<_> = r.chain.0.lock().unwrap().iter().filter(|(k, _)| k == "store.archive.keep").cloned().collect();
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0].1["keep"], serde_json::json!({"days": 7}));
    // Survives a restart: the record carries it.
    let st = serde_json::to_string(&*r.svc.lock()).unwrap();
    assert!(st.contains("\"archive_keep\":{\"days\":7}"), "{st}");
}

const EVERY2H: ArchiveSchedule = ArchiveSchedule { every_hours: 2, keep: KEEP30, confirmed: true };

#[test]
fn the_schedule_is_off_by_default_and_never_runs_until_set() {
    let r = rig(100);
    put_set(&r, "n-old", 1, &["x"]);
    assert_eq!(r.svc.archive_schedule(), None);
    NOW.store(500 * DAY, Ordering::SeqCst);
    assert!(r.svc.archive_gc_tick().unwrap().is_none());
    assert_eq!(left(&r).len(), 1, "nothing deleted without a schedule");
}

#[test]
fn a_scheduled_run_waits_one_interval_then_runs_and_then_waits_again() {
    let r = rig(100);
    put_set(&r, "n-old", 1, &["x"]);
    r.svc.set_archive_schedule(Some(EVERY2H)).unwrap();
    assert!(r.svc.archive_gc_tick().unwrap().is_none(), "first run is one interval away");
    NOW.store(100 * DAY + 2 * 3_600_000 - 1, Ordering::SeqCst);
    assert!(r.svc.archive_gc_tick().unwrap().is_none(), "one ms early");
    NOW.store(100 * DAY + 2 * 3_600_000, Ordering::SeqCst);
    let rep = r.svc.archive_gc_tick().unwrap().expect("due");
    assert_eq!((rep.deleted_objects, rep.dry_run), (1, false));
    put_set(&r, "n-old2", 1, &["y"]);
    assert!(r.svc.archive_gc_tick().unwrap().is_none(), "interval restarted");
    NOW.store(100 * DAY + 4 * 3_600_000, Ordering::SeqCst);
    assert_eq!(r.svc.archive_gc_tick().unwrap().unwrap().deleted_objects, 1);
    assert_eq!(r.chain.0.lock().unwrap().iter().filter(|(k, v)| k == "store.archive.gc" && v["actor"] == "schedule").count(), 2);
}

#[test]
fn an_unconfirmed_schedule_only_reports() {
    let r = rig(100);
    put_set(&r, "n-old", 1, &["x"]);
    r.svc.set_archive_schedule(Some(ArchiveSchedule { confirmed: false, ..EVERY2H })).unwrap();
    NOW.store(100 * DAY + 3 * 3_600_000, Ordering::SeqCst);
    let rep = r.svc.archive_gc_tick().unwrap().unwrap();
    assert!(rep.dry_run);
    assert_eq!((rep.expired.len(), rep.deleted_objects), (1, 0));
    assert_eq!(left(&r).len(), 1);
}

#[test]
fn a_schedule_with_no_day_numbers_and_no_override_does_nothing_even_when_due() {
    let r = rig(100);
    put_set(&r, "n-old", 1, &["x"]);
    r.svc.set_archive_schedule(Some(ArchiveSchedule { keep: ArchiveKeep::default(), ..EVERY2H })).unwrap();
    NOW.store(900 * DAY, Ordering::SeqCst);
    assert!(r.svc.archive_gc_tick().unwrap().is_none());
    assert_eq!(left(&r).len(), 1);
    // A per-store override alone is enough to make a due run do work.
    let p = project(&r, PROJECT);
    put_set(&r, &p.bucket(), 1, &["z"]);
    r.svc.set_archive_keep(&p, Some(KeepFor::Days(1)), "op").unwrap();
    NOW.store(1800 * DAY, Ordering::SeqCst);
    let rep = r.svc.archive_gc_tick().unwrap().unwrap();
    assert_eq!(rep.deleted_objects, 1, "only the overridden store's set; the kind has no limit");
}

#[test]
fn a_bad_schedule_is_refused_and_turning_it_off_stops_runs() {
    let r = rig(100);
    assert!(matches!(r.svc.set_archive_schedule(Some(ArchiveSchedule { every_hours: 0, ..EVERY2H })), Err(StoreError::Refused(_))));
    assert!(matches!(r.svc.set_archive_schedule(Some(ArchiveSchedule { keep: ArchiveKeep { node_days: Some(0), ..Default::default() }, ..EVERY2H })), Err(StoreError::Refused(_))));
    assert_eq!(r.svc.archive_schedule(), None, "a refused schedule does not half-apply");
    put_set(&r, "n-old", 1, &["x"]);
    r.svc.set_archive_schedule(Some(EVERY2H)).unwrap();
    r.svc.set_archive_schedule(None).unwrap();
    NOW.store(500 * DAY, Ordering::SeqCst);
    assert!(r.svc.archive_gc_tick().unwrap().is_none());
    assert_eq!(left(&r).len(), 1);
}
