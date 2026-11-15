//! WO-069 lane T: backend-call budgets of the daemon `store.worktree.*` verbs, counted at S1 (the
//! level the daemon's `Op` hands to the S2 code, so these are the calls the wire would carry, with a
//! conditional PUT as one `put_cas`). `list` counts one per request; a RustFS listing of M objects
//! is ceil(M / 1000) wire pages. Each number is a formula of N files, W worktrees, D docs. These pin
//! what the daemon does today; a lane that removes a call updates the formula, never loosens it.
//! Order of the kinds: `[get, head, list, put, put_cas, delete, copy, other]`.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::Arc;

use clawft_kernel::exposure::ExposureRegistry;
use serde_json::json;
use weftos_store::backend::{AdminBackend, BackendError, ObjectAttrs, ObjectBackend as S1, ObjectMeta};
use weftos_store::{FsBackend, StoreService};

use super::*;
use crate::store_rt::{ActivitySink, ChainAudit, RegistryGate};
use std::path::Path;

const N: u64 = 20;
const W: u64 = 3;
const D: u64 = 4;

type K = [u64; 8];

#[derive(Default)]
struct Tally([AtomicU64; 8]);

impl Tally {
    fn bump(&self, i: usize) {
        self.0[i].fetch_add(1, Relaxed);
    }
    fn take(&self) -> K {
        std::array::from_fn(|i| self.0[i].swap(0, Relaxed))
    }
}

/// S1 spy: every backend method that carries a request is one call of its kind; the `*_attrs` and
/// `list_stamped` variants count as the plain kind they replace.
struct Cnt(Arc<FsBackend>, Arc<Tally>);

impl S1 for Cnt {
    fn put(&self, b: &str, k: &str, d: &[u8], n: bool) -> Result<ObjectMeta, BackendError> {
        self.1.bump(if n { 4 } else { 3 });
        self.0.put(b, k, d, n)
    }
    fn put_if_match(&self, b: &str, k: &str, d: &[u8], e: &str) -> Result<ObjectMeta, BackendError> {
        self.1.bump(4);
        self.0.put_if_match(b, k, d, e)
    }
    fn put_attrs(&self, b: &str, k: &str, d: &[u8], n: bool, a: ObjectAttrs) -> Result<ObjectMeta, BackendError> {
        self.1.bump(if n { 4 } else { 3 });
        self.0.put_attrs(b, k, d, n, a)
    }
    fn put_if_match_attrs(&self, b: &str, k: &str, d: &[u8], e: &str, a: ObjectAttrs) -> Result<ObjectMeta, BackendError> {
        self.1.bump(4);
        self.0.put_if_match_attrs(b, k, d, e, a)
    }
    fn attrs(&self, b: &str, k: &str) -> Result<ObjectAttrs, BackendError> {
        self.1.bump(7);
        self.0.attrs(b, k)
    }
    fn list_stamped(&self, b: &str, p: &str) -> Result<Vec<(ObjectMeta, String)>, BackendError> {
        self.1.bump(2);
        self.0.list_stamped(b, p)
    }
    fn get(&self, b: &str, k: &str) -> Result<Vec<u8>, BackendError> {
        self.1.bump(0);
        self.0.get(b, k)
    }
    fn head(&self, b: &str, k: &str) -> Result<ObjectMeta, BackendError> {
        self.1.bump(1);
        self.0.head(b, k)
    }
    fn delete_if_match(&self, b: &str, k: &str, e: &str) -> Result<(), BackendError> {
        self.1.bump(5);
        self.0.delete_if_match(b, k, e)
    }
    fn delete(&self, b: &str, k: &str) -> Result<(), BackendError> {
        self.1.bump(5);
        self.0.delete(b, k)
    }
    fn copy(&self, sb: &str, sk: &str, db: &str, dk: &str) -> Result<ObjectMeta, BackendError> {
        self.1.bump(6);
        self.0.copy(sb, sk, db, dk)
    }
    fn list(&self, b: &str, p: &str) -> Result<Vec<ObjectMeta>, BackendError> {
        self.1.bump(2);
        self.0.list(b, p)
    }
}

