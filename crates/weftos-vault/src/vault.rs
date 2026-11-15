//! The vault: create, unlock, keybag changes and per-secret seal/open (spec section 9).

use ed25519_dalek::SigningKey;
use zeroize::Zeroizing;

use crate::error::{Result, VaultError};
use crate::hkdf_keys::{self, Key};
use crate::keybag::{
    FLAG_DRAFT, KIND_DEVICE, KIND_PASSCODE, KIND_RECOVERY, KIND_SIGNSEED, Keybag, Pins, Wrapper,
    WrapperBody, wrapper_aad,
};
use crate::normalize;
use crate::passcode_kdf::{KdfParams, derive_kek};
use crate::recovery::RecoveryKey;
use crate::seal::{self, NONCE_LEN};
use crate::wrap::{PasscodeSeal, open32, passcode_wrapper, recovery_wrapper, seal48};
use crate::write::PendingWrite;

pub(crate) fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// An unlocked vault: the VK and signing key in memory, plus the current signed keybag.
pub struct Vault {
    pub(crate) keybag: Keybag,
    pub(crate) bytes: Vec<u8>,
    pub(crate) vk: Zeroizing<[u8; 32]>,
    pub(crate) signing: SigningKey,
}

impl core::fmt::Debug for Vault {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Vault")
            .field("sequence", &self.keybag.sequence)
            .field("wrappers", &self.keybag.wrappers.len())
            .field("draft", &self.keybag.is_draft())
            .field("keys", &"<redacted>")
            .finish()
    }
}

/// A newly created vault and the recovery key to show the user once.
#[derive(Debug)]
pub struct NewVault {
    /// The unlocked vault.
    pub vault: Vault,
    /// Shown once; WeftOS never stores it.
    pub recovery_key: RecoveryKey,
    /// The creating device's pins for this vault, already recording the new keybag. Persist them and
    /// pass them to every later unlock and write, so the creator never trusts the store on first use.
    pub pins: Pins,
}

/// Everything random about a new vault, injectable so test vectors reproduce byte for byte. The key
/// material is zeroized on drop (review L3).
pub(crate) struct CreateInputs {
    pub vault_id: [u8; 16],
    pub vk: Zeroizing<[u8; 32]>,
    pub signing_seed: Zeroizing<[u8; 32]>,
    pub salt: [u8; 16],
    pub recovery: Zeroizing<[u8; 32]>,
    pub wrapper_ids: [[u8; 16]; 3],
    pub nonces: [[u8; NONCE_LEN]; 3],
    pub now: u64,
    pub params: KdfParams,
}

impl Vault {
    /// Create a vault (spec 9.1) with a random VK, signing seed and recovery key. The new passcode
    /// wrapper must meet the KDF policy for new wrappers.
    ///
    /// The keybag always carries the DRAFT flag, so the vault refuses real credentials: the format has
    /// not had its external review (spec Q6, review R2-M2). There is no way to ask for a vault without
    /// the flag:
    ///
    /// ```no_run
    /// # use weftos_vault::{KdfParams, Vault};
    /// let nv = Vault::create("FAKE-passcode-0001", KdfParams::RECOMMENDED).unwrap();
    /// assert!(nv.vault.is_draft());
    /// ```
    ///
    /// ```compile_fail,E0061
    /// # use weftos_vault::{KdfParams, Vault};
    /// let nv = Vault::create("FAKE-passcode-0001", KdfParams::RECOMMENDED, false);
    /// ```
    pub fn create(passcode: &str, params: KdfParams) -> Result<NewVault> {
        params.check_new_policy()?;
        Self::create_unchecked(passcode, params)
    }

    pub(crate) fn create_unchecked(passcode: &str, params: KdfParams) -> Result<NewVault> {
        Self::create_with(
            passcode,
            CreateInputs {
                vault_id: seal::random_bytes()?,
                vk: Zeroizing::new(seal::random_bytes()?),
                signing_seed: Zeroizing::new(seal::random_bytes()?),
                salt: seal::random_bytes()?,
                recovery: Zeroizing::new(seal::random_bytes()?),
                wrapper_ids: [
                    seal::random_bytes()?,
                    seal::random_bytes()?,
                    seal::random_bytes()?,
                ],
                nonces: [
                    seal::random_bytes()?,
                    seal::random_bytes()?,
                    seal::random_bytes()?,
                ],
                now: now_unix(),
                params,
            },
        )
    }

