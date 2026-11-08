//! Scoped-view behaviour on the in-memory backend. Failure modes are in `tests_fail.rs`; the live
//! RustFS run is in `live_tests.rs`.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use super::*;
use crate::backend::{AdminBackend, BackendError, ObjectMeta};
use crate::drive::{ChangeKind, Keep, SkipWhy};
use crate::mem::MemBackend;

pub(super) const B: &str = "b-view";

/// The memory backend, logging puts, optionally unreachable, optionally listing hostile keys.
pub(super) struct Probe {
    pub inner: MemBackend,
    pub puts: Mutex<Vec<String>>,
    pub down: AtomicBool,
    pub ghosts: Vec<String>,
}

impl Probe {
    fn up(&self) -> std::result::Result<(), BackendError> {
        if self.down.load(Ordering::SeqCst) { Err(BackendError::Io("store unreachable (test)".into())) } else { Ok(()) }
    }
    pub fn seed(&self, key: &str, data: &str) {
        self.inner.put(B, key, data.as_bytes(), false).unwrap();
    }
    pub fn text(&self, key: &str) -> Option<String> {
        self.inner.get(B, key).ok().map(|d| String::from_utf8(d).unwrap())
    }
    /// Data puts (the lease object and tombstones left out).
    pub fn data_puts(&self) -> Vec<String> {
        let mut v: Vec<String> =
            self.puts.lock().unwrap().iter().filter(|k| !k.starts_with(".drive/") && !k.contains(".drive-tombstones/")).cloned().collect();
        v.sort();
        v
    }
    pub fn clear(&self) {
        self.puts.lock().unwrap().clear();
    }
}

impl ObjectBackend for Probe {
    fn put(&self, b: &str, k: &str, d: &[u8], n: bool) -> std::result::Result<ObjectMeta, BackendError> {
        self.up()?;
        self.puts.lock().unwrap().push(k.to_owned());
        self.inner.put(b, k, d, n)
    }
    fn put_if_match(&self, b: &str, k: &str, d: &[u8], e: &str) -> std::result::Result<ObjectMeta, BackendError> {
        self.up()?;
        self.puts.lock().unwrap().push(k.to_owned());
        self.inner.put_if_match(b, k, d, e)
    }
    fn delete_if_match(&self, b: &str, k: &str, e: &str) -> std::result::Result<(), BackendError> {
        self.up()?;
        self.inner.delete_if_match(b, k, e)
    }
    fn get(&self, b: &str, k: &str) -> std::result::Result<Vec<u8>, BackendError> {
        self.up()?;
        self.inner.get(b, k)
    }
    fn head(&self, b: &str, k: &str) -> std::result::Result<ObjectMeta, BackendError> {
        self.up()?;
        self.inner.head(b, k)
    }
    fn delete(&self, b: &str, k: &str) -> std::result::Result<(), BackendError> {
        self.up()?;
        self.inner.delete(b, k)
    }
    fn copy(&self, sb: &str, sk: &str, db: &str, dk: &str) -> std::result::Result<ObjectMeta, BackendError> {
        self.up()?;
        self.inner.copy(sb, sk, db, dk)
    }
    fn list(&self, b: &str, p: &str) -> std::result::Result<Vec<ObjectMeta>, BackendError> {
        self.up()?;
        let mut v = self.inner.list(b, p)?;
        v.extend(self.ghosts.iter().filter(|k| k.starts_with(p)).map(|k| ObjectMeta { key: k.clone(), size: 3, etag: "x".into() }));
        Ok(v)
    }
}

/// `proj/` (the work prefix) and `docs/` (a base include), plus things a view must never show.
pub(super) fn probe() -> Probe {
    let p = Probe { inner: MemBackend::new(), puts: Mutex::new(vec![]), down: AtomicBool::new(false), ghosts: vec![] };
    p.inner.create_bucket(B, &[]).unwrap();
    for (k, v) in [
        ("proj/a.txt", "alpha"),
        ("proj/dir/b.txt", "beta"),
        ("proj/d.txt", "delta"),
        ("docs/readme.md", "docs"),
        ("docs/guide/intro.md", "intro"),
        ("other/x.txt", "other"),
        (".drive/leases/zz.json", "{}"),
        ("proj/.drive-tombstones/old.txt", "{}"),
        ("docs/.drive/index.json", "{}"),
        ("docs/.git/config", "[core]"),
        ("docs/.env", "SECRET=1"),
        (".wt/tree/x.txt", "daemon"),
    ] {
        p.seed(k, v);
    }
    p
}

