//! ADR-129 migration tests: real ML-DSA-65 checkpoints, the legacy
//! placeholder policy, mixed chains and mixed-version behaviour.

use super::*;
use crate::chain::ChainManager;
use ed25519_dalek::SigningKey;
use rand::rngs::OsRng;
use rvf_crypto::encode_signature_footer;
use rvf_crypto::hash::shake256_256;
use weftos_pq_sign::DualSignature;

fn ed() -> SigningKey {
    SigningKey::generate(&mut OsRng)
}

fn dual_chain(ed_key: &SigningKey, ml_key: mldsa65::SigningKey) -> ChainManager {
    let mut cm = ChainManager::new(7, 1000).with_signing_key(ed_key.clone());
    cm.set_ml_dsa_key(ml_key);
    cm
}

/// The pre-ADR-129 HMAC placeholder, exactly as `weftos-rvf-crypto` 0.3.0
/// computed it, keyed by SHAKE-256 of the Ed25519 secret (what boot used).
fn legacy_placeholder(ed_key: &SigningKey, msg: &[u8]) -> Vec<u8> {
    let key = shake256_256(&ed_key.to_bytes());
    let mut input = key.to_vec();
    input.extend_from_slice(msg);
    input.extend_from_slice(&key);
    let (mut sig, mut block) = (Vec::new(), shake256_256(&input));
    while sig.len() < 3309 {
        sig.extend_from_slice(&block);
        let mut next = block.to_vec();
        next.extend_from_slice(&key);
        block = shake256_256(&next);
    }
    sig.truncate(3309);
    sig
}

/// Write a chain file the way a pre-ADR-129 node did: no suite tag, an
/// Ed25519 footer, then an ML-DSA-65-labelled placeholder footer.
fn write_legacy_file(cm: &ChainManager, ed_key: &SigningKey, path: &std::path::Path) {
    assert!(!cm.has_dual_signing(), "legacy writer must not hold a real ML-DSA key");
    cm.save_to_rvf(path).unwrap();
    let mut data = std::fs::read(path).unwrap();
    let ph = legacy_placeholder(ed_key, b"checkpoint");
    let mut signature = [0u8; SignatureFooter::MAX_SIG_LEN];
    signature[..3309].copy_from_slice(&ph);
    data.extend_from_slice(&encode_signature_footer(&SignatureFooter {
        sig_algo: 1,
        sig_length: 3309,
        signature,
        footer_length: SignatureFooter::compute_footer_length(3309),
    }));
    std::fs::write(path, data).unwrap();
}

#[test]
fn new_checkpoints_carry_a_real_ml_dsa_signature() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("c.rvf");
    let (ek, mk) = (ed(), mldsa65::SigningKey::generate());
    let mlv = mk.verifying_key().clone();
    let cm = dual_chain(&ek, mk);
    cm.append("kernel", "boot.init", None);
    cm.save_to_rvf(&path).unwrap();

    let data = std::fs::read(&path).unwrap();
    let Layout { cp_start, cp_end } = locate_checkpoint(&data).unwrap();
    let (_, cp_payload) = read_segment(&data[cp_start..]).unwrap();
    let cbor = crate::chain::checkpoint_cbor(cp_payload).unwrap();
    assert_eq!(checkpoint_suite(cbor).unwrap(), SigSuite::Ed25519MlDsa65V2);

    // Check the second footer with the ML-DSA library directly.
    let (_, ml_at) = footer_at(&data, cp_end).unwrap();
    let (f, _) = footer_at(&data, ml_at).unwrap();
    assert_eq!((f.sig_algo, f.sig_length as usize), (1, mldsa65::SIGNATURE_LEN));
    let sig = mldsa65::Signature::from_bytes(&f.signature[..mldsa65::SIGNATURE_LEN]).unwrap();
    mlv.verify(&data[cp_start..cp_end], CHECKPOINT_PQ_CONTEXT, &sig).unwrap();

    let v = verify_checkpoint(&data, &ek.verifying_key(), &mlv).unwrap();
    assert_eq!(v, Verdict::Valid(PqStatus::Verified));
}

