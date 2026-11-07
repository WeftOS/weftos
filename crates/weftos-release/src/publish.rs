//! Publishing: how CI writes a release once.

use std::collections::BTreeSet;

use crate::backend::{PutOutcome, ReleaseBackend};
use crate::error::{invalid, ReleaseError, Result};
use crate::manifest::{
    hash_bytes, validate_artifact_path, ArtifactRef, Manifest, ReleaseSigner, SignedManifest,
    TrustedKeys, MAX_ARTIFACTS, SCHEMA,
};
use crate::name::{blob_key, manifest_key, tombstone_key, validate_build_id, ReleaseStoreName};

/// How far ahead of the publisher's clock `created_at` may be.
pub const MAX_FUTURE_SKEW_SECS: u64 = 300;
/// How far behind the publisher's clock `created_at` may be (a retried CI job).
pub const MAX_BACKDATE_SECS: u64 = 7 * 24 * 3600;

/// One file to publish.
pub struct Artifact {
    pub path: String,
    pub bytes: Vec<u8>,
}

/// A release to publish. `created_at` and `build_id` must be stable across retries of one CI job.
pub struct PublishRequest {
    pub build_id: String,
    pub source_rev: String,
    pub created_at: u64,
    pub builder: String,
    pub artifacts: Vec<Artifact>,
}

/// What a publish did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishOutcome {
    /// The manifest was written by this call.
    Published,
    /// The identical release was already there (a retried job). Nothing changed.
    AlreadyPresent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishReceipt {
    pub outcome: PublishOutcome,
    pub build_id: String,
    /// The release's identity ([`Manifest::digest`]).
    pub manifest_digest: String,
    pub artifacts: Vec<ArtifactRef>,
}

/// Writes releases into one release store. Holds only a [`ReleaseBackend`], so it cannot overwrite
/// or delete anything.
pub struct ReleasePublisher<'a, B: ReleaseBackend> {
    backend: &'a B,
    store: &'a ReleaseStoreName,
    signer: &'a ReleaseSigner,
}

impl<'a, B: ReleaseBackend> ReleasePublisher<'a, B> {
    pub fn new(backend: &'a B, store: &'a ReleaseStoreName, signer: &'a ReleaseSigner) -> Self {
        Self { backend, store, signer }
    }

    /// `now` is the publisher's clock (ADR-124). `created_at` must be at most [`MAX_FUTURE_SKEW_SECS`]
    /// ahead of it and at most [`MAX_BACKDATE_SECS`] behind it, because retention ranks releases by it.
    pub async fn publish(&self, req: PublishRequest, now: u64) -> Result<PublishReceipt> {
        validate_build_id(&req.build_id)?;
        if req.created_at > now.saturating_add(MAX_FUTURE_SKEW_SECS)
            || req.created_at.saturating_add(MAX_BACKDATE_SECS) < now
        {
            return Err(invalid("created_at", "too far from the publisher's clock"));
        }
        let manifest = self.build_manifest(&req)?;

        // Existing manifest first, tombstone second. The sweeper writes the tombstone before it
        // deletes a manifest, so a manifest that is gone because it was retired always has its
        // tombstone by the time we look. The other order leaves a window where both checks pass.
        if let Some(existing) = self.read_existing(&req.build_id).await? {
            let receipt = self.same_or_conflict(&manifest, &existing)?;
            // A retry is also the repair path: a sweeper that crashed after deleting a blob a
            // fresh release needed leaves a hole only the bytes in hand can fill.
            self.heal(&req).await?;
            return Ok(receipt);
        }
        if self.backend.get(&tombstone_key(&req.build_id)).await?.is_some() {
            return Err(ReleaseError::Retired { build_id: req.build_id });
        }

        // Blobs first. They are content-addressed, so a duplicate is harmless and a crash here
        // leaves unreferenced blobs and no release.
        let mut done = BTreeSet::new();
        for a in &req.artifacts {
            let hash = hash_bytes(&a.bytes);
            if done.insert(hash.clone()) {
                self.put_blob(&hash, &a.bytes).await?;
            }
        }

        // The manifest is the commit point.
        let signed = self.signer.sign(manifest.clone())?;
        match self.backend.put_if_absent(&manifest_key(&req.build_id), &signed.to_bytes()).await? {
            PutOutcome::Created => {
                // A sweep that listed manifests before this commit may delete a blob we reused.
                // The bytes are still in hand, so put them back; Exists is the normal answer.
                self.heal(&req).await?;
                Ok(receipt(PublishOutcome::Published, &manifest))
            }
            PutOutcome::Exists => {
                // Lost a race with another publisher of this build id.
                let existing = self.read_existing(&req.build_id).await?.ok_or_else(|| {
                    ReleaseError::Missing { key: manifest_key(&req.build_id) }
                })?;
                let receipt = self.same_or_conflict(&manifest, &existing)?;
                self.heal(&req).await?;
                Ok(receipt)
            }
        }
    }

