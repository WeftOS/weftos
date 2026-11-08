//! Publishing: how CI writes a release once.

use std::collections::BTreeSet;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::backend::{Lock, LockMode, PutOutcome, ReleaseBackend};
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

/// One file to publish from disk. Read in chunks, never held whole in memory, so there is no size
/// limit here beyond what the backend's upload path accepts.
pub struct FileArtifact {
    pub path: String,
    pub file: PathBuf,
}

/// A release to publish from files on disk (see [`ReleasePublisher::publish_files`]).
pub struct FileRequest {
    pub build_id: String,
    pub source_rev: String,
    pub created_at: u64,
    pub builder: String,
    pub artifacts: Vec<FileArtifact>,
}

/// An artifact as the publisher handles it: hashed once, bytes in memory or read from a file.
struct Item<'a> {
    path: &'a str,
    hash: String,
    size: u64,
    src: Src<'a>,
}

#[derive(Clone, Copy)]
enum Src<'a> {
    Bytes(&'a [u8]),
    File(&'a Path),
}

/// BLAKE3 of a file, read in 1 MiB chunks. Returns the hash and the byte count.
pub(crate) fn hash_file(path: &Path) -> std::io::Result<(String, u64)> {
    let mut f = std::fs::File::open(path)?;
    let mut h = blake3::Hasher::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut n = 0u64;
    loop {
        let k = f.read(&mut buf)?;
        if k == 0 {
            break;
        }
        h.update(&buf[..k]);
        n += k as u64;
    }
    Ok((h.finalize().to_hex().to_string(), n))
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

/// How long, and how hard, to lock what a publish writes. Needs a bucket made with Object Lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LockPolicy {
    pub mode: LockMode,
    /// Seconds from the publisher's `now`.
    pub floor_secs: u64,
}

/// Longest floor accepted (ten years): a typo should not become an irrevocable COMPLIANCE lock.
pub const MAX_LOCK_FLOOR_SECS: u64 = 10 * 365 * 24 * 3600;

impl LockPolicy {
    pub fn new(mode: LockMode, floor_secs: u64) -> Result<Self> {
        if floor_secs == 0 || floor_secs > MAX_LOCK_FLOOR_SECS {
            return Err(invalid("lock floor", "must be between one second and ten years"));
        }
        Ok(Self { mode, floor_secs })
    }

    pub(crate) fn at(&self, now: u64) -> Lock {
        Lock { mode: self.mode, until: now.saturating_add(self.floor_secs) }
    }
}

/// Writes releases into one release store. Holds only a [`ReleaseBackend`], so it cannot overwrite
/// or delete anything.
pub struct ReleasePublisher<'a, B: ReleaseBackend> {
    backend: &'a B,
    store: &'a ReleaseStoreName,
    signer: &'a ReleaseSigner,
    lock: Option<LockPolicy>,
}

impl<'a, B: ReleaseBackend> ReleasePublisher<'a, B> {
    pub fn new(backend: &'a B, store: &'a ReleaseStoreName, signer: &'a ReleaseSigner) -> Self {
        Self { backend, store, signer, lock: None }
    }

    /// Lock every object this publisher writes (manifest and new blobs) for the policy's floor. A blob
    /// that already exists keeps the lock it was written with: the transport has no way to extend it.
    pub fn with_lock(mut self, policy: LockPolicy) -> Self {
        self.lock = Some(policy);
        self
    }

    /// `now` is the publisher's clock (ADR-124). `created_at` must be at most [`MAX_FUTURE_SKEW_SECS`]
    /// ahead of it and at most [`MAX_BACKDATE_SECS`] behind it, because retention ranks releases by it.
    pub async fn publish(&self, req: PublishRequest, now: u64) -> Result<PublishReceipt> {
        let items: Vec<Item> = req
            .artifacts
            .iter()
            .map(|a| Item { path: &a.path, hash: hash_bytes(&a.bytes), size: a.bytes.len() as u64, src: Src::Bytes(&a.bytes) })
            .collect();
        self.publish_items(&req.build_id, &req.source_rev, req.created_at, &req.builder, items, now).await
    }

