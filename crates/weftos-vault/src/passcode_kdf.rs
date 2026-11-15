//! Argon2id passcode KDF (spec 3.1). The only module that touches `argon2`.

use argon2::{Algorithm, Argon2, Params, Version};
use zeroize::Zeroizing;

use crate::error::{Result, VaultError};

/// Argon2id cost parameters stored in a passcode wrapper.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KdfParams {
    /// Memory in KiB.
    pub m_kib: u32,
    /// Iterations.
    pub t: u32,
    /// Lanes.
    pub p: u32,
}

impl KdfParams {
    /// Owner decision 2026-11-15: 64 MiB, t=3, p=1.
    pub const RECOMMENDED: KdfParams = KdfParams {
        m_kib: 65536,
        t: 3,
        p: 1,
    };

    /// Fast profile for tests (format-valid, below policy). Not for real vaults.
    #[cfg(test)]
    pub(crate) const TEST: KdfParams = KdfParams {
        m_kib: 8192,
        t: 1,
        p: 1,
    };

    /// Bound check for parameters read from an untrusted keybag, done before any KDF work.
    pub fn check_format(&self) -> Result<()> {
        let ok = (8192..=1_048_576).contains(&self.m_kib)
            && (1..=10).contains(&self.t)
            && (1..=16).contains(&self.p);
        if ok {
            Ok(())
        } else {
            Err(VaultError::KdfBounds)
        }
    }

    /// Policy for NEW wrappers: at least 19456 KiB, and t of at least 2 below 46080 KiB.
    pub fn check_new_policy(&self) -> Result<()> {
        self.check_format()?;
        if self.m_kib < 19456 || (self.m_kib < 46080 && self.t < 2) {
            return Err(VaultError::KdfPolicy);
        }
        Ok(())
    }
}

/// Argon2id v0x13, 32-byte output, no secret value, no associated data.
pub(crate) fn derive_kek(
    passcode_nfkc: &str,
    salt: &[u8; 16],
    params: KdfParams,
) -> Result<Zeroizing<[u8; 32]>> {
    params.check_format()?;
    let p = Params::new(params.m_kib, params.t, params.p, Some(32))
        .map_err(|_| VaultError::KdfBounds)?;
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, p);
    let mut out = Zeroizing::new([0u8; 32]);
    argon
        .hash_password_into(passcode_nfkc.as_bytes(), salt, out.as_mut())
        .map_err(|_| VaultError::KdfBounds)?;
    Ok(out)
}