    /// Put back any blob of this release that has gone missing. `Exists` is the normal answer.
    async fn heal(&self, req: &PublishRequest) -> Result<()> {
        let mut done = BTreeSet::new();
        for a in &req.artifacts {
            let hash = hash_bytes(&a.bytes);
            if done.insert(hash.clone()) {
                self.put_blob(&hash, &a.bytes).await?;
            }
        }
        Ok(())
    }

    fn build_manifest(&self, req: &PublishRequest) -> Result<Manifest> {
        if req.artifacts.is_empty() {
            return Err(invalid("release", "a release needs at least one artifact"));
        }
        if req.artifacts.len() > MAX_ARTIFACTS {
            return Err(invalid("release", format!("more than {MAX_ARTIFACTS} artifacts")));
        }
        let mut refs = Vec::with_capacity(req.artifacts.len());
        let mut paths = BTreeSet::new();
        for a in &req.artifacts {
            validate_artifact_path(&a.path)?;
            if !paths.insert(a.path.as_str()) {
                return Err(invalid("artifact path", format!("{:?} appears twice", a.path)));
            }
            refs.push(ArtifactRef {
                path: a.path.clone(),
                hash: hash_bytes(&a.bytes),
                size: a.bytes.len() as u64,
            });
        }
        refs.sort_by(|a, b| a.path.cmp(&b.path));
        let m = Manifest {
            schema: SCHEMA,
            store: self.store.uri(),
            channel: self.store.channel().to_string(),
            build_id: req.build_id.clone(),
            source_rev: req.source_rev.clone(),
            created_at: req.created_at,
            builder: req.builder.clone(),
            artifacts: refs,
        };
        m.validate()?;
        Ok(m)
    }

    /// Store a blob, or confirm the stored one is intact. Never overwrites.
    async fn put_blob(&self, hash: &str, bytes: &[u8]) -> Result<()> {
        let key = blob_key(hash);
        if self.backend.put_if_absent(&key, bytes).await? == PutOutcome::Created {
            return Ok(());
        }
        // Already there: a rebuild of the same bytes, or another release sharing the file. Do not
        // trust the name; check the bytes.
        match self.backend.get(&key).await? {
            Some(stored) if hash_bytes(&stored) == hash => Ok(()),
            Some(_) => Err(ReleaseError::BlobConflict { hash: hash.to_string() }),
            None => Err(ReleaseError::Missing { key }),
        }
    }

    async fn read_existing(&self, build_id: &str) -> Result<Option<SignedManifest>> {
        match self.backend.get(&manifest_key(build_id)).await? {
            Some(bytes) => Ok(Some(SignedManifest::parse(&bytes)?)),
            None => Ok(None),
        }
    }

    fn same_or_conflict(&self, ours: &Manifest, existing: &SignedManifest) -> Result<PublishReceipt> {
        // A retry only counts if the stored release is really ours: signed by this signer, for this store.
        existing.verify(&TrustedKeys(vec![self.signer.trusted()]), &self.store.uri())?;
        if ours.same_content(&existing.manifest) {
            Ok(receipt(PublishOutcome::AlreadyPresent, &existing.manifest))
        } else {
            Err(ReleaseError::AlreadyPublished { build_id: ours.build_id.clone() })
        }
    }
}

fn receipt(outcome: PublishOutcome, m: &Manifest) -> PublishReceipt {
    PublishReceipt {
        outcome,
        build_id: m.build_id.clone(),
        manifest_digest: m.digest(),
        artifacts: m.artifacts.clone(),
    }
}
