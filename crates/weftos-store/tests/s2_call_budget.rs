//! WO-069 lane T: exact backend-call budgets of the S2 follower verbs, as the library makes them.
//! Each number is a formula of N files, W worktrees and D docs, with the calls named. These tests
//! pin what the code does today; a lane that removes a call must update the formula, not loosen it.
//! Order of the six kinds everywhere: `[get, head, list, put, put_cas, delete, copy]`.
mod s2_budget_common;
use s2_budget_common::*;
use weftos_store::follower::*;
use weftos_store::follower_browse::{browse_get, browse_ls, browse_registry};
use weftos_store::follower_cache::*;
use weftos_store::follower_cache_shared::{CacheCfg, SharedCache};
use weftos_store::follower_registry_cache::{RegistryCache, list_shared_cached};
use weftos_store::share_fetch::*;

fn rig() -> Spy {
    let b = spy();
    project(&b, W);
    b
}

#[test]
fn follow_digest_hit_first_poll_and_full() {
    let b = rig();
    let mut st = FollowState::default();
    // first poll, digest present: digest GET + one entry GET per worktree
    let (r, c) = delta(&b, || poll_fast(&b, PR, &mut st, false, true, 8).unwrap());
    assert_eq!(r.len() as u64, W);
    assert_eq!(c, [1 + W, 0, 0, 0, 0, 0, 0], "digest GET + W entry GET");
    // nothing changed, digest hit: one GET, no listing, no entry reads
    let (r, c) = delta(&b, || poll_fast(&b, PR, &mut st, false, true, 8).unwrap());
    assert!(r.is_empty());
    assert_eq!(c, [1, 0, 0, 0, 0, 0, 0], "digest GET");
    // --full (no digest), nothing changed: one listing, etags match, no GET
    let (r, c) = delta(&b, || poll_fast(&b, PR, &mut st, false, false, 8).unwrap());
    assert!(r.is_empty());
    assert_eq!(c, [0, 0, 1, 0, 0, 0, 0], "one registry LIST");
    // --full from an empty state: the listing plus every entry
    let mut fresh = FollowState::default();
    let (_, c) = delta(&b, || poll_fast(&b, PR, &mut fresh, false, false, 8).unwrap());
    assert_eq!(c, [W, 0, 1, 0, 0, 0, 0], "LIST + W entry GET");
    // one entry moved (a republish): digest GET + that one entry
    let s = src(N, D, "t0");
    publish(&b, &slug(0), &s, "h0b", 6_000);
    refresh(&b, &slug(0));
    let (r, c) = delta(&b, || poll_fast(&b, PR, &mut st, false, true, 8).unwrap());
    assert_eq!(r.len(), 1);
    assert_eq!(c, [1 + 1, 0, 0, 0, 0, 0, 0], "digest GET + C=1 entry GET");
}

#[test]
fn leftovers_and_registry_listing() {
    let b = rig();
    // `leftovers` reads the registry (list_shared_lenient) and computes locally
    let (e, c) = delta(&b, || list_shared_lenient(&b, PR).unwrap());
    assert_eq!(e.0.len() as u64, W);
    assert_eq!(c, [W, 0, 1, 0, 0, 0, 0], "registry LIST + W entry GET");
}

#[test]
fn search_reads_every_unmerged_worktree() {
    let b = rig();
    let (hits, c) = delta(&b, || {
        let (entries, _) = list_shared_lenient(&b, PR).unwrap();
        search_docs(&b, &entries, "doc")
    });
    assert_eq!(hits.hits.len() as u64, W * D);
    // LIST + W entry GET, then per worktree: meta GET (verify_entry) + manifest HEAD + manifest GET
    assert_eq!(c, [W + W + W, W, 1, 0, 0, 0, 0], "1 LIST; GET: W entry + W meta + W manifest; HEAD: W manifest");
}

