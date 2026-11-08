//! Signed node facts (ADR-099 section 2, card mesh-placement-03).
//!
//! The companion of [`crate::capability_claim::SignedCapabilityAdvertisement`]:
//! the coarse allow-listed tokens stay there, and the open-vocabulary
//! [`NodeFacts`] block is signed here with the same node Ed25519 key.
//!
//! The envelope carries the exact JSON bytes that were signed (`payload`),
//! so verification never depends on re-serialising floats or maps. The
//! signature covers a domain tag plus the payload, so facts, deltas and
//! capability claims can never be replayed as one another.
//!
//! Verification checks, in order: key and signature sizes, payload bound,
//! the signature, the payload's structure, that `node_id` is the id derived
//! from the signing key (so a node cannot speak for another), and the time
//! window (not expired, not issued in the future beyond [`MAX_CLOCK_SKEW_SECS`]).

use clawft_types::placement::{FactsDelta, FactsError, NodeFacts};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::node_registry::node_id_from_pubkey;

/// Domain tag signed before a facts payload.
pub const FACTS_DOMAIN: &[u8] = b"weftos.node_facts.v1\0";
/// Domain tag signed before a delta payload.
pub const DELTA_DOMAIN: &[u8] = b"weftos.node_facts_delta.v1\0";
/// Largest signed payload accepted, in bytes.
pub const MAX_PAYLOAD_BYTES: usize = 256 * 1024;
/// Tolerated clock skew for `issued_at`, in seconds.
pub const MAX_CLOCK_SKEW_SECS: u64 = 60;

/// Why a signed facts block or delta was refused.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NodeFactsAdvertError {
    /// Public key is not 32 bytes (or not a valid point).
    #[error("invalid public key")]
    BadPublicKey,
    /// Signature is not 64 bytes.
    #[error("invalid signature length (want 64, got {0})")]
    BadSignatureLen(usize),
    /// Signature does not verify: the payload was altered or another key signed it.
    #[error("signature verification failed")]
    BadSignature,
    /// Payload exceeds [`MAX_PAYLOAD_BYTES`].
    #[error("payload too large ({0} bytes)")]
    TooLarge(usize),
    /// Payload is not a well-formed facts block / delta.
    #[error("malformed payload: {0}")]
    Malformed(String),
    /// `node_id` is not the id derived from the signing key.
    #[error("node_id {claimed} does not belong to the signing key ({derived})")]
    NodeMismatch {
        /// Id in the payload.
        claimed: String,
        /// Id derived from the key.
        derived: String,
    },
    /// Structure, TTL or delta check failed.
    #[error(transparent)]
    Facts(#[from] FactsError),
}

/// A signed [`NodeFacts`] block as exchanged on the mesh.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedNodeFacts {
    /// Exact JSON bytes of the [`NodeFacts`] that were signed.
    pub payload: String,
    /// Ed25519 public key (32 bytes) of the advertising node.
    pub public_key: Vec<u8>,
    /// Ed25519 signature (64 bytes) over [`FACTS_DOMAIN`] || payload.
    pub signature: Vec<u8>,
}

/// A signed [`FactsDelta`] (busy/free update).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedFactsDelta {
    /// Exact JSON bytes of the [`FactsDelta`] that were signed.
    pub payload: String,
    /// Ed25519 public key (32 bytes).
    pub public_key: Vec<u8>,
    /// Ed25519 signature (64 bytes) over [`DELTA_DOMAIN`] || payload.
    pub signature: Vec<u8>,
}

fn signed_bytes(domain: &[u8], payload: &str) -> Vec<u8> {
    let mut v = Vec::with_capacity(domain.len() + payload.len());
    v.extend_from_slice(domain);
    v.extend_from_slice(payload.as_bytes());
    v
}

fn check_envelope(
    domain: &[u8],
    payload: &str,
    public_key: &[u8],
    signature: &[u8],
) -> Result<[u8; 32], NodeFactsAdvertError> {
    let pk: [u8; 32] = public_key
        .try_into()
        .map_err(|_| NodeFactsAdvertError::BadPublicKey)?;
    let sig: [u8; 64] = signature
        .try_into()
        .map_err(|_| NodeFactsAdvertError::BadSignatureLen(signature.len()))?;
    if payload.len() > MAX_PAYLOAD_BYTES {
        return Err(NodeFactsAdvertError::TooLarge(payload.len()));
    }
    let vk = VerifyingKey::from_bytes(&pk).map_err(|_| NodeFactsAdvertError::BadPublicKey)?;
    vk.verify_strict(&signed_bytes(domain, payload), &Signature::from_bytes(&sig))
        .map_err(|_| NodeFactsAdvertError::BadSignature)?;
    Ok(pk)
}

