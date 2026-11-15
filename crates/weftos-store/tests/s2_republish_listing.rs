//! WO-069 lane L: an unchanged republish decides "unchanged" from ONE listing of the tree prefix, not one HEAD per file.
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
}
impl<'a> Counting<'a> {
    fn new(inner: &'a MemBackend) -> Self {
        Self { inner, tree_heads: Cell::new(0), tree_lists: Cell::new(0), puts: Cell::new(0) }
    }
    fn reset(&self) {
        self.tree_heads.set(0);
        self.tree_lists.set(0);
        self.puts.set(0);
    }
}
impl ObjectBackend for Counting<'_> {
    fn head(&self, b: &str, k: &str) -> Result<Option<ObjectMeta>> {
        if k.starts_with(TREE_PREFIX) {
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
    let src = TreeSource {
        root,
        paths: ps,
        uncommitted: &[],
        changes: &[],
        branch: "f",
        base_commit: "a",
        head_commit: "b",
        bundle: None,
        writer: Actor { agent: "w".into(), host: "h".into() },
    };
    publish_tree(b, WT, &src, &Limits::default(), now).unwrap()
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
    assert_eq!(b.puts.get(), 1, "only the manifest is written");
}

#[test]
fn a_changed_file_is_uploaded_and_a_dropped_file_removed_with_one_listing() {
    let mem = MemBackend::new();
    let b = Counting::new(&mem);
    let (d, ps) = mk_tree(5);
    publish(&b, d.path(), &ps, 1);
    std::fs::write(d.path().join("src/f0.rs"), "changed").unwrap();
    b.reset();
    let r = publish(&b, d.path(), &ps[..4], 2);
    assert_eq!((r.uploaded, r.unchanged, r.removed), (1, 3, 1));
    assert_eq!(b.tree_lists.get(), 1);
    assert!(mem.head(WT, &format!("{TREE_PREFIX}src/f4.rs")).unwrap().is_none());
}

#[test]
fn an_object_deleted_behind_the_manifest_is_reuploaded() {
    let mem = MemBackend::new();
    let (d, ps) = mk_tree(5);
    publish(&mem, d.path(), &ps, 1);
    let key = format!("{TREE_PREFIX}src/f2.rs");
    mem.delete(WT, &key).unwrap();
    assert!(mem.head(WT, &key).unwrap().is_none());
    let r = publish(&mem, d.path(), &ps, 2);
    assert_eq!((r.uploaded, r.unchanged), (1, 4));
    assert_eq!(mem.get(WT, &key).unwrap().unwrap(), b"fn f2() {}");
}

#[test]
fn an_object_with_the_wrong_size_behind_the_manifest_is_reuploaded() {
    let mem = MemBackend::new();
    let (d, ps) = mk_tree(3);
    publish(&mem, d.path(), &ps, 1);
    let key = format!("{TREE_PREFIX}src/f1.rs");
    mem.put(WT, &key, b"truncated").unwrap();
    let r = publish(&mem, d.path(), &ps, 2);
    assert_eq!((r.uploaded, r.unchanged), (1, 2));
    assert_eq!(mem.get(WT, &key).unwrap().unwrap(), b"fn f1() {}");
}
