//! Card e821c460: `LeafIngress::reset_floor` recovers a reflashed leaf
//! without weakening replay protection. A reset needs a re-issued
//! certificate, so frames captured before it cannot be replayed.

use super::*;

struct Fixture {
    _dir: tempfile::TempDir,
    ingress: LeafIngress,
    parent: SigningKey,
    machine_pub: [u8; 32],
    scope: String,
    leaf: SigningKey,
    cert: LeafCertificate,
    id: String,
}

fn fixture(tag: u8) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    #[cfg(unix)] { use std::os::unix::fs::PermissionsExt; fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap(); }
    let parent = SigningKey::from_bytes(&[tag; 32]);
    let user_id = key_id(&parent.verifying_key().to_bytes());
    let machine = SigningKey::from_bytes(&[tag + 1; 32]);
    let machine_pub = machine.verifying_key().to_bytes();
    let leaf = SigningKey::from_bytes(&[tag + 2; 32]);
    let scope = format!("user:{user_id}");
    let cert = issue(&parent, &scope, machine_pub, &leaf, 1);
    let id = cert.leaf_id();
    let ingress = LeafIngress::open(dir.path(), machine).unwrap();
    let fx = Fixture { _dir: dir, ingress, parent, machine_pub, scope, leaf, cert, id };
    fx.enrol();
    fx
}

fn issue(parent: &SigningKey, scope: &str, machine_pub: [u8; 32], leaf: &SigningKey, serial: u64) -> LeafCertificate {
    let now = Fixture::now();
    LeafCertificate::issue(parent, scope.into(), machine_pub, leaf.verifying_key().to_bytes(), serial, now - 1, now + 3600, vec!["input.publish".into()]).unwrap()
}

impl Fixture {
    fn enrol(&self) {
        fs::write(self.ingress.registry.join(format!("{}.json", self.id)), serde_json::json!({"node_id": self.id, "cert": self.cert}).to_string()).unwrap();
    }
    /// The owner re-issues the certificate (same leaf key, so the same id,
    /// new serial) and re-enrols it.
    fn reissue(&mut self, serial: u64) {
        self.cert = issue(&self.parent, &self.scope, self.machine_pub, &self.leaf, serial);
        assert_eq!(self.cert.leaf_id(), self.id, "the identity is kept");
        self.enrol();
    }
    fn frame(&self, seq: u64, body: &[u8]) -> SignedPublish {
        SignedPublish::sign(self.cert.clone(), &self.leaf, seq, format!("mesh.leaf.{}.input", self.id), body.to_vec()).unwrap()
    }
    fn now() -> u64 { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() }
    fn accept(&self, seq: u64, body: &[u8]) {
        let f = self.frame(seq, body);
        assert_eq!(self.ingress.prepare(&f, Self::now()).unwrap(), Prepare::New);
        self.ingress.commit(&f).unwrap();
    }
}

#[test]
fn a_reflashed_leaf_is_refused_until_its_floor_is_reset_under_a_new_cert() {
    let mut fx = fixture(60);
    for seq in 1..=44 { fx.accept(seq, b"x"); }
    // The reflashed device starts again at 1: refused, the reported symptom.
    match fx.ingress.prepare(&fx.frame(1, b"y"), Fixture::now()) {
        Err(LeafError::Sequence { expected: 45, got: 1 }) => {}
        other => panic!("expected the sequence refusal, got {other:?}"),
    }
    assert_eq!(fx.ingress.floor_of(&fx.id).map(|f| f.0), Some(44));
    // Same certificate: the reset is refused and the floor stays (H1).
    assert!(matches!(fx.ingress.reset_floor(&fx.id), Err(LeafError::CertUnchanged)));
    assert_eq!(fx.ingress.floor_of(&fx.id).map(|f| f.0), Some(44));
    fx.reissue(2);
    fx.ingress.reset_floor(&fx.id).unwrap();
    assert_eq!(fx.ingress.floor_of(&fx.id), None);
    assert!(!fx.ingress.floor_path(&fx.id).exists());
    // Recovery: the new sequence under the new certificate starts at 1.
    fx.accept(1, b"y");
    fx.accept(2, b"z");
}

