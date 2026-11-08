//! Drive tests on the in-memory backend. The live RustFS cycle is in `live_tests.rs`.

use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime};

use super::*;
use crate::backend::{AdminBackend, ObjectBackend, ObjectMeta};
use crate::mem::MemBackend;

const B: &str = "b-test";

/// A memory backend that counts and logs what the drive asks of it.
struct Spy {
    inner: MemBackend,
    gets: AtomicUsize,
    puts: Mutex<Vec<String>>,
    /// Extra keys `list` reports that the store would never accept (a hostile or buggy listing).
    ghosts: Vec<String>,
    /// False: the conditional writes answer `Unsupported`, like a backend that has none.
    cond: bool,
}

impl Spy {
    fn new() -> Self {
        let inner = MemBackend::new();
        inner.create_bucket(B, &[]).unwrap();
        Self { inner, gets: AtomicUsize::new(0), puts: Mutex::new(vec![]), ghosts: vec![], cond: true }
    }
    fn seed(&self, key: &str, data: &str) {
        self.inner.put(B, key, data.as_bytes(), false).unwrap();
    }
    fn reset(&self) {
        self.gets.store(0, Ordering::SeqCst);
        self.puts.lock().unwrap().clear();
    }
    fn gets(&self) -> usize {
        self.gets.load(Ordering::SeqCst)
    }
    fn put_keys(&self) -> Vec<String> {
        let mut v = self.puts.lock().unwrap().clone();
        v.sort();
        v
    }
    fn text(&self, key: &str) -> Option<String> {
        self.inner.get(B, key).ok().map(|d| String::from_utf8(d).unwrap())
    }
}

impl ObjectBackend for Spy {
    fn put(&self, b: &str, k: &str, d: &[u8], n: bool) -> std::result::Result<ObjectMeta, BackendError> {
        self.puts.lock().unwrap().push(k.to_owned());
        self.inner.put(b, k, d, n)
    }
    fn put_if_match(&self, b: &str, k: &str, d: &[u8], e: &str) -> std::result::Result<ObjectMeta, BackendError> {
        if !self.cond {
            return Err(BackendError::Unsupported);
        }
        self.puts.lock().unwrap().push(k.to_owned());
        self.inner.put_if_match(b, k, d, e)
    }
    fn delete_if_match(&self, b: &str, k: &str, e: &str) -> std::result::Result<(), BackendError> {
        if !self.cond {
            return Err(BackendError::Unsupported);
        }
        self.inner.delete_if_match(b, k, e)
    }
    fn get(&self, b: &str, k: &str) -> std::result::Result<Vec<u8>, BackendError> {
        self.gets.fetch_add(1, Ordering::SeqCst);
        self.inner.get(b, k)
    }
    fn head(&self, b: &str, k: &str) -> std::result::Result<ObjectMeta, BackendError> {
        self.inner.head(b, k)
    }
    fn delete(&self, b: &str, k: &str) -> std::result::Result<(), BackendError> {
        self.inner.delete(b, k)
    }
    fn copy(&self, sb: &str, sk: &str, db: &str, dk: &str) -> std::result::Result<ObjectMeta, BackendError> {
        self.inner.copy(sb, sk, db, dk)
    }
    fn list(&self, b: &str, p: &str) -> std::result::Result<Vec<ObjectMeta>, BackendError> {
        let mut v = self.inner.list(b, p)?;
        v.extend(self.ghosts.iter().filter(|k| k.starts_with(p)).map(|k| ObjectMeta { key: k.clone(), size: 3, etag: "x".into() }));
        Ok(v)
    }
}

fn rd(dest: &Path, rel: &str) -> String {
    std::fs::read_to_string(dest.join(rel)).unwrap()
}
fn wr(dest: &Path, rel: &str, s: &str) {
    let p = dest.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, s).unwrap();
}
fn lines(dest: &Path, f: &str) -> usize {
    std::fs::read_to_string(dest.join(DRIVE_DIR).join(f)).map_or(0, |s| s.lines().count())
}
fn sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v
}

