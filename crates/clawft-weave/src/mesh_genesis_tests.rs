//! Mesh genesis: creation, refusal, licence precedence, learning with a
//! signature check. Temp dirs only.

use std::os::unix::fs::PermissionsExt;

use super::*;

pub(super) fn key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

pub(super) fn pk(seed: u8) -> [u8; 32] {
    key(seed).verifying_key().to_bytes()
}

pub(super) const NOW: &str = "2026-10-12T00:00:00Z";

fn make(dir: &Path) -> SignedMeshRecord {
    create(dir, &key(1), &key(2), None, NOW).unwrap()
}

#[test]
fn create_derives_the_id_from_user_key_and_nonce_and_writes_a_0600_record() {
    let d = tempfile::tempdir().unwrap();
    let s = make(d.path());
    let nonce = hex32(&s.record.mesh_nonce).unwrap();
    assert_eq!(s.record.mesh_id, MeshId::derive(&pk(2), &nonce).to_hex());
    assert_eq!(s.record.created_by, hex::encode(pk(2)));
    assert_eq!(s.signer, hex::encode(pk(1)));
    assert_eq!(s.verify().unwrap().to_hex(), s.record.mesh_id);
    let mode = std::fs::metadata(record_path(d.path())).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let stored = load(d.path()).unwrap().unwrap();
    assert_eq!(stored.origin, Origin::Genesis);
    assert_eq!(stored.signed, s);
    // No private material in the file.
    let text = std::fs::read_to_string(record_path(d.path())).unwrap();
    assert!(!text.contains(&hex::encode([1u8; 32])) && !text.contains(&hex::encode([2u8; 32])));
}

#[test]
fn create_twice_refuses_and_keeps_the_first_id() {
    let d = tempfile::tempdir().unwrap();
    let first = make(d.path());
    let e = create(d.path(), &key(1), &key(2), None, NOW).unwrap_err();
    assert!(e.contains("already has a MeshId") && e.contains(&first.record.mesh_id), "{e}");
    assert_eq!(load(d.path()).unwrap().unwrap().signed, first);
}

#[test]
fn create_refuses_over_an_unreadable_record_and_over_a_licence_id() {
    let d = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(d.path().join(RECORD_DIR)).unwrap();
    std::fs::write(record_path(d.path()), "not json").unwrap();
    let e = create(d.path(), &key(1), &key(2), None, NOW).unwrap_err();
    assert!(e.contains("cannot be used"), "{e}");
    assert_eq!(std::fs::read_to_string(record_path(d.path())).unwrap(), "not json");

    let d2 = tempfile::tempdir().unwrap();
    let licence = MeshId::derive(&[7; 32], &[8; 32]);
    let e = create(d2.path(), &key(1), &key(2), Some(licence), NOW).unwrap_err();
    assert!(e.contains("Seed licence") && e.contains(&licence.to_hex()), "{e}");
    assert!(load(d2.path()).unwrap().is_none());
}

#[test]
fn resolve_distinguishes_none_record_licence_and_broken() {
    let d = tempfile::tempdir().unwrap();
    assert_eq!(resolve(d.path(), None).unwrap(), None);
    let s = make(d.path());
    let r = resolve(d.path(), None).unwrap().unwrap();
    assert_eq!((r.id.to_hex(), r.source.clone()), (s.record.mesh_id.clone(), IdSource::Genesis));
    // A licence id wins and a disagreeing record is reported, not obeyed.
    let licence = MeshId::derive(&[7; 32], &[8; 32]);
    let r = resolve(d.path(), Some(licence)).unwrap().unwrap();
    assert_eq!((r.id, r.source), (licence, IdSource::Licence));
    assert_eq!(r.conflicting_record.map(|c| c.to_hex()), Some(s.record.mesh_id));
    // Broken record, no licence: an error, never None.
    std::fs::write(record_path(d.path()), "{}").unwrap();
    assert!(resolve(d.path(), None).is_err());
    // ... but the licence still resolves.
    assert!(resolve(d.path(), Some(licence)).unwrap().is_some());
}

