//! WO-069 lane D3 (card 6b58589c): the schema-2 publish, case by case. The conditional manifest write
//! and the post-check (design note B1), the sweep's kill switch, the retiring cap, clocks, bundle
//! retention, and moving a schema-1 bucket to schema 2 and back. The random interleavings are in
//! `s2_d3_gc_prop.rs`.
use std::cell::{Cell, RefCell};
use weftos_store::overlay::{ObjectBackend, ObjectMeta, Result as R};
use weftos_store::share::*;
use weftos_store::share_fetch::*;
use weftos_store::testing::MemBackend;

const WT: &str = "w-01abc-dddddddddddd";
const NOW: u64 = 1_000_000_000;
const G: u64 = DEFAULT_GRACE_MS;
const H: u64 = 60 * 60 * 1000;

fn who() -> Actor {
    Actor { agent: "writer".into(), host: "h".into() }
}

/// A tree on disk plus an optional bundle file.
struct Src {
    dir: tempfile::TempDir,
    paths: Vec<String>,
}
impl Src {
    fn new(files: &[(&str, &str)]) -> Self {
        let mut s = Src { dir: tempfile::tempdir().unwrap(), paths: vec![] };
        s.set(files);
        s
    }
    fn set(&mut self, files: &[(&str, &str)]) {
        self.paths.clear();
        for (p, c) in files {
            let f = self.dir.path().join("t").join(p);
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, c).unwrap();
            self.paths.push(p.to_string());
        }
    }
    fn publish<B: ObjectBackend>(&self, b: &B, head: &str, bundle: Option<&[u8]>, now: u64, o: &PublishOptions) -> R<PublishReport> {
        let bp = self.dir.path().join("b.bundle");
        if let Some(d) = bundle {
            std::fs::write(&bp, d).unwrap();
        }
        let src = TreeSource {
            root: &self.dir.path().join("t"),
            paths: &self.paths,
            uncommitted: &[],
            changes: &[],
            branch: "feat/d3",
            base_commit: "base",
            head_commit: head,
            bundle: bundle.map(|_| bp.as_path()),
            writer: who(),
        };
        publish_tree_opts(b, WT, &src, &Limits::default(), now, None, o).map(|p| p.report)
    }
}

fn opts() -> PublishOptions {
    PublishOptions::default()
}
fn zero() -> PublishOptions {
    PublishOptions { grace_ms: 0, bundle_grace_ms: 0, ..PublishOptions::default() }
}
fn manifest(b: &MemBackend) -> TreeManifest {
    read_manifest(b, WT).unwrap().unwrap()
}
fn pool(b: &MemBackend) -> Vec<String> {
    b.list(WT, OBJ_PREFIX).unwrap().into_iter().map(|o| o.key).collect()
}

/// Runs `before_manifest` just before the manifest `put_cas`; counts deletes.
struct Hooked<'a> {
    inner: &'a MemBackend,
    before_manifest: RefCell<Option<Box<dyn FnOnce() + 'a>>>,
    deletes: Cell<usize>,
}
impl<'a> Hooked<'a> {
    fn new(inner: &'a MemBackend, f: impl FnOnce() + 'a) -> Self {
        Hooked { inner, before_manifest: RefCell::new(Some(Box::new(f))), deletes: Cell::new(0) }
    }
}
impl ObjectBackend for Hooked<'_> {
    fn head(&self, b: &str, k: &str) -> R<Option<ObjectMeta>> {
        self.inner.head(b, k)
    }
    fn get(&self, b: &str, k: &str) -> R<Option<Vec<u8>>> {
        self.inner.get(b, k)
    }
    fn put(&self, b: &str, k: &str, d: &[u8]) -> R<ObjectMeta> {
        self.inner.put(b, k, d)
    }
    fn put_cas(&self, b: &str, k: &str, d: &[u8], e: Option<&str>) -> R<bool> {
        if k == MANIFEST_KEY {
            let f = self.before_manifest.borrow_mut().take();
            if let Some(f) = f {
                f();
            }
        }
        self.inner.put_cas(b, k, d, e)
    }
    fn delete(&self, b: &str, k: &str) -> R<()> {
        self.deletes.set(self.deletes.get() + 1);
        self.inner.delete(b, k)
    }
    fn list(&self, b: &str, p: &str) -> R<Vec<ObjectMeta>> {
        self.inner.list(b, p)
    }
}