#[test]
fn tampering_with_checkpoint_or_footer_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("c.rvf");
    let (ek, mk) = (ed(), mldsa65::SigningKey::generate());
    let mlv = mk.verifying_key().clone();
    dual_chain(&ek, mk).save_to_rvf(&path).unwrap();
    let data = std::fs::read(&path).unwrap();
    let Layout { cp_start, .. } = locate_checkpoint(&data).unwrap();

    let mut in_cp = data.clone();
    in_cp[cp_start + SEGMENT_HEADER_SIZE + 70] ^= 1;
    let v = verify_checkpoint(&in_cp, &ek.verifying_key(), &mlv);
    assert!(!v.is_ok_and(|v| v.is_valid()), "tampered checkpoint must not verify");

    let mut in_ml = data.clone();
    let n = in_ml.len();
    in_ml[n - 100] ^= 1;
    assert_eq!(
        verify_checkpoint(&in_ml, &ek.verifying_key(), &mlv).unwrap(),
        Verdict::Rejected(RejectReason::MlDsa65)
    );

    let other = mldsa65::SigningKey::generate();
    assert_eq!(
        verify_checkpoint(&data, &ek.verifying_key(), other.verifying_key()).unwrap(),
        Verdict::Rejected(RejectReason::MlDsa65)
    );
}

#[test]
fn stripping_the_ml_dsa_footer_from_a_v2_file_is_a_rejection_not_a_downgrade() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("c.rvf");
    let (ek, mk) = (ed(), mldsa65::SigningKey::generate());
    let mlv = mk.verifying_key().clone();
    dual_chain(&ek, mk).save_to_rvf(&path).unwrap();
    let data = std::fs::read(&path).unwrap();
    let Layout { cp_end, .. } = locate_checkpoint(&data).unwrap();
    let (_, ml_at) = footer_at(&data, cp_end).unwrap();
    let stripped = &data[..ml_at];
    assert_eq!(
        verify_checkpoint(stripped, &ek.verifying_key(), &mlv).unwrap(),
        Verdict::Rejected(RejectReason::MlDsa65)
    );
    // Replacing the real footer with a placeholder is rejected too.
    let mut swapped = stripped.to_vec();
    let mut signature = [0u8; SignatureFooter::MAX_SIG_LEN];
    signature[..3309].copy_from_slice(&legacy_placeholder(&ek, b"x"));
    swapped.extend_from_slice(&encode_signature_footer(&SignatureFooter {
        sig_algo: 1,
        sig_length: 3309,
        signature,
        footer_length: SignatureFooter::compute_footer_length(3309),
    }));
    assert_eq!(
        verify_checkpoint(&swapped, &ek.verifying_key(), &mlv).unwrap(),
        Verdict::Rejected(RejectReason::MlDsa65)
    );
}

#[test]
fn a_verifying_key_cannot_produce_an_accepted_checkpoint_signature() {
    // Everything public: the file, the ML-DSA verifying key. A forger who
    // re-signs with a key of their own, or builds the old placeholder keyed
    // by the public key bytes, is rejected. (Under the placeholder the
    // verify key WAS the signing key, so this forgery used to succeed.)
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("c.rvf");
    let (ek, mk) = (ed(), mldsa65::SigningKey::generate());
    let mlv = mk.verifying_key().clone();
    dual_chain(&ek, mk).save_to_rvf(&path).unwrap();
    let data = std::fs::read(&path).unwrap();
    let Layout { cp_start, cp_end } = locate_checkpoint(&data).unwrap();
    let (_, ml_at) = footer_at(&data, cp_end).unwrap();

    let attacker = mldsa65::SigningKey::generate();
    let forged_real = ml_dsa_checkpoint_footer(&data[cp_start..cp_end], &attacker).unwrap();
    let mut pk_keyed = [0u8; SignatureFooter::MAX_SIG_LEN];
    let mut block = shake256_256(mlv.as_bytes());
    for chunk in pk_keyed[..3309].chunks_mut(32) {
        chunk.copy_from_slice(&block[..chunk.len()]);
        block = shake256_256(&block);
    }
    let forged_pk = SignatureFooter {
        sig_algo: 1,
        sig_length: 3309,
        signature: pk_keyed,
        footer_length: SignatureFooter::compute_footer_length(3309),
    };
    for f in [forged_real, forged_pk] {
        let mut forged = data[..ml_at].to_vec();
        forged.extend_from_slice(&encode_signature_footer(&f));
        assert_eq!(
            verify_checkpoint(&forged, &ek.verifying_key(), &mlv).unwrap(),
            Verdict::Rejected(RejectReason::MlDsa65)
        );
    }
}

