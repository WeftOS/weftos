//! WO-069 X2: the registry-entry reuse and the shared manifest cache the daemon holds. Every test
//! names the bound it checks. Writes made "by another daemon" are direct store writes; the cache
//! only learns of them through its own validators.
mod s2_budget_common;
use s2_budget_common::*;
use std::sync::Arc;
use weftos_store::follower_cache::*;
use weftos_store::follower_cache_shared::*;
use weftos_store::follower_registry_cache::*;
use weftos_store::share::{Actor, State};
use weftos_store::share_fetch::*;

const TTL: u64 = 1_000;
const MAX_AGE: u64 = 10_000;

fn shared() -> SharedCache {
    SharedCache::new(CacheCfg { ttl_ms: TTL, max_age_ms: MAX_AGE, max_bytes: 1 << 20, max_entries: 8 })
}

/// Another writer rewrites `slug`'s registry entry (what a claim, a merge or an orphan mark does to
/// the entry; the manifest is untouched).
fn rewrite_entry(b: &Spy, slug: &str, f: impl FnOnce(&mut RegistryEntry)) {
    let mut e = entry(b, slug);
    f(&mut e);
    write_registry_entry(b, PR, &e).unwrap();
}

fn claimed(e: &mut RegistryEntry) {
    e.state = State::Claimed;
    e.merger = Some(Actor { agent: "bob".into(), host: "m2".into() });
    e.updated_ms += 1;
}

#[test]
fn an_entry_only_change_shows_within_one_ttl_with_no_manifest_change() {
    // The defect this lane fixes: revalidation used to HEAD the manifest only, so a claim was
    // served for as long as the manifest stayed the same.
    let b = spy();
    project(&b, 1);
    let c = shared();
    let state = |now| c.browse_ls(&b, PR, &slug(0), "", now).unwrap().state;
    assert_eq!(state(0), State::Open);
    rewrite_entry(&b, &slug(0), claimed);
    assert_eq!(state(TTL - 1), State::Open, "inside the TTL the slot is served (the documented bound)");
    let (s, k) = delta(&b, || state(TTL));
    assert_eq!(s, State::Claimed, "at the TTL the entry etag moved: cold load");
    assert_eq!(k, [3, 3, 0, 0, 0, 0, 0], "entry HEAD (moved), then cold: entry HEAD + GET, meta GET, manifest HEAD + GET");
    // a merge and an orphan mark are the same kind of change
    rewrite_entry(&b, &slug(0), |e| e.state = State::Merged);
    assert_eq!(state(2 * TTL), State::Merged);
    rewrite_entry(&b, &slug(0), |e| e.state = State::Orphan);
    assert_eq!(state(3 * TTL), State::Orphan);
}

#[test]
fn an_invalidation_shows_a_change_at_once_and_a_listing_drops_a_moved_slot() {
    let b = spy();
    project(&b, 2);
    let c = shared();
    c.browse_ls(&b, PR, &slug(0), "", 0).unwrap();
    rewrite_entry(&b, &slug(0), claimed);
    c.invalidate(PR, &slug(0)); // this daemon made the write
    assert_eq!(c.browse_ls(&b, PR, &slug(0), "", 1).unwrap().state, State::Claimed);
    // another writer, then a registry listing (leftovers, search, the browse list) inside the TTL
    c.browse_ls(&b, PR, &slug(1), "", 2).unwrap();
    rewrite_entry(&b, &slug(1), claimed);
    let l = c.registry(&b, PR).unwrap();
    assert!(l.entries.iter().any(|(e, _)| e.slug == slug(1) && e.state == State::Claimed), "a listing is never stale");
    assert_eq!(c.browse_ls(&b, PR, &slug(1), "", 3).unwrap().state, State::Claimed, "the listing dropped the moved slot");
}

#[test]
fn max_age_drops_a_slot_whatever_its_etags_say() {
    let b = spy();
    project(&b, 1);
    let c = shared();
    c.browse_ls(&b, PR, &slug(0), "", 0).unwrap();
    let mut now = 0;
    while now + TTL < MAX_AGE {
        now += TTL;
        let (_, k) = delta(&b, || c.browse_ls(&b, PR, &slug(0), "", now).unwrap());
        assert_eq!(k, [0, 2, 0, 0, 0, 0, 0], "revalidation at {now}: entry HEAD + manifest HEAD");
    }
    let (_, k) = delta(&b, || c.browse_ls(&b, PR, &slug(0), "", MAX_AGE).unwrap());
    assert_eq!(k, [3, 2, 0, 0, 0, 0, 0], "max_age: a full cold load although nothing changed");
    assert_eq!(c.stats().0.expired, 1);
}

