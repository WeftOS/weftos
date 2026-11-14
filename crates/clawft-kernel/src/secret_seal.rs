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

    // Card 10a2eb4a item 3: the caller's 32 bytes are the key itself (no
    // KDF, not ignored). Vector: draft-arciszewski-xchacha-03 appendix A.3.1,
    // as carried in the chacha20poly1305 0.10.1 test suite.
    const KAT_KEY: [u8; 32] = [
        0x80, 0x81, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8a, 0x8b, 0x8c, 0x8d, 0x8e,
        0x8f, 0x90, 0x91, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0x9b, 0x9c, 0x9d,
        0x9e, 0x9f,
    ];
    const KAT_AAD: &[u8] = &[
        0x50, 0x51, 0x52, 0x53, 0xc0, 0xc1, 0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc7,
    ];
    const KAT_PLAINTEXT: &[u8] = b"Ladies and Gentlemen of the class of '99: \
        If I could offer you only one tip for the future, sunscreen would be it.";
    const KAT_NONCE: [u8; 24] = [
        0x40, 0x41, 0x42, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4a, 0x4b, 0x4c, 0x4d, 0x4e,
        0x4f, 0x50, 0x51, 0x52, 0x53, 0x54, 0x55, 0x56, 0x57,
    ];
    const KAT_CT_TAG: &[u8] = &[
        0xbd, 0x6d, 0x17, 0x9d, 0x3e, 0x83, 0xd4, 0x3b, 0x95, 0x76, 0x57, 0x94, 0x93, 0xc0, 0xe9,
        0x39, 0x57, 0x2a, 0x17, 0x00, 0x25, 0x2b, 0xfa, 0xcc, 0xbe, 0xd2, 0x90, 0x2c, 0x21, 0x39,
        0x6c, 0xbb, 0x73, 0x1c, 0x7f, 0x1b, 0x0b, 0x4a, 0xa6, 0x44, 0x0b, 0xf3, 0xa8, 0x2f, 0x4e,
        0xda, 0x7e, 0x39, 0xae, 0x64, 0xc6, 0x70, 0x8c, 0x54, 0xc2, 0x16, 0xcb, 0x96, 0xb7, 0x2e,
        0x12, 0x13, 0xb4, 0x52, 0x2f, 0x8c, 0x9b, 0xa4, 0x0d, 0xb5, 0xd9, 0x45, 0xb1, 0x1b, 0x69,
        0xb9, 0x82, 0xc1, 0xbb, 0x9e, 0x3f, 0x3f, 0xac, 0x2b, 0xc3, 0x69, 0x48, 0x8f, 0x76, 0xb2,
        0x38, 0x35, 0x65, 0xd3, 0xff, 0xf9, 0x21, 0xf9, 0x66, 0x4c, 0x97, 0x63, 0x7d, 0xa9, 0x76,
        0x88, 0x12, 0xf6, 0x15, 0xc6, 0x8b, 0x13, 0xb5, 0x2e, // tag:
        0xc0, 0x87, 0x59, 0x24, 0xc1, 0xc7, 0x98, 0x79, 0x47, 0xde, 0xaf, 0xd8, 0x78, 0x0a, 0xcf,
        0x49,
    ];

    fn kat_sealed() -> Vec<u8> {
        let mut v = KAT_NONCE.to_vec();
        v.extend_from_slice(KAT_CT_TAG);
        v
    }

    #[test]
    fn caller_key_opens_known_answer_vector() {
        let s = SecretSeal::from_key(&KAT_KEY);
        assert_eq!(&s.open(KAT_AAD, &kat_sealed()).unwrap()[..], KAT_PLAINTEXT);
    }

    #[test]
    fn same_key_opens_across_instances() {
        let a = SecretSeal::from_key(&[0x5A; 32]);
        let b = SecretSeal::from_key(&[0x5A; 32]);
        let sealed = a.seal(b"ns/k", b"shared").unwrap();
        assert_eq!(&b.open(b"ns/k", &sealed).unwrap()[..], b"shared");
    }
}