#[test]
fn a_merge_while_a_publish_is_stalled_past_the_ttl_restores_the_previous_manifest_and_sweeps_nothing() {
    // Design note B1: a merger no longer counts the stalled publish's announcement as pending (it is
    // older than the TTL), so it merges the published tree. The publish's conditional manifest write
    // still succeeds (the merge does not touch the manifest); its post-check sees Merged and puts the
    // previous manifest back, conditionally, and fails without sweeping.
    let b = MemBackend::new();
    let mut s = Src::new(&[("docs/a.md", "# A1\n"), ("docs/b.md", "# B\n")]);
    s.publish(&b, "h1", None, NOW, &opts()).unwrap();
    let before = b.get(WT, MANIFEST_KEY).unwrap().unwrap();
    let m1 = manifest(&b);
    mark_handoff(&b, WT, &who(), "ready", NOW).unwrap();
    let m = Actor { agent: "m".into(), host: "x".into() };
    claim_merge(&b, WT, &m, NOW + CLAIM_SKEW_MS + 10).unwrap(); // not live for the writer: it may republish
    s.set(&[("docs/a.md", "# A2\n")]);
    let late = NOW + 1 + CLAIM_TTL_MS + 1;
    let h = Hooked::new(&b, || mark_merged(&b, WT, &m, "merged", late).unwrap());
    let e = s.publish(&h, "h2", None, NOW + 1, &zero()).unwrap_err().to_string();
    assert!(e.contains("became Merged") && e.contains("previous manifest was restored") && e.contains("nothing was swept"), "{e}");
    assert_eq!(b.get(WT, MANIFEST_KEY).unwrap().unwrap(), before, "the previous manifest, byte for byte");
    assert_eq!(h.deletes.get(), 0);
    for f in &m1.files {
        assert_eq!(sha256_hex(&b.get(WT, &m1.object_key(f)).unwrap().unwrap()), f.sha256, "{}", f.path);
    }
    assert_eq!(read_handoff(&b, WT).unwrap().unwrap().state, State::Merged);
}

#[test]
fn a_publish_that_loses_the_manifest_race_writes_and_sweeps_nothing_and_its_uploads_are_reused() {
    let b = MemBackend::new();
    let mut s = Src::new(&[("docs/a.md", "# A1\n"), ("docs/old.md", "# old\n")]);
    s.publish(&b, "h1", None, NOW, &opts()).unwrap();
    let other = Src::new(&[("docs/a.md", "# A-other\n")]);
    s.set(&[("docs/a.md", "# A2\n")]);
    let h = Hooked::new(&b, || {
        other.publish(&b, "h-other", None, NOW + 2, &opts()).unwrap();
    });
    let e = s.publish(&h, "h2", None, NOW + 1, &zero()).unwrap_err().to_string();
    assert!(e.contains("manifest changed while this publish ran"), "{e}");
    assert_eq!(h.deletes.get(), 0, "a lost race sweeps nothing");
    assert_eq!(manifest(&b).head_commit, "h-other");
    let i = read_handoff(&b, WT).unwrap().unwrap().publishing.unwrap();
    assert!(i.done && i.head == "h-other", "the winner's announcement stands, settled: {i:?}");
    // the rerun builds on the winner's manifest and re-uses the loser's upload (a stray of a newer
    // generation than the base, put by a publish built on it)
    let r = s.publish(&b, "h2", None, NOW + 3, &opts()).unwrap();
    assert_eq!((r.uploaded, r.unchanged), (0, 1), "{r:?}");
}

#[test]
fn the_kill_switch_retires_but_deletes_nothing_and_the_next_sweep_catches_up() {
    let b = MemBackend::new();
    let mut s = Src::new(&[("a.txt", "a1"), ("b.txt", "b1")]);
    s.publish(&b, "h1", None, NOW, &opts()).unwrap();
    s.set(&[("a.txt", "a2")]);
    let off = PublishOptions { sweep: false, ..zero() };
    let r = s.publish(&b, "h2", None, NOW + 1, &off).unwrap();
    assert_eq!((r.removed, r.retired, r.deferred), (0, 2, 2), "{r:?}");
    assert_eq!(pool(&b).len(), 3);
    let r = s.publish(&b, "h2", None, NOW + 2 * G, &off).unwrap();
    assert_eq!((r.removed, r.deferred), (0, 2), "still nothing deleted, however old");
    let r = s.publish(&b, "h2", None, NOW + 2 * G, &opts()).unwrap();
    assert_eq!(r.removed, 2);
    assert_eq!(pool(&b).len(), 1);
}

