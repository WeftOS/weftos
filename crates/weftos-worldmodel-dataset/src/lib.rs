//! Dataset archiver for the LeWM world-model loop (plan W1).
//!
//! Takes a finished sensor session directory, validates the evidence and
//! readings files, builds a `lewm.dataset.v1` manifest and copies the files
//! to `<dest_root>/ruview-demo/datasets/<session>/` with checksums verified.

pub mod archive;
pub mod checksum;
pub mod error;
pub mod evidence;
pub mod layout;
pub mod manifest;
pub mod proof;
pub mod senml;

pub use archive::{ArchiveOutcome, ArchiveRequest, Verification, archive_session, verify_archive};
pub use error::DatasetError;
pub use layout::SessionLayout;
pub use manifest::Manifest;
