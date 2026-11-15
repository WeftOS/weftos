//! WO-069 lane V: the two windows around `publish_tree` that finding 3a (card 6b58589c) does not yet cover, drilled deterministically.
//! W1: the git bundle is put/deleted BEFORE the manifest, so an interrupted publish leaves the old manifest pointing at the wrong bundle.
//! W2: no interruption needed, a follower that read manifest v1 while publish v2 SUCCEEDS sees overwritten and swept objects.
//! Interleaving is exact: a hook backend runs the whole publish v2 just before the Nth tree-object `head`, or fails the manifest put.
//! In-memory by default; `#[ignore]`d live twins run the same scenarios against `WEFTOS_STORE_TEST_RUSTFS_ENV`. Prints `DRILL|` / `FINDING|`.
//! Every assertion is on observed behaviour. Lane D3 (card 6b58589c, content-addressed objects) flipped the ones that marked
//! the defect: they now assert the old manifest is served clean through each window and print PASS lines (`DRILL|..|fixed|`).
#![allow(clippy::type_complexity)]

use std::cell::{Cell, RefCell};
use std::path::Path;
use weftos_store::follower::*;
use weftos_store::follower_cache::{ManifestCache, browse_get_cached};
use weftos_store::overlay::{ObjectBackend, ObjectMeta, S2Error};
use weftos_store::promote::{WorktreeMeta, write_meta};
use weftos_store::share::*;
use weftos_store::share_fetch::*;
use weftos_store::testing::MemBackend;

#[cfg(feature = "rustfs")]
#[path = "s2_drills_common/mod.rs"]
mod s2_drills_common;

const SLUG: &str = "feat";

/// An assertion of the fix for finding 3a (card 6b58589c, flipped by lane D3): before it, the opposite held.
macro_rules! fixed {
    ($cond:expr, $($arg:tt)+) => {
        assert!($cond, "finding 3a regressed (fixed by card 6b58589c): {}", format!($($arg)+))
    };
}

fn line(id: &str, step: &str, detail: impl std::fmt::Display) {
    println!("DRILL|{id}|{step}|PASS|{detail}");
}
/// A former finding that is now fixed (same shape as `s2_drills_fcommon::fixed`).
fn fixed_line(id: &str, detail: impl std::fmt::Display) {
    println!("DRILL|{id}|fixed|PASS|-|{detail}");
}

