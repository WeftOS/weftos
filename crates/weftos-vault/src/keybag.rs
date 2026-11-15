//! Keybag format (spec section 5): parse, encode, sign, verify, rollback pins.

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use sha2::{Digest, Sha256};

use crate::error::{Result, VaultError};
use crate::passcode_kdf::KdfParams;
pub use crate::pins::{Pins, SeenKeybag};

const MAGIC: [u8; 4] = *b"WVKB";
const FORMAT_VERSION: u16 = 1;
/// Flag bit 0: the format is unreviewed, real credentials are refused.
pub const FLAG_DRAFT: u16 = 1;
const HEADER_LEN: usize = 86;
const SIG_LEN: usize = 64;
const MIN_LEN: usize = 150;
pub(crate) const MAX_LEN: usize = 65536;
const MAX_WRAPPERS: usize = 64;
const MAX_BODY: usize = 16384;
const MAX_KEY_REF: usize = 256;
/// Most passcode wrappers a keybag may carry. `unlock` tries each one at the current generation, so this
/// caps the Argon2id runs a single unlock attempt can be made to do (review L2).
pub const MAX_PASSCODE_WRAPPERS: usize = 4;
/// Most total Argon2id work, as the sum of `m_kib * t` over all passcode wrappers, one keybag may ask
/// for: the cost of one wrapper at the format maximum (1 GiB, t=10). A signed keybag can therefore never
/// make one unlock cost more than the single worst format-valid wrapper (review L2, probe R3).
pub const MAX_UNLOCK_KDF_COST: u64 = 1_048_576 * 10;
const SIG_DOMAIN: &[u8] = b"WVLT-KEYBAG-SIG-v1\0";
const WRAP_DOMAIN: &[u8] = b"WVLT-WRAP-v1\0";

/// Wrapper kind numbers (spec 5.3). 6 (node) and 7 (escrow) are not defined and are rejected.
pub const KIND_PASSCODE: u8 = 1;
/// Device wrapper (platform key, opaque inner blob in V1).
pub const KIND_DEVICE: u8 = 2;
/// Recovery-key wrapper.
pub const KIND_RECOVERY: u8 = 3;
/// Old VK sealed under the new VK's history key.
pub const KIND_HISTORY: u8 = 4;
/// Ed25519 signing seed sealed under the signing-seed key.
pub const KIND_SIGNSEED: u8 = 5;

/// Per-kind wrapper body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WrapperBody {
    /// Passcode wrapper: Argon2id parameters, salt, nonce and `VK || tag`.
    Passcode {
        params: KdfParams,
        salt: [u8; 16],
        nonce: [u8; 24],
        ct: [u8; 48],
    },
    /// Device wrapper; `platform` 1 = Apple, 2 = Android. The inner `blob` is platform defined.
    Device {
        platform: u8,
        key_ref: Vec<u8>,
        blob: Vec<u8>,
    },
    /// Recovery wrapper.
    Recovery { nonce: [u8; 24], ct: [u8; 48] },
    /// Key history wrapper (rotation; carried and validated, not produced by V1).
    KeyHistory { nonce: [u8; 24], ct: [u8; 48] },
    /// Signing seed wrapper.
    SigningSeed { nonce: [u8; 24], ct: [u8; 48] },
}

/// One wrapper entry (spec 5.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wrapper {
    /// Random id, unique within the keybag.
    pub id: [u8; 16],
    /// VK generation this wrapper protects.
    pub vk_generation: u32,
    /// Kind-specific body.
    pub body: WrapperBody,
}

impl Wrapper {
    /// The kind byte.
    pub fn kind(&self) -> u8 {
        match self.body {
            WrapperBody::Passcode { .. } => KIND_PASSCODE,
            WrapperBody::Device { .. } => KIND_DEVICE,
            WrapperBody::Recovery { .. } => KIND_RECOVERY,
            WrapperBody::KeyHistory { .. } => KIND_HISTORY,
            WrapperBody::SigningSeed { .. } => KIND_SIGNSEED,
        }
    }

