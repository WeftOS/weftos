//! Loopback chain and snapshot schedule (ADR-126, "Chain compaction").

use crate::broker::GrantCheck;
use crate::compaction::{SnapshotSchedule, SnapshotVerifier};
use crate::grants::{node_store_name, GrantEvent, GrantLedger, GrantOp};
use crate::loopback::{ChainEntry, ChainError, LoopbackChain};
use crate::policy::Access;
use hmac::{Hmac, Mac};
use sha2::Sha256;

const MESH: &str = "m1";
const KEY: &[u8] = b"chain-signer-key";
const SCOPE: &str = "grants/m1";

fn mac(key: &[u8], msg: &[u8]) -> Vec<u8> {
    let mut m = Hmac::<Sha256>::new_from_slice(key).unwrap();
    m.update(msg);
    m.finalize().into_bytes().to_vec()
}

struct Ring;
impl SnapshotVerifier for Ring {
    fn verify(&self, signer: &str, message: &[u8], signature: &[u8]) -> bool {
        signer == "chain-1" && mac(KEY, message) == signature
    }
}

fn chain() -> LoopbackChain {
    LoopbackChain::new(SCOPE, "chain-1", |m| mac(KEY, m))
}

/// Pair node `n<i>` at seq `2i+1`, and for even `i` unpair it again at `2i+2`.
fn feed(c: &mut LoopbackChain, nodes: u64, now: u64) {
    for i in 0..nodes {
        let id = format!("n{i}");
        c.append(GrantEvent::pairing(2 * i + 1, MESH, &id), now + i).unwrap();
        if i % 2 == 0 {
            c.append(GrantEvent::unpair(2 * i + 2, MESH, &id), now + i).unwrap();
        }
    }
}

fn snapshots(c: &LoopbackChain) -> usize {
    c.entries().iter().filter(|e| matches!(e, ChainEntry::Snapshot(_))).count()
}

fn full(c: &LoopbackChain) -> GrantLedger {
    let g: Vec<GrantEvent> = c
        .entries()
        .iter()
        .filter_map(|e| if let ChainEntry::Grant(g) = e { Some(g.clone()) } else { None })
        .collect();
    GrantLedger::replay(g)
}

#[test]
fn the_default_schedule_is_off_and_never_takes_a_snapshot() {
    assert!(!SnapshotSchedule::default().is_enabled());
    assert!(!SnapshotSchedule::default().due(1_000_000, 1_000_000));
    assert!(!SnapshotSchedule::off().due(u64::MAX, u64::MAX));
    // Zero is off, not "always".
    assert!(!SnapshotSchedule::every_events(0).is_enabled());
    assert!(!SnapshotSchedule::every_secs(0).is_enabled());

    let mut c = chain();
    feed(&mut c, 40, 1_000);
    assert_eq!(snapshots(&c), 0, "nothing is taken unless the host asks");
}

#[test]
fn an_event_count_schedule_snapshots_every_n_events_and_recovery_matches_replay() {
    let mut c = chain().with_schedule(SnapshotSchedule::every_events(10));
    feed(&mut c, 20, 1_000); // 30 events
    assert_eq!(snapshots(&c), 3);
    let r = c.recover(&Ring).unwrap();
    assert_eq!(r.from_snapshot, Some(c.snapshots().last().unwrap().snapshot.through_seq));
    assert!(r.tail_len < 10, "only the tail is replayed, not the log");
    assert_eq!(r.ledger.snapshot(SCOPE), full(&c).snapshot(SCOPE));
}