    pub(crate) fn create_with(passcode: &str, i: CreateInputs) -> Result<NewVault> {
        let (vk, seed, recovery) = (i.vk, i.signing_seed, i.recovery);
        let signing = SigningKey::from_bytes(&seed);
        let pass = passcode_wrapper(PasscodeSeal {
            vault_id: &i.vault_id,
            id: i.wrapper_ids[0],
            vk: &vk,
            passcode,
            params: i.params,
            salt: i.salt,
            nonce: i.nonces[0],
            generation: 1,
        })?;
        let rec = recovery_wrapper(
            &i.vault_id,
            i.wrapper_ids[1],
            &vk,
            &recovery,
            i.nonces[1],
            1,
        )?;
        let sk = hkdf_keys::signseed_key(&i.vault_id, &vk);
        let aad = wrapper_aad(&i.vault_id, &i.wrapper_ids[2], KIND_SIGNSEED, 1);
        let ct = seal48(&sk, &i.nonces[2], &aad, &seed)?;
        let seed_w = Wrapper {
            id: i.wrapper_ids[2],
            vk_generation: 1,
            body: WrapperBody::SigningSeed {
                nonce: i.nonces[2],
                ct,
            },
        };
        let keybag = Keybag {
            flags: FLAG_DRAFT,
            vault_id: i.vault_id,
            sequence: 1,
            created_unix: i.now,
            updated_unix: i.now,
            current_vk_generation: 1,
            sign_pub: signing.verifying_key().to_bytes(),
            wrappers: vec![pass, rec, seed_w],
        };
        keybag.validate_structure()?;
        let bytes = keybag.sign(&signing);
        let mut pins = Pins::default();
        pins.record(&bytes, &keybag);
        tracing::debug!(sequence = 1, wrappers = 3, "vault created");
        Ok(NewVault {
            vault: Vault {
                keybag,
                bytes,
                vk,
                signing,
            },
            recovery_key: RecoveryKey(recovery),
            pins,
        })
    }

    /// Unlock by passcode (spec 9.2). Validates the keybag against `pins` first, then runs Argon2id
    /// with each passcode wrapper's stored parameters. On success `pins` records the signer and sequence.
    pub fn unlock(bytes: &[u8], passcode: &str, pins: &mut Pins) -> Result<Vault> {
        let kb = Keybag::parse(bytes, pins)?;
        let pass = normalize::passcode(passcode);
        for w in &kb.wrappers {
            if let WrapperBody::Passcode {
                params,
                salt,
                nonce,
                ct,
            } = &w.body
                && w.vk_generation == kb.current_vk_generation
            {
                let kek = derive_kek(&pass, salt, *params)?;
                let aad = wrapper_aad(&kb.vault_id, &w.id, KIND_PASSCODE, w.vk_generation);
                if let Ok(vk) = open32(&kek, nonce, &aad, ct) {
                    return Self::finish_unlock(kb, bytes, vk, pins);
                }
            }
        }
        tracing::debug!("unlock failed");
        Err(VaultError::UnlockFailed)
    }

    /// Unlock with a recovery key.
    pub fn unlock_with_recovery(bytes: &[u8], key: &RecoveryKey, pins: &mut Pins) -> Result<Vault> {
        let kb = Keybag::parse(bytes, pins)?;
        let kek = hkdf_keys::recovery_kek(&kb.vault_id, &key.0);
        for w in &kb.wrappers {
            if let WrapperBody::Recovery { nonce, ct } = &w.body
                && w.vk_generation == kb.current_vk_generation
            {
                let aad = wrapper_aad(&kb.vault_id, &w.id, KIND_RECOVERY, w.vk_generation);
                if let Ok(vk) = open32(&kek, nonce, &aad, ct) {
                    return Self::finish_unlock(kb, bytes, vk, pins);
                }
            }
        }
        Err(VaultError::UnlockFailed)
    }