    /// Like [`publish`](Self::publish), for files on disk: each is hashed in chunks and uploaded through
    /// the backend's file path (multipart on S3 once S1 has it), so a multi-gigabyte artifact never sits
    /// in memory. The files must not change while this runs; a file that does is caught because its
    /// hash is checked again against what the store ends up holding.
    pub async fn publish_files(&self, req: FileRequest, now: u64) -> Result<PublishReceipt> {
        let mut items = Vec::with_capacity(req.artifacts.len());
        for a in &req.artifacts {
            let (hash, size) = hash_file(&a.file).map_err(|e| invalid("artifact file", format!("{}: {e}", a.file.display())))?;
            items.push(Item { path: &a.path, hash, size, src: Src::File(&a.file) });
        }
        self.publish_items(&req.build_id, &req.source_rev, req.created_at, &req.builder, items, now).await
    }

    async fn publish_items(
        &self,
        build_id: &str,
        source_rev: &str,
        created_at: u64,
        builder: &str,
        items: Vec<Item<'_>>,
        now: u64,
    ) -> Result<PublishReceipt> {
        validate_build_id(build_id)?;
        if created_at > now.saturating_add(MAX_FUTURE_SKEW_SECS)
            || created_at.saturating_add(MAX_BACKDATE_SECS) < now
        {
            return Err(invalid("created_at", "too far from the publisher's clock"));
        }
        let manifest = self.build_manifest(build_id, source_rev, created_at, builder, &items)?;
        let lock = self.lock.map(|p| p.at(now));
        if lock.is_some() {
            self.check_lock_ready().await?;
        }

        // Existing manifest first, tombstone second. The sweeper writes the tombstone before it
        // deletes a manifest, so a manifest that is gone because it was retired always has its
        // tombstone by the time we look. The other order leaves a window where both checks pass.
        if let Some(existing) = self.read_existing(build_id).await? {
            let receipt = self.same_or_conflict(&manifest, &existing)?;
            // A retry is also the repair path: a sweeper that crashed after deleting a blob a
            // fresh release needed leaves a hole only the bytes in hand can fill.
            self.heal(&items, lock.as_ref()).await?;
            return Ok(receipt);
        }
        if self.backend.get(&tombstone_key(build_id)).await?.is_some() {
            return Err(ReleaseError::Retired { build_id: build_id.to_string() });
        }

        // Blobs first. They are content-addressed, so a duplicate is harmless and a crash here
        // leaves unreferenced blobs and no release.
        self.heal(&items, lock.as_ref()).await?;

        // The manifest is the commit point.
        let signed = self.signer.sign(manifest.clone())?;
        let mkey = manifest_key(build_id);
        let mbytes = signed.to_bytes();
        let put = match lock.as_ref() {
            Some(l) => self.backend.put_if_absent_locked(&mkey, &mbytes, l).await?,
            None => self.backend.put_if_absent(&mkey, &mbytes).await?,
        };
        match put {
            PutOutcome::Created => {
                // A sweep that listed manifests before this commit may delete a blob we reused.
                // The bytes are still in hand, so put them back; Exists is the normal answer.
                self.heal(&items, lock.as_ref()).await?;
                Ok(receipt(PublishOutcome::Published, &manifest))
            }
            PutOutcome::Exists => {
                // Lost a race with another publisher of this build id.
                let existing = self.read_existing(build_id).await?.ok_or_else(|| {
                    ReleaseError::Missing { key: manifest_key(build_id) }
                })?;
                let receipt = self.same_or_conflict(&manifest, &existing)?;
                self.heal(&items, lock.as_ref()).await?;
                Ok(receipt)
            }
        }
    }

