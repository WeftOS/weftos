//! WO-069 lane W1: adversarial tests that try to get around the fixes for follower findings 6a-1 (a merge recorded for a head
//! that is no longer the published one), 6a-2 (an orphan hidden behind a future stamp), 6a-3 (a far-future stamp pinning the
//! registry signature generation) and 6g (a shared `FollowState` file losing a remembered generation). The expectation tests
//! themselves are in `s2_drills_follower_findings.rs`.
#[macro_use]
#[path = "s2_drills_fcommon/mod.rs"]
mod fc;
use fc::*;
use weftos_store::follower::*;
use weftos_store::orphan::OrphanInfo;
use weftos_store::share::*;
use weftos_store::share_fetch::RegistryEntry;

// ---------------------------------------------------------------------------------------------------------------------
// 6a-2: an orphan is always an orphan
// ---------------------------------------------------------------------------------------------------------------------

fn entry(slug: &str, state: State, updated_ms: u64, orphan: Option<OrphanInfo>) -> RegistryEntry {
    RegistryEntry {
        slug: slug.into(), bucket: format!("w-x-{slug}"), branch: "b".into(), head_commit: "h".into(), state, writer: actor("w"),
        merger: None, doc_count: 0, file_count: 0, updated_ms, orphan,
    }
}

#[test]
fn an_orphan_with_a_u64_max_stamp_and_no_orphan_record_is_still_an_orphan() {
    let now = T0;
    let l = leftovers(&[entry("o", State::Orphan, u64::MAX, None)], now, &LeftoverPolicy::default());
    assert_eq!((l.len(), l[0].why, l[0].idle_ms), (1, Why::Orphan, 0), "no orphan record: age from the stamp, which is ahead, so 0");
}

#[test]
fn an_orphan_marked_ahead_of_the_follower_clock_has_age_zero_not_a_wrap() {
    let now = T0;
    let info = OrphanInfo { orphaned_at_ms: now + DAY, last_head: "h".into(), missing_since_ms: now };
    let l = leftovers(&[entry("o", State::Orphan, now + 2 * DAY, Some(info))], now, &LeftoverPolicy::default());
    assert_eq!((l[0].why, l[0].idle_ms), (Why::Orphan, 0));
}

#[test]
fn the_future_stamp_rule_still_applies_to_every_other_unmerged_state() {
    let now = T0;
    let ahead = now + DAY;
    let es: Vec<_> = [State::Open, State::Handoff, State::Claimed].iter().enumerate().map(|(i, s)| entry(&format!("s{i}"), *s, ahead, None)).collect();
    let l = leftovers(&es, now, &LeftoverPolicy::default());
    assert!(l.len() == 3 && l.iter().all(|x| x.why == Why::FutureTimestamp), "{l:?}");
    let done = [entry("m", State::Merged, ahead, None), entry("a", State::Archived, ahead, None)];
    assert!(leftovers(&done, now, &LeftoverPolicy::default()).is_empty(), "merged/archived ahead are not leftovers yet");
}

// ---------------------------------------------------------------------------------------------------------------------
// 6a-3: every signed write strictly increases the generation
// ---------------------------------------------------------------------------------------------------------------------

use weftos_store::overlay::ObjectBackend as S2;
use weftos_store::registry_sign::*;
use weftos_store::testing::MemBackend;

const PR: &str = "p-gen";

fn signed_gen(be: &MemBackend, slug: &str) -> u64 {
    let v: serde_json::Value = serde_json::from_slice(&S2::get(be, PR, &format!("worktrees.sig/{slug}.json")).unwrap().unwrap()).unwrap();
    v["generation"].as_u64().unwrap()
}

fn snapshot(be: &MemBackend, slug: &str) -> (Vec<u8>, Vec<u8>) {
    (S2::get(be, PR, &format!("worktrees/{slug}.json")).unwrap().unwrap(), S2::get(be, PR, &format!("worktrees.sig/{slug}.json")).unwrap().unwrap())
}

