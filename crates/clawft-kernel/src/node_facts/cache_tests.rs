//! TTL cache: only verified, fresh, newer facts are held; deltas must come
//! from the same key and move forward.

use super::*;
use crate::node_facts_advert::{sign_facts_delta, sign_node_facts};
use crate::node_registry::node_id_from_pubkey;
use clawft_types::placement::{CapabilityId, CapabilityState, FactsDelta, Provenance, StateChange};
use ed25519_dalek::SigningKey;
use rand::rngs::OsRng;

fn key() -> SigningKey {
    SigningKey::generate(&mut OsRng)
}

fn facts(k: &SigningKey, seq: u64, issued_at: u64) -> NodeFacts {
    let id = node_id_from_pubkey(&k.verifying_key().to_bytes());
    let mut f = NodeFacts::new(id, issued_at, 600, seq);
    f.capabilities = vec![
        Capability::new(
            CapabilityId::new("accel.tpu.coral").unwrap(),
            Provenance::Probed,
        )
        .exclusive(),
        Capability::new(CapabilityId::new("mem.system").unwrap(), Provenance::Probed)
            .with_attr("total", 8_000i64)
            .with_attr("free", 4_000i64),
    ];
    f
}

fn signed(k: &SigningKey, seq: u64, issued_at: u64) -> SignedNodeFacts {
    sign_node_facts(&facts(k, seq, issued_at), k).unwrap()
}

fn delta(k: &SigningKey, base_seq: u64, seq: u64, state: CapabilityState) -> SignedFactsDelta {
    let d = FactsDelta {
        node_id: node_id_from_pubkey(&k.verifying_key().to_bytes()),
        base_seq,
        seq,
        issued_at: 1_010,
        changes: vec![StateChange {
            index: 0,
            id: CapabilityId::new("accel.tpu.coral").unwrap(),
            state,
        }],
        mem_free: Some(1_000),
    };
    sign_facts_delta(&d, k).unwrap()
}

#[test]
fn verified_facts_are_served_until_ttl_then_hidden_and_evicted() {
    let (c, k) = (NodeFactsCache::new(), key());
    assert_eq!(
        c.insert(signed(&k, 1, 1_000), TrustTier::Paired, 1_000),
        Ok(InsertOutcome::Added)
    );
    let id = node_id_from_pubkey(&k.verifying_key().to_bytes());
    let got = c.get(&id, 1_599).unwrap();
    assert_eq!(got.trust_tier(), TrustTier::Paired);
    assert_eq!(got.capabilities().len(), 2);
    assert!(
        c.get(&id, 1_600).is_none(),
        "expired facts must not be served"
    );
    assert!(c.list(1_600).is_empty());
    assert_eq!(c.evict_expired(1_600), 1);
    assert!(c.is_empty());
}

#[test]
fn tampered_or_expired_facts_never_enter() {
    let (c, k) = (NodeFactsCache::new(), key());
    let mut s = signed(&k, 1, 1_000);
    s.payload = s.payload.replace("8000", "9000");
    assert!(matches!(
        c.insert(s, TrustTier::Paired, 1_000),
        Err(CacheError::Verify(NodeFactsAdvertError::BadSignature))
    ));
    assert!(matches!(
        c.insert(signed(&k, 1, 1_000), TrustTier::Paired, 2_000),
        Err(CacheError::Verify(NodeFactsAdvertError::Facts(
            FactsError::Expired { .. }
        )))
    ));
    assert!(c.is_empty());
}

#[test]
fn older_seq_cannot_replace_newer_facts() {
    let (c, k) = (NodeFactsCache::new(), key());
    c.insert(signed(&k, 5, 1_000), TrustTier::Paired, 1_000)
        .unwrap();
    assert_eq!(
        c.insert(signed(&k, 5, 1_000), TrustTier::Paired, 1_001),
        Ok(InsertOutcome::Unchanged)
    );
    assert!(matches!(
        c.insert(signed(&k, 4, 1_050), TrustTier::Paired, 1_060),
        Err(CacheError::Stale {
            got: 4,
            held: 5,
            ..
        })
    ));
    assert_eq!(
        c.insert(signed(&k, 6, 1_050), TrustTier::Paired, 1_060),
        Ok(InsertOutcome::Replaced)
    );
}

