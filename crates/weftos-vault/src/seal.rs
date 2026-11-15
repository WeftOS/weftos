//! XChaCha20-Poly1305 sealing. The raw AEAD helpers are shared by wrappers and blobs;
//! [`SecretSeal`] is the stable `nonce || ciphertext || tag` form extracted from
//! `clawft-kernel::secret_seal` (card a134bcf7), usable on its own.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use zeroize::Zeroizing;

use crate::error::{Result, VaultError};

/// XChaCha20 nonce length.
pub const NONCE_LEN: usize = 24;
/// Poly1305 tag length.
pub const TAG_LEN: usize = 16;

pub(crate) fn random_bytes<const N: usize>() -> Result<[u8; N]> {
    let mut b = [0u8; N];
    getrandom::getrandom(&mut b).map_err(|_| VaultError::Random)?;
    Ok(b)
}

/// Returns `ciphertext || tag`.
pub(crate) fn encrypt(
    key: &[u8; 32],
    nonce: &[u8; NONCE_LEN],
    aad: &[u8],
    pt: &[u8],
) -> Result<Vec<u8>> {
    XChaCha20Poly1305::new(Key::from_slice(key))
        .encrypt(XNonce::from_slice(nonce), Payload { msg: pt, aad })
        .map_err(|_| VaultError::AuthFailed)
}

/// Opens `ciphertext || tag`. One error for every failure cause.
pub(crate) fn decrypt(
    key: &[u8; 32],
    nonce: &[u8; NONCE_LEN],
    aad: &[u8],
    ct: &[u8],
) -> Result<Zeroizing<Vec<u8>>> {
    XChaCha20Poly1305::new(Key::from_slice(key))
        .decrypt(XNonce::from_slice(nonce), Payload { msg: ct, aad })
        .map(Zeroizing::new)
        .map_err(|_| VaultError::AuthFailed)
}

/// An XChaCha20-Poly1305 key with a stable wire format: `nonce (24) || ciphertext || tag (16)`.
/// The slot name is the associated data, so a sealed value moved to another slot fails to open.
/// There is no `Debug` impl that shows the key.
pub struct SecretSeal {
    key: Zeroizing<[u8; 32]>,
}

impl core::fmt::Debug for SecretSeal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("SecretSeal(<redacted>)")
    }
}

impl SecretSeal {
    /// Use a caller-supplied 256-bit key.
    pub fn from_key(key: &[u8; 32]) -> Self {
        Self {
            key: Zeroizing::new(*key),
        }
    }

    /// A fresh random key.
    pub fn random() -> Result<Self> {
        Ok(Self {
            key: Zeroizing::new(random_bytes::<32>()?),
        })
    }

    /// Seal `plaintext`, binding it to `slot`, under a fresh random nonce.
    pub fn seal(&self, slot: &[u8], plaintext: &[u8]) -> Result<Vec<u8>> {
        self.seal_with_nonce(&random_bytes::<NONCE_LEN>()?, slot, plaintext)
    }

    pub(crate) fn seal_with_nonce(
        &self,
        nonce: &[u8; NONCE_LEN],
        slot: &[u8],
        pt: &[u8],
    ) -> Result<Vec<u8>> {
        let mut out = nonce.to_vec();
        out.extend(encrypt(&self.key, nonce, slot, pt)?);
        Ok(out)
    }

    /// Open a value sealed for `slot`.
    pub fn open(&self, slot: &[u8], sealed: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
        if sealed.len() < NONCE_LEN + TAG_LEN {
            return Err(VaultError::AuthFailed);
        }
        let (n, ct) = sealed.split_at(NONCE_LEN);
        let nonce: [u8; NONCE_LEN] = n.try_into().map_err(|_| VaultError::AuthFailed)?;
        decrypt(&self.key, &nonce, slot, ct)
    }
}
