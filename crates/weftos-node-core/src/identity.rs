//! Node identity. Same on-disk format and the same node-id derivation as
//! `clawft-android-edge` (`node_identity.json`, `hex(SHA-256(pubkey)[..16])`,
//! ADR-103 D11), so one phone has one node id whichever crate opened the file.
//! There is deliberately no second derivation anywhere (Kotlin must not invent one).

use crate::error::NodeError;
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;
use zeroize::Zeroize;

const IDENTITY_FILE: &str = "node_identity.json";

#[derive(Serialize, Deserialize)]
struct IdentityFile {
    seed_hex: String,
    public_key_hex: String,
    version: u32,
}

pub fn bytes_to_hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn hex_to_bytes(s: &str) -> Result<Vec<u8>, NodeError> {
    if !s.len().is_multiple_of(2) || !s.is_ascii() {
        return Err(NodeError::Codec("bad hex".into()));
    }
    (0..s.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| NodeError::Codec("bad hex".into()))
        })
        .collect()
}

/// `hex(SHA-256(pubkey)[..16])`; equal to `clawft_kernel::node_id_from_pubkey`.
pub fn node_id_from_pubkey(pubkey: &[u8; 32]) -> String {
    bytes_to_hex(&Sha256::digest(pubkey)[..16])
}

/// A node's signing identity, held in memory.
pub struct Identity {
    signing: SigningKey,
}

impl Identity {
    pub fn from_seed(seed: [u8; 32]) -> Self {
        Self {
            signing: SigningKey::from_bytes(&seed),
        }
    }

    pub fn generate() -> Self {
        Self {
            signing: SigningKey::generate(&mut OsRng),
        }
    }

    /// Load `<dir>/node_identity.json`, or create and persist one (mode 0600).
    pub fn load_or_create(dir: &Path) -> Result<Self, NodeError> {
        fs::create_dir_all(dir).map_err(NodeError::io)?;
        let path = dir.join(IDENTITY_FILE);
        if path.exists() {
            let raw = fs::read_to_string(&path).map_err(NodeError::io)?;
            let mut file: IdentityFile = serde_json::from_str(&raw)
                .map_err(|e| NodeError::Identity(format!("parse: {e}")))?;
            let mut seed = hex_to_bytes(&file.seed_hex)?;
            file.seed_hex.zeroize();
            let mut arr: [u8; 32] = seed
                .as_slice()
                .try_into()
                .map_err(|_| NodeError::Identity("seed must be 32 bytes".into()))?;
            seed.zeroize();
            let id = Self::from_seed(arr);
            arr.zeroize();
            if bytes_to_hex(&id.public_key()) != file.public_key_hex {
                return Err(NodeError::Identity(
                    "stored public key does not match seed".into(),
                ));
            }
            return Ok(id);
        }
        let id = Self::generate();
        let file = IdentityFile {
            seed_hex: bytes_to_hex(&id.signing.to_bytes()),
            public_key_hex: bytes_to_hex(&id.public_key()),
            version: 1,
        };
        let json = serde_json::to_string_pretty(&file)
            .map_err(|e| NodeError::Identity(format!("serialize: {e}")))?;
        fs::write(&path, json).map_err(NodeError::io)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
        }
        Ok(id)
    }

    pub fn public_key(&self) -> [u8; 32] {
        self.signing.verifying_key().to_bytes()
    }

    pub fn node_id(&self) -> String {
        node_id_from_pubkey(&self.public_key())
    }

    pub fn sign(&self, msg: &[u8]) -> [u8; 64] {
        self.signing.sign(msg).to_bytes()
    }
}

/// Strict Ed25519 verification against a raw public key.
pub fn verify(pubkey: &[u8; 32], msg: &[u8], sig: &[u8]) -> bool {
    let (Ok(vk), Ok(sig)) = (VerifyingKey::from_bytes(pubkey), Signature::from_slice(sig)) else {
        return false;
    };
    vk.verify_strict(msg, &sig).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_id_matches_kernel_vector() {
        assert_eq!(
            node_id_from_pubkey(&[0u8; 32]),
            "66687aadf862bd776c8fc18b8e9f8e20"
        );
    }

    #[test]
    fn stable_across_restarts_and_signs() {
        let dir = tempfile::tempdir().unwrap();
        let a = Identity::load_or_create(dir.path()).unwrap();
        let b = Identity::load_or_create(dir.path()).unwrap();
        assert_eq!(a.node_id(), b.node_id());
        let sig = a.sign(b"m");
        assert!(verify(&a.public_key(), b"m", &sig));
        assert!(!verify(&a.public_key(), b"x", &sig));
    }

    #[test]
    fn same_node_id_as_clawft_android_edge() {
        let dir = tempfile::tempdir().unwrap();
        let edge = clawft_android_edge::identity::load_or_create(dir.path()).unwrap();
        let core = Identity::load_or_create(dir.path()).unwrap();
        assert_eq!(edge.node_id_hex, core.node_id());
        assert_eq!(edge.public_key_hex, bytes_to_hex(&core.public_key()));
    }
}