#[test]
fn deltas_update_state_in_order_and_only_from_the_node_key() {
    let (c, k, other) = (NodeFactsCache::new(), key(), key());
    c.insert(signed(&k, 3, 1_000), TrustTier::Paired, 1_000)
        .unwrap();
    let id = node_id_from_pubkey(&k.verifying_key().to_bytes());

    c.apply_delta(&delta(&k, 3, 1, CapabilityState::Busy), 1_020)
        .unwrap();
    let got = c.get(&id, 1_020).unwrap();
    assert_eq!(got.capabilities()[0].state, CapabilityState::Busy);
    assert_eq!(got.load().mem_free, Some(1_000));
    assert_eq!(got.load().busy, 1);

    // Replay of the same delta seq is refused.
    assert!(
        c.apply_delta(&delta(&k, 3, 1, CapabilityState::Available), 1_021)
            .is_err()
    );
    // Wrong base seq is refused.
    assert!(
        c.apply_delta(&delta(&k, 2, 2, CapabilityState::Available), 1_021)
            .is_err()
    );
    // Another node's key cannot update this node (its delta names its own id).
    assert!(matches!(
        c.apply_delta(&delta(&other, 3, 9, CapabilityState::Available), 1_021),
        Err(CacheError::UnknownNode(_))
    ));
    assert_eq!(
        c.get(&id, 1_021).unwrap().capabilities()[0].state,
        CapabilityState::Busy
    );

    // After expiry a delta has no base.
    assert!(matches!(
        c.apply_delta(&delta(&k, 3, 2, CapabilityState::Available), 1_700),
        Err(CacheError::UnknownNode(_))
    ));
}

#[test]
fn an_exact_delta_replay_is_a_benign_duplicate_but_conflicts_and_older_still_refuse() {
    let (c, k) = (NodeFactsCache::new(), key());
    c.insert(signed(&k, 3, 1_000), TrustTier::Paired, 1_000)
        .unwrap();
    let id = node_id_from_pubkey(&k.verifying_key().to_bytes());
    let busy = delta(&k, 3, 2, CapabilityState::Busy);
    c.apply_delta(&busy, 1_020).unwrap();

    // Exact replay: distinct, benign error; state untouched.
    assert_eq!(
        c.apply_delta(&busy, 1_021),
        Err(CacheError::DuplicateDelta { node_id: id.clone(), seq: 2 })
    );
    assert_eq!(c.get(&id, 1_021).unwrap().delta_seq, 2);

    // Same seq, different content: still a refusal, not a duplicate.
    let err = c
        .apply_delta(&delta(&k, 3, 2, CapabilityState::Available), 1_021)
        .unwrap_err();
    assert!(!matches!(err, CacheError::DuplicateDelta { .. }), "{err:?}");
    // Older seq: refused.
    let err = c
        .apply_delta(&delta(&k, 3, 1, CapabilityState::Available), 1_021)
        .unwrap_err();
    assert!(!matches!(err, CacheError::DuplicateDelta { .. }), "{err:?}");
    assert_eq!(
        c.get(&id, 1_021).unwrap().capabilities()[0].state,
        CapabilityState::Busy
    );
}

#[test]
fn a_rebased_sender_restarting_its_delta_seq_is_not_judged_against_the_old_base() {
    let (c, k) = (NodeFactsCache::new(), key());
    c.insert(signed(&k, 3, 1_000), TrustTier::Paired, 1_000).unwrap();
    let id = node_id_from_pubkey(&k.verifying_key().to_bytes());
    c.apply_delta(&delta(&k, 3, 1, CapabilityState::Busy), 1_020).unwrap();

    // The sender re-based (base 7) and its delta restarts at seq 1, but the
    // new base has not arrived: not a regression, ask for the base.
    let d = delta(&k, 7, 1, CapabilityState::Available);
    assert_eq!(
        c.apply_delta(&d, 1_021),
        Err(CacheError::BaseBehind { node_id: id.clone(), held: 3, need: 7 })
    );
    // Once the new base lands, the same delta applies.
    c.insert(signed(&k, 7, 1_030), TrustTier::Paired, 1_030).unwrap();
    c.apply_delta(&d, 1_031).unwrap();
    assert_eq!(c.get(&id, 1_031).unwrap().delta_seq, 1);

    // A delta for an older base than held is a genuine regression.
    let err = c.apply_delta(&delta(&k, 3, 9, CapabilityState::Busy), 1_032).unwrap_err();
    assert!(matches!(err, CacheError::Verify(_)), "{err:?}");
}