#[test]
fn legacy_checkpoint_verifies_on_ed25519_flagged_as_legacy() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy.rvf");
    let ek = ed();
    let cm = ChainManager::new(7, 1000).with_signing_key(ek.clone());
    cm.append("kernel", "boot.init", None);
    write_legacy_file(&cm, &ek, &path);

    let any_ml = mldsa65::SigningKey::generate();
    let v = ChainManager::verify_rvf_dual_signature(&path, &ek.verifying_key(), any_ml.verifying_key())
        .unwrap();
    assert_eq!(v, Verdict::Valid(PqStatus::LegacyPlaceholder));
    assert!(v.is_valid() && !v.is_post_quantum());
    assert!(ChainManager::verify_rvf_signature(&path, &ek.verifying_key()).unwrap());

    // Ed25519 stays strict on legacy files.
    let mut data = std::fs::read(&path).unwrap();
    let Layout { cp_start, .. } = locate_checkpoint(&data).unwrap();
    data[cp_start + SEGMENT_HEADER_SIZE + 70] ^= 1;
    let v = verify_checkpoint(&data, &ek.verifying_key(), any_ml.verifying_key());
    assert!(!v.is_ok_and(|v| v.is_valid()));
    let v = verify_checkpoint(&std::fs::read(&path).unwrap(), &ed().verifying_key(), any_ml.verifying_key());
    assert_eq!(v.unwrap(), Verdict::Rejected(RejectReason::Ed25519));
}

#[test]
fn mixed_chain_old_events_then_new_events_verifies_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let (old, new) = (dir.path().join("old.rvf"), dir.path().join("new.rvf"));
    let ek = ed();

    // A pre-upgrade node writes three events with a legacy signature.
    let cm = ChainManager::new(7, 1000).with_signing_key(ek.clone());
    cm.append("kernel", "boot.init", None);
    cm.append("tree", "bootstrap", Some(serde_json::json!({"n": 3})));
    write_legacy_file(&cm, &ek, &old);
    let old_hash = cm.last_hash();
    let legacy_sig = DualSignature {
        ed25519: ed25519_dalek::Signer::sign(&ek, b"event-1").to_bytes().to_vec(),
        ml_dsa65: Some(legacy_placeholder(&ek, b"event-1")),
        suite: SigSuite::LegacyPlaceholderV1,
    };
    let legacy_json = serde_json::to_value(&legacy_sig).unwrap();
    let mut old_wire = legacy_json.clone();
    old_wire.as_object_mut().unwrap().remove("suite"); // old nodes never wrote it

    // The upgraded node loads it, gets a real ML-DSA key and keeps going.
    let mut cm = ChainManager::load_from_rvf(&old, 1000).unwrap();
    assert_eq!(cm.last_hash(), old_hash);
    let mk = mldsa65::SigningKey::generate();
    let mlv = mk.verifying_key().clone();
    cm.set_signing_key(ek.clone());
    cm.set_ml_dsa_key(mk);
    cm.append("kernel", "boot.ready", None);
    let new_sig = cm.dual_sign(b"event-2").unwrap();
    cm.save_to_rvf(&new).unwrap();

    let reloaded = ChainManager::load_from_rvf(&new, 1000).unwrap();
    assert!(reloaded.verify_integrity().valid);
    let kinds: Vec<_> = reloaded.tail(0).into_iter().map(|e| e.kind).collect();
    assert_eq!(kinds, ["genesis", "boot.init", "bootstrap", "boot.ready"]);
    let v = ChainManager::verify_rvf_dual_signature(&new, &ek.verifying_key(), &mlv).unwrap();
    assert_eq!(v, Verdict::Valid(PqStatus::Verified));

    // Per-event dual signatures: old one legacy, new one post-quantum.
    let old_back: DualSignature = serde_json::from_value(old_wire).unwrap();
    let edv = ek.verifying_key();
    assert_eq!(
        ChainManager::verify_dual_signature(b"event-1", &old_back, &edv, Some(&mlv)),
        Verdict::Valid(PqStatus::LegacyPlaceholder)
    );
    assert_eq!(
        ChainManager::verify_dual_signature(b"event-2", &new_sig, &edv, Some(&mlv)),
        Verdict::Valid(PqStatus::Verified)
    );
}

