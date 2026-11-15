//! Two-phase keybag writes (review R2-M1).
//!
//! A write method (`change_passcode`, `add_recovery_wrapper`, `add_device_wrapper`, `remove_wrapper`,
//! `rotate_vk`, `drop_history`) changes nothing. It returns a [`PendingWrite`] holding the new signed
//! keybag. The caller stores [`PendingWrite::keybag_bytes`] with `put_if_match` against the bytes the
//! vault was opened from, and only when the store accepts calls [`Vault::commit_write`] with the bytes
//! the store now holds (a read-back, or the object the store returned). The commit checks those bytes
//! are the pending keybag, then installs it (and, after a rotation, the new VK) in the vault and
//! advances the pins.
//!
//! When the store refuses (another device wrote first) the caller drops the pending write. The vault and
//! the pins are then exactly as before the attempt, so the device re-reads the store and unlocks its
//! keybag as usual: no `Fork`, no `GenerationRollback` against its own unstored write. A new VK made by
//! `rotate_vk` lives only inside the pending write until it is committed, so nothing is ever sealed under
//! a VK that no stored wrapper holds.

use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::error::{Result, VaultError};
use crate::keybag::{Keybag, MAX_LEN, Pins};
use crate::rotation::RotationReport;
use crate::vault::{Vault, now_unix};

/// A signed keybag a write produced and the store has not yet accepted. Store
/// [`PendingWrite::keybag_bytes`], then pass this to [`Vault::commit_write`] on success, or drop it.
/// `T` is what the write returns once committed (a recovery key, a wrapper id, a rotation report).
#[must_use = "store keybag_bytes() and then commit_write, or drop it if the store refused"]
pub struct PendingWrite<T> {
    pub(crate) vault_id: [u8; 16],
    /// SHA-256 of the keybag bytes the write was built on.
    pub(crate) base: [u8; 32],
    pub(crate) keybag: Keybag,
    pub(crate) bytes: Vec<u8>,
    /// The new VK after a rotation; `None` for every other write.
    pub(crate) new_vk: Option<Zeroizing<[u8; 32]>>,
    pub(crate) outcome: T,
}

impl<T> core::fmt::Debug for PendingWrite<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PendingWrite")
            .field("sequence", &self.keybag.sequence)
            .field("vk_generation", &self.keybag.current_vk_generation)
            .field("keys", &"<redacted>")
            .finish()
    }
}

impl<T> PendingWrite<T> {
    /// The new signed keybag, to store with `put_if_match` against the vault's current
    /// [`Vault::keybag_bytes`].
    pub fn keybag_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The new keybag, parsed.
    pub fn keybag(&self) -> &Keybag {
        &self.keybag
    }
}

impl PendingWrite<RotationReport> {
    /// What the rotation will do to the wrapper list, readable before the write is stored.
    pub fn report(&self) -> &RotationReport {
        &self.outcome
    }
}

impl Vault {
    /// Build a pending write: apply `f` to a copy of the keybag, bump the sequence and re-sign. The
    /// vault is not changed. Refused when `pins` belong to another signer or are already past this
    /// vault's keybag.
    pub(crate) fn prepare<T>(
        &self,
        pins: &Pins,
        new_vk: Option<Zeroizing<[u8; 32]>>,
        outcome: T,
        f: impl FnOnce(&mut Keybag) -> Result<()>,
    ) -> Result<PendingWrite<T>> {
        if pins.sign_pub.is_some_and(|p| p != self.keybag.sign_pub) {
            return Err(VaultError::SignerMismatch);
        }
        let mut kb = self.keybag.clone();
        f(&mut kb)?;
        kb.sequence = kb
            .sequence
            .checked_add(1)
            .ok_or(VaultError::SequenceExhausted)?;
        if let Some(h) = pins.highest
            && kb.sequence <= h.sequence
        {
            // This vault is behind a keybag the device has already seen; the write would be refused.
            return Err(VaultError::Rollback {
                got: kb.sequence,
                seen: h.sequence,
            });
        }
        kb.updated_unix = now_unix().max(kb.updated_unix);
        kb.validate_structure()?;
        let bytes = kb.sign(&self.signing);
        if bytes.len() > MAX_LEN {
            return Err(VaultError::TooLarge("keybag"));
        }
        tracing::debug!(
            sequence = kb.sequence,
            wrappers = kb.wrappers.len(),
            "keybag write prepared"
        );
        Ok(PendingWrite {
            vault_id: self.keybag.vault_id,
            base: Sha256::digest(&self.bytes).into(),
            keybag: kb,
            bytes,
            new_vk,
            outcome,
        })
    }

