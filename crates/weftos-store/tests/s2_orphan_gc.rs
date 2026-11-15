//! WO-069 lane E: orphan marking and gc on the in-memory backend with a test clock, and a recording
//! backend that proves gc touches only the orphan's own bucket, its registry entry, that entry's
//! signature object and the digest.
use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use weftos_store::overlay::{ObjectBackend, ObjectMeta, Result as R, S2Error};
use weftos_store::orphan::*;
use weftos_store::promote::{ArchiveTarget, WorktreeMeta, write_meta};
use weftos_store::share::*;
use weftos_store::share_fetch::*;
use weftos_store::testing::MemBackend;

pub const H: u64 = 3_600_000;
pub const D: u64 = 24 * H;
const PR: &str = "proj";
const ARCH: &str = "weftos-archive";

#[derive(Default)]
struct Spy {
    inner: MemBackend,
    log: RefCell<Vec<(&'static str, String, String)>>,
    fail_copy_of: RefCell<Option<String>>,
    puts: Cell<usize>,
}
impl Spy {
    fn note(&self, op: &'static str, b: &str, k: &str) {
        self.log.borrow_mut().push((op, b.into(), k.into()));
    }
    fn clear(&self) {
        self.log.borrow_mut().clear();
    }
    /// (bucket, key) pairs mutated since the last clear: put, delete, copy destination.
    fn mutated(&self) -> BTreeSet<(String, String)> {
        self.log.borrow().iter().filter(|(o, ..)| matches!(*o, "put" | "delete" | "copy-dst")).map(|(_, b, k)| (b.clone(), k.clone())).collect()
    }
    fn buckets_touched(&self) -> BTreeSet<String> {
        self.log.borrow().iter().map(|(_, b, _)| b.clone()).collect()
    }
}
impl ObjectBackend for Spy {
    fn head(&self, b: &str, k: &str) -> R<Option<ObjectMeta>> {
        self.note("head", b, k);
        self.inner.head(b, k)
    }
    fn get(&self, b: &str, k: &str) -> R<Option<Vec<u8>>> {
        self.note("get", b, k);
        self.inner.get(b, k)
    }
    fn put(&self, b: &str, k: &str, d: &[u8]) -> R<ObjectMeta> {
        self.note("put", b, k);
        self.puts.set(self.puts.get() + 1);
        self.inner.put(b, k, d)
    }
    fn delete(&self, b: &str, k: &str) -> R<()> {
        self.note("delete", b, k);
        self.inner.delete(b, k)
    }
    fn list(&self, b: &str, p: &str) -> R<Vec<ObjectMeta>> {
        self.note("list", b, p);
        self.inner.list(b, p)
    }
    fn copy(&self, sb: &str, sk: &str, db: &str, dk: &str) -> R<ObjectMeta> {
        if self.fail_copy_of.borrow().as_deref() == Some(sk) {
            return Err(S2Error::Backend("injected copy failure".into()));
        }
        self.note("copy-src", sb, sk);
        self.note("copy-dst", db, dk);
        self.inner.copy(sb, sk, db, dk)
    }
}

fn actor() -> Actor {
    Actor { agent: "w".into(), host: "h".into() }
}

/// Publish `slug` into bucket `w-<slug>` with an overlay object, a bundle-less tree, and register it.
fn publish(b: &Spy, slug: &str, at: u64) -> RegistryEntry {
    let d = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(d.path().join("docs")).unwrap();
    std::fs::write(d.path().join("docs/plan.md"), format!("# Plan {slug}\n")).unwrap();
    let paths = vec!["docs/plan.md".to_string()];
    let bucket = format!("w-{slug}");
    let src = TreeSource {
        root: d.path(), paths: &paths, uncommitted: &paths, changes: &[], changes_recorded: false, branch: slug, base_commit: "base",
        head_commit: "head-1", bundle: None, writer: actor(),
    };
    let meta = WorktreeMeta { slug: slug.into(), project: "01abc".into(), branch: slug.into(), base_commit: "base".into(), created_ms: 0 };
    write_meta(b, &bucket, &meta).unwrap();
    b.put(&bucket, "scratch/notes.txt", b"overlay work").unwrap();
    publish_tree(b, &bucket, &src, &Limits::default(), at).unwrap();
    let e = summarize(b, slug, &bucket).unwrap().unwrap();
    write_registry_entry(b, PR, &e).unwrap();
    e
}

fn snapshot(b: &Spy, bucket: &str) -> Vec<(String, String)> {
    let mut v: Vec<_> = b.inner.list(bucket, "").unwrap().into_iter().map(|m| (m.key, m.etag)).collect();
    v.sort();
    v
}

fn pol() -> OrphanPolicy {
    OrphanPolicy::default()
}

fn entry_state(b: &Spy, slug: &str) -> Option<State> {
    list_shared(b, PR).unwrap().into_iter().find(|e| e.slug == slug).map(|e| e.state)
}

fn mark(b: &Spy, slug: &str, now: u64, since: u64) -> Result<RegistryEntry, OrphanError> {
    mark_orphan(b, PR, slug, now, since, &pol())
}

fn arch(now: u64, bucket: &str) -> (String, &'static str) {
    (format!("{bucket}@{now}/"), ARCH)
}

#[test]
fn policy_defaults_env_names_and_zero_refused() {
    let p = OrphanPolicy::default();
    assert_eq!((p.orphan_after_ms, p.archive_after_ms), (24 * H, 14 * D));
    let env = |k: &str| match k {
        ENV_ORPHAN_AFTER_HOURS => Some("6".to_string()),
        ENV_ARCHIVE_AFTER_DAYS => Some("30".to_string()),
        _ => None,
    };
    let p = OrphanPolicy::from_lookup(env).unwrap();
    assert_eq!((p.orphan_after_ms, p.archive_after_ms), (6 * H, 30 * D));
    assert_eq!(OrphanPolicy::from_lookup(|_| None).unwrap(), OrphanPolicy::default());
    assert!(OrphanPolicy::from_lookup(|k| (k == ENV_ARCHIVE_AFTER_DAYS).then(|| "0".into())).is_err(), "zero refused");
    assert!(OrphanPolicy::from_lookup(|k| (k == ENV_ORPHAN_AFTER_HOURS).then(|| "soon".into())).is_err(), "not a number");
    assert!(OrphanPolicy { orphan_after_ms: 0, archive_after_ms: 1 }.validate().is_err());
}

#[test]
fn marking_follows_the_clock_and_refuses_what_it_must() {
    let b = Spy::default();
    publish(&b, "a", 1_000);
    let since = 10 * D;
    // The grace: one millisecond short refuses, exactly on the threshold marks.
    let short = since + 24 * H - 1;
    assert!(matches!(mark(&b, "a", short, since), Err(OrphanError::Refused(Refusal::TooSoon { .. }))));
    assert_eq!(entry_state(&b, "a"), Some(State::Open), "a refusal writes nothing");
    assert!(matches!(mark(&b, "a", since - 1, since), Err(OrphanError::Refused(Refusal::FutureSince))));
    assert!(matches!(mark(&b, "ghost", since + D, since), Err(OrphanError::Refused(Refusal::NoEntry))));
    let now = since + 24 * H;
    let m = mark(&b, "a", now, since).unwrap();
    assert_eq!(m.state, State::Orphan);
    assert_eq!(m.orphan, Some(OrphanInfo { orphaned_at_ms: now, last_head: "head-1".into(), missing_since_ms: since }));
    assert_eq!(entry_state(&b, "a"), Some(State::Orphan));
    let digest = read_digest(&b, PR).unwrap().unwrap();
    assert!(digest.entries.contains_key("a"), "digest still lists it");
    // Idempotent: a second call writes nothing and keeps the first marking time.
    b.clear();
    let again = mark(&b, "a", now + 5 * H, since).unwrap();
    assert_eq!(again.orphan.unwrap().orphaned_at_ms, now);
    assert!(b.mutated().is_empty(), "no write when already an orphan");
    // Coming back clears it: a publish re-derives the entry from the bucket.
    publish(&b, "a", now + 6 * H);
    let back = list_shared(&b, PR).unwrap().into_iter().find(|e| e.slug == "a").unwrap();
    assert_eq!((back.state, back.orphan), (State::Open, None));
}

#[test]
fn only_an_open_unlanded_store_can_become_an_orphan() {
    let b = Spy::default();
    let (since, now) = (D, D + 3 * D);
    for slug in ["handoff", "claimed", "merged", "landed", "stale-entry"] {
        publish(&b, slug, 1_000);
    }
    mark_handoff(&b, "w-handoff", &actor(), "ready", 2_000).unwrap();
    mark_handoff(&b, "w-claimed", &actor(), "ready", 2_000).unwrap();
    claim_merge(&b, "w-claimed", &Actor { agent: "m".into(), host: "h2".into() }, 3_000).unwrap();
    mark_handoff(&b, "w-merged", &actor(), "ready", 2_000).unwrap();
    mark_landed(&b, "w-merged", &actor(), "landed", 3_000).unwrap();
    for s in ["handoff", "claimed", "merged"] {
        let e = summarize(&b, s, &format!("w-{s}")).unwrap().unwrap();
        write_registry_entry(&b, PR, &e).unwrap();
        assert!(matches!(mark(&b, s, now, since), Err(OrphanError::Refused(Refusal::NotOpen(_)))), "{s}");
    }
    // The registry entry still says open, but the worktree's own bucket says it landed (promoted).
    weftos_store::promote::promote(
        &b, "w-landed", PR, &ArchiveTarget { bucket: ARCH, prefix: "w-landed@5/" },
        &weftos_store::promote::PromotionPolicy::default(), 5,
    ).unwrap();
    assert!(matches!(mark(&b, "landed", now, since), Err(OrphanError::Refused(Refusal::Landed))), "promoted");
    // Entry lags the bucket: the bucket's handoff says claimed while the entry still says open.
    mark_handoff(&b, "w-stale-entry", &actor(), "ready", 2_000).unwrap();
    claim_merge(&b, "w-stale-entry", &actor(), 3_000).unwrap();
    assert!(matches!(mark(&b, "stale-entry", now, since), Err(OrphanError::Refused(Refusal::Landed))), "bucket record wins");
    for s in ["handoff", "claimed", "merged", "landed", "stale-entry"] {
        assert_ne!(entry_state(&b, s), Some(State::Orphan), "{s} was never marked");
    }
}

/// One project with orphan `a` (marked at `t0`), a second worktree `other`, and project files.
fn world() -> (Spy, u64) {
    let b = Spy::default();
    publish(&b, "a", 1_000);
    publish(&b, "other", 1_000);
    b.put(PR, "docs/readme.md", b"project file").unwrap();
    b.put(PR, "assets/x.bin", b"x").unwrap();
    let t0 = 20 * D;
    mark(&b, "a", t0, t0 - 2 * D).unwrap();
    (b, t0)
}

#[test]
fn the_dry_run_lists_orphans_with_their_age_and_changes_nothing() {
    let (b, t0) = world();
    b.clear();
    let before: Vec<_> = [PR, "w-a", "w-other"].iter().map(|x| snapshot(&b, x)).collect();
    let young = plan_orphans(&b, PR, t0 + 14 * D - 1, &pol()).unwrap();
    assert_eq!(young.len(), 1, "only the orphan, not `other`");
    let c = &young[0];
    assert_eq!((c.slug.as_str(), c.bucket.as_str(), c.last_head.as_str()), ("a", "w-a", "head-1"));
    assert!(!c.due && c.due_in_ms == 1 && c.objects > 0 && c.bytes > 0);
    let old = plan_orphans(&b, PR, t0 + 14 * D, &pol()).unwrap();
    assert!(old[0].due && old[0].due_in_ms == 0, "due exactly at the threshold");
    assert!(b.mutated().is_empty(), "planning writes nothing");
    assert_eq!(before, [PR, "w-a", "w-other"].iter().map(|x| snapshot(&b, x)).collect::<Vec<_>>());
}

fn retire(b: &Spy, now: u64, dropped: &Cell<u32>) -> Result<Retired, OrphanError> {
    let (prefix, bucket) = arch(now, "w-a");
    retire_orphan(b, PR, "a", now, &pol(), &ArchiveTarget { bucket, prefix: &prefix }, &|bk| {
        assert_eq!(bk, "w-a");
        dropped.set(dropped.get() + 1);
        assert!(b.inner.list("w-a", "").unwrap().is_empty(), "the bucket is empty when it is dropped");
        Ok(())
    })
}

#[test]
fn confirmed_gc_archives_everything_then_deletes_and_nothing_else_moves() {
    let (b, t0) = world();
    let dropped = Cell::new(0);
    assert!(matches!(retire(&b, t0 + 14 * D - 1, &dropped), Err(OrphanError::Refused(Refusal::NotDue { .. }))));
    assert_eq!(dropped.get(), 0);
    let wt_before = snapshot(&b, "w-a");
    let other_before = snapshot(&b, "w-other");
    let proj_before: Vec<_> = snapshot(&b, PR).into_iter().filter(|(k, _)| k != "worktrees/a.json" && k != DIGEST_KEY).collect();
    let now = t0 + 14 * D;
    b.clear();
    let r = retire(&b, now, &dropped).unwrap();
    assert_eq!((r.slug.as_str(), r.archived as usize), ("a", wt_before.len()));
    assert_eq!(dropped.get(), 1);
    // Every key of the bucket is in the archive, `.wt/*` and the overlay object included, same bytes.
    for (k, _) in &wt_before {
        assert!(b.inner.head(ARCH, &format!("w-a@{now}/{k}")).unwrap().is_some(), "archived {k}");
    }
    assert!(wt_before.iter().any(|(k, _)| k.starts_with(OBJ_PREFIX)) && wt_before.iter().any(|(k, _)| k == "scratch/notes.txt"));
    // self-describing (lane D3): the archived manifest resolves every object it names under the archive prefix
    let am: TreeManifest = serde_json::from_slice(&b.inner.get(ARCH, &format!("w-a@{now}/{MANIFEST_KEY}")).unwrap().unwrap()).unwrap();
    for f in &am.files {
        let d = b.inner.get(ARCH, &format!("w-a@{now}/{}", am.object_key(f))).unwrap().expect("archived object");
        assert_eq!(sha256_hex(&d), f.sha256, "{}", f.path);
    }
    assert!(snapshot(&b, "w-a").is_empty(), "objects deleted");
    assert_eq!(entry_state(&b, "a"), None, "registry entry removed");
    assert!(!read_digest(&b, PR).unwrap().unwrap().entries.contains_key("a"));
    assert_eq!(snapshot(&b, "w-other"), other_before, "the other worktree is byte-identical");
    let proj_after: Vec<_> = snapshot(&b, PR).into_iter().filter(|(k, _)| k != "worktrees/a.json" && k != DIGEST_KEY && k != "worktrees/other.json").collect();
    let proj_before: Vec<_> = proj_before.into_iter().filter(|(k, _)| k != "worktrees/other.json").collect();
    assert_eq!(proj_after, proj_before, "project keys unchanged");
    assert_eq!(entry_state(&b, "other"), Some(State::Open));
}

#[test]
fn the_spy_sees_exactly_the_worktree_keys_its_entry_and_the_digest_change() {
    let (b, t0) = world();
    let wt_keys: BTreeSet<_> = snapshot(&b, "w-a").into_iter().map(|(k, _)| k).collect();
    let now = t0 + 15 * D;
    b.clear();
    retire(&b, now, &Cell::new(0)).unwrap();
    let mut want: BTreeSet<(String, String)> = BTreeSet::new();
    for k in &wt_keys {
        want.insert(("w-a".into(), k.clone())); // deleted
        want.insert((ARCH.into(), format!("w-a@{now}/{k}"))); // archived
    }
    want.insert((PR.into(), "worktrees/a.json".into()));
    want.insert((PR.into(), DIGEST_KEY.into()));
    assert_eq!(b.mutated(), want, "mutations are exactly: the worktree's keys, their archive copies, its entry, the digest");
    let touched = b.buckets_touched();
    assert!(!touched.contains("w-other"), "the other worktree's bucket is never touched: {touched:?}");
    // On the project bucket only the registry area is read or written, nothing else.
    for (op, bk, k) in b.log.borrow().iter().filter(|(_, bk, _)| bk == PR) {
        assert!(k.starts_with("worktrees"), "{op} {bk}/{k} is outside the registry area");
    }
}

#[test]
fn a_signed_entrys_signature_object_goes_with_it_and_only_it() {
    let (b, t0) = world();
    b.put(PR, "worktrees.sig/a.json", b"sig-a").unwrap();
    b.put(PR, "worktrees.sig/other.json", b"sig-other").unwrap();
    b.clear();
    retire(&b, t0 + 14 * D, &Cell::new(0)).unwrap();
    assert!(b.inner.head(PR, "worktrees.sig/a.json").unwrap().is_none());
    assert!(b.inner.head(PR, "worktrees.sig/other.json").unwrap().is_some());
    assert!(b.mutated().contains(&(PR.to_string(), "worktrees.sig/a.json".to_string())));
}

#[test]
fn a_failed_archive_copy_deletes_nothing_and_a_rerun_finishes() {
    let (b, t0) = world();
    let before = snapshot(&b, "w-a");
    *b.fail_copy_of.borrow_mut() = Some("scratch/notes.txt".into());
    let dropped = Cell::new(0);
    let now = t0 + 14 * D;
    assert!(matches!(retire(&b, now, &dropped), Err(OrphanError::Store(_))));
    assert_eq!(snapshot(&b, "w-a"), before, "nothing deleted when an archive copy fails");
    assert_eq!((dropped.get(), entry_state(&b, "a")), (0, Some(State::Orphan)));
    *b.fail_copy_of.borrow_mut() = None;
    retire(&b, now + 1, &dropped).unwrap();
    assert_eq!((dropped.get(), entry_state(&b, "a")), (1, None));
}

#[test]
fn retire_re_checks_at_the_moment_of_deleting() {
    let (b, t0) = world();
    let now = t0 + 20 * D;
    // The worktree came back and republished: it is open, no longer an orphan. Refused.
    publish(&b, "a", t0 + D);
    assert!(matches!(retire(&b, now, &Cell::new(0)), Err(OrphanError::Refused(Refusal::NotOrphan))));
    assert!(!snapshot(&b, "w-a").is_empty());
    // An orphan entry whose bucket was claimed in the meantime is refused too.
    mark(&b, "a", t0 + 3 * D, t0 + D).unwrap();
    mark_handoff(&b, "w-a", &actor(), "ready", t0 + 3 * D).unwrap();
    assert!(matches!(retire(&b, t0 + 3 * D + 14 * D, &Cell::new(0)), Err(OrphanError::Refused(Refusal::Landed))));
    assert!(!snapshot(&b, "w-a").is_empty());
}