#[test]
fn an_operator_tier_change_applies_even_when_the_facts_are_unchanged() {
    let (c, k) = (NodeFactsCache::new(), key());
    let s = signed(&k, 1, 1_000);
    c.insert(s.clone(), TrustTier::Paired, 1_000).unwrap();
    let id = node_id_from_pubkey(&k.verifying_key().to_bytes());
    // Re-inserting the same facts is `Unchanged` and keeps the held tier ...
    assert_eq!(
        c.insert(s, TrustTier::Discovered, 1_001),
        Ok(InsertOutcome::Unchanged)
    );
    assert_eq!(c.get(&id, 1_001).unwrap().trust_tier(), TrustTier::Paired);
    // ... so a demotion must be applied explicitly.
    assert!(c.set_trust_tier(&id, TrustTier::Discovered));
    assert_eq!(
        c.get(&id, 1_001).unwrap().trust_tier(),
        TrustTier::Discovered
    );
    assert!(!c.set_trust_tier("n-unknown", TrustTier::Pinned));
}

/// The staleness check and the write are one step: however inserts for one
/// node interleave, the highest `seq` is what stays held, and every `Ok`
/// except the last winner's had a lower seq than something that followed.
#[test]
fn concurrent_inserts_for_one_node_never_leave_older_facts_held() {
    use std::sync::Arc;
    const TOP: u64 = 300;
    let k = key();
    let id = node_id_from_pubkey(&k.verifying_key().to_bytes());
    let blocks: Arc<Vec<SignedNodeFacts>> =
        Arc::new((1..=TOP).map(|s| signed(&k, s, 1_000)).collect());
    for round in 0..30 {
        let c = Arc::new(NodeFactsCache::new());
        let threads: Vec<_> = (0..16usize)
            .map(|t| {
                let (c, blocks) = (c.clone(), blocks.clone());
                std::thread::spawn(move || {
                    // Each thread walks the seqs in its own order.
                    let n = blocks.len();
                    for i in 0..n {
                        let idx = if t % 2 == 0 { i } else { n - 1 - i };
                        let r = c.insert(blocks[idx].clone(), TrustTier::Paired, 1_000);
                        assert!(
                            matches!(r, Ok(_) | Err(CacheError::Stale { .. })),
                            "unexpected {r:?}"
                        );
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(c.get(&id, 1_000).unwrap().facts.seq, TOP, "round {round}");
    }
}

/// Forces the interleave the stress test above only hopes for: insert `seq 5`
/// passes its staleness check and stops; `seq 6` is inserted meanwhile; then
/// `seq 5` resumes. With the check and the write under one lock, 6 waits for 5
/// and stays held; with a separate check and write, 5 would overwrite 6.
#[test]
fn an_older_insert_stopped_between_check_and_write_cannot_overwrite_a_newer_one() {
    use super::pause::{PAUSED, RELEASE, TARGET};
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    let c = Arc::new(NodeFactsCache::new());
    let k = key();
    let id = node_id_from_pubkey(&k.verifying_key().to_bytes());
    c.insert(signed(&k, 4, 1_000), TrustTier::Paired, 1_000).unwrap();
    *TARGET.lock().unwrap() = Some((id.clone(), 5));
    let (s5, s6) = (signed(&k, 5, 1_000), signed(&k, 6, 1_000));
    let (c5, c6) = (c.clone(), c.clone());
    let a = std::thread::spawn(move || c5.insert(s5, TrustTier::Paired, 1_000));
    for _ in 0..5_000 {
        if PAUSED.load(Ordering::SeqCst) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert!(PAUSED.load(Ordering::SeqCst), "insert 5 never reached the pause point");
    let b = std::thread::spawn(move || c6.insert(s6, TrustTier::Paired, 1_000));
    // Under the lock, B is now waiting for A. Without it, B has already written.
    std::thread::sleep(std::time::Duration::from_millis(200));
    RELEASE.store(true, Ordering::SeqCst);
    a.join().unwrap().ok();
    b.join().unwrap().unwrap();
    *TARGET.lock().unwrap() = None;
    assert_eq!(c.get(&id, 1_000).unwrap().facts.seq, 6, "an older seq must not land last");
}
