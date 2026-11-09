//! S10 key rotation: dual-signed ExoChain event + verifier (WEFT-107).
//!
//! Security-panel protocol ([`docs/weftos/k5-symposium/03-security-and-identity.md`]):
//!
//! 1. Node generates a new Ed25519 keypair.
//! 2. Node signs a `identity.key_rotation` chain event with the **old** key,
//!    embedding the new public key.
//! 3. Node signs the same canonical message with the **new** key (possession).
//! 4. Peers verify both signatures and update peer records.
//! 5. Old key is revoked after a configurable grace period.
//!
//! The dual signature lives in the chain event **payload**. Event hash
//! linking remains SHAKE-256 as for every other ExoChain entry.

use std::collections::HashMap;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::chain::{ChainEvent, ChainManager};
use crate::mesh_clock::{Clock, RealClock};

/// Chain event kind for S10 key rotation.
pub const KIND_KEY_ROTATION: &str = "identity.key_rotation";

/// Chain event source for identity / key lifecycle events.
pub const SOURCE_IDENTITY: &str = "identity";

/// Default grace period before the old key is fully revoked (24 hours).
pub const DEFAULT_GRACE_PERIOD_SECS: u64 = 86_400;

/// Domain-separated prefix for the dual-signed canonical message.
const CANONICAL_PREFIX: &str = "weftos-key-rotation:v1";

/// Errors from building or verifying a key-rotation event.
#[derive(Debug, thiserror::Error)]
pub enum KeyRotationError {
    /// Chain manager has no Ed25519 signing key configured.
    #[error("no signing key configured on chain manager")]
    NoSigningKey,
    /// Payload failed dual-signature verification.
    #[error("key rotation verification failed: {0}")]
    Verification(String),
    /// Signature bytes are not a valid Ed25519 signature encoding.
    #[error("invalid signature encoding: {0}")]
    BadSignature(String),
    /// Public key bytes are not a valid Ed25519 verifying key.
    #[error("invalid public key encoding: {0}")]
    BadPublicKey(String),
    /// Replay of an already-accepted rotation for this (old, new) pair.
    #[error("key rotation replay rejected: already applied for this key pair")]
    Replay,
    /// Old key is past its grace period and cannot sign further rotations.
    #[error("old key is past grace period and has been revoked")]
    KeyRevoked,
    /// Internal lock poison.
    #[error("lock poisoned")]
    LockPoisoned,
}

fn bytes_to_hex(data: &[u8]) -> String {
    data.iter().map(|b| format!("{b:02x}")).collect()
}

fn hex_to_bytes(s: &str) -> Result<Vec<u8>, KeyRotationError> {
    if !s.len().is_multiple_of(2) {
        return Err(KeyRotationError::BadPublicKey(
            "hex string has odd length".into(),
        ));
    }
    (0..s.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&s[i..i + 2], 16)
                .map_err(|e| KeyRotationError::BadPublicKey(e.to_string()))
        })
        .collect()
}

/// Dual-signed key-rotation payload embedded in a chain event.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KeyRotationPayload {
    /// Previous Ed25519 public key (32 bytes, hex-encoded).
    pub old_pubkey_hex: String,
    /// New Ed25519 public key (32 bytes, hex-encoded).
    pub new_pubkey_hex: String,
    /// Ed25519 signature over the canonical message by the old key (64 bytes, hex).
    pub old_signature_hex: String,
    /// Ed25519 signature over the canonical message by the new key (64 bytes, hex).
    pub new_signature_hex: String,
    /// Seconds after `rotated_at_unix` during which the old key remains
    /// accepted for verification (grace / overlap window).
    pub grace_period_secs: u64,
    /// Unix timestamp (seconds) when the rotation was produced.
    pub rotated_at_unix: u64,
}

impl KeyRotationPayload {
    /// Build the domain-separated canonical bytes both keys must sign.
    pub fn canonical_message(
        old_pubkey: &[u8; 32],
        new_pubkey: &[u8; 32],
        grace_period_secs: u64,
        rotated_at_unix: u64,
    ) -> Vec<u8> {
        format!(
            "{CANONICAL_PREFIX}\n\
             old_pubkey:{}\n\
             new_pubkey:{}\n\
             grace_period_secs:{grace_period_secs}\n\
             rotated_at:{rotated_at_unix}",
            bytes_to_hex(old_pubkey),
            bytes_to_hex(new_pubkey),
        )
        .into_bytes()
    }

