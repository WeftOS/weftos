//! Post-quantum signing of ExoChain RVF checkpoints (ADR-129).
//!
//! An RVF chain file ends with its checkpoint segment followed by an
//! Ed25519 [`SignatureFooter`] and, when the node has an ML-DSA-65 key, a
//! second footer with `sig_algo = 1` (ML-DSA-65).
//!
//! Before ADR-129 that second footer was an HMAC placeholder. Files are
//! told apart by the checkpoint payload: a v2 checkpoint carries
//! `"sig_suite": "ed25519+ml-dsa-65/v2"`. The tag sits inside the bytes
//! the Ed25519 footer signs, so it cannot be removed or added without
//! breaking Ed25519. A v2 checkpoint must be followed by a real ML-DSA-65
//! footer; a checkpoint without the tag is legacy, and its second footer,
//! if any, is reported as [`PqStatus::LegacyPlaceholder`] and never
//! counted as post-quantum.
//!
//! The ML-DSA-65 footer signs the whole checkpoint segment as written
//! (64-byte header, payload and padding) under [`CHECKPOINT_PQ_CONTEXT`].

use std::path::{Path, PathBuf};

use ed25519_dalek::VerifyingKey;
use rvf_crypto::{decode_signature_footer, verify_segment};
use rvf_types::{SEGMENT_HEADER_SIZE, SignatureFooter};
use rvf_wire::read_segment;
use rvf_wire::writer::calculate_padded_size;
use weftos_pq_sign::{PqStatus, RejectReason, SigSuite, Verdict, mldsa65};

/// The checkpoint payload key that names the signature suite.
pub const CHECKPOINT_SIG_SUITE_KEY: &str = "sig_suite";
/// FIPS 204 context for ML-DSA-65 checkpoint footers.
pub const CHECKPOINT_PQ_CONTEXT: &[u8] = b"weftos/exochain/checkpoint/ml-dsa-65/v2";
/// FIPS 204 context for [`crate::chain::ChainManager::dual_sign`].
pub const CHAIN_DUAL_SIG_CONTEXT: &[u8] = b"weftos/exochain/dual-sig/ml-dsa-65/v2";

const SIG_ALGO_ML_DSA_65: u16 = 1;

/// Where a node keeps its ML-DSA-65 key: beside its Ed25519 key, with the
/// extension `mldsa65` (`user.key` -> `user.mldsa65`).
pub fn pq_key_path_for(ed25519_key_path: &Path) -> PathBuf {
    ed25519_key_path.with_extension("mldsa65")
}

/// Load the node's ML-DSA-65 key, generating it on first use. The boolean
/// is `true` when a key was generated.
pub fn load_or_create_pq_key(
    ed25519_key_path: &Path,
) -> Result<(mldsa65::SigningKey, bool), weftos_pq_sign::Error> {
    weftos_pq_sign::keyfile::load_or_create(&pq_key_path_for(ed25519_key_path))
}

/// Build the ML-DSA-65 footer over a checkpoint segment.
pub fn ml_dsa_checkpoint_footer(
    cp_segment: &[u8],
    key: &mldsa65::SigningKey,
) -> Result<SignatureFooter, weftos_pq_sign::Error> {
    let sig = key.sign(cp_segment, CHECKPOINT_PQ_CONTEXT)?;
    let mut signature = [0u8; SignatureFooter::MAX_SIG_LEN];
    signature[..mldsa65::SIGNATURE_LEN].copy_from_slice(sig.as_bytes());
    let len = mldsa65::SIGNATURE_LEN as u16;
    Ok(SignatureFooter {
        sig_algo: SIG_ALGO_ML_DSA_65,
        sig_length: len,
        signature,
        footer_length: SignatureFooter::compute_footer_length(len),
    })
}

/// The suite named by a checkpoint payload (CBOR map). Absent means legacy.
pub fn checkpoint_suite(cp_cbor: &[u8]) -> Result<SigSuite, String> {
    let value: serde_json::Value =
        ciborium::from_reader(cp_cbor).map_err(|e| format!("checkpoint CBOR: {e}"))?;
    match value.get(CHECKPOINT_SIG_SUITE_KEY) {
        None => Ok(SigSuite::LegacyPlaceholderV1),
        Some(v) => v
            .as_str()
            .and_then(|s| SigSuite::parse(s).ok())
            .ok_or_else(|| format!("unknown checkpoint {CHECKPOINT_SIG_SUITE_KEY}: {v}")),
    }
}