/// A resolved scratch directory (the view root must not sit under a symlink such as `/var`).
pub(super) fn tmp() -> (tempfile::TempDir, PathBuf) {
    let d = tempfile::tempdir().unwrap();
    let p = std::fs::canonicalize(d.path()).unwrap();
    (d, p)
}

pub(super) fn spec(base: Vec<Include>, work: Option<WorkSpec>) -> ViewSpec {
    ViewSpec {
        bucket: B.into(),
        base,
        work,
        deny: vec![],
        limits: ViewLimits::default(),
        lease_ttl_ms: DEFAULT_LEASE_TTL_MS,
        mount_points: vec![],
        device: Some("test-host".into()),
    }
}

pub(super) fn docs() -> Include {
    Include { prefix: "docs".into(), at: "docs".into() }
}

pub(super) fn rw(start: WorkStart) -> ViewSpec {
    spec(vec![docs()], Some(WorkSpec { prefix: "proj".into(), start }))
}

/// Every entry under `dir`, sorted; directories end in `/`.
pub(super) fn files(dir: &Path) -> Vec<String> {
    fn walk(d: &Path, rel: &str, out: &mut Vec<String>) {
        for e in std::fs::read_dir(d).unwrap() {
            let e = e.unwrap();
            let name = e.file_name().to_string_lossy().into_owned();
            let r = if rel.is_empty() { name } else { format!("{rel}/{name}") };
            if std::fs::symlink_metadata(e.path()).unwrap().is_dir() {
                out.push(format!("{r}/"));
                walk(&e.path(), &r, out);
            } else {
                out.push(r);
            }
        }
    }
    let mut v = Vec::new();
    walk(dir, "", &mut v);
    v.sort();
    v
}

pub(super) fn wr(p: &Path, s: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, s).unwrap();
}

pub(super) fn rd(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap()
}

fn mode(p: &Path) -> u32 {
    std::fs::symlink_metadata(p).unwrap().permissions().mode() & 0o777
}

fn paths(v: &[String]) -> Vec<&str> {
    v.iter().map(String::as_str).collect()
}

#[test]
fn open_places_only_the_included_prefixes_and_no_control_state() {
    let s = probe();
    let (_d, t) = tmp();
    let root = t.join("v");
    let (v, rep) = ScopedView::open(&s, &rw(WorkStart::Clone), &root).unwrap();
    assert_eq!(files(&root.join("base")), ["docs/", "docs/guide/", "docs/guide/intro.md", "docs/readme.md", "work/"]);
    assert_eq!(files(&root.join("work")), ["a.txt", "d.txt", "dir/", "dir/b.txt"]);
    let why = |p: &str| rep.skipped.iter().find(|(_, s)| s.path == p).map(|(_, s)| s.why);
    assert_eq!(why(".drive/index.json"), Some(SkipWhy::Reserved));
    assert_eq!(why(".git/config"), Some(SkipWhy::Dangerous));
    assert_eq!(why(".env"), Some(SkipWhy::Secret));
    let m = v.manifest();
    assert_eq!(m.binds[0], Bind::Ro { src: root.join("base"), dest: "/".into() });
    assert_eq!(m.binds[1], Bind::Rw { src: root.join("work"), dest: "/work".into() });
    assert_eq!(m.lease.as_ref().unwrap().holder, "test-host");
    let on_disk: ViewManifest = serde_json::from_slice(&std::fs::read(root.join("view.json")).unwrap()).unwrap();
    assert_eq!(on_disk.spec_hash, v.spec().hash());

    // The whole bucket as one include: other prefixes' tombstones, drive state, lease objects and
    // the daemon's `.wt/` area stay out.
    let (_d2, t2) = tmp();
    let whole = spec(vec![Include { prefix: String::new(), at: String::new() }], None);
    let (_v, rep) = ScopedView::open(&s, &whole, &t2.join("v")).unwrap();
    let base = files(&t2.join("v/base"));
    assert!(base.iter().all(|p| !p.contains(".drive") && !p.starts_with(".wt")), "{base:?}");
    assert!(base.contains(&"proj/a.txt".to_owned()) && base.contains(&"other/x.txt".to_owned()));
    assert!(rep.skipped.iter().any(|(_, s)| s.path == "proj/.drive-tombstones/old.txt" && s.why == SkipWhy::Reserved), "{:?}", rep.skipped);
}

