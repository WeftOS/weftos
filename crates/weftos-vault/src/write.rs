//! Two-phase keybag writes (review R2-M1).
//!
//! A write method (`change_passcode`, `add_recovery_wrapper`, `add_device_wrapper`, `remove_wrapper`,
//! `rotate_vk`, `drop_history`) changes nothing. It returns a [`PendingWrite`] holding the new signed
//! keybag. The caller stores [`PendingWrite::keybag_bytes`] with `put_if_match` against the bytes the
//! vault was opened from, and only when the store accepts reads the object back with a fresh GET and
//! calls [`Vault::commit_write`] with those bytes. The commit checks they are the pending keybag, then
//! installs it (and, after a rotation, the new VK) in the vault and advances the pins. The check compares
//! the bytes it is given; it does not ask the store, so the read-back must be a real read (see
//! [`Vault::commit_write`]).
//!
//! When the store refuses (another device wrote first) the caller drops the pending write. The vault and
//! the pins are then exactly as before the attempt, so the device re-reads the store and unlocks its
//! keybag as usual: no `Fork`, no `GenerationRollback` against its own unstored write. A new VK made by
//! `rotate_vk` lives only inside the pending write until it is committed, so nothing is ever sealed under
//! a VK that no stored wrapper holds.
//!
//! One case needs more than "drop it": the store accepted the write, then another device wrote on top
//! before the read-back, so the read-back is a different keybag ([`VaultError::StoreMismatch`]).
//! [`Vault::resolve_write`] settles it: it consumes the pending write, takes the fresh read-back, and
//! answers [`Resolution::Committed`] (the read-back is the pending keybag, installed as by
//! `commit_write`), [`Resolution::Landed`] (the write is in the store under another device's later
//! keybag, and the write's result, such as the recovery key, is handed back to be shown) or
//! [`Resolution::NotLanded`] (the write is dropped and its secrets zeroized). A caller that got
//! `StoreMismatch` from `commit_write` has already lost the pending write; call `resolve_write` instead
//! of `commit_write` whenever the store accepted the PUT.
//!
//! A read-back that proves nothing is not an answer. When it does not parse or verify (truncated,
//! corrupted) or is the keybag from before the write (a lagging replica or cache), `resolve_write`
//! returns [`Resolution::Retry`] with the pending write intact. The caller re-GETs and calls
//! `resolve_write` again, a bounded number of times (a handful, with a short pause), and drops the
//! pending write if the store never settles. Only a verified keybag that is at or past the write's
//! sequence and lacks the write is `NotLanded`. Whenever `resolve_write` verifies a keybag of this vault
//! (`Committed`, `Landed` or `NotLanded`), `pins` record it, so the store cannot later serve this device
//! the keybag from before it.

use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::error::{Result, VaultError};
use crate::keybag::{Keybag, MAX_LEN, Pins, Wrapper};
use crate::rotation::RotationReport;
use crate::vault::{Vault, now_unix};

/// How [`Vault::resolve_write`] settled a write whose PUT the store accepted.
#[derive(Debug)]
pub enum Resolution<T> {
    /// The read-back was exactly the pending keybag: installed, the same result as `commit_write`.
    Committed(T),
    /// Another device wrote on top, and the store's keybag carries this write. `pins` now record that
    /// keybag, so the store cannot serve the one from before it; the vault stays at the keybag it was
    /// opened from and further writes from it are refused (`Rollback`): unlock the store's keybag with a
    /// credential. `T` is the write's result, for example the
    /// [`RecoveryKey`](crate::RecoveryKey) of an `add_recovery_wrapper` to show to the user.
    Landed(T),
    /// The read-back does not show this write. The pending write was dropped and its secrets zeroized.
    NotLanded,
    /// The read-back is no evidence either way, and a second GET may settle it: it did not parse or
    /// verify, or it is the keybag from before this write (a lagging replica or cache). The pending
    /// write is handed back intact (its secrets are still live) to call `resolve_write` again with a
    /// fresh read. Retry a bounded number of times, then drop it.
    Retry(Box<PendingWrite<T>>),
}