fn counting_rig() -> (tempfile::TempDir, StoreService, Arc<Tally>) {
    let d = tempfile::tempdir().unwrap();
    let reg = Arc::new(ExposureRegistry::new("pg-1"));
    let inner = Arc::new(FsBackend::open(d.path()).unwrap());
    let t = Arc::new(Tally::default());
    let admin: Arc<dyn AdminBackend> = inner.clone();
    let svc = StoreService::open(
        weftos_store::service::Config { node_id: "pg-1".into(), state_path: None, clock: clawft_kernel::exposure::registry::now_ms },
        admin,
        Arc::new(Cnt(inner, t.clone())),
        Arc::new(RegistryGate(reg.clone())),
        Arc::new(ActivitySink(reg)),
        Arc::new(ChainAudit(None)),
    )
    .unwrap();
    (d, svc, t)
}

fn pin(got: K, want: K, label: &str) {
    assert_eq!(got, want, "{label}");
}

fn go(svc: &StoreService, verb: &str, p: Value, now: u64) -> Result<Value, String> {
    run(svc, "local", &format!("store.worktree.{verb}"), p, now)
}

/// Run one verb from a clean counter; returns the answer and the calls it made.
fn counted(svc: &StoreService, t: &Tally, verb: &str, p: Value, now: u64) -> (Value, K) {
    t.take();
    let v = go(svc, verb, p, now).unwrap_or_else(|e| panic!("{verb}: {e}"));
    (v, t.take())
}

/// `n` files, `d` of them docs. Returns the three `*_z` list files (written next to the tree).
fn tree(dir: &Path, n: u64, d: u64, tag: &str) -> (String, String, String) {
    std::fs::create_dir_all(dir.join(".git")).unwrap();
    let mut z = String::new();
    for i in 0..n {
        let rel = if i < d { format!("docs/d{i}.md") } else { format!("src/f{i}.rs") };
        let f = dir.join(&rel);
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        let body = if i < d { format!("# Doc {i}\n{tag}\n") } else { format!("fn f{i}() {{}} // {tag}\n") };
        std::fs::write(f, body).unwrap();
        z.push_str(&rel);
        z.push('\0');
    }
    let w = |name: &str, c: &str| {
        let f = dir.join(name);
        std::fs::write(&f, c).unwrap();
        f.to_string_lossy().into_owned()
    };
    (w("paths.z", &z), w("unc.z", ""), w("chg.z", ""))
}

fn create_and_publish(svc: &StoreService, t: &Tally, slug: &str, src: &Path, tag: &str, now: u64) -> (Value, K) {
    go(svc, "create", json!({"project": PROJ, "slug": slug, "branch": slug, "base": "abc"}), now).unwrap();
    let (paths, unc, chg) = tree(src, N, D, tag);
    counted(
        svc,
        t,
        "publish",
        json!({"project": PROJ, "slug": slug, "root": src.to_string_lossy(), "branch": slug, "base": "abc", "head": "def",
               "paths_z": paths, "uncommitted_z": unc, "changes_z": chg}),
        now + 1,
    )
}