/// Three objects under `proj/`; `dup.txt` has the same bytes as `a.txt`.
fn project() -> Spy {
    let s = Spy::new();
    s.seed("proj/a.txt", "alpha");
    s.seed("proj/dir/b.txt", "beta");
    s.seed("proj/dup.txt", "alpha");
    s
}

fn drive(s: &Spy) -> (tempfile::TempDir, std::path::PathBuf) {
    let d = tempfile::tempdir().unwrap();
    let dest = d.path().join("tree");
    materialise(s, B, "proj", &dest).unwrap();
    (d, dest)
}

#[test]
fn materialise_places_files_and_writes_the_index() {
    let s = project();
    let d = tempfile::tempdir().unwrap();
    let dest = d.path().join("tree");
    let rep = materialise(&s, B, "proj", &dest).unwrap();
    assert_eq!(sorted(rep.created.clone()), ["a.txt", "dir/b.txt", "dup.txt"]);
    assert_eq!((rd(&dest, "a.txt"), rd(&dest, "dir/b.txt")), ("alpha".into(), "beta".into()));
    let ix = Index::load(&dest).unwrap().unwrap();
    assert_eq!((ix.bucket.as_str(), ix.prefix.as_str()), (B, "proj/"));
    let e = &ix.entries["a.txt"];
    assert_eq!((e.size, e.hash.len(), e.etag.as_str()), (5, 64, ix.entries["dup.txt"].etag.as_str()));
    assert!(e.mtime_ns > 0);
    assert!(dest.join(DRIVE_DIR).join("cas").is_dir());
    // Two distinct contents, three objects: identical bytes are fetched once.
    assert_eq!((rep.fetched, s.gets()), (2, 2));
    assert!(rep.skipped.is_empty());
    // Editing a placed file never reaches the cache.
    wr(&dest, "a.txt", "edited");
    assert_eq!(Cas::new(&dest).read(&e.hash).unwrap(), b"alpha");
}

use super::cas::Cas;

#[test]
fn refresh_fetches_only_the_changed_object() {
    let s = project();
    let (_d, dest) = drive(&s);
    s.reset();
    let none = materialise(&s, B, "proj", &dest).unwrap();
    assert_eq!((s.gets(), none.fetched, none.unchanged), (0, 0, 3));
    assert!(none.created.is_empty() && none.updated.is_empty() && none.removed.is_empty());
    s.seed("proj/dir/b.txt", "beta two");
    s.seed("proj/new.txt", "fresh");
    s.reset();
    let r = materialise(&s, B, "proj", &dest).unwrap();
    assert_eq!((r.updated.clone(), r.created.clone()), (vec!["dir/b.txt".to_string()], vec!["new.txt".to_string()]));
    assert_eq!((s.gets(), r.unchanged), (2, 2));
    assert_eq!((rd(&dest, "dir/b.txt"), rd(&dest, "new.txt")), ("beta two".into(), "fresh".into()));
}

#[test]
fn refresh_records_a_remote_delete_and_prunes_the_empty_directory() {
    let s = project();
    let (_d, dest) = drive(&s);
    s.inner.delete(B, "proj/dir/b.txt").unwrap();
    let r = materialise(&s, B, "proj", &dest).unwrap();
    assert_eq!(r.removed, ["dir/b.txt"]);
    assert!(!dest.join("dir").exists());
    assert!(!Index::load(&dest).unwrap().unwrap().entries.contains_key("dir/b.txt"));
    assert!(std::fs::read_to_string(dest.join(DRIVE_DIR).join("events.jsonl")).unwrap().contains("\"remove\""));
}

