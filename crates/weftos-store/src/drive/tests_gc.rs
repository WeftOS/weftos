//! Tombstone collection (gc.rs): switch, age, race, cap, scope, and the stale-device behaviour.

use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use super::*;
use crate::backend::{AdminBackend, ObjectBackend, ObjectMeta};
use crate::mem::MemBackend;

const B: &str = "b-gc";
const HOUR_MS: u64 = 3_600_000;

type R<T> = std::result::Result<T, BackendError>;

/// A memory backend that logs every call, can lack conditional deletes, and can replace one key
/// right before the next `delete_if_match` (a writer landing between the listing and the delete).
struct Counting {
    inner: MemBackend,
    log: Mutex<Vec<String>>,
    cond: bool,
    replace_before_delete: Mutex<Option<(String, String)>>,
}

impl Counting {
    fn new() -> Self {
        let inner = MemBackend::new();
        inner.create_bucket(B, &[]).unwrap();
        Self { inner, log: Mutex::new(vec![]), cond: true, replace_before_delete: Mutex::new(None) }
    }
    fn seed(&self, key: &str, body: &str) {
        self.inner.put(B, key, body.as_bytes(), false).unwrap();
    }
    fn tomb(&self, rel: &str, ts_ms: u64) {
        self.seed(&format!("proj/.drive-tombstones/{rel}"), &format!(r#"{{"device":"d","ts_ms":{ts_ms},"etag":"e"}}"#));
    }
    fn has(&self, key: &str) -> bool {
        self.inner.head(B, key).is_ok()
    }
    fn calls(&self, what: &str) -> usize {
        self.log.lock().unwrap().iter().filter(|l| l.starts_with(what)).count()
    }
    fn note(&self, op: &str, k: &str) {
        self.log.lock().unwrap().push(format!("{op} {k}"));
    }
}

impl ObjectBackend for Counting {
    fn put(&self, b: &str, k: &str, d: &[u8], n: bool) -> R<ObjectMeta> {
        self.note("put", k);
        self.inner.put(b, k, d, n)
    }
    fn put_if_match(&self, b: &str, k: &str, d: &[u8], e: &str) -> R<ObjectMeta> {
        self.note("put_if_match", k);
        self.inner.put_if_match(b, k, d, e)
    }
    fn delete_if_match(&self, b: &str, k: &str, e: &str) -> R<()> {
        self.note("delete_if_match", k);
        if !self.cond {
            return Err(BackendError::Unsupported("test backend".into()));
        }
        if let Some((key, body)) = self.replace_before_delete.lock().unwrap().take() {
            self.inner.put(B, &key, body.as_bytes(), false).unwrap();
        }
        self.inner.delete_if_match(b, k, e)
    }
    fn get(&self, b: &str, k: &str) -> R<Vec<u8>> {
        self.note("get", k);
        self.inner.get(b, k)
    }
    fn head(&self, b: &str, k: &str) -> R<ObjectMeta> {
        self.inner.head(b, k)
    }
    fn delete(&self, b: &str, k: &str) -> R<()> {
        self.note("delete", k);
        self.inner.delete(b, k)
    }
    fn copy(&self, sb: &str, sk: &str, db: &str, dk: &str) -> R<ObjectMeta> {
        self.inner.copy(sb, sk, db, dk)
    }
    fn list(&self, b: &str, p: &str) -> R<Vec<ObjectMeta>> {
        self.note("list", p);
        self.inner.list(b, p)
    }
}

fn opts(retention_ms: u64) -> Options {
    Options { collect_tombstones: true, tombstone_retention: Duration::from_millis(retention_ms), ..Options::default() }
}

#[test]
fn the_defaults_are_off_and_the_placeholder_retention() {
    let o = Options::default();
    assert!(!o.collect_tombstones);
    assert_eq!((o.tombstone_retention, o.tombstone_gc_cap), (PLACEHOLDER_RETENTION, DEFAULT_GC_CAP));
}

#[test]
fn switch_off_lists_reads_and_deletes_nothing() {
    let s = Counting::new();
    s.tomb("a.txt", 1);
    let off = Options { tombstone_retention: Duration::from_millis(1), ..Options::default() };
    let rep = collect_tombstones(&s, B, "proj", &off, 10 * HOUR_MS).unwrap();
    assert_eq!(rep, GcReport::default());
    assert!(s.log.lock().unwrap().is_empty(), "{:?}", s.log.lock().unwrap());
    assert!(s.has("proj/.drive-tombstones/a.txt"));
    // And through sync_up with no changes: no listing of the marker area, no report.
    s.seed("proj/f.txt", "x");
    let d = tempfile::tempdir().unwrap();
    materialise(&s, B, "proj", &d.path().join("t")).unwrap();
    s.log.lock().unwrap().clear();
    let up = sync_up_with(&s, &d.path().join("t"), &off).unwrap();
    assert!(up.gc.is_none() && up.gc_error.is_none());
    assert_eq!(s.calls("list"), 0);
    assert!(s.has("proj/.drive-tombstones/a.txt"));
}

#[test]
fn old_markers_are_collected_and_young_ones_kept() {
    let s = Counting::new();
    let now = 100 * HOUR_MS;
    s.tomb("old.txt", now - 5 * HOUR_MS);
    s.tomb("dir/older.txt", 1);
    s.tomb("young.txt", now - HOUR_MS / 2);
    let rep = collect_tombstones(&s, B, "proj", &opts(HOUR_MS), now).unwrap();
    assert_eq!(rep.collected, ["dir/older.txt", "old.txt"]);
    assert_eq!((rep.listed, rep.examined, rep.kept_young), (3, 3, 1));
    assert!(!s.has("proj/.drive-tombstones/old.txt") && !s.has("proj/.drive-tombstones/dir/older.txt"));
    assert!(s.has("proj/.drive-tombstones/young.txt"));
    // Idempotent: a second pass finds only the young one.
    let again = collect_tombstones(&s, B, "proj", &opts(HOUR_MS), now).unwrap();
    assert_eq!((again.listed, again.collected.len(), again.kept_young), (1, 0, 1));
}

#[test]
fn a_marker_replaced_between_the_listing_and_the_delete_survives() {
    let s = Counting::new();
    let now = 100 * HOUR_MS;
    s.tomb("a.txt", 1);
    s.tomb("b.txt", 1);
    let fresh = format!(r#"{{"device":"other","ts_ms":{now},"etag":"n"}}"#);
    *s.replace_before_delete.lock().unwrap() = Some(("proj/.drive-tombstones/a.txt".into(), fresh.clone()));
    let rep = collect_tombstones(&s, B, "proj", &opts(HOUR_MS), now).unwrap();
    assert_eq!((rep.raced, rep.collected.clone()), (1, vec!["b.txt".to_string()]));
    assert_eq!(String::from_utf8(s.inner.get(B, "proj/.drive-tombstones/a.txt").unwrap()).unwrap(), fresh);
}

#[test]
fn the_per_run_cap_holds_and_repeated_passes_drain_the_backlog() {
    let s = Counting::new();
    for i in 0..5 {
        s.tomb(&format!("f{i}.txt"), 1);
    }
    let o = Options { tombstone_gc_cap: 2, ..opts(HOUR_MS) };
    let r1 = collect_tombstones(&s, B, "proj", &o, 100 * HOUR_MS).unwrap();
    assert_eq!((r1.listed, r1.examined, r1.collected.len(), r1.capped), (5, 2, 2, true));
    assert_eq!(s.calls("get"), 2);
    collect_tombstones(&s, B, "proj", &o, 100 * HOUR_MS).unwrap();
    let r3 = collect_tombstones(&s, B, "proj", &o, 100 * HOUR_MS).unwrap();
    assert_eq!((r3.collected.len(), r3.capped), (1, false));
    assert_eq!(s.inner.list(B, "proj/.drive-tombstones/").unwrap().len(), 0);
}

#[test]
fn only_the_tombstone_prefix_of_this_drive_is_touched() {
    let s = Counting::new();
    let old = r#"{"device":"d","ts_ms":1,"etag":"e"}"#;
    for k in ["proj/a.txt", "proj/.drive-tombstones.bak/x", "proj/sub/.drive-tombstones/x", "proj2/.drive-tombstones/x", "other/.drive-tombstones/x", "proj/.drive-tombstones"] {
        s.seed(k, old);
    }
    s.tomb("x", 1);
    let rep = collect_tombstones(&s, B, "proj/", &opts(HOUR_MS), 100 * HOUR_MS).unwrap();
    assert_eq!(rep.collected, ["x"]);
    for l in s.log.lock().unwrap().iter() {
        assert!(l.contains("proj/.drive-tombstones/") || l == "list proj/.drive-tombstones/", "{l}");
    }
    assert_eq!(s.inner.list(B, "").unwrap().len(), 6);
}

#[test]
fn undated_markers_and_backends_without_a_conditional_delete_are_left_alone() {
    let mut s = Counting::new();
    s.seed("proj/.drive-tombstones/junk.txt", "not json");
    s.seed("proj/.drive-tombstones/nots.txt", r#"{"device":"d"}"#);
    let rep = collect_tombstones(&s, B, "proj", &opts(HOUR_MS), 100 * HOUR_MS).unwrap();
    assert_eq!((rep.undated, rep.collected.len()), (2, 0));
    s.tomb("a.txt", 1);
    s.cond = false;
    let rep = collect_tombstones(&s, B, "proj", &opts(HOUR_MS), 100 * HOUR_MS).unwrap();
    assert_eq!((rep.unsupported, rep.collected.len()), (1, 0));
    assert_eq!(s.calls("delete "), 0, "never a blind delete");
    assert!(s.has("proj/.drive-tombstones/a.txt"));
}

#[test]
fn sync_up_collects_behind_the_switch_and_keeps_the_marker_it_just_wrote() {
    let s = Counting::new();
    s.seed("proj/a.txt", "alpha");
    s.seed("proj/b.txt", "beta");
    s.tomb("ancient.txt", 1);
    let d = tempfile::tempdir().unwrap();
    let dest = d.path().join("t");
    materialise(&s, B, "proj", &dest).unwrap();
    std::fs::remove_file(dest.join("a.txt")).unwrap();
    let up = sync_up_with(&s, &dest, &opts(HOUR_MS)).unwrap();
    assert_eq!(up.deleted, ["a.txt"]);
    assert_eq!(up.gc.unwrap().collected, ["ancient.txt"]);
    assert!(s.has("proj/.drive-tombstones/a.txt") && !s.has("proj/.drive-tombstones/ancient.txt"));
}

fn conflicts_text(dest: &Path) -> String {
    std::fs::read_to_string(dest.join(DRIVE_DIR).join("conflicts.jsonl")).unwrap_or_default()
}

/// A device that materialised before a delete and syncs after the marker was collected: the
/// unmodified file is removed as before, an edited one is still a conflict and never re-uploaded;
/// the only loss is the deleter's name in the conflict record.
#[test]
fn a_device_that_syncs_after_collection_loses_only_the_deleters_name() {
    let s = Counting::new();
    s.seed("proj/keep.txt", "k");
    s.seed("proj/edit_a.txt", "one");
    s.seed("proj/edit_b.txt", "two");
    s.seed("proj/plain.txt", "p");
    let d = tempfile::tempdir().unwrap();
    let (a, b, c) = (d.path().join("a"), d.path().join("b"), d.path().join("c"));
    for p in [&a, &b, &c] {
        materialise(&s, B, "proj", p).unwrap();
    }
    for f in ["edit_a.txt", "edit_b.txt", "plain.txt"] {
        std::fs::remove_file(a.join(f)).unwrap();
    }
    sync_up(&s, &a).unwrap();
    // Control: b edits edit_a and syncs while the marker is there. The deleter is named.
    std::fs::write(b.join("edit_a.txt"), "b edits").unwrap();
    sync_down(&s, &b).unwrap();
    assert!(conflicts_text(&b).contains("remote deleted by "));
    // Collect everything: the markers are older than the window.
    let rep = collect_tombstones(&s, B, "proj", &opts(1), now_ms() + HOUR_MS).unwrap();
    assert_eq!(rep.collected.len(), 3);
    // Device c: one edited file, one untouched.
    std::fs::write(c.join("edit_b.txt"), "c edits").unwrap();
    let down = sync_down(&s, &c).unwrap();
    assert_eq!(down.conflicts, ["edit_b.txt"]);
    assert_eq!(sorted(down.removed), ["edit_a.txt", "plain.txt"]);
    assert!(!conflicts_text(&c).contains("remote deleted by "), "the name is gone with the marker");
    assert_eq!(std::fs::read_to_string(c.join("edit_b.txt")).unwrap(), "c edits");
    // And the edit is not silently pushed back up.
    let up = sync_up(&s, &c).unwrap();
    assert!(up.put.is_empty());
    assert!(!s.has("proj/edit_b.txt"));
}

fn sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v
}
