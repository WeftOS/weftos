//! Vault-key rotation, key-history walk and lazy re-seal (spec 4.1, 5.3 kind 4, 9.4).

use zeroize::Zeroizing;

use crate::blob::{self, Policy, SecretType};
use crate::error::{Result, VaultError};
use crate::hkdf_keys;
use crate::keybag::{
    KIND_HISTORY, KIND_PASSCODE, KIND_RECOVERY, KIND_SIGNSEED, Pins, Wrapper, WrapperBody,
    wrapper_aad,
};
use crate::normalize;
use crate::passcode_kdf::derive_kek;
use crate::recovery::RecoveryKey;
use crate::seal;
use crate::secrets::{SealRequest, SealedSecret};
use crate::vault::Vault;
use crate::wrap::{PasscodeSeal, open32, passcode_wrapper, recovery_wrapper, seal48};
use crate::write::PendingWrite;

/// What a rotation did to the wrapper list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RotationReport {
    /// The new current generation.
    pub new_generation: u32,
    /// Passcode wrappers re-sealed under the new VK.
    pub passcode_resealed: usize,
    /// Recovery wrappers re-sealed (needs the recovery key).
    pub recovery_resealed: usize,
    /// Wrappers dropped: device wrappers, recovery wrappers without the key, passcode wrappers the
    /// given passcode does not open. Re-enrol them afterwards.
    pub dropped: usize,
    /// Kind 4 entries now in the keybag (one per older generation).
    pub history_entries: usize,
}

/// One secret moved to the current generation (spec 9.4): write `new_secret`, then `tombstone`.
#[derive(Debug, Clone)]
pub struct Resealed {
    /// The secret sealed under the current generation, in its new slot.
    pub new_secret: SealedSecret,
    /// The old slot, now stale.
    pub old_slot: String,
    /// Delete marker for the old slot, at `t/<old hash>` (type 3, empty payload).
    pub tombstone: SealedSecret,
}

fn history_wrapper(
    vault_id: &[u8; 16],
    id: [u8; 16],
    old_gen: u32,
    old_vk: &[u8; 32],
    new_vk: &[u8; 32],
    new_gen: u32,
) -> Result<Wrapper> {
    let key = hkdf_keys::history_key(vault_id, new_vk, new_gen);
    let nonce = seal::random_bytes()?;
    let aad = wrapper_aad(vault_id, &id, KIND_HISTORY, old_gen);
    let ct = seal48(&key, &nonce, &aad, old_vk)?;
    Ok(Wrapper {
        id,
        vk_generation: old_gen,
        body: WrapperBody::KeyHistory { nonce, ct },
    })
}

impl Vault {
    /// The VK of `generation`: the live VK, or an older one opened from its kind 4 entry.
    pub(crate) fn vk_for_generation(&self, generation: u32) -> Result<Zeroizing<[u8; 32]>> {
        let current = self.keybag.current_vk_generation;
        if generation == current {
            return Ok(self.vk.clone());
        }
        let w = self
            .keybag
            .wrappers
            .iter()
            .find(|w| w.kind() == KIND_HISTORY && w.vk_generation == generation)
            .ok_or(VaultError::GenerationMismatch(generation))?;
        let WrapperBody::KeyHistory { nonce, ct } = &w.body else {
            return Err(VaultError::GenerationMismatch(generation));
        };
        let key = hkdf_keys::history_key(&self.keybag.vault_id, &self.vk, current);
        let aad = wrapper_aad(&self.keybag.vault_id, &w.id, KIND_HISTORY, generation);
        open32(&key, nonce, &aad, ct)
    }

    /// Rotate the vault key (spec 9.4).
    ///
    /// **This is not compromise recovery.** The signing seed and `sign_pub` survive rotation and the
    /// passcode wrappers are re-sealed under the same passcode, so a device compromised before the
    /// rotation (which held the old VK, the signing seed and probably the typed passcode) keeps full write
    /// authority over the keybag. Pinned devices refuse a pre-rotation keybag (the generation pin, review
    /// H1), but an unpinned device does not, and the attacker can still re-sign new keybags. Recovering
    /// from a compromise needs a new signing seed, a new passcode and an authenticated pin transition: a
    /// spec change, see `docs/research/vault-v1-rotation-addendum-2026-11-15.md`.
    ///
    /// A new VK takes generation g+1. Passcode wrappers that `passcode` opens are re-sealed under it (fresh
    /// salt and nonce); recovery wrappers are re-sealed only when `recovery` opens them; device wrappers are
    /// dropped (the platform seal needs the device, so re-enrol). The old VK goes into a kind 4 entry sealed
    /// under the new VK's history key, and older kind 4 entries are re-sealed the same way, so every
    /// generation opens straight from the current VK. The signing seed is re-sealed (same seed, same
    /// `sign_pub`), the sequence is bumped and the keybag is re-signed. Blobs are not touched: they open by
    /// walking the history and move with [`Vault::reseal_secret`].
    ///
    /// Returns a [`PendingWrite`] and changes nothing yet (review R2-M1). The new VK lives only inside it,
    /// so no secret can be sealed under the new generation before the store holds a wrapper for it. Store
    /// [`PendingWrite::keybag_bytes`], then [`Vault::commit_write`] installs the new VK and keybag and
    /// advances the pins to the new sequence and generation. If the store refuses, drop it: the vault and
    /// the pins stay at the stored keybag, and the device can re-read the store and rotate again.
    ///
    /// Refuses with [`VaultError::KdfPolicy`] when a passcode wrapper it would re-seal has KDF parameters
    /// below the policy for new wrappers (review M2): change that passcode first with
    /// [`Vault::change_passcode`].
    pub fn rotate_vk(
        &self,
        passcode: &str,
        recovery: Option<&RecoveryKey>,
        pins: &Pins,
    ) -> Result<PendingWrite<RotationReport>> {
        self.rotate_inner(passcode, recovery, pins, true)
    }