#[test]
fn adopt_licence_needs_a_licence_and_refuses_a_conflicting_record() {
    let d = tempfile::tempdir().unwrap();
    let e = adopt_licence(d.path(), None).unwrap_err();
    assert!(e.contains("no Seed licence") && e.contains("weaver mesh create"), "{e}");
    let licence = MeshId::derive(&[7; 32], &[8; 32]);
    assert_eq!(adopt_licence(d.path(), Some(licence)).unwrap(), licence);
    let s = make(d.path());
    let e = adopt_licence(d.path(), Some(licence)).unwrap_err();
    assert!(e.contains(&licence.to_hex()) && e.contains(&s.record.mesh_id), "{e}");
}

#[test]
fn a_record_whose_id_matches_the_licence_is_not_a_conflict() {
    let d = tempfile::tempdir().unwrap();
    let s = make(d.path());
    let same = MeshId::from_hex(&s.record.mesh_id).unwrap();
    assert_eq!(adopt_licence(d.path(), Some(same)).unwrap(), same);
}

#[test]
fn a_member_learns_the_primarys_record_when_the_pinned_key_signed_it() {
    let primary = tempfile::tempdir().unwrap();
    let s = make(primary.path());
    let member = tempfile::tempdir().unwrap();
    let id = learn(member.path(), &s, &pk(1), "aa11", None).unwrap();
    assert_eq!(id.to_hex(), s.record.mesh_id);
    let r = resolve(member.path(), None).unwrap().unwrap();
    assert_eq!(r.source, IdSource::Learned { node: "aa11".into() });
    assert_eq!(r.source.describe(), "learned from aa11");
    // Again: idempotent.
    assert_eq!(learn(member.path(), &s, &pk(1), "aa11", None).unwrap(), id);
}

#[test]
fn a_tampered_or_misattributed_record_is_refused_and_nothing_is_written() {
    let primary = tempfile::tempdir().unwrap();
    let s = make(primary.path());
    let member = tempfile::tempdir().unwrap();
    // Tampered id (still well-formed).
    let mut t = s.clone();
    t.record.mesh_id = MeshId::derive(&[9; 32], &[9; 32]).to_hex();
    assert!(learn(member.path(), &t, &pk(1), "n", None).unwrap_err().contains("not derived"));
    // Tampered timestamp: the signature no longer verifies.
    let mut t = s.clone();
    t.record.created_at = "2030-01-01T00:00:00Z".into();
    assert!(learn(member.path(), &t, &pk(1), "n", None).unwrap_err().contains("signature"));
    // Re-signed by someone else: valid record, but not the pinned primary.
    let mut t = s.clone();
    t.signer = hex::encode(pk(3));
    assert!(learn(member.path(), &t, &pk(1), "n", None).is_err());
    // Genuine record, but the pairing pins a different key.
    assert!(learn(member.path(), &s, &pk(3), "n", None).unwrap_err().contains("pins"));
    // Unknown fields are refused by the parser.
    let mut v = serde_json::to_value(&s).unwrap();
    v["extra"] = serde_json::json!(1);
    assert!(serde_json::from_value::<SignedMeshRecord>(v).is_err());
    assert!(load(member.path()).unwrap().is_none());
}

#[test]
fn a_member_refuses_a_second_mesh() {
    let (p1, p2, member) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let a = make(p1.path());
    let b = create(p2.path(), &key(1), &key(2), None, NOW).unwrap();
    learn(member.path(), &a, &pk(1), "n1", None).unwrap();
    assert!(learn(member.path(), &b, &pk(1), "n2", None).unwrap_err().contains("holds mesh"));
    assert_eq!(resolve(member.path(), None).unwrap().unwrap().id.to_hex(), a.record.mesh_id);
    // A licence-derived local id that differs also refuses.
    let fresh = tempfile::tempdir().unwrap();
    let licence = MeshId::derive(&[7; 32], &[8; 32]);
    assert!(learn(fresh.path(), &a, &pk(1), "n1", Some(licence)).unwrap_err().contains("already in mesh"));
    assert!(load(fresh.path()).unwrap().is_none());
}