#[test]
fn a_drive_stays_bound_to_its_prefix() {
    let s = project();
    let (_d, dest) = drive(&s);
    assert!(matches!(materialise(&s, B, "other", &dest), Err(DriveError::Invalid(_))));
    assert!(matches!(sync_down(&s, Path::new("/nonexistent-drive-dir")), Err(DriveError::Invalid(_))));
}

#[test]
fn watch_reports_after_the_quiet_period_and_not_before() {
    let s = project();
    let (_d, dest) = drive(&s);
    let cfg = WatchConfig { quiet: Duration::from_secs(2), ..Default::default() };
    let mut w = Watcher::new(&dest, cfg).unwrap();
    let t0 = Instant::now();
    assert!(w.poll(t0).unwrap().is_none(), "a clean tree reports nothing");
    wr(&dest, "c.txt", "created");
    assert!(w.poll(t0 + Duration::from_millis(100)).unwrap().is_none());
    assert!(w.poll(t0 + Duration::from_millis(2000)).unwrap().is_none(), "1.9 s after the change is not quiet yet");
    let got = w.poll(t0 + Duration::from_millis(2200)).unwrap().expect("quiet for 2.1 s");
    assert_eq!(got.iter().map(|c| (c.path.as_str(), c.kind)).collect::<Vec<_>>(), [("c.txt", ChangeKind::Create)]);
    assert!(w.poll(t0 + Duration::from_secs(5)).unwrap().is_none(), "reported once");

    // More activity restarts the clock; modify and delete are reported with the pending create.
    wr(&dest, "a.txt", "alpha changed");
    std::fs::remove_file(dest.join("dir/b.txt")).unwrap();
    assert!(w.poll(t0 + Duration::from_secs(6)).unwrap().is_none());
    assert!(w.poll(t0 + Duration::from_millis(7900)).unwrap().is_none());
    let got = w.poll(t0 + Duration::from_millis(8100)).unwrap().expect("quiet again");
    let kinds: Vec<_> = got.iter().map(|c| (c.path.as_str(), c.kind)).collect();
    assert_eq!(
        kinds,
        [("a.txt", ChangeKind::Modify), ("c.txt", ChangeKind::Create), ("dir/b.txt", ChangeKind::Delete)]
    );
}

#[test]
fn a_touch_with_the_same_content_is_not_a_modification() {
    let s = project();
    let (_d, dest) = drive(&s);
    let f = std::fs::OpenOptions::new().write(true).open(dest.join("a.txt")).unwrap();
    f.set_modified(SystemTime::now() + Duration::from_secs(30)).unwrap();
    drop(f);
    let ix = Index::load(&dest).unwrap().unwrap();
    let sc = scan(&dest, &ix, &Default::default()).unwrap();
    assert!(sc.changes.is_empty());
    assert_eq!(sc.touched.len(), 1);
    // A real edit that keeps the size is still caught by the hash.
    wr(&dest, "dup.txt", "ALPHA");
    let sc = scan(&dest, &ix, &Default::default()).unwrap();
    assert_eq!(sc.changes.iter().map(|c| c.path.as_str()).collect::<Vec<_>>(), ["dup.txt"]);
}

#[test]
fn watch_once_blocks_until_a_batch_is_quiet_or_times_out() {
    let s = project();
    let (_d, dest) = drive(&s);
    let cfg = WatchConfig { quiet: Duration::from_millis(150), poll: Duration::from_millis(20), ..Default::default() };
    assert!(watch_once(&dest, &cfg, Duration::from_millis(300)).unwrap().is_none());
    wr(&dest, "n.txt", "x");
    let got = watch_once(&dest, &cfg, Duration::from_secs(5)).unwrap().unwrap();
    assert_eq!(got.len(), 1);
}

