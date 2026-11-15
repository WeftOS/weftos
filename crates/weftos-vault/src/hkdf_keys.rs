//! HKDF-SHA256 key derivation (spec 3 and 4.2). The only module that touches `hkdf`.

use hkdf::Hkdf;
use sha2::Sha256;
use zeroize::Zeroizing;

pub(crate) type Key = Zeroizing<[u8; 32]>;

fn derive(salt: &[u8], ikm: &[u8], info: &[&[u8]]) -> Key {
    let hk = Hkdf::<Sha256>::new(Some(salt), ikm);
    let mut out = Zeroizing::new([0u8; 32]);
    hk.expand_multi_info(info, out.as_mut())
        .expect("32 bytes is a valid HKDF-SHA256 output length");
    out
}

pub(crate) fn blob_key(vault_id: &[u8; 16], vk: &[u8; 32], generation: u32) -> Key {
    derive(
        vault_id,
        vk,
        &[b"weftos-vault/v1/blob-key", &generation.to_be_bytes()],
    )
}

pub(crate) fn name_key(vault_id: &[u8; 16], vk: &[u8; 32], generation: u32) -> Key {
    derive(
        vault_id,
        vk,
        &[b"weftos-vault/v1/name-key", &generation.to_be_bytes()],
    )
}

pub(crate) fn signseed_key(vault_id: &[u8; 16], vk: &[u8; 32]) -> Key {
    derive(vault_id, vk, &[b"weftos-vault/v1/signseed-key"])
}

pub(crate) fn recovery_kek(vault_id: &[u8; 16], recovery_key: &[u8; 32]) -> Key {
    derive(vault_id, recovery_key, &[b"weftos-vault/v1/recovery-kek"])
}