#[test]
fn sync_docs_first_and_repeat() {
    let b = rig();
    let e = entry(&b, &slug(0));
    let cache = tempfile::tempdir().unwrap();
    let (r, c) = delta(&b, || sync_docs(&b, &e, cache.path()).unwrap());
    assert_eq!(r.written.len() as u64, D);
    // meta GET; manifest read in sync_docs (HEAD + GET) and again in fetch_tree (HEAD + GET);
    // then per doc: object HEAD + object GET
    assert_eq!(c, [1 + 2 + D, 2 + D, 0, 0, 0, 0, 0], "meta GET, 2 manifest reads, D x (HEAD + GET)");
    let (r, c) = delta(&b, || sync_docs(&b, &e, cache.path()).unwrap());
    assert_eq!(r.unchanged.len() as u64, D);
    assert_eq!(c, [1 + 2, 2, 0, 0, 0, 0, 0], "meta GET + 2 manifest reads; local sha equal: no object calls");
}

#[test]
fn browse_registry_ls_get_uncached() {
    let b = rig();
    let (r, c) = delta(&b, || browse_registry(&b, PR).unwrap());
    assert_eq!(r.worktrees.len() as u64, W);
    assert_eq!(c, [W, 0, 1, 0, 0, 0, 0], "LIST + W entry GET");
    let (_, c) = delta(&b, || browse_ls(&b, PR, &slug(0), "").unwrap());
    assert_eq!(c, [3, 1, 0, 0, 0, 0, 0], "entry GET, meta GET, manifest HEAD + GET");
    let (f, c) = delta(&b, || browse_get(&b, PR, &slug(0), "docs/d0.md", 1 << 20).unwrap());
    assert!(f.data.is_some());
    assert_eq!(c, [4, 2, 0, 0, 0, 0, 0], "open (entry GET, meta GET, manifest HEAD + GET) + object HEAD + GET");
}

#[test]
fn browse_cached_cold_warm_revalidate_and_registry() {
    let b = rig();
    let mut cache = ManifestCache::new(1_000, 8, 1 << 20);
    let (_, c) = delta(&b, || browse_ls_cached(&b, &mut cache, 0, PR, &slug(0), "").unwrap());
    assert_eq!(c, [3, 2, 0, 0, 0, 0, 0], "cold: entry HEAD + GET, meta GET, manifest HEAD + GET");
    let (_, c) = delta(&b, || browse_ls_cached(&b, &mut cache, 500, PR, &slug(0), "docs").unwrap());
    assert_eq!(c, [0, 0, 0, 0, 0, 0, 0], "warm inside the TTL");
    let (_, c) = delta(&b, || browse_ls_cached(&b, &mut cache, 1_000, PR, &slug(0), "").unwrap());
    // WO-069 X2: was [0, 1] (manifest HEAD only, so a claim or merge, which change the entry and not
    // the manifest, was never seen); the entry HEAD is the one revalidation call added
    assert_eq!(c, [0, 2, 0, 0, 0, 0, 0], "TTL lapsed: entry HEAD + manifest HEAD");
    let (f, c) = delta(&b, || browse_get_cached(&b, &mut cache, 1_001, PR, &slug(0), "docs/d0.md", 1 << 20).unwrap());
    assert!(f.data.is_some());
    assert_eq!(c, [1, 1, 0, 0, 0, 0, 0], "warm manifest; object HEAD + GET only");
    // registry from the digest: digest GET + W entry GET cold, digest GET alone once the etags are cached
    let (_, c) = delta(&b, || browse_registry_cached(&b, &mut cache, PR).unwrap());
    assert_eq!(c, [1 + W, 0, 0, 0, 0, 0, 0], "digest GET + W entry GET");
    let (_, c) = delta(&b, || browse_registry_cached(&b, &mut cache, PR).unwrap());
    assert_eq!(c, [1, 0, 0, 0, 0, 0, 0], "digest GET");
}

#[test]
fn overlaps_one_pair() {
    let b = rig();
    let (a, d) = (entry(&b, &slug(0)), entry(&b, &slug(1)));
    let (r, c) = delta(&b, || overlaps(&b, &a, &d).unwrap());
    assert!(!r.overlaps.is_empty());
    // per side: meta GET (verify_entry); then per side manifest HEAD + GET
    assert_eq!(c, [2 + 2, 2, 0, 0, 0, 0, 0], "2 meta GET + 2 manifest reads");
}