    /// Read back the bucket's lock configuration before writing anything locked. A bucket that reports
    /// Object Lock off is refused here with one clear error, instead of failing on the first put (or,
    /// on a backend that ignores lock headers, quietly storing unprotected objects). A backend that
    /// cannot report it answers `None` and the first locked put is the test.
    async fn check_lock_ready(&self) -> Result<()> {
        match self.backend.lock_config().await? {
            Some(cfg) if !cfg.enabled => {
                Err(invalid("lock", "the bucket was not made with Object Lock; locks cannot be added later"))
            }
            _ => Ok(()),
        }
    }

    /// Put every blob of this release (once per distinct hash), and where the backend can, lengthen the
    /// lock of one that was written earlier with a shorter retention. `Exists` is the normal answer.
    async fn heal(&self, items: &[Item<'_>], lock: Option<&Lock>) -> Result<()> {
        let mut done = BTreeSet::new();
        for it in items {
            if done.insert(it.hash.clone()) {
                self.put_blob(it, lock).await?;
            }
        }
        Ok(())
    }

    fn build_manifest(&self, build_id: &str, source_rev: &str, created_at: u64, builder: &str, items: &[Item<'_>]) -> Result<Manifest> {
        if items.is_empty() {
            return Err(invalid("release", "a release needs at least one artifact"));
        }
        if items.len() > MAX_ARTIFACTS {
            return Err(invalid("release", format!("more than {MAX_ARTIFACTS} artifacts")));
        }
        let mut refs = Vec::with_capacity(items.len());
        let mut paths = BTreeSet::new();
        for a in items {
            validate_artifact_path(a.path)?;
            if !paths.insert(a.path) {
                return Err(invalid("artifact path", format!("{:?} appears twice", a.path)));
            }
            refs.push(ArtifactRef { path: a.path.to_string(), hash: a.hash.clone(), size: a.size });
        }
        refs.sort_by(|a, b| a.path.cmp(&b.path));
        let m = Manifest {
            schema: SCHEMA,
            store: self.store.uri(),
            channel: self.store.channel().to_string(),
            build_id: build_id.to_string(),
            source_rev: source_rev.to_string(),
            created_at,
            builder: builder.to_string(),
            artifacts: refs,
        };
        m.validate()?;
        Ok(m)
    }

    /// Store a blob, or confirm the stored one is intact. Never overwrites. A blob that already exists
    /// with a shorter lock than this publish wants is extended where the backend can.
    async fn put_blob(&self, it: &Item<'_>, lock: Option<&Lock>) -> Result<()> {
        let key = blob_key(&it.hash);
        let put = match (it.src, lock) {
            (Src::Bytes(b), Some(l)) => self.backend.put_if_absent_locked(&key, b, l).await?,
            (Src::Bytes(b), None) => self.backend.put_if_absent(&key, b).await?,
            (Src::File(f), l) => self.backend.put_file_if_absent(&key, f, it.size, l).await?,
        };
        if put == PutOutcome::Created {
            return Ok(());
        }
        // Already there: a rebuild of the same bytes, or another release sharing the file. Do not
        // trust the name; check the bytes.
        match self.backend.blob_digest(&key).await? {
            Some((h, n)) if h == it.hash && n == it.size => {}
            Some(_) => return Err(ReleaseError::BlobConflict { hash: it.hash.clone() }),
            None => return Err(ReleaseError::Missing { key }),
        }
        if let Some(want) = lock {
            self.lengthen(&key, want).await?;
        }
        Ok(())
    }

    /// Give an existing blob at least the lock this publish asked for. A backend that cannot read or
    /// extend retention leaves it as written (the old behaviour).
    async fn lengthen(&self, key: &str, want: &Lock) -> Result<()> {
        let Some(have) = self.backend.retention_of(key).await? else { return Ok(()) };
        if have.until >= want.until && have.mode == want.mode {
            return Ok(());
        }
        // Never shorten, never weaken the mode: ask for the later date and the stronger mode.
        let until = have.until.max(want.until);
        let mode = if have.mode == LockMode::Compliance || want.mode == LockMode::Compliance {
            LockMode::Compliance
        } else {
            LockMode::Governance
        };
        self.backend.extend_lock(key, &Lock { mode, until }).await?;
        Ok(())
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
