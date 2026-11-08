//! `chain.get`: one chain event in full, with the checkpoint signature that
//! covers it.
//!
//! Appended events carry no signature of their own. The chain signs a
//! *checkpoint*: a segment holding the event count and the hash of the head
//! event, signed with Ed25519 and, when the node has the key, ML-DSA-65
//! (ADR-129). Every event hash commits to its `prev_hash`, so a signed head
//! hash commits to every event before it. An event is therefore signed when
//!
//! 1. its sequence is at or below the signed head,
//! 2. the stored events from it up to the head recompute and link, and end at
//!    exactly the signed head hash, and
//! 3. both signatures verify against the chain's own public keys.
//!
//! Anything newer than the latest signed checkpoint is reported as
//! [`CoverageStatus::UnsignedPendingCheckpoint`]; coverage is never invented.

use rvf_crypto::hash::shake256_256;
use rvf_crypto::verify_segment;
use rvf_types::SEGMENT_HEADER_SIZE;
use rvf_wire::read_segment;
use rvf_wire::writer::calculate_padded_size;
use serde::{Deserialize, Serialize};
use weftos_pq_sign::{SigSuite, mldsa65};

use crate::chain::{ChainEvent, ChainManager, checkpoint_cbor};
use crate::chain_pq::{CHECKPOINT_PQ_CONTEXT, SIG_ALGO_ML_DSA_65, checkpoint_suite, footer_at};

/// Whether an event is covered by a verified checkpoint signature.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoverageStatus {
    /// A signed checkpoint covers the event and every check passed.
    Signed,
    /// No signed checkpoint covers the event yet.
    UnsignedPendingCheckpoint,
    /// A signed checkpoint covers the event but a check failed
    /// (see `reason`): a bad signature, a missing key, or an event that no
    /// longer links to the signed head.
    SignatureInvalid,
}

/// The checkpoint that covers an event, with the checks made against it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointCoverage {
    /// First sequence the checkpoint covers (always 0: it signs the head).
    pub range_start: u64,
    /// Last sequence the checkpoint covers (the signed head).
    pub range_end: u64,
    pub event_count: u64,
    /// Hash of the signed head event (hex). This is the signed root.
    pub signed_root: String,
    /// SHAKE-256 of the checkpoint segment bytes, as signed (hex).
    pub checkpoint_hash: String,
    /// `ed25519+ml-dsa-65/v2` for a real dual signature, else `legacy/v1`.
    pub sig_suite: String,
    /// Hex of the 32-byte Ed25519 public key the signature was checked against.
    pub ed25519_key_id: Option<String>,
    /// ML-DSA-65 key id the signature was checked against.
    pub ml_dsa65_key_id: Option<String>,
    /// Hex Ed25519 signature; omitted when the caller asked for `signatures: false`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ed25519_signature: Option<String>,
    /// Hex ML-DSA-65 signature; omitted likewise, or when the checkpoint has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ml_dsa65_signature: Option<String>,
    pub ed25519_valid: bool,
    /// `None` when the checkpoint has no real ML-DSA-65 half (legacy).
    pub ml_dsa65_valid: Option<bool>,
    /// The stored events from this one to the signed head recompute and link.
    pub chain_linked: bool,
}

/// Signature coverage of one event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignatureCoverage {
    pub status: CoverageStatus,
    /// Why, in plain words. `unsigned: pending checkpoint` for an event
    /// no signed checkpoint covers yet.
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<CheckpointCoverage>,
}

/// The `chain.get` result: the full event plus coverage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainEventDetail {
    pub sequence: u64,
    pub chain_id: u32,
    pub timestamp: String,
    pub prev_hash: String,
    pub hash: String,
    pub payload_hash: String,
    pub source: String,
    pub kind: String,
    /// The full stored payload, `null` when the event has none.
    pub payload: Option<serde_json::Value>,
    /// The exact JSON text `payload_hash` is computed over (`serde_json::to_vec`
    /// of the payload), so a client can recompute the hash without
    /// reproducing the daemon's key order or number formatting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_canonical: Option<String>,
    /// Hash of the kind-registry rules in force, when the event carries one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule_hash: Option<String>,
    /// The stored `hash` equals the recomputed event hash.
    pub hash_valid: bool,
    /// The stored `payload_hash` equals the recomputed payload hash.
    pub payload_hash_valid: bool,
    pub coverage: SignatureCoverage,
}

