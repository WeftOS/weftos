//! AEAD sealing for the in-memory secret stores of
//! [`ConfigService`](crate::ConfigService) and
//! [`AuthService`](crate::AuthService) (card a134bcf7).
//!
//! Each value is sealed with XChaCha20-Poly1305 under a fresh random
//! 192-bit nonce. The sealed form is `nonce (24 bytes) || ciphertext ||
//! tag (16 bytes)`. The caller passes the slot name (for example
//! `"namespace/key"`) as associated data, so a sealed value copied into a
//! different slot fails to open.
//!
//! # Threat model
//!
//! Both services keep secrets in process memory only: nothing is written
//! to disk, serialized, sent over IPC or logged. The key lives in the same
//! process as the ciphertext, so sealing does not protect against an
//! attacker who can read the whole process memory. What it does give:
//! confidentiality and integrity for any sealed bytes that leave the
//! process without the key, detection of tampered or truncated values,
//! and binding of each value to its slot. Persisting these stores would
//! need a key from an OS keystore or a sealed key file; that is not done
//! here.

use chacha20poly1305::aead::{Aead, AeadCore, KeyInit, OsRng, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};

/// Length of the XChaCha20 nonce prefixed to every sealed value.
pub(crate) const NONCE_LEN: usize = 24;
/// Length of the Poly1305 tag appended to every ciphertext.
pub(crate) const TAG_LEN: usize = 16;

/// Why a value could not be sealed or opened. Deliberately carries no
/// detail: the caller must not learn which byte or which check failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SealError {
    /// Encryption failed (only possible for absurdly large inputs).
    Seal,
    /// The value is truncated, tampered, sealed under another key or
    /// bound to another slot.
    Open,
}

impl std::fmt::Display for SealError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Seal => f.write_str("secret could not be sealed"),
            Self::Open => f.write_str("secret failed authentication"),
        }
    }
}

/// An XChaCha20-Poly1305 key held as a ready cipher. The raw key bytes are
/// not kept anywhere else in the struct, and there is no `Debug` impl.
pub(crate) struct SecretSeal {
    cipher: XChaCha20Poly1305,
}

impl SecretSeal {
    /// Use a caller-supplied 256-bit key.
    pub(crate) fn from_key(key: &[u8; 32]) -> Self {
        Self {
            cipher: XChaCha20Poly1305::new(Key::from_slice(key)),
        }
    }

    /// A fresh random key from the OS RNG, for this process only.
    pub(crate) fn random() -> Self {
        let key = XChaCha20Poly1305::generate_key(&mut OsRng);
        Self {
            cipher: XChaCha20Poly1305::new(&key),
        }
    }

    /// Seal `plaintext`, binding it to `slot`.
    pub(crate) fn seal(&self, slot: &[u8], plaintext: &[u8]) -> Result<Vec<u8>, SealError> {
        let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
        let ct = self
            .cipher
            .encrypt(
                &nonce,
                Payload {
                    msg: plaintext,
                    aad: slot,
                },
            )
            .map_err(|_| SealError::Seal)?;
        let mut out = Vec::with_capacity(NONCE_LEN + ct.len());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ct);
        Ok(out)
    }

    /// Open a value sealed by [`seal`](Self::seal) for the same `slot`.
    pub(crate) fn open(&self, slot: &[u8], sealed: &[u8]) -> Result<Vec<u8>, SealError> {
        if sealed.len() < NONCE_LEN + TAG_LEN {
            return Err(SealError::Open);
        }
        let (nonce, ct) = sealed.split_at(NONCE_LEN);
        self.cipher
            .decrypt(
                XNonce::from_slice(nonce),
                Payload { msg: ct, aad: slot },
            )
            .map_err(|_| SealError::Open)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let s = SecretSeal::from_key(&[7u8; 32]);
        for pt in [&b""[..], b"x", b"a longer secret value with spaces"] {
            let sealed = s.seal(b"ns/k", pt).unwrap();
            assert_eq!(sealed.len(), NONCE_LEN + pt.len() + TAG_LEN);
            assert_eq!(s.open(b"ns/k", &sealed).unwrap(), pt);
        }
    }

    #[test]
    fn ciphertext_does_not_contain_plaintext() {
        let s = SecretSeal::random();
        let pt = b"plaintext-marker-0123456789";
        let sealed = s.seal(b"ns/k", pt).unwrap();
        assert!(!sealed.windows(pt.len()).any(|w| w == pt));
    }

    #[test]
    fn every_flipped_byte_is_detected() {
        let s = SecretSeal::from_key(&[7u8; 32]);
        let sealed = s.seal(b"ns/k", b"tamper-me").unwrap();
        for i in 0..sealed.len() {
            let mut t = sealed.clone();
            t[i] ^= 0x80;
            assert_eq!(s.open(b"ns/k", &t), Err(SealError::Open), "byte {i}");
        }
    }

    #[test]
    fn truncated_or_extended_input_is_rejected() {
        let s = SecretSeal::from_key(&[7u8; 32]);
        let sealed = s.seal(b"ns/k", b"value").unwrap();
        for len in 0..sealed.len() {
            assert_eq!(s.open(b"ns/k", &sealed[..len]), Err(SealError::Open));
        }
        let mut longer = sealed.clone();
        longer.push(0);
        assert_eq!(s.open(b"ns/k", &longer), Err(SealError::Open));
    }

    #[test]
    fn wrong_key_fails() {
        let a = SecretSeal::from_key(&[1u8; 32]);
        let b = SecretSeal::from_key(&[2u8; 32]);
        let sealed = a.seal(b"ns/k", b"value").unwrap();
        assert_eq!(b.open(b"ns/k", &sealed), Err(SealError::Open));
        assert_eq!(
            SecretSeal::random().open(b"ns/k", &sealed),
            Err(SealError::Open)
        );
    }

    #[test]
    fn wrong_slot_fails() {
        let s = SecretSeal::from_key(&[7u8; 32]);
        let sealed = s.seal(b"ns/a", b"value").unwrap();
        assert_eq!(s.open(b"ns/b", &sealed), Err(SealError::Open));
        assert_eq!(s.open(b"", &sealed), Err(SealError::Open));
    }

    #[test]
    fn nonces_are_unique_per_seal() {
        let s = SecretSeal::from_key(&[7u8; 32]);
        let mut seen = std::collections::HashSet::new();
        for _ in 0..1000 {
            let sealed = s.seal(b"ns/k", b"same").unwrap();
            assert!(seen.insert(sealed[..NONCE_LEN].to_vec()), "nonce reused");
        }
    }

    #[test]
    fn random_keys_differ() {
        let a = SecretSeal::random();
        let b = SecretSeal::random();
        let sealed = a.seal(b"ns/k", b"value").unwrap();
        assert_eq!(b.open(b"ns/k", &sealed), Err(SealError::Open));
    }
}
