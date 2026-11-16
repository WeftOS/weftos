//! Shared by the WO-069 lane H drill tests (`s2_drills_live.rs`, `s2_drills_fail.rs`): a failing-backend wrapper, a synthetic tree, bucket helpers.
#![cfg(feature = "rustfs")]
#![allow(dead_code)]

use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use weftos_store::backend::AdminBackend;
use weftos_store::follower_browse::browse_get;
use weftos_store::overlay::{ObjectBackend, ObjectMeta, S2Error};
use weftos_store::share::*;
use weftos_store::{RustfsBackend, RustfsConfig};

pub const MIN: u64 = 60_000;
pub const H: u64 = 3_600_000;
pub const D: u64 = 24 * H;

/// Fails the (put_at+1)th put and every later one, and/or every delete from the (delete_at+1)th on. The manifest write is a
/// `put_cas` since lane D3; it counts as a put here, so a cut index still names "the Nth object or manifest write".
pub struct Flaky<'a> {
    pub inner: &'a RustfsBackend,
    pub puts: AtomicUsize,
    pub deletes: AtomicUsize,
    pub put_at: usize,
    pub delete_at: usize,
}
impl<'a> Flaky<'a> {
    pub fn new(inner: &'a RustfsBackend, put_at: usize, delete_at: usize) -> Self {
        Self { inner, puts: AtomicUsize::new(0), deletes: AtomicUsize::new(0), put_at, delete_at }
    }
}
impl ObjectBackend for Flaky<'_> {
    fn head(&self, b: &str, k: &str) -> Result<Option<ObjectMeta>, S2Error> {
        self.inner.head(b, k)
    }
    fn get(&self, b: &str, k: &str) -> Result<Option<Vec<u8>>, S2Error> {
        self.inner.get(b, k)
    }
    fn put(&self, b: &str, k: &str, d: &[u8]) -> Result<ObjectMeta, S2Error> {
        if self.puts.fetch_add(1, SeqCst) >= self.put_at {
            return Err(S2Error::Backend("injected: the writer died".into()));
        }
        self.inner.put(b, k, d)
    }
    fn delete(&self, b: &str, k: &str) -> Result<(), S2Error> {
        if self.deletes.fetch_add(1, SeqCst) >= self.delete_at {
            return Err(S2Error::Backend("injected: the writer died".into()));
        }
        self.inner.delete(b, k)
    }
    fn list(&self, b: &str, p: &str) -> Result<Vec<ObjectMeta>, S2Error> {
        self.inner.list(b, p)
    }
    fn put_cas(&self, b: &str, k: &str, d: &[u8], e: Option<&str>) -> Result<bool, S2Error> {
        if k == MANIFEST_KEY && self.puts.fetch_add(1, SeqCst) >= self.put_at {
            return Err(S2Error::Backend("injected: the writer died".into()));
        }
        self.inner.put_cas(b, k, d, e)
    }
}

pub fn backend() -> (RustfsBackend, tempfile::TempDir) {
    let f = std::env::var("WEFTOS_STORE_TEST_RUSTFS_ENV").expect("WEFTOS_STORE_TEST_RUSTFS_ENV");
    let d = tempfile::tempdir().unwrap();
    let cfg = RustfsConfig::from_env_file(std::path::Path::new(&f), d.path().join("secrets")).unwrap();
    (RustfsBackend::new(cfg).unwrap(), d)
}

pub fn uniq() -> String {
    let u = ulid::Ulid::new().to_string().to_lowercase();
    u[10..].to_string()
}

pub fn who(a: &str, h: &str) -> Actor {
    Actor { agent: a.into(), host: h.into() }
}

pub fn line(id: &str, step: &str, ms: u128, detail: impl std::fmt::Display) {
    println!("DRILL|{id}|{step}|PASS|{ms} ms|{detail}");
}

pub fn finding(id: &str, detail: impl std::fmt::Display) {
    println!("FINDING|{id}|{detail}");
}

