//! Manifest envelope and the cog package body (ADR-100 section 1).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::canonical::{CanonicalError, canonical_json};
use super::codec::is_lower_hex;

/// Envelope schema identifier. Shared by every workload kind.
pub const MANIFEST_SCHEMA: &str = "weftos.workload-manifest.v1";
/// Domain-separation prefix for signatures over the envelope statement.
pub const SIGNING_DOMAIN: &[u8] = b"weftos.workload-manifest.v1\n";
/// Workload kind for Cognitum cogs.
pub const KIND_COG: &str = "cog";
/// Manifest file name inside a cog package directory.
pub const MANIFEST_FILE: &str = "cogpkg.json";
/// Largest manifest file accepted.
pub const MAX_MANIFEST_BYTES: usize = 1024 * 1024;
/// Most signature entries accepted on one envelope.
pub const MAX_SIGNATURES: usize = 16;
/// Architectures a cog package may carry (ADR-100 section 1).
/// Binary key for a thin arm64 Mach-O cog run natively on a macOS node
/// (ADR-100, 2026-10-28). The key doubles as the per-binary OS marker: a
/// package carries it only for cogs that declare they run on macOS.
pub const MACOS_AARCH64: &str = "aarch64-macos";

/// Binary keys a cog package may carry: Linux/WASM arches, plus
/// [`MACOS_AARCH64`].
pub const COG_ARCHES: &[&str] = &["aarch64", "armv7", "x86_64", "wasm", MACOS_AARCH64];

/// Manifest parse / validation failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ManifestError {
    /// JSON could not be parsed into the expected shape.
    #[error("manifest parse: {0}")]
    Parse(String),
    /// A field failed validation.
    #[error("manifest invalid: {0}")]
    Invalid(String),
    /// No workload kind is registered under the manifest's `kind`.
    #[error(transparent)]
    UnknownKind(#[from] crate::workload_kind::UnknownKind),
    /// Canonical encoding refused the value.
    #[error(transparent)]
    Canonical(#[from] CanonicalError),
}

fn invalid(msg: impl Into<String>) -> ManifestError {
    ManifestError::Invalid(msg.into())
}

/// Longest unauthenticated manifest text echoed into an error.
const MAX_ECHO_CHARS: usize = 32;

/// `s` cut to [`MAX_ECHO_CHARS`] characters, for error text.
fn echo(s: &str) -> String {
    s.chars().take(MAX_ECHO_CHARS).collect()
}

/// One Ed25519 signature over [`ManifestEnvelope::signed_statement`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignatureEntry {
    /// Always `"ed25519"` in v1 (ML-DSA-65 dual signing is a follow-up).
    pub algorithm: String,
    /// Signer key id; must equal the pinned entry for `public_key`.
    pub key_id: String,
    /// 32-byte public key, lower-case hex.
    pub public_key: String,
    /// 64-byte signature, lower-case hex.
    pub signature: String,
}

/// Kind-agnostic signed manifest envelope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestEnvelope {
    /// Must be [`MANIFEST_SCHEMA`].
    pub schema: String,
    /// Workload kind, e.g. [`KIND_COG`].
    pub kind: String,
    /// Kind-specific body.
    pub body: Value,
    /// Detached signatures over the statement (schema, kind, body).
    #[serde(default)]
    pub signatures: Vec<SignatureEntry>,
}

impl ManifestEnvelope {
    /// New unsigned envelope.
    pub fn new(kind: &str, body: Value) -> Self {
        Self {
            schema: MANIFEST_SCHEMA.to_string(),
            kind: kind.to_string(),
            body,
            signatures: Vec::new(),
        }
    }

    /// Canonical statement: `{"body":..,"kind":..,"schema":..}`. Signatures
    /// are excluded so they can be added independently.
    pub fn statement(&self) -> Result<Vec<u8>, ManifestError> {
        Ok(canonical_json(&json!({
            "schema": self.schema,
            "kind": self.kind,
            "body": self.body,
        }))?)
    }

    /// Bytes an Ed25519 signer signs: [`SIGNING_DOMAIN`] then the statement.
    pub fn signed_statement(&self) -> Result<Vec<u8>, ManifestError> {
        let mut msg = SIGNING_DOMAIN.to_vec();
        msg.extend_from_slice(&self.statement()?);
        Ok(msg)
    }

