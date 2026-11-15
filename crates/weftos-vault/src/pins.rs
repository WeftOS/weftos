//! Rollback pins: what a device has already seen for one vault (spec 5.6 step 4, review H1 and M1).

use sha2::{Digest, Sha256};

use crate::keybag::Keybag;

/// What a device has already seen for one vault: the signer pin, the highest keybag and the highest
/// VK generation.
///
/// Pins carry no `vault_id`: a caller keeps one `Pins` per vault and must not share it between vaults.
/// Persist the pins after every unlock and after every write made through [`crate::Vault`]; the write
/// methods advance them (review M1).
#[derive(Debug, Clone, Default)]
pub struct Pins {
    /// Pinned `sign_pub` (trust on first use, or from enrolment).
    pub sign_pub: Option<[u8; 32]>,
    /// Highest keybag stored so far.
    pub highest: Option<SeenKeybag>,
    /// Highest `current_vk_generation` seen (0 = none yet). A keybag with a lower generation is refused
    /// even when its sequence is higher and its signature verifies (review H1).
    pub vk_generation: u32,
}

/// Sequence and SHA-256 of the highest keybag seen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeenKeybag {
    /// Sequence number.
    pub sequence: u64,
    /// SHA-256 of the signed keybag bytes.
    pub digest: [u8; 32],
}

impl Pins {
    /// Record a verified keybag as seen (pins the signer on first use, keeps the highest sequence and
    /// the highest VK generation).
    pub fn record(&mut self, signed_bytes: &[u8], kb: &Keybag) {
        self.sign_pub.get_or_insert(kb.sign_pub);
        if self.highest.is_none_or(|h| kb.sequence >= h.sequence) {
            self.highest = Some(SeenKeybag {
                sequence: kb.sequence,
                digest: Sha256::digest(signed_bytes).into(),
            });
        }
        self.vk_generation = self.vk_generation.max(kb.current_vk_generation);
    }
}