#[test]
fn over_the_retiring_cap_the_publish_fails_loudly_and_changes_nothing() {
    let b = MemBackend::new();
    let lim = Limits { max_files: 2, ..Limits::default() }; // cap: 4 retiring objects
    let mut s = Src::new(&[("a", "1"), ("b", "2")]);
    let publish = |s: &Src, head: &str, now| {
        let src = TreeSource { root: &s.dir.path().join("t"), paths: &s.paths, uncommitted: &[], changes: &[], branch: "f", base_commit: "x", head_commit: head, bundle: None, writer: who() };
        publish_tree_opts(&b, WT, &src, &lim, now, None, &opts())
    };
    publish(&s, "h1", NOW).unwrap();
    s.set(&[("a", "3"), ("b", "4")]);
    publish(&s, "h2", NOW + 1).unwrap(); // 2 retiring
    s.set(&[("a", "5"), ("b", "6")]);
    publish(&s, "h3", NOW + 2).unwrap(); // 4 retiring: at the cap
    let before = (b.get(WT, MANIFEST_KEY).unwrap(), b.get(WT, HANDOFF_KEY).unwrap());
    s.set(&[("a", "7"), ("b", "8")]);
    let e = publish(&s, "h4", NOW + 3).unwrap_err().to_string();
    assert!(e.contains("over the 4 limit") && e.contains("nothing was written"), "{e}");
    assert_eq!((b.get(WT, MANIFEST_KEY).unwrap(), b.get(WT, HANDOFF_KEY).unwrap()), before);
    // past the grace the old ones are swept and the same publish goes through
    publish(&s, "h4", NOW + 3 + G).unwrap();
}

#[test]
fn a_stamp_from_the_future_counts_as_now_and_a_carried_stamp_is_never_reset() {
    let b = MemBackend::new();
    let mut s = Src::new(&[("a", "1"), ("b", "2")]);
    let fast = NOW + 10 * H; // a writer whose clock is ten hours ahead
    s.publish(&b, "h1", None, NOW, &opts()).unwrap();
    s.set(&[("a", "1")]);
    s.publish(&b, "h2", None, fast, &opts()).unwrap();
    let since = manifest(&b).retiring[0].since_ms;
    assert_eq!(since, fast);
    // back on a correct clock: a full grace of correct time later the stamp is still in the future
    // relative to (fast + G), so nothing goes; the stamp is carried unchanged
    s.publish(&b, "h3", None, NOW + G, &opts()).unwrap();
    assert_eq!(manifest(&b).retiring[0].since_ms, fast, "carried, not reset");
    assert_eq!(pool(&b).len(), 2);
    s.publish(&b, "h4", None, NOW + G + 9 * H, &opts()).unwrap();
    assert_eq!(pool(&b).len(), 2, "still inside the grace measured from the stamp");
    let r = s.publish(&b, "h5", None, fast + G, &opts()).unwrap();
    assert_eq!(r.removed, 1);
    assert_eq!(pool(&b).len(), 1);
}

#[test]
fn the_two_newest_retiring_bundles_keep_the_full_grace_and_older_ones_go_after_an_hour() {
    let b = MemBackend::new();
    let s = Src::new(&[("a", "1")]);
    let mut keys = vec![];
    for (i, t) in [0u64, 10, 20, 30].iter().enumerate() {
        s.publish(&b, &format!("h{i}"), Some(format!("BUNDLE {i}").as_bytes()), NOW + t * 60_000, &opts()).unwrap();
        keys.push(manifest(&b).bundle_object().unwrap());
    }
    // bundles 0, 1, 2 are retiring; 3 is current
    let r = s.publish(&b, "h3", None, NOW + 30 * 60_000 + H, &opts()).unwrap();
    assert_eq!(r.removed, 1, "{r:?}");
    let have = |k: &String| b.head(WT, k).unwrap().is_some();
    assert_eq!(keys.iter().map(have).collect::<Vec<_>>(), [false, true, true, true], "bundle 0 is past the hour; 1 and 2 are the newest two");
    s.publish(&b, "h3", None, NOW + 30 * 60_000 + G, &opts()).unwrap();
    assert_eq!(keys.iter().map(have).collect::<Vec<_>>(), [false, false, false, true]);
}

