//! Shared by the WO-069 lane Q follower drills (`s2_drills_follower.rs`, `s2_drills_follower_more.rs`): ADR-126 section 4a gate item 6.
//! One environment, publishing helpers and the `drill!` macro that runs a drill on the in-memory backend, the directory backend
//! and (ignored) a THROWAWAY loopback RustFS named by `WEFTOS_STORE_TEST_RUSTFS_ENV`. Output lines match the other drills:
//! `DRILL|6x|<step>|PASS|<ms> ms|<detail>` and `FINDING|6x|<detail>`.
#![allow(dead_code)]

use std::cell::RefCell;
use weftos_store::backend::AdminBackend;
use weftos_store::follower::Change;
use weftos_store::overlay::{ObjectBackend as S2, Result as R};
use weftos_store::promote::*;
use weftos_store::share::*;
use weftos_store::registry_sign::{EntrySigner, EntryVerifier};
use weftos_store::share_fetch::*;

pub const DAY: u64 = 24 * 3600 * 1000;
pub const T0: u64 = 100 * DAY;

pub fn actor(n: &str) -> Actor {
    Actor { agent: n.into(), host: format!("host-{n}") }
}

pub fn line(id: &str, step: &str, t: std::time::Instant, detail: impl std::fmt::Display) {
    println!("DRILL|{id}|{step}|PASS|{} ms|{detail}", t.elapsed().as_millis());
}

pub fn finding(id: &str, detail: impl std::fmt::Display) {
    println!("FINDING|{id}|{detail}");
}

/// A former finding that is now fixed: a DRILL line, so the drill run's findings count stays honest.
pub fn fixed(id: &str, detail: impl std::fmt::Display) {
    println!("DRILL|{id}|fixed|PASS|-|{detail}");
}

/// Runs a drill generic over the store as `mem` (in-memory), `fs` (directory backend) and `live` (ignored, throwaway RustFS).
#[macro_export]
macro_rules! drill {
    ($name:ident, $f:ident) => {
        mod $name {
            use super::*;
            #[test]
            fn mem() {
                let be = weftos_store::testing::MemBackend::new();
                super::$f(&Env::new(&be, None));
            }
            #[test]
            fn fs() {
                let d = tempfile::tempdir().unwrap();
                let be = weftos_store::FsBackend::open(d.path()).unwrap();
                super::$f(&Env::new(&be, Some(&be)));
            }
            #[test]
            #[ignore = "needs a throwaway RustFS: WEFTOS_STORE_TEST_RUSTFS_ENV"]
            #[cfg(feature = "rustfs")]
            fn live() {
                let (cfg, _d) = live_cfg();
                let be = weftos_store::RustfsBackend::new(cfg).unwrap();
                super::$f(&Env::new(&be, Some(&be)));
            }
        }
    };
}

/// Like `drill!` for a drill that reads from several threads (`poll_fast`): the in-memory backend is not `Sync`, so `fs` and `live` only.
#[macro_export]
macro_rules! drill_sync {
    ($name:ident, $f:ident) => {
        mod $name {
            use super::*;
            #[test]
            fn fs() {
                let d = tempfile::tempdir().unwrap();
                let be = weftos_store::FsBackend::open(d.path()).unwrap();
                super::$f(&Env::new(&be, Some(&be)));
            }
            #[test]
            #[ignore = "needs a throwaway RustFS: WEFTOS_STORE_TEST_RUSTFS_ENV"]
            #[cfg(feature = "rustfs")]
            fn live() {
                let (cfg, _d) = live_cfg();
                let be = weftos_store::RustfsBackend::new(cfg).unwrap();
                super::$f(&Env::new(&be, Some(&be)));
            }
        }
    };
}

#[cfg(feature = "rustfs")]
pub fn live_cfg() -> (weftos_store::RustfsConfig, tempfile::TempDir) {
    let f = std::env::var("WEFTOS_STORE_TEST_RUSTFS_ENV").expect("WEFTOS_STORE_TEST_RUSTFS_ENV");
    let d = tempfile::tempdir().unwrap();
    (weftos_store::RustfsConfig::from_env_file(std::path::Path::new(&f), d.path().join("secrets")).unwrap(), d)
}

/// One project bucket, worktree buckets made on demand; real buckets are removed on drop.
pub struct Env<'a, B: S2> {
    pub be: &'a B,
    pub admin: Option<&'a dyn AdminBackend>,
    pub u: String,
    pub pr: String,
    made: RefCell<Vec<String>>,
}

impl<'a, B: S2> Env<'a, B> {
    pub fn new(be: &'a B, admin: Option<&'a dyn AdminBackend>) -> Self {
        let id = ulid::Ulid::new().to_string().to_lowercase();
        let u = id[id.len() - 10..].to_string();
        let e = Self { be, admin, pr: format!("p-{u}"), u, made: RefCell::new(vec![]) };
        let pr = e.pr.clone();
        e.make(&pr);
        e
    }
    pub fn make(&self, b: &str) {
        if let Some(a) = self.admin {
            a.create_bucket(b, &[]).unwrap();
        }
        self.made.borrow_mut().push(b.to_string());
    }
    /// A fresh bucket named `w-<run>-<slug>` (the shape `check_entry` expects for a registry entry of `slug`).
    pub fn wt(&self, slug: &str) -> String {
        let b = format!("w-{}-{slug}", self.u);
        self.make(&b);
        b
    }
}