    /// [`Vault::rotate_vk`] without the KDF policy check (fast test profile).
    #[cfg(test)]
    pub(crate) fn rotate_vk_unchecked(
        &self,
        passcode: &str,
        recovery: Option<&RecoveryKey>,
        pins: &Pins,
    ) -> Result<PendingWrite<RotationReport>> {
        self.rotate_inner(passcode, recovery, pins, false)
    }

    fn rotate_inner(
        &self,
        passcode: &str,
        recovery: Option<&RecoveryKey>,
        pins: &Pins,
        enforce_policy: bool,
    ) -> Result<PendingWrite<RotationReport>> {
        let vault_id = self.keybag.vault_id;
        let old_gen = self.keybag.current_vk_generation;
        let new_gen = old_gen
            .checked_add(1)
            .ok_or(VaultError::InvalidOperation("generation counter exhausted"))?;
        let new_vk = Zeroizing::new(seal::random_bytes::<32>()?);
        let pass = normalize::passcode(passcode);
        let old_hist = hkdf_keys::history_key(&vault_id, &self.vk, old_gen);
        let mut out: Vec<Wrapper> = Vec::new();
        let mut report = RotationReport {
            new_generation: new_gen,
            passcode_resealed: 0,
            recovery_resealed: 0,
            dropped: 0,
            history_entries: 0,
        };
        for w in &self.keybag.wrappers {
            match &w.body {
                WrapperBody::Passcode {
                    params,
                    salt,
                    nonce,
                    ct,
                } if w.vk_generation == old_gen => {
                    let kek = derive_kek(&pass, salt, *params)?;
                    let aad = wrapper_aad(&vault_id, &w.id, KIND_PASSCODE, old_gen);
                    if open32(&kek, nonce, &aad, ct).is_ok() {
                        if enforce_policy {
                            params.check_new_policy()?;
                        }
                        out.push(passcode_wrapper(PasscodeSeal {
                            vault_id: &vault_id,
                            id: w.id,
                            vk: &new_vk,
                            passcode,
                            params: *params,
                            salt: seal::random_bytes()?,
                            nonce: seal::random_bytes()?,
                            generation: new_gen,
                        })?);
                        report.passcode_resealed += 1;
                    } else {
                        report.dropped += 1;
                    }
                }
                WrapperBody::Recovery { nonce, ct } if w.vk_generation == old_gen => {
                    let key = recovery.filter(|k| {
                        let kek = hkdf_keys::recovery_kek(&vault_id, &k.0);
                        let aad = wrapper_aad(&vault_id, &w.id, KIND_RECOVERY, old_gen);
                        open32(&kek, nonce, &aad, ct).is_ok()
                    });
                    match key {
                        Some(k) => {
                            out.push(recovery_wrapper(
                                &vault_id,
                                w.id,
                                &new_vk,
                                &k.0,
                                seal::random_bytes()?,
                                new_gen,
                            )?);
                            report.recovery_resealed += 1;
                        }
                        None => report.dropped += 1,
                    }
                }
                WrapperBody::KeyHistory { nonce, ct } => {
                    let aad = wrapper_aad(&vault_id, &w.id, KIND_HISTORY, w.vk_generation);
                    let older = open32(&old_hist, nonce, &aad, ct)?;
                    out.push(history_wrapper(
                        &vault_id,
                        w.id,
                        w.vk_generation,
                        &older,
                        &new_vk,
                        new_gen,
                    )?);
                }
                WrapperBody::SigningSeed { .. } => {
                    let seed = Zeroizing::new(self.signing.to_bytes());
                    let key = hkdf_keys::signseed_key(&vault_id, &new_vk);
                    let nonce = seal::random_bytes()?;
                    let aad = wrapper_aad(&vault_id, &w.id, KIND_SIGNSEED, new_gen);
                    let ct = seal48(&key, &nonce, &aad, &seed)?;
                    out.push(Wrapper {
                        id: w.id,
                        vk_generation: new_gen,
                        body: WrapperBody::SigningSeed { nonce, ct },
                    });
                }
                _ => report.dropped += 1,
            }
        }
        if report.passcode_resealed == 0 {
            return Err(VaultError::UnlockFailed);
        }
        out.push(history_wrapper(
            &vault_id,
            seal::random_bytes()?,
            old_gen,
            &self.vk,
            &new_vk,
            new_gen,
        )?);
        report.history_entries = out.iter().filter(|w| w.kind() == KIND_HISTORY).count();
        let pending = self.prepare(pins, Some(new_vk), report, |kb| {
            kb.wrappers = out;
            kb.current_vk_generation = new_gen;
            Ok(())
        })?;
        tracing::debug!(generation = new_gen, "vault key rotation prepared");
        Ok(pending)
    }

