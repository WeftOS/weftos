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

    /// Least memory, in KiB, a NEW or rewritten passcode wrapper may use (owner decision 2026-11-15,
    /// `docs/research/vault-v1-kdf-floor-2026-11-15.md`; supersedes spec 3.1's 19456 KiB floor).
    pub const MIN_NEW_M_KIB: u32 = 65536;
    /// Least iterations a NEW or rewritten passcode wrapper may use (same decision; supersedes spec 3.1's
    /// "t of at least 2 below 46080 KiB").
    pub const MIN_NEW_T: u32 = 3;

    /// Policy for NEW and REWRITTEN passcode wrappers: format-valid, `m_kib` at least
    /// [`KdfParams::MIN_NEW_M_KIB`] (64 MiB) and `t` at least [`KdfParams::MIN_NEW_T`] (3). Lanes are
    /// bounded by the format only (1 to 16); [`KdfParams::RECOMMENDED`] uses 1.
    ///
    /// Checked by [`crate::Vault::create`] and [`crate::Vault::change_passcode`] before any Argon2id work.
    /// [`crate::Vault::rotate_vk`] checks it too, but only after it has opened the wrapper with the STORED
    /// parameters: it refuses (`KdfPolicy`) when the passcode it was given opens a below-floor wrapper, and
    /// it drops, without refusing, a below-floor wrapper that passcode does not open. A stored wrapper below
    /// this floor (one written under the older spec 3.1 floor) still unlocks; see
    /// [`crate::Vault::below_policy_wrappers`].
    pub fn check_new_policy(&self) -> Result<()> {
        self.check_format()?;
        if self.m_kib < Self::MIN_NEW_M_KIB || self.t < Self::MIN_NEW_T {
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