/// Runs `action` just before the Nth tree-object `head` (0-based), and/or fails the put of one key. Single-threaded.
struct Hook<'a, B: ObjectBackend> {
    inner: &'a B,
    fail_put_key: Option<&'static str>,
    tree_heads: Cell<usize>,
    tree_gets: Cell<usize>,
    fire_at: Option<usize>,
    /// Same as `fire_at` but counted on tree-object `get`s (what `fetch_tree` / `sync_docs` have in
    /// common whether they size objects by HEAD or by one listing).
    fire_at_get: Option<usize>,
    action: RefCell<Option<Box<dyn FnOnce() + 'a>>>,
}
impl<'a, B: ObjectBackend> Hook<'a, B> {
    fn new(inner: &'a B) -> Self {
        Self { inner, fail_put_key: None, tree_heads: Cell::new(0), tree_gets: Cell::new(0), fire_at: None, fire_at_get: None, action: RefCell::new(None) }
    }
    fn failing_put(inner: &'a B, key: &'static str) -> Self {
        Self { fail_put_key: Some(key), ..Self::new(inner) }
    }
    /// Runs `f` just before the `at_get`th tree-object `get` (0-based).
    fn firing_get(inner: &'a B, at_get: usize, f: impl FnOnce() + 'a) -> Self {
        Self { fire_at_get: Some(at_get), action: RefCell::new(Some(Box::new(f))), ..Self::new(inner) }
    }
    fn firing(inner: &'a B, at_head: usize, f: impl FnOnce() + 'a) -> Self {
        Self { fire_at: Some(at_head), action: RefCell::new(Some(Box::new(f))), ..Self::new(inner) }
    }
}
impl<B: ObjectBackend> ObjectBackend for Hook<'_, B> {
    fn head(&self, b: &str, k: &str) -> Result<Option<ObjectMeta>, S2Error> {
        if k.starts_with(TREE_PREFIX) || k.starts_with(OBJ_PREFIX) {
            let n = self.tree_heads.get();
            self.tree_heads.set(n + 1);
            if self.fire_at == Some(n) {
                let a = self.action.borrow_mut().take();
                if let Some(f) = a {
                    f();
                }
            }
        }
        self.inner.head(b, k)
    }
    fn get(&self, b: &str, k: &str) -> Result<Option<Vec<u8>>, S2Error> {
        if k.starts_with(TREE_PREFIX) || k.starts_with(OBJ_PREFIX) {
            let n = self.tree_gets.get();
            self.tree_gets.set(n + 1);
            if self.fire_at_get == Some(n) {
                let a = self.action.borrow_mut().take();
                if let Some(f) = a {
                    f();
                }
            }
        }
        self.inner.get(b, k)
    }
    fn put(&self, b: &str, k: &str, d: &[u8]) -> Result<ObjectMeta, S2Error> {
        if self.fail_put_key == Some(k) {
            return Err(S2Error::Backend(format!("injected: the writer died before putting {k}")));
        }
        self.inner.put(b, k, d)
    }
    fn delete(&self, b: &str, k: &str) -> Result<(), S2Error> {
        self.inner.delete(b, k)
    }
    fn list(&self, b: &str, p: &str) -> Result<Vec<ObjectMeta>, S2Error> {
        self.inner.list(b, p)
    }
    fn put_cas(&self, b: &str, k: &str, d: &[u8], e: Option<&str>) -> Result<bool, S2Error> {
        // the manifest write is a put_cas since lane D3
        if self.fail_put_key == Some(k) {
            return Err(S2Error::Backend(format!("injected: the writer died before putting {k}")));
        }
        self.inner.put_cas(b, k, d, e)
    }
}

/// A worktree tree on disk that can be republished.
struct Tree {
    dir: tempfile::TempDir,
    paths: Vec<String>,
}
impl Tree {
    fn new(files: &[&str]) -> Self {
        let mut t = Tree { dir: tempfile::tempdir().unwrap(), paths: vec![] };
        for f in files {
            t.write(f, &format!("# {f}\nfirst version of {f}\n"));
        }
        t
    }
    fn write(&mut self, p: &str, c: &str) {
        let f = self.dir.path().join(p);
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(f, c).unwrap();
        if !self.paths.iter().any(|x| x == p) {
            self.paths.push(p.into());
        }
    }
    fn remove(&mut self, p: &str) {
        std::fs::remove_file(self.dir.path().join(p)).unwrap();
        self.paths.retain(|x| x != p);
    }
    fn publish<B: ObjectBackend>(&self, b: &B, wb: &str, head: &str, bundle: Option<&Path>, now: u64) -> Result<PublishReport, S2Error> {
        let src = TreeSource {
            root: self.dir.path(), paths: &self.paths, uncommitted: &[], changes: &[], branch: SLUG, base_commit: "base",
            head_commit: head, bundle, writer: Actor { agent: "writer".into(), host: "m1".into() },
        };
        publish_tree(b, wb, &src, &Limits::default(), now)
    }
}

fn setup<B: ObjectBackend>(b: &B, wb: &str) {
    write_meta(b, wb, &WorktreeMeta { slug: SLUG.into(), project: "01abc".into(), branch: SLUG.into(), base_commit: "base".into(), created_ms: 1 }).unwrap();
}
fn register<B: ObjectBackend>(b: &B, pb: &str, wb: &str) -> RegistryEntry {
    let e = summarize(b, SLUG, wb).unwrap().unwrap();
    write_registry_entry(b, pb, &e).unwrap();
    e
}
fn bundle_file(bytes: &[u8]) -> tempfile::NamedTempFile {
    let f = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(f.path(), bytes).unwrap();
    f
}
const V2_A: &str = "# docs/a.md\nSECOND version, longer, edited in place\n";