impl<B: S2> Drop for Env<'_, B> {
    fn drop(&mut self) {
        if let Some(a) = self.admin {
            for b in self.made.borrow().iter() {
                if let Ok(ms) = S2::list(self.be, b, "") {
                    for m in ms {
                        let _ = S2::delete(self.be, b, &m.key);
                    }
                }
                let _ = a.delete_bucket(b);
            }
        }
    }
}

pub struct Tree {
    pub dir: tempfile::TempDir,
    pub paths: Vec<String>,
}

pub fn tree(files: &[(&str, &str)]) -> Tree {
    tree_bytes(&files.iter().map(|(p, c)| (p.to_string(), c.as_bytes().to_vec())).collect::<Vec<_>>())
}

pub fn tree_bytes(files: &[(String, Vec<u8>)]) -> Tree {
    let dir = tempfile::tempdir().unwrap();
    let mut paths = vec![];
    for (p, c) in files {
        let f = dir.path().join(p);
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(f, c).unwrap();
        paths.push(p.clone());
    }
    Tree { dir, paths }
}

pub fn init_wt<B: S2>(be: &B, bucket: &str, slug: &str, base: &str) {
    write_meta(be, bucket, &WorktreeMeta { slug: slug.into(), project: "p".into(), branch: slug.into(), base_commit: base.into(), created_ms: 0 }).unwrap();
}

pub struct Pub<'a> {
    pub head: &'a str,
    pub base: &'a str,
    pub changes: &'a [PathChange],
    /// The caller computed `changes` (so an empty list means "changed nothing").
    pub recorded: bool,
    pub now: u64,
}

impl Default for Pub<'_> {
    fn default() -> Self {
        Pub { head: "h1", base: "base", changes: &[], recorded: false, now: T0 }
    }
}

pub fn push<B: S2>(be: &B, bucket: &str, t: &Tree, p: &Pub<'_>) -> R<PublishReport> {
    let src = TreeSource {
        root: t.dir.path(), paths: &t.paths, uncommitted: &[], changes: p.changes, changes_recorded: p.recorded, branch: bucket, base_commit: p.base,
        head_commit: p.head, bundle: None, writer: actor("writer"),
    };
    publish_tree(be, bucket, &src, &Limits::default(), p.now)
}

/// What the daemon does after a publish, handoff, claim or merge.
pub fn register<B: S2>(be: &B, pr: &str, slug: &str, bucket: &str) -> RegistryEntry {
    let e = summarize(be, slug, bucket).unwrap().unwrap();
    write_registry_entry(be, pr, &e).unwrap();
    e
}

/// A worktree made and published in one go, then registered.
pub fn up<B: S2>(env: &Env<'_, B>, slug: &str, files: &[(&str, &str)], p: &Pub<'_>) -> RegistryEntry {
    let wb = env.wt(slug);
    init_wt(env.be, &wb, slug, p.base);
    push(env.be, &wb, &tree(files), p).unwrap();
    register(env.be, &env.pr, slug, &wb)
}

pub fn ch(p: &str, k: ChangeKind) -> PathChange {
    PathChange { path: p.into(), kind: k }
}

/// A poll result as sorted `kind:slug` strings.
pub fn kinds(c: &[Change]) -> Vec<String> {
    let mut v: Vec<String> = c
        .iter()
        .map(|x| match x {
            Change::New(e) => format!("new:{}", e.slug),
            Change::HeadMoved { entry, .. } => format!("head:{}", entry.slug),
            Change::StateChanged { entry, .. } => format!("state:{}", entry.slug),
            Change::Gone { slug } => format!("gone:{slug}"),
            Change::Unreadable { slug } => format!("bad:{slug}"),
        })
        .collect();
    v.sort();
    v
}

/// A keyed-hash stand-in for the daemon's registry signer and the follower's verifier.
pub struct Toy;
impl EntrySigner for Toy {
    fn signer_id(&self) -> String {
        "toy:daemon".into()
    }
    fn alg(&self) -> &'static str {
        "toy-sha256"
    }
    fn sign(&self, msg: &[u8]) -> Vec<u8> {
        {
            use sha2::Digest;
            sha2::Sha256::new().chain_update(b"daemon").chain_update(msg).finalize().to_vec()
        }
    }
}
impl EntryVerifier for Toy {
    fn verify(&self, signer: &str, alg: &str, msg: &[u8], sig: &[u8]) -> bool {
        signer == "toy:daemon" && alg == "toy-sha256" && self.sign(msg) == sig
    }
}

