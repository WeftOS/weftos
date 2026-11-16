//! WO-069 lane L: an unchanged republish decides "unchanged" from ONE listing of the tree prefix, not one HEAD per file.
//! Lane D3: the objects are the content-named pool (`.wt/obj/`), the manifest write is a `put_cas`, and an object a
//! publish drops is retired, not deleted, until the grace period has passed.
use std::cell::Cell;
use weftos_store::overlay::*;
use weftos_store::share::*;
use weftos_store::testing::MemBackend;

const WT: &str = "w-01abc-aaaaaaaaaaaa";

/// Counts calls by kind; tree HEADs and tree listings are counted separately from manifest/handoff traffic.
struct Counting<'a> {
    inner: &'a MemBackend,
    tree_heads: Cell<usize>,
    tree_lists: Cell<usize>,
    puts: Cell<usize>,
    manifest_cas: Cell<usize>,
}
impl<'a> Counting<'a> {
    fn new(inner: &'a MemBackend) -> Self {
        Self { inner, tree_heads: Cell::new(0), tree_lists: Cell::new(0), puts: Cell::new(0), manifest_cas: Cell::new(0) }
    }
    fn reset(&self) {
        self.tree_heads.set(0);
        self.tree_lists.set(0);
        self.puts.set(0);
        self.manifest_cas.set(0);
    }
}
impl ObjectBackend for Counting<'_> {
    fn head(&self, b: &str, k: &str) -> Result<Option<ObjectMeta>> {
        if k.starts_with(TREE_PREFIX) || k.starts_with(OBJ_PREFIX) {
            self.tree_heads.set(self.tree_heads.get() + 1);
        }
        self.inner.head(b, k)
    }
    fn get(&self, b: &str, k: &str) -> Result<Option<Vec<u8>>> {
        self.inner.get(b, k)
    }
    fn put(&self, b: &str, k: &str, d: &[u8]) -> Result<ObjectMeta> {
        self.puts.set(self.puts.get() + 1);
        self.inner.put(b, k, d)
    }
    fn delete(&self, b: &str, k: &str) -> Result<()> {
        self.inner.delete(b, k)
    }
    fn list(&self, b: &str, p: &str) -> Result<Vec<ObjectMeta>> {
        // WO-069 X3: the one listing is now of the whole bucket (it also shows a leftover bundle), so
        // a listing of "" counts as the tree listing; the asserted number (one) is unchanged.
        if p == TREE_PREFIX || p.is_empty() {
            self.tree_lists.set(self.tree_lists.get() + 1);
        }
        self.inner.list(b, p)
    }
    fn put_cas(&self, b: &str, k: &str, d: &[u8], e: Option<&str>) -> Result<bool> {
        if k == MANIFEST_KEY {
            self.manifest_cas.set(self.manifest_cas.get() + 1);
        }
        self.inner.put_cas(b, k, d, e)
    }
}

fn mk_tree(n: usize) -> (tempfile::TempDir, Vec<String>) {
    let d = tempfile::tempdir().unwrap();
    let mut ps = Vec::new();
    for i in 0..n {
        let p = format!("src/f{i}.rs");
        std::fs::create_dir_all(d.path().join("src")).unwrap();
        std::fs::write(d.path().join(&p), format!("fn f{i}() {{}}")).unwrap();
        ps.push(p);
    }
    (d, ps)
}

fn publish<B: ObjectBackend>(b: &B, root: &std::path::Path, ps: &[String], now: u64) -> PublishReport {
    publish_with(b, root, ps, now, &PublishOptions::default())
}

fn publish_with<B: ObjectBackend>(b: &B, root: &std::path::Path, ps: &[String], now: u64, o: &PublishOptions) -> PublishReport {
    let src = TreeSource {
        root,
        paths: ps,
        uncommitted: &[],
        changes: &[],
        changes_recorded: false,
        branch: "f",
        base_commit: "a",
        head_commit: "b",
        bundle: None,
        writer: Actor { agent: "w".into(), host: "h".into() },
    };
    publish_tree_opts(b, WT, &src, &Limits::default(), now, None, o).unwrap().report
}

/// The object the current manifest names for `path`.
fn object_of(mem: &MemBackend, path: &str) -> String {
    let m = read_manifest(mem, WT).unwrap().unwrap();
    m.object_key(m.files.iter().find(|f| f.path == path).unwrap())
}

