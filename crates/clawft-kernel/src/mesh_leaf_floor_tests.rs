//! Card e821c460: `LeafIngress::reset_floor` recovers a reflashed leaf
//! without weakening replay protection.

use super::*;

struct Fixture {
    _dir: tempfile::TempDir,
    ingress: LeafIngress,
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
    let leaf = SigningKey::from_bytes(&[tag + 2; 32]);
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let cert = LeafCertificate::issue(&parent, format!("user:{user_id}"), machine.verifying_key().to_bytes(), leaf.verifying_key().to_bytes(), 1, now - 1, now + 3600, vec!["input.publish".into()]).unwrap();
    let id = cert.leaf_id();
    let ingress = LeafIngress::open(dir.path(), machine).unwrap();
    fs::write(ingress.registry.join(format!("{id}.json")), serde_json::json!({"node_id": id, "cert": cert}).to_string()).unwrap();
    Fixture { _dir: dir, ingress, leaf, cert, id }
}

impl Fixture {
    fn frame(&self, seq: u64, body: &[u8]) -> SignedPublish {
        SignedPublish::sign(self.cert.clone(), &self.leaf, seq, "t".into(), body.to_vec()).unwrap()
    }
    fn now() -> u64 { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() }
    fn accept(&self, seq: u64, body: &[u8]) {
        let f = self.frame(seq, body);
        assert_eq!(self.ingress.prepare(&f, Self::now()).unwrap(), Prepare::New);
        self.ingress.commit(&f).unwrap();
    }
}

#[test]
fn a_reflashed_leaf_is_refused_until_its_floor_is_reset() {
    let fx = fixture(60);
    for seq in 1..=44 { fx.accept(seq, b"x"); }
    // The reflashed device starts again at 1: refused, the reported symptom.
    match fx.ingress.prepare(&fx.frame(1, b"y"), Fixture::now()) {
        Err(LeafError::Sequence { expected: 45, got: 1 }) => {}
        other => panic!("expected the sequence refusal, got {other:?}"),
    }
    assert_eq!(fx.ingress.floor_of(&fx.id).map(|f| f.0), Some(44));
    fx.ingress.reset_floor(&fx.id).unwrap();
    assert_eq!(fx.ingress.floor_of(&fx.id), None);
    assert!(!fx.ingress.floor_path(&fx.id).exists());
    // Recovery: the new sequence starting at 1 is accepted.
    fx.accept(1, b"y");
    fx.accept(2, b"z");
}

#[test]
fn replay_protection_still_holds_after_a_reset() {
    let fx = fixture(70);
    for seq in 1..=5 { fx.accept(seq, b"old"); }
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
    // After a reset the first frame must be seq 1, not a high number.
    fx.ingress.reset_floor(&fx.id).unwrap();
    assert!(matches!(fx.ingress.prepare(&fx.frame(6, b"x"), Fixture::now()), Err(LeafError::Sequence { expected: 1, got: 6 })));
}

#[test]
fn reset_floor_takes_only_a_canonical_leaf_id_and_touches_only_that_floor() {
    let fx = fixture(80);
    fx.accept(1, b"x");
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
}
