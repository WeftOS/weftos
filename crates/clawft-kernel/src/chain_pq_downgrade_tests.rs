//! ADR-129 downgrade rule: once a chain has cut over to ML-DSA-65, a
//! legacy or Ed25519-only checkpoint over it is refused.

use super::*;
use crate::chain::ChainManager;
use ed25519_dalek::SigningKey;
use rand::rngs::OsRng;
use rvf_crypto::{encode_signature_footer, sign_segment};
use rvf_wire::writer::write_segment;

fn ed() -> SigningKey {
    SigningKey::generate(&mut OsRng)
}

fn cutover_events(cm: &ChainManager) -> Vec<u64> {
    cm.tail(0)
        .into_iter()
        .filter(|e| is_cutover_event(&e.source, &e.kind))
        .map(|e| e.sequence)
        .collect()
}

/// Save a dual-signed chain with one ordinary event at `path`.
fn save_dual(path: &Path, ek: &SigningKey) -> (ChainManager, mldsa65::VerifyingKey) {
    let mk = mldsa65::SigningKey::generate();
    let mlv = mk.verifying_key().clone();
    let mut cm = ChainManager::new(7, 1000).with_signing_key(ek.clone());
    cm.set_ml_dsa_key(mk);
    cm.append("kernel", "boot.ready", None);
    cm.save_to_rvf(path).unwrap();
    (cm, mlv)
}

/// What an attacker who can forge Ed25519 (but not ML-DSA-65) makes of a
/// v2 file: the same history, the suite tag removed from the checkpoint,
/// re-signed with Ed25519 and followed by no second footer or by `second`.
fn resign_without_pq(data: &[u8], ek: &SigningKey, second: Option<&[u8]>) -> Vec<u8> {
    let Layout { cp_start, .. } = locate_checkpoint(data).unwrap();
    let (h, payload) = read_segment(&data[cp_start..]).unwrap();
    let (exo, cbor) = payload.split_at(64);
    let mut cp: serde_json::Value = ciborium::from_reader(cbor).unwrap();
    cp.as_object_mut().unwrap().remove(CHECKPOINT_SIG_SUITE_KEY);
    let mut body = exo.to_vec();
    ciborium::into_writer(&cp, &mut body).unwrap();
    let seg = write_segment(h.seg_type, &body, rvf_types::SegmentFlags::empty(), h.segment_id);
    let (nh, np) = read_segment(&seg).unwrap();
    let mut out = data[..cp_start].to_vec();
    out.extend_from_slice(&seg);
    out.extend_from_slice(&encode_signature_footer(&sign_segment(&nh, np, ek)));
    if let Some(s) = second {
        out.extend_from_slice(s);
    }
    out
}

fn placeholder_footer() -> Vec<u8> {
    let mut signature = [0u8; SignatureFooter::MAX_SIG_LEN];
    signature[..3309].fill(0x5a);
    encode_signature_footer(&SignatureFooter {
        sig_algo: 1,
        sig_length: 3309,
        signature,
        footer_length: SignatureFooter::compute_footer_length(3309),
    })
}

#[test]
fn first_dual_save_appends_one_cutover_event() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("c.rvf");
    let (cm, mlv) = save_dual(&path, &ed());
    let seqs = cutover_events(&cm);
    assert_eq!(seqs.len(), 1);
    cm.save_to_rvf(&path).unwrap();
    assert_eq!(cutover_events(&cm), seqs, "later saves must not add another cutover");
    let data = std::fs::read(&path).unwrap();
    let c = cutover_in_file(&data).unwrap().unwrap();
    assert_eq!((c.chain_id, c.sequence), (7, seqs[0]));
    let ek_v = cm.verifying_key().unwrap();
    assert_eq!(verify_checkpoint(&data, &ek_v, &mlv).unwrap(), Verdict::Valid(PqStatus::Verified));
    let back = ChainManager::load_from_rvf(&path, 1000).unwrap();
    assert_eq!(cutover_events(&back), seqs);
}

#[test]
fn legacy_checkpoint_before_cutover_verifies_flagged() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("c.rvf");
    let ek = ed();
    let cm = ChainManager::new(7, 1000).with_signing_key(ek.clone());
    cm.append("kernel", "boot.init", None);
    cm.save_to_rvf(&path).unwrap();
    assert!(cutover_events(&cm).is_empty(), "an Ed25519-only save is not a cutover");
    let mut data = std::fs::read(&path).unwrap();
    let mlv = mldsa65::SigningKey::generate().verifying_key().clone();
    assert_eq!(
        verify_checkpoint(&data, &ek.verifying_key(), &mlv).unwrap(),
        Verdict::Valid(PqStatus::Absent)
    );
    data.extend_from_slice(&placeholder_footer());
    std::fs::write(&path, &data).unwrap();
    assert_eq!(
        verify_checkpoint(&data, &ek.verifying_key(), &mlv).unwrap(),
        Verdict::Valid(PqStatus::LegacyPlaceholder)
    );
    assert!(ChainManager::load_from_rvf(&path, 1000).is_ok());
}

