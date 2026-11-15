//! WO-069 lane H failure drills (ADR-126 section 4a gate item 5) against a REAL RustFS: 3a a publish interrupted at every stage,
//! 3b stale and future-stamped claims (wall clock and the daemon's observed expiry), 3c a registry refresh cut between the entry and
//! the digest. Same environment and runner as `s2_drills_live.rs`; prints `DRILL|`, `DETAIL|` and `FINDING|` lines.
#![cfg(feature = "rustfs")]

#[path = "s2_drills_common/mod.rs"]
mod s2_drills_common;
use s2_drills_common::*;
use std::time::Instant;
use weftos_store::claim_watch::{ClaimWatch, claim_merge_observed};
use weftos_store::follower::{FollowState, poll_fast};
use weftos_store::overlay::{ObjectBackend, S2Error};
use weftos_store::promote::{WorktreeMeta, write_meta};
use weftos_store::share::*;
use weftos_store::share_fetch::*;

#[test]
#[ignore = "needs a throwaway RustFS: WEFTOS_STORE_TEST_RUSTFS_ENV"]
fn drill_3a_publish_interrupted_midway() {
    let (be, _d) = backend();
    let me = who("writer", "m1");
    // Cut points: the Nth put fails. Changes in v2: d3 edited in place, d20..d22 new, d0 deleted.
    // v2 needs 4 object puts (d3, d20, d21, d22) then the manifest (put #5); delete #0 is the first stale object (the publish no longer issues a blind DELETE of an absent bundle key).
    for (label, put_at, delete_at) in [("before_any_put", 0usize, usize::MAX), ("after_first_put", 1, usize::MAX), ("at_manifest_put", 4, usize::MAX), ("during_stale_sweep", usize::MAX, 0)] {
        let u = uniq();
        let (pb, wb) = mk_buckets(&be, &u);
        let slug = "feat-x";
        write_meta(&be, &wb, &WorktreeMeta { slug: slug.into(), project: "01abc".into(), branch: "feat-x".into(), base_commit: "base".into(), created_ms: 1 }).unwrap();
        let files = doc_set(20);
        let refs: Vec<(&str, String)> = files.iter().map(|(p, c)| (p.as_str(), c.clone())).collect();
        let mut t = Tree::new(&refs);
        t.publish(&be, &wb, "head-1", &me, 1_000).unwrap();
        write_registry_entry(&be, &pb, &summarize(&be, slug, &wb).unwrap().unwrap()).unwrap();
        let m1 = be.get(&wb, MANIFEST_KEY).unwrap().unwrap();
        let (ok1, bad1, _) = read_all(&be, &pb, slug, &wb);
        assert_eq!((ok1, bad1), (20, 0));

        t.write("docs/d3.md", "# Doc 3\nSECOND version, longer, edited in place\n");
        for i in 20..23 {
            t.write(&format!("docs/d{i}.md"), &format!("# Doc {i}\nnew in v2\n"));
        }
        t.remove("docs/d0.md");

        let t0 = Instant::now();
        let flaky = Flaky::new(&be, put_at, delete_at);
        let r = t.publish(&flaky, &wb, "head-2", &me, 2_000);
        assert!(r.is_err(), "{label}: the injected failure must surface, got {r:?}");
        let m_after = be.get(&wb, MANIFEST_KEY).unwrap().unwrap();
        let manifest_is_old = m_after == m1;
        if put_at != usize::MAX && put_at <= 4 {
            assert!(manifest_is_old, "{label}: the previous manifest must still be served byte for byte");
        } else {
            assert!(!manifest_is_old, "{label}: the manifest was already replaced");
        }
        let (ok, bad, other) = read_all(&be, &pb, slug, &wb);
        assert_eq!(other, 0, "{label}: no read may fail any way but a loud checksum mismatch");
        // doc set reachable through the registry entry, as a follower sees it
        let still = summarize(&be, slug, &wb).unwrap().unwrap();
        println!("DETAIL|3a|{label}|manifest_old={manifest_is_old}|readable={ok}|checksum_mismatch={bad}|head_in_entry={}|failed_after={} ms", still.head_commit, t0.elapsed().as_millis());
        if bad > 0 {
            finding("3a", format!("{label}: with the previous manifest still served, {bad} file(s) of it fail with a checksum mismatch until the rerun (publish overwrites a changed object in place before it writes the new manifest)"));
        }

        // rerun: completes, nothing lost, nothing stale
        let t1 = Instant::now();
        let rep = t.publish(&be, &wb, "head-2", &me, 3_000).unwrap();
        write_registry_entry(&be, &pb, &summarize(&be, slug, &wb).unwrap().unwrap()).unwrap();
        let (ok2, bad2, other2) = read_all(&be, &pb, slug, &wb);
        assert_eq!((ok2, bad2, other2), (22, 0, 0), "{label}: after the rerun all 22 files read clean");
        let objs: Vec<_> = ObjectBackend::list(&be, &wb, TREE_PREFIX).unwrap().into_iter().map(|m| m.key).collect();
        assert_eq!(objs.len(), 22, "{label}: no stale object left (d0 removed)");
        assert!(!objs.iter().any(|k| k.ends_with("docs/d0.md")));
        let dest = tempfile::tempdir().unwrap();
        let fr = fetch_tree(&be, &wb, dest.path(), Select::All, false).unwrap();
        assert_eq!((fr.written.len(), fr.missing.len(), fr.conflicts.len(), fr.refused.len()), (22, 0, 0, 0));
        assert_eq!(std::fs::read_to_string(dest.path().join("docs/d3.md")).unwrap(), "# Doc 3\nSECOND version, longer, edited in place\n");
        line("3a", label, t1.elapsed().as_millis(), format!("rerun uploaded={} unchanged={} removed={}; 22/22 read clean, fetch 22 written 0 missing", rep.uploaded, rep.unchanged, rep.removed));
        wipe(&be, &[&wb, &pb]);
    }
}

