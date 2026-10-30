//! ML-DSA-65 (FIPS 204) keys and signatures, on RustCrypto's pure-Rust
//! `ml-dsa` crate.
//!
//! [`SigningKey`] holds the 32-byte FIPS 204 seed ξ, the key expanded from
//! it, and its public half; [`VerifyingKey`] holds only the public key and
//! exposes no way to sign. A verifying key cannot become a signing key:
//!
//! ```compile_fail,E0599
//! use weftos_pq_sign::mldsa65::SigningKey;
//! let vk = SigningKey::generate().verifying_key().clone();
//! let _ = vk.sign(b"msg", b"ctx"); // no such method
//! ```
//!
//! and its bytes are rejected by [`SigningKey::from_keypair_bytes`] (the
//! length is wrong, and a padded attempt fails the pair self-check).
//!
//! Signing is the hedged (randomized) ML-DSA.Sign of FIPS 204 Algorithm 2,
//! with fresh randomness from the operating system for every signature.

use ml_dsa::common::getrandom;
use ml_dsa::{EncodedVerifyingKey, ExpandedSigningKey, MlDsa65};
use zeroize::Zeroizing;

use crate::{Error, Result};

/// Public key length (FIPS 204, ML-DSA-65).
pub const PUBLIC_KEY_LEN: usize = 1952;
/// Length of the FIPS 204 expanded secret key encoding (`skEncode`). Keys are
/// not stored in this form; it is kept for reference and interop checks.
pub const SECRET_KEY_LEN: usize = 4032;
/// Length of the FIPS 204 key-generation seed ξ, the stored private key.
pub const SEED_LEN: usize = 32;
/// Length of [`SigningKey::to_keypair_bytes`]: public key then seed.
pub const KEYPAIR_LEN: usize = PUBLIC_KEY_LEN + SEED_LEN;
/// Signature length (FIPS 204, ML-DSA-65).
pub const SIGNATURE_LEN: usize = 3309;
/// Maximum FIPS 204 context string length.
pub const MAX_CONTEXT_LEN: usize = 255;

type MlVerifyingKey = ml_dsa::VerifyingKey<MlDsa65>;
type MlSignature = ml_dsa::Signature<MlDsa65>;

/// Stack for one ML-DSA operation. `ml-dsa` keeps its polynomial matrices
/// on the stack: measured at 256-512 KiB per key generation, signature or
/// verification in a release build and 512 KiB-1 MiB in a debug one, too
/// much for a caller deep inside an async task.
#[cfg(not(target_family = "wasm"))]
const OP_STACK: usize = 8 << 20;

/// Run `f` on a thread with [`OP_STACK`] of stack. On wasm, or if no thread
/// can be spawned, it runs on the caller's stack.
fn on_big_stack<R: Send, F: FnOnce() -> R + Send>(f: F) -> R {
    let mut slot = Some(f);
    #[cfg(not(target_family = "wasm"))]
    {
        let ran = std::thread::scope(|s| {
            let slot = &mut slot;
            std::thread::Builder::new()
                .name("ml-dsa-65".into())
                .stack_size(OP_STACK)
                .spawn_scoped(s, move || slot.take().map(|f| f()))
                .ok()
                .map(|h| h.join())
        });
        match ran {
            Some(Ok(Some(r))) => return r,
            Some(Err(panic)) => std::panic::resume_unwind(panic),
            _ => {}
        }
    }
    (slot.take().expect("ML-DSA operation already ran"))()
}

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
        let enc = EncodedVerifyingKey::<MlDsa65>::try_from(self.as_bytes())
            .map_err(|_| Error::Malformed("ML-DSA-65 public key"))?;
        on_big_stack(|| {
            let vk = MlVerifyingKey::decode(&enc);
            // Decoding rejects a malformed hint or an out-of-range z; such a
            // signature cannot verify under any key.
            let s = MlSignature::try_from(sig.as_bytes()).map_err(|_| Error::BadSignature)?;
            if vk.verify_with_context(msg, ctx, &s) {
                Ok(())
            } else {
                Err(Error::BadSignature)
            }
        })
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