#[test]
fn sync_up_puts_exactly_the_changed_files_and_marks_the_delete() {
    let s = project();
    let (d, dest) = drive(&s);
    wr(&dest, "a.txt", "alpha v2");
    wr(&dest, "new/n.txt", "new file");
    std::fs::remove_file(dest.join("dir/b.txt")).unwrap();
    s.reset();
    let r = sync_up(&s, &dest).unwrap();
    assert_eq!((sorted(r.put.clone()), r.deleted.clone()), (vec!["a.txt".to_string(), "new/n.txt".to_string()], vec!["dir/b.txt".to_string()]));
    // The unchanged dup.txt was not sent; the delete left a marker.
    assert_eq!(s.put_keys(), ["proj/.drive-tombstones/dir/b.txt", "proj/a.txt", "proj/new/n.txt"]);
    assert_eq!((s.text("proj/a.txt").as_deref(), s.text("proj/dir/b.txt")), (Some("alpha v2"), None));
    assert!(s.text("proj/.drive-tombstones/dir/b.txt").unwrap().contains("\"device\""));
    s.reset();
    let again = sync_up(&s, &dest).unwrap();
    assert!(again.put.is_empty() && again.deleted.is_empty() && s.put_keys().is_empty());

    // A second device sees the edit and the delete, and never the marker.
    let dest2 = d.path().join("tree2");
    let m = materialise(&s, B, "proj", &dest2).unwrap();
    assert_eq!(rd(&dest2, "a.txt"), "alpha v2");
    assert!(!dest2.join("dir").exists() && !dest2.join(TOMBSTONE_DIR).exists());
    assert!(m.skipped.is_empty());

    // Writing the key again clears the marker.
    wr(&dest, "dir/b.txt", "back");
    sync_up(&s, &dest).unwrap();
    assert!(s.text("proj/.drive-tombstones/dir/b.txt").is_none());
    assert_eq!(s.text("proj/dir/b.txt").as_deref(), Some("back"));
}

#[test]
fn sync_down_applies_a_remote_change_and_records_a_conflict_for_the_edited_file() {
    let s = project();
    let (_d, dest) = drive(&s);
    wr(&dest, "a.txt", "local edit");
    s.seed("proj/a.txt", "remote edit");
    s.seed("proj/dir/b.txt", "remote beta");
    let r = sync_down(&s, &dest).unwrap();
    assert_eq!(r.conflicts, ["a.txt"]);
    assert_eq!(r.updated, ["dir/b.txt"]);
    // Local kept, remote beside it, record appended, nothing picked silently.
    assert_eq!(rd(&dest, "a.txt"), "local edit");
    assert_eq!(rd(&dest, "dir/b.txt"), "remote beta");
    let ix = Index::load(&dest).unwrap().unwrap();
    let copy = ix.conflicts["a.txt"].remote_copy.clone().unwrap();
    assert!(copy.starts_with("a.txt.remote-") && copy.len() == "a.txt.remote-".len() + 8);
    assert_eq!(rd(&dest, &copy), "remote edit");
    let rec: serde_json::Value = serde_json::from_str(std::fs::read_to_string(dest.join(DRIVE_DIR).join("conflicts.jsonl")).unwrap().lines().next().unwrap()).unwrap();
    assert_eq!((rec["path"].as_str(), rec["kind"].as_str()), (Some("a.txt"), Some("both_modified")));

    // Running again does not re-record, does not overwrite, and sync_up leaves both files alone.
    let r2 = sync_down(&s, &dest).unwrap();
    assert!(r2.conflicts.is_empty() && r2.open_conflicts == 1);
    s.reset();
    let up = sync_up(&s, &dest).unwrap();
    assert!(up.put.is_empty() && s.put_keys().is_empty());
    assert_eq!(lines(&dest, "conflicts.jsonl"), 1);

    // Keep the local side: the next sync_up makes the remote match it, the copy is gone.
    resolve(&dest, "a.txt", Keep::Local).unwrap();
    assert!(!dest.join(&copy).exists());
    assert_eq!(sync_up(&s, &dest).unwrap().put, ["a.txt"]);
    assert_eq!(s.text("proj/a.txt").as_deref(), Some("local edit"));
}