// Forwarding countersignatures: A (seed 1) creates, P (seed 3) forwards to M,
// M (seed 4) forwards to N.

#[test]
fn a_creator_forwards_its_own_record_unchanged() {
    let s = make(tempfile::tempdir().unwrap().path());
    let sent = s.forwarded_by(&key(1)).unwrap();
    assert_eq!(sent, s);
    assert!(sent.forwarder.is_none() && sent.forward_signature.is_none());
    let json = serde_json::to_string(&sent).unwrap();
    assert!(!json.contains("forwarder") && !json.contains("forward_signature"));
}

#[test]
fn a_forwarded_record_is_accepted_from_the_pinned_forwarder() {
    let s = make(tempfile::tempdir().unwrap().path());
    let sent = s.forwarded_by(&key(3)).unwrap();
    assert_eq!(sent.forwarder.as_deref(), Some(hex::encode(pk(3)).as_str()));
    assert_eq!(sent.verify().unwrap().to_hex(), s.record.mesh_id);
    let m = tempfile::tempdir().unwrap();
    let id = learn(m.path(), &sent, &pk(3), "pp", None).unwrap();
    assert_eq!(id.to_hex(), s.record.mesh_id);
    let stored = load(m.path()).unwrap().unwrap();
    assert_eq!(stored.origin, Origin::Learned { node: "pp".into() });
    assert_eq!(stored.signed, s, "forward fields are not stored");
    // The creator's key still works as the pin for the plain record.
    assert!(learn(tempfile::tempdir().unwrap().path(), &s, &pk(1), "a", None).is_ok());
}

#[test]
fn a_forwarded_record_from_an_unpinned_forwarder_is_refused() {
    let s = make(tempfile::tempdir().unwrap().path());
    let sent = s.forwarded_by(&key(3)).unwrap();
    let m = tempfile::tempdir().unwrap();
    let e = learn(m.path(), &sent, &pk(5), "n", None).unwrap_err();
    assert!(e.contains("pins") && e.contains(&format!("forwarded by {}", hex::encode(pk(3)))), "{e}");
    assert!(load(m.path()).unwrap().is_none());
}

#[test]
fn a_tampered_or_lone_countersignature_is_refused() {
    let s = make(tempfile::tempdir().unwrap().path());
    let sent = s.forwarded_by(&key(3)).unwrap();
    let m = tempfile::tempdir().unwrap();
    let mut t = sent.clone();
    let mut sig = t.forward_signature.clone().unwrap().into_bytes();
    sig[0] = if sig[0] == b'0' { b'1' } else { b'0' };
    t.forward_signature = Some(String::from_utf8(sig).unwrap());
    assert!(learn(m.path(), &t, &pk(3), "n", None).unwrap_err().contains("countersignature"));
    // Claiming a different forwarder with P's signature.
    let mut t = sent.clone();
    t.forwarder = Some(hex::encode(pk(5)));
    assert!(learn(m.path(), &t, &pk(5), "n", None).is_err());
    // One field without the other.
    let mut t = sent.clone();
    t.forward_signature = None;
    assert!(learn(m.path(), &t, &pk(3), "n", None).is_err());
    assert!(load(m.path()).unwrap().is_none());
}

#[test]
fn creator_and_forward_signatures_are_not_interchangeable() {
    let s = make(tempfile::tempdir().unwrap().path());
    // A forwarder tries to pass the creator's node signature as its countersignature.
    let mut t = s.clone();
    t.forwarder = Some(s.signer.clone());
    t.forward_signature = Some(s.signature.clone());
    assert!(t.verify().unwrap_err().contains("countersignature"));
    // P countersigns; P's countersignature offered as the creator's node signature.
    let sent = s.forwarded_by(&key(3)).unwrap();
    let mut t = s.clone();
    t.signer = hex::encode(pk(3));
    t.signature = sent.forward_signature.clone().unwrap();
    assert!(t.verify().unwrap_err().contains("signer's signature"));
    // ... or as the user signature.
    let mut t = s.clone();
    t.user_signature = sent.forward_signature.unwrap();
    assert!(t.verify().unwrap_err().contains("creator's signature"));
}