    fn finish_unlock(
        kb: Keybag,
        bytes: &[u8],
        vk: Zeroizing<[u8; 32]>,
        pins: &mut Pins,
    ) -> Result<Vault> {
        let sk: Key = hkdf_keys::signseed_key(&kb.vault_id, &vk);
        let w = kb
            .wrappers
            .iter()
            .find(|w| w.kind() == KIND_SIGNSEED)
            .ok_or(VaultError::Malformed("no signing seed wrapper"))?;
        let WrapperBody::SigningSeed { nonce, ct } = &w.body else {
            return Err(VaultError::Malformed("no signing seed wrapper"));
        };
        let aad = wrapper_aad(&kb.vault_id, &w.id, KIND_SIGNSEED, w.vk_generation);
        let seed = open32(&sk, nonce, &aad, ct)?;
        let signing = SigningKey::from_bytes(&seed);
        if signing.verifying_key().to_bytes() != kb.sign_pub {
            return Err(VaultError::SigningKeyMismatch);
        }
        pins.record(bytes, &kb);
        tracing::debug!(sequence = kb.sequence, "vault unlocked");
        Ok(Vault {
            bytes: bytes.to_vec(),
            keybag: kb,
            vk,
            signing,
        })
    }

    /// The signed keybag this vault holds: the base for `put_if_match` when storing a
    /// [`PendingWrite`].
    pub fn keybag_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The parsed keybag.
    pub fn keybag(&self) -> &Keybag {
        &self.keybag
    }

    /// Whether the keybag carries the DRAFT flag.
    pub fn is_draft(&self) -> bool {
        self.keybag.is_draft()
    }

    /// Ids of current-generation passcode wrappers whose KDF parameters are below the policy for new
    /// wrappers ([`KdfParams::check_new_policy`]: 64 MiB, t=3 since the owner's 2026-11-15 floor change).
    /// Check it after every unlock: a non-empty list means "ask the user to set the passcode again".
    ///
    /// Legacy wrappers (written under the older 19 MiB floor of spec 3.1) are treated like this:
    /// - `unlock` still opens them, so no existing vault is locked out by the floor change;
    /// - [`Vault::rotate_vk`] refuses with [`VaultError::KdfPolicy`] instead of re-sealing them with their
    ///   old parameters, and [`Vault::change_passcode`] refuses any parameters below the floor, so no
    ///   write ever produces a new below-floor wrapper;
    /// - [`Vault::change_passcode`] at the floor replaces the wrapper and clears it from this list;
    /// - writes that carry the wrapper unchanged (adding a recovery or device wrapper, removing another
    ///   wrapper, dropping history) are allowed: they do not touch its KEK or ciphertext, and refusing
    ///   them would block safety actions such as removing a lost device.
    ///
    /// See review M2 and `docs/research/vault-v1-kdf-floor-2026-11-15.md`.
    pub fn below_policy_wrappers(&self) -> Vec<[u8; 16]> {
        self.keybag
            .wrappers
            .iter()
            .filter(|w| w.vk_generation == self.keybag.current_vk_generation)
            .filter_map(|w| match &w.body {
                WrapperBody::Passcode { params, .. } if params.check_new_policy().is_err() => {
                    Some(w.id)
                }
                _ => None,
            })
            .collect()
    }

    /// Re-seal one passcode wrapper under a new passcode (spec 9.3): new salt and nonce, sequence
    /// bumped, keybag re-signed. No blob changes. The new passcode must meet the passcode rules
    /// (`PasscodeRejected`). Returns a [`PendingWrite`]: store it, then [`Vault::commit_write`].
    pub fn change_passcode(
        &self,
        wrapper_id: &[u8; 16],
        new_passcode: &str,
        params: KdfParams,
        pins: &Pins,
    ) -> Result<PendingWrite<()>> {
        params.check_new_policy()?;
        self.change_passcode_with(wrapper_id, new_passcode, params, pins)
    }

    /// [`Vault::change_passcode`] without the KDF policy check (fast test profile).
    #[cfg(test)]
    pub(crate) fn change_passcode_unchecked(
        &self,
        wrapper_id: &[u8; 16],
        new_passcode: &str,
        params: KdfParams,
        pins: &Pins,
    ) -> Result<PendingWrite<()>> {
        self.change_passcode_with(wrapper_id, new_passcode, params, pins)
    }

