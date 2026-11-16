//! WO-069 lane J2: what a reader sees late, and by how much. Every test names the bound it pins.
//! Writes "by another daemon" are direct store writes; the cache only learns of them through its own
//! validators. See ADR-126 section 4a, "What a reader can see late, and by how much".
mod s2_budget_common;
use s2_budget_common::*;
use weftos_store::follower_cache_shared::*;
use weftos_store::overlay::ObjectBackend;
use weftos_store::share::{Actor, State};
use weftos_store::share_fetch::*;

const TTL: u64 = 1_000;
const MAX_AGE: u64 = 10_000;

fn shared() -> SharedCache {
    SharedCache::new(CacheCfg { ttl_ms: TTL, max_age_ms: MAX_AGE, max_bytes: 1 << 20, max_entries: 8 })
}

fn claimed(b: &Spy, slug: &str) {
    let mut e = entry(b, slug);
    e.state = State::Claimed;
    e.merger = Some(Actor { agent: "bob".into(), host: "m2".into() });
    e.updated_ms += 1;
    write_registry_entry(b, PR, &e).unwrap();
}

fn etag_of(b: &Spy, slug: &str) -> String {
    b.head(PR, &format!("{REGISTRY_PREFIX}{slug}.json")).unwrap().unwrap().etag
}

/// A listing caller (`search`) hands `loaded` the entry and etag its own LIST just returned. A slot
/// whose entry etag differs is not what the LIST showed, so it must not be served, whatever its age:
/// the race is a cold single-slug load that installs an older slot after the LIST's drop pass.
#[test]
fn a_listing_caller_never_gets_a_slot_older_than_its_list() {
    let b = spy();
    project(&b, 1);
    let c = shared();
    c.browse_ls(&b, PR, &slug(0), "", 0).unwrap();
    claimed(&b, &slug(0)); // another writer; no listing between, so the slot is still in place
    let (e, tag) = (entry(&b, &slug(0)), etag_of(&b, &slug(0)));
    let (l, _) = c.loaded(&b, PR, &slug(0), Some((&e, &tag)), 1).unwrap();
    assert_eq!(l.entry.state, State::Claimed, "the LIST showed a moved etag: the slot must not be served");
}

fn head_of(c: &SharedCache, b: &Spy, now: u64, fresh: bool) -> (String, CacheInfo) {
    let (l, i) = c.browse_ls_info(b, PR, &slug(0), "", now, fresh).unwrap();
    (l.head, i)
}

/// Another process republishes (manifest and entry both move). Single-worktree views are late by up
/// to one TTL, say so in the reply (`cache`), and `fresh` reads the store at once.
#[test]
fn another_writers_publish_is_late_by_at_most_one_ttl_and_fresh_sees_it() {
    let b = spy();
    let s = project(&b, 1);
    let c = shared();
    let (h, i) = head_of(&c, &b, 0, false);
    assert_eq!((h.as_str(), i.source, i.age_ms), ("h0", "store", 0), "cold: read from the store");
    publish(&b, &slug(0), &s[0], "h0b", 6_000);
    refresh(&b, &slug(0));
    let (h, i) = head_of(&c, &b, TTL - 1, false);
    assert_eq!((h.as_str(), i.source, i.age_ms, i.ttl_ms), ("h0", "cache", TTL - 1, TTL), "TTL-1: the cached snapshot, labelled");
    let (h, i) = head_of(&c, &b, TTL - 1, true);
    assert_eq!((h.as_str(), i.source, i.age_ms), ("h0b", "store", 0), "fresh: the store, and the slot is refreshed");
    let (h, i) = head_of(&c, &b, TTL, false);
    assert_eq!((h.as_str(), i.source), ("h0b", "cache"), "after the refresh the slot holds the new one");
    assert_eq!(i.age_ms, 1);
}