/// Summary used by `chain.verify`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignatureSummary {
    /// Every check on the latest signed checkpoint passed, over the whole
    /// range up to its head.
    pub valid: bool,
    /// The signed head sequence.
    pub signed_through: u64,
    /// `Some(true)` only for a verified real ML-DSA-65 half.
    pub pq_verified: Option<bool>,
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// What a signed checkpoint says and what it checked out as, before any
/// event-specific question is asked.
struct Parsed {
    head: u64,
    event_count: u64,
    last_hash: [u8; 32],
    suite: SigSuite,
    checkpoint_hash: [u8; 32],
    ed_sig: Vec<u8>,
    ml_sig: Option<Vec<u8>>,
    ed_valid: bool,
    ml_valid: Option<bool>,
}

fn parse_checkpoint(cm: &ChainManager, tail: &[u8]) -> Result<Parsed, String> {
    let (hdr, payload) = read_segment(tail).map_err(|e| format!("checkpoint segment: {e}"))?;
    let cp_end = calculate_padded_size(SEGMENT_HEADER_SIZE, hdr.payload_length as usize);
    let cbor = checkpoint_cbor(payload).ok_or("checkpoint is not an ExoChain segment")?;
    let body: serde_json::Value =
        ciborium::from_reader(cbor).map_err(|e| format!("checkpoint CBOR: {e}"))?;
    let event_count = body
        .get("event_count")
        .and_then(|v| v.as_u64())
        .filter(|n| *n > 0)
        .ok_or("checkpoint has no event_count")?;
    let last_hash_hex = body
        .get("last_hash")
        .and_then(|v| v.as_str())
        .ok_or("checkpoint has no last_hash")?;
    let mut last_hash = [0u8; 32];
    if last_hash_hex.len() != 64 {
        return Err("checkpoint last_hash is not 32 bytes".into());
    }
    for (i, b) in last_hash.iter_mut().enumerate() {
        *b = u8::from_str_radix(&last_hash_hex[i * 2..i * 2 + 2], 16)
            .map_err(|_| "checkpoint last_hash is not hex")?;
    }
    let suite = checkpoint_suite(cbor)?;
    let (ed_footer, ml_at) = footer_at(tail, cp_end).ok_or("checkpoint has no signature footer")?;
    let ed_valid = cm
        .verifying_key()
        .is_some_and(|k| verify_segment(&hdr, payload, &ed_footer, &k));
    let second = footer_at(tail, ml_at).map(|(f, end)| (f, end));
    let ml_sig = second
        .as_ref()
        .filter(|(f, _)| f.sig_algo == SIG_ALGO_ML_DSA_65 && f.sig_length as usize == mldsa65::SIGNATURE_LEN)
        .map(|(f, _)| f.signature[..mldsa65::SIGNATURE_LEN].to_vec());
    let ml_valid = match suite {
        // The legacy second footer is an HMAC placeholder, never ML-DSA.
        SigSuite::LegacyPlaceholderV1 => None,
        SigSuite::Ed25519MlDsa65V2 => Some(match (&ml_sig, cm.ml_dsa_verifying_key()) {
            (Some(sig), Some(vk)) => mldsa65::Signature::from_bytes(sig)
                .is_ok_and(|s| vk.verify(&tail[..cp_end], CHECKPOINT_PQ_CONTEXT, &s).is_ok()),
            _ => false,
        }),
    };
    Ok(Parsed {
        head: event_count - 1,
        event_count,
        last_hash,
        suite,
        checkpoint_hash: shake256_256(&tail[..cp_end]),
        ed_sig: ed_footer.signature[..ed_footer.sig_length as usize].to_vec(),
        ml_sig: if suite == SigSuite::Ed25519MlDsa65V2 { ml_sig } else { None },
        ed_valid,
        ml_valid,
    })
}

fn unsigned(cm: &ChainManager, head: Option<u64>) -> SignatureCoverage {
    let reason = match head {
        Some(h) => format!(
            "unsigned: pending checkpoint (latest signed checkpoint ends at sequence {h})"
        ),
        None if !cm.has_signing_key() => {
            "unsigned: this chain has no signing key, so checkpoints are not signed".into()
        }
        None => "unsigned: pending checkpoint".into(),
    };
    SignatureCoverage { status: CoverageStatus::UnsignedPendingCheckpoint, reason, checkpoint: None }
}