#[test]
fn base_is_read_only_after_open() {
    let s = probe();
    let (_d, t) = tmp();
    let root = t.join("v");
    ScopedView::open(&s, &rw(WorkStart::Clone), &root).unwrap();
    let base = root.join("base");
    assert_eq!(mode(&base), 0o555);
    assert_eq!(mode(&base.join("docs/guide")), 0o555);
    assert_eq!(mode(&base.join("docs/readme.md")), 0o444);
    assert!(std::fs::write(base.join("docs/readme.md"), "x").is_err());
    assert!(std::fs::write(base.join("docs/new.md"), "x").is_err());
    assert_eq!(mode(&root.join("work/a.txt")), 0o644);
    std::fs::write(root.join("work/a.txt"), "edited").unwrap();
}

#[test]
fn a_read_only_view_takes_no_lease_and_refuses_publish() {
    let s = probe();
    let (_d, t) = tmp();
    let ro = spec(vec![docs()], None);
    let (mut v1, _) = ScopedView::open(&s, &ro, &t.join("one")).unwrap();
    let (_v2, _) = ScopedView::open(&s, &ro, &t.join("two")).unwrap();
    assert!(v1.lease().is_none() && v1.manifest().binds.len() == 1);
    s.clear();
    assert!(matches!(v1.publish(&s), Err(ViewError::ReadOnly)));
    assert!(s.puts.lock().unwrap().is_empty());
}

fn plant_lease(s: &Probe, acquired_ms: u64, expires_ms: u64) {
    let body = LeaseBody {
        id: "old".into(),
        holder: "other-host".into(),
        bucket: B.into(),
        prefix: "proj/".into(),
        acquired_ms,
        expires_ms,
    };
    s.inner.put(B, &lease_key(B, "proj/"), &serde_json::to_vec(&body).unwrap(), false).unwrap();
}

#[test]
fn a_live_lease_refuses_a_second_open_and_a_stale_one_is_taken_over() {
    let s = probe();
    let (_d, t) = tmp();
    let (v, _) = ScopedView::open(&s, &rw(WorkStart::Clone), &t.join("a")).unwrap();
    let err = ScopedView::open(&s, &rw(WorkStart::Clone), &t.join("b")).unwrap_err();
    assert!(matches!(&err, ViewError::LeaseHeld { holder, .. } if holder == "test-host"), "{err}");
    assert!(!t.join("b").exists(), "a refused open leaves nothing");
    v.close(&s, CloseMode::Discard).unwrap();

    let now = now_ms();
    plant_lease(&s, now - 3_600_000, now - 60_000);
    let (v, rep) = ScopedView::open(&s, &rw(WorkStart::Clone), &t.join("c")).unwrap();
    assert_eq!(rep.took_over.as_deref(), Some("other-host"));
    v.close(&s, CloseMode::Discard).unwrap();

    // Stamped 11 minutes ahead of our clock: not trusted as live.
    plant_lease(&s, now + 11 * 60_000, now + 40 * 60_000);
    let (_v, rep) = ScopedView::open(&s, &rw(WorkStart::Clone), &t.join("d")).unwrap();
    assert_eq!(rep.took_over.as_deref(), Some("other-host"));

    // 5 minutes ahead is within the skew allowance: live.
    let (_d2, t2) = tmp();
    let s2 = probe();
    plant_lease(&s2, now + 5 * 60_000, now + 40 * 60_000);
    assert!(matches!(ScopedView::open(&s2, &rw(WorkStart::Clone), &t2.join("e")), Err(ViewError::LeaseHeld { .. })));
}

