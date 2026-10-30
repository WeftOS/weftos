use super::*;

const CTX: &[u8] = b"test";

/// Parse a `key=hex` line from a test-vector file.
fn field(vector: &str, key: &str) -> Vec<u8> {
    let prefix = format!("{key}=");
    let hex = vector
        .lines()
        .find_map(|l| l.strip_prefix(&prefix))
        .unwrap_or_else(|| panic!("vector has no {key}"));
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect()
}

#[test]
fn sizes_match_fips_204() {
    use ml_dsa::{EncodedSignature, ExpandedSigningKeyBytes};
    assert_eq!(EncodedVerifyingKey::<MlDsa65>::default().len(), PUBLIC_KEY_LEN);
    assert_eq!(ExpandedSigningKeyBytes::<MlDsa65>::default().len(), SECRET_KEY_LEN);
    assert_eq!(EncodedSignature::<MlDsa65>::default().len(), SIGNATURE_LEN);
    assert_eq!(ml_dsa::Seed::default().len(), SEED_LEN);
}

/// Known answer: the IETF LAMPS ML-DSA-65 example seed (00..1f) must derive
/// the published public key byte for byte.
#[test]
fn lamps_seed_derives_the_published_public_key() {
    let v = include_str!("../testdata/lamps-ml-dsa-65.vector");
    let seed: [u8; SEED_LEN] = field(v, "seed").try_into().unwrap();
    assert_eq!(seed, core::array::from_fn(|i| i as u8));
    let k = SigningKey::from_seed(&seed);
    assert_eq!(k.verifying_key().as_bytes(), field(v, "pk").as_slice());
    let sig = k.sign(b"lamps", CTX).unwrap();
    k.verifying_key().verify(b"lamps", CTX, &sig).unwrap();
}

/// Cross-implementation: a signature made by PQClean (the previous backend,
/// pqcrypto-mldsa 0.1.2) with a FIPS 204 context verifies here, and only for
/// that message, context and key.
#[test]
fn pqclean_signature_verifies() {
    let v = include_str!("../testdata/pqclean-ml-dsa-65.vector");
    let (ctx, msg) = (field(v, "ctx"), field(v, "msg"));
    let vk = VerifyingKey::from_bytes(&field(v, "pk")).unwrap();
    let sig = Signature::from_bytes(&field(v, "sig")).unwrap();
    assert_eq!(ctx, b"weftos/exochain/checkpoint/ml-dsa-65/v2");
    vk.verify(&msg, &ctx, &sig).unwrap();
    assert!(vk.verify(b"other", &ctx, &sig).is_err());
    assert!(vk.verify(&msg, b"", &sig).is_err());
    let other = SigningKey::generate();
    assert!(other.verifying_key().verify(&msg, &ctx, &sig).is_err());
}

/// Cross-encoding: the FIPS 204 expanded secret key (`skEncode`, the form
/// PQClean stores) derived from our seed signs messages our verifying key
/// accepts, and our signatures verify under the key decoded from it.
#[test]
#[allow(deprecated)]
fn expanded_encoding_round_trips_with_seed_keys() {
    use ml_dsa::{ExpandedSigningKeyBytes, Signer as _};
    let k = SigningKey::generate();
    let enc = k.expanded.to_expanded();
    assert_eq!(enc.len(), SECRET_KEY_LEN);
    let again = ExpandedSigningKey::<MlDsa65>::from_expanded(
        &ExpandedSigningKeyBytes::<MlDsa65>::try_from(enc.as_slice()).unwrap(),
    );
    assert_eq!(again.verifying_key().encode().as_slice(), k.verifying_key().as_bytes());
    let foreign = again.sign_deterministic(b"m", CTX).unwrap();
    k.verifying_key()
        .verify(b"m", CTX, &Signature::from_bytes(foreign.encode().as_slice()).unwrap())
        .unwrap();
    // The ml-dsa `Signer` trait signs with an empty context; ours matches it.
    let empty_ctx = again.sign(b"m");
    k.verifying_key()
        .verify(b"m", b"", &Signature::from_bytes(empty_ctx.encode().as_slice()).unwrap())
        .unwrap();
    let ours = k.sign(b"m", CTX).unwrap();
    let decoded = MlSignature::try_from(ours.as_bytes()).unwrap();
    assert!(again.verifying_key().verify_with_context(b"m", CTX, &decoded));
}

#[test]
fn signing_is_hedged() {
    let k = SigningKey::generate();
    let a = k.sign(b"same", CTX).unwrap();
    let b = k.sign(b"same", CTX).unwrap();
    assert_ne!(a, b, "two signatures of one message should use fresh randomness");
    k.verifying_key().verify(b"same", CTX, &a).unwrap();
    k.verifying_key().verify(b"same", CTX, &b).unwrap();
}

#[test]
fn sign_verify_round_trip() {
    let k = SigningKey::generate();
    let sig = k.sign(b"hello", CTX).unwrap();
    assert_eq!(sig.as_bytes().len(), SIGNATURE_LEN);
    k.verifying_key().verify(b"hello", CTX, &sig).unwrap();
}

#[test]
fn tampered_message_signature_and_context_are_rejected() {
    let k = SigningKey::generate();
    let vk = k.verifying_key();
    let sig = k.sign(b"hello", CTX).unwrap();
    assert!(vk.verify(b"hellp", CTX, &sig).is_err());
    assert!(vk.verify(b"hello", b"other", &sig).is_err());
    let mut bad = sig.as_bytes().to_vec();
    bad[100] ^= 1;
    assert!(vk.verify(b"hello", CTX, &Signature::from_bytes(&bad).unwrap()).is_err());
}