#[test]
fn frames_captured_before_a_reset_cannot_be_replayed_after_it() {
    let mut fx = fixture(65);
    let captured: Vec<SignedPublish> = (1..=3).map(|s| fx.frame(s, b"old")).collect();
    for f in &captured {
        assert_eq!(fx.ingress.prepare(f, Fixture::now()).unwrap(), Prepare::New);
        fx.ingress.commit(f).unwrap();
    }
    fx.reissue(2);
    fx.ingress.reset_floor(&fx.id).unwrap();
    // Anyone who recorded frames 1..3 holds only the old certificate.
    for f in &captured {
        assert!(matches!(fx.ingress.prepare(f, Fixture::now()), Err(LeafError::NotEnrolled)), "seq {}", f.seq);
    }
    // Fresh frames under the new certificate are accepted from seq 1.
    fx.accept(1, b"new");
}

#[test]
fn replay_protection_still_holds_after_a_reset() {
    let mut fx = fixture(70);
    for seq in 1..=5 { fx.accept(seq, b"old"); }
    fx.reissue(2);
    fx.ingress.reset_floor(&fx.id).unwrap();
    fx.accept(1, b"new1");
    fx.accept(2, b"new2");
    // An exact replay of the accepted frame is a duplicate, never delivered again.
    assert_eq!(fx.ingress.prepare(&fx.frame(2, b"new2"), Fixture::now()).unwrap(), Prepare::Duplicate);
    // The same sequence with other content is equivocation.
    assert!(matches!(fx.ingress.prepare(&fx.frame(2, b"other"), Fixture::now()), Err(LeafError::Equivocation)));
    // A regression or a gap is refused against the new floor.
    assert!(matches!(fx.ingress.prepare(&fx.frame(1, b"new1"), Fixture::now()), Err(LeafError::Sequence { expected: 3, got: 1 })));
    assert!(matches!(fx.ingress.prepare(&fx.frame(5, b"old"), Fixture::now()), Err(LeafError::Sequence { expected: 3, got: 5 })));
    // The new floor is bound to the new certificate: another reset needs a third.
    assert!(matches!(fx.ingress.reset_floor(&fx.id), Err(LeafError::CertUnchanged)));
    fx.reissue(3);
    fx.ingress.reset_floor(&fx.id).unwrap();
    // After a reset the first frame must be seq 1, not a high number.
    assert!(matches!(fx.ingress.prepare(&fx.frame(6, b"x"), Fixture::now()), Err(LeafError::Sequence { expected: 1, got: 6 })));
}

#[test]
fn a_floor_written_before_the_cert_binding_is_not_reset() {
    let mut fx = fixture(75);
    fx.accept(1, b"x");
    // A floor as an older daemon wrote it: no cert field.
    let (seq, digest) = fx.ingress.floor_of(&fx.id).unwrap();
    fs::write(fx.ingress.floor_path(&fx.id), serde_json::json!({"seq": seq, "digest": digest}).to_string()).unwrap();
    fx.reissue(2);
    assert!(matches!(fx.ingress.reset_floor(&fx.id), Err(LeafError::FloorUnbound)));
    assert!(fx.ingress.floor_path(&fx.id).exists());
    // A frame under the enrolled certificate rebinds the floor; then it can be reset.
    let f = fx.frame(2, b"y");
    assert_eq!(fx.ingress.prepare(&f, Fixture::now()).unwrap(), Prepare::New);
    fx.ingress.commit(&f).unwrap();
    assert!(matches!(fx.ingress.reset_floor(&fx.id), Err(LeafError::CertUnchanged)));
}

#[test]
fn reset_floor_takes_only_a_canonical_leaf_id_and_touches_only_that_floor() {
    let mut fx = fixture(80);
    fx.accept(1, b"x");
    fx.reissue(2);
    let other = "a".repeat(32);
    fs::write(fx.ingress.floor_path(&other), b"{}").unwrap();
    for bad in ["", "..", "../registry/x", "a/b", &"A".repeat(32), &"a".repeat(31), &"a".repeat(33), &format!("{}/..", "a".repeat(29))] {
        assert!(fx.ingress.reset_floor(bad).is_err(), "{bad:?} must be refused");
    }
    assert!(fx.ingress.floor_path(&other).exists(), "another leaf's floor is untouched");
    assert!(fx.ingress.floor_path(&fx.id).exists(), "refused ids removed nothing");
    assert!(fx.ingress.registry.join(format!("{}.json", fx.id)).exists());
    // No floor: a reset is a harmless no-op.
    fx.ingress.reset_floor(&"b".repeat(32)).unwrap();
    assert_eq!(fx.ingress.check_reset(&"b".repeat(32)).unwrap(), None);
}
