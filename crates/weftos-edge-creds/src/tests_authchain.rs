//! Chain-authenticated feed for the grant ledger (ADR-126 S3).

use std::sync::Arc;

use crate::authchain::*;
use crate::broker::GrantCheck;
use crate::compaction::SnapshotVerifier;
use crate::grants::{node_store_name, Applied, GrantEvent, GrantLedger, GrantOp, MaxSeqJump};
use crate::policy::Access;
use hmac::{Hmac, Mac};
use sha2::Sha256;

const MESH: &str = "m1";
const SCOPE: &str = "grants/m1";
const KEY: &[u8] = b"chain-signer-key";

fn mac(key: &[u8], msg: &[u8]) -> Vec<u8> {
    let mut m = Hmac::<Sha256>::new_from_slice(key).unwrap();
    m.update(msg);
    m.finalize().into_bytes().to_vec()
}

/// Stand-in for the chain's signer; the real one is the chain owner's.
#[derive(Clone)]
struct Ring;
impl SnapshotVerifier for Ring {
    fn verify(&self, signer: &str, message: &[u8], signature: &[u8]) -> bool {
        signer == "chain-1" && mac(KEY, message) == signature
    }
}

fn feed() -> ChainFeed<Ring> {
    ChainFeed::new(Arc::new(GrantLedger::new()), SCOPE, Ring)
}

/// The chain side: signs each event as following the previous one.
struct Chain {
    shadow: GrantLedger,
}
impl Chain {
    fn new() -> Self {
        Self { shadow: GrantLedger::new() }
    }
    fn emit(&self, ev: GrantEvent) -> ChainGrantEvent {
        let head = self.shadow.snapshot(SCOPE).events_head;
        self.shadow.apply(&ev);
        ChainGrantEvent::sign(SCOPE, ev, &head, "chain-1", |m| mac(KEY, m))
    }
}

fn three() -> Vec<ChainGrantEvent> {
    let c = Chain::new();
    vec![
        c.emit(GrantEvent::pairing(1, MESH, "a")),
        c.emit(GrantEvent::pairing(2, MESH, "b")),
        c.emit(GrantEvent::unpair(3, MESH, "a")),
    ]
}

fn state(f: &ChainFeed<Ring>) -> crate::compaction::Snapshot {
    f.ledger().snapshot(SCOPE)
}

#[test]
fn an_in_order_signed_chain_applies_and_equals_a_plain_replay() {
    let f = feed();
    let evs = three();
    for e in &evs {
        f.ingest(e).unwrap();
    }
    let plain = GrantLedger::replay(evs.iter().map(|e| e.event.clone()));
    assert_eq!(state(&f), plain.snapshot(SCOPE));
    assert!(f.ledger().allows("b", Access::Write) && !f.ledger().allows("a", Access::Write));
}

#[test]
fn a_forged_event_is_refused_and_changes_nothing() {
    let f = feed();
    let evs = three();
    f.ingest(&evs[0]).unwrap();
    let before = state(&f);

    // Edited after signing: grant `mallory` instead of `b`.
    let mut edited = evs[1].clone();
    edited.event.principal = "mallory".into();
    edited.event.store = node_store_name(MESH, "mallory");
    assert_eq!(f.ingest(&edited), Err(FeedError::BadSignature));

    // Signed by a key nobody trusts.
    let forged = ChainGrantEvent::sign(SCOPE, evs[1].event.clone(), &f.head(), "chain-1", |m| mac(b"attacker", m));
    assert_eq!(f.ingest(&forged), Err(FeedError::BadSignature));
    let other = ChainGrantEvent::sign(SCOPE, evs[1].event.clone(), &f.head(), "chain-2", |m| mac(KEY, m));
    assert_eq!(f.ingest(&other), Err(FeedError::BadSignature));

    // Unsigned.
    let mut bare = evs[1].clone();
    bare.signature.clear();
    assert_eq!(f.ingest(&bare), Err(FeedError::BadSignature));

    assert_eq!(state(&f), before, "nothing was applied, not even the head");
    f.ingest(&evs[1]).unwrap();
}