#[test]
fn refresh_updates_the_base_and_unedited_work_and_leaves_edits_alone() {
    let s = probe();
    let (_d, t) = tmp();
    let root = t.join("v");
    let (mut v, _) = ScopedView::open(&s, &rw(WorkStart::Clone), &root).unwrap();
    wr(&root.join("work/a.txt"), "edited in the session");
    s.seed("docs/readme.md", "docs v2");
    s.seed("docs/new.md", "new doc");
    s.inner.delete(B, "docs/guide/intro.md").unwrap();
    s.seed("proj/a.txt", "alpha v2 remote");
    s.seed("proj/dir/b.txt", "beta v2 remote");
    s.seed("proj/c.txt", "gamma");
    s.inner.delete(B, "proj/d.txt").unwrap();
    s.clear();
    let r = v.refresh(&s).unwrap();
    assert!(s.puts.lock().unwrap().is_empty(), "refresh never writes the store");
    let base = root.join("base");
    assert_eq!(rd(&base.join("docs/readme.md")), "docs v2");
    assert_eq!(files(&base), ["docs/", "docs/new.md", "docs/readme.md", "work/"]);
    assert_eq!(mode(&base.join("docs/new.md")), 0o444);
    assert_eq!(mode(&base.join("docs")), 0o555);
    let work = root.join("work");
    assert_eq!(rd(&work.join("a.txt")), "edited in the session");
    assert_eq!(rd(&work.join("dir/b.txt")), "beta v2 remote");
    assert_eq!(rd(&work.join("c.txt")), "gamma");
    assert!(!work.join("d.txt").exists());
    assert_eq!(paths(&r.work_conflicts), ["a.txt"]);
    let mut updated = r.work_updated.clone();
    updated.sort();
    assert_eq!(paths(&updated), ["c.txt", "dir/b.txt"]);
    assert_eq!(paths(&r.work_removed), ["d.txt"]);

    // The edit made against the old base is a conflict at publish, not an overwrite.
    let p = v.publish(&s).unwrap();
    assert_eq!(paths(&p.up.conflicts), ["a.txt"]);
    assert_eq!(s.text("proj/a.txt").as_deref(), Some("alpha v2 remote"));
    assert_eq!(p.open_conflicts, 1);
}

#[test]
fn publish_goes_through_the_conditional_put_and_tombstones() {
    let s = probe();
    let (_d, t) = tmp();
    let root = t.join("v");
    let (mut v, _) = ScopedView::open(&s, &rw(WorkStart::Clone), &root).unwrap();
    wr(&root.join("work/a.txt"), "alpha edited");
    wr(&root.join("work/new/n.txt"), "created");
    std::fs::remove_file(root.join("work/d.txt")).unwrap();
    assert_eq!(v.status().unwrap().pending.len(), 3);
    s.clear();
    let p = v.publish(&s).unwrap();
    assert_eq!(s.data_puts(), ["proj/a.txt", "proj/new/n.txt"]);
    assert_eq!((p.up.put.len(), p.up.deleted.len(), p.up.conflicts.len()), (2, 1, 0), "{p:?}");
    assert_eq!((p.up.conditional, p.up.head_then_write), (2, 0), "modify and delete carry the etag");
    assert_eq!(s.text("proj/a.txt").as_deref(), Some("alpha edited"));
    assert!(s.text("proj/d.txt").is_none() && s.text("proj/.drive-tombstones/d.txt").is_some());
    assert!(v.status().unwrap().pending.is_empty());

    // Nothing changed since: no writes.
    s.clear();
    let p = v.publish(&s).unwrap();
    assert!(p.applied.is_empty() && s.data_puts().is_empty(), "{p:?}");

    // A stale etag: the remote moved after our last publish.
    wr(&root.join("work/a.txt"), "alpha edited twice");
    s.seed("proj/a.txt", "remote wins the race");
    let p = v.publish(&s).unwrap();
    assert_eq!(paths(&p.up.conflicts), ["a.txt"]);
    assert_eq!(s.text("proj/a.txt").as_deref(), Some("remote wins the race"));
    assert!(matches!(v.clone().close(&s, CloseMode::Publish), Err(ViewError::Unpublished(1))));
    assert!(root.join("view.json").exists(), "a refused close removes nothing");

    // Keep the session's version: the next publish writes it over the recorded remote version.
    v.resolve("a.txt", Keep::Local).unwrap();
    let p = v.publish(&s).unwrap();
    assert_eq!((p.up.put.len(), p.open_conflicts), (1, 0), "{p:?}");
    assert_eq!(s.text("proj/a.txt").as_deref(), Some("alpha edited twice"));
    let c = v.close(&s, CloseMode::Publish).unwrap();
    assert!(c.root_removed && c.lease_released == Some(true), "{c:?}");
    assert!(s.inner.head(B, &lease_key(B, "proj/")).is_err());
}