#[test]
fn publish_first_no_change_and_one_file_changed() {
    let (_d, svc, t) = counting_rig();
    let src = tempfile::tempdir().unwrap();
    let (r, c) = create_and_publish(&svc, &t, "wt0", src.path(), "t", 10);
    assert_eq!(r["uploaded"], json!(N));
    // meta GET (live check), quota LIST (whole bucket, `retention::usage`), handoff GET, manifest HEAD
    // (none yet), tree LIST, N object PUT, bundle DELETE (none given), manifest PUT, handoff create
    // as one conditional PUT; then refresh: manifest HEAD + GET, handoff GET, entry PUT, registry LIST,
    // digest PUT. GET 1+1+2 = 4, HEAD 1+1 = 2, LIST 3, PUT N+1+2 = N+3, cas 1, DELETE 1: N + 14 calls.
    pin(c, [4, 2, 3, N + 3, 1, 1, 0, 0], "first publish");
    let publish = |now: u64, head: &str, edit: bool| {
        let (paths, unc, chg) = tree(src.path(), N, D, "t");
        if edit {
            std::fs::write(src.path().join("src/f10.rs"), "changed\n").unwrap();
        }
        counted(
            &svc,
            &t,
            "publish",
            json!({"project": PROJ, "slug": "wt0", "root": src.path().to_string_lossy(), "branch": "wt0", "base": "abc", "head": head,
                   "paths_z": paths, "uncommitted_z": unc, "changes_z": chg}),
            now,
        )
    };
    let (r, c) = publish(30, "def", false);
    assert_eq!((&r["uploaded"], &r["unchanged"]), (&json!(0), &json!(N)));
    // The same walk with nothing uploaded: GET 1 (meta) + 2 (handoff, manifest) + 2 (refresh) = 5,
    // HEAD 2, LIST 3 (quota, tree, registry), PUT 3 (manifest, entry, digest), cas 1 (loses, 412), DELETE 1.
    // The 13 + L(N+5) + L(N) of the note at L = 1: 15 calls. Two full listings, N-dependent on S3.
    pin(c, [5, 2, 3, 3, 1, 1, 0, 0], "no-change publish");
    let (r, c) = publish(40, "def2", true);
    assert_eq!(r["uploaded"], json!(1));
    pin(c, [5, 2, 3, 4, 1, 1, 0, 0], "one file changed = no-change + 1 object PUT");
}

/// W published worktrees `wt0..`, each its own tag; the source trees stay alive with the rig.
struct Rig {
    _d: tempfile::TempDir,
    svc: StoreService,
    t: Arc<Tally>,
    srcs: Vec<tempfile::TempDir>,
}

fn rig_w() -> Rig {
    let (d, svc, t) = counting_rig();
    let mut srcs = vec![];
    for i in 0..W {
        let s = tempfile::tempdir().unwrap();
        create_and_publish(&svc, &t, &format!("wt{i}"), s.path(), &format!("t{i}"), 10 + 10 * i);
        srcs.push(s);
    }
    Rig { _d: d, svc, t, srcs }
}

impl Rig {
    fn run(&self, verb: &str, p: Value) -> (Value, K) {
        counted(&self.svc, &self.t, verb, p, 1_000)
    }
}

#[test]
fn browse_registry_ls_get() {
    let r = rig_w();
    let (v, c) = r.run("browse", json!({"project": PROJ}));
    assert_eq!(v["worktrees"].as_array().unwrap().len() as u64, W);
    // LIST + W entry GET
    pin(c, [W, 0, 1, 0, 0, 0, 0, 0], "browse registry");
    let (_, c) = r.run("browse", json!({"project": PROJ, "slug": "wt0"}));
    // entry GET, meta GET, manifest HEAD + GET
    pin(c, [3, 1, 0, 0, 0, 0, 0, 0], "browse ls");
    let (_, c) = r.run("browse", json!({"project": PROJ, "slug": "wt0", "path": "docs/d0.md"}));
    // open (3 GET + 1 HEAD) + object HEAD + GET
    pin(c, [4, 2, 0, 0, 0, 0, 0, 0], "browse get");
}

#[test]
fn follow_search_leftovers() {
    let r = rig_w();
    let st = r.srcs[0].path().join("follow.json");
    let p = json!({"project": PROJ, "state": st.to_string_lossy()});
    let (_, c) = r.run("follow", p.clone());
    // digest GET + W entry GET
    pin(c, [1 + W, 0, 0, 0, 0, 0, 0, 0], "follow first");
    let (_, c) = r.run("follow", p.clone());
    // digest GET
    pin(c, [1, 0, 0, 0, 0, 0, 0, 0], "follow digest hit");
    let mut full = p.clone();
    full["full"] = json!(true);
    let (_, c) = r.run("follow", full);
    // one registry LIST, etags equal
    pin(c, [0, 0, 1, 0, 0, 0, 0, 0], "follow --full");
    let (_, c) = r.run("search", json!({"project": PROJ, "query": "doc"}));
    // registry LIST + W entry GET; per worktree meta GET + manifest HEAD + GET: GET 3W, HEAD W
    pin(c, [3 * W, W, 1, 0, 0, 0, 0, 0], "search");
    let (_, c) = r.run("leftovers", json!({"project": PROJ}));
    // LIST + W entry GET
    pin(c, [W, 0, 1, 0, 0, 0, 0, 0], "leftovers");
}