    /// Decode hex pubkey to 32 raw bytes.
    pub fn decode_pubkey(hex_str: &str) -> Result<[u8; 32], KeyRotationError> {
        let bytes = hex_to_bytes(hex_str)?;
        if bytes.len() != 32 {
            return Err(KeyRotationError::BadPublicKey(format!(
                "expected 32 bytes, got {}",
                bytes.len()
            )));
        }
        let mut out = [0u8; 32];
        out.copy_from_slice(&bytes);
        Ok(out)
    }

    /// Decode hex signature to 64 raw bytes.
    pub fn decode_signature(hex_str: &str) -> Result<[u8; 64], KeyRotationError> {
        let bytes = hex_to_bytes(hex_str).map_err(|e| match e {
            KeyRotationError::BadPublicKey(m) => KeyRotationError::BadSignature(m),
            other => other,
        })?;
        if bytes.len() != 64 {
            return Err(KeyRotationError::BadSignature(format!(
                "expected 64 bytes, got {}",
                bytes.len()
            )));
        }
        let mut out = [0u8; 64];
        out.copy_from_slice(&bytes);
        Ok(out)
    }

    /// Parse payload from a chain event JSON value.
    pub fn from_event_payload(
        payload: &serde_json::Value,
    ) -> Result<Self, KeyRotationError> {
        serde_json::from_value(payload.clone()).map_err(|e| {
            KeyRotationError::Verification(format!("payload deserialize: {e}"))
        })
    }
}

/// Build a dual-signed key-rotation payload.
///
/// Signs the canonical message with both `old_key` and `new_key`.
pub fn build_key_rotation_payload(
    old_key: &SigningKey,
    new_key: &SigningKey,
    grace_period_secs: u64,
    rotated_at_unix: u64,
) -> KeyRotationPayload {
    let old_pubkey = old_key.verifying_key().to_bytes();
    let new_pubkey = new_key.verifying_key().to_bytes();
    let msg = KeyRotationPayload::canonical_message(
        &old_pubkey,
        &new_pubkey,
        grace_period_secs,
        rotated_at_unix,
    );
    let old_sig = old_key.sign(&msg);
    let new_sig = new_key.sign(&msg);
    KeyRotationPayload {
        old_pubkey_hex: bytes_to_hex(&old_pubkey),
        new_pubkey_hex: bytes_to_hex(&new_pubkey),
        old_signature_hex: bytes_to_hex(&old_sig.to_bytes()),
        new_signature_hex: bytes_to_hex(&new_sig.to_bytes()),
        grace_period_secs,
        rotated_at_unix,
    }
}

/// Verify both Ed25519 signatures on a key-rotation payload.
///
/// Rejects if either signature fails, keys are malformed, or the
/// embedded pubkeys do not match the signers of their signatures.
pub fn verify_key_rotation_payload(
    payload: &KeyRotationPayload,
) -> Result<(), KeyRotationError> {
    let old_pk = KeyRotationPayload::decode_pubkey(&payload.old_pubkey_hex)?;
    let new_pk = KeyRotationPayload::decode_pubkey(&payload.new_pubkey_hex)?;
    let old_sig_bytes = KeyRotationPayload::decode_signature(&payload.old_signature_hex)?;
    let new_sig_bytes = KeyRotationPayload::decode_signature(&payload.new_signature_hex)?;

    if old_pk == new_pk {
        return Err(KeyRotationError::Verification(
            "old and new public keys must differ".into(),
        ));
    }

    let old_vk = VerifyingKey::from_bytes(&old_pk)
        .map_err(|e| KeyRotationError::BadPublicKey(e.to_string()))?;
    let new_vk = VerifyingKey::from_bytes(&new_pk)
        .map_err(|e| KeyRotationError::BadPublicKey(e.to_string()))?;

    let msg = KeyRotationPayload::canonical_message(
        &old_pk,
        &new_pk,
        payload.grace_period_secs,
        payload.rotated_at_unix,
    );

    let old_sig = Signature::from_bytes(&old_sig_bytes);
    let new_sig = Signature::from_bytes(&new_sig_bytes);

    old_vk
        .verify_strict(&msg, &old_sig)
        .map_err(|_| KeyRotationError::Verification("old-key signature invalid".into()))?;
    new_vk
        .verify_strict(&msg, &new_sig)
        .map_err(|_| KeyRotationError::Verification("new-key signature invalid".into()))?;

    Ok(())
}