#[test]
fn a_time_schedule_needs_both_elapsed_time_and_a_new_event() {
    let s = SnapshotSchedule::every_secs(60);
    assert!(!s.due(0, 10_000), "nothing new: not due however long it has been");
    assert!(!s.due(5, 59));
    assert!(s.due(1, 60));

    let mut c = chain().with_schedule(s);
    c.append(GrantEvent::pairing(1, MESH, "a"), 1_000).unwrap();
    c.append(GrantEvent::pairing(2, MESH, "b"), 1_059).unwrap();
    assert_eq!(snapshots(&c), 0);
    c.append(GrantEvent::pairing(3, MESH, "c"), 1_060).unwrap();
    assert_eq!(snapshots(&c), 1);
}

#[test]
fn taking_a_snapshot_does_not_change_the_ledger() {
    let mut c = chain();
    feed(&mut c, 6, 1_000);
    let before = c.ledger().snapshot(SCOPE);
    c.snapshot_now(2_000).unwrap();
    assert_eq!(c.ledger().snapshot(SCOPE), before);
    // A snapshot entry is not a grant event: the next event keeps counting seq.
    c.append(GrantEvent::pairing(100, MESH, "z"), 2_001).unwrap();
    assert!(c.ledger().allows("z", Access::Write));
}

#[test]
fn an_empty_chain_has_nothing_to_snapshot_and_a_repeated_seq_is_refused() {
    let mut c = chain();
    assert_eq!(c.snapshot_now(1).unwrap_err(), ChainError::Empty);
    c.append(GrantEvent::pairing(5, MESH, "a"), 1).unwrap();
    assert_eq!(c.append(GrantEvent::pairing(5, MESH, "b"), 2).unwrap_err(), ChainError::OutOfOrder(5));
    assert_eq!(c.append(GrantEvent::pairing(4, MESH, "b"), 2).unwrap_err(), ChainError::OutOfOrder(4));
}

#[test]
fn a_forged_newest_snapshot_falls_back_to_the_previous_one_then_to_a_full_replay() {
    let mut c = chain();
    feed(&mut c, 8, 1_000);
    c.snapshot_now(2_000).unwrap();
    feed_more(&mut c, 100, 4);
    c.snapshot_now(3_000).unwrap();
    feed_more(&mut c, 200, 2);

    // Forge the newest snapshot's signature.
    let last = c.entries().iter().rposition(|e| matches!(e, ChainEntry::Snapshot(_))).unwrap();
    if let ChainEntry::Snapshot(s) = &mut c.entries_mut()[last] {
        s.signature[0] ^= 1;
    }
    let r = c.recover(&Ring).unwrap();
    assert_eq!(r.refused, 1);
    assert_eq!(r.from_snapshot, Some(c.snapshots()[0].snapshot.through_seq), "used the older snapshot");
    assert_eq!(r.ledger.snapshot(SCOPE), full(&c).snapshot(SCOPE));

    // Forge the older one as well: nothing pruned, so a full replay still works.
    let first = c.entries().iter().position(|e| matches!(e, ChainEntry::Snapshot(_))).unwrap();
    if let ChainEntry::Snapshot(s) = &mut c.entries_mut()[first] {
        s.snapshot.state.clear(); // edited state: hash no longer matches
    }
    let r = c.recover(&Ring).unwrap();
    assert_eq!(r.from_snapshot, None);
    assert_eq!(r.refused, 2);
    assert_eq!(r.ledger.snapshot(SCOPE), full(&c).snapshot(SCOPE));
}

fn feed_more(c: &mut LoopbackChain, first_seq: u64, n: u64) {
    for i in 0..n {
        c.append(GrantEvent::pairing(first_seq + i, MESH, &format!("x{first_seq}-{i}")), 1_500 + i).unwrap();
    }
}

#[test]
fn prune_is_refused_until_the_archive_reproduces_the_snapshot() {
    let mut c = chain();
    feed(&mut c, 10, 1_000);
    assert_eq!(c.prune(&Ring).unwrap_err(), ChainError::NoUsableSnapshot, "no snapshot yet");
    c.snapshot_now(2_000).unwrap();
    assert_eq!(c.prune(&Ring).unwrap_err(), ChainError::NotArchived, "snapshot but no archive");

    c.archive(&Ring).unwrap();
    // A damaged archive no longer reproduces the snapshot: prune must refuse.
    c.archive_mut().pop();
    assert_eq!(c.prune(&Ring).unwrap_err(), ChainError::NotArchived);
}

