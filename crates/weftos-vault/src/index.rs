//! The sealed index (spec 7.4, format fixed in `docs/research/vault-v1-index-format-2026-11-15.md`).
//!
//! One blob at slot `index`, `type` 16. The payload is a canonical binary encoding: entries sorted by
//! `name_hash`, no duplicates, no trailing bytes. Names live only inside the sealed payload; the store
//! sees ciphertext. The index is a derived view, never merged, and never holds secret values.

use crate::blob::{Policy, SecretType};
use crate::error::{Result, VaultError};
use crate::hkdf_keys;
use crate::normalize;
use crate::secrets::SealedSecret;
use crate::vault::Vault;

const INDEX_VERSION: u16 = 1;
const MAX_TAGS: usize = 16;
const MAX_TAG: usize = 64;
const MAX_SOURCE: usize = 64;

/// One secret as the index describes it. Never a value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexEntry {
    /// Secret name (NFC, 1 to 255 bytes).
    pub name: String,
    /// Item type (not `Index`).
    pub secret_type: SecretType,
    /// Use policy.
    pub policy: Policy,
    /// Revision of the secret's blob.
    pub revision: u64,
    /// Last use, unix seconds, 0 for never.
    pub last_used_unix: u64,
    /// Up to 16 tags of 1 to 64 bytes.
    pub tags: Vec<String>,
    /// Reserved for imports (Q7), up to 64 bytes.
    pub source: Option<String>,
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if n > self.0.len() {
            return Err(VaultError::Malformed("index runs past the payload"));
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn num<const N: usize>(&mut self) -> Result<[u8; N]> {
        Ok(self.take(N)?.try_into().expect("length checked"))
    }
    fn text(&mut self, min: usize, max: usize) -> Result<String> {
        let n = usize::from(self.u8()?);
        if n < min || n > max {
            return Err(VaultError::Malformed("index string length out of range"));
        }
        String::from_utf8(self.take(n)?.to_vec())
            .map_err(|_| VaultError::Malformed("index string is not UTF-8"))
    }
}

fn put_text(out: &mut Vec<u8>, s: &str, min: usize, max: usize) -> Result<()> {
    if s.len() < min || s.len() > max {
        return Err(VaultError::TooLarge("index string"));
    }
    out.push(s.len() as u8);
    out.extend(s.as_bytes());
    Ok(())
}

impl Vault {
    fn name_key_at(&self, generation: u32) -> Result<hkdf_keys::Key> {
        let vk = self.vk_for_generation(generation)?;
        Ok(hkdf_keys::name_key(&self.keybag.vault_id, &vk, generation))
    }

    /// The canonical plaintext payload (before sealing).
    pub(crate) fn encode_index(&self, entries: &[IndexEntry]) -> Result<Vec<u8>> {
        let generation = self.keybag.current_vk_generation;
        let nk = self.name_key_at(generation)?;
        let mut rows: Vec<([u8; 32], &IndexEntry)> = Vec::with_capacity(entries.len());
        for e in entries {
            if e.secret_type == SecretType::Index {
                return Err(VaultError::InvalidOperation(
                    "the index does not index itself",
                ));
            }
            if e.tags.len() > MAX_TAGS {
                return Err(VaultError::TooLarge("tags"));
            }
            let nfc = normalize::name(&e.name);
            if nfc != e.name {
                return Err(VaultError::InvalidOperation("name must be NFC"));
            }
            if nfc.is_empty() || nfc.len() > 255 {
                return Err(VaultError::TooLarge("name"));
            }
            rows.push((*blake3::keyed_hash(&nk, nfc.as_bytes()).as_bytes(), e));
        }
        rows.sort_by_key(|r| r.0);
        if rows.windows(2).any(|w| w[0].0 == w[1].0) {
            return Err(VaultError::InvalidOperation("duplicate secret name"));
        }
        let mut out = Vec::new();
        out.extend(INDEX_VERSION.to_be_bytes());
        out.extend(generation.to_be_bytes());
        out.extend((rows.len() as u32).to_be_bytes());
        for (hash, e) in rows {
            out.extend(hash);
            out.push(e.secret_type as u8);
            out.push(e.policy as u8);
            out.extend(e.revision.to_be_bytes());
            out.extend(e.last_used_unix.to_be_bytes());
            put_text(&mut out, &e.name, 1, 255)?;
            out.push(e.tags.len() as u8);
            for t in &e.tags {
                put_text(&mut out, t, 1, MAX_TAG)?;
            }
            put_text(&mut out, e.source.as_deref().unwrap_or(""), 0, MAX_SOURCE)?;
        }
        Ok(out)
    }