#[test]
fn resolving_for_the_remote_side_takes_the_remote_bytes() {
    let s = project();
    let (_d, dest) = drive(&s);
    wr(&dest, "dir/b.txt", "mine");
    s.seed("proj/dir/b.txt", "theirs");
    sync_down(&s, &dest).unwrap();
    resolve(&dest, "dir/b.txt", Keep::Remote).unwrap();
    assert_eq!(rd(&dest, "dir/b.txt"), "theirs");
    s.reset();
    assert!(sync_up(&s, &dest).unwrap().put.is_empty());
    assert!(matches!(resolve(&dest, "dir/b.txt", Keep::Remote), Err(DriveError::Invalid(_))));
}

#[test]
fn a_remote_delete_over_a_local_edit_is_a_conflict_with_the_deleter_named() {
    let s = project();
    let (_d, dest) = drive(&s);
    wr(&dest, "dup.txt", "i edited this");
    s.inner.delete(B, "proj/dup.txt").unwrap();
    let r = sync_down(&s, &dest).unwrap();
    assert_eq!(r.conflicts, ["dup.txt"]);
    assert_eq!(rd(&dest, "dup.txt"), "i edited this");
    // Keep local: it is a new file again and goes back up.
    resolve(&dest, "dup.txt", Keep::Local).unwrap();
    assert_eq!(sync_up(&s, &dest).unwrap().put, ["dup.txt"]);
    assert_eq!(s.text("proj/dup.txt").as_deref(), Some("i edited this"));
}

#[test]
fn a_local_edit_over_a_remote_delete_is_found_by_sync_up_too() {
    let s = project();
    let (d, a) = drive(&s);
    let b = d.path().join("other");
    materialise(&s, B, "proj", &b).unwrap();
    std::fs::remove_file(a.join("dup.txt")).unwrap();
    sync_up(&s, &a).unwrap();
    wr(&b, "dup.txt", "b still edits it");
    let up = sync_up(&s, &b).unwrap();
    assert_eq!((up.put.len(), up.conflicts.clone()), (0, vec!["dup.txt".to_string()]));
    assert_eq!(rd(&b, "dup.txt"), "b still edits it");
    assert!(s.text("proj/dup.txt").is_none(), "the remote delete was not undone silently");
    let rec = std::fs::read_to_string(b.join(DRIVE_DIR).join("conflicts.jsonl")).unwrap();
    assert!(rec.contains("remote deleted by "));
}

#[test]
fn two_devices_creating_the_same_path_conflict_instead_of_overwriting() {
    let s = project();
    let (d, a) = drive(&s);
    let b = d.path().join("other");
    materialise(&s, B, "proj", &b).unwrap();
    wr(&a, "x.txt", "from a");
    wr(&b, "x.txt", "from b");
    assert_eq!(sync_up(&s, &a).unwrap().put, ["x.txt"]);
    let up = sync_up(&s, &b).unwrap();
    assert_eq!(up.conflicts, ["x.txt"]);
    assert_eq!((s.text("proj/x.txt").as_deref(), rd(&b, "x.txt").as_str()), (Some("from a"), "from b"));
    let ix = Index::load(&b).unwrap().unwrap();
    assert_eq!(ix.conflicts["x.txt"].kind, ConflictKind::BothCreated);
    assert_eq!(rd(&b, &ix.conflicts["x.txt"].remote_copy.clone().unwrap()), "from a");
    // Identical bytes on both sides are not a conflict.
    wr(&a, "same.txt", "same");
    wr(&b, "same.txt", "same");
    sync_up(&s, &a).unwrap();
    assert!(sync_up(&s, &b).unwrap().conflicts.is_empty());
}

