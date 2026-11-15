//! Recovery key and its display code (spec section 6): `WVR1-` plus Crockford base32 of
//! `key || SHA-256(key)[0..2]`, in groups of 4. WeftOS never stores it.

use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::error::{Result, VaultError};

const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
const PREFIX: &str = "WVR1-";
const CODE_CHARS: usize = 55;

/// 32 random bytes that can unlock the vault. Redacted in `Debug`.
pub struct RecoveryKey(pub(crate) Zeroizing<[u8; 32]>);

impl core::fmt::Debug for RecoveryKey {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("RecoveryKey(<redacted>)")
    }
}

impl RecoveryKey {
    pub(crate) fn from_bytes(b: [u8; 32]) -> Self {
        Self(Zeroizing::new(b))
    }

    pub(crate) fn random() -> Result<Self> {
        Ok(Self::from_bytes(crate::seal::random_bytes()?))
    }

    /// The display code shown once to the user.
    pub fn to_code(&self) -> Zeroizing<String> {
        let mut raw = Zeroizing::new(self.0.to_vec());
        raw.extend(&Sha256::digest(*self.0)[..2]);
        let chars = encode(&raw);
        let mut out = String::from(PREFIX);
        for (i, ch) in chars.chars().enumerate() {
            if i > 0 && i % 4 == 0 {
                out.push('-');
            }
            out.push(ch);
        }
        Zeroizing::new(out)
    }

    /// Parse a display code; the checksum catches a typo before any unwrap attempt.
    pub fn from_code(code: &str) -> Result<Self> {
        let upper = Zeroizing::new(code.trim().to_ascii_uppercase());
        let rest = upper
            .strip_prefix(PREFIX)
            .ok_or(VaultError::BadRecoveryCode)?;
        let chars: Zeroizing<String> = Zeroizing::new(rest.chars().filter(|c| *c != '-').collect());
        if chars.chars().count() != CODE_CHARS {
            return Err(VaultError::BadRecoveryCode);
        }
        let raw = decode(&chars)?;
        if raw.len() != 34 {
            return Err(VaultError::BadRecoveryCode);
        }
        let mut key = [0u8; 32];
        key.copy_from_slice(&raw[..32]);
        if Sha256::digest(key)[..2] != raw[32..] {
            return Err(VaultError::BadRecoveryCode);
        }
        Ok(Self::from_bytes(key))
    }
}

fn encode(data: &[u8]) -> Zeroizing<String> {
    let mut out = Zeroizing::new(String::new());
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
    let mut out = Zeroizing::new(Vec::new());
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
