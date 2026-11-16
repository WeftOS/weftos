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
    // WO-069 X2: the read cache with a TTL real test time never reaches; `wtcache::advance` lapses it.
    wtcache::set_cfg(&svc, Some(CACHE));
    (d, svc, t)
}

const TTL: u64 = 600_000;
const CACHE: weftos_store::follower_cache_shared::CacheCfg =
    weftos_store::follower_cache_shared::CacheCfg { ttl_ms: TTL, max_age_ms: 2 * TTL, max_bytes: 32 << 20, max_entries: 64 };

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

/// The daemon's no-change publish: meta GET, handoff HEAD + GET, manifest HEAD + GET, whole-bucket
/// LIST, the announcement put_cas, manifest PUT, refresh (handoff GET, entry PUT, registry LIST, digest
/// PUT). 12 calls: X3's 10 plus lane Y A4's handoff HEAD and announcement. Lane D3: the manifest PUT
/// is a put_cas on the version read (PUT -1, cas +1) and the post-check reads the handoff and marks
/// the announcement done (HEAD +1, GET +1, cas +1): 15 calls.
const NO_CHANGE_PUBLISH: K = [5, 3, 2, 2, 3, 0, 0, 0];

#[test]
fn publish_first_no_change_and_one_file_changed() {
    let (_d, svc, t) = counting_rig();
    let src = tempfile::tempdir().unwrap();
    let (r, c) = create_and_publish(&svc, &t, "wt0", src.path(), "t", 10);
    assert_eq!(r["uploaded"], json!(N));
    // meta GET (live check), ONE whole-bucket LIST (the quota check and, handed on, the publish's tree
    // objects and bundle check), handoff GET, manifest HEAD (none yet), N object PUT, manifest PUT,
    // handoff create as one conditional PUT; refresh gets the manifest and the created handoff from
    // the publish: entry PUT, registry LIST, digest PUT. GET 1+1 = 2, HEAD 1, LIST 2, PUT N+1+2 = N+3,
    // cas 1, DELETE 0 (no bundle object listed): N + 9 calls. WO-069 X3: was [4,2,3,N+3,1,1] = N + 14
    // (quota LIST + tree LIST merged, bundle DELETE gone, refresh manifest HEAD + GET + handoff GET gone).
    // WO-069 lane Y A4: the handoff is read with its etag, HEAD (no record) instead of GET, and its create
    // carries the publish announcement and comes before the manifest PUT: same N + 9 calls.
    // Lane D3: N pool-object PUTs; the manifest is a create-only put_cas (PUT -1, cas +1); the
    // post-check reads the handoff (HEAD + GET) and marks the announcement done (cas): N + 12 calls.
    pin(c, [2, 3, 2, N + 2, 3, 0, 0, 0], "first publish");
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
    // The same walk with nothing uploaded: GET 1 (meta) + 2 (handoff, manifest) + 1 (refresh re-reads
    // the existing handoff: it may have changed during the upload) = 4, HEAD 1 (manifest), LIST 2
    // (whole bucket, registry), PUT 3 (manifest, entry, digest), cas 0 (handoff known), DELETE 0.
    // X3: was [5,2,3,3,1,1] = 15 calls; now 10. Lane Y A4 adds two: handoff HEAD (the record is read
    // with its etag) and the publish announcement, one conditional PUT on that version before the
    // manifest PUT, so a fenced merge cannot land between its fence and its write: 12. Lane D3: +3
    // (manifest put_cas instead of PUT, post-check HEAD + GET + put_cas): 15.
    pin(c, NO_CHANGE_PUBLISH, "no-change publish");
    let (r, c) = publish(40, "def2", true);
    assert_eq!(r["uploaded"], json!(1));
    pin(c, [5, 3, 2, 3, 3, 0, 0, 0], "one file changed = no-change + 1 object PUT (the old object is retired, not deleted)");
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
    let (_, c) = r.run("browse", json!({"project": PROJ}));
    // X2: the same listing, every entry etag unchanged: no entry GET (uncached: LIST + W GET again)
    pin(c, [0, 0, 1, 0, 0, 0, 0, 0], "browse registry, warm");
    let (_, c) = r.run("browse", json!({"project": PROJ, "slug": "wt0"}));
    // cold: entry HEAD + GET, meta GET, manifest HEAD + GET (uncached [3, 1]: the entry HEAD is the
    // validator a later revalidation compares against)
    pin(c, [3, 2, 0, 0, 0, 0, 0, 0], "browse ls, cold");
    let (_, c) = r.run("browse", json!({"project": PROJ, "slug": "wt0", "path": "docs/d0.md"}));
    // warm manifest: object HEAD + GET only (uncached [4, 2])
    pin(c, [1, 1, 0, 0, 0, 0, 0, 0], "browse get, warm");
    let (_, c) = r.run("browse", json!({"project": PROJ, "slug": "wt0", "path": "docs/"}));
    pin(c, [0, 0, 0, 0, 0, 0, 0, 0], "browse ls, warm");
    wtcache::advance(&r.svc, TTL);
    let (_, c) = r.run("browse", json!({"project": PROJ, "slug": "wt0"}));
    // TTL lapsed: entry HEAD + manifest HEAD, both unchanged
    pin(c, [0, 2, 0, 0, 0, 0, 0, 0], "browse ls, TTL lapsed");
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
    // cold, as uncached: registry LIST + W entry GET; per worktree meta GET + manifest HEAD + GET
    pin(c, [3 * W, W, 1, 0, 0, 0, 0, 0], "search, cold");
    let (_, c) = r.run("search", json!({"project": PROJ, "query": "doc"}));
    // X2 warm: the LIST alone (uncached [3W, W, 1] every time)
    pin(c, [0, 0, 1, 0, 0, 0, 0, 0], "search, warm");
    let (_, c) = r.run("leftovers", json!({"project": PROJ}));
    // X2: the registry was just read; the LIST alone (uncached LIST + W entry GET)
    pin(c, [0, 0, 1, 0, 0, 0, 0, 0], "leftovers, warm");
    wtcache::advance(&r.svc, TTL);
    let (_, c) = r.run("search", json!({"project": PROJ, "query": "doc"}));
    // TTL lapsed: LIST + one manifest HEAD per worktree (the entry etags come from the LIST)
    pin(c, [0, W, 1, 0, 0, 0, 0, 0], "search, TTL lapsed");
}