#[test]
fn a_republish_inside_the_ttl_gives_the_new_bytes_on_get() {
    let b = spy();
    let s = project(&b, 1);
    let c = shared();
    let f = c.browse_get(&b, PR, &slug(0), "docs/d0.md", 1 << 20, 0).unwrap();
    assert!(String::from_utf8(f.data.unwrap()).unwrap().contains("t0"));
    std::fs::write(s[0].dir.path().join("docs/d0.md"), "# Doc 0\nrepublished\n").unwrap();
    publish(&b, &slug(0), &s[0], "h0b", 6_000);
    refresh(&b, &slug(0));
    let f = c.browse_get(&b, PR, &slug(0), "docs/d0.md", 1 << 20, 1).unwrap();
    assert_eq!(f.data.as_deref(), Some(&b"# Doc 0\nrepublished\n"[..]), "checksum mismatch dropped the slot; one cold retry");
}

#[test]
fn a_recreated_slug_never_serves_the_old_slot() {
    let b = spy();
    project(&b, 1);
    let c = shared();
    assert_eq!(c.browse_ls(&b, PR, &slug(0), "", 0).unwrap().head, "h0");
    // another daemon: archive and delete the bucket, recreate the slug (new created_ms), publish new content
    for o in weftos_store::overlay::ObjectBackend::list(&b, &bucket(slug(0).as_str()), "").unwrap() {
        weftos_store::overlay::ObjectBackend::delete(&b, &bucket(&slug(0)), &o.key).unwrap();
    }
    let m = weftos_store::promote::WorktreeMeta { slug: slug(0), project: "01abc".into(), branch: "re".into(), base_commit: "base".into(), created_ms: 77 };
    weftos_store::promote::write_meta(&b, &bucket(&slug(0)), &m).unwrap();
    publish(&b, &slug(0), &src(3, 1, "new life"), "h-new", 7_000);
    refresh(&b, &slug(0));
    let l = c.browse_ls(&b, PR, &slug(0), "", TTL).unwrap();
    assert_eq!((l.head.as_str(), l.file_count), ("h-new", 3), "first validation after the recreate: cold");
    // and with the bucket gone (no manifest), the slot is not served past the TTL either
    for o in weftos_store::overlay::ObjectBackend::list(&b, &bucket(&slug(0)), ".wt/tree").unwrap() {
        weftos_store::overlay::ObjectBackend::delete(&b, &bucket(&slug(0)), &o.key).unwrap();
    }
    weftos_store::overlay::ObjectBackend::delete(&b, &bucket(&slug(0)), weftos_store::share::MANIFEST_KEY).unwrap();
    assert!(c.browse_ls(&b, PR, &slug(0), "", 2 * TTL).is_err(), "missing manifest: dropped, reload fails, nothing cached");
    assert_eq!(c.footprint().0, 0, "a negative result is not cached");
}

#[test]
fn a_load_that_overlaps_an_invalidation_is_served_but_not_stored() {
    let b = spy();
    project(&b, 1);
    let mut m = ManifestCache::new(TTL, 8, 1 << 20);
    let Lookup::Miss(t) = m.lookup(PR, &slug(0), 0) else { panic!("empty cache") };
    let f = load_slot(&b, PR, &slug(0), None).unwrap();
    m.invalidate(PR, &slug(0)); // a write finished while the load was in flight
    m.install(PR, &slug(0), &f, t, 0);
    assert!(m.is_empty());
    assert_eq!(m.stats.discarded, 1);
}

#[test]
fn eviction_keeps_the_shared_cache_under_its_byte_cap() {
    let b = spy();
    project(&b, 3);
    let size = weftos_store::overlay::ObjectBackend::head(&b, &bucket(&slug(0)), weftos_store::share::MANIFEST_KEY).unwrap().unwrap().size;
    let c = SharedCache::new(CacheCfg { ttl_ms: TTL, max_age_ms: MAX_AGE, max_bytes: size * 2 + size / 2, max_entries: 8 });
    for i in 0..3 {
        c.browse_ls(&b, PR, &slug(i), "", i).unwrap();
        assert!(c.footprint().1 <= size * 2 + size / 2);
    }
    assert_eq!(c.footprint().0, 2);
    assert_eq!(c.stats().0.evicted, 1);
}

