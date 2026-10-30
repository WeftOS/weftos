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
//!
//! # Downgrade rule
//!
//! A chain file holds one checkpoint, rewritten on every save, so a
//! downgrade is a whole file re-signed with Ed25519 alone. The first
//! dual-signed save appends a hash-linked `chain`/`pq.cutover` event
//! ([`PQ_CUTOVER_KIND`]). From then on every checkpoint over that history
//! must be v2: a legacy or unsigned checkpoint over a chain that contains
//! the cutover is [`RejectReason::Downgrade`] (in [`verify_checkpoint`]) or
//! a load error (in `load_from_rvf`), and `save_to_rvf` refuses to write
//! one. A node also keeps a cutover record beside its chain file
//! ([`cutover_record_path_for`]); once that exists, a file that lacks the
//! recorded cutover event (a history rewritten from before the cutover) is
//! refused. Before the cutover, legacy checkpoints verify and are flagged.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

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

/// Source of the cutover event appended by the first dual-signed save.
pub const PQ_CUTOVER_SOURCE: &str = "chain";
/// Kind of the cutover event appended by the first dual-signed save.
pub const PQ_CUTOVER_KIND: &str = "pq.cutover";

/// Where in a chain the cutover to dual signing happened.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cutover {
    /// Chain the cutover event belongs to.
    pub chain_id: u32,
    /// Sequence number of the cutover event.
    pub sequence: u64,
    /// Hex SHAKE-256 hash of the cutover event.
    pub hash: String,
}

/// Whether an event is the ADR-129 cutover marker.
pub fn is_cutover_event(source: &str, kind: &str) -> bool {
    source == PQ_CUTOVER_SOURCE && kind == PQ_CUTOVER_KIND
}

/// The first cutover event in a chain file's event segments, if any.
pub fn cutover_in_file(data: &[u8]) -> Result<Option<Cutover>, String> {
    let mut offset = 0usize;
    while data.len().saturating_sub(offset) >= SEGMENT_HEADER_SIZE {
        let Ok((h, payload)) = read_segment(&data[offset..]) else { break };
        if let Some(c) = crate::chain::cutover_from_segment(payload)? {
            return Ok(Some(c));
        }
        offset = offset
            .checked_add(calculate_padded_size(SEGMENT_HEADER_SIZE, h.payload_length as usize))
            .ok_or("segment length overflow")?;
    }
    Ok(None)
}

/// The node's cutover record for a chain file: same directory and stem,
/// extension `pq-cutover` (`chain.rvf` -> `chain.pq-cutover`).
pub fn cutover_record_path_for(rvf_path: &Path) -> PathBuf {
    rvf_path.with_extension("pq-cutover")
}

/// Read the cutover record for `rvf_path`. A record that exists but cannot
/// be parsed is an error, never "no record".
pub fn read_cutover_record(rvf_path: &Path) -> Result<Option<Cutover>, String> {
    let p = cutover_record_path_for(rvf_path);
    match std::fs::read(&p) {
        Ok(b) => serde_json::from_slice(&b)
            .map(Some)
            .map_err(|e| format!("cutover record {}: {e}", p.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("cutover record {}: {e}", p.display())),
    }
}

/// Write (or replace) the cutover record for `rvf_path` atomically.
pub fn write_cutover_record(rvf_path: &Path, c: &Cutover) -> std::io::Result<()> {
    let p = cutover_record_path_for(rvf_path);
    let tmp = p.with_extension("pq-cutover-next");
    let body = serde_json::to_vec(c).map_err(std::io::Error::other)?;
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&body)?;
        f.sync_all()?;
    }
    std::fs::rename(tmp, p)
}

/// Enforce the downgrade rule on a chain file's bytes: a file whose events
/// contain the cutover must end in a v2, dual-signed checkpoint
/// (`v2_dual_signed`), and when the node holds a cutover record for this
/// path the file must contain exactly that cutover event.
pub fn check_cutover(rvf_path: Option<&Path>, data: &[u8], v2_dual_signed: bool) -> Result<(), String> {
    let found = cutover_in_file(data)?;
    if let Some(c) = &found
        && !v2_dual_signed
    {
        return Err(format!(
            "downgrade refused: chain {} cut over to ML-DSA-65 at sequence {}, but its checkpoint is not dual-signed (ADR-129)",
            c.chain_id, c.sequence
        ));
    }
    if let Some(p) = rvf_path
        && let Some(rec) = read_cutover_record(p)?
        && found.as_ref() != Some(&rec)
    {
        return Err(format!(
            "downgrade refused: this node recorded the ML-DSA-65 cutover of chain {} at sequence {}, and {} does not contain it (ADR-129)",
            rec.chain_id,
            rec.sequence,
            p.display()
        ));
    }
    Ok(())
}

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
/// the second footer is not checked and the verdict says so, unless the
/// chain already contains the cutover event: then it is a downgrade.
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
        SigSuite::LegacyPlaceholderV1 if cutover_in_file(&data[..cp_start])?.is_some() => {
            Ok(Verdict::Rejected(RejectReason::Downgrade))
        }
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

#[cfg(all(test, feature = "exochain"))]
#[path = "chain_pq_downgrade_tests.rs"]
mod downgrade_tests;
