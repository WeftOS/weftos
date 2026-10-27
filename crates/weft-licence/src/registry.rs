//! The registry fetch, reusing the Cogs release's `cog-sources` (https reader, registry
//! sha256 and size checks, https-to-http redirect refusal). Feature
//! `registry`; the Seed build also enables `net` for the https reader.
//!
//! The Cognitum registry lists one armhf (`arm`) binary per cog today
//! (question C7 asks about the rest), so other architectures answer
//! `arch_unavailable`.

use sha2::{Digest, Sha256};
use cog_sources::config::{CogLicence, CogSource, LicensedCogs, SourceKind};
use cog_sources::fetch::{Reader, MAX_BINARY_BYTES};
use cog_sources::resolve::{load_source, Listing, LoadedSource};
use cog_sources::{fetch_verified, FetchCtx, RevokedKeys, SourceError};

use crate::providers::{CogEntry, CogFetcher, EntryArtifact, FetchError};

/// Clamps every read to `max` bytes, so the 64 MiB limit applies even though
/// `fetch_verified` asks for 256 MiB.
struct BoundedReader {
    inner: Box<dyn Reader + Send + Sync>,
    max: u64,
}

impl Reader for BoundedReader {
    fn read(&self, location: &str, max: u64) -> cog_sources::Result<Vec<u8>> {
        self.inner.read(location, max.min(self.max))
    }
}

/// Fetches from one Cognitum registry.
pub struct RegistryFetcher {
    source: CogSource,
    reader: BoundedReader,
}

impl RegistryFetcher {
    /// `url` is the `app-registry.json` location. https only unless
    /// `allow_insecure` (lab: a local path or http, never on a real Seed).
    pub fn new(url: &str, allow_insecure: bool, reader: Box<dyn Reader + Send + Sync>, max_bytes: u64) -> Self {
        Self {
            source: CogSource {
                name: "cognitum".into(),
                kind: SourceKind::Cognitum,
                url: url.into(),
                pinned_keys: vec![],
                priority: 0,
                enabled: true,
                allow_insecure,
            },
            reader: BoundedReader { inner: reader, max: max_bytes.min(MAX_BINARY_BYTES) },
        }
    }

    fn load(&self) -> Result<LoadedSource, FetchError> {
        load_source(&self.source, &self.reader).map_err(|e| FetchError::Failed(e.to_string()))
    }
}

impl CogFetcher for RegistryFetcher {
    fn resolve(&self, cog_id: &str, version: &str) -> Result<CogEntry, FetchError> {
        let loaded = self.load()?;
        let Listing::Cognitum(reg) = &loaded.listing else {
            return Err(FetchError::Failed("not a cognitum listing".into()));
        };
        let cog = reg.cogs.iter().find(|c| c.id == cog_id).ok_or(FetchError::NotFound)?;
        let listed = if cog.version.is_empty() { "0.0.0".to_string() } else { cog.version.clone() };
        if version != "latest" && version != listed {
            return Err(FetchError::VersionUnavailable(listed));
        }
        let size = cog.binary_size.or(cog.size_kb.map(|k| k * 1024)).ok_or(FetchError::SizeUnknown)?;
        let sha = cog.sha256.clone().unwrap_or_default().to_ascii_lowercase();
        let manifest = serde_json::to_vec(cog).unwrap_or_default();
        Ok(CogEntry {
            cog_id: cog_id.into(),
            version: listed,
            registry: self.source.url.clone(),
            manifest_sha256: hex(&Sha256::digest(&manifest)),
            artifacts: vec![EntryArtifact { arch: "arm".into(), size, sha256: sha }],
        })
    }

    fn fetch(&self, entry: &CogEntry, arch: &str) -> Result<Vec<u8>, FetchError> {
        if arch != "arm" {
            return Err(FetchError::ArchUnavailable(arch.into()));
        }
        let loaded = self.load()?;
        // Our own licence check ran first; this stands in for the one
        // `fetch_verified` insists on so it can do the hash and size checks.
        let lic = CogLicence {
            source: self.source.name.clone(),
            cogs: LicensedCogs::All("all".into()),
            account: "weft-licence".into(),
            expires: None,
        };
        // Cognitum sources are sha256-pinned; `revoked` only applies to signed
        // WeftOS/private listings.
        let ctx = FetchCtx {
            reader: &self.reader,
            licences: &[lic],
            now: chrono::Utc::now(),
            extra_weftos_keys: &[],
            revoked: &RevokedKeys::none(),
        };
        fetch_verified(&loaded, &entry.cog_id, "arm", &ctx).map(|f| f.bytes).map_err(|e| match e {
            SourceError::Verify { reason, .. } => FetchError::Verify(reason),
            other => FetchError::Failed(other.to_string()),
        })
    }
}

fn hex(b: &[u8]) -> String {
    weft_licence_wire::hex_encode(b)
}
