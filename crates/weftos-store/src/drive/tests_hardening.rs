//! Hardening scenarios for the drive (races, tombstones, a lost index save). The killed watch loop
//! is in `tests_crash.rs`.
//!
//! Each scenario is a generic function over `ObjectBackend`, so the same code runs here on the
//! in-memory (and file-system) backend and, ignored, on a real RustFS in `live_hardening.rs`.
//! "Device B" is always a second drive directory, never a raw write, except where a test says so.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::cas::hash_of;
use super::*;
use crate::backend::{AdminBackend, ObjectBackend};
use crate::mem::MemBackend;

pub(super) const BK: &str = "b-harden";
pub(super) const QUIET_PLUS: Duration = Duration::from_millis(2100);

/// One prefix in one bucket plus a scratch directory for the devices.
pub(super) struct Rig<'a, B: ObjectBackend + ?Sized> {
    be: &'a B,
    bk: &'a str,
    pre: &'a str,
    tmp: tempfile::TempDir,
}

impl<'a, B: ObjectBackend + ?Sized> Rig<'a, B> {
    pub(super) fn new(be: &'a B, bk: &'a str, pre: &'a str) -> Self {
        Self { be, bk, pre, tmp: tempfile::tempdir().unwrap() }
    }
    pub(super) fn key(&self, rel: &str) -> String {
        format!("{}/{rel}", self.pre)
    }
    pub(super) fn seed(&self, rel: &str, s: &str) {
        self.be.put(self.bk, &self.key(rel), s.as_bytes(), false).unwrap();
    }
    pub(super) fn text(&self, rel: &str) -> Option<String> {
        self.be.get(self.bk, &self.key(rel)).ok().map(|d| String::from_utf8(d).unwrap())
    }
    pub(super) fn etag(&self, rel: &str) -> String {
        self.be.head(self.bk, &self.key(rel)).unwrap().etag
    }
    pub(super) fn tomb(&self, rel: &str) -> Option<serde_json::Value> {
        let d = self.be.get(self.bk, &format!("{}/{TOMBSTONE_DIR}/{rel}", self.pre)).ok()?;
        serde_json::from_slice(&d).ok()
    }
    pub(super) fn join(&self, name: &str) -> PathBuf {
        let d = self.tmp.path().join(name);
        materialise(self.be, self.bk, self.pre, &d).unwrap();
        d
    }
    /// Watch (clock moved past the quiet period), then `sync_up`.
    pub(super) fn up(&self, d: &Path) -> UpReport {
        assert!(!quiet_batch(d).is_empty());
        sync_up(self.be, d).unwrap()
    }
}

pub(super) fn quiet_batch(d: &Path) -> Vec<Change> {
    let mut w = Watcher::new(d, WatchConfig::default()).unwrap();
    let t = Instant::now();
    assert!(w.poll(t).unwrap().is_none(), "reported before the quiet period");
    w.poll(t + QUIET_PLUS).unwrap().unwrap_or_default()
}

pub(super) fn rd(d: &Path, rel: &str) -> String {
    std::fs::read_to_string(d.join(rel)).unwrap()
}
pub(super) fn wr(d: &Path, rel: &str, s: &str) {
    let p = d.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, s).unwrap();
}
pub(super) fn sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v
}
pub(super) fn rows(d: &Path) -> Vec<serde_json::Value> {
    let s = std::fs::read_to_string(d.join(DRIVE_DIR).join("conflicts.jsonl")).unwrap_or_default();
    s.lines().map(|l| serde_json::from_str(l).unwrap()).collect()
}
pub(super) fn copy_of(rel: &str, remote_text: &str) -> String {
    format!("{rel}.remote-{}", &hash_of(remote_text.as_bytes())[..8])
}