/// A signed keybag a write produced and the store has not yet accepted. Store
/// [`PendingWrite::keybag_bytes`], then pass this to [`Vault::commit_write`] (or
/// [`Vault::resolve_write`]) on success, or drop it.
/// `T` is what the write returns once committed (a recovery key, a wrapper id, a rotation report).
#[must_use = "store keybag_bytes() and then commit_write or resolve_write, or drop it if the store refused"]
pub struct PendingWrite<T> {
    pub(crate) vault_id: [u8; 16],
    /// SHA-256 of the keybag bytes the write was built on.
    pub(crate) base: [u8; 32],
    pub(crate) keybag: Keybag,
    pub(crate) bytes: Vec<u8>,
    /// Wrappers this write adds or replaces (absent from the keybag it was built on, compared by id and
    /// body). [`PendingWrite::landed_in`] looks for them in a later keybag.
    pub(crate) added: Vec<Wrapper>,
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

    /// Whether `stored` shows this write. `stored` must be a keybag from a FRESH GET after the caller's
    /// `put_if_match` returned true (the same rule as [`Vault::commit_write`]); the pending bytes or a PUT
    /// echo prove nothing and come back `true`. Nothing changes; the vault and `pins` are not touched.
    /// Most callers want [`Vault::resolve_write`], which does this and hands back the write's result.
    ///
    /// `stored` is parsed and verified against `pins` like any keybag (`BadSignature`, `SignerMismatch`,
    /// `Rollback` and so on come back as errors). It then counts as landed when
    /// - it is exactly the pending keybag, or
    /// - it is the same vault, signed by the same key, its sequence is past the pending one, and
    ///   - for a write that adds wrappers, it carries every wrapper the write added, byte for byte (the ids
    ///     are random 16 bytes and the keybag is signed, so a match is this write, not a look-alike); or
    ///   - for a VK rotation, its VK generation is at least the rotation's and it still carries at least one
    ///     wrapper the rotation wrote. A later `change_passcode` replaces some of them and `drop_history`
    ///     removes some; one survivor still shows the rotation. The signing-seed wrapper is that survivor:
    ///     every non-rotation write keeps it, so the answer stays `true` until another rotation replaces
    ///     it. (Generation alone is not enough: a refused rotation and another device's winning rotation
    ///     reach the same generation.)
    ///
    /// `false` means "not shown to have landed", not "refused". A write that adds no wrapper
    /// (`remove_wrapper`, `drop_history`) is recognised only as the exact keybag, and a rotation or wrapper
    /// that a later write removed or replaced everywhere also reads `false`.
    pub fn landed_in(&self, stored: &[u8], pins: &Pins) -> Result<bool> {
        let kb = Keybag::parse(stored, pins)?;
        Ok(self.landed_in_parsed(stored, &kb))
    }

    /// [`PendingWrite::landed_in`] on a keybag that is already parsed and verified.
    fn landed_in_parsed(&self, stored: &[u8], kb: &Keybag) -> bool {
        if kb.vault_id != self.vault_id || kb.sign_pub != self.keybag.sign_pub {
            return false;
        }
        if <[u8; 32]>::from(Sha256::digest(stored)) == <[u8; 32]>::from(Sha256::digest(&self.bytes))
        {
            return true;
        }
        if kb.sequence <= self.keybag.sequence || self.added.is_empty() {
            return false;
        }
        if self.new_vk.is_some() {
            return kb.current_vk_generation >= self.keybag.current_vk_generation
                && self.added.iter().any(|w| kb.wrappers.contains(w));
        }
        self.added.iter().all(|w| kb.wrappers.contains(w))
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
        let added = kb
            .wrappers
            .iter()
            .filter(|w| !self.keybag.wrappers.contains(w))
            .cloned()
            .collect();
        Ok(PendingWrite {
            vault_id: self.keybag.vault_id,
            base: Sha256::digest(&self.bytes).into(),
            keybag: kb,
            bytes,
            added,
            new_vk,
            outcome,
        })
    }

