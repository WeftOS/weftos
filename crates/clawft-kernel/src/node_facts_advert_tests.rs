//! Signed node facts: tampered, foreign, expired and replayed blocks are refused.

use super::*;
use clawft_types::placement::{Capability, CapabilityId, CapabilityState, Provenance, StateChange};
use rand::rngs::OsRng;

fn key() -> SigningKey {
    SigningKey::generate(&mut OsRng)
}

fn facts_for(k: &SigningKey, issued_at: u64) -> NodeFacts {
    let id = node_id_from_pubkey(&k.verifying_key().to_bytes());
    let mut f = NodeFacts::new(id, issued_at, 600, 1);
    f.capabilities = vec![
        Capability::new(
            CapabilityId::new("cpu.arch.aarch64").unwrap(),
            Provenance::Probed,
        ),
        Capability::new(
            CapabilityId::new("accel.gpu.metal").unwrap(),
            Provenance::Probed,
        )
        .with_attr("unified", true)
        .with_attr("cores", 40i64),
    ];
    f
}

#[test]
fn sign_then_verify_roundtrips_through_json() {
    let k = key();
    let f = facts_for(&k, 1_000);
    let s = sign_node_facts(&f, &k).unwrap();
    let wire = serde_json::to_string(&s).unwrap();
    let back: SignedNodeFacts = serde_json::from_str(&wire).unwrap();
    assert_eq!(verify_node_facts(&back, 1_100).unwrap(), f);
}

#[test]
fn tampered_payload_is_rejected() {
    let k = key();
    let mut s = sign_node_facts(&facts_for(&k, 1_000), &k).unwrap();
    s.payload = s.payload.replace("\"cores\":40", "\"cores\":80");
    assert_eq!(
        verify_node_facts(&s, 1_100),
        Err(NodeFactsAdvertError::BadSignature)
    );
}

#[test]
fn facts_resigned_by_another_key_cannot_claim_the_node() {
    let (k, other) = (key(), key());
    let f = facts_for(&k, 1_000);
    // A different key cannot sign facts naming k's node id.
    assert!(matches!(
        sign_node_facts(&f, &other),
        Err(NodeFactsAdvertError::NodeMismatch { .. })
    ));
    // Nor can it swap its key into k's envelope.
    let mut s = sign_node_facts(&f, &k).unwrap();
    s.public_key = other.verifying_key().to_bytes().to_vec();
    assert_eq!(
        verify_node_facts(&s, 1_100),
        Err(NodeFactsAdvertError::BadSignature)
    );
}

#[test]
fn expired_and_future_facts_are_rejected() {
    let k = key();
    let s = sign_node_facts(&facts_for(&k, 1_000), &k).unwrap();
    assert!(matches!(
        verify_node_facts(&s, 1_600),
        Err(NodeFactsAdvertError::Facts(FactsError::Expired { .. }))
    ));
    assert!(matches!(
        verify_node_facts(&s, 900),
        Err(NodeFactsAdvertError::Facts(FactsError::FromFuture { .. }))
    ));
    assert!(verify_node_facts(&s, 1_599).is_ok());
}

#[test]
fn a_delta_signature_cannot_be_replayed_as_facts() {
    let k = key();
    let f = facts_for(&k, 1_000);
    let s = sign_node_facts(&f, &k).unwrap();
    // Sign the same bytes under the delta domain and present them as facts.
    let forged_sig = k.sign(&signed_bytes(DELTA_DOMAIN, &s.payload));
    let forged = SignedNodeFacts {
        signature: forged_sig.to_bytes().to_vec(),
        ..s
    };
    assert_eq!(
        verify_node_facts(&forged, 1_100),
        Err(NodeFactsAdvertError::BadSignature)
    );
}

#[test]
fn malformed_envelopes_are_rejected() {
    let k = key();
    let mut s = sign_node_facts(&facts_for(&k, 1_000), &k).unwrap();
    s.signature.pop();
    assert_eq!(
        verify_node_facts(&s, 1_100),
        Err(NodeFactsAdvertError::BadSignatureLen(63))
    );
    let mut s = sign_node_facts(&facts_for(&k, 1_000), &k).unwrap();
    s.public_key = vec![0; 31];
    assert_eq!(
        verify_node_facts(&s, 1_100),
        Err(NodeFactsAdvertError::BadPublicKey)
    );
    let mut s = sign_node_facts(&facts_for(&k, 1_000), &k).unwrap();
    s.payload = "x".repeat(MAX_PAYLOAD_BYTES + 1);
    assert!(matches!(
        verify_node_facts(&s, 1_100),
        Err(NodeFactsAdvertError::TooLarge(_))
    ));
}

#[test]
fn delta_sign_verify_and_tamper() {
    let k = key();
    let f = facts_for(&k, 1_000);
    let d = FactsDelta {
        node_id: f.node_id.clone(),
        base_seq: 1,
        seq: 1,
        issued_at: 1_010,
        changes: vec![StateChange {
            index: 1,
            id: CapabilityId::new("accel.gpu.metal").unwrap(),
            state: CapabilityState::Busy,
        }],
        mem_free: Some(1),
    };
    let s = sign_facts_delta(&d, &k).unwrap();
    let (back, pk) = verify_facts_delta(&s, 1_020).unwrap();
    assert_eq!(back, d);
    assert_eq!(pk, k.verifying_key().to_bytes());
    let mut t = s.clone();
    t.payload = t.payload.replace("busy", "available");
    assert_eq!(
        verify_facts_delta(&t, 1_020),
        Err(NodeFactsAdvertError::BadSignature)
    );
    assert!(verify_facts_delta(&s, 900).is_err(), "future-dated delta");
}

/// The identity point is a small-order key: with R = identity and S = 0 the
/// cofactorless check accepts a "signature" over any message. A peer that
/// presents such a key must be refused (verify_strict).
#[test]
fn small_order_key_with_a_trivial_signature_is_refused() {
    let mut pk = [0u8; 32];
    pk[0] = 1; // identity point
    let mut sig = vec![0u8; 64];
    sig[0] = 1; // R = identity, S = 0
    let mut f = NodeFacts::new(node_id_from_pubkey(&pk), 1_000, 600, 1);
    f.capabilities = vec![Capability::new(CapabilityId::new("cpu.arch.aarch64").unwrap(), Provenance::Probed)];
    let signed = SignedNodeFacts {
        payload: serde_json::to_string(&f).unwrap(),
        public_key: pk.to_vec(),
        signature: sig,
    };
    assert_eq!(verify_node_facts(&signed, 1_100), Err(NodeFactsAdvertError::BadSignature));
}
