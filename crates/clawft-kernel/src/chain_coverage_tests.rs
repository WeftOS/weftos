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

// ---- chain.get_range ----

fn assert_same_as_single_reads(cm: &ChainManager, r: &ChainRangeDetail) {
    for ev in &r.events {
        let single = get(cm, ev.event.sequence);
        assert_eq!(ev.event.hash, single.hash);
        assert_eq!(ev.event.payload, single.payload);
        assert_eq!(ev.event.payload_canonical, single.payload_canonical);
        assert_eq!(ev.event.hash_valid, single.hash_valid);
        assert_eq!(ev.event.payload_hash_valid, single.payload_hash_valid);
        assert_eq!(ev.coverage.status, single.coverage.status, "seq {}", ev.event.sequence);
        assert_eq!(ev.coverage.reason, single.coverage.reason, "seq {}", ev.event.sequence);
        assert_eq!(
            ev.coverage.chain_linked,
            single.coverage.checkpoint.as_ref().map(|c| c.chain_linked),
            "seq {}",
            ev.event.sequence
        );
        if let (Some(c), Some(s)) = (&r.checkpoint, &single.coverage.checkpoint) {
            assert_eq!(c.signed_root, s.signed_root);
            assert_eq!(c.checkpoint_hash, s.checkpoint_hash);
            assert_eq!(c.range_end, s.range_end);
            assert_eq!(c.ed25519_signature, s.ed25519_signature);
            assert_eq!(c.ml_dsa65_signature, s.ml_dsa65_signature);
            assert_eq!((c.ed25519_valid, c.ml_dsa65_valid), (s.ed25519_valid, s.ml_dsa65_valid));
        }
    }
}

fn filled(n: usize) -> ChainManager {
    let cm = dual();
    for i in 0..n {
        cm.append("journal", "turn.digest", Some(json!({"i": i})));
    }
    cm
}

#[test]
fn a_500_entry_range_matches_500_single_reads() {
    let dir = tempfile::tempdir().unwrap();
    let cm = filled(600);
    cm.save_to_rvf(&dir.path().join("c.rvf")).unwrap();
    let r = range_detail(&cm, 10, 509, None, true).unwrap();
    assert_eq!((r.count, r.events.len()), (500, 500));
    assert!(r.events.iter().all(|e| e.coverage.status == CoverageStatus::Signed));
    assert_same_as_single_reads(&cm, &r);
    // Signatures once per response, not per entry.
    assert!(r.checkpoint.as_ref().unwrap().ml_dsa65_signature.is_some());
    let wire = serde_json::to_value(&r).unwrap();
    assert!(wire["events"][0]["coverage"].get("checkpoint").is_none());
    let lean = range_detail(&cm, 10, 12, None, false).unwrap();
    assert!(lean.checkpoint.unwrap().ed25519_signature.is_none());
}

#[test]
fn over_limit_and_bad_ranges_are_refused_never_truncated() {
    let cm = filled(1200);
    let e = range_detail(&cm, 0, 1000, None, true).unwrap_err();
    assert!(e.contains("1001 events") && e.contains("limit"), "{e}");
    assert!(range_detail(&cm, 0, 999, None, true).is_ok());
    assert!(range_detail(&cm, 0, 9, Some(5), true).unwrap_err().contains("over the limit of 5"));
    assert!(range_detail(&cm, 0, 9, Some(10), true).is_ok());
    assert!(range_detail(&cm, 0, 0, Some(0), true).unwrap_err().contains("limit"));
    assert!(range_detail(&cm, 0, 5, Some(CHAIN_RANGE_MAX + 1), true).unwrap_err().contains("limit"));
    assert!(range_detail(&cm, 9, 3, None, true).unwrap_err().contains("after"));
    // Past the head: refused, not clipped.
    let past = range_detail(&cm, 1190, 1300, None, true).unwrap_err();
    assert!(past.contains("outside the chain"), "{past}");
    assert!(range_detail(&cm, 5000, 5001, None, true).is_err());
}

#[test]
fn a_range_spanning_the_signed_head_goes_signed_then_pending() {
    let dir = tempfile::tempdir().unwrap();
    let cm = filled(10);
    cm.save_to_rvf(&dir.path().join("c.rvf")).unwrap();
    let head = r_head(&cm);
    for i in 0..5 {
        cm.append("journal", "turn.digest", Some(json!({"late": i})));
    }
    let r = range_detail(&cm, head - 2, head + 3, None, true).unwrap();
    let st: Vec<_> = r.events.iter().map(|e| e.coverage.status).collect();
    use CoverageStatus::*;
    assert_eq!(st, [Signed, Signed, Signed, UnsignedPendingCheckpoint, UnsignedPendingCheckpoint, UnsignedPendingCheckpoint]);
    assert!(r.events[3].coverage.reason.contains("unsigned: pending checkpoint"));
    assert_same_as_single_reads(&cm, &r);
    // Entirely past the head: all pending, checkpoint still reported once.
    let tail = range_detail(&cm, head + 1, head + 5, None, true).unwrap();
    assert!(tail.events.iter().all(|e| e.coverage.status == UnsignedPendingCheckpoint));
    assert_same_as_single_reads(&cm, &tail);
}

fn r_head(cm: &ChainManager) -> u64 {
    signature_summary(cm).unwrap().signed_through
}

#[test]
fn no_checkpoint_at_all_is_pending_for_every_entry() {
    let cm = filled(4);
    let r = range_detail(&cm, 0, 3, None, true).unwrap();
    assert!(r.checkpoint.is_none());
    assert!(r.events.iter().all(|e| e.coverage.status == CoverageStatus::UnsignedPendingCheckpoint));
    assert_same_as_single_reads(&cm, &r);
}

#[test]
fn a_tampered_event_breaks_itself_and_everything_before_it_in_the_range() {
    let dir = tempfile::tempdir().unwrap();
    let cm = filled(30);
    cm.save_to_rvf(&dir.path().join("c.rvf")).unwrap();
    cm.tamper_for_test(20, |ev| ev.payload = Some(json!({"i": "forged"})));
    let r = range_detail(&cm, 5, 29, None, true).unwrap();
    for ev in &r.events {
        let want = if ev.event.sequence <= 20 { CoverageStatus::SignatureInvalid } else { CoverageStatus::Signed };
        assert_eq!(ev.coverage.status, want, "seq {}", ev.event.sequence);
    }
    assert_same_as_single_reads(&cm, &r);

    // A self-consistent forgery is caught at the link to its successor, so
    // it breaks events before it only; the forged one itself still links up.
    let cm2 = filled(30);
    cm2.save_to_rvf(&dir.path().join("d.rvf")).unwrap();
    cm2.tamper_for_test(15, |ev| {
        ev.payload = Some(json!({"i": "forged"}));
        ev.payload_hash = crate::chain::compute_payload_hash(&ev.payload);
        ev.hash = crate::chain::compute_event_hash(
            ev.sequence, ev.chain_id, &ev.prev_hash, &ev.source, &ev.kind,
            &ev.timestamp, &ev.payload_hash, ev.rule_hash.as_ref(),
        );
    });
    let r2 = range_detail(&cm2, 0, 29, None, true).unwrap();
    assert_same_as_single_reads(&cm2, &r2);
    assert_eq!(r2.events[14].coverage.status, CoverageStatus::SignatureInvalid);
    assert_eq!(r2.events[16].coverage.status, CoverageStatus::Signed);
}