/// Verify a chain event that claims to be a key rotation.
pub fn verify_key_rotation_event(event: &ChainEvent) -> Result<KeyRotationPayload, KeyRotationError> {
    if event.kind != KIND_KEY_ROTATION {
        return Err(KeyRotationError::Verification(format!(
            "expected kind {KIND_KEY_ROTATION}, got {}",
            event.kind
        )));
    }
    let payload_val = event
        .payload
        .as_ref()
        .ok_or_else(|| KeyRotationError::Verification("missing payload".into()))?;
    let payload = KeyRotationPayload::from_event_payload(payload_val)?;
    verify_key_rotation_payload(&payload)?;
    Ok(payload)
}

/// Perform a local key rotation on a [`ChainManager`].
///
/// Generates a new Ed25519 keypair, dual-signs a rotation payload,
/// appends `identity.key_rotation` to the chain, and swaps the
/// manager's signing key to the new key.
///
/// Returns `(event, new_signing_key)`.
pub fn rotate_chain_signing_key(
    chain: &mut ChainManager,
    grace_period_secs: u64,
    clock: &dyn Clock,
) -> Result<(ChainEvent, SigningKey), KeyRotationError> {
    let old_key = chain
        .signing_key_clone()
        .ok_or(KeyRotationError::NoSigningKey)?;
    let new_key = SigningKey::generate(&mut OsRng);
    let rotated_at = clock.unix_time_us() / 1_000_000;
    let payload =
        build_key_rotation_payload(&old_key, &new_key, grace_period_secs, rotated_at);
    // Defense in depth: never append an unverifiable payload.
    verify_key_rotation_payload(&payload)?;

    let event = chain.append(
        SOURCE_IDENTITY,
        KIND_KEY_ROTATION,
        Some(serde_json::to_value(&payload).expect("KeyRotationPayload serializes")),
    );
    chain.set_signing_key(new_key.clone());
    info!(
        sequence = event.sequence,
        old = %payload.old_pubkey_hex,
        new = %payload.new_pubkey_hex,
        "S10 key rotation applied"
    );
    Ok((event, new_key))
}

/// Convenience: rotate using [`RealClock`].
pub fn rotate_chain_signing_key_now(
    chain: &mut ChainManager,
    grace_period_secs: u64,
) -> Result<(ChainEvent, SigningKey), KeyRotationError> {
    rotate_chain_signing_key(chain, grace_period_secs, &RealClock)
}

/// Tracks accepted key rotations for peer verification + grace windows.
///
/// During the grace period both the old and new pubkeys of a node are
/// trusted for signature verification. After expiry the old key is
/// revoked. Replays of the same (old, new) pair are rejected.
pub struct KeyRotationVerifier {
    /// node_id → currently trusted verifying keys (with optional expiry).
    trusted: Mutex<HashMap<String, Vec<TrustedKey>>>,
    /// Fingerprints of applied rotations (old||new hex) for replay defense.
    applied: Mutex<HashMap<String, u64>>,
    clock: std::sync::Arc<dyn Clock>,
}

#[derive(Debug, Clone)]
struct TrustedKey {
    pubkey: [u8; 32],
    /// Unix seconds after which this key is no longer trusted.
    /// `None` = no expiry (current primary key).
    expires_at_unix: Option<u64>,
}

impl KeyRotationVerifier {
    /// Create a verifier bound to the given clock (production: RealClock).
    pub fn new(clock: std::sync::Arc<dyn Clock>) -> Self {
        Self {
            trusted: Mutex::new(HashMap::new()),
            applied: Mutex::new(HashMap::new()),
            clock,
        }
    }

    /// Production default with [`RealClock`].
    pub fn with_real_clock() -> Self {
        Self::new(RealClock::shared())
    }

    /// Seed a peer's current public key (no expiry).
    pub fn trust_peer(&self, node_id: &str, pubkey: [u8; 32]) -> Result<(), KeyRotationError> {
        let mut map = self
            .trusted
            .lock()
            .map_err(|_| KeyRotationError::LockPoisoned)?;
        map.insert(
            node_id.to_string(),
            vec![TrustedKey {
                pubkey,
                expires_at_unix: None,
            }],
        );
        Ok(())
    }