/// `WEFTOS_WORKTREE_CACHE_MB=0` (here: no cache configured) gives today's budgets exactly.
#[test]
fn cache_off_gives_the_uncached_budgets() {
    let r = rig_w();
    wtcache::set_cfg(&r.svc, None);
    for _ in 0..2 {
        pin(r.run("browse", json!({"project": PROJ})).1, [W, 0, 1, 0, 0, 0, 0, 0], "browse registry, off");
        pin(r.run("browse", json!({"project": PROJ, "slug": "wt0"})).1, [3, 1, 0, 0, 0, 0, 0, 0], "browse ls, off");
        pin(r.run("browse", json!({"project": PROJ, "slug": "wt0", "path": "docs/d0.md"})).1, [4, 2, 0, 0, 0, 0, 0, 0], "browse get, off");
        pin(r.run("search", json!({"project": PROJ, "query": "doc"})).1, [3 * W, W, 1, 0, 0, 0, 0, 0], "search, off");
        pin(r.run("leftovers", json!({"project": PROJ})).1, [W, 0, 1, 0, 0, 0, 0, 0], "leftovers, off");
        pin(r.run("overlaps", json!({"project": PROJ, "a": "wt0", "b": "wt1"})).1, [8, 4, 0, 0, 0, 0, 0, 0], "overlaps, off");
        pin(r.run("gc", json!({"project": PROJ})).1, gc_dry_run_uncached(), "gc dry run, off");
    }
}

/// gc dry run with no orphans and nothing landed: the orphan plan's registry LIST + W entry GET; the
/// service's store listing sizes each store it knows (the project store and W worktree stores: 1 + W
/// LIST); then one landed-record GET per worktree store.
fn gc_dry_run_uncached() -> K {
    [W + W, 0, 1 + 1 + W, 0, 0, 0, 0, 0]
}

#[test]
fn gc_dry_run_plan_reuses_registry_entries() {
    let r = rig_w();
    pin(r.run("gc", json!({"project": PROJ})).1, gc_dry_run_uncached(), "gc dry run, cold");
    // X2: the display plan reuses the entries; the W landed checks still read the store
    pin(r.run("gc", json!({"project": PROJ})).1, [W, 0, 1 + 1 + W, 0, 0, 0, 0, 0], "gc dry run, warm");
}