#[test]
#[ignore = "needs a throwaway RustFS: WEFTOS_STORE_TEST_RUSTFS_ENV"]
fn drill_3b_stale_claim_and_future_stamp() {
    let (be, _d) = backend();
    let u = uniq();
    let (pb, wb) = mk_buckets(&be, &u);
    let (w, a, b) = (who("writer", "m1"), who("merger-a", "m2"), who("merger-b", "m3"));
    let t = Tree::new(&[("docs/a.md", "# A\n".into())]);
    let base = 1_000 * D;
    t.publish(&be, &wb, "h1", &w, base).unwrap();
    mark_handoff(&be, &wb, &w, "ready", base + MIN).unwrap();

    let t0 = Instant::now();
    let c1 = claim_merge(&be, &wb, &a, base + 2 * MIN).unwrap();
    assert_eq!((c1.state, c1.epoch), (State::Claimed, 1));
    // a second claimer inside the TTL is refused; the writer cannot republish over a live claim either
    let r = claim_merge(&be, &wb, &b, base + 2 * MIN + 29 * MIN);
    assert!(matches!(r, Err(S2Error::Claimed(_))), "29 min old: still live, got {r:?}");
    let r = t.publish(&be, &wb, "h2", &w, base + 2 * MIN + 29 * MIN);
    assert!(matches!(r, Err(S2Error::Claimed(_))), "writer refused while the claim is live");
    line("3b", "claim inside the 30 min TTL is refused (29 min)", t0.elapsed().as_millis(), "second claimer and the writer both refused");

    let t1 = Instant::now();
    let c2 = claim_merge(&be, &wb, &b, base + 2 * MIN + 31 * MIN).unwrap();
    assert_eq!((c2.epoch, c2.merger.as_ref().map(|m| m.agent.as_str())), (2, Some("merger-b")));
    line("3b", "claim older than the TTL (31 min) is taken over", t1.elapsed().as_millis(), "epoch 1 -> 2, merger b");
    // fencing: the displaced claimer cannot finish the merge with its old epoch, nor at all
    let r = mark_merged_at(&be, &wb, &a, Some(1), "late", base + 40 * MIN);
    assert!(matches!(r, Err(S2Error::Claimed(_))), "displaced claimer refused: {r:?}");
    let r = mark_merged_at(&be, &wb, &b, Some(1), "stale epoch", base + 40 * MIN);
    assert!(matches!(r, Err(S2Error::Claimed(_))), "stale epoch refused even for the new claimer: {r:?}");
    line("3b", "displaced claimer and a stale epoch are fenced off", t1.elapsed().as_millis(), "mark_merged_at refused twice");

    // future stamp: a claimer whose clock is 11 min ahead (past the 10 min skew) is not trusted as live
    let t2 = Instant::now();
    let u2 = uniq();
    let wb2 = format!("t-w2-{u2}");
    make_bucket(&be, &wb2);
    t.publish(&be, &wb2, "h1", &w, base).unwrap();
    mark_handoff(&be, &wb2, &w, "ready", base).unwrap();
    claim_merge(&be, &wb2, &a, base + 11 * MIN).unwrap(); // stamped 11 min after the asking clock
    let r = claim_merge(&be, &wb2, &b, base);
    assert_eq!(r.as_ref().map(|h| h.epoch).ok(), Some(2), "claim stamped 11 min in the future is expired: {r:?}");
    line("3b", "claim stamped 11 min ahead of this clock is treated as expired", t2.elapsed().as_millis(), "taken over at epoch 2");
    // ...and one stamped 9 min ahead (inside the skew) is still live
    let t3 = Instant::now();
    let wb3 = format!("t-w3-{u2}");
    make_bucket(&be, &wb3);
    t.publish(&be, &wb3, "h1", &w, base).unwrap();
    mark_handoff(&be, &wb3, &w, "ready", base).unwrap();
    claim_merge(&be, &wb3, &a, base + 9 * MIN).unwrap();
    let r = claim_merge(&be, &wb3, &b, base);
    assert!(matches!(r, Err(S2Error::Claimed(_))), "9 min ahead is inside the skew: still live, got {r:?}");
    line("3b", "claim stamped 9 min ahead (inside the 10 min skew) stays live", t3.elapsed().as_millis(), "second claimer refused");
    // a claim stamped a day ahead stays takeable at every later clock until real time passes it: it cannot block for the TTL
    let r = claim_merge(&be, &wb3, &b, base + 9 * MIN + 31 * MIN);
    assert!(r.is_ok(), "and 31 min after its stamp it is expired: {r:?}");
    wipe(&be, &[&wb, &wb2, &wb3, &pb]);
}