#[test]
fn secret_names_are_refused_both_ways() {
    let s = project();
    s.seed("proj/.env", "TOKEN=1");
    s.seed("proj/cfg/prod.tfvars", "x=1");
    s.seed("proj/ok.txt", "fine");
    let d = tempfile::tempdir().unwrap();
    let dest = d.path().join("tree");
    let r = materialise(&s, B, "proj", &dest).unwrap();
    let mut why: Vec<_> = r.skipped.iter().map(|k| (k.path.as_str(), k.why)).collect();
    why.sort();
    assert_eq!(why, [(".env", SkipWhy::Secret), ("cfg/prod.tfvars", SkipWhy::Secret)]);
    assert!(!dest.join(".env").exists() && !dest.join("cfg").exists() && dest.join("ok.txt").exists());

    wr(&dest, ".env.local", "local token");
    wr(&dest, "id_ed25519", "k");
    wr(&dest, "innocent.txt", "-----BEGIN OPENSSH PRIVATE KEY-----\nabc\n");
    wr(&dest, "notes.txt", "notes");
    s.reset();
    let up = sync_up(&s, &dest).unwrap();
    assert_eq!(up.put, ["notes.txt"]);
    assert_eq!(s.put_keys(), ["proj/notes.txt"]);
    assert_eq!(up.skipped.iter().filter(|k| k.why == SkipWhy::Secret).count(), 3);
}

#[test]
fn hostile_listings_cannot_escape_the_tree_or_overwrite_drive_state() {
    let mut s = project();
    s.ghosts = ["proj/../../escape.txt", "proj/a/../../escape2.txt", "proj//double", "proj/.git/config", "proj/.drive/index.json", "proj/back\\slash", "proj/dir/"]
        .map(String::from)
        .to_vec();
    let d = tempfile::tempdir().unwrap();
    let dest = d.path().join("tree");
    let r = materialise(&s, B, "proj", &dest).unwrap();
    assert_eq!(r.skipped.len(), 7, "{:?}", r.skipped);
    assert!(r.skipped.iter().all(|k| matches!(k.why, SkipWhy::BadPath | SkipWhy::Dangerous | SkipWhy::Reserved)));
    assert!(!d.path().join("escape.txt").exists() && !d.path().join("escape2.txt").exists());
    assert!(!dest.join(".git").exists());
    assert!(Index::load(&dest).unwrap().is_some(), "index.json is still the drive's own");
    assert_eq!(r.created.len(), 3);
}

#[cfg(unix)]
#[test]
fn a_symlinked_parent_is_not_followed_and_symlinks_are_not_uploaded() {
    let s = project();
    s.seed("proj/link/x.txt", "through the link");
    let d = tempfile::tempdir().unwrap();
    let outside = d.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    let dest = d.path().join("tree");
    std::fs::create_dir(&dest).unwrap();
    std::os::unix::fs::symlink(&outside, dest.join("link")).unwrap();
    let r = materialise(&s, B, "proj", &dest).unwrap();
    assert!(!outside.join("x.txt").exists());
    assert!(r.skipped.iter().any(|k| k.path == "link/x.txt" && k.why == SkipWhy::Io), "{:?}", r.skipped);
    std::os::unix::fs::symlink(dest.join("a.txt"), dest.join("ln")).unwrap();
    s.reset();
    let up = sync_up(&s, &dest).unwrap();
    assert!(up.put.is_empty());
    assert!(up.skipped.iter().any(|k| k.path == "ln" && k.why == SkipWhy::Symlink));
}

#[test]
fn keys_that_differ_only_by_case_are_reported_not_merged() {
    let s = Spy::new();
    s.seed("p/README.md", "upper");
    s.seed("p/Readme.md", "mixed");
    let d = tempfile::tempdir().unwrap();
    let r = materialise(&s, B, "p", &d.path().join("t")).unwrap();
    assert_eq!(r.created, ["README.md"]);
    assert_eq!(r.skipped.iter().map(|k| (k.path.as_str(), k.why)).collect::<Vec<_>>(), [("Readme.md", SkipWhy::CaseCollision)]);
}