#[test]
fn sync_docs_overlaps_claim() {
    let r = rig_w();
    let cache = tempfile::tempdir().unwrap();
    let p = json!({"project": PROJ, "slug": "wt0", "cache": cache.path().to_string_lossy()});
    let (_, c) = r.run("sync-docs", p.clone());
    // ONE manifest read (HEAD + GET) and the handoff GET (the entry), then sync_docs_from: meta GET and,
    // per doc, object HEAD + GET (D is under HEAD_LIST_THRESHOLD): 4 + 2D calls
    pin(c, [3 + D, 1 + D, 0, 0, 0, 0, 0, 0], "sync-docs first");
    let (_, c) = r.run("sync-docs", p);
    // 4 calls: manifest HEAD + GET, handoff GET, meta GET; local sha equal, no object calls
    pin(c, [3, 1, 0, 0, 0, 0, 0, 0], "sync-docs repeat");
    let (_, c) = r.run("overlaps", json!({"project": PROJ, "a": "wt0", "b": "wt1"}));
    // X2 cold: two cold loads (entry HEAD + GET, meta GET, manifest HEAD + GET each): 10 calls
    // (uncached 12: 2 summarize (3 each) + 2 meta GET + 2 manifest reads)
    pin(c, [6, 4, 0, 0, 0, 0, 0, 0], "overlaps one pair, cold");
    let (_, c) = r.run("overlaps", json!({"project": PROJ, "a": "wt0", "b": "wt1"}));
    pin(c, [0, 0, 0, 0, 0, 0, 0, 0], "overlaps one pair, warm");
    let (_, c) = r.run("claim", json!({"project": PROJ, "slug": "wt0", "agent": "bob", "host": "m2"}));
    // meta GET, manifest HEAD + GET (the claimed head, 6a-1), handoff HEAD + GET, conditional PUT,
    // refresh [2 GET, 1 HEAD, 1 LIST, 2 PUT]: 12 calls
    pin(c, [5, 3, 1, 2, 1, 0, 0, 0], "claim");
}

/// `promote --epoch` on a W-rig worktree, as measured: the land (handoff HEAD + GET, manifest HEAD +
/// GET, one conditional PUT) plus what promotion itself does (meta and promotion records, the archive
/// copy of the N tree objects and the control objects, the registry refresh), which A3 left unchanged.
const PROMOTE_FENCED: K = [7, 3, 4, 4, 1, 0, N + 3, 0];

#[test]
fn promote_with_and_without_a_claim_epoch() {
    let r = rig_w();
    let claim = |slug: &str| r.run("claim", json!({"project": PROJ, "slug": slug, "agent": "bob", "host": "m2"})).0["epoch"].as_u64().unwrap();
    let ep = claim("wt0");
    claim("wt1");
    let (_, fenced) = r.run("promote", json!({"project": PROJ, "slug": "wt0", "agent": "bob", "host": "m2", "epoch": ep}));
    let (_, plain) = r.run("promote", json!({"project": PROJ, "slug": "wt1", "agent": "bob", "host": "m2"}));
    // The fence runs inside the land's conditional write (WO-069 lane Y, A3): handoff HEAD + GET,
    // manifest HEAD + GET (the claimed head), one conditional PUT. Unfenced: handoff HEAD + GET, PUT.
    // So the fence costs one manifest HEAD + GET. Before A3 it was a separate `fence_claim` (handoff
    // GET, manifest HEAD + GET) ahead of the land's own read: one GET more.
    let diff: K = std::array::from_fn(|i| fenced[i] - plain[i]);
    pin(diff, [1, 1, 0, 0, 0, 0, 0, 0], "promote --epoch minus promote");
    pin(fenced, PROMOTE_FENCED, "promote --epoch");
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
    // manifest HEAD + GET, then (N is over HEAD_LIST_THRESHOLD) one tree LIST and one GET per file, no per-file HEAD
    pin(c, [1 + N, 1, 1, 0, 0, 0, 0, 0], "fetch first");
    let (v, c) = counted(&svc, &t, "fetch", p.clone(), 21);
    assert_eq!(v["unchanged"], json!(N));
    // local sha256 equal: the manifest read only
    pin(c, [1, 1, 0, 0, 0, 0, 0, 0], "fetch repeat");
    let out = dest.path().join("out.bundle");
    let mut pb = p;
    pb["bundle_out"] = json!(out.to_string_lossy());
    let (_, c) = counted(&svc, &t, "fetch", pb, 22);
    // ONE manifest read (HEAD + GET) shared by the tree, the has_bundle check and the bundle, then the
    // bundle HEAD + GET (the whole bundle again): 4 calls
    pin(c, [2, 2, 0, 0, 0, 0, 0, 0], "fetch repeat with bundle");
}