/// Each v1 file as the objects stand now: clean / checksum-mismatch / missing.
fn classify<B: ObjectBackend>(b: &B, wb: &str, m1: &TreeManifest) -> Vec<(String, &'static str)> {
    m1.files
        .iter()
        .map(|f| {
            let s = match b.get(wb, &m1.object_key(f)).unwrap() {
                None => "missing",
                Some(d) if sha256_hex(&d) == f.sha256 => "clean",
                Some(_) => "checksum-mismatch",
            };
            (f.path.clone(), s)
        })
        .collect()
}
fn show(c: &[(String, &'static str)]) -> String {
    c.iter().map(|(p, s)| format!("{p}={s}")).collect::<Vec<_>>().join(",")
}
fn err_text<T: std::fmt::Debug, E: std::fmt::Display>(r: &Result<T, E>) -> String {
    match r {
        Ok(v) => format!("Ok({v:?})"),
        Err(e) => format!("Err({e})"),
    }
}

/// W1a/W1b: publish v1 with a bundle, cut v2's publish after the bundle put/delete and before the manifest put.
fn w1<B: ObjectBackend>(be: &B, pb: &str, wb: &str, v2_has_bundle: bool) {
    let id = if v2_has_bundle { "W1a" } else { "W1b" };
    setup(be, wb);
    let mut t = Tree::new(&["docs/a.md", "docs/b.md"]);
    let (b1, b2) = (bundle_file(b"BUNDLE-ONE"), bundle_file(b"BUNDLE-TWO-longer"));
    t.publish(be, wb, "h1", Some(b1.path()), 1_000).unwrap();
    register(be, pb, wb);
    let m1 = read_manifest(be, wb).unwrap().unwrap();
    assert!(m1.has_bundle && m1.head_commit == "h1" && m1.bundle_sha256.as_deref() == Some(sha256_hex(b"BUNDLE-ONE").as_str()));

    t.write("docs/a.md", V2_A);
    let cut = Hook::failing_put(be, MANIFEST_KEY);
    let r = t.publish(&cut, wb, "h2", v2_has_bundle.then(|| b2.path()), 2_000);
    assert!(r.is_err(), "{id}: the cut must surface: {r:?}");
    let m_now = read_manifest(be, wb).unwrap().unwrap();
    assert_eq!(m_now, m1, "{id}: the previous manifest is still served byte for byte");

    let out = tempfile::tempdir().unwrap();
    let fb = fetch_bundle(be, wb, &out.path().join("b.bundle"));
    let tree = classify(be, wb, &m1);
    let ftree = fetch_tree(be, wb, tempfile::tempdir().unwrap().path(), Select::All, false);
    // v2's bundle, when it had one, is in the pool already, under its own key: unreferenced
    let v2_bundle = obj_key(&sha256_hex(b"BUNDLE-TWO-longer"), m1.generation + 1);
    let v2_obj = be.get(wb, &v2_bundle).unwrap();
    println!("DETAIL|{id}|v2_bundle_object={:?}|manifest head={} bundle_sha_is_v1={}|fetch_bundle={}|tree {}|fetch_tree={}", v2_obj.as_ref().map(|o| String::from_utf8_lossy(o).to_string()), m_now.head_commit, m_now.bundle_sha256 == m1.bundle_sha256, err_text(&fb), show(&tree), err_text(&ftree.as_ref().map(|r| (r.written.len(), r.missing.len())).map_err(|e| S2Error::Backend(e.to_string()))));
    // finding 3a, fixed by card 6b58589c: nothing the old manifest names was touched
    fixed!(tree.iter().all(|(_, st)| *st == "clean"), "tree: {}", show(&tree));
    fixed!(ftree.as_ref().is_ok_and(|r| r.written.len() == 2 && r.missing.is_empty()), "fetch_tree with the old manifest is clean");
    fixed!(fb.as_ref().ok().map(String::as_str) == Some("h1"), "fetch_bundle with the old manifest: {}", err_text(&fb));
    fixed!(std::fs::read(out.path().join("b.bundle")).unwrap() == b"BUNDLE-ONE", "the old manifest serves v1's bundle");
    fixed!(v2_obj.is_some() == v2_has_bundle, "v2's bundle went to its own key, never over v1's");
    fixed_line(id, format!("old manifest serves v1's bundle (head h1) and every v1 file through a cut publish; fetch_bundle {}", err_text(&fb)));
    line(id, "publish cut between bundle step and manifest put; old manifest served", format!("fetch_bundle {}", err_text(&fb)));

    // the rerun completes, re-using what the cut publish uploaded
    let rr = t.publish(be, wb, "h2", v2_has_bundle.then(|| b2.path()), 3_000).unwrap();
    assert!(!rr.bundle_uploaded && rr.uploaded == 0, "{id}: the rerun re-uses the cut publish's uploads: {rr:?}");
    let fb2 = fetch_bundle(be, wb, &out.path().join("b2.bundle"));
    if v2_has_bundle {
        assert_eq!(fb2.as_ref().ok().map(String::as_str), Some("h2"), "{id}: rerun heals: {fb2:?}");
        assert_eq!(std::fs::read(out.path().join("b2.bundle")).unwrap(), b"BUNDLE-TWO-longer");
    } else {
        assert!(matches!(&fb2, Err(S2Error::Invalid(e)) if e.contains("without a git bundle")), "{id}: rerun: {fb2:?}");
    }
    line(id, "rerun of the publish completes it", err_text(&fb2));
}

/// The v2 change set of every W2 drill: docs/a.md edited in place, docs/b.md deleted, docs/e.md added.
fn publish_v2(be: &impl ObjectBackend, t: &mut Tree, wb: &str) {
    t.write("docs/a.md", V2_A);
    if t.paths.iter().any(|p| p == "docs/b.md") {
        t.remove("docs/b.md");
    }
    t.write("docs/e.md", "# e\nnew in v2\n");
    t.publish(be, wb, "h2", None, 2_000).unwrap();
}
fn v1<B: ObjectBackend>(be: &B, pb: &str, wb: &str, order: &[&str]) -> (Tree, TreeManifest, RegistryEntry) {
    setup(be, wb);
    let t = Tree::new(order);
    t.publish(be, wb, "h1", None, 1_000).unwrap();
    let e = register(be, pb, wb);
    let m1 = read_manifest(be, wb).unwrap().unwrap();
    (t, m1, e)
}

/// W2a: `fetch_tree` with a publish v2 completing mid-call (before the 2nd tree object is read: the 2nd tree-object `get`).
fn w2a<B: ObjectBackend>(be: &B, pb: &str, wb: &str, with_edit: bool) {
    let id = "W2a";
    let order: &[&str] = if with_edit { &["docs/c.md", "docs/a.md", "docs/b.md", "docs/d.md"] } else { &["docs/c.md", "docs/b.md", "docs/d.md"] };
    let (mut t, m1, _) = v1(be, pb, wb, order);
    let dest = tempfile::tempdir().unwrap();
    let r = {
        let t = RefCell::new(&mut t);
        let hook = Hook::firing_get(be, 1, || publish_v2(be, &mut t.borrow_mut(), wb));
        fetch_tree(&hook, wb, dest.path(), Select::All, false)
    };
    let per_file = classify(be, wb, &m1);
    let on_disk: Vec<String> = ["c", "a", "b", "d"].iter().map(|n| format!("docs/{n}.md")).filter(|p| dest.path().join(p).exists()).collect();
    println!("DETAIL|{id}|edit={with_edit}|per v1 file after the publish {}|fetch_tree={}|on disk {on_disk:?}", show(&per_file), match &r {
        Ok(rep) => format!("Ok(written={:?} missing={:?})", rep.written, rep.missing),
        Err(e) => format!("Err({e})"),
    });
    // finding 3a, fixed by card 6b58589c: a follower holding manifest v1 reads every v1 file clean
    fixed!(per_file.iter().all(|(_, st)| *st == "clean"), "{}", show(&per_file));
    let rep = r.as_ref().map_err(|e| e.to_string());
    fixed!(rep.as_ref().is_ok_and(|rep| rep.missing.is_empty() && rep.written.len() == order.len()), "fetch_tree completes from manifest v1: {:?}", rep.as_ref().map(|r| (&r.written, &r.missing)));
    fixed_line(id, format!("edit={with_edit}: a fetch across a completed publish reads every v1 file clean ({} written, none missing)", order.len()));
    // the next fetch reads manifest v2: the added file arrives; a fetch never clobbers or deletes local
    // files, so the edited one is a conflict (local v1 copy) and the deleted one stays on disk
    let r2 = fetch_tree(be, wb, dest.path(), Select::All, false).unwrap();
    assert!(r2.missing.is_empty(), "{id}: rerun is clean: {r2:?}");
    assert!(dest.path().join("docs/e.md").exists());
    assert_eq!(r2.conflicts, if with_edit { vec!["docs/a.md".to_string()] } else { vec![] }, "{id}: {r2:?}");
    line(id, if with_edit { "mid-fetch publish with an edited and a deleted file" } else { "mid-fetch publish with a deleted file only" }, format!("rerun written={:?}", r2.written));
}

/// W2b: `sync_docs` with the same mid-call publish.
fn w2b<B: ObjectBackend>(be: &B, pb: &str, wb: &str, with_edit: bool) {
    let id = "W2b";
    let order: &[&str] = if with_edit { &["docs/c.md", "docs/a.md", "docs/b.md", "docs/d.md"] } else { &["docs/c.md", "docs/b.md", "docs/d.md"] };
    let (mut t, _m1, e) = v1(be, pb, wb, order);
    let cache = tempfile::tempdir().unwrap();
    let r = {
        let t = RefCell::new(&mut t);
        let hook = Hook::firing_get(be, 1, || publish_v2(be, &mut t.borrow_mut(), wb));
        sync_docs(&hook, &e, cache.path())
    };
    let mirror = cache.path().join(SLUG);
    let have = |n: &str| mirror.join(format!("docs/{n}.md")).exists();
    let mirror_docs: Vec<&str> = ["a", "b", "c", "d", "e"].into_iter().filter(|n| have(n)).collect();
    let index = std::fs::read_to_string(mirror.join(".wt-index.json")).unwrap_or_default();
    println!("DETAIL|{id}|edit={with_edit}|sync_docs={}|mirror has {mirror_docs:?}|index {index}", match &r {
        Ok(rep) => format!("Ok(written={:?} missing={:?})", rep.written, rep.missing),
        Err(e) => format!("Err({e})"),
    });
    // finding 3a, fixed by card 6b58589c: the mirror of manifest v1 is complete, nothing silently lacking
    let mut want_sorted: Vec<&str> = order.iter().map(|p| &p[5..6]).collect();
    want_sorted.sort();
    fixed!(r.as_ref().is_ok_and(|rep| rep.missing.is_empty()), "sync_docs: {}", err_text(&r.as_ref().map(|r| r.written.len())));
    fixed!(mirror_docs == want_sorted, "mirror {mirror_docs:?}, want {want_sorted:?}");
    fixed_line(id, format!("edit={with_edit}: sync_docs across a completed publish mirrors every v1 doc ({mirror_docs:?})"));
    let r2 = sync_docs(be, &e, cache.path()).unwrap();
    let healed: Vec<&str> = ["a", "b", "c", "d", "e"].into_iter().filter(|n| have(n)).collect();
    assert!(r2.missing.is_empty(), "{id}: next sync is clean: {r2:?}");
    assert_eq!(healed, ["a", "c", "d", "e"], "{id}: v2 docs, a (edited or added) and e (added) included, b gone");
    line(id, if with_edit { "sync_docs mid-publish with an edited and a deleted doc" } else { "sync_docs mid-publish with a deleted doc only" }, format!("next sync heals: mirror {healed:?}"));
}

/// W2c: `browse_get` and `browse_get_cached` across a publish v2.
fn w2c<B: ObjectBackend>(be: &B, pb: &str, wb: &str) {
    let id = "W2c";
    let (mut t, m1, _) = v1(be, pb, wb, &["docs/a.md", "docs/b.md", "docs/c.md"]);
    // uncached: the manifest is read inside the call, the publish lands before the object is read
    let t2 = RefCell::new(&mut t);
    let hook = Hook::firing(be, 0, || publish_v2(be, &mut t2.borrow_mut(), wb));
    let ra = browse_get(&hook, pb, SLUG, "docs/a.md", 1 << 20);
    drop(hook);
    let rb = browse_get(be, pb, SLUG, "docs/b.md", 1 << 20);
    let rc = browse_get(be, pb, SLUG, "docs/c.md", 1 << 20);
    println!("DETAIL|{id}|uncached, publish inside the call: a={} b(after, plain)={} c={}", err_text(&ra.as_ref().map(|f| f.size)), err_text(&rb.as_ref().map(|f| f.size)), err_text(&rc.as_ref().map(|f| f.size)));
    // finding 3a, fixed by card 6b58589c: the manifest read inside the call is v1, and so are the bytes
    fixed!(ra.as_ref().is_ok_and(|f| f.sha256 == m1.files[0].sha256 && f.data.is_some()), "edited file mid-call: {}", err_text(&ra.as_ref().map(|f| f.size)));
    fixed_line(id, "browse_get across a completed publish serves v1's bytes of the edited file");
    assert!(rc.is_ok() && rb.is_err(), "after the publish: c clean, b gone from the new manifest");
    line(id, "browse_get: edited file read across a completed publish", err_text(&ra.as_ref().map(|f| f.size)));

    // uncached, deleted file: fresh rig on its own bucket pair is not available generically, so reuse v1's tree by republishing the v1 content
    let mut t = Tree::new(&["docs/a.md", "docs/b.md", "docs/c.md"]);
    t.publish(be, wb, "h1", None, 3_000).unwrap(); // back to v1 content (a, b, c first versions)
    register(be, pb, wb);
    let t3 = RefCell::new(&mut t);
    let hook = Hook::firing(be, 0, || publish_v2(be, &mut t3.borrow_mut(), wb));
    let rb2 = browse_get(&hook, pb, SLUG, "docs/b.md", 1 << 20);
    drop(hook);
    println!("DETAIL|{id}|uncached deleted file, publish inside the call: b={}", err_text(&rb2.as_ref().map(|f| f.size)));
    // finding 3a, fixed by card 6b58589c: the deleted file's object is retired, not gone
    fixed!(rb2.as_ref().is_ok_and(|f| f.data.is_some()), "{}", err_text(&rb2.as_ref().map(|f| f.size)));
    fixed_line(id, "browse_get across a completed publish serves v1's bytes of the deleted file");
    line(id, "browse_get: deleted file read across a completed publish", err_text(&rb2.as_ref().map(|f| f.size)));

    // cached: warm the cache on manifest v1, publish v2 behind it, read inside the TTL
    let (mut t, _m, _) = {
        let t = Tree::new(&["docs/a.md", "docs/b.md", "docs/c.md"]);
        t.publish(be, wb, "h1", None, 4_000).unwrap();
        let e = register(be, pb, wb);
        let m = read_manifest(be, wb).unwrap().unwrap();
        (t, m, e)
    };
    let mut cache = ManifestCache::new(60_000, 16, 1 << 20);
    let warm = browse_get_cached(be, &mut cache, 1_000, pb, SLUG, "docs/a.md", 1 << 20).unwrap();
    assert_eq!(warm.sha256, m1.files[0].sha256);
    publish_v2(be, &mut t, wb);
    let ca = browse_get_cached(be, &mut cache, 2_000, pb, SLUG, "docs/a.md", 1 << 20);
    let cb = browse_get_cached(be, &mut cache, 2_000, pb, SLUG, "docs/b.md", 1 << 20);
    let cc = browse_get_cached(be, &mut cache, 2_000, pb, SLUG, "docs/c.md", 1 << 20);
    println!("DETAIL|{id}|cached (manifest v1, fresh slot) after publish v2: a={} b={} c={}", err_text(&ca.as_ref().map(|f| f.data.clone().map(|d| d.len()))), err_text(&cb.as_ref().map(|f| f.size)), err_text(&cc.as_ref().map(|f| f.size)));
    // Lane D3 (owner decision: accept up to one cache TTL of previous-version bytes): a fresh cached
    // slot serves manifest v1's objects, which still exist, consistent with its cached listing.
    assert_eq!(ca.as_ref().ok().map(|f| f.sha256.clone()), Some(m1.files[0].sha256.clone()), "the cached slot serves v1's a");
    assert!(cb.as_ref().is_ok_and(|f| f.data.is_some()), "v1's b, retired, still served: {}", err_text(&cb.as_ref().map(|f| f.size)));
    assert!(cc.is_ok());
    line(id, "browse_get_cached: stale fresh slot, publish v2 behind it", "edited -> v1 bytes (cached snapshot), deleted -> v1 bytes, unchanged clean");

    // cached, cold slot loaded mid-window: no retry on a cold load
    let (mut t, _m, _) = {
        let t = Tree::new(&["docs/a.md", "docs/b.md", "docs/c.md"]);
        t.publish(be, wb, "h1", None, 5_000).unwrap();
        let e = register(be, pb, wb);
        let m = read_manifest(be, wb).unwrap().unwrap();
        (t, m, e)
    };
    let t4 = RefCell::new(&mut t);
    let hook = Hook::firing(be, 0, || publish_v2(be, &mut t4.borrow_mut(), wb));
    let mut cold = ManifestCache::new(60_000, 16, 1 << 20);
    let rcold = browse_get_cached(&hook, &mut cold, 1_000, pb, SLUG, "docs/a.md", 1 << 20);
    println!("DETAIL|{id}|cached, cold load then publish inside the call: a={}", err_text(&rcold.as_ref().map(|f| f.size)));
    // finding 3a, fixed by card 6b58589c: a cold load straddling a publish reads v1's object clean
    fixed!(rcold.as_ref().is_ok_and(|f| f.data.is_some()), "{}", err_text(&rcold.as_ref().map(|f| f.size)));
    fixed_line(id, "browse_get_cached: a cold load that straddles a publish reads clean (no retry needed)");
    line(id, "browse_get_cached: cold load straddling a publish", err_text(&rcold.as_ref().map(|f| f.size)));
}

fn mem() -> (MemBackend, &'static str, &'static str) {
    (MemBackend::new(), "p-01abc", "w-feat")
}
#[test]
fn w1a_bundle_put_before_manifest_old_manifest_served() {
    let (be, pb, wb) = mem();
    w1(&be, pb, wb, true);
}
#[test]
fn w1b_bundle_deleted_before_manifest_old_manifest_served() {
    let (be, pb, wb) = mem();
    w1(&be, pb, wb, false);
}
#[test]
fn w2a_fetch_tree_across_a_completed_publish() {
    for edit in [false, true] {
        let (be, pb, wb) = mem();
        w2a(&be, pb, wb, edit);
    }
}
#[test]
fn w2b_sync_docs_across_a_completed_publish() {
    for edit in [false, true] {
        let (be, pb, wb) = mem();
        w2b(&be, pb, wb, edit);
    }
}
#[test]
fn w2c_browse_get_across_a_completed_publish() {
    let (be, pb, wb) = mem();
    w2c(&be, pb, wb);
}

#[cfg(feature = "rustfs")]
mod live {
    use super::*;
    use s2_drills_common::{backend, mk_buckets, uniq, wipe};
    fn run(f: impl Fn(&weftos_store::RustfsBackend, &str, &str)) {
        let (be, _d) = backend();
        let (pb, wb) = mk_buckets(&be, &uniq());
        f(&be, &pb, &wb);
        wipe(&be, &[&pb, &wb]);
    }
    #[test]
    #[ignore = "needs a throwaway RustFS: WEFTOS_STORE_TEST_RUSTFS_ENV"]
    fn live_w1a_w1b() {
        run(|b, p, w| w1(b, p, w, true));
        run(|b, p, w| w1(b, p, w, false));
    }
    #[test]
    #[ignore = "needs a throwaway RustFS: WEFTOS_STORE_TEST_RUSTFS_ENV"]
    fn live_w2a_w2b_w2c() {
        for edit in [false, true] {
            run(|b, p, w| w2a(b, p, w, edit));
            run(|b, p, w| w2b(b, p, w, edit));
        }
        run(w2c);
    }
}