    /// Package id: BLAKE3 hex of the statement. Stable across re-signing.
    pub fn package_id(&self) -> Result<String, ManifestError> {
        Ok(blake3::hash(&self.statement()?).to_hex().to_string())
    }

    /// Parse and validate the envelope frame (not the body).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, ManifestError> {
        if bytes.len() > MAX_MANIFEST_BYTES {
            return Err(invalid(format!(
                "manifest exceeds {MAX_MANIFEST_BYTES} bytes"
            )));
        }
        let env: Self =
            serde_json::from_slice(bytes).map_err(|e| ManifestError::Parse(e.to_string()))?;
        if env.schema != MANIFEST_SCHEMA {
            return Err(invalid(format!("unknown schema {:?}", echo(&env.schema))));
        }
        if !valid_token(&env.kind, 32) {
            return Err(invalid(format!("bad kind {:?}", echo(&env.kind))));
        }
        if env.signatures.len() > MAX_SIGNATURES {
            return Err(invalid(format!("more than {MAX_SIGNATURES} signatures")));
        }
        if let Some(s) = env.signatures.iter().find(|s| !valid_token(&s.key_id, 128)) {
            return Err(invalid(format!(
                "bad signature key id {:?}",
                s.key_id.chars().take(40).collect::<String>()
            )));
        }
        Ok(env)
    }

    /// Pretty JSON with a trailing newline, for writing `cogpkg.json`.
    pub fn to_pretty_json(&self) -> Result<Vec<u8>, ManifestError> {
        let mut out =
            serde_json::to_vec_pretty(self).map_err(|e| ManifestError::Parse(e.to_string()))?;
        out.push(b'\n');
        Ok(out)
    }

    /// Typed cog body, validated. Fails if `kind != "cog"`.
    pub fn cog_body(&self) -> Result<CogPackageBody, ManifestError> {
        if self.kind != KIND_COG {
            return Err(invalid(format!("kind {:?} is not a cog", self.kind)));
        }
        self.cog_shaped_body()
    }

    /// The body parsed and validated as a cog package body, whatever the
    /// envelope's `kind`. A registered kind whose verified package reuses
    /// the cog body shape (the only shape `VerifiedPackage` carries today)
    /// goes through this after its own `validate`.
    pub fn cog_shaped_body(&self) -> Result<CogPackageBody, ManifestError> {
        let body: CogPackageBody = serde_json::from_value(self.body.clone())
            .map_err(|e| ManifestError::Parse(format!("cog body: {e}")))?;
        body.validate()?;
        Ok(body)
    }
}

/// A file inside the package, pinned by BLAKE3 and size.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileRef {
    /// Relative path inside the package directory (forward slashes).
    pub path: String,
    /// Size in bytes.
    pub size: u64,
    /// BLAKE3 hex of the content; also its `ArtifactStore` key.
    pub blake3: String,
}

impl FileRef {
    fn validate(&self) -> Result<(), ManifestError> {
        validate_rel_path(&self.path)?;
        if !is_lower_hex(&self.blake3, 64) {
            return Err(invalid(format!(
                "{}: blake3 must be 64 lower-case hex",
                self.path
            )));
        }
        Ok(())
    }
}

/// Where the package content came from (ADR-100 section 6.3).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageSource {
    /// Source repository (for example the cogs fork).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// Git commit the binaries were built from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// Upstream release URL, when packaging released binaries. A URL is
    /// provenance, never trust: the operator signature is the trust.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_url: Option<String>,
}

impl PackageSource {
    fn validate(&self) -> Result<(), ManifestError> {
        if self.commit.is_none() && self.release_url.is_none() {
            return Err(invalid("source needs a commit or a release_url"));
        }
        if let Some(c) = &self.commit
            && !(c.len() >= 7 && c.len() <= 64 && is_lower_hex(c, c.len()))
        {
            return Err(invalid("source.commit must be 7..64 lower-case hex"));
        }
        for s in [&self.repo, &self.release_url].into_iter().flatten() {
            if s.is_empty() || s.len() > 512 || s.chars().any(|c| c.is_control()) {
                return Err(invalid("source strings must be 1..512 printable chars"));
            }
        }
        Ok(())
    }
}