#[test]
fn a_schema_1_bucket_moves_to_schema_2_keeps_its_old_objects_for_the_grace_and_old_readers_read_clean() {
    let b = MemBackend::new();
    let v1 = PublishOptions { schema: SCHEMA_V1, ..opts() };
    let mut s = Src::new(&[("docs/a.md", "# A1\n"), ("docs/b.md", "# B\n")]);
    s.publish(&b, "h1", Some(b"BUNDLE-1"), NOW, &v1).unwrap();
    let old = manifest(&b);
    assert_eq!((old.schema, old.object_key(&old.files[0]).as_str()), (SCHEMA_V1, ".wt/tree/docs/a.md"));
    // the first schema-2 publish, same head and no bundle file: the bundle is copied into the pool
    s.set(&[("docs/a.md", "# A2\n"), ("docs/b.md", "# B\n")]);
    let r = s.publish(&b, "h1", None, NOW + 1, &opts()).unwrap();
    assert_eq!((r.uploaded, r.removed, r.bundle_uploaded), (2, 0, true), "{r:?}");
    let m = manifest(&b);
    assert_eq!((m.schema, m.v1_retired_ms), (SCHEMA_V2, Some(NOW + 1)));
    let out = tempfile::tempdir().unwrap();
    assert_eq!(fetch_bundle(&b, WT, &out.path().join("x")).unwrap(), "h1");
    assert_eq!(std::fs::read(out.path().join("x")).unwrap(), b"BUNDLE-1");
    // a follower still holding the schema-1 manifest reads every file and the bundle clean
    let held = tempfile::tempdir().unwrap();
    let rep = fetch_tree_from(&b, WT, &old, held.path(), Select::All, false, &Limits::default()).unwrap();
    assert_eq!((rep.written.len(), rep.missing.len()), (2, 0));
    assert_eq!(std::fs::read(held.path().join("docs/a.md")).unwrap(), b"# A1\n");
    fetch_bundle_from(&b, WT, &old, &out.path().join("y")).unwrap();
    // the stamp is carried; a grace later the schema-1 objects go
    s.publish(&b, "h1", None, NOW + G, &opts()).unwrap();
    assert_eq!(manifest(&b).v1_retired_ms, Some(NOW + 1));
    assert!(!b.list(WT, TREE_PREFIX).unwrap().is_empty());
    let r = s.publish(&b, "h1", None, NOW + 1 + G, &opts()).unwrap();
    assert_eq!(r.removed, 3, "two tree objects and the old bundle key");
    assert!(b.list(WT, TREE_PREFIX).unwrap().is_empty() && b.head(WT, BUNDLE_KEY).unwrap().is_none());
    s.publish(&b, "h1", None, NOW + 2 + G, &opts()).unwrap();
    assert_eq!(manifest(&b).v1_retired_ms, None);
}

#[test]
fn the_schema_1_lever_writes_what_rc1_reads_and_still_sweeps_the_pool() {
    let b = MemBackend::new();
    let v1 = PublishOptions { schema: SCHEMA_V1, ..opts() };
    let s = Src::new(&[("docs/a.md", "# A\n"), ("src/x.rs", "x")]);
    s.publish(&b, "h1", Some(b"B1"), NOW, &opts()).unwrap();
    let pooled = pool(&b);
    assert_eq!(pooled.len(), 3);
    // roll back: this build writes schema 1 again (a full upload to `.wt/tree/`, the bundle key)
    let r = s.publish(&b, "h1", Some(b"B1"), NOW + 1, &v1).unwrap();
    assert_eq!((r.uploaded, r.bundle_uploaded, r.retired), (2, true, 3), "{r:?}");
    let raw: serde_json::Value = serde_json::from_slice(&b.get(WT, MANIFEST_KEY).unwrap().unwrap()).unwrap();
    assert_eq!(raw["schema"], 1);
    assert!(raw["files"].is_array() && raw.get("entries").is_none(), "the rc.1 shape: {raw}");
    let dest = tempfile::tempdir().unwrap();
    assert_eq!(fetch_tree(&b, WT, dest.path(), Select::All, false).unwrap().written.len(), 2);
    assert_eq!(b.get(WT, BUNDLE_KEY).unwrap().as_deref(), Some(&b"B1"[..]));
    // the pool the schema-2 manifest named is retired, kept for the grace, then swept by the lever too
    assert_eq!(pool(&b), pooled);
    s.publish(&b, "h1", None, NOW + 1 + G, &v1).unwrap();
    assert!(pool(&b).is_empty());
    // and forward again
    s.publish(&b, "h1", None, NOW + 2 + G, &opts()).unwrap();
    assert_eq!(manifest(&b).schema, SCHEMA_V2);
    assert!(fetch_tree(&b, WT, tempfile::tempdir().unwrap().path(), Select::All, false).unwrap().missing.is_empty());
}
