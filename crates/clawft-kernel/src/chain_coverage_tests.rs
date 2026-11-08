//! `chain.get` and signature coverage (ADR-129).

use super::*;
use crate::chain::ChainManager;
use ed25519_dalek::SigningKey;
use rand::rngs::OsRng;
use rvf_crypto::hash::shake256_256;
use serde_json::json;

fn dual() -> ChainManager {
    let mut cm = ChainManager::new(3, 0).with_signing_key(SigningKey::generate(&mut OsRng));
    cm.set_ml_dsa_key(mldsa65::SigningKey::generate());
    cm
}

fn get(cm: &ChainManager, seq: u64) -> ChainEventDetail {
    event_detail(cm, seq, true).expect("event exists")
}

#[test]
fn get_returns_the_exact_appended_payload() {
    let cm = dual();
    let payload = json!({"digest": "ab".repeat(32), "nested": {"n": [1, 2, 3]}, "s": "caf\u{e9}"});
    let ev = cm.append("journal", "turn.digest", Some(payload.clone()));
    let d = get(&cm, ev.sequence);
    assert_eq!(d.payload, Some(payload.clone()));
    // The canonical text is what the payload hash covers.
    let canon = d.payload_canonical.clone().unwrap();
    assert_eq!(shake256_256(canon.as_bytes()), ev.payload_hash);
    assert_eq!(serde_json::from_str::<serde_json::Value>(&canon).unwrap(), payload);
    assert_eq!((d.source.as_str(), d.kind.as_str()), ("journal", "turn.digest"));
    assert_eq!(d.hash, hex(&ev.hash));
    assert_eq!(d.prev_hash, hex(&ev.prev_hash));
    assert_eq!(d.payload_hash, hex(&ev.payload_hash));
    assert!(d.hash_valid && d.payload_hash_valid);
    assert!(d.rule_hash.is_none());
}

#[test]
fn get_of_a_missing_sequence_is_none_not_an_empty_event() {
    let cm = dual();
    assert!(event_detail(&cm, 99, true).is_none());
}

#[test]
fn coverage_is_pending_before_a_checkpoint_and_signed_after() {
    let dir = tempfile::tempdir().unwrap();
    let cm = dual();
    let e = cm.append("journal", "turn.digest", Some(json!({"d": 1})));
    let before = get(&cm, e.sequence).coverage;
    assert_eq!(before.status, CoverageStatus::UnsignedPendingCheckpoint);
    assert_eq!(before.reason, "unsigned: pending checkpoint");
    assert!(before.checkpoint.is_none());
    assert!(signature_summary(&cm).is_none());

    cm.save_to_rvf(&dir.path().join("c.rvf")).unwrap();
    let after = get(&cm, e.sequence).coverage;
    assert_eq!(after.status, CoverageStatus::Signed, "{}", after.reason);
    let cp = after.checkpoint.unwrap();
    assert_eq!(cp.range_start, 0);
    assert!(cp.range_end >= e.sequence);
    assert!(cp.ed25519_valid && cp.chain_linked);
    assert_eq!(cp.sig_suite, "ed25519+ml-dsa-65/v2");
    assert_eq!(cp.ed25519_signature.as_ref().unwrap().len(), 128);
    assert_eq!(cp.ml_dsa65_signature.as_ref().unwrap().len(), mldsa65::SIGNATURE_LEN * 2);
    assert_eq!(cp.ed25519_key_id, cm.verifying_key().map(|k| hex(k.as_bytes())));
    assert_eq!(cp.ml_dsa65_key_id, cm.ml_dsa_verifying_key().map(|k| k.key_id()));
    let sum = signature_summary(&cm).unwrap();
    assert!(sum.valid && sum.pq_verified == Some(true) && sum.signed_through == cp.range_end);

    // An event appended after the checkpoint is not covered: say so.
    let later = cm.append("journal", "turn.digest", Some(json!({"d": 2})));
    let c = get(&cm, later.sequence).coverage;
    assert_eq!(c.status, CoverageStatus::UnsignedPendingCheckpoint);
    assert!(c.reason.contains("latest signed checkpoint ends at sequence"), "{}", c.reason);
    // ...and the earlier one stays signed.
    assert_eq!(get(&cm, e.sequence).coverage.status, CoverageStatus::Signed);

    // `signatures: false` leaves the bulky signatures out but keeps the verdicts.
    let lean = event_detail(&cm, e.sequence, false).unwrap().coverage.checkpoint.unwrap();
    assert!(lean.ed25519_signature.is_none() && lean.ml_dsa65_signature.is_none());
    assert!(lean.ed25519_valid && lean.ml_dsa65_valid == Some(true));
}