/// Signature coverage of `event` against the chain's latest signed checkpoint.
pub fn coverage_for(cm: &ChainManager, event: &ChainEvent, include_signatures: bool) -> SignatureCoverage {
    let Some(tail) = cm.signed_checkpoint_bytes() else {
        return unsigned(cm, None);
    };
    let p = match parse_checkpoint(cm, &tail) {
        Ok(p) => p,
        Err(e) => {
            return SignatureCoverage {
                status: CoverageStatus::SignatureInvalid,
                reason: format!("signature invalid: unreadable checkpoint: {e}"),
                checkpoint: None,
            };
        }
    };
    if event.sequence > p.head {
        return unsigned(cm, Some(p.head));
    }
    let link = cm.verify_link_to(event.sequence, p.head, &p.last_hash);
    let sigs_ok = p.ed_valid && p.ml_valid.unwrap_or(true);
    let (status, reason) = match (&link, sigs_ok) {
        (Ok(()), true) => (CoverageStatus::Signed, "signed: covered by a valid checkpoint signature".to_string()),
        (Err(e), _) => (
            CoverageStatus::SignatureInvalid,
            format!("signature invalid: stored events no longer match the signed head ({e})"),
        ),
        (Ok(()), false) if !p.ed_valid => (
            CoverageStatus::SignatureInvalid,
            "signature invalid: Ed25519 checkpoint signature does not verify".to_string(),
        ),
        (Ok(()), false) => (
            CoverageStatus::SignatureInvalid,
            "signature invalid: ML-DSA-65 checkpoint signature does not verify".to_string(),
        ),
    };
    let checkpoint = CheckpointCoverage {
        range_start: 0,
        range_end: p.head,
        event_count: p.event_count,
        signed_root: hex(&p.last_hash),
        checkpoint_hash: hex(&p.checkpoint_hash),
        sig_suite: p.suite.as_str().to_string(),
        ed25519_key_id: cm.verifying_key().map(|k| hex(k.as_bytes())),
        ml_dsa65_key_id: cm.ml_dsa_verifying_key().map(|k| k.key_id()),
        ed25519_signature: include_signatures.then(|| hex(&p.ed_sig)),
        ml_dsa65_signature: include_signatures.then(|| p.ml_sig.as_deref().map(hex)).flatten(),
        ed25519_valid: p.ed_valid,
        ml_dsa65_valid: p.ml_valid,
        chain_linked: link.is_ok(),
    };
    SignatureCoverage { status, reason, checkpoint: Some(checkpoint) }
}

/// `chain.get`: the event at `sequence` in full, or `None` when absent.
pub fn event_detail(cm: &ChainManager, sequence: u64, include_signatures: bool) -> Option<ChainEventDetail> {
    let e = cm.event(sequence)?;
    let payload_hash_valid = e.payload_hash == crate::chain::compute_payload_hash(&e.payload);
    let hash_valid = e.hash
        == crate::chain::compute_event_hash(
            e.sequence,
            e.chain_id,
            &e.prev_hash,
            &e.source,
            &e.kind,
            &e.timestamp,
            &e.payload_hash,
            e.rule_hash.as_ref(),
        );
    let coverage = coverage_for(cm, &e, include_signatures);
    Some(ChainEventDetail {
        sequence: e.sequence,
        chain_id: e.chain_id,
        timestamp: e.timestamp.to_rfc3339(),
        prev_hash: hex(&e.prev_hash),
        hash: hex(&e.hash),
        payload_hash: hex(&e.payload_hash),
        source: e.source,
        kind: e.kind,
        payload_canonical: e
            .payload
            .as_ref()
            .and_then(|p| serde_json::to_vec(p).ok())
            .and_then(|b| String::from_utf8(b).ok()),
        payload: e.payload,
        rule_hash: e.rule_hash.as_ref().map(|h| hex(h)),
        hash_valid,
        payload_hash_valid,
        coverage,
    })
}

/// For `chain.verify`: did the latest signed checkpoint verify, over the
/// whole chain up to its head? `None` when no signed checkpoint exists.
pub fn signature_summary(cm: &ChainManager) -> Option<SignatureSummary> {
    let tail = cm.signed_checkpoint_bytes()?;
    let Ok(p) = parse_checkpoint(cm, &tail) else {
        return Some(SignatureSummary { valid: false, signed_through: 0, pq_verified: None });
    };
    let linked = cm.verify_link_to(0, p.head, &p.last_hash).is_ok();
    Some(SignatureSummary {
        valid: linked && p.ed_valid && p.ml_valid.unwrap_or(true),
        signed_through: p.head,
        pq_verified: p.ml_valid,
    })
}

#[cfg(test)]
#[path = "chain_coverage_tests.rs"]
mod tests;
