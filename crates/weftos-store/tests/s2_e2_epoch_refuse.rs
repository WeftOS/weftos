//! WO-069 lane E2, owner decision Q9 (2026-11-15): a caller that presents a claim epoch asserts "I hold
//! claim N". When the handoff record is not `Claimed`, the fenced paths (`fence_claim` and
//! `mark_landed_at(.., Some(epoch), ..)`) refuse with the record's state and the next step, instead of
//! passing as "nothing to fence". Presenting no epoch (design note B6) is unchanged. A repeated land by
//! the actor that already recorded it at that epoch stays the idempotent `Ok(false)`.
use weftos_store::overlay::{ObjectBackend, S2Error};
use weftos_store::share::*;
use weftos_store::testing::MemBackend;

const WT: &str = "w-01abc-cccccccccccc";
const NOW: u64 = 1_000_000_000;

fn who(n: &str) -> Actor {
    Actor { agent: n.into(), host: "h".into() }
}

fn publish(b: &MemBackend, head: &str, now: u64) {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("a.md"), "a").unwrap();
    let ps = vec!["a.md".to_string()];
    let src = TreeSource {
        root: d.path(),
        paths: &ps,
        uncommitted: &[],
        changes: &[],
        changes_recorded: false,
        branch: "feat/e2",
        base_commit: "base",
        head_commit: head,
        bundle: None,
        writer: who("writer"),
    };
    publish_tree(b, WT, &src, &Limits::default(), now).unwrap();
}

fn handed_off() -> MemBackend {
    let b = MemBackend::new();
    publish(&b, "h1", NOW);
    mark_handoff(&b, WT, &who("writer"), "ready", NOW).unwrap();
    b
}

fn state(b: &MemBackend) -> Handoff {
    read_handoff(b, WT).unwrap().unwrap()
}

/// Rewrite the stored record's state, for states no code path writes to `handoff.json` (archived).
fn force_state(b: &MemBackend, s: &str) {
    let mut v: serde_json::Value = serde_json::from_slice(&ObjectBackend::get(b, WT, HANDOFF_KEY).unwrap().unwrap()).unwrap();
    v["state"] = serde_json::json!(s);
    ObjectBackend::put(b, WT, HANDOFF_KEY, &serde_json::to_vec(&v).unwrap()).unwrap();
}

/// Both fenced entry points, as the closures the table below runs.
fn both(b: &MemBackend, epoch: u64, now: u64) -> [Result<(), S2Error>; 2] {
    [fence_claim(b, WT, epoch, now), mark_landed_at(b, WT, &who("bob"), Some(epoch), "landed", now).map(|_| ())]
}

fn refused(r: Result<(), S2Error>, state_word: &str) {
    let e = r.unwrap_err();
    assert!(matches!(e, S2Error::Claimed(_)), "{e}");
    let m = e.to_string();
    assert!(m.contains("not claimed") || m.contains("no handoff record"), "{m}");
    assert!(m.starts_with("claim refused: ") && !m.contains("is claimed by"), "a fence refusal is not worded as a holder: {m}");
    assert!(m.contains(state_word), "names the state ({state_word}): {m}");
}

#[test]
fn an_epoch_with_no_handoff_record_is_refused_and_writes_nothing() {
    let b = MemBackend::new();
    for r in both(&b, 0, NOW) {
        refused(r, "no handoff record");
    }
    assert!(read_handoff(&b, WT).unwrap().is_none());
}

#[test]
fn an_epoch_on_an_open_record_is_refused() {
    let b = MemBackend::new();
    publish(&b, "h1", NOW);
    assert_eq!(state(&b).state, State::Open);
    for r in both(&b, 0, NOW) {
        refused(r, "Open");
    }
    assert_eq!(state(&b).state, State::Open, "nothing was written");
}

#[test]
fn an_epoch_on_a_handed_off_record_is_refused_with_the_next_step() {
    let b = handed_off();
    for r in both(&b, 0, NOW) {
        let m = r.unwrap_err().to_string();
        assert!(m.contains("Handoff") && m.contains("claim it again"), "{m}");
    }
    assert_eq!(state(&b).state, State::Handoff);
}