#[test]
fn registry_reuse_reads_only_moved_entries_and_drops_removed_ones() {
    let b = spy();
    project(&b, W);
    let mut rc = RegistryCache::default();
    let (l, k) = delta(&b, || list_shared_cached(&b, &mut rc, PR).unwrap());
    let (plain, _) = list_shared_lenient(&b, PR).unwrap();
    assert_eq!(l.clone().into_entries(), plain, "same result as list_shared_lenient");
    assert_eq!(k, [W, 0, 1, 0, 0, 0, 0], "cold: LIST + W entry GET");
    let (_, k) = delta(&b, || list_shared_cached(&b, &mut rc, PR).unwrap());
    assert_eq!(k, [0, 0, 1, 0, 0, 0, 0], "unchanged listing: no entry GET");
    rewrite_entry(&b, &slug(1), claimed);
    let (l, k) = delta(&b, || list_shared_cached(&b, &mut rc, PR).unwrap());
    assert_eq!(k, [1, 0, 1, 0, 0, 0, 0], "one etag moved: one GET");
    assert!(l.entries.iter().any(|(e, _)| e.slug == slug(1) && e.state == State::Claimed));
    weftos_store::follower::remove_registry_entry(&b, PR, &slug(2)).unwrap();
    let l = list_shared_cached(&b, &mut rc, PR).unwrap();
    assert_eq!(l.entries.len() as u64, W - 1);
    assert_eq!(rc.len() as u64, W - 1, "an entry the listing no longer has is dropped");
}

#[test]
fn a_corrupt_entry_is_reported_every_time_and_never_cached() {
    let b = spy();
    project(&b, 2);
    weftos_store::overlay::ObjectBackend::put(&b, PR, &format!("{REGISTRY_PREFIX}{}.json", slug(1)), b"{not json").unwrap();
    let mut rc = RegistryCache::default();
    for _ in 0..2 {
        let (l, k) = delta(&b, || list_shared_cached(&b, &mut rc, PR).unwrap());
        assert_eq!(l.unreadable, vec![slug(1)]);
        assert_eq!(l.entries.len(), 1);
        assert!(k[0] >= 1, "the corrupt entry is read again: {k:?}");
    }
    assert_eq!(rc.len(), 1);
}

#[test]
fn concurrent_browse_never_deadlocks_and_never_mixes_two_publishes() {
    let b = Arc::new(spy());
    let s = project(&b, 1);
    let c = Arc::new(SharedCache::new(CacheCfg { ttl_ms: 0, max_age_ms: MAX_AGE, max_bytes: 1 << 20, max_entries: 8 }));
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let readers: Vec<_> = (0..6)
        .map(|t| {
            let (b, c, stop) = (b.clone(), c.clone(), stop.clone());
            std::thread::spawn(move || {
                let b: &Spy = &b;
                let mut n = 0u64;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) || n < 20 {
                    n += 1;
                    // ttl 0: every read revalidates, so slots are replaced while others read them
                    if let Ok(l) = c.browse_ls(b, PR, &slug(0), "docs", t * 1_000 + n) {
                        // one load is one manifest: every listed doc carries that publish's title tag
                        let tags: std::collections::BTreeSet<_> = l.entries.iter().map(|e| e.size).collect();
                        assert_eq!(tags.len(), 1, "a listing mixed two publishes: {tags:?}");
                    }
                }
            })
        })
        .collect();
    for i in 0..10 {
        let body = format!("# Doc\n{}\n", "x".repeat(i + 1));
        for d in 0..D {
            std::fs::write(s[0].dir.path().join(format!("docs/d{d}.md")), &body).unwrap();
        }
        publish(&b, &slug(0), &s[0], &format!("h{i}"), 6_000 + i as u64);
        refresh(&b, &slug(0));
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    for r in readers {
        r.join().unwrap();
    }
}

#[test]
fn config_from_the_environment_shape() {
    let none = |_: &str| None;
    assert_eq!(CacheCfg::from_lookup(none), Some(CacheCfg::default()));
    let get = |m: &'static [(&'static str, &'static str)]| move |v: &str| m.iter().find(|(k, _)| *k == v).map(|(_, x)| x.to_string());
    assert_eq!(CacheCfg::from_lookup(get(&[(ENV_MB, "0")])), None, "0 MB is off");
    assert_eq!(CacheCfg::from_lookup(get(&[(ENV_MB, "lots")])), None, "a bad value is off, never a longer window");
    let c = CacheCfg::from_lookup(get(&[(ENV_MB, "8"), (ENV_TTL_MS, "90000"), (ENV_MAX_AGE_MS, "30000")])).unwrap();
    assert_eq!((c.max_bytes, c.ttl_ms, c.max_age_ms), (8 << 20, 30_000, 30_000), "the TTL is clamped to max_age");
}