#[test]
fn the_size_ceilings_apply_both_ways() {
    let s = Spy::new();
    s.seed("p/small", "tiny");
    s.seed("p/big", &"x".repeat(100));
    s.seed("p/z", "z");
    let tight = Options { limits: crate::share::Limits { max_file: 10, max_files: 2, max_total: 1 << 20, max_bundle: 0 }, device: None, read_only: false, ..Options::default() };
    let d = tempfile::tempdir().unwrap();
    let dest = d.path().join("t");
    let r = materialise_with(&s, B, "p", &dest, &tight).unwrap();
    let mut why: Vec<_> = r.skipped.iter().map(|k| (k.path.as_str(), k.why)).collect();
    why.sort();
    assert_eq!(why, [("big", SkipWhy::TooLarge)]);
    assert_eq!(sorted(r.created.clone()), ["small", "z"]);
    wr(&dest, "huge.bin", &"y".repeat(50));
    wr(&dest, "extra", "e");
    let up = sync_up_with(&s, &dest, &tight).unwrap();
    assert!(up.skipped.iter().any(|k| k.path == "huge.bin" && k.why == SkipWhy::TooLarge));
    assert_eq!(up.put, ["extra"]);
}

#[cfg(feature = "drive-notify")]
#[test]
fn event_driven_watch_reports_a_change_made_while_it_waits() {
    let s = project();
    let (_d, dest) = drive(&s);
    let cfg = WatchConfig { quiet: Duration::from_millis(200), poll: Duration::from_secs(30), ..Default::default() };
    let d2 = dest.clone();
    // With a 30 s poll the quiet batch can only arrive in time if file events wake the loop.
    let t = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        wr(&d2, "ev.txt", "x");
    });
    let started = Instant::now();
    let got = watch_once_events(&dest, &cfg, Duration::from_secs(10)).unwrap();
    t.join().unwrap();
    assert_eq!(got.map(|c| c.len()), Some(1));
    assert!(started.elapsed() < Duration::from_secs(10));
}

fn fs_project() -> (tempfile::TempDir, crate::fs::FsBackend) {
    let root = tempfile::tempdir().unwrap();
    let be = crate::fs::FsBackend::open(&root.path().join("store")).unwrap();
    be.create_bucket(B, &[]).unwrap();
    be.put(B, "proj/a.txt", b"alpha", false).unwrap();
    be.put(B, "proj/dir/b.txt", b"beta", false).unwrap();
    (root, be)
}

#[test]
fn modify_and_delete_use_the_conditional_write_when_the_backend_has_one() {
    let (root, be) = fs_project();
    let dest = root.path().join("tree");
    materialise(&be, B, "proj", &dest).unwrap();
    wr(&dest, "a.txt", "alpha v2");
    std::fs::remove_file(dest.join("dir/b.txt")).unwrap();
    let r = sync_up(&be, &dest).unwrap();
    assert_eq!((r.put.clone(), r.deleted.clone()), (vec!["a.txt".to_string()], vec!["dir/b.txt".to_string()]));
    assert_eq!((r.conditional, r.head_then_write), (2, 0));
    assert_eq!(be.get(B, "proj/a.txt").unwrap(), b"alpha v2");
    assert!(be.get(B, "proj/dir/b.txt").is_err());
}

#[test]
fn a_conditional_modify_refused_by_the_store_becomes_a_conflict_and_leaves_the_remote_alone() {
    let (root, be) = fs_project();
    let dest = root.path().join("tree");
    materialise(&be, B, "proj", &dest).unwrap();
    wr(&dest, "a.txt", "mine");
    be.put(B, "proj/a.txt", b"theirs", false).unwrap();
    std::fs::remove_file(dest.join("dir/b.txt")).unwrap();
    be.put(B, "proj/dir/b.txt", b"beta v2", false).unwrap();
    let r = sync_up(&be, &dest).unwrap();
    assert_eq!((r.put.len(), r.deleted.len(), r.conditional), (0, 0, 0));
    assert_eq!(sorted(r.conflicts.clone()), ["a.txt", "dir/b.txt"]);
    assert_eq!(be.get(B, "proj/a.txt").unwrap(), b"theirs");
    assert_eq!(be.get(B, "proj/dir/b.txt").unwrap(), b"beta v2");
}

