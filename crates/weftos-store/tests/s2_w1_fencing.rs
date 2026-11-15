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
