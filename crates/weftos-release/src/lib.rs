//! ADR-126 S4: release stores.
//!
//! CI writes a release's artifacts once into `weftos://<mesh>/projects/<ULID>/releases/<channel>/store`.
//! Artifacts are stored and referenced by BLAKE3 hash, the release is a signed manifest, retention
//! removes old releases without freeing their ids, and a rebuild cannot overwrite anything.
//!
//! # How "cannot overwrite" is held
//!
//! Three layers, so no single one is the only defence:
//! 1. **The API.** [`ReleasePublisher`] is generic over [`ReleaseBackend`], which has no
//!    unconditional put and no delete. A build id that exists with different content is
//!    [`ReleaseError::AlreadyPublished`]; the same content is an idempotent retry.
//! 2. **The server.** `put_if_absent` maps to PutObject `If-None-Match: *` (412 on an existing
//!    key, verified in ADR-126 S0), so a second writer cannot win a race.
//! 3. **The grant.** The CI principal's policy has PutObject and GetObject and no DeleteObject
//!    (ADR-126 §3). Only the daemon's retention sweeper holds delete, through [`DeletableBackend`].
//!
//! # Layout inside the store
//!
//! ```text
//! blobs/<blake3 hex>                    artifact bytes, content-addressed
//! releases/<build id>/manifest.json     signed manifest (the commit point, written last)
//! tombstones/<build id>                 retired ids and the blobs they referenced
//! ```
//!
//! # Grants S1 must apply (the API alone is not the boundary)
//!
//! - **CI principal:** PutObject and GetObject on `blobs/*` and `releases/*`, and GetObject (read only)
//!   on `tombstones/*`, which a publish checks. No DeleteObject, and no PutObject on `tombstones/*`: a
//!   writer there could retire build ids or nominate live-but-unreferenced blobs for collection.
//! - **Daemon sweeper:** the only holder of DeleteObject and of PutObject on `tombstones/*`. It also
//!   needs PutObject and server-side copy on `blobs/*` and `quarantine/*`, to set a blob aside and undo a delete (below).
//! - **Everyone else in the project:** read.
//!
//! # Publish versus sweep
//!
//! A publish may reuse a blob that a sweep is about to delete. Three steps close that, in order:
//! the sweeper re-reads every manifest right before deleting a blob; it sets the blob aside with a
//! server-side copy to `quarantine/<hash>` (the bytes never pass through the sweeper), deletes, then
//! re-reads the manifests again and **copies the blob back** if any now names it (or if they cannot be
//! read); and the publisher, after its manifest commits, re-puts every blob it needs. A manifest
//! written before the sweeper's second read is seen by it; one written after is followed by the
//! publisher's re-put, which finds the blob gone and fills it. Either way no committed release is
//! left without its blob, provided the store lists and reads strongly consistently (S3 and RustFS do).
//! A reader can see `Missing` for the instant between the delete and the restore and should retry.
//!
//! A sweeper that dies in between leaves its `quarantine/` copy. The next sweep starts by putting back
//! every set-aside blob a manifest still names, then drops the copies. Until then the hole is visible
//! to [`ReleaseReader::audit`], and re-running the publish for that build id puts the bytes back too.
//!
//! A principal with write access but not the signing key can still plant junk objects or squat a build
//! id. Readers and the sweeper treat anything unsigned as untrusted, and the sweeper reports it and
//! leaves it in place for the operator.
//!
//! # Version history and the CI route (feature `store`)
//!
//! A versioned (Object Lock) bucket keeps every write. [`ReleaseReader::history`] reads that history
//! and reports a manifest, tombstone or blob replaced with different bytes, and a manifest deleted with
//! no tombstone. It is how the direct CI route's one weakness (a key that can write can replace) is made
//! visible. `route` holds the two ways CI reaches the bucket, daemon-mediated `append` and the direct
//! role, behind one constant (`route::DEFAULT_CI_ROUTE`). See
//! `docs/research/adr126-s4-release-bucket-immutability-2026-11-08.md`.
//!
//! # What this crate does not do
//!
//! The S3 adapter, the name-to-bucket mapping, grants and the `weaver` verb belong to
//! `weftos-store` (ADR-126 S1). Artifacts are passed as byte slices; a streaming/multipart path
//! for multi-gigabyte files comes with the S1 client.

pub mod backend;
pub mod error;
pub mod manifest;
pub mod name;
pub mod publish;
pub mod read;
pub mod retention;
#[cfg(feature = "store")]
pub mod route;
#[cfg(feature = "store")]
pub mod s3;

pub use backend::{BackendError, DeletableBackend, KeyVersion, Lock, LockMode, MemoryBackend, PutOutcome, ReleaseBackend};
pub use error::{ReleaseError, Result};
pub use manifest::{
    ArtifactRef, Manifest, ReleaseSigner, SignedManifest, TrustedKey, TrustedKeys, SCHEMA,
};
pub use name::ReleaseStoreName;
pub use publish::{
    LockPolicy, MAX_BACKDATE_SECS, MAX_FUTURE_SKEW_SECS, MAX_LOCK_FLOOR_SECS, Artifact, PublishOutcome, PublishReceipt, PublishRequest, ReleasePublisher,
};
pub use read::{HistoryFinding, HistoryReport, ReleaseReader, VerifiedManifest};
pub use retention::{
    plan, KeepReason, ReleaseMeta, RetentionPlan, RetentionPolicy, SweepReport, Sweeper, Tombstone,
};