#[test]
fn another_writers_publish_shows_at_one_ttl() {
    let b = spy();
    let s = project(&b, 1);
    let c = shared();
    head_of(&c, &b, 0, false);
    publish(&b, &slug(0), &s[0], "h0b", 6_000);
    refresh(&b, &slug(0));
    assert_eq!(head_of(&c, &b, TTL - 1, false).0, "h0");
    let (h, i) = head_of(&c, &b, TTL, false);
    assert_eq!((h.as_str(), i.source), ("h0b", "store"), "TTL: revalidated, the entry etag moved, loaded again");
}

/// A claim (entry only) seen through `browse`: same bound, same `fresh`.
#[test]
fn another_writers_claim_is_late_by_at_most_one_ttl_and_fresh_sees_it() {
    let b = spy();
    project(&b, 1);
    let c = shared();
    let st = |now, fresh| c.browse_ls_info(&b, PR, &slug(0), "", now, fresh).unwrap();
    assert_eq!(st(0, false).0.state, State::Open);
    claimed(&b, &slug(0));
    let (l, i) = st(TTL - 1, false);
    assert_eq!((l.state, i.source), (State::Open, "cache"));
    assert_eq!(st(TTL - 1, true).0.state, State::Claimed);
    let c2 = shared();
    let st2 = |now| c2.browse_ls_info(&b, PR, &slug(0), "", now, false).unwrap().0.state;
    st2(0);
    // (a second claim after c2 loaded)
    let mut e = entry(&b, &slug(0));
    e.state = State::Merged;
    e.updated_ms += 1;
    write_registry_entry(&b, PR, &e).unwrap();
    assert_eq!(st2(TTL - 1), State::Claimed, "inside the TTL");
    assert_eq!(st2(TTL), State::Merged, "at the TTL");
}

/// Listing verbs (the browse list, `search`, `leftovers`) are as of their own LIST: another writer's
/// claim and republish show at once, and a slot a LIST contradicts is dropped.
#[test]
fn listing_verbs_see_another_writers_change_at_once() {
    let b = spy();
    let s = project(&b, 2);
    let c = shared();
    head_of(&c, &b, 0, false);
    c.search(&b, PR, "d0", 0).unwrap();
    claimed(&b, &slug(0));
    publish(&b, &slug(1), &s[1], "h1b", 6_000);
    refresh(&b, &slug(1));
    let l = c.browse_registry(&b, PR).unwrap();
    assert_eq!(l.worktrees.iter().find(|w| w.slug == slug(0)).unwrap().state, State::Claimed);
    assert_eq!(l.worktrees.iter().find(|w| w.slug == slug(1)).unwrap().head, "h1b");
    let (r, _, i) = c.search_info(&b, PR, "d0", 1, false).unwrap();
    assert!(r.hits.iter().any(|h| h.slug == slug(0) && h.state == State::Claimed), "search shows the state of its LIST");
    assert_eq!(i.source, "store", "every manifest it used was reloaded: the LIST had moved their entries");
}

/// A manifest that moved without its registry entry (the daemon always writes both; a writer that
/// died between them, or a raw write, leaves this) is seen by `search` after one TTL, or at once with `fresh`.
#[test]
fn search_sees_a_manifest_only_change_after_one_ttl_or_with_fresh() {
    let b = spy();
    project(&b, 1);
    let c = shared();
    assert!(c.search(&b, PR, "d5", 0).unwrap().0.hits.is_empty());
    publish(&b, &slug(0), &src(N, 6, "t0"), "h0", 7_000); // d4 and d5 are docs now; the entry is not refreshed
    let (r, _, i) = c.search_info(&b, PR, "d5", TTL - 1, false).unwrap();
    assert!(r.hits.is_empty(), "inside the TTL: the cached doc set");
    assert_eq!((i.source, i.age_ms), ("cache", TTL - 1));
    let (r, _, i) = c.search_info(&b, PR, "d5", TTL - 1, true).unwrap();
    assert_eq!((r.hits.len(), i.source), (1, "store"), "fresh");
    let c2 = shared();
    c2.search(&b, PR, "d5", 0).unwrap();
    publish(&b, &slug(0), &src(N, 7, "t0"), "h0", 8_000);
    assert_eq!(c2.search(&b, PR, "d6", TTL - 1).unwrap().0.hits.len(), 0);
    assert_eq!(c2.search(&b, PR, "d6", TTL).unwrap().0.hits.len(), 1, "one TTL: the manifest HEAD moved");
}

