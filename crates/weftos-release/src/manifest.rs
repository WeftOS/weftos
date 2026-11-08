//! The signed manifest: the one object that makes a release exist.
//!
//! Artifacts are stored by BLAKE3 hash under `blobs/<hash>`. A release is a manifest at
//! `releases/<build id>/manifest.json` that lists `(path, hash, size)` and is signed with the
//! ADR-129 Ed25519 + ML-DSA-65 dual signature. The manifest is written last, so a publish that
//! dies halfway leaves unreferenced blobs and no release.

use serde::{Deserialize, Serialize};
use weftos_pq_sign::{mldsa65, DualSignature, SigSuite};

use crate::error::{invalid, ReleaseError, Result};
use crate::name::{validate_build_id, validate_channel};

pub const SCHEMA: u32 = 1;
pub const MAX_ARTIFACTS: usize = 10_000;
pub const MAX_PATH_LEN: usize = 512;

/// Domain separation for the signed bytes.
const SIGN_DOMAIN: &[u8] = b"weftos-release-manifest/v1\n";
/// FIPS 204 context for the ML-DSA half.
const SIGN_CTX: &[u8] = b"weftos/release-manifest/v1";

/// One artifact in a release.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactRef {
    /// Relative path inside the release (`dist/weaver-linux-x86_64`).
    pub path: String,
    /// Lowercase hex BLAKE3 of the bytes. The blob lives at `blobs/<hash>`.
    pub hash: String,
    pub size: u64,
}

/// What a release contains. Everything here is covered by the signature.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub schema: u32,
    /// The store this release belongs to (`weftos://.../releases/<channel>/store`).
    pub store: String,
    pub channel: String,
    pub build_id: String,
    /// The source revision built (a git SHA).
    pub source_rev: String,
    /// Unix seconds, from the publisher's clock (ADR-124).
    pub created_at: u64,
    /// Who built it (a runner or principal name). Informational; the signature is the proof.
    pub builder: String,
    /// Sorted by path, paths unique.
    pub artifacts: Vec<ArtifactRef>,
}

impl Manifest {
    /// The bytes that are signed: a domain tag, then the manifest's JSON.
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut out = SIGN_DOMAIN.to_vec();
        out.extend(serde_json::to_vec(self).expect("manifest serializes"));
        out
    }

    /// Hex BLAKE3 of the signing bytes: the release's identity.
    pub fn digest(&self) -> String {
        blake3::hash(&self.signing_bytes()).to_hex().to_string()
    }

    /// Same release? Ignores `created_at` and `builder` so a CI retry of the same job is idempotent.
    pub fn same_content(&self, other: &Manifest) -> bool {
        self.schema == other.schema
            && self.store == other.store
            && self.channel == other.channel
            && self.build_id == other.build_id
            && self.source_rev == other.source_rev
            && self.artifacts == other.artifacts
    }

    /// Structural checks. Run on everything read from a store and everything about to be written.
    pub fn validate(&self) -> Result<()> {
        if self.schema != SCHEMA {
            return Err(invalid("manifest", format!("schema {} is not {SCHEMA}", self.schema)));
        }
        validate_channel(&self.channel)?;
        validate_build_id(&self.build_id)?;
        validate_text("source rev", &self.source_rev, 128)?;
        validate_text("builder", &self.builder, 128)?;
        if self.artifacts.is_empty() {
            return Err(invalid("manifest", "a release needs at least one artifact"));
        }
        if self.artifacts.len() > MAX_ARTIFACTS {
            return Err(invalid("manifest", format!("more than {MAX_ARTIFACTS} artifacts")));
        }
        let mut prev: Option<&str> = None;
        for a in &self.artifacts {
            validate_artifact_path(&a.path)?;
            validate_hash(&a.hash)?;
            if prev.is_some_and(|p| p >= a.path.as_str()) {
                return Err(invalid("manifest", "artifacts must be sorted by path with unique paths"));
            }
            prev = Some(&a.path);
        }
        Ok(())
    }

    /// Hashes this release references, sorted and de-duplicated.
    pub fn blob_hashes(&self) -> Vec<String> {
        let mut v: Vec<String> = self.artifacts.iter().map(|a| a.hash.clone()).collect();
        v.sort();
        v.dedup();
        v
    }
}

fn validate_text(what: &'static str, s: &str, max: usize) -> Result<()> {
    if s.is_empty() || s.len() > max || s.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(invalid(what, format!("1-{max} characters, no whitespace or control characters")));
    }
    Ok(())
}

/// Relative, `/`-separated, no empty / `.` / `..` segments, no backslash or control characters.
pub fn validate_artifact_path(path: &str) -> Result<()> {
    let bad = path.is_empty()
        || path.len() > MAX_PATH_LEN
        || path.starts_with('/')
        || path.ends_with('/')
        || path.chars().any(|c| c == '\\' || c.is_control())
        || path.split('/').any(|s| s.is_empty() || s == "." || s == "..");
    if bad {
        Err(invalid(
            "artifact path",
            format!("{path:?}: must be relative, '/'-separated, with no empty, '.' or '..' segments"),
        ))
    } else {
        Ok(())
    }
}