/// An external attestation file carried in the package.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttestationRef {
    /// Attestation kind, e.g. `cognitum.cog.release-record.v1`.
    pub kind: String,
    /// The file.
    pub file: FileRef,
}

/// Body of a `kind = "cog"` manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CogPackageBody {
    /// Cog id from `[cog].id`.
    pub id: String,
    /// Cog version from `[cog].version`.
    pub version: String,
    /// The unmodified `cog.toml`.
    pub cog_toml: FileRef,
    /// One binary per arch, keyed by arch name.
    pub binaries: BTreeMap<String, FileRef>,
    /// Provenance.
    pub source: PackageSource,
    /// Optional external attestations.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attestations: Vec<AttestationRef>,
    /// The signer states this package may be handed to other nodes (seeded,
    /// advertised, served over the swarm). Absent means false: sharing is
    /// opt-in, so an operator re-pack of a licence-gated binary is never
    /// redistributed by accident. It is part of the signed statement.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub redistributable: bool,
}

impl CogPackageBody {
    /// Validate ids, paths and layout.
    pub fn validate(&self) -> Result<(), ManifestError> {
        if !valid_cog_id(&self.id) {
            return Err(invalid(format!("bad cog id {:?}", self.id)));
        }
        if !valid_token(&self.version, 64) {
            return Err(invalid(format!("bad version {:?}", self.version)));
        }
        self.cog_toml.validate()?;
        if self.cog_toml.path != "cog.toml" {
            return Err(invalid("cog_toml.path must be cog.toml"));
        }
        if self.binaries.is_empty() {
            return Err(invalid("a cog package needs at least one binary"));
        }
        for (arch, f) in &self.binaries {
            if !COG_ARCHES.contains(&arch.as_str()) {
                return Err(invalid(format!("unknown arch {arch:?}")));
            }
            f.validate()?;
            if f.path != binary_path(arch, &self.id) {
                return Err(invalid(format!(
                    "{arch} binary must be at {}",
                    binary_path(arch, &self.id)
                )));
            }
        }
        for a in &self.attestations {
            if !valid_token(&a.kind, 64) {
                return Err(invalid(format!("bad attestation kind {:?}", a.kind)));
            }
            a.file.validate()?;
            if !a.file.path.starts_with("attestations/") {
                return Err(invalid("attestation files live under attestations/"));
            }
        }
        self.source.validate()?;
        let mut paths: Vec<&str> = self.files().map(|f| f.path.as_str()).collect();
        paths.sort_unstable();
        if paths.windows(2).any(|w| w[0] == w[1]) {
            return Err(invalid("duplicate file path"));
        }
        Ok(())
    }

    /// Every file the package lists.
    pub fn files(&self) -> impl Iterator<Item = &FileRef> {
        std::iter::once(&self.cog_toml)
            .chain(self.binaries.values())
            .chain(self.attestations.iter().map(|a| &a.file))
    }
}

/// Canonical in-package path of a cog binary for `arch`.
pub fn binary_path(arch: &str, id: &str) -> String {
    if arch == "wasm" {
        format!("wasm/{id}.wasm")
    } else {
        format!("{arch}/cog-{id}")
    }
}

/// Cog ids: lower-case alphanumerics separated by single hyphens.
pub fn valid_cog_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && !s.starts_with('-')
        && !s.ends_with('-')
        && !s.contains("--")
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Printable token: `[A-Za-z0-9][A-Za-z0-9._+:-]*`, bounded.
pub fn valid_token(s: &str, max: usize) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= max
        && b[0].is_ascii_alphanumeric()
        && b.iter()
            .all(|c| c.is_ascii_alphanumeric() || b"._+:-".contains(c))
}

/// Relative, normalized, forward-slash path with no traversal.
pub fn validate_rel_path(p: &str) -> Result<(), ManifestError> {
    let ok = !p.is_empty()
        && p.len() <= 256
        && !p.starts_with('/')
        && p.split('/')
            .all(|c| !c.is_empty() && c != "." && c != ".." && valid_token(c, 128));
    if ok {
        Ok(())
    } else {
        Err(invalid(format!("unsafe path {p:?}")))
    }
}