    fn encode_body(&self) -> Vec<u8> {
        let mut b = Vec::new();
        match &self.body {
            WrapperBody::Passcode {
                params,
                salt,
                nonce,
                ct,
            } => {
                b.push(1);
                b.extend(params.m_kib.to_be_bytes());
                b.extend(params.t.to_be_bytes());
                b.extend(params.p.to_be_bytes());
                b.extend(salt);
                b.extend(nonce);
                b.extend(ct);
            }
            WrapperBody::Device {
                platform,
                key_ref,
                blob,
            } => {
                b.push(*platform);
                b.extend((key_ref.len() as u16).to_be_bytes());
                b.extend(key_ref);
                b.extend((blob.len() as u32).to_be_bytes());
                b.extend(blob);
            }
            WrapperBody::Recovery { nonce, ct }
            | WrapperBody::KeyHistory { nonce, ct }
            | WrapperBody::SigningSeed { nonce, ct } => {
                b.extend(nonce);
                b.extend(ct);
            }
        }
        b
    }
}

/// The wrapper AEAD associated data (spec 5.2). Excludes `sequence`.
pub(crate) fn wrapper_aad(
    vault_id: &[u8; 16],
    wrapper_id: &[u8; 16],
    kind: u8,
    vk_generation: u32,
) -> Vec<u8> {
    let mut a = WRAP_DOMAIN.to_vec();
    a.extend(vault_id);
    a.extend(wrapper_id);
    a.push(kind);
    a.extend(vk_generation.to_be_bytes());
    a
}

/// A parsed keybag (spec 5.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keybag {
    /// Flags; bit 0 = DRAFT.
    pub flags: u16,
    /// Random vault id.
    pub vault_id: [u8; 16],
    /// Strictly increasing write counter, starts at 1.
    pub sequence: u64,
    /// Creation time, seconds.
    pub created_unix: u64,
    /// Last write time, seconds.
    pub updated_unix: u64,
    /// Generation new blobs are sealed under.
    pub current_vk_generation: u32,
    /// Ed25519 public key that signs this vault's keybags.
    pub sign_pub: [u8; 32],
    /// Wrapper entries in order.
    pub wrappers: Vec<Wrapper>,
}

struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|e| *e <= self.buf.len())
            .ok_or(VaultError::Malformed("length runs past the buffer"))?;
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }
    fn arr<const N: usize>(&mut self) -> Result<[u8; N]> {
        let mut a = [0u8; N];
        a.copy_from_slice(self.take(N)?);
        Ok(a)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.arr::<1>()?[0])
    }
    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_be_bytes(self.arr()?))
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.arr()?))
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_be_bytes(self.arr()?))
    }
    fn done(&self) -> bool {
        self.pos == self.buf.len()
    }
}

fn parse_body(kind: u8, body: &[u8]) -> Result<WrapperBody> {
    let mut c = Cursor { buf: body, pos: 0 };
    let parsed = match kind {
        KIND_PASSCODE => {
            if c.u8()? != 1 {
                return Err(VaultError::Malformed("unknown kdf id"));
            }
            let params = KdfParams {
                m_kib: c.u32()?,
                t: c.u32()?,
                p: c.u32()?,
            };
            params.check_format()?;
            WrapperBody::Passcode {
                params,
                salt: c.arr()?,
                nonce: c.arr()?,
                ct: c.arr()?,
            }
        }
        KIND_DEVICE => {
            let platform = c.u8()?;
            if !matches!(platform, 1 | 2) {
                return Err(VaultError::RejectedPlatform(platform));
            }
            let kr = c.u16()? as usize;
            if kr > MAX_KEY_REF {
                return Err(VaultError::Malformed("key_ref too long"));
            }
            let key_ref = c.take(kr)?.to_vec();
            let bl = c.u32()? as usize;
            WrapperBody::Device {
                platform,
                key_ref,
                blob: c.take(bl)?.to_vec(),
            }
        }
        KIND_RECOVERY => WrapperBody::Recovery {
            nonce: c.arr()?,
            ct: c.arr()?,
        },
        KIND_HISTORY => WrapperBody::KeyHistory {
            nonce: c.arr()?,
            ct: c.arr()?,
        },
        KIND_SIGNSEED => WrapperBody::SigningSeed {
            nonce: c.arr()?,
            ct: c.arr()?,
        },
        other => return Err(VaultError::RejectedWrapperKind(other)),
    };
    if !c.done() {
        return Err(VaultError::Malformed("wrapper body has trailing bytes"));
    }
    Ok(parsed)
}