/// `overlaps` is bounded like `browse`: both manifests from slots, `fresh` loads both.
#[test]
fn overlaps_is_late_by_at_most_one_ttl_and_fresh_sees_it() {
    let b = spy();
    let s = project(&b, 2);
    let c = shared();
    let n = |now, fresh| c.overlaps_info(&b, PR, &slug(0), &slug(1), now, fresh).unwrap();
    assert_eq!(n(0, false).1.source, "store");
    assert_eq!(n(1, false).1.source, "cache");
    publish(&b, &slug(1), &s[0], "h1b", 6_000); // wt1 now has wt0's content: nothing differs
    refresh(&b, &slug(1));
    let before = n(TTL - 1, false);
    assert_eq!(before.1.source, "cache");
    let fresh = n(TTL - 1, true);
    assert_eq!(fresh.1.source, "store");
    assert!(!before.0.overlaps.is_empty(), "the cached pair still differs");
    assert!(fresh.0.overlaps.is_empty(), "fresh: the two trees are identical now");
    assert!(n(TTL, false).0.overlaps.is_empty() && n(TTL, false).1.source == "cache", "the refreshed slots");
}

/// An entry deleted by another process: the slot answers inside the TTL (the bound), `fresh` and the
/// TTL both find it gone, and the miss is not cached (a re-created entry shows at once).
#[test]
fn a_deleted_entry_is_found_gone_at_one_ttl_or_with_fresh_and_the_miss_is_not_cached() {
    let b = spy();
    project(&b, 1);
    let c = shared();
    head_of(&c, &b, 0, false);
    let e = entry(&b, &slug(0));
    b.delete(PR, &format!("{REGISTRY_PREFIX}{}.json", slug(0))).unwrap();
    assert_eq!(head_of(&c, &b, TTL - 1, false).1.source, "cache", "inside the TTL");
    assert!(c.browse_ls_info(&b, PR, &slug(0), "", TTL - 1, true).is_err(), "fresh: gone");
    assert!(c.browse_ls_info(&b, PR, &slug(0), "", TTL - 1, false).is_err(), "a failed fresh read dropped the slot");
    write_registry_entry(&b, PR, &e).unwrap(); // re-created by another writer
    assert_eq!(head_of(&c, &b, TTL - 1, false).0, "h0", "no negative result was cached");
    // and with no fresh: the TTL finds it
    let c2 = shared();
    head_of(&c2, &b, 0, false);
    b.delete(PR, &format!("{REGISTRY_PREFIX}{}.json", slug(0))).unwrap();
    assert!(c2.browse_ls_info(&b, PR, &slug(0), "", TTL, false).is_err());
}

/// `fresh` costs what a cold load costs, refreshes the slot, and leaves the default path alone.
#[test]
fn fresh_costs_a_cold_load_and_the_default_path_is_unchanged() {
    let b = spy();
    project(&b, 1);
    let c = shared();
    head_of(&c, &b, 0, false);
    let (_, k) = delta(&b, || head_of(&c, &b, 1, false));
    assert_eq!(k, [0, 0, 0, 0, 0, 0, 0], "default, warm: no call");
    let (_, k) = delta(&b, || head_of(&c, &b, 2, true));
    assert_eq!(k, [3, 2, 0, 0, 0, 0, 0], "fresh: entry HEAD + GET, meta GET, manifest HEAD + GET (the cold row)");
    let (_, k) = delta(&b, || head_of(&c, &b, 3, false));
    assert_eq!(k, [0, 0, 0, 0, 0, 0, 0], "the slot was refreshed: warm again");
    let (_, k) = delta(&b, || head_of(&c, &b, 2 + TTL, false));
    assert_eq!(k, [0, 2, 0, 0, 0, 0, 0], "default, TTL lapsed: entry HEAD + manifest HEAD");
}