fn check_binding(node_id: &str, pk: &[u8; 32]) -> Result<(), NodeFactsAdvertError> {
    let derived = node_id_from_pubkey(pk);
    if node_id != derived {
        return Err(NodeFactsAdvertError::NodeMismatch {
            claimed: node_id.to_string(),
            derived,
        });
    }
    Ok(())
}

/// Sign facts with the node key. The facts must validate and their
/// `node_id` must be the id derived from `key`.
pub fn sign_node_facts(
    facts: &NodeFacts,
    key: &SigningKey,
) -> Result<SignedNodeFacts, NodeFactsAdvertError> {
    facts.validate()?;
    let pk = key.verifying_key().to_bytes();
    check_binding(&facts.node_id, &pk)?;
    let payload =
        serde_json::to_string(facts).map_err(|e| NodeFactsAdvertError::Malformed(e.to_string()))?;
    if payload.len() > MAX_PAYLOAD_BYTES {
        return Err(NodeFactsAdvertError::TooLarge(payload.len()));
    }
    let signature = key.sign(&signed_bytes(FACTS_DOMAIN, &payload));
    Ok(SignedNodeFacts {
        payload,
        public_key: pk.to_vec(),
        signature: signature.to_bytes().to_vec(),
    })
}

/// Verify a signed facts block at time `now` (unix seconds) and return the
/// facts. Tampered, foreign-keyed, expired or future-dated facts are refused.
pub fn verify_node_facts(
    signed: &SignedNodeFacts,
    now: u64,
) -> Result<NodeFacts, NodeFactsAdvertError> {
    let pk = check_envelope(
        FACTS_DOMAIN,
        &signed.payload,
        &signed.public_key,
        &signed.signature,
    )?;
    let facts: NodeFacts = serde_json::from_str(&signed.payload)
        .map_err(|e| NodeFactsAdvertError::Malformed(e.to_string()))?;
    check_binding(&facts.node_id, &pk)?;
    facts.check_window(now, MAX_CLOCK_SKEW_SECS)?;
    Ok(facts)
}

/// Sign a delta with the node key.
pub fn sign_facts_delta(
    delta: &FactsDelta,
    key: &SigningKey,
) -> Result<SignedFactsDelta, NodeFactsAdvertError> {
    let pk = key.verifying_key().to_bytes();
    check_binding(&delta.node_id, &pk)?;
    let payload =
        serde_json::to_string(delta).map_err(|e| NodeFactsAdvertError::Malformed(e.to_string()))?;
    let signature = key.sign(&signed_bytes(DELTA_DOMAIN, &payload));
    Ok(SignedFactsDelta {
        payload,
        public_key: pk.to_vec(),
        signature: signature.to_bytes().to_vec(),
    })
}

/// Verify a signed delta and return it with the signer's key. Whether it
/// fits the cached base is checked by the cache ([`NodeFacts::apply_delta`]).
pub fn verify_facts_delta(
    signed: &SignedFactsDelta,
    now: u64,
) -> Result<(FactsDelta, [u8; 32]), NodeFactsAdvertError> {
    let pk = check_envelope(
        DELTA_DOMAIN,
        &signed.payload,
        &signed.public_key,
        &signed.signature,
    )?;
    let delta: FactsDelta = serde_json::from_str(&signed.payload)
        .map_err(|e| NodeFactsAdvertError::Malformed(e.to_string()))?;
    check_binding(&delta.node_id, &pk)?;
    if delta.issued_at > now.saturating_add(MAX_CLOCK_SKEW_SECS) {
        return Err(FactsError::FromFuture {
            issued_at: delta.issued_at,
            now,
        }
        .into());
    }
    Ok((delta, pk))
}

#[cfg(test)]
#[path = "node_facts_advert_tests.rs"]
mod tests;