    /// Apply a verified key-rotation event for `node_id`.
    ///
    /// Rejects:
    /// - Invalid dual signatures
    /// - Replay of the same (old, new) pair
    /// - Rotations where the claimed old key is not currently trusted
    /// - Rotations where the old key is past its grace period
    pub fn apply_rotation(
        &self,
        node_id: &str,
        event: &ChainEvent,
    ) -> Result<KeyRotationPayload, KeyRotationError> {
        let payload = verify_key_rotation_event(event)?;
        let old_pk = KeyRotationPayload::decode_pubkey(&payload.old_pubkey_hex)?;
        let new_pk = KeyRotationPayload::decode_pubkey(&payload.new_pubkey_hex)?;

        let fingerprint = format!(
            "{}:{}",
            payload.old_pubkey_hex, payload.new_pubkey_hex
        );
        {
            let applied = self
                .applied
                .lock()
                .map_err(|_| KeyRotationError::LockPoisoned)?;
            if applied.contains_key(&fingerprint) {
                return Err(KeyRotationError::Replay);
            }
        }

        let now = self.clock.unix_time_us() / 1_000_000;
        {
            let mut map = self
                .trusted
                .lock()
                .map_err(|_| KeyRotationError::LockPoisoned)?;
            let keys = map.entry(node_id.to_string()).or_default();

            // Prune expired keys and require old_pk currently trusted.
            keys.retain(|k| match k.expires_at_unix {
                Some(exp) => now < exp,
                None => true,
            });
            let old_trusted = keys.iter().any(|k| k.pubkey == old_pk);
            if !old_trusted {
                // First rotation for this peer may seed from event if empty.
                if keys.is_empty() {
                    keys.push(TrustedKey {
                        pubkey: old_pk,
                        expires_at_unix: Some(payload.rotated_at_unix + payload.grace_period_secs),
                    });
                } else {
                    return Err(KeyRotationError::KeyRevoked);
                }
            }

            // Mark old key with grace expiry; add new key as primary.
            for k in keys.iter_mut() {
                if k.pubkey == old_pk {
                    k.expires_at_unix =
                        Some(payload.rotated_at_unix.saturating_add(payload.grace_period_secs));
                }
            }
            if !keys.iter().any(|k| k.pubkey == new_pk) {
                keys.push(TrustedKey {
                    pubkey: new_pk,
                    expires_at_unix: None,
                });
            }
        }

        self.applied
            .lock()
            .map_err(|_| KeyRotationError::LockPoisoned)?
            .insert(fingerprint, now);

        Ok(payload)
    }

    /// Whether `pubkey` is currently trusted for `node_id` (grace-aware).
    pub fn is_trusted(&self, node_id: &str, pubkey: &[u8; 32]) -> bool {
        let now = self.clock.unix_time_us() / 1_000_000;
        let Ok(map) = self.trusted.lock() else {
            return false;
        };
        map.get(node_id)
            .map(|keys| {
                keys.iter().any(|k| {
                    k.pubkey == *pubkey
                        && match k.expires_at_unix {
                            Some(exp) => now < exp,
                            None => true,
                        }
                })
            })
            .unwrap_or(false)
    }

    /// Number of applied (non-replay) rotations.
    pub fn applied_count(&self) -> usize {
        self.applied.lock().map(|m| m.len()).unwrap_or(0)
    }
}