#[test]
fn archive_then_prune_keeps_recovery_identical_and_shrinks_the_chain() {
    let mut c = chain();
    feed(&mut c, 30, 1_000);
    let reference = full(&c).snapshot(SCOPE);
    c.snapshot_now(2_000).unwrap();
    feed_more(&mut c, 1_000, 3);
    let reference_after = full(&c).snapshot(SCOPE);
    let n_before = c.entries().len();

    assert!(c.archive(&Ring).unwrap() > 0);
    let removed = c.prune(&Ring).unwrap();
    assert!(removed > 0 && c.entries().len() == n_before - removed);

    let r = c.recover(&Ring).unwrap();
    assert_eq!(r.tail_len, 3);
    assert_eq!(r.ledger.snapshot(SCOPE), reference_after);
    assert_ne!(reference, reference_after);

    // The archive plus the retained tail still tells the whole history.
    let mut history = c.archive_mut().clone();
    history.extend(c.entries().iter().filter_map(|e| if let ChainEntry::Grant(g) = e { Some(g.clone()) } else { None }));
    assert_eq!(GrantLedger::replay(history).snapshot(SCOPE), reference_after);
}

#[test]
fn after_a_prune_a_forged_snapshot_is_an_error_not_a_silent_empty_ledger() {
    let mut c = chain();
    feed(&mut c, 12, 1_000);
    c.snapshot_now(2_000).unwrap();
    c.archive(&Ring).unwrap();
    c.prune(&Ring).unwrap();
    let i = c.entries().iter().position(|e| matches!(e, ChainEntry::Snapshot(_))).unwrap();
    if let ChainEntry::Snapshot(s) = &mut c.entries_mut()[i] {
        s.signature[0] ^= 1;
    }
    assert_eq!(c.recover(&Ring).err(), Some(ChainError::NoUsableSnapshot));
}

#[test]
fn a_revoke_inside_the_snapshot_window_survives_compaction() {
    let mut c = chain();
    c.append(GrantEvent::pairing(1, MESH, "a"), 1).unwrap();
    c.append(
        GrantEvent { seq: 2, principal: "a".into(), store: node_store_name(MESH, "a"), access: Access::Write, op: GrantOp::Revoke },
        2,
    )
    .unwrap();
    c.snapshot_now(3).unwrap();
    c.archive(&Ring).unwrap();
    c.prune(&Ring).unwrap();
    let r = c.recover(&Ring).unwrap();
    assert!(!r.ledger.allows("a", Access::Write), "a revoked node must not come back");
}

#[test]
fn after_a_prune_an_older_snapshot_cannot_stand_in_for_a_forged_newer_one() {
    // The older snapshot's tail is gone from the chain, so using it would
    // silently drop every event between the two.
    let mut c = chain();
    feed(&mut c, 6, 1_000);
    c.snapshot_now(2_000).unwrap();
    feed_more(&mut c, 500, 4);
    c.snapshot_now(3_000).unwrap();
    c.archive(&Ring).unwrap();
    c.prune(&Ring).unwrap();
    let last = c.entries().iter().rposition(|e| matches!(e, ChainEntry::Snapshot(_))).unwrap();
    if let ChainEntry::Snapshot(s) = &mut c.entries_mut()[last] {
        s.signature[0] ^= 1;
    }
    assert_eq!(c.recover(&Ring).err(), Some(ChainError::NoUsableSnapshot));
}

