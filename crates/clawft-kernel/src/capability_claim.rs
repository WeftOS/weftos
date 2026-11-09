//! Cross-node capability-claim verification (WEFT-147).
//!
//! Peer nodes previously advertised free-form capability strings on
//! [`crate::cluster::PeerNode`] with no authenticity check. This module
//! provides:
//!
//! 1. An **allow-list** of known capability tokens.
//! 2. A canonical claim payload + Ed25519 signed advertisement.
//! 3. Verify helpers that reject unknown tokens and tampered signatures.
//!
//! Wire format: JSON body + 64-byte Ed25519 signature over the canonical
//! CBOR-ish UTF-8 payload bytes (stable field order via
//! [`CapabilityClaim::signing_bytes`]).

use serde::{Deserialize, Serialize};

/// Allow-listed capability tokens a node may advertise (WEFT-147).
///
/// Free-form strings outside this list are rejected at verification.
pub const CAPABILITY_ALLOWLIST: &[&str] = &[
    "mesh",
    "code",
    "review",
    "test",
    "ipc",
    "spawn",
    "ecc",
    "hnsw",
    "causal",
    "splat",
    "world_model",
    "llm",
    "voice",
    "storage",
    "governance",
];

/// Errors from capability-claim validation / crypto.
#[non_exhaustive]
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum CapabilityClaimError {
    #[error("capability not on allow-list: {0}")]
    UnknownCapability(String),
    #[error("empty capabilities list")]
    EmptyCapabilities,
    #[error("node_id must not be empty")]
    EmptyNodeId,
    #[error("invalid public key length (want 32, got {0})")]
    BadPublicKeyLen(usize),
    #[error("invalid signature length (want 64, got {0})")]
    BadSignatureLen(usize),
    #[error("signature verification failed")]
    BadSignature,
    #[error("serde error: {0}")]
    Serde(String),
}

/// Unsigned capability claim body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityClaim {
    /// Advertising node id (DID / mesh node id string).
    pub node_id: String,
    /// Allow-listed capability tokens (sorted for stable signing).
    pub capabilities: Vec<String>,
    /// Unix epoch seconds when the claim was issued.
    pub issued_at: u64,
    /// Optional expiry (unix epoch seconds). `None` = no expiry checked here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
}

impl CapabilityClaim {
    /// Build a claim after validating the allow-list and sorting tokens.
    pub fn new(
        node_id: impl Into<String>,
        capabilities: Vec<String>,
        issued_at: u64,
    ) -> Result<Self, CapabilityClaimError> {
        let node_id = node_id.into();
        if node_id.is_empty() {
            return Err(CapabilityClaimError::EmptyNodeId);
        }
        if capabilities.is_empty() {
            return Err(CapabilityClaimError::EmptyCapabilities);
        }
        let mut caps = capabilities;
        for c in &caps {
            validate_capability(c)?;
        }
        caps.sort();
        caps.dedup();
        Ok(Self {
            node_id,
            capabilities: caps,
            issued_at,
            expires_at: None,
        })
    }

    /// Canonical bytes signed / verified (stable JSON, sorted keys via serde
    /// field order on this struct).
    pub fn signing_bytes(&self) -> Result<Vec<u8>, CapabilityClaimError> {
        serde_json::to_vec(self).map_err(|e| CapabilityClaimError::Serde(e.to_string()))
    }
}

/// Signed capability advertisement for mesh exchange.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedCapabilityAdvertisement {
    /// Claim body.
    pub claim: CapabilityClaim,
    /// Ed25519 public key (32 bytes) of the advertising node.
    pub public_key: Vec<u8>,
    /// Ed25519 signature (64 bytes) over [`CapabilityClaim::signing_bytes`].
    pub signature: Vec<u8>,
}

/// Return `Ok(())` if `cap` is on the allow-list.
pub fn validate_capability(cap: &str) -> Result<(), CapabilityClaimError> {
    if CAPABILITY_ALLOWLIST.contains(&cap) {
        Ok(())
    } else {
        Err(CapabilityClaimError::UnknownCapability(cap.to_string()))
    }
}

/// Validate every capability string in a free-form peer list.
pub fn validate_capabilities(caps: &[String]) -> Result<(), CapabilityClaimError> {
    if caps.is_empty() {
        return Err(CapabilityClaimError::EmptyCapabilities);
    }
    for c in caps {
        validate_capability(c)?;
    }
    Ok(())
}

/// Sign a claim with an Ed25519 signing key (mesh / exochain feature).
#[cfg(any(feature = "mesh", feature = "exochain"))]
pub fn sign_claim(
    claim: CapabilityClaim,
    signing_key: &ed25519_dalek::SigningKey,
) -> Result<SignedCapabilityAdvertisement, CapabilityClaimError> {
    use ed25519_dalek::Signer;
    let bytes = claim.signing_bytes()?;
    let sig = signing_key.sign(&bytes);
    Ok(SignedCapabilityAdvertisement {
        claim,
        public_key: signing_key.verifying_key().to_bytes().to_vec(),
        signature: sig.to_bytes().to_vec(),
    })
}

