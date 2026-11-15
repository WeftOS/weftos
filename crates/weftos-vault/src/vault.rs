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

fn seal48(key: &[u8; 32], nonce: &[u8; NONCE_LEN], aad: &[u8], pt: &[u8; 32]) -> Result<[u8; 48]> {
    seal::encrypt(key, nonce, aad, pt)?
        .try_into()
        .map_err(|_| VaultError::AuthFailed)
}

fn open32(
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
}

/// Everything random about a new vault, injectable so test vectors reproduce byte for byte.
pub(crate) struct CreateInputs {
    pub vault_id: [u8; 16],
    pub vk: [u8; 32],
    pub signing_seed: [u8; 32],
    pub salt: [u8; 16],
    pub recovery: [u8; 32],
    pub wrapper_ids: [[u8; 16]; 3],
    pub nonces: [[u8; NONCE_LEN]; 3],
    pub now: u64,
    pub draft: bool,
    pub params: KdfParams,
}

struct PasscodeSeal<'a> {
    vault_id: &'a [u8; 16],
    id: [u8; 16],
    vk: &'a [u8; 32],
    passcode: &'a str,
    params: KdfParams,
    salt: [u8; 16],
    nonce: [u8; NONCE_LEN],
    generation: u32,
}

fn passcode_wrapper(s: PasscodeSeal<'_>) -> Result<Wrapper> {
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

fn recovery_wrapper(
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
                vk: seal::random_bytes()?,
                signing_seed: seal::random_bytes()?,
                salt: seal::random_bytes()?,
                recovery: seal::random_bytes()?,
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
        let vk = Zeroizing::new(i.vk);
        let seed = Zeroizing::new(i.signing_seed);
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
            &i.recovery,
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
        tracing::debug!(sequence = 1, wrappers = 3, draft = i.draft, "vault created");
        Ok(NewVault {
            vault: Vault {
                keybag,
                bytes,
                vk,
                signing,
            },
            recovery_key: RecoveryKey::from_bytes(i.recovery),
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

    fn commit(&mut self, f: impl FnOnce(&mut Keybag) -> Result<()>) -> Result<&[u8]> {
        let mut kb = self.keybag.clone();
        f(&mut kb)?;
        kb.sequence += 1;
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
        self.keybag = kb;
        self.bytes = bytes;
        Ok(&self.bytes)
    }

    /// Re-seal one passcode wrapper under a new passcode (spec 9.3): new salt and nonce, sequence
    /// bumped, keybag re-signed. No blob changes. Returns the new keybag bytes.
    pub fn change_passcode(
        &mut self,
        wrapper_id: &[u8; 16],
        new_passcode: &str,
        params: KdfParams,
    ) -> Result<&[u8]> {
        params.check_new_policy()?;
        self.change_passcode_unchecked(wrapper_id, new_passcode, params)
    }

    pub(crate) fn change_passcode_unchecked(
        &mut self,
        wrapper_id: &[u8; 16],
        new_passcode: &str,
        params: KdfParams,
    ) -> Result<&[u8]> {
        let (vk, vault_id) = (self.vk.clone(), self.keybag.vault_id);
        let (salt, nonce) = (seal::random_bytes()?, seal::random_bytes()?);
        self.commit(|kb| {
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

    /// Add a recovery wrapper and return its key (shown once). The new keybag is `keybag_bytes()`.
    pub fn add_recovery_wrapper(&mut self) -> Result<RecoveryKey> {
        let key = RecoveryKey::random()?;
        let (id, nonce) = (seal::random_bytes()?, seal::random_bytes()?);
        let (vk, vault_id, generation) = (
            self.vk.clone(),
            self.keybag.vault_id,
            self.keybag.current_vk_generation,
        );
        self.commit(|kb| {
            kb.wrappers.push(recovery_wrapper(
                &vault_id, id, &vk, &key.0, nonce, generation,
            )?);
            Ok(())
        })?;
        Ok(key)
    }

    /// Add a device wrapper (platform 1 = Apple, 2 = Android). `make_blob` receives the wrapper id,
    /// the wrapper AAD and the VK, and returns the platform-sealed inner blob.
    pub fn add_device_wrapper(
        &mut self,
        platform: u8,
        key_ref: &[u8],
        make_blob: impl FnOnce(&[u8; 16], &[u8], &[u8; 32]) -> Result<Vec<u8>>,
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
        self.commit(|kb| {
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
    pub fn remove_wrapper(&mut self, id: &[u8; 16]) -> Result<&[u8]> {
        self.commit(|kb| {
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