    fn change_passcode_with(
        &self,
        wrapper_id: &[u8; 16],
        new_passcode: &str,
        params: KdfParams,
        pins: &Pins,
    ) -> Result<PendingWrite<()>> {
        let (vk, vault_id) = (self.vk.clone(), self.keybag.vault_id);
        let (salt, nonce) = (seal::random_bytes()?, seal::random_bytes()?);
        self.prepare(pins, None, (), |kb| {
            let w = kb
                .wrappers
                .iter_mut()
                .find(|w| &w.id == wrapper_id && w.kind() == KIND_PASSCODE)
                .ok_or(VaultError::InvalidOperation("no such passcode wrapper"))?;
            *w = passcode_wrapper(PasscodeSeal {
                vault_id: &vault_id,
                id: w.id,
                vk: &vk,
                passcode: new_passcode,
                params,
                salt,
                nonce,
                generation: w.vk_generation,
            })?;
            Ok(())
        })
    }

    /// Add a recovery wrapper. [`Vault::commit_write`] returns its key (show it once, after the store
    /// took the keybag).
    pub fn add_recovery_wrapper(&self, pins: &Pins) -> Result<PendingWrite<RecoveryKey>> {
        let key = RecoveryKey::random()?;
        let (id, nonce) = (seal::random_bytes()?, seal::random_bytes()?);
        let (vk, vault_id, generation) = (
            self.vk.clone(),
            self.keybag.vault_id,
            self.keybag.current_vk_generation,
        );
        let wrapper = recovery_wrapper(&vault_id, id, &vk, &key.0, nonce, generation)?;
        self.prepare(pins, None, key, |kb| {
            kb.wrappers.push(wrapper);
            Ok(())
        })
    }

    /// Add a device wrapper (platform 1 = Apple, 2 = Android). `make_blob` receives the wrapper id,
    /// the wrapper AAD and the VK, and returns the platform-sealed inner blob. [`Vault::commit_write`]
    /// returns the new wrapper id.
    pub fn add_device_wrapper(
        &self,
        platform: u8,
        key_ref: &[u8],
        make_blob: impl FnOnce(&[u8; 16], &[u8], &[u8; 32]) -> Result<Vec<u8>>,
        pins: &Pins,
    ) -> Result<PendingWrite<[u8; 16]>> {
        if !matches!(platform, 1 | 2) {
            return Err(VaultError::RejectedPlatform(platform));
        }
        if key_ref.len() > 256 {
            return Err(VaultError::TooLarge("key_ref"));
        }
        let id: [u8; 16] = seal::random_bytes()?;
        let (vk, vault_id, generation) = (
            self.vk.clone(),
            self.keybag.vault_id,
            self.keybag.current_vk_generation,
        );
        let aad = wrapper_aad(&vault_id, &id, KIND_DEVICE, generation);
        let blob = make_blob(&id, &aad, &vk)?;
        if blob.len() > 16000 {
            return Err(VaultError::TooLarge("device blob"));
        }
        self.prepare(pins, None, id, |kb| {
            kb.wrappers.push(Wrapper {
                id,
                vk_generation: generation,
                body: WrapperBody::Device {
                    platform,
                    key_ref: key_ref.to_vec(),
                    blob,
                },
            });
            Ok(())
        })
    }

    /// Remove a wrapper. The signing seed wrapper and the last current-generation passcode wrapper
    /// cannot be removed. Once committed, this device refuses the keybag that still had the wrapper.
    pub fn remove_wrapper(&self, id: &[u8; 16], pins: &Pins) -> Result<PendingWrite<()>> {
        self.prepare(pins, None, (), |kb| {
            let pos = kb
                .wrappers
                .iter()
                .position(|w| &w.id == id)
                .ok_or(VaultError::InvalidOperation("no such wrapper"))?;
            if kb.wrappers[pos].kind() == KIND_SIGNSEED {
                return Err(VaultError::InvalidOperation(
                    "the signing seed wrapper cannot be removed",
                ));
            }
            kb.wrappers.remove(pos);
            Ok(())
        })
    }
}
