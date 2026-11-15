//! Per-secret seal and open on an unlocked [`Vault`] (spec section 7).

use crate::blob::{self, OpenedSecret, Policy, SealParams, SecretType};
use crate::error::{Result, VaultError};
use crate::hkdf_keys;
use crate::normalize;
use crate::seal;
use crate::vault::Vault;

/// One secret to seal. `Debug` shows lengths only.
pub struct SealRequest<'a> {
    /// Secret name (normalized to NFC, 1 to 255 bytes).
    pub name: &'a str,
    /// Item type (not `Index`).
    pub secret_type: SecretType,
    /// Use policy.
    pub policy: Policy,
    /// The secret bytes, at most 65530. Shown by `Debug` only as a length.
    pub payload: &'a [u8],
    /// Per-slot revision, starting at 1 and +1 on each rewrite.
    pub revision: u64,
    /// True for a real credential. Refused while the keybag carries the DRAFT flag.
    pub real_credential: bool,
}

impl core::fmt::Debug for SealRequest<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SealRequest")
            .field("name_len", &self.name.len())
            .field("secret_type", &self.secret_type)
            .field("policy", &self.policy)
            .field("payload_len", &self.payload.len())
            .field("revision", &self.revision)
            .field("real_credential", &self.real_credential)
            .finish()
    }
}

/// A sealed blob and the slot it belongs to.
#[derive(Debug, Clone)]
pub struct SealedSecret {
    /// Slot under the vault root: `s/<name_hash_hex>` or `index`.
    pub slot: String,
    /// The blob bytes to store.
    pub bytes: Vec<u8>,
}

impl Vault {
    /// Slot for a secret name under the current generation.
    pub fn slot_for_name(&self, name: &str) -> Result<String> {
        let nk = hkdf_keys::name_key(
            &self.keybag.vault_id,
            &self.vk,
            self.keybag.current_vk_generation,
        );
        Ok(format!(
            "s/{}",
            blob::name_hash_hex(&nk, &normalize::name(name))?
        ))
    }

    fn seal_params<'a>(
        &'a self,
        key: &'a [u8; 32],
        slot: &'a str,
        revision: u64,
    ) -> Result<SealParams<'a>> {
        Ok(SealParams {
            vault_id: &self.keybag.vault_id,
            blob_key: key,
            generation: self.keybag.current_vk_generation,
            slot,
            revision,
            nonce: seal::random_bytes()?,
        })
    }

    /// Seal one secret into its slot (spec 7). A DRAFT vault refuses `real_credential`.
    pub fn seal_secret(&self, req: &SealRequest<'_>) -> Result<SealedSecret> {
        if req.real_credential && self.is_draft() {
            return Err(VaultError::DraftRefusesRealCredentials);
        }
        if req.secret_type == SecretType::Index {
            return Err(VaultError::InvalidOperation("use seal_index for the index"));
        }
        let slot = self.slot_for_name(req.name)?;
        let key = hkdf_keys::blob_key(
            &self.keybag.vault_id,
            &self.vk,
            self.keybag.current_vk_generation,
        );
        let p = self.seal_params(&key, &slot, req.revision)?;
        let bytes = blob::seal_blob(&p, req.secret_type, req.policy, req.payload)?;
        tracing::debug!(
            payload_len = req.payload.len(),
            blob_len = bytes.len(),
            "secret sealed"
        );
        Ok(SealedSecret { slot, bytes })
    }

    /// Seal the index blob (slot `index`, type 16).
    pub fn seal_index(&self, payload: &[u8], revision: u64) -> Result<SealedSecret> {
        let key = hkdf_keys::blob_key(
            &self.keybag.vault_id,
            &self.vk,
            self.keybag.current_vk_generation,
        );
        let slot = "index".to_string();
        let p = self.seal_params(&key, &slot, revision)?;
        let bytes = blob::seal_blob(&p, SecretType::Index, Policy::Standing, payload)?;
        Ok(SealedSecret { slot, bytes })
    }

    /// Open the blob stored at `slot`. A blob moved from another slot or vault fails.
    pub fn open_secret(&self, slot: &str, blob_bytes: &[u8]) -> Result<OpenedSecret> {
        let key = hkdf_keys::blob_key(
            &self.keybag.vault_id,
            &self.vk,
            self.keybag.current_vk_generation,
        );
        let out = blob::open_blob(
            &self.keybag.vault_id,
            &key,
            self.keybag.current_vk_generation,
            slot,
            blob_bytes,
        );
        tracing::debug!(ok = out.is_ok(), "secret open attempted");
        out
    }

    /// Open the blob for a secret by name.
    pub fn open_named(&self, name: &str, blob_bytes: &[u8]) -> Result<OpenedSecret> {
        self.open_secret(&self.slot_for_name(name)?, blob_bytes)
    }
}