/// Format a human-readable rotation summary for CLI output.
pub fn format_rotation_summary(
    event: &ChainEvent,
    payload: &KeyRotationPayload,
    new_node_id: &str,
) -> String {
    let ts = DateTime::from_timestamp(payload.rotated_at_unix as i64, 0)
        .unwrap_or_else(Utc::now);
    format!(
        "Key rotation recorded at chain sequence {}\n\
         Old pubkey: {}…\n\
         New pubkey: {}…\n\
         New node id: {new_node_id}\n\
         Grace period: {}s\n\
         Rotated at: {}",
        event.sequence,
        &payload.old_pubkey_hex[..16.min(payload.old_pubkey_hex.len())],
        &payload.new_pubkey_hex[..16.min(payload.new_pubkey_hex.len())],
        payload.grace_period_secs,
        ts.to_rfc3339(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mesh_clock::MockClock;
    use std::time::Duration;

    fn test_keys() -> (SigningKey, SigningKey) {
        let old = SigningKey::from_bytes(&[1u8; 32]);
        let new = SigningKey::from_bytes(&[2u8; 32]);
        (old, new)
    }

    #[test]
    fn dual_sign_happy_path() {
        let (old, new) = test_keys();
        let payload = build_key_rotation_payload(&old, &new, 3600, 1_700_000_000);
        assert!(verify_key_rotation_payload(&payload).is_ok());
    }

    #[test]
    fn rejects_missing_old_signature() {
        let (old, new) = test_keys();
        let mut payload = build_key_rotation_payload(&old, &new, 3600, 1_700_000_000);
        payload.old_signature_hex = bytes_to_hex(&[0u8; 64]);
        let err = verify_key_rotation_payload(&payload).unwrap_err();
        assert!(err.to_string().contains("old-key"));
    }

    #[test]
    fn rejects_missing_new_signature() {
        let (old, new) = test_keys();
        let mut payload = build_key_rotation_payload(&old, &new, 3600, 1_700_000_000);
        // Sign with wrong key for new slot.
        let wrong = SigningKey::from_bytes(&[9u8; 32]);
        let old_pk = old.verifying_key().to_bytes();
        let new_pk = new.verifying_key().to_bytes();
        let msg = KeyRotationPayload::canonical_message(&old_pk, &new_pk, 3600, 1_700_000_000);
        payload.new_signature_hex = bytes_to_hex(&wrong.sign(&msg).to_bytes());
        let err = verify_key_rotation_payload(&payload).unwrap_err();
        assert!(err.to_string().contains("new-key"));
    }

    #[test]
    fn rejects_only_one_of_two_signatures() {
        let (old, new) = test_keys();
        let mut payload = build_key_rotation_payload(&old, &new, 3600, 1_700_000_000);
        // Valid-length garbage sig.
        payload.new_signature_hex = bytes_to_hex(&[0xAAu8; 64]);
        assert!(verify_key_rotation_payload(&payload).is_err());
    }

    #[test]
    fn chain_rotate_appends_and_swaps_key() {
        let old = SigningKey::from_bytes(&[3u8; 32]);
        let mut cm = ChainManager::new(0, 1000).with_signing_key(old.clone());
        let old_vk = cm.verifying_key().unwrap();
        let (event, new_key) = rotate_chain_signing_key_now(&mut cm, 7200).unwrap();
        assert_eq!(event.kind, KIND_KEY_ROTATION);
        assert_eq!(event.source, SOURCE_IDENTITY);
        let payload = verify_key_rotation_event(&event).unwrap();
        assert_eq!(
            payload.old_pubkey_hex,
            bytes_to_hex(&old_vk.to_bytes())
        );
        assert_eq!(
            payload.new_pubkey_hex,
            bytes_to_hex(&new_key.verifying_key().to_bytes())
        );
        // Manager now holds the new key.
        assert_eq!(
            cm.verifying_key().unwrap().to_bytes(),
            new_key.verifying_key().to_bytes()
        );
    }

    #[test]
    fn verifier_accepts_then_rejects_replay() {
        let clock = MockClock::new();
        clock.set_unix_us(1_700_000_000_000_000);
        let verifier = KeyRotationVerifier::new(clock.clone().shared());

        let old = SigningKey::from_bytes(&[4u8; 32]);
        let mut cm = ChainManager::new(0, 1000).with_signing_key(old);
        let (event, _) = rotate_chain_signing_key(&mut cm, 3600, &clock).unwrap();

        let node = "n-test";
        verifier.apply_rotation(node, &event).unwrap();
        assert_eq!(verifier.applied_count(), 1);

        // Replay of same event must fail.
        let err = verifier.apply_rotation(node, &event).unwrap_err();
        assert!(matches!(err, KeyRotationError::Replay));
    }

    #[test]
    fn grace_period_revokes_old_key() {
        let clock = MockClock::new();
        clock.set_unix_us(1_000_000_000_000_000); // 1e9 seconds
        let verifier = KeyRotationVerifier::new(clock.clone().shared());

        let old = SigningKey::from_bytes(&[5u8; 32]);
        let old_pk = old.verifying_key().to_bytes();
        verifier.trust_peer("peer-a", old_pk).unwrap();
        assert!(verifier.is_trusted("peer-a", &old_pk));

        let mut cm = ChainManager::new(0, 1000).with_signing_key(old);
        let grace = 100u64;
        let (event, new_key) = rotate_chain_signing_key(&mut cm, grace, &clock).unwrap();
        let new_pk = new_key.verifying_key().to_bytes();
        verifier.apply_rotation("peer-a", &event).unwrap();

        // During grace both keys trusted.
        assert!(verifier.is_trusted("peer-a", &old_pk));
        assert!(verifier.is_trusted("peer-a", &new_pk));

        // Advance past grace.
        clock.advance(Duration::from_secs(grace + 1));
        assert!(!verifier.is_trusted("peer-a", &old_pk));
        assert!(verifier.is_trusted("peer-a", &new_pk));
    }

    #[test]
    fn rotate_without_key_errors() {
        let mut cm = ChainManager::new(0, 1000);
        let err = rotate_chain_signing_key_now(&mut cm, 60).unwrap_err();
        assert!(matches!(err, KeyRotationError::NoSigningKey));
    }
}
