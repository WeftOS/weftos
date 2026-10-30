//! ML-DSA-65 (FIPS 204) keys and signatures.
//!
//! [`SigningKey`] holds the secret key together with its public half;
//! [`VerifyingKey`] holds only the public key and exposes no way to sign.
//! A verifying key cannot become a signing key:
//!
//! ```compile_fail,E0599
//! use weftos_pq_sign::mldsa65::SigningKey;
//! let vk = SigningKey::generate().verifying_key().clone();
//! let _ = vk.sign(b"msg", b"ctx"); // no such method
//! ```
//!
//! and its bytes are rejected by [`SigningKey::from_keypair_bytes`] (the
//! length is wrong, and a padded attempt fails the pair self-check).

#[cfg(feature = "mldsa")]
use pqcrypto_mldsa::mldsa65 as ml;
#[cfg(feature = "mldsa")]
use pqcrypto_traits::sign::{DetachedSignature as _, PublicKey as _, SecretKey as _};
use zeroize::Zeroizing;

use crate::{Error, Result};

/// Public key length (FIPS 204, ML-DSA-65).
pub const PUBLIC_KEY_LEN: usize = 1952;
/// Secret key length (FIPS 204, ML-DSA-65).
pub const SECRET_KEY_LEN: usize = 4032;
/// Signature length (FIPS 204, ML-DSA-65).
pub const SIGNATURE_LEN: usize = 3309;
/// Maximum FIPS 204 context string length.
pub const MAX_CONTEXT_LEN: usize = 255;

/// Context used by the pair self-check; never used for real signatures.
const SELF_CHECK_CONTEXT: &[u8] = b"weftos-pq-sign/keypair-self-check";

/// An ML-DSA-65 public key. It verifies; it cannot sign.
#[derive(Clone, PartialEq, Eq)]
pub struct VerifyingKey(Box<[u8; PUBLIC_KEY_LEN]>);

impl VerifyingKey {
    /// Parse a public key; the length must be exactly [`PUBLIC_KEY_LEN`].
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let arr: [u8; PUBLIC_KEY_LEN] = bytes.try_into().map_err(|_| Error::Length {
            what: "ML-DSA-65 public key",
            expected: PUBLIC_KEY_LEN,
            actual: bytes.len(),
        })?;
        Ok(Self(Box::new(arr)))
    }

    /// The raw public key bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0[..]
    }

    /// A short, non-secret identifier: hex of SHAKE-256(pk) truncated to 16 bytes.
    pub fn key_id(&self) -> String {
        crate::hex(&crate::shake256_16(&[b"ml-dsa-65", self.as_bytes()]))
    }

    /// Verify `sig` over `msg` under the FIPS 204 context `ctx`.
    pub fn verify(&self, msg: &[u8], ctx: &[u8], sig: &Signature) -> Result<()> {
        check_context(ctx)?;
        #[cfg(not(feature = "mldsa"))]
        {
            let _ = (msg, sig);
            Err(Error::Unsupported)
        }
        #[cfg(feature = "mldsa")]
        {
            let pk = ml::PublicKey::from_bytes(self.as_bytes())
                .map_err(|_| Error::Malformed("ML-DSA-65 public key"))?;
            let s = ml::DetachedSignature::from_bytes(sig.as_bytes())
                .map_err(|_| Error::Malformed("ML-DSA-65 signature"))?;
            ml::verify_detached_signature_ctx(&s, msg, ctx, &pk).map_err(|_| Error::BadSignature)
        }
    }
}

impl std::fmt::Debug for VerifyingKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "MlDsa65VerifyingKey({})", self.key_id())
    }
}

/// An ML-DSA-65 signature of exactly [`SIGNATURE_LEN`] bytes.
#[derive(Clone, PartialEq, Eq)]
pub struct Signature(Vec<u8>);

impl Signature {
    /// Parse a signature; the length must be exactly [`SIGNATURE_LEN`].
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != SIGNATURE_LEN {
            return Err(Error::Length {
                what: "ML-DSA-65 signature",
                expected: SIGNATURE_LEN,
                actual: bytes.len(),
            });
        }
        Ok(Self(bytes.to_vec()))
    }

    /// The raw signature bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl std::fmt::Debug for Signature {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "MlDsa65Signature({} bytes)", self.0.len())
    }
}

/// An ML-DSA-65 signing key: the secret key plus its verifying key.
///
/// Not `Clone`, and its `Debug` prints only the public key id. The secret
/// bytes held here are zeroized on drop. `pqcrypto-mldsa` copies the
/// secret into a stack buffer for each signature, which this crate cannot
/// zeroize; that residue is documented in ADR-129.
pub struct SigningKey {
    sk: Zeroizing<Vec<u8>>,
    vk: VerifyingKey,
}