impl Keybag {
    /// Whether the DRAFT flag is set.
    pub fn is_draft(&self) -> bool {
        self.flags & FLAG_DRAFT != 0
    }

    /// The wrapper with this id.
    pub fn wrapper(&self, id: &[u8; 16]) -> Option<&Wrapper> {
        self.wrappers.iter().find(|w| &w.id == id)
    }

    /// Structural rules of spec 5.3 and 5.5 for a keybag about to be written or accepted.
    pub fn validate_structure(&self) -> Result<()> {
        if self.flags & !FLAG_DRAFT != 0 {
            return Err(VaultError::Malformed("reserved flag bits set"));
        }
        if self.sequence == 0 {
            return Err(VaultError::Malformed("sequence is zero"));
        }
        if self.wrappers.is_empty() || self.wrappers.len() > MAX_WRAPPERS {
            return Err(VaultError::Malformed("wrapper count out of range"));
        }
        let mut ids: Vec<_> = self.wrappers.iter().map(|w| w.id).collect();
        ids.sort_unstable();
        ids.dedup();
        if ids.len() != self.wrappers.len() {
            return Err(VaultError::Malformed("duplicate wrapper id"));
        }
        if !self.wrappers.iter().any(|w| w.kind() == KIND_PASSCODE) {
            return Err(VaultError::Malformed("no passcode wrapper"));
        }
        let seeds = self
            .wrappers
            .iter()
            .filter(|w| w.kind() == KIND_SIGNSEED && w.vk_generation == self.current_vk_generation)
            .count();
        let all_seeds = self
            .wrappers
            .iter()
            .filter(|w| w.kind() == KIND_SIGNSEED)
            .count();
        let mut hist: Vec<u32> = self
            .wrappers
            .iter()
            .filter(|w| w.kind() == KIND_HISTORY)
            .map(|w| w.vk_generation)
            .collect();
        hist.sort_unstable();
        let n = hist.len();
        hist.dedup();
        if hist.len() != n
            || hist
                .last()
                .is_some_and(|g| *g >= self.current_vk_generation)
        {
            return Err(VaultError::Malformed(
                "key history must be one entry per older generation",
            ));
        }
        if seeds != 1 || all_seeds != 1 {
            return Err(VaultError::Malformed(
                "need exactly one signing seed wrapper",
            ));
        }
        let mut passcodes = 0usize;
        let mut cost = 0u64;
        for w in &self.wrappers {
            if let WrapperBody::Passcode { params, .. } = &w.body {
                passcodes += 1;
                cost += u64::from(params.m_kib) * u64::from(params.t);
            }
        }
        if passcodes > MAX_PASSCODE_WRAPPERS {
            return Err(VaultError::Malformed("too many passcode wrappers"));
        }
        if cost > MAX_UNLOCK_KDF_COST {
            return Err(VaultError::KdfBounds);
        }
        Ok(())
    }

    /// Bytes before the signature.
    pub fn unsigned_bytes(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(512);
        b.extend(MAGIC);
        b.extend(FORMAT_VERSION.to_be_bytes());
        b.extend(self.flags.to_be_bytes());
        b.extend(self.vault_id);
        b.extend(self.sequence.to_be_bytes());
        b.extend(self.created_unix.to_be_bytes());
        b.extend(self.updated_unix.to_be_bytes());
        b.extend(self.current_vk_generation.to_be_bytes());
        b.extend(self.sign_pub);
        b.extend((self.wrappers.len() as u16).to_be_bytes());
        for w in &self.wrappers {
            let body = w.encode_body();
            b.push(w.kind());
            b.extend(w.id);
            b.extend(w.vk_generation.to_be_bytes());
            b.extend((body.len() as u32).to_be_bytes());
            b.extend(body);
        }
        b
    }