fn replay(be: &MemBackend, slug: &str, s: &(Vec<u8>, Vec<u8>)) {
    S2::put(be, PR, &format!("worktrees/{slug}.json"), &s.0).unwrap();
    S2::put(be, PR, &format!("worktrees.sig/{slug}.json"), &s.1).unwrap();
}

fn reg_be() -> MemBackend {
    MemBackend::new() // in-memory buckets need no creating
}

#[test]
fn next_generation_never_saturates_into_a_repeat() {
    let c = GENERATION_STAMP_CEILING;
    assert_eq!(next_generation(0, u64::MAX).unwrap(), c, "the stamp is clamped");
    assert_eq!(next_generation(c, u64::MAX).unwrap(), c + 1, "and the next write still adds one");
    assert_eq!(next_generation(5, 3).unwrap(), 6, "a past stamp never lowers it");
    assert_eq!(next_generation(u64::MAX - 1, u64::MAX).unwrap(), u64::MAX);
    assert!(next_generation(u64::MAX, 0).is_err(), "an exhausted generation is refused, never repeated");
    let mut g = 0;
    for stamp in [u64::MAX, u64::MAX, 0, c, c + 7, u64::MAX] {
        let n = next_generation(g, stamp).unwrap();
        assert!(n > g, "{n} > {g} for stamp {stamp}");
        g = n;
    }
}

#[test]
fn a_legacy_sidecar_pinned_at_u64_max_refuses_the_write_and_changes_nothing() {
    let be = reg_be();
    let e = entry("pin", State::Open, T0, None);
    write_registry_entry_signed(&be, PR, &e, &Toy).unwrap();
    let mut side: serde_json::Value = serde_json::from_slice(&snapshot(&be, "pin").1).unwrap();
    side["generation"] = u64::MAX.into(); // what the old saturating rule left behind
    S2::put(&be, PR, "worktrees.sig/pin.json", &serde_json::to_vec(&side).unwrap()).unwrap();
    let before = snapshot(&be, "pin");
    let next = RegistryEntry { state: State::Merged, ..e };
    assert!(write_registry_entry_signed(&be, PR, &next, &Toy).is_err());
    assert_eq!(snapshot(&be, "pin"), before, "neither the entry nor its signature was rewritten");
}

#[test]
fn equal_generation_is_the_same_write_lower_is_refused_and_a_past_stamp_after_a_future_one_still_climbs() {
    let be = reg_be();
    let mut st = FollowState::default();
    // a future stamp, then the writer's clock is corrected to the past: the generation still climbs
    let e = entry("wt", State::Open, T0 + DAY, None);
    write_registry_entry_signed(&be, PR, &e, &Toy).unwrap();
    let future = snapshot(&be, "wt");
    assert_eq!(kinds(&weftos_store::registry_sign::poll_verified(&be, PR, &mut st, &Toy, true, false).unwrap()), ["new:wt"]);
    let e2 = RegistryEntry { state: State::Handoff, updated_ms: T0, ..e.clone() };
    write_registry_entry_signed(&be, PR, &e2, &Toy).unwrap();
    assert_eq!(signed_gen(&be, "wt"), T0 + DAY + 1);
    assert_eq!(kinds(&poll_verified(&be, PR, &mut st, &Toy, true, false).unwrap()), ["state:wt"]);
    // the same write seen again (equal generation) is not a rollback
    assert!(poll_verified(&be, PR, &mut st, &Toy, true, false).unwrap().is_empty());
    // the older, future-stamped write put back is
    replay(&be, "wt", &future);
    assert_eq!(kinds(&poll_verified(&be, PR, &mut st, &Toy, true, false).unwrap()), ["bad:wt"]);
    assert_eq!(st.seen["wt"].state, State::Handoff, "the refused entry did not overwrite what was known");
}

