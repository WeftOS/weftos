//! Signed workload package manifests (mesh-placement-07; ADR-100 section 1,
//! ADR-099 section 8).
//!
//! A workload package is a directory holding a signed manifest envelope
//! (`cogpkg.json` for cogs) plus the files it lists, each pinned by BLAKE3
//! hash and size. The envelope is kind-agnostic (`kind = "cog"` today; model
//! manifests from ADR-101 reuse it with their own body), so signing and
//! verification never look inside the body beyond what the kind requires.
//!
//! Trust (ADR-099 section 8): at least one valid Ed25519 signature from the
//! pinned WeftOS signer set or an operator-pinned key is required. A
//! Cognitum ADR-154/155 release record is an optional, opt-in additional
//! verifier (ADR-100 section 6.2): when enabled and its registry key is
//! pinned, a valid record bound to one of the package binaries counts as a
//! signature.
//!
//! Layout of a cog package directory:
//!
//! ```text
//! cogpkg.json                 signed envelope (schema weftos.workload-manifest.v1)
//! cog.toml                    unmodified upstream manifest
//! aarch64/cog-<id>            one binary per arch
//! armv7/cog-<id>
//! attestations/<name>.json    optional external attestations (Cognitum record)
//! ```
//!
//! Files are stored in [`crate::artifact_store::ArtifactStore`] by BLAKE3
//! hash; the package id is the BLAKE3 hash of the signed statement.

pub mod canonical;
pub mod codec;
pub mod cognitum;
pub mod manifest;
pub mod pack;
pub mod sign;
pub mod store;
pub mod trust;
pub mod verify;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_decl;

pub use manifest::{
    AttestationRef, CogPackageBody, FileRef, KIND_COG, MANIFEST_FILE, MANIFEST_SCHEMA,
    MAX_MANIFEST_BYTES, ManifestEnvelope, ManifestError, PackageSource, SignatureEntry,
};
pub use pack::{CogPackInput, PackError, pack_cog, write_manifest, write_source_build_provenance};
pub use sign::{key_id_for, sign_envelope, signing_key_from_hex};
pub use store::{StoredPackage, store_package, verify_stored, verify_stored_in};
pub use trust::{KeyOrigin, PinnedKey, TrustAnchors, TrustFile};
pub use verify::{
    AcceptedSigner, DirSource, FileSource, VerifiedPackage, VerifyError, VerifyPolicy, verify_dir,
    verify_dir_in, verify_manifest_signatures, verify_manifest_signatures_in, verify_with_source,
    verify_with_source_in,
};