#[test]
fn sync_docs_overlaps_claim() {
    let r = rig_w();
    let cache = tempfile::tempdir().unwrap();
    let p = json!({"project": PROJ, "slug": "wt0", "cache": cache.path().to_string_lossy()});
    let (_, c) = r.run("sync-docs", p.clone());
    // summarize (manifest HEAD + GET, handoff GET) + library sync_docs [3 + D, 2 + D] = 8 + 2D calls
    pin(c, [5 + D, 3 + D, 0, 0, 0, 0, 0, 0], "sync-docs first");
    let (_, c) = r.run("sync-docs", p);
    // 8 calls: 3 HEAD + 5 GET, the manifest read three times
    pin(c, [5, 3, 0, 0, 0, 0, 0, 0], "sync-docs repeat");
    let (_, c) = r.run("overlaps", json!({"project": PROJ, "a": "wt0", "b": "wt1"}));
    // 12 calls: 2 summarize (3 each) + 2 meta GET + 2 manifest reads
    pin(c, [8, 4, 0, 0, 0, 0, 0, 0], "overlaps one pair");
    let (_, c) = r.run("claim", json!({"project": PROJ, "slug": "wt0", "agent": "bob", "host": "m2"}));
    // meta GET, manifest HEAD + GET (the claimed head, 6a-1), handoff HEAD + GET, conditional PUT,
    // refresh [2 GET, 1 HEAD, 1 LIST, 2 PUT]: 12 calls
    pin(c, [5, 3, 1, 2, 1, 0, 0, 0], "claim");
}

#[test]
fn fetch_tree_repeat_and_with_bundle() {
    let (_d, svc, t) = counting_rig();
    let src = tempfile::tempdir().unwrap();
    go(&svc, "create", json!({"project": PROJ, "slug": "wt0", "branch": "wt0", "base": "abc"}), 10).unwrap();
    let (paths, unc, chg) = tree(src.path(), N, D, "t");
    let bundle = src.path().join("x.bundle");
    std::fs::write(&bundle, b"BUNDLE").unwrap();
    go(
        &svc,
        "publish",
        json!({"project": PROJ, "slug": "wt0", "root": src.path().to_string_lossy(), "branch": "wt0", "base": "abc", "head": "def",
               "paths_z": paths, "uncommitted_z": unc, "changes_z": chg, "bundle": bundle.to_string_lossy()}),
        11,
    )
    .unwrap();
    let dest = tempfile::tempdir().unwrap();
    let p = json!({"project": PROJ, "slug": "wt0", "dest": dest.path().to_string_lossy()});
    let (v, c) = counted(&svc, &t, "fetch", p.clone(), 20);
    assert_eq!(v["written"], json!(N));
    // manifest HEAD + GET, then HEAD + GET per file
    pin(c, [1 + N, 1 + N, 0, 0, 0, 0, 0, 0], "fetch first");
    let (v, c) = counted(&svc, &t, "fetch", p.clone(), 21);
    assert_eq!(v["unchanged"], json!(N));
    // local sha256 equal: the manifest read only
    pin(c, [1, 1, 0, 0, 0, 0, 0, 0], "fetch repeat");
    let out = dest.path().join("out.bundle");
    let mut pb = p;
    pb["bundle_out"] = json!(out.to_string_lossy());
    let (_, c) = counted(&svc, &t, "fetch", pb, 22);
    // fetch_tree (manifest read) + the has_bundle check (manifest read) + fetch_bundle (manifest read,
    // bundle HEAD + GET, the whole bundle again): 8 calls
    pin(c, [4, 4, 0, 0, 0, 0, 0, 0], "fetch repeat with bundle");
}