#[test]
fn a_reordered_or_dropped_event_breaks_the_link_and_nothing_after_it_applies() {
    let f = feed();
    let evs = three();
    f.ingest(&evs[0]).unwrap();
    let before = state(&f);
    // The unpair (3) arrives before the pairing (2).
    assert_eq!(f.ingest(&evs[2]), Err(FeedError::BrokenLink(3)));
    assert_eq!(state(&f), before);
    // Delivered in order it works.
    f.ingest(&evs[1]).unwrap();
    f.ingest(&evs[2]).unwrap();

    // A dropped event: event 1 never arrives at a fresh feed.
    let g = feed();
    assert_eq!(g.ingest(&evs[1]), Err(FeedError::BrokenLink(2)));
    assert_eq!(state(&g).through_seq, 0);
}

#[test]
fn a_replayed_old_event_is_dropped_and_cannot_undo_a_later_revoke() {
    let f = feed();
    let evs = three();
    for e in &evs {
        f.ingest(e).unwrap();
    }
    let before = state(&f);
    // Re-delivering the old pairing of `a` (a grant) after its unpair.
    assert_eq!(f.ingest(&evs[0]), Err(FeedError::Replay(1)));
    assert_eq!(f.ingest(&evs[2]), Err(FeedError::Replay(3)), "even the last event twice");
    assert_eq!(state(&f), before);
    assert!(!f.ledger().allows("a", Access::Write));
}

#[test]
fn an_event_signed_for_another_scope_is_refused() {
    let f = feed();
    let c = Chain::new();
    let e = c.emit(GrantEvent::pairing(1, MESH, "a"));
    let elsewhere = ChainGrantEvent::sign("grants/m2", e.event.clone(), &e.prev_head, "chain-1", |m| mac(KEY, m));
    assert!(matches!(f.ingest(&elsewhere), Err(FeedError::Scope { .. })));
    // Relabelling the scope without re-signing fails on the signature.
    let mut relabel = elsewhere.clone();
    relabel.scope = SCOPE.into();
    assert_eq!(f.ingest(&relabel), Err(FeedError::BadSignature));
    assert_eq!(state(&f).through_seq, 0);
}

#[test]
fn the_signature_covers_seq_so_a_far_future_seq_cannot_be_added_by_a_relay() {
    let f = feed();
    let evs = three();
    let mut bumped = evs[0].clone();
    bumped.event.seq = u64::MAX;
    assert_eq!(f.ingest(&bumped), Err(FeedError::BadSignature));
    assert_eq!(state(&f).through_seq, 0, "the ledger is not frozen");
    f.ingest(&evs[0]).unwrap();
}

#[test]
fn a_correctly_signed_far_future_seq_is_still_stopped_by_the_seq_guard() {
    let f = feed();
    f.ledger().set_seq_guard(MaxSeqJump(1_000));
    let c = Chain::new();
    f.ingest(&c.emit(GrantEvent::pairing(1, MESH, "a"))).unwrap();
    let wild = c.emit(GrantEvent { seq: u64::MAX, principal: "a".into(), store: node_store_name(MESH, "a"), access: Access::Read, op: GrantOp::Grant });
    assert_eq!(f.ingest(&wild), Err(FeedError::OutOfRange(u64::MAX)));
    // The shadow chain moved on; a feed that refused the wild one cannot follow
    // it, which is the point: an operator must look. A fresh in-range event on
    // the feed's own head still works.
    let ok = ChainGrantEvent::sign(SCOPE, GrantEvent::unpair(5, MESH, "a"), &f.head(), "chain-1", |m| mac(KEY, m));
    assert!(matches!(f.ingest(&ok), Ok(Applied::Changed { lost_write: true })));
}

#[test]
fn the_link_survives_a_snapshot_restore() {
    let f = feed();
    let evs = three();
    f.ingest(&evs[0]).unwrap();
    f.ingest(&evs[1]).unwrap();
    let signed = f.ledger().signed_snapshot(SCOPE, "chain-1", |m| mac(KEY, m));
    let restored = GrantLedger::restore(&signed, SCOPE, &Ring, &[]).unwrap();
    let g = ChainFeed::new(Arc::new(restored), SCOPE, Ring);
    g.ingest(&evs[2]).unwrap();
    assert!(!g.ledger().allows("a", Access::Write) && g.ledger().allows("b", Access::Write));
}