#[test]
fn archive_refuses_events_that_do_not_reproduce_the_signed_snapshot() {
    let mut c = chain();
    feed(&mut c, 6, 1_000);
    // Rewrite one retained event after the fact, then snapshot the honest ledger.
    let i = c.entries().iter().position(|e| matches!(e, ChainEntry::Grant(_))).unwrap();
    if let ChainEntry::Grant(g) = &mut c.entries_mut()[i] {
        g.principal = "mallory".into();
    }
    c.snapshot_now(2_000).unwrap();
    assert!(matches!(c.archive(&Ring), Err(ChainError::Snapshot(_))));
    assert_eq!(c.prune(&Ring).unwrap_err(), ChainError::NotArchived);
}

/// S1's `StoreName::Node` (weftos-store, name.rs): bucket `n-<id>`, id is
/// lowercase `[a-z0-9-]` and at most 61 characters, bucket at most 63. This
/// crate's rule must stay a subset, so every id it accepts maps to the bucket
/// S1 would derive. S1 is not a dependency; the rule is restated here.
#[test]
fn node_bucket_agrees_with_s1s_store_name_rule_and_is_never_looser() {
    use crate::policy::{node_bucket, validate_node_id};
    let s1_accepts = |id: &str| {
        !id.is_empty() && id.len() <= 61 && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    };
    for id in ["a", "node-1", "0abc", "a-b-c", &"a".repeat(48), &"9".repeat(48)] {
        assert!(s1_accepts(id));
        let b = node_bucket(id).unwrap();
        assert_eq!(b, format!("n-{id}"));
        assert!(b.len() <= 63 && b == b.to_lowercase());
    }
    // Anything this crate accepts, S1 accepts (never looser)...
    for id in ["", "A", "a_b", "a b", "a/b", "-a", "a-", "é", &"a".repeat(49), &"a".repeat(62)] {
        if validate_node_id(id).is_ok() {
            assert!(s1_accepts(id), "{id:?} accepted here but not by S1");
        }
    }
    // ...and the gaps are deliberate: S1 allows edge dashes and 49-61 characters.
    assert!(validate_node_id("-a").is_err() && s1_accepts("-a"));
    assert!(validate_node_id(&"a".repeat(49)).is_err() && s1_accepts(&"a".repeat(49)));
}

// ---- the spike as tested code: tick, compact, cadence property ----

#[test]
fn tick_does_nothing_when_the_schedule_is_off_or_nothing_is_new() {
    let mut off = chain();
    feed(&mut off, 10, 1_000);
    assert!(!off.tick(u64::MAX).unwrap(), "off means off, however late");
    assert_eq!(snapshots(&off), 0);

    let mut c = chain().with_schedule(SnapshotSchedule::every_secs(60));
    assert!(!c.tick(10_000).unwrap(), "empty chain");
    c.append(GrantEvent::pairing(1, MESH, "a"), 1_000).unwrap();
    assert!(!c.tick(1_030).unwrap(), "not yet 60 s");
    assert!(c.tick(1_061).unwrap(), "due by time, no append needed");
    assert!(!c.tick(9_999).unwrap(), "nothing new since the snapshot");
    assert_eq!(snapshots(&c), 1);
    // The clock going backwards is not "due".
    c.append(GrantEvent::pairing(2, MESH, "b"), 2_000).unwrap();
    assert!(!c.tick(1).unwrap());
}

#[test]
fn a_second_snapshot_with_nothing_new_is_the_same_snapshot_not_a_duplicate() {
    let mut c = chain();
    feed(&mut c, 6, 1_000);
    let a = c.snapshot_now(2_000).unwrap();
    let b = c.snapshot_now(3_000).unwrap();
    assert_eq!(a, b);
    assert_eq!(snapshots(&c), 1);
}

#[test]
fn compact_is_never_run_by_the_schedule() {
    let mut c = chain().with_schedule(SnapshotSchedule::every_events(2));
    feed(&mut c, 20, 1_000);
    assert!(snapshots(&c) > 0);
    assert!(c.entries().iter().any(|e| matches!(e, ChainEntry::Grant(g) if g.seq == 1)), "a schedule snapshots; it does not prune");
    assert_eq!(c.archive_mut().len(), 0, "and it does not archive");
}