#[test]
fn a_chain_without_a_signing_key_says_why_it_is_unsigned() {
    let dir = tempfile::tempdir().unwrap();
    let cm = ChainManager::new(0, 0);
    let e = cm.append("journal", "turn.digest", None);
    cm.save_to_rvf(&dir.path().join("c.rvf")).unwrap();
    let c = get(&cm, e.sequence).coverage;
    assert_eq!(c.status, CoverageStatus::UnsignedPendingCheckpoint);
    assert!(c.reason.contains("no signing key"), "{}", c.reason);
}

#[test]
fn tampering_with_a_stored_payload_is_detected() {
    let dir = tempfile::tempdir().unwrap();
    let cm = dual();
    let e = cm.append("journal", "turn.digest", Some(json!({"d": "original"})));
    cm.append("journal", "turn.digest", Some(json!({"d": "next"})));
    cm.save_to_rvf(&dir.path().join("c.rvf")).unwrap();
    assert_eq!(get(&cm, e.sequence).coverage.status, CoverageStatus::Signed);

    cm.tamper_for_test(e.sequence, |ev| ev.payload = Some(json!({"d": "forged"})));
    let d = get(&cm, e.sequence);
    assert!(!d.payload_hash_valid);
    assert_eq!(d.coverage.status, CoverageStatus::SignatureInvalid);
    assert!(!d.coverage.checkpoint.unwrap().chain_linked);
    assert!(!signature_summary(&cm).unwrap().valid);
}

#[test]
fn a_forged_event_with_recomputed_hashes_still_breaks_the_signed_link() {
    let dir = tempfile::tempdir().unwrap();
    let cm = dual();
    let e = cm.append("journal", "turn.digest", Some(json!({"d": "original"})));
    cm.append("journal", "turn.digest", Some(json!({"d": "next"})));
    cm.save_to_rvf(&dir.path().join("c.rvf")).unwrap();

    // The attacker rewrites the payload and recomputes both hashes so the
    // event is internally consistent. The next event's prev_hash and the
    // signed head no longer agree.
    cm.tamper_for_test(e.sequence, |ev| {
        ev.payload = Some(json!({"d": "forged"}));
        ev.payload_hash = crate::chain::compute_payload_hash(&ev.payload);
        ev.hash = crate::chain::compute_event_hash(
            ev.sequence, ev.chain_id, &ev.prev_hash, &ev.source, &ev.kind,
            &ev.timestamp, &ev.payload_hash, ev.rule_hash.as_ref(),
        );
    });
    let d = get(&cm, e.sequence);
    assert!(d.hash_valid && d.payload_hash_valid, "forgery is self-consistent");
    assert_eq!(d.coverage.status, CoverageStatus::SignatureInvalid);
    assert!(d.coverage.reason.contains("no longer match the signed head"), "{}", d.coverage.reason);
}

#[test]
fn ml_dsa_65_coverage_verifies_survives_a_reload_and_rejects_a_wrong_key() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("c.rvf");
    let ek = SigningKey::generate(&mut OsRng);
    // The key file gives the restarted node the same key back.
    let kf = dir.path().join("node.mldsa65");
    let (mk, _) = weftos_pq_sign::keyfile::load_or_create(&kf).unwrap();
    let mut cm = ChainManager::new(3, 0).with_signing_key(ek.clone());
    cm.set_ml_dsa_key(mk);
    let e = cm.append("journal", "session.digest", Some(json!({"d": 9})));
    cm.save_to_rvf(&path).unwrap();

    // A restarted node loads the file, gets its keys back and still knows
    // the checkpoint.
    let mut restored = ChainManager::load_from_rvf(&path, 0).unwrap();
    assert!(restored.signed_checkpoint_bytes().is_some());
    restored.set_signing_key(ek.clone());
    restored.set_ml_dsa_key(weftos_pq_sign::keyfile::load_or_create(&kf).unwrap().0);
    let c = get(&restored, e.sequence).coverage;
    assert_eq!(c.status, CoverageStatus::Signed, "{}", c.reason);
    assert_eq!(c.checkpoint.unwrap().ml_dsa65_valid, Some(true));

    // The right Ed25519 key with another node's ML-DSA key must not pass.
    let mut other = ChainManager::load_from_rvf(&path, 0).unwrap();
    other.set_signing_key(ek);
    other.set_ml_dsa_key(mldsa65::SigningKey::generate());
    let c = get(&other, e.sequence).coverage;
    assert_eq!(c.status, CoverageStatus::SignatureInvalid);
    assert!(c.reason.contains("ML-DSA-65"), "{}", c.reason);
    assert_eq!(c.checkpoint.unwrap().ml_dsa65_valid, Some(false));

    // A node holding no keys cannot vouch for it either.
    let keyless = ChainManager::load_from_rvf(&path, 0).unwrap();
    assert_eq!(get(&keyless, e.sequence).coverage.status, CoverageStatus::SignatureInvalid);
}