#[test]
fn legacy_or_absent_checkpoint_after_cutover_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("c.rvf");
    let ek = ed();
    let (_, mlv) = save_dual(&path, &ek);
    let v2 = std::fs::read(&path).unwrap();
    let placeholder = placeholder_footer();
    for (what, second) in [("absent", None), ("legacy placeholder", Some(placeholder.as_slice()))] {
        let forged = resign_without_pq(&v2, &ek, second);
        assert_eq!(
            verify_checkpoint(&forged, &ek.verifying_key(), &mlv).unwrap(),
            Verdict::Rejected(RejectReason::Downgrade),
            "{what}"
        );
        let p = dir.path().join(format!("forged-{}.rvf", what.len()));
        std::fs::write(&p, &forged).unwrap();
        let Err(err) = ChainManager::load_from_rvf(&p, 1000) else { panic!("{what}: loaded") };
        let err = err.to_string();
        assert!(err.contains("downgrade refused"), "{what}: {err}");
        assert_eq!(
            ChainManager::verify_rvf_dual_signature(&p, &ek.verifying_key(), &mlv).unwrap(),
            Verdict::Rejected(RejectReason::Downgrade),
            "{what}"
        );
    }
}

#[test]
fn save_refuses_an_ed25519_only_checkpoint_after_cutover() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("c.rvf");
    let ek = ed();
    save_dual(&path, &ek);
    let mut cm = ChainManager::load_from_rvf(&path, 1000).unwrap();
    cm.set_signing_key(ek);
    let err = cm.save_to_rvf(&dir.path().join("out.rvf")).unwrap_err().to_string();
    assert!(err.contains("cut over to dual signing"), "{err}");
    let unsigned = ChainManager::load_from_rvf(&path, 1000).unwrap();
    assert!(unsigned.save_to_rvf(&dir.path().join("out2.rvf")).is_err());
}

#[test]
fn node_with_a_cutover_record_refuses_a_history_rewritten_from_before_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("chain.rvf");
    let ek = ed();
    let (cm, mlv) = save_dual(&path, &ek);
    cm.record_cutover(&path).unwrap();
    assert!(cutover_record_path_for(&path).exists());
    assert!(ChainManager::load_from_rvf(&path, 1000).is_ok());

    // A forger replaces the file with a fresh Ed25519-only history.
    let rewritten = ChainManager::new(7, 1000).with_signing_key(ek.clone());
    rewritten.append("kernel", "boot.init", None);
    rewritten.save_to_rvf(&path).unwrap();
    let Err(err) = ChainManager::load_from_rvf(&path, 1000) else { panic!("rewritten history loaded") };
    let err = err.to_string();
    assert!(err.contains("recorded the ML-DSA-65 cutover"), "{err}");
    assert_eq!(
        ChainManager::verify_rvf_dual_signature(&path, &ek.verifying_key(), &mlv).unwrap(),
        Verdict::Rejected(RejectReason::Downgrade)
    );

    // The same file at a path this node never cut over is legacy, flagged.
    let elsewhere = dir.path().join("other.rvf");
    std::fs::copy(&path, &elsewhere).unwrap();
    assert!(ChainManager::load_from_rvf(&elsewhere, 1000).is_ok());

    // A corrupt record fails closed.
    std::fs::write(cutover_record_path_for(&elsewhere), b"not json").unwrap();
    assert!(ChainManager::load_from_rvf(&elsewhere, 1000).is_err());
}

#[test]
fn recording_is_idempotent_and_a_chain_without_cutover_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("chain.rvf");
    let ek = ed();
    let legacy = ChainManager::new(7, 1000).with_signing_key(ek.clone());
    legacy.save_to_rvf(&path).unwrap();
    legacy.record_cutover(&path).unwrap();
    assert!(!cutover_record_path_for(&path).exists());
    let (cm, _) = save_dual(&path, &ek);
    cm.record_cutover(&path).unwrap();
    let first = std::fs::read(cutover_record_path_for(&path)).unwrap();
    cm.save_to_rvf(&path).unwrap();
    cm.record_cutover(&path).unwrap();
    assert_eq!(std::fs::read(cutover_record_path_for(&path)).unwrap(), first);
}