    /// Signed keybag bytes: `unsigned || Ed25519(domain || unsigned)`.
    pub fn sign(&self, key: &SigningKey) -> Vec<u8> {
        sign_bytes(&self.unsigned_bytes(), key)
    }

    /// Parse and verify per spec 5.6 steps 1 to 4. Does no KDF or unwrap work.
    pub fn parse(bytes: &[u8], pins: &Pins) -> Result<Keybag> {
        if bytes.len() < MIN_LEN || bytes.len() > MAX_LEN {
            return Err(VaultError::Malformed("length out of range"));
        }
        let body_end = bytes.len() - SIG_LEN;
        let mut c = Cursor {
            buf: &bytes[..body_end],
            pos: 0,
        };
        if c.arr::<4>()? != MAGIC {
            return Err(VaultError::Malformed("bad magic"));
        }
        if c.u16()? != FORMAT_VERSION {
            return Err(VaultError::UnsupportedVersion);
        }
        let flags = c.u16()?;
        let vault_id = c.arr()?;
        let sequence = c.u64()?;
        let created_unix = c.u64()?;
        let updated_unix = c.u64()?;
        let current_vk_generation = c.u32()?;
        let sign_pub: [u8; 32] = c.arr()?;
        let count = c.u16()? as usize;
        if count == 0 || count > MAX_WRAPPERS {
            return Err(VaultError::Malformed("wrapper count out of range"));
        }
        debug_assert_eq!(c.pos, HEADER_LEN);
        let mut wrappers = Vec::with_capacity(count);
        for _ in 0..count {
            let kind = c.u8()?;
            let id = c.arr()?;
            let vk_generation = c.u32()?;
            let body_len = c.u32()? as usize;
            if body_len > MAX_BODY {
                return Err(VaultError::Malformed("wrapper body too long"));
            }
            let body = parse_body(kind, c.take(body_len)?)?;
            wrappers.push(Wrapper {
                id,
                vk_generation,
                body,
            });
        }
        if !c.done() {
            return Err(VaultError::Malformed("trailing bytes"));
        }
        let kb = Keybag {
            flags,
            vault_id,
            sequence,
            created_unix,
            updated_unix,
            current_vk_generation,
            sign_pub,
            wrappers,
        };
        kb.validate_structure()?;

        if pins.sign_pub.is_some_and(|p| p != sign_pub) {
            return Err(VaultError::SignerMismatch);
        }
        let vk = VerifyingKey::from_bytes(&sign_pub).map_err(|_| VaultError::BadSignature)?;
        let sig =
            Signature::from_slice(&bytes[body_end..]).map_err(|_| VaultError::BadSignature)?;
        let mut msg = SIG_DOMAIN.to_vec();
        msg.extend(&bytes[..body_end]);
        vk.verify_strict(&msg, &sig)
            .map_err(|_| VaultError::BadSignature)?;

        if current_vk_generation < pins.vk_generation {
            return Err(VaultError::GenerationRollback {
                got: current_vk_generation,
                seen: pins.vk_generation,
            });
        }
        if let Some(h) = pins.highest {
            if sequence < h.sequence {
                return Err(VaultError::Rollback {
                    got: sequence,
                    seen: h.sequence,
                });
            }
            if sequence == h.sequence && h.digest != <[u8; 32]>::from(Sha256::digest(bytes)) {
                return Err(VaultError::Fork { sequence });
            }
        }
        Ok(kb)
    }
}

/// Sign already-encoded unsigned bytes (also used by tests to forge structurally odd keybags).
pub(crate) fn sign_bytes(unsigned: &[u8], key: &SigningKey) -> Vec<u8> {
    let mut msg = SIG_DOMAIN.to_vec();
    msg.extend(unsigned);
    let mut out = unsigned.to_vec();
    out.extend(key.sign(&msg).to_bytes());
    out
}
