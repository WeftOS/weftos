//! Node secret generation for pairing (ADR-126 section 3).
//!
//! The parent creates the RustFS user key when it pairs a node and keeps it;
//! the edge never sees it. The crate does no I/O, so the pure function takes
//! entropy from the host. The `keygen` feature adds a convenience that reads
//! the OS generator through `getrandom`.

use crate::policy::{principal_for, validate_node_id, NameError};
use crate::rustfs::StaticKey;

/// Entropy bytes in a node secret (160 bits).
pub const SECRET_ENTROPY_BYTES: usize = 20;
/// Characters in the encoded secret (hex, within the 8..40 range S3 stores accept).
pub const SECRET_LEN: usize = SECRET_ENTROPY_BYTES * 2;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum KeygenError {
    #[error("entropy source failed")]
    Entropy,
    #[error("entropy is all zero or constant; refusing a guessable secret")]
    Degenerate,
    #[error(transparent)]
    Node(#[from] NameError),
}

/// Encode host-supplied entropy as a secret key. Refuses constant input so a
/// broken generator cannot silently mint a guessable key.
pub fn secret_from_entropy(entropy: &[u8; SECRET_ENTROPY_BYTES]) -> Result<String, KeygenError> {
    if entropy.iter().all(|b| *b == entropy[0]) {
        return Err(KeygenError::Degenerate);
    }
    Ok(crate::sigv4::hex(entropy))
}

/// Build the `node-<id>` user key from host entropy.
pub fn node_key_from_entropy(
    node_id: &str,
    entropy: &[u8; SECRET_ENTROPY_BYTES],
) -> Result<StaticKey, KeygenError> {
    validate_node_id(node_id)?;
    Ok(StaticKey {
        access_key: principal_for(node_id),
        secret_key: secret_from_entropy(entropy)?,
    })
}

/// Generate a key for `node_id` from the OS generator.
#[cfg(feature = "keygen")]
pub fn generate_node_key(node_id: &str) -> Result<StaticKey, KeygenError> {
    let mut buf = [0u8; SECRET_ENTROPY_BYTES];
    getrandom::getrandom(&mut buf).map_err(|_| KeygenError::Entropy)?;
    let key = node_key_from_entropy(node_id, &buf);
    zeroize::Zeroize::zeroize(&mut buf);
    key
}