/// Both sides edited `x.txt` and `y.txt`; A holds a conflict on each. Resolve x for the remote and
/// y for the local side, then check that nothing was lost and the other device converges.
fn resolve_both_ways<B: ObjectBackend + ?Sized>(r: &Rig<'_, B>, a: &Path, b: &Path, (bx, by, ay): (&str, &str, &str)) {
    resolve(a, "x.txt", Keep::Remote).unwrap();
    resolve(a, "y.txt", Keep::Local).unwrap();
    assert_eq!(rd(a, "x.txt"), bx);
    assert!(!a.join(copy_of("x.txt", bx)).exists() && !a.join(copy_of("y.txt", by)).exists());
    let up = sync_up(r.be, a).unwrap();
    assert_eq!((up.put, up.conditional, up.conflicts.len()), (vec!["y.txt".to_owned()], 1, 0), "x must not be re-uploaded");
    assert_eq!((r.text("x.txt").as_deref(), r.text("y.txt").as_deref()), (Some(bx), Some(ay)));
    let dn = sync_down(r.be, b).unwrap();
    assert_eq!((dn.updated, dn.conflicts.len()), (vec!["y.txt".to_owned()], 0));
    assert_eq!((rd(b, "x.txt"), rd(b, "y.txt")), (bx.to_owned(), ay.to_owned()));
}

const OX: &str = "x original";
const OY: &str = "y original";
const BX: &str = "x from device B, longer";
const BY: &str = "y from device B, longer";
const AX: &str = "x edit A";
const AY: &str = "y edit A";

fn raced_pair<B: ObjectBackend + ?Sized>(r: &Rig<'_, B>) -> (PathBuf, PathBuf) {
    r.seed("x.txt", OX);
    r.seed("y.txt", OY);
    let (a, b) = (r.join("a"), r.join("b"));
    wr(&b, "x.txt", BX);
    wr(&b, "y.txt", BY);
    assert_eq!(r.up(&b).put.len(), 2);
    wr(&a, "x.txt", AX);
    wr(&a, "y.txt", AY);
    (a, b)
}

/// 1a: B's write lands first; A's watch then `sync_up` must be refused by the store and recorded.
pub(super) fn race_sync_up_first<B: ObjectBackend + ?Sized>(be: &B, bk: &str) {
    let r = Rig::new(be, bk, "r1");
    let (a, b) = raced_pair(&r);
    let up = r.up(&a);
    assert!(up.put.is_empty() && up.deleted.is_empty() && up.conditional == 0, "{up:?}");
    assert_eq!(sorted(up.conflicts), ["x.txt", "y.txt"]);
    assert_eq!((r.text("x.txt").as_deref(), r.text("y.txt").as_deref()), (Some(BX), Some(BY)), "B's writes survive");
    assert_eq!((rd(&a, "x.txt"), rd(&a, "y.txt")), (AX.to_owned(), AY.to_owned()), "A's edits are untouched");
    assert_eq!(rd(&a, &copy_of("x.txt", BX)), BX);
    let rw = rows(&a);
    assert_eq!(rw.len(), 2);
    assert!(rw.iter().all(|c| c["kind"] == "both_modified" && c["remote_copy"].is_string()), "{rw:?}");
    // A later sync_down leaves the open conflicts alone and adds no rows.
    let dn = sync_down(be, &a).unwrap();
    assert_eq!((dn.open_conflicts, dn.fetched, dn.created.len(), dn.updated.len()), (2, 0, 0, 0));
    assert_eq!(rows(&a).len(), 2);
    resolve_both_ways(&r, &a, &b, (BX, BY, AY));
}

/// 1b: A pulls first. `sync_down` writes the remote copy and the row; the watch and `sync_up`
/// that follow have nothing to upload for the held-open paths.
pub(super) fn race_sync_down_first<B: ObjectBackend + ?Sized>(be: &B, bk: &str) {
    let r = Rig::new(be, bk, "r2");
    let (a, b) = raced_pair(&r);
    let dn = sync_down(be, &a).unwrap();
    assert_eq!((sorted(dn.conflicts), dn.fetched), (vec!["x.txt".to_owned(), "y.txt".to_owned()], 2));
    assert_eq!(rd(&a, &copy_of("y.txt", BY)), BY);
    assert_eq!(rows(&a).len(), 2);
    let mut w = Watcher::new(&a, WatchConfig::default()).unwrap();
    let t = Instant::now();
    assert!(w.poll(t).unwrap().is_none() && w.poll(t + QUIET_PLUS).unwrap().is_none(), "held-open paths are not a batch");
    let up = sync_up(be, &a).unwrap();
    assert!(up.put.is_empty() && up.deleted.is_empty() && up.conflicts.is_empty() && up.conditional == 0, "{up:?}");
    assert_eq!(up.open_conflicts, 2);
    assert_eq!((r.text("x.txt").as_deref(), r.text("y.txt").as_deref()), (Some(BX), Some(BY)));
    resolve_both_ways(&r, &a, &b, (BX, BY, AY));
}