#[test]
fn old_records_without_forward_fields_parse_and_verify() {
    let s = make(tempfile::tempdir().unwrap().path());
    let mut v = serde_json::to_value(&s).unwrap();
    assert!(v.get("forwarder").is_none());
    let old: SignedMeshRecord = serde_json::from_value(v.clone()).unwrap();
    assert!(old.verify().is_ok());
    v["forwarder"] = serde_json::Value::Null;
    assert!(serde_json::from_value::<SignedMeshRecord>(v).is_ok());
}

#[test]
fn a_two_hop_chain_pins_each_hop() {
    let (a, p, m, n) = (
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
    );
    let s = make(a.path());
    // P imports the creator's record (stored without forward fields).
    publish(p.path(), &StoredRecord { signed: s.clone(), origin: Origin::Imported { node: "a".into() } }).unwrap();
    // P -> M, pinned to P.
    let mut report = serde_json::json!({});
    let id = MeshId::from_hex(&s.record.mesh_id).unwrap();
    let to_m = load(p.path()).unwrap().unwrap().signed.forwarded_by(&key(3)).unwrap();
    report["mesh_record"] = serde_json::json!(to_m);
    let got: SignedMeshRecord = serde_json::from_value(report["mesh_record"].clone()).unwrap();
    learn(m.path(), &got, &pk(3), "p", None).unwrap();
    // M -> N, pinned to M; M countersigns with its own key.
    let to_n = load(m.path()).unwrap().unwrap().signed.forwarded_by(&key(4)).unwrap();
    assert_eq!(to_n.forwarder.as_deref(), Some(hex::encode(pk(4)).as_str()));
    learn(n.path(), &to_n, &pk(4), "m", None).unwrap();
    assert_eq!(resolve(n.path(), None).unwrap().unwrap().id, id);
    // N pinning P (the wrong hop) refuses M's forward.
    assert!(learn(tempfile::tempdir().unwrap().path(), &to_n, &pk(3), "x", None).is_err());
}

#[test]
fn attach_countersigns_a_record_the_node_did_not_create() {
    use std::os::unix::fs::PermissionsExt;
    let p = tempfile::tempdir().unwrap();
    let s = make(tempfile::tempdir().unwrap().path());
    publish(p.path(), &StoredRecord { signed: s.clone(), origin: Origin::Imported { node: "a".into() } }).unwrap();
    let id = MeshId::from_hex(&s.record.mesh_id).unwrap();
    let kp = p.path().join("node.key");
    std::fs::write(&kp, [3u8; 32]).unwrap();
    std::fs::set_permissions(&kp, std::fs::Permissions::from_mode(0o600)).unwrap();
    let mut report = serde_json::json!({});
    attach(&mut report, Some(id), Some(p.path()));
    let sent: SignedMeshRecord = serde_json::from_value(report["mesh_record"].clone()).unwrap();
    assert_eq!(sent.forwarder.as_deref(), Some(hex::encode(pk(3)).as_str()));
    learn(tempfile::tempdir().unwrap().path(), &sent, &pk(3), "p", None).unwrap();
    // The creator's own node sends the record as is.
    let c = tempfile::tempdir().unwrap();
    let own = make(c.path());
    let ck = c.path().join("node.key");
    std::fs::write(&ck, [1u8; 32]).unwrap();
    std::fs::set_permissions(&ck, std::fs::Permissions::from_mode(0o600)).unwrap();
    let mut r2 = serde_json::json!({});
    attach(&mut r2, Some(MeshId::from_hex(&own.record.mesh_id).unwrap()), Some(c.path()));
    assert!(r2["mesh_record"].get("forwarder").is_none());
}