#[test]
fn a_run_of_u64_max_stamps_keeps_every_older_write_replayable_as_a_refusal() {
    let be = reg_be();
    let mut st = FollowState::default();
    let mut olds = Vec::new();
    for (i, s) in [State::Open, State::Handoff, State::Claimed, State::Merged].into_iter().enumerate() {
        write_registry_entry_signed(&be, PR, &entry("max", s, u64::MAX, None), &Toy).unwrap();
        assert_eq!(signed_gen(&be, "max"), GENERATION_STAMP_CEILING + i as u64);
        poll_verified(&be, PR, &mut st, &Toy, true, false).unwrap();
        olds.push(snapshot(&be, "max"));
    }
    let newest = olds.pop().unwrap();
    for old in &olds {
        replay(&be, "max", old);
        assert_eq!(kinds(&poll_verified(&be, PR, &mut st, &Toy, true, false).unwrap()), ["bad:max"]);
    }
    replay(&be, "max", &newest);
    assert!(poll_verified(&be, PR, &mut st, &Toy, true, false).unwrap().is_empty(), "the newest write is accepted again, with no change");
}

// ---------------------------------------------------------------------------------------------------------------------
// 6g: a save never lowers a remembered generation
// ---------------------------------------------------------------------------------------------------------------------

fn with_gen(slug: &str, g: u64) -> FollowState {
    let mut s = FollowState::default();
    s.generations.insert(slug.into(), g);
    s
}

fn dir_files(d: &std::path::Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(d).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    v.sort();
    v
}

#[test]
fn three_savers_in_every_order_leave_the_maximum_per_slug() {
    let views = [
        { let mut s = with_gen("a", 300); s.generations.insert("b".into(), 1); s },
        { let mut s = with_gen("a", 100); s.generations.insert("b".into(), 50); s },
        { let mut s = with_gen("a", 200); s.generations.insert("c".into(), 7); s },
    ];
    for order in [[0, 1, 2], [0, 2, 1], [1, 0, 2], [1, 2, 0], [2, 0, 1], [2, 1, 0]] {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("follow.json");
        for i in order {
            views[i].save(&path).unwrap();
        }
        let got = FollowState::load(&path).unwrap().generations;
        assert_eq!((got["a"], got["b"], got["c"]), (300, 50, 7), "order {order:?}");
        assert_eq!(dir_files(d.path()), ["follow.json"], "no lock or temp file left");
    }
}

#[test]
fn many_threads_saving_at_once_never_lose_the_highest_generation() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("state").join("follow.json");
    std::thread::scope(|s| {
        for t in 0..8u64 {
            let path = &path;
            s.spawn(move || {
                for i in 0..40u64 {
                    // each thread climbs its own slug and also writes a stale view of a shared one
                    let mut st = with_gen(&format!("t{t}"), i);
                    st.generations.insert("shared".into(), t * 1000 + i);
                    st.save(path).unwrap();
                }
            });
        }
    });
    let got = FollowState::load(&path).unwrap().generations;
    for t in 0..8u64 {
        assert_eq!(got[&format!("t{t}")], 39, "thread {t}'s last generation survived every other saver");
    }
    assert_eq!(got["shared"], 7039);
    assert_eq!(dir_files(path.parent().unwrap()), ["follow.json"]);
}

#[test]
fn a_corrupt_or_missing_file_does_not_block_a_save_and_contributes_nothing() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("follow.json");
    with_gen("a", 5).save(&path).unwrap(); // missing: plain write
    assert_eq!(FollowState::load(&path).unwrap().generations["a"], 5);
    for junk in [&b""[..], b"{\"generations\":{\"a\":", b"[]", b"{\"generations\":{\"a\":\"high\"}}", &[0xff, 0x00]] {
        std::fs::write(&path, junk).unwrap();
        with_gen("a", 5).save(&path).unwrap();
        assert_eq!(FollowState::load(&path).unwrap().generations["a"], 5, "{junk:?}: replaced, nothing invented");
    }
}

#[test]
fn a_file_whose_seen_this_version_cannot_parse_still_contributes_its_generations() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("follow.json");
    std::fs::write(&path, br#"{"seen":{"a":{"bucket":"b","head_commit":"h","state":"reviewing"}},"generations":{"a":900}}"#).unwrap();
    assert!(FollowState::load(&path).is_err(), "load still refuses it");
    with_gen("a", 10).save(&path).unwrap();
    assert_eq!(FollowState::load(&path).unwrap().generations["a"], 900);
}