/// Verify signature + allow-list. Does not check `expires_at` clock skew
/// (caller may enforce).
#[cfg(any(feature = "mesh", feature = "exochain"))]
pub fn verify_signed_advertisement(
    advert: &SignedCapabilityAdvertisement,
) -> Result<(), CapabilityClaimError> {
    use ed25519_dalek::{Signature, VerifyingKey};

    if advert.public_key.len() != 32 {
        return Err(CapabilityClaimError::BadPublicKeyLen(advert.public_key.len()));
    }
    if advert.signature.len() != 64 {
        return Err(CapabilityClaimError::BadSignatureLen(advert.signature.len()));
    }
    validate_capabilities(&advert.claim.capabilities)?;

    let vk = VerifyingKey::from_bytes(
        advert
            .public_key
            .as_slice()
            .try_into()
            .map_err(|_| CapabilityClaimError::BadPublicKeyLen(advert.public_key.len()))?,
    )
    .map_err(|_| CapabilityClaimError::BadPublicKeyLen(advert.public_key.len()))?;

    let sig = Signature::from_bytes(
        advert
            .signature
            .as_slice()
            .try_into()
            .map_err(|_| CapabilityClaimError::BadSignatureLen(advert.signature.len()))?,
    );

    let bytes = advert.claim.signing_bytes()?;
    vk.verify_strict(&bytes, &sig)
        .map_err(|_| CapabilityClaimError::BadSignature)?;
    Ok(())
}

/// Apply a verified advertisement onto a [`crate::cluster::PeerNode`].
///
/// Returns `Err` without mutating when verification fails.
#[cfg(any(feature = "mesh", feature = "exochain"))]
pub fn apply_verified_capabilities(
    peer: &mut crate::cluster::PeerNode,
    advert: &SignedCapabilityAdvertisement,
) -> Result<(), CapabilityClaimError> {
    verify_signed_advertisement(advert)?;
    if peer.id != advert.claim.node_id {
        // Soft mismatch: still reject — node_id in claim must match peer.
        return Err(CapabilityClaimError::Serde(format!(
            "claim node_id {} != peer.id {}",
            advert.claim.node_id, peer.id
        )));
    }
    peer.capabilities = advert.claim.capabilities.clone();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_rejects_unknown() {
        assert!(validate_capability("code").is_ok());
        assert!(matches!(
            validate_capability("pwn-root"),
            Err(CapabilityClaimError::UnknownCapability(_))
        ));
    }

    #[test]
    fn claim_sorts_and_dedups() {
        let c = CapabilityClaim::new(
            "node-a",
            vec!["review".into(), "code".into(), "code".into()],
            1,
        )
        .unwrap();
        assert_eq!(c.capabilities, vec!["code".to_string(), "review".to_string()]);
    }

    #[test]
    fn claim_rejects_unknown_cap() {
        let err = CapabilityClaim::new("n", vec!["not-a-real-cap".into()], 0).unwrap_err();
        assert!(matches!(err, CapabilityClaimError::UnknownCapability(_)));
    }

    #[cfg(any(feature = "mesh", feature = "exochain"))]
    #[test]
    fn sign_and_verify_ok() {
        use ed25519_dalek::SigningKey;
        use rand::rngs::OsRng;
        let key = SigningKey::generate(&mut OsRng);
        let claim = CapabilityClaim::new("did:weft:node-a", vec!["mesh".into(), "ecc".into()], 100)
            .unwrap();
        let advert = sign_claim(claim, &key).unwrap();
        assert!(verify_signed_advertisement(&advert).is_ok());
    }

    #[cfg(any(feature = "mesh", feature = "exochain"))]
    #[test]
    fn tampered_claim_fails() {
        use ed25519_dalek::SigningKey;
        use rand::rngs::OsRng;
        let key = SigningKey::generate(&mut OsRng);
        let claim = CapabilityClaim::new("node-a", vec!["code".into()], 1).unwrap();
        let mut advert = sign_claim(claim, &key).unwrap();
        advert.claim.capabilities.push("review".into());
        assert_eq!(
            verify_signed_advertisement(&advert),
            Err(CapabilityClaimError::BadSignature)
        );
    }

    #[cfg(any(feature = "mesh", feature = "exochain"))]
    #[test]
    fn wrong_key_fails() {
        use ed25519_dalek::SigningKey;
        use rand::rngs::OsRng;
        let key = SigningKey::generate(&mut OsRng);
        let other = SigningKey::generate(&mut OsRng);
        let claim = CapabilityClaim::new("node-a", vec!["ipc".into()], 1).unwrap();
        let mut advert = sign_claim(claim, &key).unwrap();
        advert.public_key = other.verifying_key().to_bytes().to_vec();
        assert_eq!(
            verify_signed_advertisement(&advert),
            Err(CapabilityClaimError::BadSignature)
        );
    }

    #[cfg(any(feature = "mesh", feature = "exochain"))]
    #[test]
    fn serde_roundtrip_advert() {
        use ed25519_dalek::SigningKey;
        use rand::rngs::OsRng;
        let key = SigningKey::generate(&mut OsRng);
        let claim = CapabilityClaim::new("n1", vec!["storage".into()], 42).unwrap();
        let advert = sign_claim(claim, &key).unwrap();
        let json = serde_json::to_string(&advert).unwrap();
        let restored: SignedCapabilityAdvertisement = serde_json::from_str(&json).unwrap();
        assert!(verify_signed_advertisement(&restored).is_ok());
    }
}
