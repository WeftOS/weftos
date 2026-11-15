//! WO-069 lane T: fixtures shared by the S2 call-budget tests. Test support only.
#![allow(dead_code)]
use std::path::Path;
use weftos_store::FsBackend;
use weftos_store::backend::AdminBackend;
use weftos_store::overlay::{ObjectBackend, ObjectMeta, Result};
use weftos_store::promote::WorktreeMeta;
use weftos_store::share::*;
use weftos_store::share_fetch::*;
use weftos_store::testing::{CountingBackend, Counts};

pub const PR: &str = "p-01abc";
/// Files per worktree, worktrees, docs among the files (the "small N and W" of the budgets).
pub const N: u64 = 20;
pub const W: u64 = 3;
pub const D: u64 = 4;

/// A counting spy over S1's directory backend: `Sync` (the follower polls from threads) and with a
/// real compare-and-swap, so `put_cas` is what the daemon's backend does, not head-then-put.
pub struct Spy {
    pub be: CountingBackend<FsBackend>,
    _dir: tempfile::TempDir,
}

pub fn spy() -> Spy {
    let dir = tempfile::tempdir().unwrap();
    let be = CountingBackend::new(FsBackend::open(dir.path()).unwrap());
    AdminBackend::create_bucket(&be.inner, PR, &[]).unwrap();
    Spy { be, _dir: dir }
}

impl Spy {
    pub fn counts(&self) -> Counts {
        self.be.counts()
    }
    pub fn reset(&self) {
        self.be.reset()
    }
    pub fn make_bucket(&self, name: &str) {
        AdminBackend::create_bucket(&self.be.inner, name, &[]).unwrap();
    }
}

impl ObjectBackend for Spy {
    fn head(&self, b: &str, k: &str) -> Result<Option<ObjectMeta>> {
        self.be.head(b, k)
    }
    fn get(&self, b: &str, k: &str) -> Result<Option<Vec<u8>>> {
        self.be.get(b, k)
    }
    fn put(&self, b: &str, k: &str, d: &[u8]) -> Result<ObjectMeta> {
        self.be.put(b, k, d)
    }
    fn delete(&self, b: &str, k: &str) -> Result<()> {
        self.be.delete(b, k)
    }
    fn list(&self, b: &str, p: &str) -> Result<Vec<ObjectMeta>> {
        self.be.list(b, p)
    }
    fn put_cas(&self, b: &str, k: &str, d: &[u8], e: Option<&str>) -> Result<bool> {
        self.be.put_cas(b, k, d, e)
    }
    fn copy(&self, sb: &str, sk: &str, db: &str, dk: &str) -> Result<ObjectMeta> {
        self.be.copy(sb, sk, db, dk)
    }
}

/// `[get, head, list, put, put_cas, delete, copy]`.
pub type K = [u64; 7];

pub fn k(c: Counts) -> K {
    [c.get, c.head, c.list, c.put, c.put_cas, c.delete, c.copy]
}

/// Calls made by `f`, from a clean counter.
pub fn delta<T>(b: &Spy, f: impl FnOnce() -> T) -> (T, K) {
    b.reset();
    let v = f();
    (v, k(b.counts()))
}

/// `tag` goes into every file body, so two sources with different tags differ in every file.
pub struct Src {
    pub dir: tempfile::TempDir,
    pub paths: Vec<String>,
}

/// `n` files, `d` of them docs (`docs/dK.md`, each with a markdown title), the rest `src/fK.rs`.
pub fn src(n: u64, d: u64, tag: &str) -> Src {
    let dir = tempfile::tempdir().unwrap();
    let mut paths = vec![];
    for i in 0..n {
        let (p, body) = if i < d {
            (format!("docs/d{i}.md"), format!("# Doc {i}\n{tag}\n"))
        } else {
            (format!("src/f{i}.rs"), format!("fn f{i}() {{}} // {tag}\n"))
        };
        let f = dir.path().join(&p);
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(f, body).unwrap();
        paths.push(p);
    }
    Src { dir, paths }
}

pub fn bucket(slug: &str) -> String {
    format!("w-{slug}")
}

pub fn actor(a: &str) -> Actor {
    Actor { agent: a.into(), host: "h".into() }
}

pub fn create(b: &Spy, slug: &str) {
    b.make_bucket(&bucket(slug));
    let m = WorktreeMeta { slug: slug.into(), project: "01abc".into(), branch: slug.into(), base_commit: "base".into(), created_ms: 0 };
    weftos_store::promote::write_meta(b, &bucket(slug), &m).unwrap();
}

pub fn tree_src<'a>(s: &'a Src, slug: &'a str, head: &'a str, bundle: Option<&'a Path>) -> TreeSource<'a> {
    TreeSource {
        root: s.dir.path(),
        paths: &s.paths,
        uncommitted: &[],
        changes: &[],
        branch: slug,
        base_commit: "base",
        head_commit: head,
        bundle,
        writer: actor("w"),
    }
}

pub fn publish(b: &Spy, slug: &str, s: &Src, head: &str, now: u64) -> PublishReport {
    publish_tree(b, &bucket(slug), &tree_src(s, slug, head, None), &Limits::default(), now).unwrap()
}

/// What the daemon does after a publish: derive the entry and write it with the digest.
pub fn refresh(b: &Spy, slug: &str) {
    let e = summarize(b, slug, &bucket(slug)).unwrap().unwrap();
    write_registry_entry(b, PR, &e).unwrap();
}

/// A registered, published worktree.
pub fn worktree(b: &Spy, slug: &str, s: &Src, head: &str) {
    create(b, slug);
    publish(b, slug, s, head, 5_000);
    refresh(b, slug);
}

pub fn slug(i: u64) -> String {
    format!("wt{i}")
}

/// W registered worktrees (each its own tag, so no two share content).
pub fn project(b: &Spy, w: u64) -> Vec<Src> {
    (0..w)
        .map(|i| {
            let s = src(N, D, &format!("t{i}"));
            worktree(b, &slug(i), &s, &format!("h{i}"));
            s
        })
        .collect()
}

pub fn entry(b: &Spy, slug: &str) -> RegistryEntry {
    list_shared(b, PR).unwrap().into_iter().find(|e| e.slug == slug).unwrap()
}