#[test]
fn a_merged_save_keeps_rollback_protection_for_a_slug_the_saver_never_knew() {
    let be = reg_be();
    let e = entry("wt", State::Open, T0, None);
    write_registry_entry_signed(&be, PR, &e, &Toy).unwrap();
    let old = snapshot(&be, "wt");
    write_registry_entry_signed(&be, PR, &RegistryEntry { state: State::Merged, ..e }, &Toy).unwrap();
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("follow.json");
    let mut knows = FollowState::default();
    poll_verified(&be, PR, &mut knows, &Toy, true, false).unwrap();
    knows.save(&path).unwrap();
    FollowState::default().save(&path).unwrap(); // a fresh run that never saw `wt` saves last
    replay(&be, "wt", &old);
    let mut st = FollowState::load(&path).unwrap();
    assert!(st.seen.is_empty(), "seen is last writer wins: the fresh run's view");
    assert_eq!(kinds(&poll_verified(&be, PR, &mut st, &Toy, true, false).unwrap()), ["bad:wt"], "the generation survived, so the replay is refused");
}

// ---------------------------------------------------------------------------------------------------------------------
// 6a-1: a claimer can record a merge only of the head it claimed
// ---------------------------------------------------------------------------------------------------------------------

const NOW: u64 = T0 + 10 * DAY;

/// A published, handed-off worktree at head h1, then the writer's republish attempt at h2.
fn handed_off(env: &Env<'_, MemBackend>, slug: &str) -> String {
    let w = up(env, slug, &[("docs/u.md", "# U\n")], &Pub { now: NOW, ..Pub::default() });
    mark_handoff(env.be, &w.bucket, &actor("writer"), "go", NOW).unwrap();
    w.bucket
}

fn republish(be: &MemBackend, bucket: &str, head: &str, now: u64) -> weftos_store::overlay::Result<PublishReport> {
    push(be, bucket, &tree(&[("docs/u.md", &format!("# U {head}\n"))]), &Pub { head, now, ..Pub::default() })
}

#[test]
fn a_claim_stamped_exactly_at_the_skew_boundary_is_live_and_one_ms_past_it_is_not() {
    let be = MemBackend::new();
    let env = Env::new(&be, None);
    // at the boundary: live, so the writer's republish is refused and the claimer merges the head it claimed
    let b = handed_off(&env, "edge");
    let c = claim_merge(&be, &b, &actor("fast"), NOW + CLAIM_SKEW_MS).unwrap();
    assert_eq!(c.claimed_head.as_deref(), Some("h1"));
    assert!(matches!(republish(&be, &b, "h2", NOW), Err(weftos_store::overlay::S2Error::Claimed(_))));
    mark_merged_at(&be, &b, &actor("fast"), Some(c.epoch), "merged h1", NOW).unwrap();
    // one ms past it: not live, the republish goes through and the merge of h1 is refused, with or without an epoch
    let b = handed_off(&env, "past");
    let c = claim_merge(&be, &b, &actor("fast"), NOW + CLAIM_SKEW_MS + 1).unwrap();
    republish(&be, &b, "h2", NOW).unwrap();
    let err = mark_merged_at(&be, &b, &actor("fast"), Some(c.epoch), "merged h1", NOW).unwrap_err();
    assert!(err.to_string().contains("from h1 to h2"), "{err}");
    assert!(mark_merged(&be, &b, &actor("fast"), "merged h1", NOW).is_err(), "mark_merged (no epoch) is fenced on head too");
    assert_eq!(read_handoff(&be, &b).unwrap().unwrap().state, State::Claimed);
}