/// Whatever the sequence, an answer that disagrees with the store is labelled `cache`, is under one
/// TTL old, and the disagreement began less than one TTL ago; a slot never outlives `max_age`.
#[test]
fn a_stale_answer_is_always_labelled_and_under_one_ttl() {
    let b = spy();
    let s = project(&b, 1);
    let c = shared();
    let (mut now, mut seed, mut truth, mut changed_at, mut n) = (0u64, 0x2545F491u64, "h0".to_string(), 0u64, 0u64);
    let mut next = move || {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        seed >> 33
    };
    for _ in 0..400 {
        now += next() % (TTL + 400);
        match next() % 4 {
            0 => {
                n += 1;
                truth = format!("h{n}");
                publish(&b, &slug(0), &s[0], &truth, 6_000 + n);
                refresh(&b, &slug(0));
                changed_at = now;
            }
            1 => {
                head_of(&c, &b, now, true);
            }
            _ => {
                let (h, i) = head_of(&c, &b, now, false);
                if h != truth {
                    assert_eq!(i.source, "cache", "a stale answer must say so (now {now})");
                    assert!(i.age_ms < TTL, "age {} at {now}", i.age_ms);
                    assert!(now - changed_at < TTL, "stale {} ms after the change at {changed_at}", now - changed_at);
                }
                assert!(i.age_ms < MAX_AGE);
            }
        }
    }
}

/// A registry object written with no digest refresh (a raw write): every LIST-based verb sees it at
/// once; only the digest-driven `follow` does not, until `--full` (no digest) or the next refresh.
#[test]
fn an_entry_written_without_a_digest_refresh_shows_to_listings_not_to_the_digest_follow() {
    use weftos_store::follower::{FollowState, poll_fast};
    let b = spy();
    project(&b, 1);
    let mut st = FollowState::default();
    poll_fast(&b, PR, &mut st, true, true, 2).unwrap();
    let mut e = entry(&b, &slug(0));
    e.slug = "wt9".into();
    e.bucket = bucket("wt9");
    b.put(PR, &format!("{REGISTRY_PREFIX}wt9.json"), &serde_json::to_vec(&e).unwrap()).unwrap();
    let c = shared();
    assert!(c.browse_registry(&b, PR).unwrap().worktrees.iter().any(|w| w.slug == "wt9"), "a LIST sees it at once");
    assert!(poll_fast(&b, PR, &mut st.clone(), true, true, 2).unwrap().is_empty(), "digest-driven follow: not until the digest is refreshed");
    assert_eq!(poll_fast(&b, PR, &mut st, true, false, 2).unwrap().len(), 1, "follow --full lists: it sees it");
}

/// `leftovers` survives a damaged registry entry and names it: one that does not parse, and one
/// whose body names a different worktree than its key. Never cached: named on every call.
#[test]
fn leftovers_names_damaged_entries_and_never_caches_them() {
    use weftos_store::follower::{LeftoverPolicy, leftovers_report};
    let b = spy();
    project(&b, 1);
    let e = entry(&b, &slug(0)); // read before the damage: `entry` lists strictly
    b.put(PR, &format!("{REGISTRY_PREFIX}wt7.json"), b"{ not json").unwrap();
    b.put(PR, &format!("{REGISTRY_PREFIX}wt8.json"), &serde_json::to_vec(&e).unwrap()).unwrap(); // body says wt0
    let c = shared();
    let pol = LeftoverPolicy::default();
    for pass in 0..2 {
        let (_, k) = delta(&b, || c.leftovers(&b, PR, 1 << 40, &pol).unwrap());
        let r = c.leftovers(&b, PR, 1 << 40, &pol).unwrap();
        assert_eq!(r.unreadable, vec!["wt7".to_string(), "wt8".to_string()], "pass {pass}");
        assert_eq!(r.leftovers.len(), 1, "the readable entry is still judged");
        assert!(k[2] == 1, "one LIST, pass {pass}: {k:?}");
    }
    let r = leftovers_report(&[e], vec!["x".into()], 1 << 40, &pol);
    assert_eq!((r.leftovers.len(), r.unreadable), (1, vec!["x".to_string()]));
}