/// 1c: the remote change arrives inside A's quiet period, once before the batch is reported
/// (then `sync_up` is refused) and once with A pulling in the middle of the quiet period.
pub(super) fn race_in_the_quiet_period<B: ObjectBackend + ?Sized>(be: &B, bk: &str) {
    let r = Rig::new(be, bk, "r3");
    r.seed("x.txt", OX);
    r.seed("y.txt", OY);
    let (a, b) = (r.join("a"), r.join("b"));
    // x: edit, first poll, B lands, batch reported, sync_up refused.
    wr(&a, "x.txt", AX);
    let mut w = Watcher::new(&a, WatchConfig::default()).unwrap();
    let t0 = Instant::now();
    assert!(w.poll(t0).unwrap().is_none());
    wr(&b, "x.txt", BX);
    assert_eq!(r.up(&b).put, ["x.txt"]);
    assert!(w.poll(t0 + Duration::from_secs(1)).unwrap().is_none());
    let ch = w.poll(t0 + QUIET_PLUS).unwrap().expect("the batch is reported");
    assert_eq!(ch.iter().map(|c| c.path.as_str()).collect::<Vec<_>>(), ["x.txt"]);
    let up = sync_up(be, &a).unwrap();
    assert_eq!((up.conflicts, up.put.len(), up.conditional), (vec!["x.txt".to_owned()], 0, 0));
    assert_eq!(r.text("x.txt").as_deref(), Some(BX));
    assert_eq!(rd(&a, "x.txt"), AX);
    // y: edit, first poll, B lands, A pulls inside the quiet period: the batch dissolves.
    wr(&a, "y.txt", AY);
    let mut w = Watcher::new(&a, WatchConfig::default()).unwrap();
    let t0 = Instant::now();
    assert!(w.poll(t0).unwrap().is_none());
    wr(&b, "y.txt", BY);
    assert_eq!(r.up(&b).put, ["y.txt"]);
    let dn = sync_down(be, &a).unwrap();
    assert_eq!(dn.conflicts, ["y.txt"]);
    assert!(w.poll(t0 + QUIET_PLUS).unwrap().is_none(), "y is held open, not reported");
    let up = sync_up(be, &a).unwrap();
    assert!(up.put.is_empty() && up.conflicts.is_empty(), "{up:?}");
    assert_eq!(r.text("y.txt").as_deref(), Some(BY));
    assert_eq!(rows(&a).len(), 2);
    resolve(&a, "x.txt", Keep::Remote).unwrap();
    resolve(&a, "y.txt", Keep::Remote).unwrap();
    assert_eq!((rd(&a, "x.txt"), rd(&a, "y.txt")), (BX.to_owned(), BY.to_owned()));
    assert!(sync_up(be, &a).unwrap().put.is_empty());
}