/// Where the checkpoint segment and its footers sit in a chain file.
struct Layout {
    cp_start: usize,
    cp_end: usize,
}

fn locate_checkpoint(data: &[u8]) -> Result<Layout, String> {
    let (mut offset, mut cp_start) = (0usize, None);
    while data.len().saturating_sub(offset) >= SEGMENT_HEADER_SIZE {
        let Ok((h, _)) = read_segment(&data[offset..]) else { break };
        cp_start = Some(offset);
        offset = offset
            .checked_add(calculate_padded_size(SEGMENT_HEADER_SIZE, h.payload_length as usize))
            .ok_or("segment length overflow")?;
    }
    let cp_start = cp_start.ok_or("no RVF segments in chain file")?;
    if offset >= data.len() {
        return Err("no signature footer found in RVF file".into());
    }
    Ok(Layout { cp_start, cp_end: offset })
}

/// Decode a footer at `at` and return it with the offset just past it,
/// using the size implied by `sig_length`, not the file's `footer_length`.
fn footer_at(data: &[u8], at: usize) -> Option<(SignatureFooter, usize)> {
    let f = decode_signature_footer(data.get(at..)?).ok()?;
    let end = at.checked_add(SignatureFooter::compute_footer_length(f.sig_length) as usize)?;
    (end <= data.len()).then_some((f, end))
}

/// Verify the dual signature on a chain file's bytes.
///
/// Ed25519 is mandatory. For a v2 checkpoint the ML-DSA-65 footer must be
/// present, well formed and valid under `ml_key`. For a legacy checkpoint
/// the second footer is not checked and the verdict says so.
pub fn verify_checkpoint(
    data: &[u8],
    ed_key: &VerifyingKey,
    ml_key: &mldsa65::VerifyingKey,
) -> Result<Verdict, String> {
    let Layout { cp_start, cp_end } = locate_checkpoint(data)?;
    let (cp_header, cp_payload) =
        read_segment(&data[cp_start..]).map_err(|e| format!("re-read checkpoint segment: {e}"))?;
    let Some((ed_footer, ml_at)) = footer_at(data, cp_end) else {
        return Ok(Verdict::Rejected(RejectReason::Ed25519));
    };
    if !verify_segment(&cp_header, cp_payload, &ed_footer, ed_key) {
        return Ok(Verdict::Rejected(RejectReason::Ed25519));
    }
    let cbor = crate::chain::checkpoint_cbor(cp_payload).ok_or("checkpoint is not an ExoChain segment")?;
    let second = footer_at(data, ml_at);
    match checkpoint_suite(cbor)? {
        SigSuite::LegacyPlaceholderV1 => Ok(Verdict::Valid(match second {
            Some(_) => PqStatus::LegacyPlaceholder,
            None => PqStatus::Absent,
        })),
        SigSuite::Ed25519MlDsa65V2 => {
            let Some((f, end)) = second else {
                return Ok(Verdict::Rejected(RejectReason::MlDsa65));
            };
            let well_formed = f.sig_algo == SIG_ALGO_ML_DSA_65
                && f.sig_length as usize == mldsa65::SIGNATURE_LEN
                && end == data.len();
            let ok = well_formed
                && mldsa65::Signature::from_bytes(&f.signature[..mldsa65::SIGNATURE_LEN])
                    .is_ok_and(|s| ml_key.verify(&data[cp_start..cp_end], CHECKPOINT_PQ_CONTEXT, &s).is_ok());
            Ok(if ok {
                Verdict::Valid(PqStatus::Verified)
            } else {
                Verdict::Rejected(RejectReason::MlDsa65)
            })
        }
    }
}

// Only in clawft-kernel; the wasm project guest includes this file by path.
#[cfg(all(test, feature = "exochain"))]
#[path = "chain_pq_tests.rs"]
mod tests;
