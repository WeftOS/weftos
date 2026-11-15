//! WO-069 lane F: the follower manifest cache. A spy backend counts every call, so each test says
//! exactly how many reads a cold load, a warm hit, a TTL revalidation and an invalidation cost.
use std::collections::BTreeMap;
use weftos_store::FsBackend;
use weftos_store::backend::AdminBackend;
use weftos_store::follower::*;
use weftos_store::follower_browse::{browse_get, browse_ls, browse_registry};
use weftos_store::follower_cache::*;
use weftos_store::overlay::ObjectBackend;
use weftos_store::promote::{WorktreeMeta, write_meta};
use weftos_store::share::*;
use weftos_store::share_fetch::*;
use weftos_store::testing::{Counts, CountingBackend, MemBackend};

const PR: &str = "p-01abc";
type Spy = CountingBackend<MemBackend>;

fn spy() -> Spy {
    CountingBackend::new(MemBackend::new())
}

/// Publish (or republish) `slug` with `files` at `head`, and register it.
fn publish<B: ObjectBackend>(b: &B, slug: &str, files: &[(&str, &[u8])], head: &str) {
    let d = tempfile::tempdir().unwrap();
    let mut ps = vec![];
    for (p, c) in files {
        let f = d.path().join(p);
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(f, c).unwrap();
        ps.push(p.to_string());
    }
    let bucket = format!("w-{slug}");
    let src = TreeSource {
        root: d.path(), paths: &ps, uncommitted: &[], changes: &[], changes_recorded: false, branch: slug, base_commit: "base",
        head_commit: head, bundle: None, writer: Actor { agent: "w".into(), host: "h".into() },
    };
    let meta = WorktreeMeta { slug: slug.into(), project: "01abc".into(), branch: slug.into(), base_commit: "base".into(), created_ms: 0 };
    write_meta(b, &bucket, &meta).unwrap();
    publish_tree(b, &bucket, &src, &Limits::default(), 5_000).unwrap();
    let e = summarize(b, slug, &bucket).unwrap().unwrap();
    write_registry_entry(b, PR, &e).unwrap();
}

fn delta(b: &Spy, f: impl FnOnce()) -> Counts {
    b.reset();
    f();
    b.counts()
}

fn two(b: &Spy) {
    publish(b, "alpha", &[("docs/a.md", b"# A\n"), ("src/lib.rs", b"fn a(){}")], "h1");
    publish(b, "beta", &[("docs/b.md", b"# B\n")], "h1");
}

#[test]
fn cold_does_the_full_load_and_warm_within_ttl_reads_nothing() {
    let b = spy();
    two(&b);
    let mut c = ManifestCache::new(1_000, 8, 1 << 20);
    let cold = delta(&b, || { browse_ls_cached(&b, &mut c, 0, PR, "alpha", "").unwrap(); });
    // entry head+get, meta.json get, manifest head+get
    assert_eq!((cold.get, cold.head, cold.list), (3, 2, 0), "{cold:?}");
    let warm = delta(&b, || {
        for t in [1, 500, 999] {
            browse_ls_cached(&b, &mut c, t, PR, "alpha", "docs").unwrap();
        }
    });
    assert_eq!(warm, Counts::default(), "warm browse within the TTL reads nothing");
    assert_eq!((c.stats.cold, c.stats.fresh), (1, 3));
    // identical answers to the uncached reader
    assert_eq!(browse_ls_cached(&b, &mut c, 2, PR, "alpha", "").unwrap(), browse_ls(&b, PR, "alpha", "").unwrap());
}

#[test]
fn get_reads_the_object_live_but_not_the_manifest() {
    let b = spy();
    two(&b);
    let mut c = ManifestCache::default();
    browse_ls_cached(&b, &mut c, 0, PR, "alpha", "").unwrap();
    let d = delta(&b, || {
        let f = browse_get_cached(&b, &mut c, 10, PR, "alpha", "docs/a.md", 1 << 20).unwrap();
        assert_eq!(f.data.as_deref(), Some(&b"# A\n"[..]));
    });
    assert_eq!((d.head, d.get, d.list), (1, 1, 0), "only the tree object: {d:?}");
}