/// 2: delete, recreate, delete again, with a stale device and a stale tombstone in the way.
pub(super) fn tombstone_cycle<B: ObjectBackend + ?Sized>(be: &B, bk: &str) {
    let r = Rig::new(be, bk, "t1");
    r.seed("x.txt", "x v0");
    r.seed("keep.txt", "keep");
    let (a, b, c) = (r.join("a"), r.join("b"), r.join("c"));
    let (dev_a, dev_b) = (Index::load(&a).unwrap().unwrap().device, Index::load(&b).unwrap().unwrap().device);
    let e0 = r.etag("x.txt");
    // A deletes x: remote object gone, create-only tombstone names A and the deleted version.
    std::fs::remove_file(a.join("x.txt")).unwrap();
    assert_eq!(r.up(&a).deleted, ["x.txt"]);
    assert!(r.text("x.txt").is_none());
    let t = r.tomb("x.txt").expect("tombstone");
    assert_eq!((t["device"].as_str(), t["etag"].as_str()), (Some(dev_a.as_str()), Some(e0.as_str())));
    // C is stale: it deletes its copy and stays offline.
    std::fs::remove_file(c.join("x.txt")).unwrap();
    // B follows the delete, then recreates x. The create clears the tombstone.
    assert_eq!(sync_down(be, &b).unwrap().removed, ["x.txt"]);
    wr(&b, "x.txt", "x v2 recreated");
    assert_eq!(r.up(&b).put, ["x.txt"]);
    assert!(r.tomb("x.txt").is_none(), "a create revives the key");
    let e2 = r.etag("x.txt");
    // A's next sync_down restores x; it does not delete it again.
    let dn = sync_down(be, &a).unwrap();
    assert_eq!((dn.created, dn.removed, dn.conflicts.len()), (vec!["x.txt".to_owned()], vec![], 0));
    assert_eq!(rd(&a, "x.txt"), "x v2 recreated");
    assert!(sync_up(be, &a).unwrap().put.is_empty());
    // A stale tombstone survives next to the live key (a crash between create and clear): it is
    // never consulted when the key exists.
    let ghost = r#"{"device":"ghost","ts_ms":1,"etag":"old"}"#;
    be.put(bk, &format!("t1/{TOMBSTONE_DIR}/x.txt"), ghost.as_bytes(), true).unwrap();
    let dn = sync_down(be, &a).unwrap();
    assert!(dn.removed.is_empty() && dn.conflicts.is_empty() && a.join("x.txt").exists(), "{dn:?}");
    let fresh = r.join("d");
    assert_eq!(rd(&fresh, "x.txt"), "x v2 recreated", "a new device materialises the live key");
    // C comes back: its delete is for version v0, the store holds v2. Conflict, nothing deleted,
    // the (older) tombstone is not touched by the refused delete.
    let up = sync_up(be, &c).unwrap_or_else(|e| panic!("{e}"));
    assert!(up.deleted.is_empty() && up.conflicts == ["x.txt"], "{up:?}");
    assert_eq!(r.text("x.txt").as_deref(), Some("x v2 recreated"));
    assert_eq!(r.tomb("x.txt").unwrap()["device"], "ghost");
    assert_eq!(rows(&c)[0]["kind"], "remote_modified_local_deleted");
    resolve(&c, "x.txt", Keep::Remote).unwrap();
    assert_eq!(rd(&c, "x.txt"), "x v2 recreated");
    assert!(sync_up(be, &c).unwrap().put.is_empty());
    // B deletes x again. A marker is already there (the ghost): older and for another version, so
    // it is replaced by a conditional put and now names B and v2.
    std::fs::remove_file(b.join("x.txt")).unwrap();
    assert_eq!(r.up(&b).deleted, ["x.txt"]);
    let t = r.tomb("x.txt").unwrap();
    assert_eq!((t["device"].as_str(), t["etag"].as_str()), (Some(dev_b.as_str()), Some(e2.as_str())), "{t}");
    // A follows the delete, recreates once more; B follows. The tombstone is cleared again.
    assert_eq!(sync_down(be, &a).unwrap().removed, ["x.txt"]);
    wr(&a, "x.txt", "x v3");
    assert_eq!(r.up(&a).put, ["x.txt"]);
    assert!(r.tomb("x.txt").is_none());
    let dn = sync_down(be, &b).unwrap();
    assert_eq!((dn.created, dn.conflicts.len()), (vec!["x.txt".to_owned()], 0));
    assert_eq!(rd(&b, "x.txt"), "x v3");
}

pub(super) fn mem() -> MemBackend {
    let m = MemBackend::new();
    m.create_bucket(BK, &[]).unwrap();
    m
}

#[test]
fn a_racing_remote_write_is_refused_and_becomes_a_conflict_when_sync_up_goes_first() {
    race_sync_up_first(&mem(), BK);
}

#[test]
fn a_racing_remote_write_found_by_sync_down_first_holds_the_path_open() {
    race_sync_down_first(&mem(), BK);
}

#[test]
fn a_remote_write_inside_the_quiet_period_never_overwrites_the_edit() {
    race_in_the_quiet_period(&mem(), BK);
}

#[test]
fn tombstones_survive_recreate_and_redelete_and_a_stale_device_cannot_win() {
    tombstone_cycle(&mem(), BK);
}