impl SigningKey {
    /// Generate a fresh key pair from the operating system's RNG.
    #[cfg(feature = "mldsa")]
    pub fn generate() -> Self {
        let (pk, sk) = ml::keypair();
        Self {
            sk: Zeroizing::new(sk.as_bytes().to_vec()),
            vk: VerifyingKey(Box::new(
                pk.as_bytes().try_into().expect("pqcrypto public key length is fixed"),
            )),
        }
    }

    /// The matching verifying key.
    pub fn verifying_key(&self) -> &VerifyingKey {
        &self.vk
    }

    /// Sign `msg` under the FIPS 204 context `ctx` (at most 255 bytes).
    pub fn sign(&self, msg: &[u8], ctx: &[u8]) -> Result<Signature> {
        check_context(ctx)?;
        #[cfg(not(feature = "mldsa"))]
        {
            let _ = msg;
            Err(Error::Unsupported)
        }
        #[cfg(feature = "mldsa")]
        {
            let sk = ml::SecretKey::from_bytes(&self.sk)
                .map_err(|_| Error::Malformed("ML-DSA-65 secret key"))?;
            let sig = ml::detached_sign_ctx(msg, ctx, &sk);
            Signature::from_bytes(sig.as_bytes())
        }
    }

    /// Serialize as `public key || secret key`. The buffer zeroizes on drop.
    pub fn to_keypair_bytes(&self) -> Zeroizing<Vec<u8>> {
        let mut out = Zeroizing::new(Vec::with_capacity(PUBLIC_KEY_LEN + SECRET_KEY_LEN));
        out.extend_from_slice(self.vk.as_bytes());
        out.extend_from_slice(&self.sk);
        out
    }

    /// Parse `public key || secret key` and prove the halves match by
    /// signing and verifying a self-check message.
    pub fn from_keypair_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != PUBLIC_KEY_LEN + SECRET_KEY_LEN {
            return Err(Error::Length {
                what: "ML-DSA-65 key pair",
                expected: PUBLIC_KEY_LEN + SECRET_KEY_LEN,
                actual: bytes.len(),
            });
        }
        let key = Self {
            vk: VerifyingKey::from_bytes(&bytes[..PUBLIC_KEY_LEN])?,
            sk: Zeroizing::new(bytes[PUBLIC_KEY_LEN..].to_vec()),
        };
        let probe = key.sign(SELF_CHECK_CONTEXT, SELF_CHECK_CONTEXT)?;
        key.vk
            .verify(SELF_CHECK_CONTEXT, SELF_CHECK_CONTEXT, &probe)
            .map_err(|_| Error::KeyPairMismatch)?;
        Ok(key)
    }
}

impl std::fmt::Debug for SigningKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "MlDsa65SigningKey(public={})", self.vk.key_id())
    }
}

fn check_context(ctx: &[u8]) -> Result<()> {
    if ctx.len() > MAX_CONTEXT_LEN {
        return Err(Error::ContextTooLong(ctx.len()));
    }
    Ok(())
}

#[cfg(all(test, feature = "mldsa"))]
mod tests {
    use super::*;

    const CTX: &[u8] = b"test";

    #[test]
    fn sizes_match_fips_204() {
        assert_eq!(ml::public_key_bytes(), PUBLIC_KEY_LEN);
        assert_eq!(ml::secret_key_bytes(), SECRET_KEY_LEN);
        assert_eq!(ml::signature_bytes(), SIGNATURE_LEN);
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
        // Public key twice, padded to the pair length: the self-check fails.
        let mut forged = vk.clone();
        forged.extend_from_slice(&vk);
        forged.resize(PUBLIC_KEY_LEN + SECRET_KEY_LEN, 0);
        assert!(SigningKey::from_keypair_bytes(&forged).is_err());
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
        let k2 = SigningKey::from_keypair_bytes(&bytes).unwrap();
        assert_eq!(k.verifying_key(), k2.verifying_key());
        // Swap in another key's public half.
        let other = SigningKey::generate();
        let mut mixed = other.verifying_key().as_bytes().to_vec();
        mixed.extend_from_slice(&bytes[PUBLIC_KEY_LEN..]);
        assert!(matches!(SigningKey::from_keypair_bytes(&mixed), Err(Error::KeyPairMismatch)));
    }

    #[test]
    fn boundary_inputs_are_checked() {
        let k = SigningKey::generate();
        assert!(matches!(k.sign(b"m", &[0u8; 256]), Err(Error::ContextTooLong(256))));
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
}
