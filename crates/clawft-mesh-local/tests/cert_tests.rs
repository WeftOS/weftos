use clawft_mesh_local::cert::{CertError, UserCert, CERT_DOMAIN, DEFAULT_TTL_S, LEEWAY_S};
use clawft_mesh_local::node_id_from_pubkey;
use ed25519_dalek::{Signer, SigningKey};

const T0: u64 = 1_790_000_000;

fn machine() -> SigningKey {
    SigningKey::from_bytes(&[7u8; 32])
}
fn user() -> [u8; 32] {
    SigningKey::from_bytes(&[9u8; 32]).verifying_key().to_bytes()
}
fn issued() -> (UserCert, [u8; 32]) {
    (UserCert::issue(&machine(), user(), 17, T0, DEFAULT_TTL_S), machine().verifying_key().to_bytes())
}

#[test]
fn sign_verify_roundtrip_and_json_shape() {
    let (c, m) = issued();
    c.verify(&m, T0 + 10).unwrap();
    assert_eq!(c.not_after, T0 + 86_400);
    assert_eq!(c.user_id, node_id_from_pubkey(&user()));
    let j = serde_json::to_value(&c).unwrap();
    for k in ["v", "node_id", "machine_pubkey", "user_pubkey", "user_id", "serial", "issued_at", "not_after", "sig"] {
        assert!(j.get(k).is_some(), "missing {k}");
    }
    assert!(j.get("uid").is_none(), "uid must not be in the certificate");
    assert_eq!(j["machine_pubkey"].as_str().unwrap().len(), 64);
    assert_eq!(j["sig"].as_str().unwrap().len(), 128);
    let back: UserCert = serde_json::from_value(j).unwrap();
    assert_eq!(back, c);
}

#[test]
fn signed_bytes_layout_is_fixed() {
    let b = UserCert::signed_bytes(&[1; 32], &[2; 32], 3, 4, 5);
    assert_eq!(&b[..CERT_DOMAIN.len()], b"weftos/user-cert/v1\0");
    assert_eq!(b.len(), CERT_DOMAIN.len() + 32 + 32 + 24);
    assert_eq!(&b[b.len() - 8..], &5u64.to_be_bytes());
}

#[test]
fn expiry_and_leeway_edges() {
    let (c, m) = issued();
    c.verify(&m, c.not_after).unwrap();
    c.verify(&m, c.not_after + LEEWAY_S).unwrap();
    assert!(matches!(c.verify(&m, c.not_after + LEEWAY_S + 1), Err(CertError::Expired { .. })));
    c.verify(&m, T0 - LEEWAY_S).unwrap();
    assert!(matches!(c.verify(&m, T0 - LEEWAY_S - 1), Err(CertError::NotYetValid { .. })));
    assert_eq!(c.renew_due_at(), T0 + 43_200);
}

#[test]
fn tamper_every_field() {
    let (c, m) = issued();
    let other = SigningKey::from_bytes(&[8u8; 32]).verifying_key().to_bytes();
    type Case = (&'static str, UserCert, fn(&CertError) -> bool);
    let cases: Vec<Case> = vec![
        ("v", UserCert { v: 2, ..c.clone() }, |e| matches!(e, CertError::BadVersion(2))),
        ("node_id", UserCert { node_id: "0".repeat(32), ..c.clone() }, |e| *e == CertError::NodeIdMismatch),
        ("machine_pubkey", UserCert { machine_pubkey: other, ..c.clone() }, |e| *e == CertError::NodeIdMismatch),
        ("user_pubkey", UserCert { user_pubkey: other, ..c.clone() }, |e| *e == CertError::UserIdMismatch),
        ("user_id", UserCert { user_id: "f".repeat(32), ..c.clone() }, |e| *e == CertError::UserIdMismatch),
        ("serial", UserCert { serial: 18, ..c.clone() }, |e| *e == CertError::BadSignature),
        ("issued_at", UserCert { issued_at: c.issued_at + 1, ..c.clone() }, |e| *e == CertError::BadSignature),
        ("not_after", UserCert { not_after: c.not_after + 1, ..c.clone() }, |e| *e == CertError::BadSignature),
        ("sig", UserCert { sig: { let mut s = c.sig; s[0] ^= 1; s }, ..c.clone() }, |e| *e == CertError::BadSignature),
        ("window", UserCert { not_after: c.issued_at, ..c.clone() }, |e| *e == CertError::BadValidity),
    ];
    for (name, bad, ok) in cases {
        let e = bad.verify(&m, T0 + 1).expect_err(name);
        assert!(ok(&e), "{name}: got {e:?}");
    }
}

#[test]
fn untrusted_machine_is_rejected_even_if_self_consistent() {
    let rogue = SigningKey::from_bytes(&[42u8; 32]);
    let c = UserCert::issue(&rogue, user(), 1, T0, 60);
    let trusted = machine().verifying_key().to_bytes();
    assert_eq!(c.verify(&trusted, T0), Err(CertError::UntrustedMachine));
    c.verify(&rogue.verifying_key().to_bytes(), T0).unwrap();
}

#[test]
fn domain_separation() {
    let (c, m) = issued();
    // Same fields signed without the domain prefix must not verify.
    let mut bare = Vec::new();
    bare.extend_from_slice(&c.machine_pubkey);
    bare.extend_from_slice(&c.user_pubkey);
    bare.extend_from_slice(&c.serial.to_be_bytes());
    bare.extend_from_slice(&c.issued_at.to_be_bytes());
    bare.extend_from_slice(&c.not_after.to_be_bytes());
    let forged = UserCert { sig: machine().sign(&bare).to_bytes(), ..c.clone() };
    assert_eq!(forged.verify(&m, T0), Err(CertError::BadSignature));
    // A signature made under another protocol's domain must not verify either.
    let mut other = b"weftos/mesh-local/register/v1\0".to_vec();
    other.extend_from_slice(&bare);
    let forged = UserCert { sig: machine().sign(&other).to_bytes(), ..c };
    assert_eq!(forged.verify(&m, T0), Err(CertError::BadSignature));
}

#[test]
fn ttl_overflow_saturates() {
    let c = UserCert::issue(&machine(), user(), 1, u64::MAX - 5, u64::MAX);
    assert_eq!(c.not_after, u64::MAX);
}

/// A small-order machine key (the identity point) with R = identity, S = 0
/// "signs" anything under a cofactorless check. The cert must be refused even
/// though the key is self-consistent and listed as trusted.
#[test]
fn small_order_machine_key_is_rejected() {
    let (c, _) = issued();
    let mut weak = [0u8; 32];
    weak[0] = 1;
    let mut sig = [0u8; 64];
    sig[0] = 1;
    let forged = UserCert { machine_pubkey: weak, node_id: node_id_from_pubkey(&weak), sig, ..c };
    assert_eq!(forged.verify(&weak, T0 + 1), Err(CertError::BadSignature));
}