    /// Seal the index (slot `index`, type 16) with the entries in canonical order.
    pub fn seal_index_entries(
        &self,
        entries: &[IndexEntry],
        revision: u64,
    ) -> Result<SealedSecret> {
        self.seal_index(&self.encode_index(entries)?, revision)
    }

    /// Open the blob stored at `index` and return its entries, sorted by name hash. Refuses a blob that
    /// fails authentication, is not type 16, or whose payload breaks any canonical rule: unknown version,
    /// unsorted or repeated hashes, a hash that is not the keyed hash of its name, trailing bytes.
    pub fn open_index(&self, blob_bytes: &[u8]) -> Result<Vec<IndexEntry>> {
        let opened = self.open_secret("index", blob_bytes)?;
        if opened.secret_type != SecretType::Index {
            return Err(VaultError::Malformed("blob at index is not an index"));
        }
        let mut r = Reader(&opened.payload);
        if u16::from_be_bytes(r.num()?) != INDEX_VERSION {
            return Err(VaultError::UnsupportedVersion);
        }
        let generation = u32::from_be_bytes(r.num()?);
        if generation != crate::blob::peek_generation(blob_bytes)? {
            // The payload is authenticated, so this is a writer bug rather than an attack (review L6),
            // but an index whose name hashes use another generation's key is not canonical.
            return Err(VaultError::Malformed(
                "index generation differs from its blob generation",
            ));
        }
        let nk = self.name_key_at(generation)?;
        let count = u32::from_be_bytes(r.num()?);
        let mut entries = Vec::new();
        let mut prev: Option<[u8; 32]> = None;
        for _ in 0..count {
            let hash: [u8; 32] = r.num()?;
            if prev.is_some_and(|p| p >= hash) {
                return Err(VaultError::Malformed(
                    "index entries are not sorted and unique",
                ));
            }
            prev = Some(hash);
            let secret_type = match r.u8()? {
                1 => SecretType::Login,
                2 => SecretType::Token,
                3 => SecretType::Note,
                4 => SecretType::Payment,
                _ => return Err(VaultError::Malformed("bad type in index entry")),
            };
            let policy = match r.u8()? {
                0 => Policy::Standing,
                1 => Policy::PerUse,
                _ => return Err(VaultError::Malformed("bad policy in index entry")),
            };
            let revision = u64::from_be_bytes(r.num()?);
            let last_used_unix = u64::from_be_bytes(r.num()?);
            let name = r.text(1, 255)?;
            if normalize::name(&name) != name
                || *blake3::keyed_hash(&nk, name.as_bytes()).as_bytes() != hash
            {
                return Err(VaultError::Malformed("index name does not match its hash"));
            }
            let tag_count = usize::from(r.u8()?);
            if tag_count > MAX_TAGS {
                return Err(VaultError::Malformed("too many tags"));
            }
            let tags = (0..tag_count)
                .map(|_| r.text(1, MAX_TAG))
                .collect::<Result<Vec<_>>>()?;
            let source = Some(r.text(0, MAX_SOURCE)?).filter(|s| !s.is_empty());
            entries.push(IndexEntry {
                name,
                secret_type,
                policy,
                revision,
                last_used_unix,
                tags,
                source,
            });
        }
        if !r.0.is_empty() {
            return Err(VaultError::Malformed("trailing bytes in index"));
        }
        tracing::debug!(entries = entries.len(), "index opened");
        Ok(entries)
    }
}
