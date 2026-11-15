//! Rollback pins: what a device has already seen for one vault (spec 5.6 step 4, review H1 and M1).

use sha2::{Digest, Sha256};

use crate::error::{Result, VaultError};
use crate::keybag::Keybag;

/// What a device has already seen for one vault: the signer pin, the highest keybag and the highest
/// VK generation.
///
/// Pins carry no `vault_id`: a caller keeps one `Pins` per vault and must not share it between vaults.
/// Persist them with [`Pins::to_bytes`] after every unlock and after every write made through
/// [`crate::Vault`] (the write methods advance them, review M1), and load them with [`Pins::from_bytes`].
///
/// The fields are private and only the crate records a keybag, after it has verified the signature
/// and the rollback rules (an unlock, a create, or one of its own writes). A caller cannot pin an unverified
/// signer or overwrite the highest digest to hide a fork (review R2-L1):
///
/// ```compile_fail,E0616
/// let mut pins = weftos_vault::Pins::default();
/// pins.vk_generation = 0;
/// ```
///
/// ```compile_fail,E0624
/// fn pin_anything(pins: &mut weftos_vault::Pins, kb: &weftos_vault::Keybag) {
///     pins.record(&[], kb);
/// }
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pins {
    /// Pinned `sign_pub` (trust on first use, or from enrolment).
    pub(crate) sign_pub: Option<[u8; 32]>,
    /// Highest keybag stored so far.
    pub(crate) highest: Option<SeenKeybag>,
    /// Highest `current_vk_generation` seen (0 = none yet). A keybag with a lower generation is refused
    /// even when its sequence is higher and its signature verifies (review H1).
    pub(crate) vk_generation: u32,
}

/// Sequence and SHA-256 of the highest keybag seen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeenKeybag {
    /// Sequence number.
    pub sequence: u64,
    /// SHA-256 of the signed keybag bytes.
    pub digest: [u8; 32],
}

const PINS_MAGIC: [u8; 4] = *b"WVPN";
const PINS_VERSION: u8 = 1;
/// Length of [`Pins::to_bytes`]: magic, version, presence byte, `sign_pub`, sequence, digest, generation.
pub const PINS_LEN: usize = 4 + 1 + 1 + 32 + 8 + 32 + 4;

impl Pins {
    /// The pinned signer, if any.
    pub fn sign_pub(&self) -> Option<[u8; 32]> {
        self.sign_pub
    }

    /// The highest keybag seen, if any.
    pub fn highest(&self) -> Option<SeenKeybag> {
        self.highest
    }

    /// The highest `current_vk_generation` seen (0 = none yet).
    pub fn vk_generation(&self) -> u32 {
        self.vk_generation
    }

    /// Record a keybag the crate has verified (pins the signer on first use, keeps the highest sequence
    /// and the highest VK generation). Callers: `Keybag::parse` succeeded on `signed_bytes` against these
    /// pins, or the crate signed `signed_bytes` itself and checked them against these pins.
    pub(crate) fn record(&mut self, signed_bytes: &[u8], kb: &Keybag) {
        self.sign_pub.get_or_insert(kb.sign_pub);
        if self.highest.is_none_or(|h| kb.sequence >= h.sequence) {
            self.highest = Some(SeenKeybag {
                sequence: kb.sequence,
                digest: Sha256::digest(signed_bytes).into(),
            });
        }
        self.vk_generation = self.vk_generation.max(kb.current_vk_generation);
    }

    /// Device-local persistence form, [`PINS_LEN`] bytes. Store it where only this device writes: a
    /// party that can rewrite these bytes can reset the pins to trust on first use.
    pub fn to_bytes(&self) -> [u8; PINS_LEN] {
        let mut b = [0u8; PINS_LEN];
        b[..4].copy_from_slice(&PINS_MAGIC);
        b[4] = PINS_VERSION;
        if let (Some(pk), Some(h)) = (self.sign_pub, self.highest) {
            b[5] = 1;
            b[6..38].copy_from_slice(&pk);
            b[38..46].copy_from_slice(&h.sequence.to_be_bytes());
            b[46..78].copy_from_slice(&h.digest);
            b[78..82].copy_from_slice(&self.vk_generation.to_be_bytes());
        }
        b
    }

    /// Load pins written by [`Pins::to_bytes`]. Strict: wrong length, magic or version, an unknown
    /// presence byte, nonzero fields in empty pins, or a zero sequence or generation are refused.
    pub fn from_bytes(b: &[u8]) -> Result<Pins> {
        if b.len() != PINS_LEN || b[..4] != PINS_MAGIC || b[4] != PINS_VERSION {
            return Err(VaultError::Malformed("pins"));
        }
        match b[5] {
            0 if b[6..].iter().all(|x| *x == 0) => Ok(Pins::default()),
            1 => {
                let num = |r: core::ops::Range<usize>| -> [u8; 8] {
                    let mut a = [0u8; 8];
                    a[8 - r.len()..].copy_from_slice(&b[r]);
                    a
                };
                let sequence = u64::from_be_bytes(num(38..46));
                let vk_generation = u64::from_be_bytes(num(78..82)) as u32;
                if sequence == 0 || vk_generation == 0 {
                    return Err(VaultError::Malformed("pins"));
                }
                let mut pk = [0u8; 32];
                pk.copy_from_slice(&b[6..38]);
                let mut digest = [0u8; 32];
                digest.copy_from_slice(&b[46..78]);
                Ok(Pins {
                    sign_pub: Some(pk),
                    highest: Some(SeenKeybag { sequence, digest }),
                    vk_generation,
                })
            }
            _ => Err(VaultError::Malformed("pins")),
        }
    }
}