    /// Install a pending write the store has ACCEPTED: the vault takes the new keybag (and the new VK
    /// after a rotation) and `pins` record it, so the store cannot replay the keybag from before the
    /// write to this device (review M1).
    ///
    /// `stored` is the keybag the store holds after the caller's `put_if_match`: read it back, or pass
    /// the object the store returned. The commit goes through only when its SHA-256 equals the pending
    /// keybag's, so a write the store refused is not committed even when the caller misreads the
    /// `put_if_match` result (review R3-L3, [`VaultError::StoreMismatch`]). After a refusal, drop the
    /// pending write and unlock the store's keybag instead.
    ///
    /// Refused, with the vault and `pins` unchanged, when the write was built on other bytes than the
    /// vault holds now ([`VaultError::StaleWrite`]), when `stored` is not the pending keybag, when `pins`
    /// belong to another signer (`SignerMismatch`), or when `pins` have already seen a newer generation
    /// (`GenerationRollback`) or a keybag at or past the write's sequence (`Rollback`, or `Fork` at the
    /// same sequence with other bytes: another device's keybag reached this device first).
    pub fn commit_write<T>(
        &mut self,
        pending: PendingWrite<T>,
        stored: &[u8],
        pins: &mut Pins,
    ) -> Result<T> {
        let base: [u8; 32] = Sha256::digest(&self.bytes).into();
        if pending.vault_id != self.keybag.vault_id || pending.base != base {
            return Err(VaultError::StaleWrite);
        }
        let digest: [u8; 32] = Sha256::digest(&pending.bytes).into();
        if <[u8; 32]>::from(Sha256::digest(stored)) != digest {
            return Err(VaultError::StoreMismatch);
        }
        let kb = &pending.keybag;
        if pins.sign_pub.is_some_and(|p| p != kb.sign_pub) {
            return Err(VaultError::SignerMismatch);
        }
        if kb.current_vk_generation < pins.vk_generation {
            return Err(VaultError::GenerationRollback {
                got: kb.current_vk_generation,
                seen: pins.vk_generation,
            });
        }
        if let Some(h) = pins.highest {
            if kb.sequence < h.sequence {
                return Err(VaultError::Rollback {
                    got: kb.sequence,
                    seen: h.sequence,
                });
            }
            if kb.sequence == h.sequence && h.digest != digest {
                return Err(VaultError::Fork {
                    sequence: kb.sequence,
                });
            }
        }
        pins.record(&pending.bytes, kb);
        tracing::debug!(sequence = kb.sequence, "keybag write committed");
        let PendingWrite {
            keybag,
            bytes,
            new_vk,
            outcome,
            ..
        } = pending;
        self.keybag = keybag;
        self.bytes = bytes;
        if let Some(vk) = new_vk {
            self.vk = vk;
        }
        Ok(outcome)
    }

    /// Prepare and commit at once, for tests that model a store that always accepts.
    #[cfg(test)]
    pub(crate) fn write_now<T>(
        &mut self,
        pins: &mut Pins,
        f: impl FnOnce(&Vault, &Pins) -> Result<PendingWrite<T>>,
    ) -> Result<T> {
        let pending = f(self, pins)?;
        let stored = pending.bytes.clone();
        self.commit_write(pending, &stored, pins)
    }
}