    /// Move one secret sealed under an older generation to the current one (spec 9.4). `None` when the
    /// blob is already current. The name hash changes with the generation, so the result carries the new
    /// slot, the stale slot and a delete marker for it. The revision is the old revision plus one.
    ///
    /// `expected_revision` is the slot's latest revision as the caller knows it (from the index or a local
    /// cache). A blob at any other revision is refused with [`VaultError::StaleRevision`] (review R2-L2):
    /// otherwise a store replaying an older valid revision during the lazy pass would have it moved forward
    /// to a fresh-looking current-generation blob, and the tombstone would hide the real latest revision.
    pub fn reseal_secret(
        &self,
        name: &str,
        blob_bytes: &[u8],
        expected_revision: u64,
    ) -> Result<Option<Resealed>> {
        let generation = blob::peek_generation(blob_bytes)?;
        if generation == self.keybag.current_vk_generation {
            return Ok(None);
        }
        let opened = self.open_named(name, blob_bytes)?;
        if opened.revision != expected_revision {
            return Err(VaultError::StaleRevision {
                got: opened.revision,
                expected: expected_revision,
            });
        }
        let revision = opened
            .revision
            .checked_add(1)
            .ok_or(VaultError::InvalidOperation("revision counter exhausted"))?;
        let new_secret = self.seal_secret(&SealRequest {
            name,
            secret_type: opened.secret_type,
            policy: opened.policy,
            payload: &opened.payload,
            revision,
            real_credential: false,
        })?;
        let old_slot = self.slot_for_name_at(name, generation)?;
        let marker_slot = format!("t/{}", &old_slot[2..]);
        let key = hkdf_keys::blob_key(
            &self.keybag.vault_id,
            &self.vk,
            self.keybag.current_vk_generation,
        );
        let p = self.seal_params(&key, &marker_slot, revision)?;
        let tombstone = SealedSecret {
            bytes: blob::seal_blob(&p, SecretType::Note, Policy::Standing, &[])?,
            slot: marker_slot,
        };
        Ok(Some(Resealed {
            new_secret,
            old_slot,
            tombstone,
        }))
    }

    /// [`Vault::reseal_secret`] over `(name, blob, expected_revision)` items, skipping blobs that are
    /// already current.
    pub fn reseal_all(&self, items: &[(&str, &[u8], u64)]) -> Result<Vec<Resealed>> {
        let mut out = Vec::new();
        for (name, bytes, expected) in items {
            out.extend(self.reseal_secret(name, bytes, *expected)?);
        }
        Ok(out)
    }

    /// Older generations still reachable through kind 4 entries, ascending.
    pub fn history_generations(&self) -> Vec<u32> {
        let mut g: Vec<u32> = self
            .keybag
            .wrappers
            .iter()
            .filter(|w| w.kind() == KIND_HISTORY)
            .map(|w| w.vk_generation)
            .collect();
        g.sort_unstable();
        g
    }

    /// Remove the kind 4 entry for `generation` once no blob uses it any more (spec 9.4). Bumps the
    /// sequence; returns a [`PendingWrite`]. Checking that the lazy re-seal pass is complete is the
    /// caller's job: a blob still at `generation` stops opening afterwards.
    pub fn drop_history(&self, generation: u32, pins: &Pins) -> Result<PendingWrite<()>> {
        self.prepare(pins, None, (), |kb| {
            let pos = kb
                .wrappers
                .iter()
                .position(|w| w.kind() == KIND_HISTORY && w.vk_generation == generation)
                .ok_or(VaultError::InvalidOperation("no such history entry"))?;
            kb.wrappers.remove(pos);
            Ok(())
        })
    }
}