    /// Install a pending write the store has ACCEPTED: the vault takes the new keybag (and the new VK
    /// after a rotation) and `pins` record it, so the store cannot replay the keybag from before the
    /// write to this device (review M1).
    ///
    /// `stored` is the keybag the store holds after the caller's `put_if_match`, read back with a FRESH
    /// GET (or a content hash the server computed). Do not pass the request body, the pending bytes or
    /// whatever the PUT call echoed: for most object stores the PUT answer is the caller's own body or an
    /// ETag, and the check then tests nothing. The commit goes through only when the SHA-256 of `stored`
    /// equals the pending keybag's, so a write the store refused is not committed when the caller misreads
    /// the `put_if_match` result (review R3-L3, [`VaultError::StoreMismatch`]).
    ///
    /// This is a consistency check on what the caller hands in, not a store attestation: it cannot tell a
    /// read-back from a copy of the pending bytes, so an echoed `stored` commits a write the store never
    /// took. The pins then refuse the store's real keybag on the next unlock (`Fork`, `Rollback` or
    /// `GenerationRollback`) and the device is locked out until its pins are reset. Getting the read-back
    /// right is the caller's job.
    ///
    /// This consumes the pending write, so after [`VaultError::StoreMismatch`] it is gone and a
    /// `RecoveryKey` in it is zeroized. When the `put_if_match` succeeded and the read-back may be another
    /// device's keybag, call [`Vault::resolve_write`] instead: it does this check and, when another
    /// device's write buried this one, hands the result back.
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

    /// Settle a write the store ACCEPTED (`put_if_match` returned true) once the fresh read-back is in
    /// hand. Consumes the pending write; see [`Resolution`] for the answers.
    ///
    /// - `stored` is the pending keybag: the same as [`Vault::commit_write`] (and with the same refusals).
    /// - `stored` does not parse or verify against `pins`, or is a keybag of this vault from before the
    ///   write (sequence below the pending one): `Retry`, the pending write handed back untouched. Re-GET
    ///   and call again, a bounded number of times. Nothing is recorded in `pins`.
    /// - Otherwise [`PendingWrite::landed_in`] decides. `Landed(T)` hands back the write's result so a
    ///   recovery key can still be shown; `NotLanded` drops the pending write and zeroizes its recovery
    ///   key or new VK. In both, `pins` record the verified `stored` (the vault is not changed): unlock
    ///   the store's keybag to carry on.
    ///
    /// On `Err` (a commit refusal such as `StaleWrite`, `SignerMismatch`, `Rollback` or `Fork` on the
    /// exact match) the pending write is dropped and zeroized. The same read-back rules as `commit_write`
    /// apply.
    pub fn resolve_write<T>(
        &mut self,
        pending: PendingWrite<T>,
        stored: &[u8],
        pins: &mut Pins,
    ) -> Result<Resolution<T>> {
        if <[u8; 32]>::from(Sha256::digest(stored))
            == <[u8; 32]>::from(Sha256::digest(&pending.bytes))
        {
            return self
                .commit_write(pending, stored, pins)
                .map(Resolution::Committed);
        }
        let Ok(kb) = Keybag::parse(stored, pins) else {
            return Ok(Resolution::Retry(Box::new(pending)));
        };
        let ours = kb.vault_id == pending.vault_id && kb.sign_pub == pending.keybag.sign_pub;
        if ours && kb.sequence < pending.keybag.sequence {
            return Ok(Resolution::Retry(Box::new(pending)));
        }
        let landed = pending.landed_in_parsed(stored, &kb);
        if ours {
            pins.record(stored, &kb);
        }
        if landed {
            Ok(Resolution::Landed(pending.outcome))
        } else {
            Ok(Resolution::NotLanded)
        }
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
