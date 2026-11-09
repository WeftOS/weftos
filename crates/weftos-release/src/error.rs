//! Errors for release stores.

use crate::backend::BackendError;

/// Everything that can go wrong publishing, reading or sweeping a release store.
#[derive(Debug, thiserror::Error)]
pub enum ReleaseError {
    /// A name, id, path or field failed validation. Nothing was written.
    #[error("invalid {what}: {why}")]
    Invalid { what: &'static str, why: String },

    /// The build id already has a different release. Release stores are write-once: a rebuild
    /// needs a new build id.
    #[error("build {build_id} is already published with different content; a rebuild needs a new build id")]
    AlreadyPublished { build_id: String },

    /// The build id belonged to a release that retention removed. Ids are never reused.
    #[error("build {build_id} was removed by retention and its id is retired")]
    Retired { build_id: String },

    /// An object already stored under a content hash does not hash to it. The store is damaged
    /// or was tampered with. It is never overwritten here; an operator must look.
    #[error("stored blob {hash} does not match its hash; refusing to overwrite or trust it")]
    BlobConflict { hash: String },

    /// An object a manifest needs is not in the store.
    #[error("missing object {key}")]
    Missing { key: String },

    /// A stored object failed to parse.
    #[error("malformed {what}: {why}")]
    Malformed { what: &'static str, why: String },

    /// A manifest is not signed by a trusted release key, or is for another store.
    #[error("manifest rejected: {0}")]
    Untrusted(String),

    /// An artifact's bytes do not match the hash or size its manifest records.
    #[error("artifact {path} does not match its manifest entry")]
    HashMismatch { path: String },

    /// The signer failed.
    #[error("signing failed: {0}")]
    Sign(String),

    /// The store backend failed.
    #[error(transparent)]
    Backend(#[from] BackendError),
}

/// Result alias for this crate.
pub type Result<T> = std::result::Result<T, ReleaseError>;

pub(crate) fn invalid(what: &'static str, why: impl Into<String>) -> ReleaseError {
    ReleaseError::Invalid { what, why: why.into() }
}
