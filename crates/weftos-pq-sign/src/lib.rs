//! Post-quantum signing for WeftOS (ADR-129).
//!
//! This crate replaces the ML-DSA-65 *placeholder* that shipped in the
//! `weftos-rvf-crypto` fork. That placeholder was an HMAC-SHAKE-256 keyed
//! hash: its "verifying key" was the same 32 bytes as its "signing key",
//! so anyone able to verify could also sign. It was never post-quantum.
//!
//! What this crate provides:
//!
//! - [`mldsa65`]: real ML-DSA-65 (FIPS 204, security category 3) through
//!   RustCrypto's pure-Rust `ml-dsa`. The signing key and the verifying key are separate
//!   types; [`mldsa65::VerifyingKey`] has no signing method, and its bytes
//!   cannot be loaded as a signing key.
//! - [`dual`]: an Ed25519 + ML-DSA-65 dual signature carrying a
//!   [`dual::SigSuite`] tag, so signatures written with the legacy
//!   placeholder stay distinguishable from real ones. Verification returns
//!   a [`dual::Verdict`] that says whether the post-quantum half was
//!   actually verified; a legacy signature is never reported as
//!   post-quantum.
//! - [`keyfile`]: a versioned on-disk key format written owner-only
//!   (0600, `create_new`, no symlinks). Keys are never logged or printed.
//!
//! Inputs from outside the process (signature bytes, key bytes, key files,
//! context strings) are length- and format-checked at the API boundary and
//! fail closed.

#![forbid(unsafe_code)]

pub mod dual;
pub mod keyfile;
pub mod mldsa65;

pub use dual::{DualSignature, PqStatus, RejectReason, SigSuite, Verdict};
pub use mldsa65::{Signature as MlDsa65Signature, SigningKey as MlDsa65SigningKey};
pub use mldsa65::VerifyingKey as MlDsa65VerifyingKey;

/// Errors at the crate boundary.
///
/// No variant carries key material.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A byte string had the wrong length for its type.
    #[error("{what}: expected {expected} bytes, got {actual}")]
    Length {
        what: &'static str,
        expected: usize,
        actual: usize,
    },
    /// The FIPS 204 context string is longer than 255 bytes.
    #[error("ML-DSA context string is {0} bytes; the maximum is 255")]
    ContextTooLong(usize),
    /// A signature did not verify.
    #[error("signature verification failed")]
    BadSignature,
    /// An encoded value was malformed (bad magic, version or tag).
    #[error("malformed {0}")]
    Malformed(&'static str),
    /// A key pair's halves do not belong together.
    #[error("ML-DSA-65 key pair self-check failed: the public key does not match the secret key")]
    KeyPairMismatch,
    /// A key file failed a permission or file-type check.
    #[error("key file {path}: {reason}")]
    KeyFile { path: String, reason: String },
    /// The operating system RNG failed while generating a key or signing.
    #[error("operating system RNG failed")]
    Rng,
    /// An I/O error reading or writing a key file.
    #[error("key file I/O: {0}")]
    Io(#[from] std::io::Error),
}

/// Result alias for this crate.
pub type Result<T> = std::result::Result<T, Error>;

/// SHAKE-256 truncated to 16 bytes, used for public key identifiers.
pub(crate) fn shake256_16(parts: &[&[u8]]) -> [u8; 16] {
    use sha3::Shake256;
    use sha3::digest::{ExtendableOutput, Update, XofReader};
    let mut h = Shake256::default();
    for p in parts {
        h.update(p);
    }
    let mut out = [0u8; 16];
    h.finalize_xof().read(&mut out);
    out
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