#[test]
fn keep_remote_makes_the_work_file_the_remote_version() {
    let s = probe();
    let (_d, t) = tmp();
    let root = t.join("v");
    let (mut v, _) = ScopedView::open(&s, &rw(WorkStart::Clone), &root).unwrap();
    wr(&root.join("work/a.txt"), "local");
    s.seed("proj/a.txt", "remote");
    assert_eq!(v.publish(&s).unwrap().open_conflicts, 1);
    v.resolve("a.txt", Keep::Remote).unwrap();
    assert_eq!(rd(&root.join("work/a.txt")), "remote");
    let p = v.publish(&s).unwrap();
    assert!(p.applied.is_empty() && p.open_conflicts == 0, "{p:?}");
}

#[test]
fn an_empty_work_tree_is_an_overlay_upper_whose_files_are_the_change_set() {
    let s = probe();
    let (_d, t) = tmp();
    let root = t.join("v");
    let (mut v, _) = ScopedView::open(&s, &rw(WorkStart::Empty), &root).unwrap();
    assert!(files(&root.join("work")).is_empty());
    assert_eq!(files(&root.join("lower")), ["a.txt", "d.txt", "dir/", "dir/b.txt"]);
    assert_eq!(mode(&root.join("lower/a.txt")), 0o444);
    assert!(matches!(&v.manifest().binds[1], Bind::Overlay { dest, .. } if dest == "/work"));
    // What overlayfs would leave in the upper: a modified copy and a new file.
    wr(&root.join("work/a.txt"), "alpha via overlay");
    wr(&root.join("work/dir/new.txt"), "new via overlay");
    wr(&root.join("work/d.txt"), "delta");
    s.clear();
    let p = v.publish(&s).unwrap();
    assert_eq!(s.data_puts(), ["proj/a.txt", "proj/dir/new.txt"], "an unchanged copy-up is not a change");
    assert_eq!(p.up.conditional, 1);
    s.clear();
    assert!(v.publish(&s).unwrap().applied.is_empty() && s.data_puts().is_empty());
    let r = v.refresh(&s).unwrap();
    assert!(r.work_not_refreshed && r.work.is_none());
    assert!(matches!(v.resolve("a.txt", Keep::Remote), Err(ViewError::Unsupported(_))));
}

#[test]
fn deny_patterns_keep_paths_out_of_the_view_and_out_of_publish() {
    let s = probe();
    s.seed("docs/build.log", "log");
    s.seed("docs/target/out.bin", "bin");
    s.seed("proj/cache/x", "x");
    let (_d, t) = tmp();
    let root = t.join("v");
    let mut sp = rw(WorkStart::Clone);
    sp.deny = vec!["*.log".into(), "target/".into(), "cache/".into()];
    let (mut v, rep) = ScopedView::open(&s, &sp, &root).unwrap();
    let base = files(&root.join("base"));
    assert!(!base.iter().any(|p| p.contains("build.log") || p.contains("target")), "{base:?}");
    assert!(!root.join("work/cache").exists());
    assert_eq!(rep.skipped.iter().filter(|(_, s)| s.why == SkipWhy::Denied).count(), 3, "{:?}", rep.skipped);
    wr(&root.join("work/run.log"), "session log");
    s.clear();
    let p = v.publish(&s).unwrap();
    assert!(s.data_puts().is_empty(), "{p:?}");
    assert!(p.skipped.iter().any(|k| k.path == "run.log" && k.why == SkipWhy::Denied));
}

