//! Personal vault core (Vault V1, board card 02301ad0).
//!
//! Implements the byte formats of `docs/research/vault-v0-format-spec-2026-11-15.md`:
//! a signed keybag ([`keybag`]), unlock by passcode or recovery key ([`Vault`]), and per-secret
//! sealed blobs bound to their slot ([`blob`]).
//!
//! **DRAFT.** The format has not had the external review the owner requires. Every vault created here
//! carries the DRAFT flag (there is no way to create one without it) and refuses real credentials
//! ([`SealRequest::real_credential`]). Every value in the tests is fake.
//!
//! Keybag writes are two-phase ([`PendingWrite`], [`Vault::commit_write`]): nothing changes in the
//! vault or the device pins until the caller reports that the store accepted the new keybag.
//!
//! No escrow wrapper, no node (unattended) wrapper: parsers reject those kinds. No type in this
//! crate prints a secret in `Debug` or `Display`, and the crate logs only counts and sequence
//! numbers.

#![forbid(unsafe_code)]

pub mod blob;
pub mod error;
pub mod index;
pub mod keybag;
pub mod recovery;
pub mod rotation;
pub mod seal;
pub mod vault;

mod hkdf_keys;
mod normalize;
mod passcode_kdf;
mod pins;
mod secrets;
mod wrap;
mod write;

pub use blob::{OpenedSecret, Policy, SecretType};
pub use error::VaultError;
pub use index::IndexEntry;
pub use keybag::{Keybag, Pins, SeenKeybag, Wrapper, WrapperBody};
pub use passcode_kdf::KdfParams;
pub use pins::PINS_LEN;
pub use recovery::RecoveryKey;
pub use rotation::{Resealed, RotationReport};
pub use seal::SecretSeal;
pub use secrets::{SealRequest, SealedSecret};
pub use vault::{NewVault, Vault};
pub use write::PendingWrite;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_index;
#[cfg(test)]
mod tests_kdf_floor;
#[cfg(test)]
mod tests_review;
#[cfg(test)]
mod tests_review_r2;
#[cfg(test)]
mod tests_rotation;
#[cfg(test)]
mod tests_secrets;
#[cfg(test)]
mod vectors;
