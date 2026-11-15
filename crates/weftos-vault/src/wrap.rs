//! Wrapper sealing helpers shared by create, unlock, wrapper changes and rotation (spec 5.2, 5.3).

use zeroize::Zeroizing;

use crate::error::{Result, VaultError};
use crate::hkdf_keys;
use crate::keybag::{KIND_PASSCODE, KIND_RECOVERY, Wrapper, WrapperBody, wrapper_aad};
use crate::normalize;
use crate::passcode_kdf::{KdfParams, derive_kek};
use crate::seal::{self, NONCE_LEN};

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