#[test]
fn new_chain_event_signatures_are_real_ml_dsa() {
    let (ek, mk) = (ed(), mldsa65::SigningKey::generate());
    let mlv = mk.verifying_key().clone();
    let cm = dual_chain(&ek, mk);
    let e = cm.append("agent", "spawn", Some(serde_json::json!({"pid": 1})));
    let sig = cm.dual_sign(&e.hash).unwrap();
    assert_eq!(sig.suite, SigSuite::Ed25519MlDsa65V2);
    let pq = mldsa65::Signature::from_bytes(sig.ml_dsa65.as_ref().unwrap()).unwrap();
    mlv.verify(&e.hash, CHAIN_DUAL_SIG_CONTEXT, &pq).unwrap();
    assert!(ChainManager::verify_dual_signature(&e.hash, &sig, &ek.verifying_key(), Some(&mlv)).is_post_quantum());
}

#[test]
fn old_node_view_of_new_files_and_signatures() {
    // What a pre-ADR-129 node can still check: the Ed25519 footer, and the
    // Ed25519 half of a DualSignature (it ignores the unknown `suite` field).
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("c.rvf");
    let (ek, mk) = (ed(), mldsa65::SigningKey::generate());
    let cm = dual_chain(&ek, mk);
    cm.save_to_rvf(&path).unwrap();
    assert!(ChainManager::verify_rvf_signature(&path, &ek.verifying_key()).unwrap());
    let sig = cm.dual_sign(b"m").unwrap();
    #[derive(serde::Deserialize)]
    struct OldDualSignature {
        ed25519: Vec<u8>,
        #[allow(dead_code)]
        ml_dsa65: Option<Vec<u8>>,
    }
    let old: OldDualSignature = serde_json::from_value(serde_json::to_value(&sig).unwrap()).unwrap();
    let s = ed25519_dalek::Signature::from_slice(&old.ed25519).unwrap();
    assert!(ed25519_dalek::Verifier::verify(&ek.verifying_key(), b"m", &s).is_ok());
}

#[test]
fn pq_key_is_created_beside_the_ed25519_key_and_reused() {
    let dir = tempfile::tempdir().unwrap();
    let ed_path = dir.path().join("chain.key");
    assert_eq!(pq_key_path_for(&ed_path), dir.path().join("chain.mldsa65"));
    let (a, created) = load_or_create_pq_key(&ed_path).unwrap();
    assert!(created);
    let (b, created) = load_or_create_pq_key(&ed_path).unwrap();
    assert!(!created);
    assert_eq!(a.verifying_key(), b.verifying_key());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(pq_key_path_for(&ed_path)).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
