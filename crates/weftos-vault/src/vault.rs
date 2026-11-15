//! The vault: create, unlock, keybag changes and per-secret seal/open (spec section 9).

use ed25519_dalek::SigningKey;
use zeroize::Zeroizing;

use crate::error::{Result, VaultError};
use crate::hkdf_keys::{self, Key};
use crate::keybag::{
    FLAG_DRAFT, KIND_DEVICE, KIND_PASSCODE, KIND_RECOVERY, KIND_SIGNSEED, Keybag, MAX_LEN, Pins,
    Wrapper, WrapperBody, wrapper_aad,
};
use crate::normalize;
use crate::passcode_kdf::{KdfParams, derive_kek};
use crate::recovery::RecoveryKey;
use crate::seal::{self, NONCE_LEN};

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub(crate) fn seal48(
    key: &[u8; 32],
    nonce: &[u8; NONCE_LEN],
    aad: &[u8],
    pt: &[u8; 32],
) -> Result<[u8; 48]> {
    seal::encrypt(key, nonce, aad, pt)?
        .try_into()
        .map_err(|_| VaultError::AuthFailed)
}

pub(crate) fn open32(
    key: &[u8; 32],
    nonce: &[u8; NONCE_LEN],
    aad: &[u8],
    ct: &[u8; 48],
) -> Result<Zeroizing<[u8; 32]>> {
    let pt = seal::decrypt(key, nonce, aad, ct)?;
    let mut out = Zeroizing::new([0u8; 32]);
    if pt.len() != 32 {
        return Err(VaultError::AuthFailed);
    }
    out.copy_from_slice(&pt);
    Ok(out)
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
    pub draft: bool,
    pub params: KdfParams,
}

pub(crate) struct PasscodeSeal<'a> {
    pub vault_id: &'a [u8; 16],
    pub id: [u8; 16],
    pub vk: &'a [u8; 32],
    pub passcode: &'a str,
    pub params: KdfParams,
    pub salt: [u8; 16],
    pub nonce: [u8; NONCE_LEN],
    pub generation: u32,
}

pub(crate) fn passcode_wrapper(s: PasscodeSeal<'_>) -> Result<Wrapper> {
    normalize::check_new_passcode(s.passcode)?;
    let kek = derive_kek(&normalize::passcode(s.passcode), &s.salt, s.params)?;
    let aad = wrapper_aad(s.vault_id, &s.id, KIND_PASSCODE, s.generation);
    let ct = seal48(&kek, &s.nonce, &aad, s.vk)?;
    Ok(Wrapper {
        id: s.id,
        vk_generation: s.generation,
        body: WrapperBody::Passcode {
            params: s.params,
            salt: s.salt,
            nonce: s.nonce,
            ct,
        },
    })
}

pub(crate) fn recovery_wrapper(
    vault_id: &[u8; 16],
    id: [u8; 16],
    vk: &[u8; 32],
    recovery: &[u8; 32],
    nonce: [u8; NONCE_LEN],
    generation: u32,
) -> Result<Wrapper> {
    let kek = hkdf_keys::recovery_kek(vault_id, recovery);
    let aad = wrapper_aad(vault_id, &id, KIND_RECOVERY, generation);
    let ct = seal48(&kek, &nonce, &aad, vk)?;
    Ok(Wrapper {
        id,
        vk_generation: generation,
        body: WrapperBody::Recovery { nonce, ct },
    })
}

impl Vault {
    /// Create a vault (spec 9.1) with a random VK, signing seed and recovery key. The new passcode
    /// wrapper must meet the KDF policy for new wrappers. The keybag carries the DRAFT flag when `draft`.
    pub fn create(passcode: &str, params: KdfParams, draft: bool) -> Result<NewVault> {
        params.check_new_policy()?;
        Self::create_unchecked(passcode, params, draft)
    }

