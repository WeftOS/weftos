//! Per-secret blobs (spec section 7): layout, padding, AAD, name hashing.

use zeroize::Zeroizing;

use crate::error::{Result, VaultError};
use crate::seal::{self, NONCE_LEN, TAG_LEN};

const MAGIC: [u8; 4] = *b"WVBL";
const HEADER_LEN: usize = 42;
const MAX_CLASS: usize = 65536;
const MIN_CLASS: usize = 256;
const PLAIN_HEADER: usize = 6;

/// Plaintext `type` byte (spec 7.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretType {
    /// Login password.
    Login = 1,
    /// Token.
    Token = 2,
    /// Note.
    Note = 3,
    /// Payment item.
    Payment = 4,
    /// The sealed index.
    Index = 16,
}

impl SecretType {
    fn from_u8(v: u8) -> Result<Self> {
        Ok(match v {
            1 => Self::Login,
            2 => Self::Token,
            3 => Self::Note,
            4 => Self::Payment,
            16 => Self::Index,
            _ => return Err(VaultError::Malformed("unknown secret type")),
        })
    }
}

/// Use policy byte (spec 7.2). The format only carries it; the daemon enforces it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    /// A standing grant may be given.
    Standing = 0,
    /// Fresh user presence on every use.
    PerUse = 1,
}

impl Policy {
    fn from_u8(v: u8) -> Result<Self> {
        match v {
            0 => Ok(Self::Standing),
            1 => Ok(Self::PerUse),
            _ => Err(VaultError::Malformed("unknown policy")),
        }
    }
}

/// A decrypted secret. `Debug` never shows the payload.
pub struct OpenedSecret {
    /// Item type.
    pub secret_type: SecretType,
    /// Use policy.
    pub policy: Policy,
    /// Revision recorded in the blob header.
    pub revision: u64,
    /// The secret bytes.
    pub payload: Zeroizing<Vec<u8>>,
}

impl core::fmt::Debug for OpenedSecret {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("OpenedSecret")
            .field("secret_type", &self.secret_type)
            .field("policy", &self.policy)
            .field("revision", &self.revision)
            .field("payload", &"<redacted>")
            .finish()
    }
}

/// `blob` AAD (spec 7.3).
pub(crate) fn blob_aad(vault_id: &[u8; 16], slot: &str, generation: u32, revision: u64) -> Vec<u8> {
    let mut a = b"WVLT-BLOB-v1\0".to_vec();
    a.extend(vault_id);
    a.push(slot.len() as u8);
    a.extend(slot.as_bytes());
    a.push(1); // version
    a.push(1); // alg
    a.extend(generation.to_be_bytes());
    a.extend(revision.to_be_bytes());
    a
}

fn size_class(total: usize) -> Result<usize> {
    let mut c = MIN_CLASS;
    while c < total {
        c *= 2;
    }
    if c > MAX_CLASS {
        Err(VaultError::TooLarge("payload"))
    } else {
        Ok(c)
    }
}

pub(crate) fn check_slot(slot: &str) -> Result<()> {
    if slot.is_empty() || slot.len() > 255 || !slot.is_ascii() {
        return Err(VaultError::TooLarge("slot"));
    }
    Ok(())
}

/// Hex slot suffix for a secret name: `BLAKE3.keyed_hash(name_key, NFC(name))`.
pub(crate) fn name_hash_hex(name_key: &[u8; 32], nfc_name: &str) -> Result<String> {
    if nfc_name.is_empty() || nfc_name.len() > 255 {
        return Err(VaultError::TooLarge("name"));
    }
    Ok(blake3::keyed_hash(name_key, nfc_name.as_bytes())
        .to_hex()
        .to_string())
}

pub(crate) struct SealParams<'a> {
    pub vault_id: &'a [u8; 16],
    pub blob_key: &'a [u8; 32],
    pub generation: u32,
    pub slot: &'a str,
    pub revision: u64,
    pub nonce: [u8; NONCE_LEN],
}

/// Seal one blob with an explicit nonce (callers pass a random one; vectors pass a fixed one).
pub(crate) fn seal_blob(
    p: &SealParams<'_>,
    ty: SecretType,
    policy: Policy,
    payload: &[u8],
) -> Result<Vec<u8>> {
    check_slot(p.slot)?;
    if p.revision == 0 {
        return Err(VaultError::InvalidOperation("revision starts at 1"));
    }
    let class = size_class(PLAIN_HEADER + payload.len())?;
    let mut pt = Zeroizing::new(Vec::with_capacity(class));
    pt.push(ty as u8);
    pt.push(policy as u8);
    pt.extend((payload.len() as u32).to_be_bytes());
    pt.extend(payload);
    pt.resize(class, 0);
    seal_plaintext(p, &pt)
}

pub(crate) fn seal_plaintext(p: &SealParams<'_>, padded_plaintext: &[u8]) -> Result<Vec<u8>> {
    let aad = blob_aad(p.vault_id, p.slot, p.generation, p.revision);
    let ct = seal::encrypt(p.blob_key, &p.nonce, &aad, padded_plaintext)?;
    let mut out = Vec::with_capacity(HEADER_LEN + ct.len());
    out.extend(MAGIC);
    out.push(1);
    out.push(1);
    out.extend(p.generation.to_be_bytes());
    out.extend(p.revision.to_be_bytes());
    out.extend(p.nonce);
    out.extend(ct);
    Ok(out)
}

/// Open a blob for `slot`. A blob moved from another slot or vault, or with any byte changed, fails.
pub(crate) fn open_blob(
    vault_id: &[u8; 16],
    blob_key: &[u8; 32],
    current_generation: u32,
    slot: &str,
    blob: &[u8],
) -> Result<OpenedSecret> {
    check_slot(slot)?;
    if blob.len() < HEADER_LEN + MIN_CLASS + TAG_LEN
        || blob.len() > HEADER_LEN + MAX_CLASS + TAG_LEN
    {
        return Err(VaultError::Malformed("blob length out of range"));
    }
    if blob[..4] != MAGIC || blob[4] != 1 || blob[5] != 1 {
        return Err(VaultError::Malformed("bad blob header"));
    }
    let generation = u32::from_be_bytes(blob[6..10].try_into().expect("4 bytes"));
    let revision = u64::from_be_bytes(blob[10..18].try_into().expect("8 bytes"));
    let nonce: [u8; NONCE_LEN] = blob[18..42].try_into().expect("24 bytes");
    if generation != current_generation {
        return Err(VaultError::GenerationMismatch(generation));
    }
    let class = blob.len() - HEADER_LEN - TAG_LEN;
    if !class.is_power_of_two() {
        return Err(VaultError::Malformed("blob is not a size class"));
    }
    let aad = blob_aad(vault_id, slot, generation, revision);
    let pt = seal::decrypt(blob_key, &nonce, &aad, &blob[HEADER_LEN..])?;
    let secret_type = SecretType::from_u8(pt[0])?;
    let policy = Policy::from_u8(pt[1])?;
    let len = u32::from_be_bytes(pt[2..6].try_into().expect("4 bytes")) as usize;
    if len > pt.len() - PLAIN_HEADER {
        return Err(VaultError::Malformed(
            "payload length runs past the plaintext",
        ));
    }
    if pt[PLAIN_HEADER + len..].iter().any(|b| *b != 0) {
        return Err(VaultError::Malformed("padding is not zero"));
    }
    Ok(OpenedSecret {
        secret_type,
        policy,
        revision,
        payload: Zeroizing::new(pt[PLAIN_HEADER..PLAIN_HEADER + len].to_vec()),
    })
}