#[test]
fn ttl_expiry_costs_one_entry_head_and_one_manifest_head_and_restarts_the_age() {
    let b = spy();
    two(&b);
    let mut c = ManifestCache::new(1_000, 8, 1 << 20);
    browse_ls_cached(&b, &mut c, 0, PR, "alpha", "").unwrap();
    let d = delta(&b, || { browse_ls_cached(&b, &mut c, 1_000, PR, "alpha", "").unwrap(); });
    // WO-069 X2: the entry is re-checked too (it was not, so an entry-only change had no bound)
    assert_eq!(d, Counts { head: 2, ..Counts::default() });
    assert_eq!(c.stats.revalidated, 1);
    // the age restarted at 1_000, so 1_999 is fresh again
    let d = delta(&b, || { browse_ls_cached(&b, &mut c, 1_999, PR, "alpha", "").unwrap(); });
    assert_eq!(d, Counts::default());
}

#[test]
fn a_changed_digest_etag_drops_every_slot_of_the_project() {
    let b = spy();
    two(&b);
    let mut c = ManifestCache::new(60_000, 8, 1 << 20);
    for s in ["alpha", "beta"] {
        browse_ls_cached(&b, &mut c, 0, PR, s, "").unwrap();
    }
    assert!(!c.invalidate_if_digest_changed(PR, "d1"), "first observation only records");
    assert!(!c.invalidate_if_digest_changed(PR, "d1"), "same etag keeps everything");
    assert_eq!(c.len(), 2);
    assert!(!c.invalidate_if_digest_changed("p-other", "x"));
    assert!(c.invalidate_if_digest_changed(PR, "d2"));
    assert!(c.is_empty() && c.bytes() == 0);
    let d = delta(&b, || { browse_ls_cached(&b, &mut c, 1, PR, "alpha", "").unwrap(); });
    assert_eq!((d.get, d.head), (3, 2), "cold again, with the meta cross-check: {d:?}");
}

#[test]
fn a_moved_registry_entry_etag_drops_only_that_slug() {
    let b = spy();
    two(&b);
    let mut c = ManifestCache::new(60_000, 8, 1 << 20);
    for s in ["alpha", "beta"] {
        browse_ls_cached(&b, &mut c, 0, PR, s, "").unwrap();
    }
    let same = registry_listing(&b, PR).unwrap();
    assert_eq!(c.observe_registry(PR, &same), 0, "nothing moved");
    publish(&b, "alpha", &[("docs/a.md", b"# A2\n")], "h2");
    let now = registry_listing(&b, PR).unwrap();
    assert_eq!(c.observe_registry(PR, &now), 1);
    // beta still warm, alpha cold and now shows the new tree
    assert_eq!(delta(&b, || { browse_ls_cached(&b, &mut c, 1, PR, "beta", "").unwrap(); }), Counts::default());
    let l = browse_ls_cached(&b, &mut c, 1, PR, "alpha", "").unwrap();
    assert_eq!((l.head.as_str(), l.file_count), ("h2", 1));
    // a slug that left the registry is dropped too
    let gone: BTreeMap<_, _> = now.into_iter().filter(|(s, _)| s != "beta").collect();
    assert_eq!(c.observe_registry(PR, &gone), 1);
}

#[test]
fn poll_fast_feeds_the_cache() {
    // poll_fast reads from threads, so this one needs a Sync backend: S1's directory backend.
    let dir = tempfile::tempdir().unwrap();
    let b = CountingBackend::new(FsBackend::open(dir.path()).unwrap());
    for bk in [PR, "w-alpha", "w-beta"] {
        AdminBackend::create_bucket(&b.inner, bk, &[]).unwrap();
    }
    publish(&b, "alpha", &[("docs/a.md", b"# A\n")], "h1");
    publish(&b, "beta", &[("docs/b.md", b"# B\n")], "h1");
    let mut c = ManifestCache::new(60_000, 8, 1 << 20);
    let mut st = FollowState::default();
    poll_fast_cached(&b, PR, &mut st, false, false, 2, Some(&mut c)).unwrap();
    browse_ls_cached(&b, &mut c, 0, PR, "alpha", "").unwrap();
    poll_fast_cached(&b, PR, &mut st, false, false, 2, Some(&mut c)).unwrap();
    assert_eq!(c.len(), 1, "nothing moved");
    publish(&b, "alpha", &[("docs/a.md", b"# A2\n")], "h2");
    let ch = poll_fast_cached(&b, PR, &mut st, false, false, 2, Some(&mut c)).unwrap();
    assert!(matches!(ch.as_slice(), [Change::HeadMoved { .. }]));
    assert!(c.is_empty(), "the poll saw alpha's entry move");
}