#[test]
fn close_removes_exactly_the_view_and_releases_the_lease() {
    let s = probe();
    let (_d, t) = tmp();
    let root = t.join("v");
    wr(&t.join("sibling/keep.txt"), "untouched");
    let (v, _) = ScopedView::open(&s, &rw(WorkStart::Clone), &root).unwrap();
    wr(&root.join("stray.txt"), "not the view's");
    wr(&root.join("work/unpublished.txt"), "dropped by discard");
    let c = v.close(&s, CloseMode::Discard).unwrap();
    assert_eq!(c.lease_released, Some(true));
    assert!(!c.root_removed);
    assert_eq!(c.left, ["stray.txt"]);
    assert_eq!(files(&root), ["stray.txt"]);
    assert_eq!(rd(&t.join("sibling/keep.txt")), "untouched");
    assert!(s.inner.head(B, &lease_key(B, "proj/")).is_err());
    assert!(s.text("proj/unpublished.txt").is_none());
    let mut removed: Vec<_> = c.removed.iter().map(|p| p.strip_prefix(&root).unwrap().display().to_string()).collect();
    removed.sort();
    assert_eq!(removed, [".state", "base", "view.json", "work"]);
}

#[test]
fn a_view_reloads_from_disk() {
    let s = probe();
    let (_d, t) = tmp();
    let root = t.join("v");
    let (v, _) = ScopedView::open(&s, &rw(WorkStart::Clone), &root).unwrap();
    let mut again = ScopedView::load(&root).unwrap();
    assert_eq!(again.manifest().spec_hash, v.manifest().spec_hash);
    assert_eq!(again.lease(), v.lease());
    wr(&root.join("work/a.txt"), "from a reloaded view");
    again.publish(&s).unwrap();
    assert_eq!(s.text("proj/a.txt").as_deref(), Some("from a reloaded view"));
    assert!(matches!(ScopedView::load(&t.join("nothing")), Err(ViewError::NotAView(_))));
    assert!(matches!(ScopedView::open(&s, &rw(WorkStart::Clone), &root), Err(ViewError::AlreadyOpen(_))));
}

#[test]
fn bad_specs_are_refused_before_anything_is_created() {
    let s = probe();
    let (_d, t) = tmp();
    let cases = [
        spec(vec![], None),
        spec(vec![Include { prefix: "a".into(), at: String::new() }, docs()], None),
        spec(vec![Include { prefix: "a".into(), at: "x".into() }, Include { prefix: "b".into(), at: "x/y".into() }], None),
        spec(vec![Include { prefix: "a".into(), at: "../up".into() }], None),
        spec(vec![Include { prefix: "a".into(), at: "work".into() }], Some(WorkSpec { prefix: "p".into(), start: WorkStart::Clone })),
        spec(vec![Include { prefix: "a".into(), at: ".drive".into() }], None),
        ViewSpec { deny: vec!["**".into()], ..spec(vec![docs()], None) },
        ViewSpec { lease_ttl_ms: 0, ..rw(WorkStart::Clone) },
        ViewSpec { mount_points: vec!["a/b".into()], ..spec(vec![docs()], None) },
    ];
    for (i, sp) in cases.iter().enumerate() {
        let root = t.join(format!("v{i}"));
        assert!(matches!(ScopedView::open(&s, sp, &root), Err(ViewError::Spec(_))), "case {i}");
        assert!(!root.exists(), "case {i}");
    }
    let mut sp = spec(vec![Include { prefix: "proj".into(), at: String::new() }], None);
    sp.mount_points = vec!["dev".into(), "proc".into()];
    s.seed("proj/dev/null", "shadowed by the mount point");
    let (_v, rep) = ScopedView::open(&s, &sp, &t.join("ok")).unwrap();
    assert!(t.join("ok/base/dev").is_dir() && !t.join("ok/base/dev/null").exists());
    assert!(rep.skipped.iter().any(|(_, k)| k.path == "dev/null" && k.why == SkipWhy::Reserved));
    let _ = ChangeKind::Create;
}