/// An ML-DSA-65 signing key: the seed, the key expanded from it, and the
/// verifying key.
///
/// Not `Clone`, and its `Debug` prints only the public key id. The seed and
/// the expanded key are zeroized on drop. Intermediate values computed while
/// signing live on the stack inside `ml-dsa` and are not guaranteed to be
/// wiped; ADR-129 records that residue.
pub struct SigningKey {
    seed: Zeroizing<[u8; SEED_LEN]>,
    // Boxed: the expanded key is tens of KiB and must not sit on the stack.
    expanded: Box<ExpandedSigningKey<MlDsa65>>,
    vk: VerifyingKey,
}

impl SigningKey {
    /// Generate a fresh key pair from the operating system's RNG.
    ///
    /// # Panics
    ///
    /// If the operating system RNG fails; use [`SigningKey::try_generate`]
    /// to handle that case.
    pub fn generate() -> Self {
        Self::try_generate().expect("operating system RNG failed while generating an ML-DSA-65 key")
    }

    /// Generate a fresh key pair, reporting an RNG failure as [`Error::Rng`].
    pub fn try_generate() -> Result<Self> {
        let mut seed = Zeroizing::new([0u8; SEED_LEN]);
        getrandom::fill(&mut seed[..]).map_err(|_| Error::Rng)?;
        Ok(Self::from_seed(&seed))
    }

    /// Derive the key pair from a FIPS 204 seed ξ (ML-DSA.KeyGen_internal,
    /// Algorithm 6). The same seed always gives the same key pair.
    pub fn from_seed(seed: &[u8; SEED_LEN]) -> Self {
        let (expanded, pk) = on_big_stack(|| {
            let mut xi = ml_dsa::Seed::from(*seed);
            let expanded = Box::new(ExpandedSigningKey::<MlDsa65>::from_seed(&xi));
            zeroize::Zeroize::zeroize(&mut xi[..]);
            let pk = expanded.verifying_key().encode();
            (expanded, pk)
        });
        let vk = VerifyingKey(Box::new(
            pk.as_slice().try_into().expect("ML-DSA-65 public key length is fixed"),
        ));
        Self { seed: Zeroizing::new(*seed), expanded, vk }
    }

    /// The matching verifying key.
    pub fn verifying_key(&self) -> &VerifyingKey {
        &self.vk
    }

    /// Sign `msg` under the FIPS 204 context `ctx` (at most 255 bytes).
    pub fn sign(&self, msg: &[u8], ctx: &[u8]) -> Result<Signature> {
        check_context(ctx)?;
        on_big_stack(|| {
            let sig = self
                .expanded
                .sign_randomized(msg, ctx, &mut getrandom::SysRng)
                .map_err(|_| Error::Rng)?;
            Signature::from_bytes(sig.encode().as_slice())
        })
    }

    /// Serialize as `public key || seed` ([`KEYPAIR_LEN`] bytes). The buffer
    /// zeroizes on drop.
    pub fn to_keypair_bytes(&self) -> Zeroizing<Vec<u8>> {
        let mut out = Zeroizing::new(Vec::with_capacity(KEYPAIR_LEN));
        out.extend_from_slice(self.vk.as_bytes());
        out.extend_from_slice(&self.seed[..]);
        out
    }

    /// Parse `public key || seed` and prove the halves match: the public key
    /// derived from the seed must equal the stored one.
    pub fn from_keypair_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != KEYPAIR_LEN {
            return Err(Error::Length {
                what: "ML-DSA-65 key pair",
                expected: KEYPAIR_LEN,
                actual: bytes.len(),
            });
        }
        let stored = VerifyingKey::from_bytes(&bytes[..PUBLIC_KEY_LEN])?;
        let seed: Zeroizing<[u8; SEED_LEN]> =
            Zeroizing::new(bytes[PUBLIC_KEY_LEN..].try_into().expect("length checked above"));
        let key = Self::from_seed(&seed);
        if key.vk != stored {
            return Err(Error::KeyPairMismatch);
        }
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

#[cfg(test)]
#[path = "mldsa65_tests.rs"]
mod tests;
