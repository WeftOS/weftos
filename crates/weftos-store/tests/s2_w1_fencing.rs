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