    pub(crate) fn create_unchecked(
        passcode: &str,
        params: KdfParams,
        draft: bool,
    ) -> Result<NewVault> {
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
                draft,
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
            flags: if i.draft { FLAG_DRAFT } else { 0 },
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
        tracing::debug!(sequence = 1, wrappers = 3, draft = i.draft, "vault created");
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

    /// The current signed keybag bytes, to store with `put_if_match`.
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
    /// wrappers. `unlock` still opens them (they are format-valid), but [`Vault::rotate_vk`] refuses to
    /// carry them forward; the caller should prompt for [`Vault::change_passcode`] (review M2, L4).
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

    /// Apply `f` to a copy of the keybag, bump the sequence, re-sign, and record the result in `pins`
    /// (review M1: the device's own writes advance its pins, so the store cannot replay the keybag from
    /// before this write). Nothing changes, in the vault or in `pins`, if any step fails.
    pub(crate) fn commit(
        &mut self,
        pins: &mut Pins,
        f: impl FnOnce(&mut Keybag) -> Result<()>,
    ) -> Result<&[u8]> {
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
            "keybag rewritten"
        );
        pins.record(&bytes, &kb);
        self.keybag = kb;
        self.bytes = bytes;
        Ok(&self.bytes)
    }

    /// Re-seal one passcode wrapper under a new passcode (spec 9.3): new salt and nonce, sequence
    /// bumped, keybag re-signed, `pins` advanced. No blob changes. Returns the new keybag bytes. The new
    /// passcode must be non-empty and use only assigned, non-control code points.
    pub fn change_passcode(
        &mut self,
        wrapper_id: &[u8; 16],
        new_passcode: &str,
        params: KdfParams,
        pins: &mut Pins,
    ) -> Result<&[u8]> {
        params.check_new_policy()?;
        self.change_passcode_with(wrapper_id, new_passcode, params, pins)
    }

    /// [`Vault::change_passcode`] without the KDF policy check (fast test profile).
    #[cfg(test)]
    pub(crate) fn change_passcode_unchecked(
        &mut self,
        wrapper_id: &[u8; 16],
        new_passcode: &str,
        params: KdfParams,
        pins: &mut Pins,
    ) -> Result<&[u8]> {
        self.change_passcode_with(wrapper_id, new_passcode, params, pins)
    }

    fn change_passcode_with(
        &mut self,
        wrapper_id: &[u8; 16],
        new_passcode: &str,
        params: KdfParams,
        pins: &mut Pins,
    ) -> Result<&[u8]> {
        let (vk, vault_id) = (self.vk.clone(), self.keybag.vault_id);
        let (salt, nonce) = (seal::random_bytes()?, seal::random_bytes()?);
        self.commit(pins, |kb| {
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

    /// Add a recovery wrapper and return its key (shown once). The new keybag is `keybag_bytes()`;
    /// `pins` is advanced.
    pub fn add_recovery_wrapper(&mut self, pins: &mut Pins) -> Result<RecoveryKey> {
        let key = RecoveryKey::random()?;
        let (id, nonce) = (seal::random_bytes()?, seal::random_bytes()?);
        let (vk, vault_id, generation) = (
            self.vk.clone(),
            self.keybag.vault_id,
            self.keybag.current_vk_generation,
        );
        self.commit(pins, |kb| {
            kb.wrappers.push(recovery_wrapper(
                &vault_id, id, &vk, &key.0, nonce, generation,
            )?);
            Ok(())
        })?;
        Ok(key)
    }

    /// Add a device wrapper (platform 1 = Apple, 2 = Android). `make_blob` receives the wrapper id,
    /// the wrapper AAD and the VK, and returns the platform-sealed inner blob. `pins` is advanced.
    pub fn add_device_wrapper(
        &mut self,
        platform: u8,
        key_ref: &[u8],
        make_blob: impl FnOnce(&[u8; 16], &[u8], &[u8; 32]) -> Result<Vec<u8>>,
        pins: &mut Pins,
    ) -> Result<[u8; 16]> {
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
        self.commit(pins, |kb| {
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
        })?;
        Ok(id)
    }

    /// Remove a wrapper. The signing seed wrapper and the last passcode wrapper cannot be removed.
    /// `pins` is advanced, so this device refuses the keybag that still had the wrapper.
    pub fn remove_wrapper(&mut self, id: &[u8; 16], pins: &mut Pins) -> Result<&[u8]> {
        self.commit(pins, |kb| {
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