#[test]
fn two_threads_feeding_the_same_chain_apply_each_event_once() {
    let f = Arc::new(feed());
    let evs = three();
    let results: Vec<_> = (0..4)
        .map(|_| {
            let (f, evs) = (f.clone(), evs.clone());
            std::thread::spawn(move || evs.iter().filter(|e| f.ingest(e).is_ok()).count())
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|h| h.join().unwrap())
        .collect();
    assert_eq!(results.iter().sum::<usize>(), 3);
    assert_eq!(state(&f).through_seq, 3);
}

#[test]
fn rewriting_prev_head_to_skip_an_event_breaks_the_signature() {
    // A relay wants to drop event 2 (a revoke, say) and splice 3 after 1.
    let f = feed();
    let evs = three();
    f.ingest(&evs[0]).unwrap();
    let mut spliced = evs[2].clone();
    spliced.prev_head = f.head();
    assert_eq!(f.ingest(&spliced), Err(FeedError::BadSignature));
    assert_eq!(state(&f).through_seq, 1);
}

// ---- who may sign: any other node or person (owner decision 2026-11-08 11:21) ----

/// A verifier that trusts several identities, each with the same stand-in key.
#[derive(Clone)]
struct Many;
impl SnapshotVerifier for Many {
    fn verify(&self, signer: &str, message: &[u8], signature: &[u8]) -> bool {
        ["person:ann", "node:b", "node:a", "a"].contains(&signer) && mac(KEY, message) == signature
    }
}

fn signed_by(signer: &str, ev: GrantEvent, head: &str) -> ChainGrantEvent {
    ChainGrantEvent::sign(SCOPE, ev, head, signer, |m| mac(KEY, m))
}

fn many() -> ChainFeed<Many> {
    ChainFeed::new(Arc::new(GrantLedger::new()), SCOPE, Many)
}

#[test]
fn a_person_or_another_node_may_sign_a_grant() {
    let f = many();
    f.ingest(&signed_by("person:ann", GrantEvent::pairing(1, MESH, "a"), &f.head())).unwrap();
    f.ingest(&signed_by("node:b", GrantEvent::pairing(2, MESH, "c"), &f.head())).unwrap();
    assert!(f.ledger().allows("a", Access::Write) && f.ledger().allows("c", Access::Write));
}

#[test]
fn nobody_may_sign_a_grant_of_access_to_themselves() {
    let f = many();
    for (i, signer) in ["node:a", "a"].into_iter().enumerate() {
        let ev = signed_by(signer, GrantEvent::pairing(1 + i as u64, MESH, "a"), &f.head());
        assert_eq!(f.ingest(&ev), Err(FeedError::SelfGrant(1 + i as u64)), "{signer}");
    }
    assert!(!f.ledger().allows("a", Access::Write), "a refused self-grant changes nothing");
    assert_eq!(f.head(), many().head());
    // Another identity can still grant it.
    f.ingest(&signed_by("node:b", GrantEvent::pairing(1, MESH, "a"), &f.head())).unwrap();
    assert!(f.ledger().allows("a", Access::Write));
}

#[test]
fn signing_your_own_revoke_is_allowed_because_it_gives_you_nothing() {
    let f = many();
    f.ingest(&signed_by("person:ann", GrantEvent::pairing(1, MESH, "a"), &f.head())).unwrap();
    f.ingest(&signed_by("node:a", GrantEvent::unpair(2, MESH, "a"), &f.head())).unwrap();
    assert!(!f.ledger().allows("a", Access::Write));
}

#[test]
fn the_anyone_rule_turns_the_self_grant_check_off() {
    let f = many().with_rule(SignerRule::Anyone);
    f.ingest(&signed_by("node:a", GrantEvent::pairing(1, MESH, "a"), &f.head())).unwrap();
    assert!(f.ledger().allows("a", Access::Write));
}

#[test]
fn an_identity_the_verifier_does_not_trust_still_cannot_sign() {
    let f = many();
    let ev = signed_by("person:mallory", GrantEvent::pairing(1, MESH, "a"), &f.head());
    assert_eq!(f.ingest(&ev), Err(FeedError::BadSignature));
}