pub fn validate_hash(hash: &str) -> Result<()> {
    if hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        Ok(())
    } else {
        Err(invalid("hash", "expected 64 lowercase hex digits"))
    }
}

pub(crate) fn hash_bytes(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// A manifest and the signature over it, as stored.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedManifest {
    pub manifest: Manifest,
    /// Which release key signed ([`ReleaseSigner::key_id`]).
    pub key_id: String,
    /// Hex of the `DualSignature` binary form.
    pub signature: String,
}

impl SignedManifest {
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec_pretty(self).expect("signed manifest serializes")
    }

    pub fn parse(bytes: &[u8]) -> Result<Self> {
        serde_json::from_slice(bytes)
            .map_err(|e| ReleaseError::Malformed { what: "manifest", why: e.to_string() })
    }

    /// Check the signature against `keys` and that the manifest is for `expected_store`.
    /// Returns the manifest only if both hold and the structure is valid.
    pub fn verify(&self, keys: &TrustedKeys, expected_store: &str) -> Result<()> {
        if self.manifest.store != expected_store {
            return Err(ReleaseError::Untrusted(format!(
                "manifest is for {}, not {expected_store}",
                self.manifest.store
            )));
        }
        self.manifest.validate()?;
        if !expected_store.ends_with(&format!("/releases/{}/store", self.manifest.channel)) {
            return Err(ReleaseError::Untrusted("manifest channel does not match its store".into()));
        }
        let key = keys
            .0
            .iter()
            .find(|k| k.key_id() == self.key_id)
            .ok_or_else(|| ReleaseError::Untrusted(format!("key {} is not a trusted release key", self.key_id)))?;
        let raw = unhex(&self.signature)
            .ok_or_else(|| ReleaseError::Untrusted("signature is not hex".into()))?;
        let sig = DualSignature::from_bytes(&raw)
            .map_err(|e| ReleaseError::Untrusted(format!("signature: {e}")))?;
        if sig.suite != SigSuite::Ed25519MlDsa65V2 {
            return Err(ReleaseError::Untrusted("legacy or non post-quantum signature suite".into()));
        }
        let verdict = sig.verify(&self.manifest.signing_bytes(), SIGN_CTX, &key.ed, Some(&key.ml));
        if verdict.is_post_quantum() {
            Ok(())
        } else {
            Err(ReleaseError::Untrusted(format!("signature check failed: {verdict:?}")))
        }
    }
}

/// The release signing keys. Held by the publisher, never by a store.
pub struct ReleaseSigner {
    ed: ed25519_dalek::SigningKey,
    ml: mldsa65::SigningKey,
}

impl ReleaseSigner {
    pub fn new(ed: ed25519_dalek::SigningKey, ml: mldsa65::SigningKey) -> Self {
        Self { ed, ml }
    }

    /// Build a signer from the two 32-byte seeds (Ed25519, then ML-DSA-65).
    pub fn from_seeds(ed_seed: &[u8; 32], ml_seed: &[u8; 32]) -> Self {
        Self::new(ed25519_dalek::SigningKey::from_bytes(ed_seed), mldsa65::SigningKey::from_seed(ml_seed))
    }

    pub fn key_id(&self) -> String {
        key_id(&self.ed.verifying_key(), self.ml.verifying_key())
    }

    /// The public half, for building a [`TrustedKeys`].
    pub fn trusted(&self) -> TrustedKey {
        TrustedKey { ed: self.ed.verifying_key(), ml: self.ml.verifying_key().clone() }
    }

    pub fn sign(&self, manifest: Manifest) -> Result<SignedManifest> {
        manifest.validate()?;
        let sig = DualSignature::sign(&manifest.signing_bytes(), SIGN_CTX, &self.ed, Some(&self.ml))
            .map_err(|e| ReleaseError::Sign(e.to_string()))?;
        Ok(SignedManifest { key_id: self.key_id(), signature: hex(&sig.to_bytes()), manifest })
    }
}

/// One trusted public key pair.
#[derive(Clone)]
pub struct TrustedKey {
    pub ed: ed25519_dalek::VerifyingKey,
    pub ml: mldsa65::VerifyingKey,
}

impl TrustedKey {
    pub fn key_id(&self) -> String {
        key_id(&self.ed, &self.ml)
    }
}

/// The set of keys a reader accepts. More than one allows rotation.
#[derive(Clone, Default)]
pub struct TrustedKeys(pub Vec<TrustedKey>);

fn key_id(ed: &ed25519_dalek::VerifyingKey, ml: &mldsa65::VerifyingKey) -> String {
    let mut h = blake3::Hasher::new();
    h.update(ed.as_bytes());
    h.update(ml.as_bytes());
    h.finalize().to_hex()[..32].to_string()
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    const D: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(D[(b >> 4) as usize] as char);
        s.push(D[(b & 15) as usize] as char);
    }
    s
}

pub(crate) fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let v = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    };
    s.as_bytes().chunks(2).map(|p| Some(v(p[0])? << 4 | v(p[1])?)).collect()
}
