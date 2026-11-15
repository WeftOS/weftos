//! Recovery key and its display code (spec section 6): `WVR1-` plus Crockford base32 of
//! `key || SHA-256(key)[0..2]`, in groups of 4. WeftOS never stores it.

use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::error::{Result, VaultError};

const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
const PREFIX: &str = "WVR1-";
const CODE_CHARS: usize = 55;
/// `WVR1-` plus 55 characters in groups of 4 (13 dashes).
const CODE_LEN: usize = PREFIX.len() + CODE_CHARS + CODE_CHARS / 4;

/// 32 random bytes that can unlock the vault. Redacted in `Debug`.
pub struct RecoveryKey(pub(crate) Zeroizing<[u8; 32]>);

impl core::fmt::Debug for RecoveryKey {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("RecoveryKey(<redacted>)")
    }
}

impl RecoveryKey {
    pub(crate) fn random() -> Result<Self> {
        Ok(Self(Zeroizing::new(crate::seal::random_bytes()?)))
    }

    /// The display code shown once to the user.
    ///
    /// Every buffer that holds key or code bytes is sized up front and zeroized, so no reallocation
    /// leaves an unwiped copy behind (review L3).
    pub fn to_code(&self) -> Zeroizing<String> {
        let mut raw = Zeroizing::new([0u8; 34]);
        raw[..32].copy_from_slice(&self.0[..]);
        raw[32..].copy_from_slice(&Sha256::digest(&self.0[..])[..2]);
        let chars = encode(&raw[..]);
        let mut out = Zeroizing::new(String::with_capacity(CODE_LEN));
        out.push_str(PREFIX);
        for (i, ch) in chars.chars().enumerate() {
            if i > 0 && i % 4 == 0 {
                out.push('-');
            }
            out.push(ch);
        }
        debug_assert_eq!(out.len(), CODE_LEN);
        out
    }

    /// Parse a display code; the checksum catches a typo before any unwrap attempt.
    pub fn from_code(code: &str) -> Result<Self> {
        let upper = Zeroizing::new(code.trim().to_ascii_uppercase());
        let rest = upper
            .strip_prefix(PREFIX)
            .ok_or(VaultError::BadRecoveryCode)?;
        let mut chars = Zeroizing::new(String::with_capacity(rest.len()));
        chars.extend(rest.chars().filter(|c| *c != '-'));
        if chars.chars().count() != CODE_CHARS {
            return Err(VaultError::BadRecoveryCode);
        }
        let raw = decode(&chars)?;
        if raw.len() != 34 {
            return Err(VaultError::BadRecoveryCode);
        }
        let mut key = Zeroizing::new([0u8; 32]);
        key.copy_from_slice(&raw[..32]);
        if Sha256::digest(&key[..])[..2] != raw[32..] {
            return Err(VaultError::BadRecoveryCode);
        }
        Ok(Self(key))
    }
}

fn encode(data: &[u8]) -> Zeroizing<String> {
    let mut out = Zeroizing::new(String::with_capacity((data.len() * 8).div_ceil(5)));
    let (mut acc, mut bits) = (0u32, 0u32);
    for &b in data {
        acc = (acc << 8) | u32::from(b);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPHABET[((acc >> bits) & 31) as usize] as char);
        }
        acc &= (1 << bits) - 1;
    }
    if bits > 0 {
        out.push(ALPHABET[((acc << (5 - bits)) & 31) as usize] as char);
    }
    out
}

fn decode(s: &str) -> Result<Zeroizing<Vec<u8>>> {
    let mut out = Zeroizing::new(Vec::with_capacity(s.len() * 5 / 8));
    let (mut acc, mut bits) = (0u32, 0u32);
    for ch in s.chars() {
        let ch = match ch {
            'O' => '0',
            'I' | 'L' => '1',
            c => c,
        };
        let v = ALPHABET
            .iter()
            .position(|a| *a as char == ch)
            .ok_or(VaultError::BadRecoveryCode)? as u32;
        acc = (acc << 5) | v;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    if acc != 0 {
        return Err(VaultError::BadRecoveryCode);
    }
    Ok(out)
}