/// WO-069 X2: the verbs as the daemon runs them through its shared cache. Cold, warm (inside the
/// TTL) and TTL lapsed. Uncached budgets for comparison are the tests above.
#[test]
fn shared_cache_registry_search_browse_overlaps() {
    let b = rig();
    let c = SharedCache::new(CacheCfg { ttl_ms: 1_000, max_age_ms: 60_000, max_bytes: 1 << 20, max_entries: 8 });
    // registry (leftovers, browse list, orphan display plan): uncached LIST + W GET every call
    let (_, k) = delta(&b, || c.registry(&b, PR).unwrap());
    assert_eq!(k, [W, 0, 1, 0, 0, 0, 0], "cold: LIST + W entry GET (as uncached)");
    let (_, k) = delta(&b, || c.registry(&b, PR).unwrap());
    assert_eq!(k, [0, 0, 1, 0, 0, 0, 0], "warm: the LIST alone, every etag unchanged");
    let mut rc = RegistryCache::default();
    let (_, k) = delta(&b, || list_shared_cached(&b, &mut rc, PR).unwrap());
    assert_eq!(k, [W, 0, 1, 0, 0, 0, 0], "library form, cold");
    // search: uncached [3W, W, 1] every call
    let (r, k) = delta(&b, || c.search(&b, PR, "doc", 0).unwrap());
    assert_eq!(r.0.hits.len() as u64, W * D);
    assert_eq!(k, [2 * W, W, 1, 0, 0, 0, 0], "registry warm: LIST; per worktree meta GET + manifest HEAD + GET");
    let (_, k) = delta(&b, || c.search(&b, PR, "doc", 500).unwrap());
    assert_eq!(k, [0, 0, 1, 0, 0, 0, 0], "warm: the LIST alone");
    let (_, k) = delta(&b, || c.search(&b, PR, "doc", 1_000).unwrap());
    assert_eq!(k, [0, W, 1, 0, 0, 0, 0], "TTL lapsed: LIST + W manifest HEAD (entry etags come from the LIST)");
    // browse ls / get by slug (no listing): uncached [3, 1] and [4, 2]
    let c = SharedCache::new(CacheCfg { ttl_ms: 1_000, max_age_ms: 60_000, max_bytes: 1 << 20, max_entries: 8 });
    let (_, k) = delta(&b, || c.browse_ls(&b, PR, &slug(0), "", 0).unwrap());
    assert_eq!(k, [3, 2, 0, 0, 0, 0, 0], "cold: entry HEAD + GET, meta GET, manifest HEAD + GET (+1 HEAD on uncached)");
    let (_, k) = delta(&b, || c.browse_ls(&b, PR, &slug(0), "docs", 999).unwrap());
    assert_eq!(k, [0, 0, 0, 0, 0, 0, 0], "warm");
    let (_, k) = delta(&b, || c.browse_get(&b, PR, &slug(0), "docs/d0.md", 1 << 20, 999).unwrap());
    assert_eq!(k, [1, 1, 0, 0, 0, 0, 0], "warm get: object HEAD + GET");
    let (_, k) = delta(&b, || c.browse_ls(&b, PR, &slug(0), "", 1_000).unwrap());
    assert_eq!(k, [0, 2, 0, 0, 0, 0, 0], "TTL lapsed: entry HEAD + manifest HEAD");
    // overlaps: uncached [8, 4] in the daemon (summarize x2 + library [4, 2])
    let (_, k) = delta(&b, || c.overlaps(&b, PR, &slug(1), &slug(2), 1_000).unwrap());
    assert_eq!(k, [6, 4, 0, 0, 0, 0, 0], "cold: two cold loads");
    let (_, k) = delta(&b, || c.overlaps(&b, PR, &slug(1), &slug(2), 1_500).unwrap());
    assert_eq!(k, [0, 0, 0, 0, 0, 0, 0], "warm: no call");
    let (_, k) = delta(&b, || c.overlaps(&b, PR, &slug(1), &slug(2), 2_000).unwrap());
    assert_eq!(k, [0, 4, 0, 0, 0, 0, 0], "TTL lapsed: entry HEAD + manifest HEAD per side");
}