#[test]
fn eviction_by_max_entries_drops_the_oldest() {
    let b = spy();
    for s in ["one", "two", "three"] {
        publish(&b, s, &[("a.md", b"# t\n")], "h1");
    }
    let mut c = ManifestCache::new(60_000, 2, 1 << 20);
    for (t, s) in [(1, "one"), (2, "two"), (3, "three")] {
        browse_ls_cached(&b, &mut c, t, PR, s, "").unwrap();
    }
    assert_eq!((c.len(), c.stats.evicted), (2, 1));
    assert_eq!(delta(&b, || { browse_ls_cached(&b, &mut c, 4, PR, "three", "").unwrap(); }), Counts::default());
    assert_eq!(delta(&b, || { browse_ls_cached(&b, &mut c, 4, PR, "two", "").unwrap(); }), Counts::default());
    assert!(delta(&b, || { browse_ls_cached(&b, &mut c, 5, PR, "one", "").unwrap(); }).get > 0, "oldest was evicted");
}

#[test]
fn eviction_by_max_bytes_and_an_oversize_manifest_is_served_not_stored() {
    let b = spy();
    publish(&b, "one", &[("a.md", b"# t\n")], "h1");
    publish(&b, "two", &[("a.md", b"# t\n")], "h1");
    let size = b.inner.get("w-one", MANIFEST_KEY).unwrap().unwrap().len() as u64;
    // room for one manifest but not two
    let mut c = ManifestCache::new(60_000, 8, size + size / 2);
    browse_ls_cached(&b, &mut c, 1, PR, "one", "").unwrap();
    assert_eq!(c.bytes(), size);
    browse_ls_cached(&b, &mut c, 2, PR, "two", "").unwrap();
    assert_eq!((c.len(), c.stats.evicted), (1, 1));
    assert!(c.bytes() <= size + size / 2);
    // smaller than one manifest: still answers, stores nothing
    let mut tiny = ManifestCache::new(60_000, 8, size - 1);
    assert_eq!(browse_ls_cached(&b, &mut tiny, 1, PR, "one", "").unwrap().entries.len(), 1);
    assert!(tiny.is_empty() && tiny.bytes() == 0);
}

#[test]
fn a_republish_is_never_served_stale_past_the_ttl() {
    let b = spy();
    two(&b);
    let mut c = ManifestCache::new(1_000, 8, 1 << 20);
    assert_eq!(browse_ls_cached(&b, &mut c, 0, PR, "alpha", "docs").unwrap().entries[0].title.as_deref(), Some("A"));
    publish(&b, "alpha", &[("docs/a.md", b"# A2\n"), ("docs/new.md", b"# N\n")], "h2");
    // inside the TTL the cached listing is still the old one (that is the whole contract)...
    assert_eq!(browse_ls_cached(&b, &mut c, 999, PR, "alpha", "docs").unwrap().entries.len(), 1);
    // ...at the TTL one head sees the manifest etag moved and the next read is a full cold load
    let d = delta(&b, || {
        let l = browse_ls_cached(&b, &mut c, 1_000, PR, "alpha", "docs").unwrap();
        assert_eq!((l.entries.len(), l.head.as_str()), (2, "h2"));
    });
    assert!(d.head >= 1 && d.get >= 3, "head, then entry + meta + manifest again: {d:?}");
    assert_eq!(c.stats.revalidated, 0);
}

/// Lane D3 flipped this (owner decision 2026-11-15, "accept up to one cache TTL of previous-version
/// bytes"): objects are immutable and content-named, so inside the TTL a get after a republish is
/// served from the object the CACHED manifest names, the previous version, consistent with the
/// cached listing. Before, that object had been overwritten, the checksum failed, and the slot was
/// dropped for a cold retry that returned the new bytes.
#[test]
fn a_get_inside_the_ttl_after_a_republish_returns_the_previous_bytes_consistent_with_the_listing() {
    let b = spy();
    two(&b);
    let mut c = ManifestCache::new(60_000, 8, 1 << 20);
    let before = browse_ls_cached(&b, &mut c, 0, PR, "alpha", "").unwrap();
    let old = browse_get_cached(&b, &mut c, 1, PR, "alpha", "docs/a.md", 1 << 20).unwrap();
    publish(&b, "alpha", &[("docs/a.md", b"# A2 changed\n"), ("src/lib.rs", b"fn a(){}")], "h2");
    let f = browse_get_cached(&b, &mut c, 5, PR, "alpha", "docs/a.md", 1 << 20).unwrap();
    assert_eq!((f.data.as_deref(), &f.sha256), (old.data.as_deref(), &old.sha256), "the previous version, as the cached manifest says");
    assert_ne!(f.data.as_deref(), Some(&b"# A2 changed\n"[..]));
    assert_eq!(browse_ls_cached(&b, &mut c, 5, PR, "alpha", "").unwrap(), before, "and the listing it agrees with");
    // a file that left the tree is the same story: still served from the cached snapshot (its
    // object is retired, kept for the grace), an error only once it is really gone
    publish(&b, "alpha", &[("docs/a.md", b"# A2 changed\n")], "h3");
    let f = browse_get_cached(&b, &mut c, 6, PR, "alpha", "src/lib.rs", 1 << 20).unwrap();
    assert_eq!(f.data.as_deref(), Some(&b"fn a(){}"[..]));
    // past the TTL the new manifest is loaded and its bytes served
    let f = browse_get_cached(&b, &mut c, 60_000, PR, "alpha", "docs/a.md", 1 << 20).unwrap();
    assert_eq!(f.data.as_deref(), Some(&b"# A2 changed\n"[..]));
}

