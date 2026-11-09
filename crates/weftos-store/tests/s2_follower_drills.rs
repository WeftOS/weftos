//! ADR-126 §4a "Gate for moving a project onto shared worktrees", items 3 to 6: the part that can be
//! run on one machine. Every drill that touches the store is written once and run three ways:
//! `mem` (in-memory, deterministic), `fs` (the local-directory S1 backend, a real bucketed store)
//! and `live` (a THROWAWAY loopback RustFS; ignored by default).
//!
//! Live runs: set `WEFTOS_STORE_TEST_RUSTFS_ENV` to an env file with `MESH_S3_ENDPOINT`,
//! `MESH_S3_ACCESS_KEY`, `MESH_S3_SECRET_KEY` of a throwaway RustFS (never PG), then
//! `cargo test -p weftos-store --features rustfs --test s2_follower_drills -- --ignored --nocapture`.
//! `--nocapture` shows the measured numbers (item 3).
//!
//! Already covered elsewhere and NOT repeated here: three identities and IAM (`s2_follower_rustfs`),
//! concurrent claims and CAS races (`s2_handoff_cas`, `s2_land_rustfs`), claim expiry on observed
//! silence and epoch fencing (`s2_follower`), registry signing (`s2_registry_sign`). This file adds
//! what those do not: scale, the end-to-end leftover path, crash and outage injection, skewed
//! clocks, damaged registries, the overlap false-positive rate and a shared state file.

use std::cell::{Cell, RefCell};
use weftos_store::backend::AdminBackend;
use weftos_store::follower::*;
use weftos_store::overlay::{ObjectBackend as S2, ObjectMeta, Overlay, Result as R, S2Error};
use weftos_store::promote::*;
use weftos_store::share::*;
use weftos_store::share_fetch::*;
use weftos_store::testing::MemBackend;

const DAY: u64 = 24 * 3600 * 1000;
const T0: u64 = 100 * DAY;

fn actor(n: &str) -> Actor {
    Actor { agent: n.into(), host: format!("host-{n}") }
}

// ---- fault injector -------------------------------------------------------------------------------

/// Wraps a backend and fails on demand: after N more writes, after N more reads, or for everything
/// ("daemon / store down"). A failed call changes nothing, like a connection that never opened.
struct Faulty<'a, B: S2 + ?Sized> {
    inner: &'a B,
    writes_left: Cell<Option<usize>>,
    reads_left: Cell<Option<usize>>,
    down: Cell<bool>,
    writes: Cell<usize>,
}

impl<'a, B: S2 + ?Sized> Faulty<'a, B> {
    fn new(inner: &'a B) -> Self {
        Self { inner, writes_left: Cell::new(None), reads_left: Cell::new(None), down: Cell::new(false), writes: Cell::new(0) }
    }
    fn fail_writes_after(&self, n: usize) {
        self.writes_left.set(Some(n));
    }
    fn fail_reads_after(&self, n: usize) {
        self.reads_left.set(Some(n));
    }
    fn set_down(&self, d: bool) {
        self.down.set(d);
    }
    fn tick(left: &Cell<Option<usize>>, down: bool, what: &str) -> R<()> {
        if down {
            return Err(S2Error::Backend(format!("injected: store down ({what})")));
        }
        match left.get() {
            Some(0) => Err(S2Error::Backend(format!("injected: killed at {what}"))),
            Some(n) => {
                left.set(Some(n - 1));
                Ok(())
            }
            None => Ok(()),
        }
    }
    fn w(&self, what: &str) -> R<()> {
        Self::tick(&self.writes_left, self.down.get(), what)?;
        self.writes.set(self.writes.get() + 1);
        Ok(())
    }
    fn r(&self, what: &str) -> R<()> {
        Self::tick(&self.reads_left, self.down.get(), what)
    }
}

impl<B: S2 + ?Sized> S2 for Faulty<'_, B> {
    fn head(&self, b: &str, k: &str) -> R<Option<ObjectMeta>> {
        self.r("head")?;
        self.inner.head(b, k)
    }
    fn get(&self, b: &str, k: &str) -> R<Option<Vec<u8>>> {
        self.r("get")?;
        self.inner.get(b, k)
    }
    fn put(&self, b: &str, k: &str, d: &[u8]) -> R<ObjectMeta> {
        self.w("put")?;
        self.inner.put(b, k, d)
    }
    fn delete(&self, b: &str, k: &str) -> R<()> {
        self.w("delete")?;
        self.inner.delete(b, k)
    }
    fn list(&self, b: &str, p: &str) -> R<Vec<ObjectMeta>> {
        self.r("list")?;
        self.inner.list(b, p)
    }
    fn put_cas(&self, b: &str, k: &str, d: &[u8], e: Option<&str>) -> R<bool> {
        self.w("put_cas")?;
        self.inner.put_cas(b, k, d, e)
    }
    fn copy(&self, sb: &str, sk: &str, db: &str, dk: &str) -> R<ObjectMeta> {
        self.w("copy")?;
        self.inner.copy(sb, sk, db, dk)
    }
}

// ---- environment: one project bucket, worktree buckets made on demand ----------------------------------

struct Env<'a, B: S2> {
    be: &'a B,
    /// Present for stores that have real buckets (the directory backend, RustFS).
    admin: Option<&'a dyn AdminBackend>,
    u: String,
    pr: String,
    made: RefCell<Vec<String>>,
}

impl<'a, B: S2> Env<'a, B> {
    fn new(be: &'a B, admin: Option<&'a dyn AdminBackend>) -> Self {
        let id = ulid::Ulid::new().to_string().to_lowercase();
        let u = id[id.len() - 10..].to_string();
        let e = Self { be, admin, pr: format!("p-{u}"), u, made: RefCell::new(vec![]) };
        let pr = e.pr.clone();
        e.make(&pr);
        e
    }
    fn make(&self, b: &str) {
        if let Some(a) = self.admin {
            a.create_bucket(b, &[]).unwrap();
        }
        self.made.borrow_mut().push(b.to_string());
    }
    /// A fresh worktree bucket for `slug`, unique to this run.
    fn wt(&self, slug: &str) -> String {
        let b = format!("w-{}-{slug}", self.u);
        self.make(&b);
        b
    }
    fn wipe(&self, b: &str) {
        for m in S2::list(self.be, b, "").unwrap() {
            S2::delete(self.be, b, &m.key).unwrap();
        }
        if let Some(a) = self.admin {
            a.delete_bucket(b).unwrap();
        }
    }
    fn gone(&self, b: &str) -> bool {
        match self.admin {
            Some(a) => !a.bucket_exists(b).unwrap(),
            None => S2::list(self.be, b, "").unwrap().is_empty(),
        }
    }
}