/// A former finding that is now fixed: a DRILL line, so the drill run's findings count stays honest.
pub fn fixed(id: &str, detail: impl std::fmt::Display) {
    println!("DRILL|{id}|fixed|PASS|-|{detail}");
}

pub struct Tree {
    pub dir: tempfile::TempDir,
    pub paths: Vec<String>,
}
impl Tree {
    pub fn new(files: &[(&str, String)]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let t = Tree { dir, paths: vec![] };
        let mut t = t;
        for (p, c) in files {
            t.write(p, c);
        }
        t
    }
    pub fn write(&mut self, p: &str, c: &str) {
        let f = self.dir.path().join(p);
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(f, c).unwrap();
        if !self.paths.iter().any(|x| x == p) {
            self.paths.push(p.to_string());
        }
    }
    pub fn remove(&mut self, p: &str) {
        std::fs::remove_file(self.dir.path().join(p)).unwrap();
        self.paths.retain(|x| x != p);
    }
    pub fn publish<B: ObjectBackend>(&self, b: &B, wb: &str, head: &str, who: &Actor, now: u64) -> Result<PublishReport, S2Error> {
        self.publish_opts(b, wb, head, who, now, &PublishOptions::default())
    }
    pub fn publish_opts<B: ObjectBackend>(&self, b: &B, wb: &str, head: &str, who: &Actor, now: u64, o: &PublishOptions) -> Result<PublishReport, S2Error> {
        let src = TreeSource {
            root: self.dir.path(), paths: &self.paths, uncommitted: &[], changes: &[], changes_recorded: false, branch: "feat-x", base_commit: "base",
            head_commit: head, bundle: None, writer: who.clone(),
        };
        publish_tree_opts(b, wb, &src, &Limits::default(), now, None, o).map(|p| p.report)
    }
}

pub fn doc_set(n: usize) -> Vec<(String, String)> {
    (0..n).map(|i| (format!("docs/d{i}.md"), format!("# Doc {i}\nfirst version of document {i}\n"))).collect()
}

/// Create a bucket, retrying a transient transport error (a loaded loopback server drops a connection now and then).
pub fn make_bucket(be: &RustfsBackend, b: &str) {
    for attempt in 1..=4 {
        match AdminBackend::create_bucket(be, b, &[]) {
            Ok(_) => return,
            Err(e) if attempt < 4 => {
                println!("DETAIL|retry|create_bucket {b} attempt {attempt}: {e}");
                std::thread::sleep(std::time::Duration::from_secs(2));
            }
            Err(e) => panic!("create_bucket {b}: {e}"),
        }
    }
}

pub fn mk_buckets(be: &RustfsBackend, u: &str) -> (String, String) {
    let (pb, wb) = (format!("t-p-{u}"), format!("t-w-{u}"));
    for b in [&pb, &wb] {
        make_bucket(be, b);
    }
    (pb, wb)
}

pub fn wipe(be: &RustfsBackend, buckets: &[&str]) {
    for b in buckets {
        for m in ObjectBackend::list(be, b, "").unwrap() {
            ObjectBackend::delete(be, b, &m.key).unwrap();
        }
        let _ = AdminBackend::delete_bucket(be, b);
    }
}

/// Every manifest file read through the browse path: (readable, checksum-mismatch, other errors).
pub fn read_all(be: &RustfsBackend, pb: &str, slug: &str, wb: &str) -> (usize, usize, usize) {
    let m = read_manifest(be, wb).unwrap().unwrap();
    let (mut ok, mut bad, mut other) = (0, 0, 0);
    for f in &m.files {
        match browse_get(be, pb, slug, &f.path, 1 << 20) {
            Ok(r) => {
                assert_eq!(r.data.as_deref().map(sha256_hex), Some(f.sha256.clone()), "never wrong bytes served silently");
                ok += 1;
            }
            Err(S2Error::Backend(e)) if e.contains("checksum mismatch") => bad += 1,
            Err(_) => other += 1,
        }
    }
    (ok, bad, other)
}

