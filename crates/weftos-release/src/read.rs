//! Reading: every read checks the signature, the store name and the content hash.

use crate::backend::ReleaseBackend;
use crate::error::{ReleaseError, Result};
use crate::manifest::{hash_bytes, Manifest, SignedManifest, TrustedKeys};
use crate::name::{blob_key, build_id_from_manifest_key, manifest_key, ReleaseStoreName, RELEASES};

/// A manifest whose signature and store name have been checked. Only [`ReleaseReader`] makes one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedManifest(Manifest);

impl VerifiedManifest {
    pub fn manifest(&self) -> &Manifest {
        &self.0
    }
}

pub struct ReleaseReader<'a, B: ReleaseBackend> {
    backend: &'a B,
    store: &'a ReleaseStoreName,
    trusted: &'a TrustedKeys,
}

impl<'a, B: ReleaseBackend> ReleaseReader<'a, B> {
    pub fn new(backend: &'a B, store: &'a ReleaseStoreName, trusted: &'a TrustedKeys) -> Self {
        Self { backend, store, trusted }
    }

    /// Build ids that have a manifest object (signed or not), sorted.
    pub async fn build_ids(&self) -> Result<Vec<String>> {
        let mut ids: Vec<String> = self
            .backend
            .list(RELEASES)
            .await?
            .iter()
            .filter_map(|k| build_id_from_manifest_key(k).map(str::to_string))
            .collect();
        ids.sort();
        ids.dedup();
        Ok(ids)
    }

    /// The release's manifest, if its signature verifies for this store.
    pub async fn manifest(&self, build_id: &str) -> Result<VerifiedManifest> {
        let key = manifest_key(build_id);
        let bytes = self.backend.get(&key).await?.ok_or(ReleaseError::Missing { key })?;
        let signed = SignedManifest::parse(&bytes)?;
        signed.verify(self.trusted, &self.store.uri())?;
        if signed.manifest.build_id != build_id {
            return Err(ReleaseError::Untrusted(format!(
                "manifest at {build_id} claims build {}",
                signed.manifest.build_id
            )));
        }
        Ok(VerifiedManifest(signed.manifest))
    }

    /// One artifact's bytes, hash- and size-checked against the manifest.
    pub async fn read_artifact(&self, m: &VerifiedManifest, path: &str) -> Result<Vec<u8>> {
        let entry = m
            .0
            .artifacts
            .iter()
            .find(|a| a.path == path)
            .ok_or_else(|| ReleaseError::Missing { key: format!("{}:{path}", m.0.build_id) })?;
        let key = blob_key(&entry.hash);
        let bytes = self.backend.get(&key).await?.ok_or(ReleaseError::Missing { key })?;
        if bytes.len() as u64 != entry.size || hash_bytes(&bytes) != entry.hash {
            return Err(ReleaseError::HashMismatch { path: path.to_string() });
        }
        Ok(bytes)
    }

    /// Re-hash every artifact. Returns the paths that are missing or damaged.
    pub async fn audit(&self, m: &VerifiedManifest) -> Result<Vec<String>> {
        let mut bad = Vec::new();
        for a in &m.0.artifacts {
            match self.read_artifact(m, &a.path).await {
                Ok(_) => {}
                Err(ReleaseError::Missing { .. } | ReleaseError::HashMismatch { .. }) => {
                    bad.push(a.path.clone())
                }
                Err(e) => return Err(e),
            }
        }
        Ok(bad)
    }
}