#[test]
fn unchanged_republish_makes_no_tree_head_and_one_tree_listing() {
    let mem = MemBackend::new();
    let b = Counting::new(&mem);
    let (d, ps) = mk_tree(50);
    let r = publish(&b, d.path(), &ps, 1);
    assert_eq!((r.uploaded, r.unchanged), (50, 0));
    assert_eq!(b.tree_heads.get(), 0, "the first publish needs no HEAD either");
    assert_eq!(b.tree_lists.get(), 1);
    b.reset();
    let r = publish(&b, d.path(), &ps, 2);
    assert_eq!((r.uploaded, r.unchanged, r.removed), (0, 50, 0));
    assert_eq!(b.tree_heads.get(), 0);
    assert_eq!(b.tree_lists.get(), 1, "one listing serves both the unchanged check and the stale sweep");
    assert_eq!((b.puts.get(), b.manifest_cas.get()), (0, 1), "no object is written; the manifest is one conditional write");
}

#[test]
fn a_changed_file_is_uploaded_and_a_dropped_file_retired_with_one_listing_then_swept_after_the_grace() {
    let mem = MemBackend::new();
    let b = Counting::new(&mem);
    let (d, ps) = mk_tree(5);
    publish(&b, d.path(), &ps, 1);
    let (old_f0, f4) = (object_of(&mem, "src/f0.rs"), object_of(&mem, "src/f4.rs"));
    std::fs::write(d.path().join("src/f0.rs"), "changed").unwrap();
    b.reset();
    let r = publish(&b, d.path(), &ps[..4], 2);
    // f0's old content and f4 leave the tree: retired (kept for the grace), not deleted
    assert_eq!((r.uploaded, r.unchanged, r.removed, r.retired, r.deferred), (1, 3, 0, 2, 2));
    assert_eq!(b.tree_lists.get(), 1);
    assert!(mem.head(WT, &f4).unwrap().is_some() && mem.head(WT, &old_f0).unwrap().is_some());
    // a publish a full grace later sweeps both, still with one listing
    b.reset();
    let r = publish(&b, d.path(), &ps[..4], 2 + DEFAULT_GRACE_MS);
    assert_eq!((r.uploaded, r.removed, r.retired), (0, 2, 0));
    assert_eq!(b.tree_lists.get(), 1);
    assert!(mem.head(WT, &f4).unwrap().is_none() && mem.head(WT, &old_f0).unwrap().is_none());
    // with a zero grace the sweep is immediate, as schema 1 was
    let r = publish_with(&b, d.path(), &ps[..3], 3 + DEFAULT_GRACE_MS, &PublishOptions { grace_ms: 0, ..PublishOptions::default() });
    assert_eq!(r.removed, 1);
}

#[test]
fn an_object_deleted_behind_the_manifest_is_reuploaded_to_a_fresh_key() {
    let mem = MemBackend::new();
    let (d, ps) = mk_tree(5);
    publish(&mem, d.path(), &ps, 1);
    let key = object_of(&mem, "src/f2.rs");
    mem.delete(WT, &key).unwrap();
    assert!(mem.head(WT, &key).unwrap().is_none());
    let r = publish(&mem, d.path(), &ps, 2);
    assert_eq!((r.uploaded, r.unchanged), (1, 4));
    // never back to a key that is gone (a sweep may still be deleting it): a new generation
    let fresh = object_of(&mem, "src/f2.rs");
    assert_ne!(fresh, key);
    assert_eq!(mem.get(WT, &fresh).unwrap().unwrap(), b"fn f2() {}");
}

#[test]
fn an_object_with_the_wrong_size_behind_the_manifest_is_reuploaded_to_a_fresh_key() {
    let mem = MemBackend::new();
    let (d, ps) = mk_tree(3);
    publish(&mem, d.path(), &ps, 1);
    let key = object_of(&mem, "src/f1.rs");
    mem.put(WT, &key, b"truncated").unwrap();
    let r = publish(&mem, d.path(), &ps, 2);
    assert_eq!((r.uploaded, r.unchanged), (1, 2));
    let fresh = object_of(&mem, "src/f1.rs");
    assert_ne!(fresh, key);
    assert_eq!(mem.get(WT, &fresh).unwrap().unwrap(), b"fn f1() {}");
    // the bad object is retired like any other unreferenced one
    assert!(read_manifest(&mem, WT).unwrap().unwrap().retiring.iter().any(|r| r.key() == key));
}