impl<B: S2> Drop for Env<'_, B> {
    fn drop(&mut self) {
        // Best effort, so a failed assertion does not leave buckets in a throwaway store.
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

// ---- publishing helpers ----------------------------------------------------------------------------

struct Tree {
    dir: tempfile::TempDir,
    paths: Vec<String>,
}

fn tree(files: &[(&str, &str)]) -> Tree {
    tree_bytes(&files.iter().map(|(p, c)| (p.to_string(), c.as_bytes().to_vec())).collect::<Vec<_>>())
}

fn tree_bytes(files: &[(String, Vec<u8>)]) -> Tree {
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

fn init_wt<B: S2>(be: &B, bucket: &str, slug: &str, base: &str) {
    write_meta(be, bucket, &WorktreeMeta { slug: slug.into(), project: "p".into(), branch: slug.into(), base_commit: base.into(), created_ms: 0 })
        .unwrap();
}

struct Pub<'a> {
    head: &'a str,
    base: &'a str,
    changes: &'a [PathChange],
    writer: &'a str,
    now: u64,
}

impl Default for Pub<'_> {
    fn default() -> Self {
        Pub { head: "h1", base: "base", changes: &[], writer: "writer", now: T0 }
    }
}

fn push<B: S2>(be: &B, bucket: &str, t: &Tree, p: &Pub<'_>, lim: &Limits) -> R<PublishReport> {
    let src = TreeSource {
        root: t.dir.path(),
        paths: &t.paths,
        uncommitted: &[],
        changes: p.changes,
        branch: bucket,
        base_commit: p.base,
        head_commit: p.head,
        bundle: None,
        writer: actor(p.writer),
    };
    publish_tree(be, bucket, &src, lim, p.now)
}

/// What the daemon does after a publish, handoff, claim or merge.
fn register<B: S2>(be: &B, pr: &str, slug: &str, bucket: &str) -> RegistryEntry {
    let e = summarize(be, slug, bucket).unwrap().unwrap();
    write_registry_entry(be, pr, &e).unwrap();
    e
}

/// A worktree made and published in one go, then registered.
fn up<B: S2>(env: &Env<'_, B>, slug: &str, files: &[(&str, &str)], p: &Pub<'_>) -> RegistryEntry {
    let wb = env.wt(slug);
    init_wt(env.be, &wb, slug, p.base);
    push(env.be, &wb, &tree(files), p, &Limits::default()).unwrap();
    register(env.be, &env.pr, slug, &wb)
}

fn ch(p: &str, k: ChangeKind) -> PathChange {
    PathChange { path: p.into(), kind: k }
}