#[test]
fn the_meta_cross_check_runs_on_every_cold_load_and_a_failure_is_not_cached() {
    let b = spy();
    two(&b);
    let mut c = ManifestCache::new(60_000, 8, 1 << 20);
    browse_ls_cached(&b, &mut c, 0, PR, "alpha", "").unwrap();
    // tamper with the bucket's metadata (it names another slug now); inside the TTL the slot is
    // served (no read at all), after an invalidation the cold load refuses it
    let bad = WorktreeMeta { slug: "beta".into(), project: "01abc".into(), branch: "x".into(), base_commit: "b".into(), created_ms: 0 };
    write_meta(&b, "w-alpha", &bad).unwrap();
    assert!(browse_ls_cached(&b, &mut c, 1, PR, "alpha", "").is_ok());
    assert!(c.invalidate(PR, "alpha"));
    assert!(browse_ls_cached(&b, &mut c, 2, PR, "alpha", "").is_err());
    assert!(c.is_empty(), "a refused load stores nothing");
    assert!(browse_ls_cached(&b, &mut c, 3, PR, "ghost", "").is_err());
    assert!(browse_ls_cached(&b, &mut c, 3, PR, "../x", "").is_err());
}

#[test]
fn registry_from_the_digest_matches_the_listing_and_reads_one_object_when_warm() {
    let b = spy();
    two(&b);
    write_digest(&b, PR).unwrap();
    let mut c = ManifestCache::default();
    let cold = browse_registry_cached(&b, &mut c, PR).unwrap();
    assert_eq!(cold, browse_registry(&b, PR).unwrap());
    let warm = delta(&b, || { assert_eq!(browse_registry_cached(&b, &mut c, PR).unwrap(), cold); });
    assert_eq!(warm, Counts { get: 1, bytes_read: warm.bytes_read, ..Counts::default() }, "{warm:?}");
    // one entry moves: the digest is rewritten, one entry is re-read
    publish(&b, "beta", &[("docs/b.md", b"# B2\n")], "h2");
    write_digest(&b, PR).unwrap();
    let d = delta(&b, || { assert_eq!(browse_registry_cached(&b, &mut c, PR).unwrap().worktrees.len(), 2); });
    assert_eq!(d.get, 2, "digest + the one moved entry: {d:?}");
    // no digest: falls back to the listing
    let b2 = spy();
    two(&b2);
    let mut c2 = ManifestCache::default();
    assert_eq!(browse_registry_cached(&b2, &mut c2, PR).unwrap(), browse_registry(&b2, PR).unwrap());
}

#[test]
fn cached_browse_equals_uncached_on_every_path() {
    let b = spy();
    two(&b);
    let mut c = ManifestCache::default();
    for p in ["", "docs", "src", "nope", ".wt", "../x", "docs/a.md"] {
        let (x, y) = (browse_ls(&b, PR, "alpha", p), browse_ls_cached(&b, &mut c, 1, PR, "alpha", p));
        assert_eq!(x.is_ok(), y.is_ok(), "ls {p}");
        if let (Ok(x), Ok(y)) = (x, y) { assert_eq!(x, y); }
    }
    for p in ["docs/a.md", "src/lib.rs", "docs", ".wt/meta.json", "missing"] {
        let (x, y) = (browse_get(&b, PR, "alpha", p, 100), browse_get_cached(&b, &mut c, 1, PR, "alpha", p, 100));
        assert_eq!(x.is_ok(), y.is_ok(), "get {p}");
        if let (Ok(x), Ok(y)) = (x, y) { assert_eq!(x, y); }
    }
}