#[test]
fn epochs_only_go_up_so_another_actor_cannot_hold_the_same_epoch() {
    // By construction: every claim (first, takeover, or the holder's own re-claim) writes epoch + 1,
    // so no two claims, by anyone, share an epoch number.
    let b = handed_off();
    let a = claim_merge(&b, WT, &who("bob"), NOW).unwrap();
    let again = claim_merge(&b, WT, &who("bob"), NOW + 1).unwrap();
    let taken = claim_merge(&b, WT, &who("carol"), NOW + CLAIM_TTL_MS + 5).unwrap();
    assert!(a.epoch < again.epoch && again.epoch < taken.epoch, "{} {} {}", a.epoch, again.epoch, taken.epoch);
}

#[test]
fn a_claimed_record_at_another_epoch_is_still_refused_and_the_current_one_lands() {
    let b = handed_off();
    let old = claim_merge(&b, WT, &who("bob"), NOW).unwrap();
    let new = claim_merge(&b, WT, &who("carol"), NOW + CLAIM_TTL_MS + 5).unwrap();
    for r in both(&b, old.epoch, NOW + CLAIM_TTL_MS + 6) {
        assert!(r.unwrap_err().to_string().contains("stale"));
    }
    assert!(mark_landed_at(&b, WT, &who("carol"), Some(new.epoch), "landed", NOW + CLAIM_TTL_MS + 6).unwrap());
}

#[test]
fn a_merged_record_refuses_a_presented_epoch_except_the_same_actor_repeating_at_the_same_epoch() {
    let b = handed_off();
    let c = claim_merge(&b, WT, &who("bob"), NOW).unwrap();
    assert!(mark_landed_at(&b, WT, &who("bob"), Some(c.epoch), "landed", NOW + 1).unwrap());
    let landed = state(&b);
    // the repeat (a promote retried after the land was recorded) is the idempotent success: nothing is written
    assert!(!mark_landed_at(&b, WT, &who("bob"), Some(c.epoch), "again", NOW + 2).unwrap());
    assert_eq!(state(&b), landed, "the record is untouched");
    // another actor, an older epoch, or a later epoch: refused, naming who merged and when
    for (lander, epoch) in [("carol", c.epoch), ("bob", c.epoch - 1), ("bob", c.epoch + 1)] {
        let m = mark_landed_at(&b, WT, &who(lander), Some(epoch), "x", NOW + 3).unwrap_err().to_string();
        assert!(m.contains("Merged") && m.contains("bob@h"), "{lander} {epoch}: {m}");
    }
    // fence_claim has no actor to compare, so a merged record refuses it
    refused(fence_claim(&b, WT, c.epoch, NOW + 3), "Merged");
    // and without an epoch a repeat is still the unfenced Ok(false) (B6)
    assert!(!mark_landed_at(&b, WT, &who("carol"), None, "x", NOW + 3).unwrap());
}

#[test]
fn an_archived_or_orphan_record_refuses_a_presented_epoch_and_not_an_unfenced_land() {
    let b = handed_off();
    force_state(&b, "archived");
    for r in both(&b, 0, NOW) {
        refused(r, "Archived");
    }
    assert!(!mark_landed_at(&b, WT, &who("bob"), None, "x", NOW).unwrap(), "unfenced: unchanged");
}

#[test]
fn without_an_epoch_every_state_is_unchanged() {
    // absent record: nothing to record
    assert!(!mark_landed_at(&MemBackend::new(), WT, &who("bob"), None, "x", NOW).unwrap());
    // handed off, never claimed: the land records itself (B6, open)
    let b = handed_off();
    assert!(mark_landed_at(&b, WT, &who("bob"), None, "x", NOW).unwrap());
    assert_eq!(state(&b).state, State::Merged);
}

/// Gate block 4, finding 1: a holder reads "worktree is claimed by agent@host"; a fence refusal
/// (a sentence) reads "claim refused: ...", never "claimed by <sentence>".
#[test]
fn claimed_display_tells_a_holder_from_a_fence_refusal() {
    assert_eq!(S2Error::Claimed("merger@xpc-m1".into()).to_string(), "worktree is claimed by merger@xpc-m1");
    assert_eq!(
        S2Error::Claimed("claim epoch 1 is stale (now 2); the claim was taken over".into()).to_string(),
        "claim refused: claim epoch 1 is stale (now 2); the claim was taken over"
    );
}