#[test]
fn malformed_signature_encodings_fail_closed() {
    let k = SigningKey::generate();
    let vk = k.verifying_key();
    for fill in [0x00u8, 0xff] {
        let sig = Signature::from_bytes(&[fill; SIGNATURE_LEN]).unwrap();
        assert!(matches!(vk.verify(b"m", CTX, &sig), Err(Error::BadSignature)));
    }
    // Corrupt only the hint section (the last bytes): decoding rejects it.
    let mut bad = k.sign(b"m", CTX).unwrap().as_bytes().to_vec();
    let n = bad.len();
    bad[n - 1] = 0xff;
    bad[n - 2] = 0xff;
    assert!(vk.verify(b"m", CTX, &Signature::from_bytes(&bad).unwrap()).is_err());
}

#[test]
fn wrong_key_is_rejected() {
    let a = SigningKey::generate();
    let b = SigningKey::generate();
    let sig = a.sign(b"m", CTX).unwrap();
    assert!(b.verifying_key().verify(b"m", CTX, &sig).is_err());
}

#[test]
fn verifying_key_bytes_cannot_become_a_signing_key() {
    let k = SigningKey::generate();
    let vk = k.verifying_key().as_bytes().to_vec();
    // The public key alone has the wrong length.
    assert!(matches!(SigningKey::from_keypair_bytes(&vk), Err(Error::Length { .. })));
    // Public key, then 32 public bytes as the "seed": the self-check fails.
    let mut forged = vk.clone();
    forged.extend_from_slice(&vk[..SEED_LEN]);
    assert!(matches!(SigningKey::from_keypair_bytes(&forged), Err(Error::KeyPairMismatch)));
}

#[test]
fn forged_signatures_from_public_material_fail() {
    // Everything a verifier holds (pk, message, an honest signature on a
    // different message) cannot be turned into a signature on a new message.
    let k = SigningKey::generate();
    let vk = k.verifying_key();
    let honest = k.sign(b"old", CTX).unwrap();
    let mut pk_padded = vk.as_bytes().to_vec();
    pk_padded.resize(SIGNATURE_LEN, 0);
    for forged in [honest.as_bytes().to_vec(), pk_padded, vec![0u8; SIGNATURE_LEN]] {
        let sig = Signature::from_bytes(&forged).unwrap();
        assert!(vk.verify(b"new", CTX, &sig).is_err());
    }
}

#[test]
fn keypair_bytes_round_trip_and_mismatch() {
    let k = SigningKey::generate();
    let bytes = k.to_keypair_bytes();
    assert_eq!(bytes.len(), KEYPAIR_LEN);
    let k2 = SigningKey::from_keypair_bytes(&bytes).unwrap();
    assert_eq!(k.verifying_key(), k2.verifying_key());
    // A key restored from bytes signs for the original verifying key.
    let sig = k2.sign(b"m", CTX).unwrap();
    k.verifying_key().verify(b"m", CTX, &sig).unwrap();
    // Swap in another key's public half.
    let other = SigningKey::generate();
    let mut mixed = other.verifying_key().as_bytes().to_vec();
    mixed.extend_from_slice(&bytes[PUBLIC_KEY_LEN..]);
    assert!(matches!(SigningKey::from_keypair_bytes(&mixed), Err(Error::KeyPairMismatch)));
}

#[test]
fn from_seed_is_deterministic() {
    let seed = [7u8; SEED_LEN];
    assert_eq!(SigningKey::from_seed(&seed).verifying_key(), SigningKey::from_seed(&seed).verifying_key());
    assert_ne!(
        SigningKey::from_seed(&seed).verifying_key(),
        SigningKey::from_seed(&[8u8; SEED_LEN]).verifying_key()
    );
}

#[test]
fn boundary_inputs_are_checked() {
    let k = SigningKey::generate();
    assert!(matches!(k.sign(b"m", &[0u8; 256]), Err(Error::ContextTooLong(256))));
    let sig = k.sign(b"m", &[0u8; 255]).unwrap();
    k.verifying_key().verify(b"m", &[0u8; 255], &sig).unwrap();
    assert!(matches!(
        k.verifying_key().verify(b"m", &[0u8; 256], &sig),
        Err(Error::ContextTooLong(256))
    ));
    assert!(Signature::from_bytes(&[0u8; SIGNATURE_LEN - 1]).is_err());
    assert!(VerifyingKey::from_bytes(&[0u8; 32]).is_err());
}

#[test]
fn debug_output_never_contains_key_bytes() {
    let k = SigningKey::generate();
    let s = format!("{k:?} {:?}", k.verifying_key());
    assert!(s.contains(&k.verifying_key().key_id()));
    assert!(s.len() < 200, "debug output should be a short id, got {} chars", s.len());
}

/// Callers with little stack (an async task, a small worker thread) can
/// still generate, sign and verify: the work runs on its own stack.
#[test]
fn operations_work_from_a_small_stack() {
    std::thread::Builder::new()
        .stack_size(128 * 1024)
        .spawn(|| {
            let k = SigningKey::generate();
            let sig = k.sign(b"m", CTX).unwrap();
            k.verifying_key().verify(b"m", CTX, &sig).unwrap();
            SigningKey::from_keypair_bytes(&k.to_keypair_bytes()).unwrap();
        })
        .unwrap()
        .join()
        .unwrap();
}