#[test]
#[ignore = "needs a throwaway RustFS: WEFTOS_STORE_TEST_RUSTFS_ENV"]
fn drill_3b_observed_claim_expiry_is_what_the_daemon_uses() {
    // The daemon's `claim` verb uses claim_merge_observed (expiry on time the daemon watched itself), not the wall-clock claim_merge above.
    let (be, _d) = backend();
    let u = uniq();
    let (pb, wb) = mk_buckets(&be, &u);
    let (w, a, b) = (who("writer", "m1"), who("merger-a", "m2"), who("merger-b", "m3"));
    let t = Tree::new(&[("docs/a.md", "# A\n".into())]);
    let base = 1_000 * D;
    t.publish(&be, &wb, "h1", &w, base).unwrap();
    mark_handoff(&be, &wb, &w, "ready", base).unwrap();
    let t0 = Instant::now();
    let mut watch = ClaimWatch::new();
    claim_merge_observed(&be, &wb, &a, &mut watch, 0, base).unwrap();
    // an old stamp does not make it stale: the first sighting starts the clock, whatever the stored stamp says
    let r = claim_merge_observed(&be, &wb, &b, &mut watch, 1_000, base + 5 * D);
    assert!(matches!(r, Err(S2Error::Claimed(_))), "first sighting: age 0, refused even though the stamp is 5 days old: {r:?}");
    let r = claim_merge_observed(&be, &wb, &b, &mut watch, 29 * MIN, base + 5 * D);
    assert!(matches!(r, Err(S2Error::Claimed(_))), "29 min observed: refused: {r:?}");
    let c = claim_merge_observed(&be, &wb, &b, &mut watch, 31 * MIN, base + 5 * D).unwrap();
    assert_eq!(c.epoch, 2);
    line("3b", "observed expiry: refused at first sight and 29 min, taken at 31 min observed (stamp age irrelevant)", t0.elapsed().as_millis(), "epoch 1 -> 2");
    // a daemon restart forgets the watch: the next taker waits the full TTL again
    let t1 = Instant::now();
    let mut fresh = ClaimWatch::new();
    let r = claim_merge_observed(&be, &wb, &a, &mut fresh, 10 * D, base + 6 * D);
    assert!(matches!(r, Err(S2Error::Claimed(_))), "after a restart the claim is watched again from zero: {r:?}");
    let c = claim_merge_observed(&be, &wb, &a, &mut fresh, 10 * D + 31 * MIN, base + 6 * D).unwrap();
    assert_eq!(c.epoch, 3);
    line("3b", "daemon restart forgets the watch: a 6-day-old claim waits 31 observed minutes again (safe side)", t1.elapsed().as_millis(), "epoch 2 -> 3");
    wipe(&be, &[&wb, &pb]);
}