// ---- WO-069 X3: the quota check shares the publish's one listing; it must still refuse before any write ----

fn republish_params(src: &Path, quota: Option<u64>) -> Value {
    let (paths, unc, chg) = tree(src, N, D, "t");
    let mut p = json!({"project": PROJ, "slug": "wt0", "root": src.to_string_lossy(), "branch": "wt0", "base": "abc", "head": "def",
        "paths_z": paths, "uncommitted_z": unc, "changes_z": chg});
    if let Some(q) = quota {
        p["quota_bytes"] = json!(q);
    }
    p
}

/// Bytes the quota counts: every object in the worktree bucket, as `retention::usage` sums them.
fn used(svc: &StoreService) -> u64 {
    let n = names("local", PROJ, "wt0").unwrap();
    weftos_store::retention::usage(&Op(svc.operator_objects()), &n.wt).unwrap().1
}

#[test]
fn over_quota_by_one_byte_refuses_before_any_write_and_exactly_at_quota_passes() {
    let (_d, svc, t) = counting_rig();
    let src = tempfile::tempdir().unwrap();
    create_and_publish(&svc, &t, "wt0", src.path(), "t", 10);
    let u = used(&svc);
    t.take();
    let e = go(&svc, "publish", republish_params(src.path(), Some(u - 1)), 30).unwrap_err();
    assert!(e.contains(&format!("holds {u} bytes, over its {} byte quota", u - 1)), "{e}");
    // meta GET (live check) and the one listing; nothing else: no put, no put_cas, no delete, no copy
    pin(t.take(), [1, 0, 1, 0, 0, 0, 0, 0], "refused publish makes no write");
    assert_eq!(used(&svc), u, "the bucket is as it was");
    let (r, c) = counted(&svc, &t, "publish", republish_params(src.path(), Some(u)), 31);
    assert_eq!(r["unchanged"], json!(N), "exactly at quota is allowed (the check is `used > quota`)");
    pin(c, NO_CHANGE_PUBLISH, "exactly at quota: the no-change publish");
}

#[test]
fn overlay_objects_count_toward_the_quota() {
    let (_d, svc, t) = counting_rig();
    let src = tempfile::tempdir().unwrap();
    create_and_publish(&svc, &t, "wt0", src.path(), "t", 10);
    let u = used(&svc);
    let n = names("local", PROJ, "wt0").unwrap();
    weftos_store::overlay::Overlay::new(&Op(svc.operator_objects()), &n.wt, &n.pr).put("assets/o.bin", &[9u8; 40]).unwrap();
    let u2 = used(&svc);
    assert!(u2 > u, "the overlay object adds bytes to the bucket: {u} -> {u2}");
    // the old total would pass; with the overlay object it is refused, and its exact total passes
    t.take();
    let e = go(&svc, "publish", republish_params(src.path(), Some(u)), 30).unwrap_err();
    assert!(e.contains("over its"), "{e}");
    let e = go(&svc, "publish", republish_params(src.path(), Some(u2 - 1)), 31).unwrap_err();
    assert!(e.contains(&format!("holds {u2} bytes")), "{e}");
    assert_eq!((t.take()[3..7]).iter().sum::<u64>(), 0, "neither refusal wrote anything");
    go(&svc, "publish", republish_params(src.path(), Some(u2)), 32).unwrap();
    // the publish's own sweep leaves the overlay object alone
    assert_eq!(used(&svc), u2, "overlay object kept, tree unchanged");
}

#[test]
fn quota_zero_is_unlimited_and_takes_its_listing_inside_the_publish() {
    let (_d, svc, t) = counting_rig();
    let src = tempfile::tempdir().unwrap();
    create_and_publish(&svc, &t, "wt0", src.path(), "t", 10);
    t.take();
    assert!(go(&svc, "publish", republish_params(src.path(), Some(1)), 30).is_err(), "quota 1 refuses");
    t.take();
    let (r, c) = counted(&svc, &t, "publish", republish_params(src.path(), Some(0)), 31);
    assert_eq!(r["unchanged"], json!(N));
    // no quota listing in the daemon: publish_tree lists the bucket itself, so the call count is the
    // same as with a quota, and the default quota (8 GiB) takes the shared-listing path
    pin(c, NO_CHANGE_PUBLISH, "quota 0");
    let (_, c) = counted(&svc, &t, "publish", republish_params(src.path(), None), 32);
    pin(c, NO_CHANGE_PUBLISH, "default quota");
}

#[path = "store_worktree_budget_fresh.rs"]
mod fresh;