#[test]
fn compact_snapshots_archives_and_prunes_and_recovery_matches() {
    let mut c = chain();
    feed(&mut c, 30, 1_000);
    let reference = full(&c).snapshot(SCOPE);
    let report = c.compact(&Ring, 2_000).unwrap();
    assert!(report.snapshot_taken && report.archived_through > 0 && report.pruned_entries > 0);
    let r = c.recover(&Ring).unwrap();
    assert_eq!(r.ledger.snapshot(SCOPE), reference);
    assert_eq!(r.tail_len, 0);

    // Again with nothing new: nothing to snapshot, nothing left to prune.
    let again = c.compact(&Ring, 3_000).unwrap();
    assert!(!again.snapshot_taken);
    assert_eq!(again.pruned_entries, 0);
}

#[test]
fn compact_on_an_empty_chain_or_with_a_wrong_verifier_changes_nothing() {
    let mut empty = chain();
    assert!(empty.compact(&Ring, 1).is_err());

    struct Nobody;
    impl SnapshotVerifier for Nobody {
        fn verify(&self, _: &str, _: &[u8], _: &[u8]) -> bool {
            false
        }
    }
    let mut c = chain();
    feed(&mut c, 10, 1_000);
    let grants = |c: &LoopbackChain| c.entries().iter().filter(|e| matches!(e, ChainEntry::Grant(_))).count();
    let n = grants(&c);
    assert!(c.compact(&Nobody, 2_000).is_err(), "a verifier that trusts nobody archives and prunes nothing");
    assert_eq!(c.archive_mut().len(), 0);
    assert_eq!(grants(&c), n, "no grant event was removed");
    assert_eq!(c.recover(&Ring).unwrap().ledger.snapshot(SCOPE), full(&c).snapshot(SCOPE));
}

#[test]
fn compact_survives_a_restart_between_archive_and_prune() {
    // Archive done, prune not yet: the chain is whole and a second compact finishes the job.
    let mut c = chain();
    feed(&mut c, 20, 1_000);
    c.snapshot_now(2_000).unwrap();
    c.archive(&Ring).unwrap();
    let reference = full(&c).snapshot(SCOPE);
    assert_eq!(c.recover(&Ring).unwrap().ledger.snapshot(SCOPE), reference);
    let report = c.compact(&Ring, 3_000).unwrap();
    assert!(report.pruned_entries > 0);
    assert_eq!(c.recover(&Ring).unwrap().ledger.snapshot(SCOPE), reference);
}

#[test]
fn any_cadence_and_any_compaction_point_recovers_the_same_ledger() {
    // Property: for every schedule size and every point we compact at,
    // recovery equals a full replay of everything ever appended.
    let mut all: Vec<GrantEvent> = Vec::new();
    for i in 0..12u64 {
        let id = format!("n{}", i % 5);
        all.push(if i % 3 == 2 { GrantEvent::unpair(i + 1, MESH, &id) } else { GrantEvent::pairing(i + 1, MESH, &id) });
    }
    let want = GrantLedger::replay(all.clone()).snapshot(SCOPE);
    for every in [0u64, 1, 2, 3, 5, 12, 100] {
        for compact_at in 0..=all.len() {
            let mut c = chain().with_schedule(SnapshotSchedule::every_events(every));
            for (i, ev) in all.iter().enumerate() {
                if i == compact_at && i > 0 {
                    let _ = c.compact(&Ring, 5_000 + i as u64);
                }
                c.append(ev.clone(), 1_000 + i as u64).unwrap();
            }
            let got = c.recover(&Ring).unwrap().ledger.snapshot(SCOPE);
            assert_eq!(got, want, "every={every} compact_at={compact_at}");
        }
    }
}