/// A poll result as sorted `kind:slug` strings, so assertions read plainly.
fn kinds(c: &[Change]) -> Vec<String> {
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

fn entries<B: S2>(be: &B, pr: &str) -> Vec<RegistryEntry> {
    list_shared(be, pr).unwrap()
}

// ---- the three ways to run a drill -----------------------------------------------------------------------

macro_rules! drill {
    ($name:ident, $f:ident) => {
        mod $name {
            use super::*;
            #[test]
            fn mem() {
                let be = MemBackend::new();
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
                let (cfg, _d) = super::live_cfg();
                let be = weftos_store::RustfsBackend::new(cfg).unwrap();
                super::$f(&Env::new(&be, Some(&be)));
            }
        }
    };
}

#[cfg(feature = "rustfs")]
fn live_cfg() -> (weftos_store::RustfsConfig, tempfile::TempDir) {
    let f = std::env::var("WEFTOS_STORE_TEST_RUSTFS_ENV").expect("WEFTOS_STORE_TEST_RUSTFS_ENV");
    let d = tempfile::tempdir().unwrap();
    (weftos_store::RustfsConfig::from_env_file(std::path::Path::new(&f), d.path().join("secrets")).unwrap(), d)
}

// =====================================================================================================
// Item 4: leftover drill, end to end
// =====================================================================================================

fn leftover_end_to_end<B: S2>(env: &Env<'_, B>) {
    let (be, pr) = (env.be, env.pr.as_str());
    let pol = LeftoverPolicy::default();
    let wb = env.wt("idle-docs");
    init_wt(be, &wb, "idle-docs", "base");
    push(be, &wb, &tree(&[("docs/plan.md", "# Plan\n"), ("src/lib.rs", "fn a() {}\n")]), &Pub::default(), &Limits::default()).unwrap();
    // the worktree's own writes (what promotion lands), separate from the shared tree
    Overlay::new(be, &wb, pr).put("assets/plan.md", b"# Plan\n").unwrap();
    let e = register(be, pr, "idle-docs", &wb);

    let mut st = FollowState::default();
    assert_eq!(kinds(&poll(be, pr, &mut st).unwrap()), ["new:idle-docs"]);

    // 1. idle past the threshold: not before it, reported after it, with the real idle time
    assert!(leftovers(std::slice::from_ref(&e), T0 + 2 * DAY, &pol).is_empty(), "two days idle is not forgotten");
    let l = leftovers(&entries(be, pr), T0 + 4 * DAY, &pol);
    assert_eq!((l.len(), l[0].why, l[0].idle_ms), (1, Why::ForgottenOpen, 4 * DAY));

    // 2. hand it off; the daemon refreshes the entry; a follower sees the state change
    mark_handoff(be, &wb, &actor("writer"), "please land", T0 + 4 * DAY).unwrap();
    register(be, pr, "idle-docs", &wb);
    assert_eq!(kinds(&poll(be, pr, &mut st).unwrap()), ["state:idle-docs"]);
    assert!(leftovers(&entries(be, pr), T0 + 4 * DAY + DAY / 2, &pol).is_empty());
    let l = leftovers(&entries(be, pr), T0 + 5 * DAY, &pol);
    assert_eq!((l.len(), l[0].why), (1, Why::UnclaimedHandoff), "a handoff nobody claimed for a day");

    // 3. a second principal claims; the writer can no longer publish over it
    let h = claim_merge(be, &wb, &actor("merger"), T0 + 5 * DAY).unwrap();
    assert_eq!((h.state, h.epoch), (State::Claimed, 1));
    register(be, pr, "idle-docs", &wb);
    let late = push(be, &wb, &tree(&[("docs/plan.md", "# Plan v2\n")]), &Pub { now: T0 + 5 * DAY + 1, ..Pub::default() }, &Limits::default());
    assert!(matches!(late, Err(S2Error::Claimed(_))), "writer is refused while the claim is live: {late:?}");
    assert!(leftovers(&entries(be, pr), T0 + 5 * DAY + 10 * 60_000, &pol).is_empty());
    let l = leftovers(&entries(be, pr), T0 + 5 * DAY + 31 * 60_000, &pol);
    assert_eq!((l.len(), l[0].why), (1, Why::AbandonedClaim), "a claim silent past its TTL");

    // 4. the merger reads the tree (what it would hand to git), lands it, marks it merged
    let out = tempfile::tempdir().unwrap();
    let rep = fetch_tree(be, &wb, out.path(), Select::All, false).unwrap();
    assert_eq!(rep.written.len(), 2);
    assert_eq!(std::fs::read(out.path().join("docs/plan.md")).unwrap(), b"# Plan\n");
    let arch = ArchiveTarget { bucket: pr, prefix: "archive/idle-docs@1/" };
    let pr_rep = promote(be, &wb, pr, &arch, &PromotionPolicy::default(), T0 + 5 * DAY + 1000).unwrap();
    assert_eq!(pr_rep.promoted, ["assets/plan.md"]);
    mark_merged_at(be, &wb, &actor("merger"), Some(1), "landed", T0 + 5 * DAY + 2000).unwrap();
    register(be, pr, "idle-docs", &wb);
    assert_eq!(kinds(&poll(be, pr, &mut st).unwrap()), ["state:idle-docs"]);
    assert!(search_docs(be, &entries(be, pr), "plan").hits.is_empty(), "merged worktrees are not searched");

    // 5. cleanup: the registry entry goes, then the bucket
    remove_registry_entry(be, pr, "idle-docs").unwrap();
    env.wipe(&wb);
    // the last worktree leaving makes the registry empty: a plain poll calls that a possible wrong
    // bucket (an error, state untouched); the confirmed poll accepts it
    assert!(poll(be, pr, &mut st).is_err());
    assert_eq!(kinds(&poll_confirmed(be, pr, &mut st).unwrap()), ["gone:idle-docs"]);
    assert!(entries(be, pr).is_empty());
    assert!(env.gone(&wb), "worktree bucket is gone");
    assert_eq!(S2::get(be, pr, "assets/plan.md").unwrap().unwrap(), b"# Plan\n", "the landed object is in the project");
    assert!(S2::get(be, pr, "archive/idle-docs@1/.wt/handoff.json").unwrap().is_some(), "the record of what was unmerged is kept");
    assert!(leftovers(&entries(be, pr), T0 + 30 * DAY, &pol).is_empty(), "nothing left to nag about");
}
drill!(leftover_drill, leftover_end_to_end);

// =====================================================================================================
// Item 5: failure drills
// =====================================================================================================

/// Writer killed after N writes of a republish, for every N. Nothing may be lost, a follower must
/// never be handed wrong bytes, and retrying the publish must finish the job.
fn writer_killed_mid_publish<B: S2>(env: &Env<'_, B>) {
    let (be, pr) = (env.be, env.pr.as_str());
    let v1 = [("docs/a.md", "# A1\n"), ("docs/b.md", "# B1\n"), ("src/x.rs", "x1"), ("src/gone.rs", "g")];
    let v2 = [("docs/a.md", "# A2\n"), ("docs/b.md", "# B1\n"), ("src/x.rs", "x2"), ("docs/new.md", "# N\n")];
    let (mut failed_at, mut mismatch_windows, mut orphan_runs) = (0, 0, 0);
    for n in 0..=12usize {
        let slug = format!("kill{n}");
        let wb = env.wt(&slug);
        init_wt(be, &wb, &slug, "base");
        push(be, &wb, &tree(&v1), &Pub { head: "h1", ..Pub::default() }, &Limits::default()).unwrap();
        let e1 = register(be, pr, &slug, &wb);
        let mut st = FollowState::default();
        poll(be, pr, &mut st).unwrap();

        let f = Faulty::new(be);
        f.fail_writes_after(n);
        let t2 = tree(&v2);
        let res = push(&f, &wb, &t2, &Pub { head: "h2", now: T0 + 1000, ..Pub::default() }, &Limits::default());
        if res.is_ok() {
            // the sweep reached the end: every write landed
            assert!(n >= 2 && failed_at >= 2, "the sweep must have hit failures first (n={n})");
            break;
        }
        failed_at += 1;

        // The follower's view after the crash: the registry was never refreshed, so no change is seen.
        assert!(poll(be, pr, &mut st).unwrap().is_empty(), "n={n}: a dead writer changes nothing a follower sees");
        let m = read_manifest(be, &wb).unwrap().unwrap();
        assert!(m.head_commit == "h1" || m.head_commit == "h2", "manifest is one whole version, n={n}");
        let hits = search_docs(be, &entries(be, pr), "").hits;
        assert!(hits.iter().any(|h| h.path == "docs/a.md") && hits.iter().any(|h| h.path == "docs/b.md"), "n={n}: no doc lost from the listing");
        // Mirror it: either the whole listed set arrives with matching checksums, or the follower is
        // refused (a changed file was already overwritten under the old manifest). Never wrong bytes.
        let cache = tempfile::tempdir().unwrap();
        match sync_docs(be, &e1, cache.path()) {
            Ok(_) => {
                for d in &read_manifest(be, &wb).unwrap().unwrap().docs {
                    let got = std::fs::read(cache.path().join(&slug).join(&d.path)).unwrap();
                    let want = read_manifest(be, &wb).unwrap().unwrap().files.into_iter().find(|f| f.path == d.path).unwrap().sha256;
                    assert_eq!(sha256_hex(&got), want, "n={n}: {} must match its manifest", d.path);
                }
            }
            Err(S2Error::Backend(m)) if m.contains("checksum mismatch") => mismatch_windows += 1,
            Err(other) => panic!("n={n}: unexpected sync failure {other:?}"),
        }

        // Retry: the same publish, with the store healthy.
        push(be, &wb, &t2, &Pub { head: "h2", now: T0 + 1000, ..Pub::default() }, &Limits::default()).unwrap();
        let m = read_manifest(be, &wb).unwrap().unwrap();
        assert_eq!(m.head_commit, "h2");
        let mut want: Vec<_> = v2.iter().map(|(p, _)| p.to_string()).collect();
        want.sort();
        let mut got: Vec<_> = m.files.iter().map(|f| f.path.clone()).collect();
        got.sort();
        assert_eq!(got, want, "n={n}");
        let mut stored: Vec<_> = S2::list(be, &wb, TREE_PREFIX).unwrap().into_iter().map(|o| o.key[TREE_PREFIX.len()..].to_string()).collect();
        stored.sort();
        if stored != want {
            orphan_runs += 1;
            eprintln!("DRILL n={n}: bucket holds {stored:?}, manifest lists {want:?}");
        }
        for (p, c) in &v2 {
            assert_eq!(S2::get(be, &wb, &format!("{TREE_PREFIX}{p}")).unwrap().unwrap(), c.as_bytes(), "n={n}: {p}");
        }
        let e2 = register(be, pr, &slug, &wb);
        let cache = tempfile::tempdir().unwrap();
        sync_docs(be, &e2, cache.path()).unwrap();
        assert_eq!(std::fs::read(cache.path().join(&slug).join("docs/a.md")).unwrap(), b"# A2\n");
        assert!(kinds(&poll(be, pr, &mut st).unwrap()).contains(&format!("head:{slug}")), "n={n}: the follower sees the finished publish");
    }
    assert!(failed_at >= 4, "the injector must have killed the writer at several different points ({failed_at})");
    eprintln!("DRILL writer-kill: {failed_at} kill points; {mismatch_windows} left followers refused (checksum) until the retry; {orphan_runs} left orphan objects after the retry");
    assert_eq!(orphan_runs, 0, "a retried publish leaves no object the manifest does not list");
}
drill!(writer_kill, writer_killed_mid_publish);

/// Merger killed after claiming, and killed halfway through promoting.
fn merger_killed<B: S2>(env: &Env<'_, B>) {
    let (be, pr) = (env.be, env.pr.as_str());
    let pol = LeftoverPolicy::default();

    // (a) killed right after the claim: the claim is refused to others until the TTL, then recoverable
    let wb = env.wt("claimed");
    init_wt(be, &wb, "claimed", "base");
    push(be, &wb, &tree(&[("docs/a.md", "# A\n")]), &Pub::default(), &Limits::default()).unwrap();
    mark_handoff(be, &wb, &actor("writer"), "go", T0 + 1).unwrap();
    let h1 = claim_merge(be, &wb, &actor("m1"), T0 + 10).unwrap();
    register(be, pr, "claimed", &wb);
    // m1 dies here
    let r = claim_merge(be, &wb, &actor("m2"), T0 + 10 + CLAIM_TTL_MS - 1);
    assert!(matches!(r, Err(S2Error::Claimed(_))), "inside the window: {r:?}");
    let l = leftovers(&entries(be, pr), T0 + 10 + CLAIM_TTL_MS, &pol);
    assert_eq!((l.len(), l[0].why), (1, Why::AbandonedClaim), "a follower can see the stale claim");
    let h2 = claim_merge(be, &wb, &actor("m2"), T0 + 10 + CLAIM_TTL_MS).unwrap();
    assert_eq!((h2.merger, h2.epoch), (Some(actor("m2")), h1.epoch + 1), "taken over, epoch moves");
    // the dead merger wakes up late: its merge is fenced off
    assert!(mark_merged_at(be, &wb, &actor("m1"), Some(h1.epoch), "late", T0 + 2 * CLAIM_TTL_MS).is_err());
    mark_merged_at(be, &wb, &actor("m2"), Some(h2.epoch), "landed", T0 + 2 * CLAIM_TTL_MS).unwrap();
    assert_eq!(read_handoff(be, &wb).unwrap().unwrap().state, State::Merged);

    // (b) killed after N writes of the land, for every N; a second merger finishes it after the window
    let (mut killed, mut finished) = (0, false);
    for n in 0..=40usize {
        let slug = format!("land{n}");
        let wb = env.wt(&slug);
        init_wt(be, &wb, &slug, "base");
        push(be, &wb, &tree(&[("docs/a.md", "# A\n")]), &Pub::default(), &Limits::default()).unwrap();
        let ov = Overlay::new(be, &wb, pr);
        let keys = [format!("assets/it{n}/a.bin"), format!("assets/it{n}/b.bin"), format!("shared/it{n}/c.txt")];
        for (i, k) in keys.iter().enumerate() {
            ov.put(k, format!("payload-{n}-{i}").as_bytes()).unwrap();
        }
        ov.put(&format!("agents/i1/it{n}.log"), b"scratch").unwrap();
        mark_handoff(be, &wb, &actor("writer"), "go", T0 + 1).unwrap();
        let c1 = claim_merge(be, &wb, &actor("m1"), T0 + 10).unwrap();
        let arch = format!("archive/{slug}@1/");
        let at = ArchiveTarget { bucket: pr, prefix: &arch };
        let f = Faulty::new(be);
        f.fail_writes_after(n);
        let first = promote(&f, &wb, pr, &at, &PromotionPolicy::default(), T0 + 20);
        if first.is_ok() {
            finished = true;
            assert!(killed >= 5, "the sweep must have killed the merger at several points ({killed})");
            break;
        }
        killed += 1;
        assert!(is_promoted(be, &wb).unwrap().is_none(), "n={n}: a killed land is not recorded as done");
        // the stale claim, then the takeover by m2 after the window, then the land is retried
        assert!(matches!(claim_merge(be, &wb, &actor("m2"), T0 + 11), Err(S2Error::Claimed(_))));
        let c2 = claim_merge(be, &wb, &actor("m2"), T0 + 10 + CLAIM_TTL_MS).unwrap();
        assert_eq!(c2.epoch, c1.epoch + 1);
        let done = promote(be, &wb, pr, &at, &PromotionPolicy::default(), T0 + 30)
            .unwrap_or_else(|e| panic!("n={n}: the retried land must finish, got {e}"));
        let _ = done;
        for (i, k) in keys.iter().enumerate() {
            assert_eq!(S2::get(be, pr, k).unwrap().unwrap(), format!("payload-{n}-{i}").as_bytes(), "n={n}: {k} landed whole");
        }
        assert!(S2::get(be, pr, &format!("agents/i1/it{n}.log")).unwrap().is_none(), "scratch is archived, never promoted");
        assert!(is_promoted(be, &wb).unwrap().is_some());
        mark_merged_at(be, &wb, &actor("m2"), Some(c2.epoch), "landed", T0 + 40).unwrap();
        assert!(mark_merged_at(be, &wb, &actor("m1"), Some(c1.epoch), "late", T0 + 41).is_err());
    }
    assert!(finished, "raise the sweep bound: the land never completed");
    eprintln!("DRILL merger-kill: killed at {killed} different points of a land; every retry finished");
}
drill!(merger_kill, merger_killed);

/// The daemon (or the store behind it) is down while the registry is being refreshed or read.
fn daemon_down<B: S2>(env: &Env<'_, B>) {
    let (be, pr) = (env.be, env.pr.as_str());
    let a = up(env, "wa", &[("docs/a.md", "# A\n")], &Pub { head: "h1", ..Pub::default() });
    let wb = a.bucket.clone();
    up(env, "wb", &[("docs/b.md", "# B\n")], &Pub { head: "h1", ..Pub::default() });
    let mut st = FollowState::default();
    assert_eq!(kinds(&poll(be, pr, &mut st).unwrap()), ["new:wa", "new:wb"]);
    let before = st.clone();

    // 1. the writer publishes, then the daemon is down for the registry write
    push(be, &wb, &tree(&[("docs/a.md", "# A2\n"), ("docs/c.md", "# C\n")]), &Pub { head: "h2", now: T0 + 5, ..Pub::default() }, &Limits::default()).unwrap();
    let down = Faulty::new(be);
    down.set_down(true);
    let e2 = summarize(be, "wa", &wb).unwrap().unwrap();
    assert!(write_registry_entry(&down, pr, &e2).is_err(), "the refresh fails while the daemon is down");
    assert!(poll(be, pr, &mut st).unwrap().is_empty(), "the follower keeps its previous view");
    assert_eq!(st, before);
    // 2. the daemon is back and refreshes: the follower catches up with one head move
    down.set_down(false);
    write_registry_entry(&down, pr, &e2).unwrap();
    assert_eq!(kinds(&poll(be, pr, &mut st).unwrap()), ["head:wa"]);
    assert!(poll(be, pr, &mut st).unwrap().is_empty(), "and reports it once");

    // 3. the store is unreachable for the follower, at every point of a poll
    push(be, &wb, &tree(&[("docs/a.md", "# A3\n")]), &Pub { head: "h3", now: T0 + 6, ..Pub::default() }, &Limits::default()).unwrap();
    register(be, pr, "wa", &wb);
    let before = st.clone();
    for n in 0..3 {
        let f = Faulty::new(be);
        f.fail_reads_after(n);
        let r = poll(&f, pr, &mut st);
        assert!(r.is_err(), "n={n}: a poll that cannot read the registry is an error, not 'nothing changed'");
        assert_eq!(st, before, "n={n}: and it leaves the follower's memory alone");
    }
    let f = Faulty::new(be);
    f.set_down(true);
    let sr = search_docs(&f, &entries(be, pr), "");
    assert!(sr.hits.is_empty() && sr.unreadable.len() == 2, "an outage is reported as unreadable, not as no docs: {sr:?}");
    assert!(sync_docs(&f, &entries(be, pr)[0], tempfile::tempdir().unwrap().path()).is_err());
    assert_eq!(kinds(&poll(be, pr, &mut st).unwrap()), ["head:wa"], "recovery: the move is reported, nothing is lost or doubled");
}
drill!(daemon_outage, daemon_down);

// =====================================================================================================
// Item 6: follower drills
// =====================================================================================================

/// A merged worktree whose cleanup (entry removal, bucket deletion) never ran, and the opposite:
/// the bucket gone but the entry left behind.
fn merged_but_never_cleaned<B: S2>(env: &Env<'_, B>) {
    let (be, pr) = (env.be, env.pr.as_str());
    let pol = LeftoverPolicy::default();
    let e = up(env, "stuck", &[("docs/a.md", "# A\n")], &Pub::default());
    mark_handoff(be, &e.bucket, &actor("writer"), "go", T0 + 1).unwrap();
    claim_merge(be, &e.bucket, &actor("m"), T0 + 2).unwrap();
    mark_merged_at(be, &e.bucket, &actor("m"), Some(1), "landed", T0 + 3).unwrap();
    register(be, pr, "stuck", &e.bucket);
    let mut st = FollowState::default();
    assert_eq!(kinds(&poll(be, pr, &mut st).unwrap()), ["new:stuck"]);
    assert!(search_docs(be, &entries(be, pr), "a").hits.is_empty(), "never searched as live work");
    assert!(leftovers(&entries(be, pr), T0 + DAY / 2, &pol).is_empty(), "half a day: cleanup may just be slow");
    let l = leftovers(&entries(be, pr), T0 + DAY + 10, &pol);
    assert_eq!((l.len(), l[0].why), (1, Why::StuckCleanup));
    // the stuck bucket stays readable, so an operator can still mirror it
    sync_docs(be, &entries(be, pr)[0], tempfile::tempdir().unwrap().path()).unwrap();
    // cleanup finally runs
    remove_registry_entry(be, pr, "stuck").unwrap();
    env.wipe(&e.bucket);
    assert_eq!(kinds(&poll_confirmed(be, pr, &mut st).unwrap()), ["gone:stuck"]);

    // the other half: the bucket is gone, the entry survived (the removal call failed)
    let o = up(env, "orphan", &[("docs/o.md", "# O\n")], &Pub::default());
    poll(be, pr, &mut st).unwrap();
    env.wipe(&o.bucket);
    assert!(poll(be, pr, &mut st).unwrap().is_empty(), "poll reads only the registry, so it cannot tell");
    let sr = search_docs(be, &entries(be, pr), "o");
    assert_eq!((sr.hits.len(), sr.unreadable.as_slice()), (0, ["orphan".to_string()].as_slice()), "search flags it instead of crashing");
    assert!(sync_docs(be, &entries(be, pr)[0], tempfile::tempdir().unwrap().path()).is_err());
    remove_registry_entry(be, pr, "orphan").unwrap();
    assert_eq!(kinds(&poll_confirmed(be, pr, &mut st).unwrap()), ["gone:orphan"]);
}
drill!(merged_cleanup, merged_but_never_cleaned);

/// Interrupted `sync_docs` (a read fails partway) followed by a doc being deleted: the mirror must
/// end up exactly the published docs, whichever point the interruption hit.
fn interrupted_sync_then_delete<B: S2>(env: &Env<'_, B>) {
    let (be, pr) = (env.be, env.pr.as_str());
    let v1: Vec<(String, String)> = (1..=6).map(|i| (format!("docs/d{i}.md"), format!("# D{i} v1\n"))).collect();
    let mut checked = 0;
    for k in [0usize, 2, 3, 4, 6, 9, 14] {
        let slug = format!("sync{k}");
        let wb = env.wt(&slug);
        init_wt(be, &wb, &slug, "base");
        let v1r: Vec<(&str, &str)> = v1.iter().map(|(p, c)| (p.as_str(), c.as_str())).collect();
        push(be, &wb, &tree(&v1r), &Pub::default(), &Limits::default()).unwrap();
        let e1 = register(be, pr, &slug, &wb);
        let cache = tempfile::tempdir().unwrap();
        let f = Faulty::new(be);
        f.fail_reads_after(k);
        let first = sync_docs(&f, &e1, cache.path());
        assert!(first.is_err() || k >= 9, "k={k}: expected the interruption to bite ({first:?})");
        // d3 deleted, d5 edited, d7 added
        let v2 = [("docs/d1.md", "# D1 v1\n"), ("docs/d2.md", "# D2 v1\n"), ("docs/d4.md", "# D4 v1\n"), ("docs/d5.md", "# D5 v2\n"), ("docs/d6.md", "# D6 v1\n"), ("docs/d7.md", "# D7\n")];
        push(be, &wb, &tree(&v2), &Pub { head: "h2", now: T0 + 9, ..Pub::default() }, &Limits::default()).unwrap();
        let e2 = register(be, pr, &slug, &wb);
        sync_docs(be, &e2, cache.path()).unwrap();
        // the mirror is exactly v2, byte for byte, and holds nothing else
        let mut have = vec![];
        fn walk(d: &std::path::Path, root: &std::path::Path, out: &mut Vec<(String, Vec<u8>)>) {
            for e in std::fs::read_dir(d).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    walk(&p, root, out);
                } else if p.file_name().unwrap() != ".wt-index.json" {
                    out.push((p.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/"), std::fs::read(&p).unwrap()));
                }
            }
        }
        let root = cache.path().join(&slug);
        walk(&root, &root, &mut have);
        have.sort();
        let want: Vec<_> = v2.iter().map(|(p, c)| (p.to_string(), c.as_bytes().to_vec())).collect();
        assert_eq!(have, want, "k={k}: mirror ends consistent");
        checked += 1;
    }
    assert_eq!(checked, 7);
}
drill!(interrupted_sync, interrupted_sync_then_delete);

/// Damaged registries: a corrupt entry, a newer-schema entry, a mislabelled one, a corrupt manifest,
/// an empty project bucket, a wrong project bucket.
fn damaged_registries<B: S2>(env: &Env<'_, B>) {
    let (be, pr) = (env.be, env.pr.as_str());
    up(env, "good", &[("docs/g.md", "# G\n")], &Pub::default());
    let fwd = up(env, "fwd", &[("docs/f.md", "# F\n")], &Pub::default());
    // an entry written by a newer daemon that added a field: still readable
    let mut j: serde_json::Value = serde_json::to_value(&fwd).unwrap();
    j["added_in_v2"] = serde_json::json!({"x": 1});
    S2::put(be, pr, "worktrees/fwd.json", &serde_json::to_vec(&j).unwrap()).unwrap();
    S2::put(be, pr, "worktrees/corrupt.json", b"{ truncated").unwrap();
    S2::put(be, pr, "worktrees/future.json", br#"{"slug":"future","state":"reviewing","bucket":"w-x"}"#).unwrap();
    let mut mis = serde_json::to_value(&fwd).unwrap();
    mis["slug"] = "someone-else".into();
    S2::put(be, pr, "worktrees/mislabel.json", &serde_json::to_vec(&mis).unwrap()).unwrap();

    let mut st = FollowState::default();
    let c = poll(be, pr, &mut st).unwrap();
    assert_eq!(kinds(&c), ["bad:corrupt", "bad:future", "bad:mislabel", "new:fwd", "new:good"]);
    assert_eq!(kinds(&poll(be, pr, &mut st).unwrap()), ["bad:corrupt", "bad:future", "bad:mislabel"], "reported every poll until fixed, nothing else repeats");
    assert!(list_shared(be, pr).is_err(), "the strict listing refuses a damaged registry");
    let (ok, bad) = list_shared_lenient(be, pr).unwrap();
    assert_eq!((ok.len(), bad.len()), (2, 3));
    let sr = search_docs(be, &ok, "");
    assert_eq!(sr.hits.len(), 2, "the readable ones are still searched");

    // a corrupt manifest in one worktree: search flags it, mirroring and overlaps refuse, poll is unaffected
    S2::put(be, &fwd.bucket, MANIFEST_KEY, b"junk").unwrap();
    let sr = search_docs(be, &ok, "");
    assert_eq!((sr.hits.len(), sr.unreadable), (1, vec!["fwd".to_string()]));
    assert!(sync_docs(be, &fwd, tempfile::tempdir().unwrap().path()).is_err());
    assert!(overlaps(be, &fwd, &ok.iter().find(|e| e.slug == "good").unwrap().clone()).is_err());
    assert!(!kinds(&poll(be, pr, &mut st).unwrap()).iter().any(|k| k.starts_with("gone")));

    // an empty project bucket
    let empty = env.wt("emptyproj");
    assert!(poll(be, &empty, &mut FollowState::default()).unwrap().is_empty(), "a first look at an empty project is not an error");
    let before = st.clone();
    assert!(poll(be, &empty, &mut st).is_err(), "an empty registry after worktrees were known is refused");
    assert_eq!(st, before);
    assert!(kinds(&poll_confirmed(be, &empty, &mut st).unwrap()).iter().all(|k| k.starts_with("gone")));

    // a wrong (other) project bucket: it is just another registry; what a follower sees flips wholesale
    // and loudly in one poll, never silently mixed
    let mut st = FollowState::default();
    poll(be, pr, &mut st).unwrap();
    let other = env.wt("otherproj");
    let ob = env.wt("otherwt");
    init_wt(be, &ob, "theirs", "base");
    push(be, &ob, &tree(&[("docs/t.md", "# T\n")]), &Pub::default(), &Limits::default()).unwrap();
    register(be, &other, "theirs", &ob);
    let c = kinds(&poll(be, &other, &mut st).unwrap());
    assert_eq!(c, ["gone:fwd", "gone:good", "new:theirs"]);
}
drill!(damaged_registry, damaged_registries);

/// A bucket that does not exist at all (live stores refuse; the in-memory one reports empty).
#[cfg(feature = "rustfs")]
#[test]
#[ignore = "needs a throwaway RustFS: WEFTOS_STORE_TEST_RUSTFS_ENV"]
fn live_nonexistent_project_bucket_is_an_error_and_keeps_state() {
    let (cfg, _d) = live_cfg();
    let be = weftos_store::RustfsBackend::new(cfg).unwrap();
    let env = Env::new(&be, Some(&be));
    up(&env, "a", &[("docs/a.md", "# A\n")], &Pub::default());
    let mut st = FollowState::default();
    poll(&be, &env.pr, &mut st).unwrap();
    let before = st.clone();
    let missing = format!("p-nope-{}", env.u);
    let first = poll(&be, &missing, &mut FollowState::default());
    eprintln!("DRILL missing-bucket: first poll with no state -> {:?}", first.as_ref().map(|c| c.len()).map_err(|e| e.to_string()));
    assert!(poll(&be, &missing, &mut st).is_err());
    assert_eq!(st, before, "the follower's memory survives a wrong bucket name");
}

// ---- clocks ---------------------------------------------------------------------------------------------

#[test]
fn skewed_writer_clocks_give_sane_verdicts() {
    let pol = LeftoverPolicy::default();
    let b = MemBackend::new();
    let now = T0 + 10 * DAY; // the follower's clock
    let mk = |slug: &str, state: &str, writer_now: u64| {
        let wb = format!("w-{slug}");
        init_wt(&b, &wb, slug, "base");
        push(&b, &wb, &tree(&[("docs/a.md", "# A\n")]), &Pub { now: writer_now, ..Pub::default() }, &Limits::default()).unwrap();
        let w = actor("writer");
        match state {
            "handoff" => mark_handoff(&b, &wb, &w, "go", writer_now).unwrap(),
            "claimed" => {
                mark_handoff(&b, &wb, &w, "go", writer_now).unwrap();
                claim_merge(&b, &wb, &actor("m"), writer_now).unwrap();
            }
            "merged" => {
                mark_handoff(&b, &wb, &w, "go", writer_now).unwrap();
                claim_merge(&b, &wb, &actor("m"), writer_now).unwrap();
                mark_merged_at(&b, &wb, &actor("m"), Some(1), "done", writer_now).unwrap();
            }
            _ => {}
        }
        summarize(&b, slug, &wb).unwrap().unwrap()
    };
    let why = |es: &[RegistryEntry]| -> Vec<(String, Why)> { leftovers(es, now, &pol).into_iter().map(|l| (l.entry.slug, l.why)).collect() };

    // writer a day AHEAD: its stamps are in the follower's future by a day, far beyond the skew allowance
    let ahead = [mk("a-open", "open", now + DAY), mk("a-hand", "handoff", now + DAY), mk("a-claim", "claimed", now + DAY)];
    let got = why(&ahead);
    assert_eq!(got.len(), 3);
    assert!(got.iter().all(|(_, w)| *w == Why::FutureTimestamp), "reported as skew, never as 'idle for a negative time': {got:?}");
    assert!(leftovers(&ahead, now, &pol).iter().all(|l| l.idle_ms == 0));
    // a merged entry stamped in the future is not nagged about until the follower's clock reaches it
    assert!(why(&[mk("a-merged", "merged", now + DAY)]).is_empty());
    // within the allowance (30 min ahead) it is simply fresh
    assert!(why(&[mk("a-near", "open", now + 30 * 60_000)]).is_empty());

    // writer a day BEHIND: ages are a day too old
    let open = mk("b-open", "open", now - 3 * DAY + 1);
    assert!(why(&[open]).is_empty(), "stamped just inside the 3-day limit: not forgotten");
    let open = mk("b-open2", "open", now - 3 * DAY - DAY);
    assert_eq!(why(&[open]), [("b-open2".to_string(), Why::ForgottenOpen)]);
    let fresh_but_stamped_early = mk("b-fresh", "open", now - DAY);
    assert!(why(&[fresh_but_stamped_early]).is_empty(), "a live open worktree one day behind is not forgotten (limit is 3 days)");
    let hand = mk("b-hand", "handoff", now - DAY);
    assert_eq!(why(&[hand]), [("b-hand".to_string(), Why::UnclaimedHandoff)], "a day of skew turns a fresh handoff into a nudge: documented, harmless");
    let claim = mk("b-claim", "claimed", now - DAY);
    assert_eq!(why(&[claim]), [("b-claim".to_string(), Why::AbandonedClaim)], "a live claim looks abandoned to a follower, only as a report");
    // never a panic or a wrap-around at the extremes
    let mut e = mk("x", "open", 0);
    for u in [0, 1, u64::MAX - 1, u64::MAX] {
        e.updated_ms = u;
        for n in [0, 1, u64::MAX] {
            let _ = leftovers(&[e.clone()], n, &pol);
        }
    }
}

// ---- overlaps with different bases ----------------------------------------------------------------------

fn project_files(drift: bool) -> Vec<(String, Vec<u8>)> {
    (0..200)
        .map(|i| (format!("src/f{i}.rs"), if drift && i < 10 { format!("main {i}") } else { format!("base {i}") }.into_bytes()))
        .collect()
}

fn edit(mut files: Vec<(String, Vec<u8>)>, who: &str, idx: &[usize]) -> (Vec<(String, Vec<u8>)>, Vec<PathChange>) {
    let mut ch_ = vec![];
    for &i in idx {
        files[i].1 = format!("{who} {i}").into_bytes();
        ch_.push(ch(&files[i].0, ChangeKind::Modified));
    }
    (files, ch_)
}

/// Naive content comparison, the way `overlaps` used to answer: any path whose bytes differ.
fn naive_diff(a: &[(String, Vec<u8>)], b: &[(String, Vec<u8>)]) -> usize {
    a.iter().zip(b).filter(|(x, y)| x.0 == y.0 && x.1 != y.1).count()
}

#[test]
fn overlaps_report_only_paths_both_worktrees_changed_even_with_different_bases() {
    let b = MemBackend::new();
    let truth = ["src/f100.rs", "src/f101.rs"];
    for (label, base_b, drift_b) in [("same base", "c1", false), ("different bases", "c2", true)] {
        let (fa, ca) = edit(project_files(false), "a", &[20, 21, 22, 23, 24, 100, 101]);
        let (fb, cb) = edit(project_files(drift_b), "b", &[30, 31, 32, 33, 34, 100, 101]);
        let naive = naive_diff(&fa, &fb);
        let (wa, wb) = (format!("w-a-{}", label.len()), format!("w-b-{}", label.len()));
        init_wt(&b, &wa, "a", "c1");
        init_wt(&b, &wb, "b", base_b);
        push(&b, &wa, &tree_bytes(&fa), &Pub { base: "c1", changes: &ca, ..Pub::default() }, &Limits::default()).unwrap();
        push(&b, &wb, &tree_bytes(&fb), &Pub { base: base_b, changes: &cb, ..Pub::default() }, &Limits::default()).unwrap();
        let (ea, eb) = (summarize(&b, "a", &wa).unwrap().unwrap(), summarize(&b, "b", &wb).unwrap().unwrap());
        let rep = overlaps(&b, &ea, &eb).unwrap();
        assert_eq!(rep.same_base, !drift_b, "{label}");
        let got: Vec<_> = rep.overlaps.iter().map(|o| o.path.as_str()).collect();
        let fp = naive - truth.len();
        eprintln!("DRILL overlaps [{label}]: {} true conflicts; plain content comparison would report {naive} ({fp} false positives, {:.0}%); reported now: {}", truth.len(), 100.0 * fp as f64 / naive as f64, got.len());
        assert_eq!(got, truth, "{label}: only the two files both worktrees edited");
    }

    // an older manifest with no change list: falls back to content comparison, which over-reports
    let (fa, _) = edit(project_files(false), "a", &[20, 100]);
    let (fb, _) = edit(project_files(false), "b", &[30, 100]);
    init_wt(&b, "w-old-a", "olda", "c1");
    init_wt(&b, "w-old-b", "oldb", "c1");
    push(&b, "w-old-a", &tree_bytes(&fa), &Pub { base: "c1", ..Pub::default() }, &Limits::default()).unwrap();
    push(&b, "w-old-b", &tree_bytes(&fb), &Pub { base: "c1", ..Pub::default() }, &Limits::default()).unwrap();
    let (ea, eb) = (summarize(&b, "olda", "w-old-a").unwrap().unwrap(), summarize(&b, "oldb", "w-old-b").unwrap().unwrap());
    let got: Vec<_> = overlaps(&b, &ea, &eb).unwrap().overlaps.into_iter().map(|o| o.path).collect();
    assert_eq!(got, ["src/f100.rs", "src/f20.rs", "src/f30.rs"], "without change lists it is a superset, as documented");
}

// ---- two follow runs sharing one state file ---------------------------------------------------------------

#[test]
fn two_followers_sharing_a_state_file_lose_nothing_and_leave_a_valid_file() {
    let b = MemBackend::new();
    let pr = "p-shared";
    let sf = tempfile::tempdir().unwrap();
    let path = sf.path().join("follow.json");
    let mk = |slug: &str, head: &str| {
        let wb = format!("w-{slug}");
        if read_meta(&b, &wb).unwrap().is_none() {
            init_wt(&b, &wb, slug, "base");
        }
        push(&b, &wb, &tree(&[("docs/a.md", head)]), &Pub { head, ..Pub::default() }, &Limits::default()).unwrap();
        register(&b, pr, slug, &wb);
    };
    mk("one", "h1");
    mk("two", "h1");

    // X and Y both start from the same (empty) file, as two cron jobs would
    let (mut x, mut y) = (FollowState::load(&path).unwrap(), FollowState::load(&path).unwrap());
    let cx = poll(&b, pr, &mut x).unwrap();
    x.save(&path).unwrap();
    mk("three", "h1");
    let cy = poll(&b, pr, &mut y).unwrap(); // Y never saw X's save
    y.save(&path).unwrap();
    assert_eq!(kinds(&cx), ["new:one", "new:two"]);
    assert_eq!(kinds(&cy), ["new:one", "new:three", "new:two"], "Y re-reports what X already reported: duplicates, not losses");
    assert_eq!(FollowState::load(&path).unwrap(), y, "last writer wins");

    // the loser overwrites a newer state with an older one: the next run re-reports, never skips
    mk("one", "h2");
    let mut x2 = FollowState::load(&path).unwrap();
    let c1 = poll(&b, pr, &mut x2).unwrap();
    assert_eq!(kinds(&c1), ["head:one"]);
    let y2 = y.clone(); // Y still holds the pre-h2 view, and saves after X
    x2.save(&path).unwrap();
    y2.save(&path).unwrap();
    let mut z = FollowState::load(&path).unwrap();
    assert_eq!(kinds(&poll(&b, pr, &mut z).unwrap()), ["head:one"], "the h2 move is reported again, not lost");
    z.save(&path).unwrap();
    assert!(poll(&b, pr, &mut z).unwrap().is_empty());

    // the file is valid JSON at the end, and no temp files are left behind
    let _: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let left: Vec<_> = std::fs::read_dir(sf.path()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    assert_eq!(left, ["follow.json"], "{left:?}");
}

#[test]
fn concurrent_saves_and_loads_never_see_a_torn_file_or_fail() {
    let sf = tempfile::tempdir().unwrap();
    let path = sf.path().join("follow.json");
    let state = |i: usize| {
        let mut s = FollowState::default();
        for k in 0..200 {
            s.seen.insert(format!("wt-{k}"), Seen { etag: None, bucket: format!("w-{i}-{k}"), head_commit: "h".repeat(40), state: State::Open });
        }
        s
    };
    FollowState::default().save(&path).unwrap();
    std::thread::scope(|s| {
        for t in 0..6 {
            let (path, state) = (&path, &state);
            s.spawn(move || {
                for i in 0..150 {
                    state(t * 1000 + i).save(path).unwrap_or_else(|e| panic!("save failed under contention: {e}"));
                    FollowState::load(path).unwrap_or_else(|e| panic!("load saw a torn file: {e}"));
                }
            });
        }
    });
    let last = FollowState::load(&path).unwrap();
    assert_eq!(last.seen.len(), 200);
    let left: Vec<_> = std::fs::read_dir(sf.path()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    assert_eq!(left, ["follow.json"], "{left:?}");
}

// =====================================================================================================
// Item 3: list and head latency for a Sansone-sized worktree (live only: numbers are the point)
// =====================================================================================================

#[cfg(feature = "rustfs")]
#[test]
#[ignore = "needs a throwaway RustFS: WEFTOS_STORE_TEST_RUSTFS_ENV"]
fn live_sansone_sized_worktree_listing_and_poll_latency() {
    use std::time::Instant;
    use weftos_store::backend::ObjectBackend as S1;
    let (cfg, _d) = live_cfg();
    let be = weftos_store::RustfsBackend::new(cfg).unwrap();
    let env = Env::new(&be, Some(&be));
    let pr = env.pr.clone();
    let ms = |t: Instant| t.elapsed().as_secs_f64() * 1000.0;

    // 4800 small source files (~1 to 3 KB), 200 docs, 3 files of 20 MB
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    for i in 0..4800 {
        let body = format!("// file {i}\n").repeat(40 + (i % 120));
        files.push((format!("src/m{:02}/f{i}.rs", i % 60), body.into_bytes()));
    }
    for i in 0..200 {
        files.push((format!("docs/d{i}.md"), format!("# Doc {i}\n{}", "text line\n".repeat(60 + i % 50)).into_bytes()));
    }
    let mut x = 0x9E3779B97F4A7C15u64;
    for n in 0..3 {
        let mut big = vec![0u8; 20 << 20];
        for c in big.chunks_mut(8) {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            c.copy_from_slice(&x.to_le_bytes()[..c.len()]);
        }
        files.push((format!("assets/big{n}.bin"), big));
    }
    let total: usize = files.iter().map(|f| f.1.len()).sum();
    let t = tree_bytes(&files);
    let lim = Limits { max_file: 32 << 20, ..Limits::default() };
    let wb = env.wt("sansone");
    init_wt(&be, &wb, "sansone", "base");

    let t1 = Instant::now();
    let rep = push(&be, &wb, &t, &Pub::default(), &lim).unwrap();
    let publish_first = ms(t1);
    assert_eq!((rep.uploaded, rep.skipped.len()), (5003, 0), "{:?}", rep.skipped);
    let t1 = Instant::now();
    let rep = push(&be, &wb, &t, &Pub { now: T0 + 1, ..Pub::default() }, &lim).unwrap();
    let publish_again = ms(t1);
    assert_eq!((rep.uploaded, rep.unchanged), (0, 5003));
    std::fs::write(t.dir.path().join("docs/d1.md"), "# Doc 1 edited\n").unwrap();
    let t1 = Instant::now();
    let rep = push(&be, &wb, &t, &Pub { now: T0 + 2, head: "h2", ..Pub::default() }, &lim).unwrap();
    let publish_one_edit = ms(t1);
    assert_eq!(rep.uploaded, 1);
    let t1 = Instant::now();
    let e = register(&be, &pr, "sansone", &wb);
    let registry_write = ms(t1);

    // nine small neighbours, so a poll lists and reads ten entries
    for i in 0..9 {
        up(&env, &format!("nb{i}"), &[("docs/n.md", "# N\n"), ("src/a.rs", "fn a(){}")], &Pub::default());
    }
    let mut st = FollowState::default();
    let t1 = Instant::now();
    let c = poll(&be, &pr, &mut st).unwrap();
    let poll_first = ms(t1);
    assert_eq!(c.len(), 10);
    let t1 = Instant::now();
    assert!(poll(&be, &pr, &mut st).unwrap().is_empty());
    let poll_quiet = ms(t1);
    let t1 = Instant::now();
    let l = S2::list(&be, &wb, "").unwrap();
    let list_all = ms(t1);
    assert!(l.len() >= 5003 + 3, "list pages through every object: {}", l.len());
    let t1 = Instant::now();
    S1::head(&be, &wb, MANIFEST_KEY).unwrap();
    let head_manifest = ms(t1);
    let t1 = Instant::now();
    let m = read_manifest(&be, &wb).unwrap().unwrap();
    let get_manifest = ms(t1);
    let man_bytes = serde_json::to_vec_pretty(&m).unwrap().len();
    let all = entries(&be, &pr);
    let t1 = Instant::now();
    let sr = search_docs(&be, &all, "doc");
    let search = ms(t1);
    assert!(sr.unreadable.is_empty() && sr.hits.len() >= 200);
    let cache = tempfile::tempdir().unwrap();
    let t1 = Instant::now();
    let fr = sync_docs(&be, &e, cache.path()).unwrap();
    let sync_first = ms(t1);
    assert_eq!(fr.written.len(), 200);
    let t1 = Instant::now();
    let fr = sync_docs(&be, &e, cache.path()).unwrap();
    let sync_again = ms(t1);
    assert_eq!(fr.written.len(), 0);
    let out = tempfile::tempdir().unwrap();
    let t1 = Instant::now();
    let fr = fetch_tree_with(&be, &wb, out.path(), Select::All, false, &lim).unwrap();
    let fetch_all = ms(t1);
    assert_eq!(fr.written.len(), 5003);

    eprintln!("DRILL perf: {} files, {:.1} MB (3 x 20 MB), manifest {:.0} KB", files.len(), total as f64 / 1048576.0, man_bytes as f64 / 1024.0);
    eprintln!("DRILL perf: publish first {publish_first:.0} ms | republish unchanged {publish_again:.0} ms | one edit {publish_one_edit:.0} ms | registry write {registry_write:.0} ms");
    eprintln!("DRILL perf: poll 10 worktrees first {poll_first:.0} ms | quiet {poll_quiet:.0} ms | list worktree ({} objects) {list_all:.0} ms | head manifest {head_manifest:.0} ms | get manifest {get_manifest:.0} ms", l.len());
    eprintln!("DRILL perf: search_docs over 10 {search:.0} ms | sync_docs first (200 docs) {sync_first:.0} ms | again {sync_again:.0} ms | fetch whole tree {fetch_all:.0} ms");
    // generous bounds: the point is the printed numbers, these only catch something badly wrong
    assert!(poll_first < 10_000.0 && poll_quiet < 10_000.0, "poll");
    assert!(list_all < 30_000.0 && head_manifest < 5_000.0 && get_manifest < 10_000.0);
    assert!(search < 20_000.0 && sync_first < 60_000.0 && sync_again < 30_000.0);
    assert!(publish_first < 600_000.0 && publish_again < 300_000.0 && fetch_all < 600_000.0);
}