#[test]
fn a_republish_then_a_reclaim_fences_the_old_claim_and_admits_only_the_new_one() {
    let be = MemBackend::new();
    let env = Env::new(&be, None);
    // the same actor re-claims after the republish: its old epoch is fenced, its new claim (on h2) merges
    let b = handed_off(&env, "same");
    let old = claim_merge(&be, &b, &actor("fast"), NOW + DAY).unwrap();
    republish(&be, &b, "h2", NOW + 1).unwrap();
    let new = claim_merge(&be, &b, &actor("fast"), NOW + 2).unwrap();
    assert_eq!((new.epoch, new.claimed_head.as_deref()), (old.epoch + 1, Some("h2")));
    assert!(mark_merged_at(&be, &b, &actor("fast"), Some(old.epoch), "merged h1", NOW + 3).is_err());
    mark_merged_at(&be, &b, &actor("fast"), Some(new.epoch), "merged h2", NOW + 3).unwrap();
    // another actor re-claims: the old claimer is refused with its epoch, without one, and through fence_claim
    let b = handed_off(&env, "other");
    let old = claim_merge(&be, &b, &actor("fast"), NOW + DAY).unwrap();
    republish(&be, &b, "h2", NOW + 1).unwrap();
    let slow = claim_merge(&be, &b, &actor("slow"), NOW + 2).unwrap();
    assert!(mark_merged_at(&be, &b, &actor("fast"), Some(old.epoch), "late", NOW + 3).is_err());
    assert!(mark_merged(&be, &b, &actor("fast"), "late", NOW + 3).is_err());
    assert!(fence_claim(&be, &b, old.epoch).is_err());
    fence_claim(&be, &b, slow.epoch).unwrap();
    mark_merged_at(&be, &b, &actor("slow"), Some(slow.epoch), "merged h2", NOW + 3).unwrap();
}

#[test]
fn fence_claim_refuses_a_moved_head_at_the_current_epoch_and_passes_when_nothing_is_claimed() {
    let be = MemBackend::new();
    let env = Env::new(&be, None);
    let b = handed_off(&env, "fence");
    fence_claim(&be, &b, 0).unwrap(); // handed off, not claimed: nothing to fence
    let c = claim_merge(&be, &b, &actor("fast"), NOW + DAY).unwrap();
    fence_claim(&be, &b, c.epoch).unwrap();
    republish(&be, &b, "h2", NOW + 1).unwrap();
    let err = fence_claim(&be, &b, c.epoch).unwrap_err();
    assert!(err.to_string().contains("from h1 to h2"), "the epoch is current, the head is not: {err}");
}

#[test]
fn a_republish_of_the_claimed_head_by_the_holder_still_merges() {
    let be = MemBackend::new();
    let env = Env::new(&be, None);
    let b = handed_off(&env, "holder");
    let c = claim_merge(&be, &b, &actor("writer"), NOW).unwrap();
    republish(&be, &b, "h1", NOW + 1).unwrap(); // the holder may republish; the head is unchanged
    mark_merged_at(&be, &b, &actor("writer"), Some(c.epoch), "merged h1", NOW + 2).unwrap();
}

#[test]
fn a_record_from_before_claimed_head_still_parses_and_is_fenced_by_epoch_only() {
    let be = MemBackend::new();
    let env = Env::new(&be, None);
    let b = handed_off(&env, "legacy");
    let c = claim_merge(&be, &b, &actor("fast"), NOW).unwrap();
    let mut v: serde_json::Value = serde_json::from_slice(&S2::get(&be, &b, HANDOFF_KEY).unwrap().unwrap()).unwrap();
    v.as_object_mut().unwrap().remove("claimed_head");
    S2::put(&be, &b, HANDOFF_KEY, &serde_json::to_vec(&v).unwrap()).unwrap();
    let h = read_handoff(&be, &b).unwrap().unwrap();
    assert_eq!((h.state, h.claimed_head), (State::Claimed, None));
    assert!(mark_merged_at(&be, &b, &actor("fast"), Some(c.epoch + 1), "x", NOW).is_err(), "epoch fencing still holds");
    mark_merged_at(&be, &b, &actor("fast"), Some(c.epoch), "merged", NOW).unwrap();
}