#[test]
fn a_backend_without_a_conditional_write_falls_back_to_head_then_write() {
    let mut s = project();
    s.cond = false;
    let (_d, dest) = drive(&s);
    wr(&dest, "a.txt", "alpha v2");
    std::fs::remove_file(dest.join("dir/b.txt")).unwrap();
    let r = sync_up(&s, &dest).unwrap();
    assert_eq!((r.put.clone(), r.deleted.clone()), (vec!["a.txt".to_string()], vec!["dir/b.txt".to_string()]));
    assert_eq!((r.conditional, r.head_then_write), (0, 2));
    assert_eq!(s.text("proj/a.txt").as_deref(), Some("alpha v2"));
}

#[test]
fn a_read_only_drive_refuses_sync_up_but_still_materialises_and_syncs_down() {
    let s = project();
    let d = tempfile::tempdir().unwrap();
    let dest = d.path().join("tree");
    let ro = Options { read_only: true, ..Options::default() };
    materialise_with(&s, B, "proj", &dest, &ro).unwrap();
    assert_eq!(rd(&dest, "a.txt"), "alpha");
    wr(&dest, "a.txt", "local edit");
    s.reset();
    let e = sync_up_with(&s, &dest, &ro).unwrap_err();
    assert!(matches!(e, DriveError::ReadOnly(_)), "{e}");
    assert!(e.to_string().contains("read-only"));
    assert!(s.put_keys().is_empty() && s.text("proj/a.txt").as_deref() == Some("alpha"));
    // A remote change still comes down, and the local edit to another path is not pushed.
    s.seed("proj/new.txt", "fresh");
    let r = sync_down_with(&s, &dest, &ro).unwrap();
    assert_eq!(r.created, ["new.txt"]);
    assert_eq!(rd(&dest, "new.txt"), "fresh");
    assert!(s.put_keys().is_empty());
}

/// Delete `dir/b.txt` from a fresh drive whose store already holds `tomb` for it; returns the tombstone after.
fn delete_over_tombstone(tomb: Option<&str>, cond: bool) -> Option<String> {
    let mut s = project();
    s.cond = cond;
    if let Some(t) = tomb {
        s.seed("proj/.drive-tombstones/dir/b.txt", t);
    }
    let (_d, dest) = drive(&s);
    std::fs::remove_file(dest.join("dir/b.txt")).unwrap();
    let r = sync_up(&s, &dest).unwrap();
    assert_eq!(r.deleted, ["dir/b.txt"]);
    s.text("proj/.drive-tombstones/dir/b.txt")
}

#[test]
fn a_stale_writer_does_not_overwrite_a_newer_tombstone() {
    let newer = r#"{"device":"other","ts_ms":99999999999999,"etag":"zzz"}"#;
    assert_eq!(delete_over_tombstone(Some(newer), true).as_deref(), Some(newer));
}

#[test]
fn an_older_tombstone_is_replaced_with_a_conditional_put_and_kept_when_the_backend_has_none() {
    let older = r#"{"device":"other","ts_ms":1,"etag":"zzz"}"#;
    let after = delete_over_tombstone(Some(older), true).unwrap();
    assert!(after.contains("\"device\"") && !after.contains("\"other\""), "{after}");
    // No conditional write: the existing marker stays rather than being overwritten blind.
    assert_eq!(delete_over_tombstone(Some(older), false).as_deref(), Some(older));
    // Nothing there: a plain create.
    assert!(delete_over_tombstone(None, true).is_some());
}