#[test]
#[ignore = "needs a throwaway RustFS: WEFTOS_STORE_TEST_RUSTFS_ENV"]
fn drill_3c_registry_refresh_cut_between_entry_and_digest() {
    // The daemon refresh is: entry put, then a digest rewrite (a hint, its failure is swallowed). Cut it between the two and
    // show what a digest follower and a --full follower see, then that the next refresh heals it.
    let (be, _d) = backend();
    let u = uniq();
    let (pb, wb) = mk_buckets(&be, &u);
    let w = who("writer", "m1");
    let mut t = Tree::new(&[("docs/a.md", "# A\n".into())]);
    t.publish(&be, &wb, "h1", &w, 1_000).unwrap();
    write_registry_entry(&be, &pb, &summarize(&be, "wt", &wb).unwrap().unwrap()).unwrap();
    let mut fast = FollowState::default();
    let mut full = FollowState::default();
    assert_eq!(poll_fast(&be, &pb, &mut fast, false, true, 4).unwrap().len(), 1);
    assert_eq!(poll_fast(&be, &pb, &mut full, false, false, 4).unwrap().len(), 1);

    // head moves; the refresh dies after the entry put (put #0), before the digest put (put #1, a hint, error swallowed)
    t.write("docs/a.md", "# A\nsecond\n");
    t.publish(&be, &wb, "h2", &w, 2_000).unwrap();
    let e2 = summarize(&be, "wt", &wb).unwrap().unwrap();
    let t0 = Instant::now();
    write_registry_entry(&Flaky::new(&be, 1, usize::MAX), &pb, &e2).unwrap();
    let digest_etag = read_digest(&be, &pb).unwrap().unwrap().entries["wt"].clone();
    let live_etag = registry_listing(&be, &pb).unwrap()["wt"].clone();
    assert_ne!(digest_etag, live_etag, "the digest is one update behind");
    let ch_fast = poll_fast(&be, &pb, &mut fast, false, true, 4).unwrap();
    let ch_full = poll_fast(&be, &pb, &mut full, false, false, 4).unwrap();
    println!("DETAIL|3c|cut after the entry put: digest follower sees {} change(s), --full follower sees {} ({ch_full:?})", ch_fast.len(), ch_full.len());
    assert_eq!(ch_full.len(), 1, "--full sees the head move");
    if ch_fast.is_empty() {
        finding("3c", "documented limit reproduced: with the digest one update behind, a digest follower misses the head move until the next registry refresh of ANY worktree; `follow --full` sees it at once. Self-heals on the next refresh (below). Non-blocking; a daemon restart does not rewrite the digest");
    }
    line("3c", "refresh cut between entry put and digest put: entry is current, digest stale, --full sees it", t0.elapsed().as_millis(), format!("digest follower saw {} change(s)", ch_fast.len()));

    // heal: the next refresh of anything rewrites the digest from a fresh listing
    let t1 = Instant::now();
    write_digest(&be, &pb).unwrap();
    let ch = poll_fast(&be, &pb, &mut fast, false, true, 4).unwrap();
    assert!(ch.is_empty() || ch.len() == 1);
    assert_eq!(read_digest(&be, &pb).unwrap().unwrap().entries, registry_listing(&be, &pb).unwrap());
    line("3c", "next digest rewrite heals it: digest == live listing, the digest follower has caught up", t1.elapsed().as_millis(), format!("late change(s) reported {}", ch.len()));

    // removal cut: entry deleted, digest still names it
    let t2 = Instant::now();
    ObjectBackend::delete(&be, &pb, "worktrees/wt.json").unwrap(); // the digest write that follows in remove_registry_entry never happens
    assert!(read_digest(&be, &pb).unwrap().unwrap().entries.contains_key("wt"));
    let r = poll_fast(&be, &pb, &mut fast, true, true, 4);
    println!("DETAIL|3c|entry deleted, digest still lists it: digest follower -> {r:?}");
    let r_full = poll_fast(&be, &pb, &mut full, true, false, 4);
    println!("DETAIL|3c|same, --full follower -> {r_full:?}");
    match (&r, &r_full) {
        (Ok(a), Ok(b)) if a == b => {}
        _ => finding("3c", format!("removal cut: digest follower {r:?} vs --full follower {r_full:?} disagree on a registry entry that was deleted while the digest still names it")),
    }
    write_digest(&be, &pb).unwrap();
    line("3c", "removal cut: digest still names a deleted entry; both pollers answered, digest rewrite heals", t2.elapsed().as_millis(), format!("{r:?}"));
    wipe(&be, &[&wb, &pb]);
}

