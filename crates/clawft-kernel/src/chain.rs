//! Local exochain manager for kernel event logging.
//!
//! Provides an append-only event chain with SHAKE-256 hash linking
//! (via [`weftos_rvf_crypto`]). Each event references the hash of the
//! previous event *and* a content hash of its payload, forming
//! an immutable, tamper-evident audit trail suitable for cross-service
//! and cross-node verification.
//!
//! ## Hash scheme
//!
//! Every event carries three hashes:
//! - **`prev_hash`** — SHAKE-256 of the preceding event (chain link)
//! - **`payload_hash`** — SHAKE-256 of the canonical JSON payload bytes
//!   (content commitment; zeroed when payload is `None`)
//! - **`hash`** — SHAKE-256 of `(sequence ‖ chain_id ‖ prev_hash ‖
//!   source ‖ 0x00 ‖ kind ‖ 0x00 ‖ timestamp ‖ payload_hash)`
//!
//! Together these enable *two-way verification*: given an event you
//! can verify the chain link backward *and* the payload content
//! independently.
//!
//! # K0 Scope
//! Local chain only: genesis, append, checkpoint.
//!
//! # K1+ Scope (not implemented)
//! Global root chain, BridgeEvent anchoring, ruvector-raft consensus.

use std::path::Path;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use ed25519_dalek::{SigningKey, VerifyingKey};
use rvf_types::SEGMENT_HEADER_SIZE;
use weftos_rvf_crypto::hash::shake256_256;
use weftos_rvf_crypto::{
    MlDsa65Key, MlDsa65VerifyKey, WitnessEntry, create_witness_chain, decode_signature_footer,
    encode_signature_footer, lineage_record_to_bytes, lineage_witness_entry, sign_segment,
    sign_segment_ml_dsa, verify_segment, verify_segment_ml_dsa, verify_witness_chain,
};
use weftos_rvf_wire::writer::{calculate_padded_size, write_segment};
use weftos_rvf_wire::{read_segment, validate_segment};

// ── ExoChain-specific RVF types ─────────────────────────────────────
//
// These were previously in rvf-types/rvf-wire but were removed upstream.
// They are exochain-specific protocol types that belong with this module.

/// Magic number for ExoChain headers inside RVF segment payloads.
const EXOCHAIN_MAGIC: u32 = 0x4558_4F43; // "EXOC"

/// 64-byte header embedded in the payload of an RVF segment for exochain events.
///
/// Layout (all fields little-endian):
///   [0..4]   magic: u32        EXOCHAIN_MAGIC
///   [4]      version: u8       protocol version (1)
///   [5]      subtype: u8       0x40=Event, 0x41=Checkpoint, 0x42=Proof
///   [6..8]   flags: u16        reserved
///   [8..12]  chain_id: u32     chain identifier
///   [12..16] _reserved: u32    must be 0
///   [16..24] sequence: u64     event sequence number
///   [24..32] timestamp_secs: u64  unix timestamp
///   [32..64] prev_hash: [u8;32]  hash of previous event
#[derive(Debug, Clone)]
struct ExoChainHeader {
    magic: u32,
    version: u8,
    subtype: u8,
    flags: u16,
    chain_id: u32,
    _reserved: u32,
    sequence: u64,
    timestamp_secs: u64,
    prev_hash: [u8; 32],
}

const EXOCHAIN_HEADER_SIZE: usize = 64;

impl ExoChainHeader {
    fn to_bytes(&self) -> [u8; EXOCHAIN_HEADER_SIZE] {
        let mut buf = [0u8; EXOCHAIN_HEADER_SIZE];
        buf[0..4].copy_from_slice(&self.magic.to_le_bytes());
        buf[4] = self.version;
        buf[5] = self.subtype;
        buf[6..8].copy_from_slice(&self.flags.to_le_bytes());
        buf[8..12].copy_from_slice(&self.chain_id.to_le_bytes());
        buf[12..16].copy_from_slice(&self._reserved.to_le_bytes());
        buf[16..24].copy_from_slice(&self.sequence.to_le_bytes());
        buf[24..32].copy_from_slice(&self.timestamp_secs.to_le_bytes());
        buf[32..64].copy_from_slice(&self.prev_hash);
        buf
    }

    fn from_bytes(data: &[u8]) -> Option<Self> {
        if data.len() < EXOCHAIN_HEADER_SIZE {
            return None;
        }
        let magic = u32::from_le_bytes(data[0..4].try_into().ok()?);
        if magic != EXOCHAIN_MAGIC {
            return None;
        }
        Some(Self {
            magic,
            version: data[4],
            subtype: data[5],
            flags: u16::from_le_bytes(data[6..8].try_into().ok()?),
            chain_id: u32::from_le_bytes(data[8..12].try_into().ok()?),
            _reserved: u32::from_le_bytes(data[12..16].try_into().ok()?),
            sequence: u64::from_le_bytes(data[16..24].try_into().ok()?),
            timestamp_secs: u64::from_le_bytes(data[24..32].try_into().ok()?),
            prev_hash: data[32..64].try_into().ok()?,
        })
    }
}

/// Write an RVF segment containing an ExoChainHeader + CBOR payload.
fn write_exochain_event(header: &ExoChainHeader, cbor: &[u8], segment_id: u64) -> Vec<u8> {
    let exo_bytes = header.to_bytes();
    let mut payload = Vec::with_capacity(exo_bytes.len() + cbor.len());
    payload.extend_from_slice(&exo_bytes);
    payload.extend_from_slice(cbor);
    write_segment(
        0x10, // domain-specific segment type
        &payload,
        rvf_types::SegmentFlags::empty(),
        segment_id,
    )
}

/// Decode an RVF segment payload into ExoChainHeader + remaining CBOR bytes.
fn decode_exochain_payload(payload: &[u8]) -> Option<(ExoChainHeader, &[u8])> {
    let header = ExoChainHeader::from_bytes(payload)?;
    Some((header, &payload[EXOCHAIN_HEADER_SIZE..]))
}
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

/// A chain event -- one entry in the append-only log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainEvent {
    /// Sequence number (0 = genesis).
    pub sequence: u64,
    /// Chain ID (0 = local).
    pub chain_id: u32,
    /// Event timestamp.
    pub timestamp: DateTime<Utc>,
    /// SHAKE-256 hash of the previous event (zeroed for genesis).
    pub prev_hash: [u8; 32],
    /// SHAKE-256 hash of this event (covers all fields incl. payload).
    pub hash: [u8; 32],
    /// SHAKE-256 hash of the canonical payload bytes (zeroed when
    /// payload is `None`). Enables independent content verification.
    #[serde(default)]
    pub payload_hash: [u8; 32],
    /// Event source (e.g. "kernel", "service.cron", "cluster").
    pub source: String,
    /// Event kind (e.g. "boot", "service.start", "peer.join").
    pub kind: String,
    /// Optional payload (JSON).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<serde_json::Value>,
    /// Optional idempotency key for replay protection (WEFT-103).
    ///
    /// When provided to [`ChainManager::append_idempotent`], the chain
    /// rejects (returns the prior event without appending a duplicate)
    /// any new event whose key matches an entry within the last
    /// [`IDEMPOTENCY_LOOKBACK`] events. This shields the append path
    /// from replayed writes at retry boundaries (mesh peers, HTTP
    /// reties, sync-sweep loops).
    ///
    /// **Not covered by the event hash.** Idempotency keys are an
    /// out-of-band deduplication hint, not part of the immutable
    /// chain commitment. This keeps existing chains hash-stable as
    /// the field rolls out and allows different writers to retry the
    /// same logical operation without invalidating the ledger.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<[u8; 32]>,
    /// Hash of the kind-registry rules in force when this event was
    /// appended (ADR-103 A7). `None` for every pre-P2 event.
    ///
    /// **Covered by the event hash only when `Some`**, so events
    /// without it hash byte-identically to before the field existed.
    /// A binary that predates the field ignores it on read, recomputes
    /// a different hash and fails integrity verification loudly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule_hash: Option<[u8; 32]>,
}

/// Number of recent events scanned for idempotency-key collisions
/// in [`ChainManager::append_idempotent`].
///
/// 1000 events is large enough to cover bursts of retries from any
/// reasonable client TTL window without making the lookup O(n) over
/// the whole chain. Configurable in a future pass if needed.
pub const IDEMPOTENCY_LOOKBACK: usize = 1000;

/// Chain sources only the daemon's own code may append under: the user
/// chain's identity events (`user.projects`), project anchors
/// (`project.anchor`, on both the user and the project chain) and token
/// issue/revoke records (`auth.token`, folded by `TokenAuthority`). Refused
/// from the `chain.append` RPC and `chain_bridge`, and from replicated events
/// ([`ChainManager::append_signed`]): a forged one would feed the identity
/// view, fake an anchor or mint/revoke a bearer token. This is the ONE list:
/// every authority-owned source a kernel component folds belongs here (the
/// literals are pinned to the owning modules' constants by a test).
pub const RESERVED_SOURCES: &[&str] = &["user.projects", "project.anchor", "auth.token"];

/// Is `source` reserved for the daemon's own events?
pub fn is_reserved_source(source: &str) -> bool {
    RESERVED_SOURCES.contains(&source.trim())
}

/// Sources the kernel's own code appends under and that a project kernel
/// trusts when it reads its chain back (review M3): `governance` (the
/// rollback floor reads `governance.overlay.applied`), `project` (genesis)
/// and `project.supervisor`. Refused, with [`RESERVED_SOURCES`], from the two
/// caller doors (the `chain.append` RPC and `chain_bridge`). Not refused
/// from replication ([`ChainManager::append_signed`]): every chain carries
/// governance events and a peer's must sync; a project kernel replicates
/// nothing (its mesh is off).
pub const KERNEL_SOURCES: &[&str] = &["governance", "project", "project.supervisor"];

/// Event kinds a project kernel folds as authority regardless of the source
/// they carry (the overlay rollback floor and `user_pin_used` read
/// `governance.overlay.applied`). Refused from replication under any source:
/// a forged replicated one with `parent_version = u64::MAX` would brick boot
/// and reload.
pub const RESERVED_KINDS: &[&str] = &["governance.overlay.applied"];

/// Is `kind` an authority-folded kind that replication must never carry?
pub fn is_reserved_kind(kind: &str) -> bool {
    RESERVED_KINDS.contains(&kind.trim())
}

/// Domain-tag prefix of every project-key signed format
/// (`weftos-project-cert-v1`, `-anchor-v1`, `-anchor-record-v1`, forward
/// header, ...). A signer that takes caller-chosen bytes under a key it does
/// not tag (the chain's `dual_sign`, the tree manager's `sign_bytes`, the mesh
/// handshake) must never sign bytes that start with it, or its signature
/// could be replayed as one of those statements (ADR-103 A7 note).
pub const PROJECT_DOMAIN_PREFIX: &[u8] = b"weftos-project-";

/// True when an untagged signer would be signing a project-domain-tagged
/// message. Callers refuse (return `None`/`Err`) in release builds too.
pub fn signs_project_domain(data: &[u8]) -> bool {
    data.starts_with(PROJECT_DOMAIN_PREFIX)
}

/// May a caller (an RPC or a tracing emitter) NOT append under `source`?
pub fn is_caller_reserved_source(source: &str) -> bool {
    is_reserved_source(source) || KERNEL_SOURCES.contains(&source.trim())
}

/// Errors from [`ChainManager::append_signed`] (WEFT-105 / K6.4 replay).
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum AppendSignedError {
    /// Event with this sequence and hash is already on the local chain.
    #[error("event seq {sequence} already present (idempotent skip)")]
    AlreadyPresent { sequence: u64 },
    /// Same sequence, different hash — DAG tip conflict (ADR-089).
    #[error("fork at seq {sequence}: local and remote hashes diverge")]
    Fork {
        sequence: u64,
        local_hash: [u8; 32],
        remote_hash: [u8; 32],
    },
    /// Remote event skips ahead of the next expected sequence.
    #[error("gap: expected seq {expected}, got {got}")]
    Gap { expected: u64, got: u64 },
    /// The event's source is reserved for locally appended events.
    #[error("source {event_source:?} is reserved and cannot be replicated")]
    ReservedSource { event_source: String },
    /// Chain ID mismatch between local and remote event.
    #[error("chain_id mismatch: local={local}, remote={remote}")]
    ChainIdMismatch { local: u32, remote: u32 },
    /// `prev_hash` does not link to the local tip.
    #[error("prev_hash mismatch at append")]
    PrevHashMismatch {
        expected: [u8; 32],
        got: [u8; 32],
    },
    /// Stored `payload_hash` does not match a recomputation of the payload.
    #[error("payload_hash mismatch at seq {sequence}")]
    PayloadHashMismatch { sequence: u64 },
    /// Stored event hash does not match a recomputation of the commitment.
    #[error("event hash mismatch at seq {sequence}")]
    HashMismatch {
        sequence: u64,
        recomputed: [u8; 32],
        stored: [u8; 32],
    },
}

/// A checkpoint snapshot of the chain state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainCheckpoint {
    /// Chain ID.
    pub chain_id: u32,
    /// Sequence number at checkpoint.
    pub sequence: u64,
    /// Hash of the last event at checkpoint.
    pub last_hash: [u8; 32],
    /// Timestamp of the checkpoint.
    pub timestamp: DateTime<Utc>,
    /// Number of events since last checkpoint.
    pub events_since_last: u64,
}

/// Result of chain integrity verification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainVerifyResult {
    /// Whether the entire chain is valid.
    pub valid: bool,
    /// Number of events verified.
    pub event_count: usize,
    /// List of errors found (empty if valid).
    pub errors: Vec<String>,
    /// Ed25519 signature verification status.
    /// `None` = no signature present, `Some(true)` = valid, `Some(false)` = invalid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature_verified: Option<bool>,
}

/// Compute the SHAKE-256 content hash of a payload.
///
/// Returns the 32-byte SHAKE-256 hash of the canonical JSON bytes,
/// or all zeros if the payload is `None`.
pub(crate) fn compute_payload_hash(payload: &Option<serde_json::Value>) -> [u8; 32] {
    match payload {
        Some(val) => {
            let bytes = serde_json::to_vec(val).unwrap_or_default();
            shake256_256(&bytes)
        }
        None => [0u8; 32],
    }
}

/// Compute the SHAKE-256 hash for a chain event.
///
/// This is the canonical hash function used for both event creation
/// and integrity verification. The hash commits to **all** fields:
///
/// ```text
/// SHAKE-256(
///     sequence(8)  ‖  chain_id(4)  ‖  prev_hash(32)  ‖
///     source  ‖  0x00  ‖  kind  ‖  0x00  ‖
///     timestamp(8)  ‖  payload_hash(32)
///     [ ‖  0x01  ‖  rule_hash(32) ]      -- only when `rule_hash` is Some
/// )
/// ```
///
/// The null-byte separators between `source` and `kind` prevent
/// domain collisions (e.g. "foo" + "bar.baz" vs "foo.bar" + "baz").
// The positional arguments are the hash preimage fields in wire order; a params struct would only restate them.
#[allow(clippy::too_many_arguments)]
pub(crate) fn compute_event_hash(
    sequence: u64,
    chain_id: u32,
    prev_hash: &[u8; 32],
    source: &str,
    kind: &str,
    timestamp: &DateTime<Utc>,
    payload_hash: &[u8; 32],
    rule_hash: Option<&[u8; 32]>,
) -> [u8; 32] {
    let mut buf = Vec::with_capacity(160);
    buf.extend_from_slice(&sequence.to_le_bytes());
    buf.extend_from_slice(&chain_id.to_le_bytes());
    buf.extend_from_slice(prev_hash);
    buf.extend_from_slice(source.as_bytes());
    buf.push(0x00); // separator
    buf.extend_from_slice(kind.as_bytes());
    buf.push(0x00); // separator
    buf.extend_from_slice(&timestamp.timestamp().to_le_bytes());
    buf.extend_from_slice(payload_hash);
    // None vs Some non-collision (P2 D review) relies on the fixed-width
    // tail before this point: the i64-LE timestamp, whose top byte is 0x00
    // for any realistic date, then payload_hash; the 0x01 tag cannot
    // align with a None event's tail.
    if let Some(rh) = rule_hash {
        buf.push(0x01); // domain tag: rule_hash present
        buf.extend_from_slice(rh);
    }
    shake256_256(&buf)
}

/// Witness type constants for kernel events.
const WITNESS_PROVENANCE: u8 = 0x01;

// ── Well-known chain event kinds (k3:D8, k2:D8) ────────────────────
//
// These constants define canonical `kind` strings for chain events
// so that producers and consumers agree on the vocabulary.

/// Capability revocation event (k3:D8 — informational revocation).
///
/// Revocation is recorded in the chain as data; enforcement is
/// handled by the governance gate at evaluation time. This allows
/// governance rules to decide whether revoked capabilities result
/// in a hard block, a warning, or are allowed in specific contexts.
pub const EVENT_KIND_CAPABILITY_REVOKED: &str = "capability.revoked";

/// API contract registration event (k2:D8).
///
/// Emitted when a service registers or updates its API schema.
/// The payload should include `service`, `version`, `schema_hash`.
pub const EVENT_KIND_API_CONTRACT_REGISTERED: &str = "service.contract.register";

/// Tool version deployment event.
pub const EVENT_KIND_TOOL_DEPLOYED: &str = "tool.deploy";

/// Tool version revocation event.
pub const EVENT_KIND_TOOL_VERSION_REVOKED: &str = "tool.version.revoke";

/// TileZero gate permitted an action (WEFT-152).
///
/// Emitted by [`crate::gate::TileZeroGate`] when `cognitum-gate-tilezero`
/// returns `Permit`. Payload: `agent_id`, `action`, `sequence`, `witness_hash`.
pub const EVENT_KIND_GATE_PERMIT: &str = "gate.permit";

/// TileZero gate deferred an action for review (WEFT-152).
///
/// Emitted when coherence is uncertain. Payload: `agent_id`, `action`,
/// `sequence`, `witness_hash`.
pub const EVENT_KIND_GATE_DEFER: &str = "gate.defer";

/// TileZero gate denied an action (WEFT-152).
///
/// Emitted when coherence is below threshold. Payload: `agent_id`, `action`,
/// `sequence`, `witness_hash`.
pub const EVENT_KIND_GATE_DENY: &str = "gate.deny";

/// Sandbox sudo override event (k3:D12).
///
/// Emitted when a sudo override bypasses environment sandbox restrictions.
/// The payload should include `agent_id`, `tool`, `path`, `reason`.
pub const EVENT_KIND_SANDBOX_SUDO_OVERRIDE: &str = "sandbox.sudo.override";

/// Tool signed event (k3:D9).
///
/// Emitted when a tool is registered with a verified cryptographic signature.
/// The payload should include `tool_name`, `tool_hash`, `signer_id`.
pub const EVENT_KIND_TOOL_SIGNED: &str = "tool.signed";

/// Shell command execution event (k3:D10).
///
/// Emitted when a shell command is executed through the sandbox.
/// The payload should include `command`, `exit_code`, `execution_time_ms`.
pub const EVENT_KIND_SHELL_EXEC: &str = "shell.exec";

/// EML model trained event.
///
/// Emitted when an EML model completes a training cycle.
/// The payload should include `model_name`, `samples_used`, `mse_before`,
/// `mse_after`, `converged`, `param_count`.
pub const EVENT_KIND_EML_TRAINED: &str = "eml.trained";

/// EML model drift detected event.
///
/// Emitted when an EML model's prediction diverges significantly
/// from the ground truth. The payload includes `model_name`,
/// `predicted`, `actual`, `drift_pct`.
pub const EVENT_KIND_EML_DRIFT: &str = "eml.drift";

/// EML model saved event.
pub const EVENT_KIND_EML_SAVED: &str = "eml.saved";

/// EML model loaded event.
pub const EVENT_KIND_EML_LOADED: &str = "eml.loaded";

/// LeWM per-frame observational tuple attestation (WEFT-533).
///
/// Emitted by `clawft-worldmodel-service` (or a daemon bridge) for every
/// processed frame. Payload is a version-tagged SIGReg manifold reference
/// plus `(a_t, z_t, z_{t+1}, surprise)`. Kind string matches
/// `weftos_worldmodel_core::EVENT_KIND_LEWM_FRAME_ATTESTATION`.
pub const EVENT_KIND_LEWM_FRAME_ATTESTATION: &str = "lewm.frame.attestation";

/// LeWM mesh sensor topic frame index (WEFT-526).
///
/// Emitted when a signed `mesh.sensor.v1.{encoded,consensus,control}` frame
/// is published on the mesh bus. Payload includes topic, cluster, node_id,
/// timestamp, and a blake3/hex digest of the CBOR frame for ExoChain index.
pub const EVENT_KIND_MESH_SENSOR_FRAME: &str = "mesh.sensor.v1.frame";

/// LeWM SIGReg health snapshot / rollback gate (WEFT-528).
///
/// Emitted by the Welford `sigreg_health` monitor when health is sampled or
/// the auto-rollback gate fires (`score < 0.85` sustained for 30 s).
pub const EVENT_KIND_SIGREG_HEALTH: &str = "lewm.sigreg.health";

/// Auth credential registration event.
///
/// Emitted when a new credential is registered with the AuthService.
/// The payload should include `credential_name`, `credential_type`.
pub const EVENT_KIND_AUTH_CREDENTIAL_REGISTER: &str = "auth.credential.register";

/// Auth credential rotation event.
///
/// Emitted when an existing credential's value is rotated.
/// The payload should include `credential_name`.
pub const EVENT_KIND_AUTH_CREDENTIAL_ROTATE: &str = "auth.credential.rotate";

/// Auth token issuance event.
///
/// Emitted when a scoped token is issued to an agent.
/// The payload should include `token_id`, `credential_name`, `agent_id`.
pub const EVENT_KIND_AUTH_TOKEN_ISSUE: &str = "auth.token.issue";

/// Auth token revocation event.
///
/// Emitted when an active token is revoked.
/// The payload should include `token_id`.
pub const EVENT_KIND_AUTH_TOKEN_REVOKE: &str = "auth.token.revoke";

/// Auth authentication attempt event.
///
/// Emitted on every authentication attempt (success or failure).
/// The payload should include `agent_id`, `success`.
pub const EVENT_KIND_AUTH_ATTEMPT: &str = "auth.attempt";

/// Configuration set event.
///
/// Emitted when a configuration value is created or updated.
/// The payload should include `namespace`, `key`, `changed_by`.
pub const EVENT_KIND_CONFIG_SET: &str = "config.set";

/// Configuration delete event.
///
/// Emitted when a configuration value is deleted.
/// The payload should include `namespace`, `key`, `changed_by`.
pub const EVENT_KIND_CONFIG_DELETE: &str = "config.delete";

/// Secret set event.
///
/// Emitted when an encrypted secret is stored.
/// The payload should include `namespace`, `key`.
pub const EVENT_KIND_CONFIG_SECRET_SET: &str = "config.secret.set";

/// Application installed event.
pub const EVENT_KIND_APP_INSTALL: &str = "app.install";

/// Application removed event.
pub const EVENT_KIND_APP_REMOVE: &str = "app.remove";

/// Application started event.
pub const EVENT_KIND_APP_START: &str = "app.start";

/// Application stopped event.
pub const EVENT_KIND_APP_STOP: &str = "app.stop";

/// Application state transition event.
pub const EVENT_KIND_APP_TRANSITION: &str = "app.transition";

// ── Workload placement events (ADR-099 sections 4 and 7) ─────────
//
// Governance decisions on a `workload.*` action are chained under the
// action's own kind with `payload.decision` = permit | deny | defer.

/// Workload package install decision / event.
pub const EVENT_KIND_WORKLOAD_INSTALL: &str = "workload.install";
/// Workload placement decision / event.
pub const EVENT_KIND_WORKLOAD_PLACE: &str = "workload.place";
/// Workload load decision / event.
pub const EVENT_KIND_WORKLOAD_LOAD: &str = "workload.load";
/// Workload start decision / event.
pub const EVENT_KIND_WORKLOAD_START: &str = "workload.start";
/// Workload stop decision / event.
pub const EVENT_KIND_WORKLOAD_STOP: &str = "workload.stop";
/// Workload unload decision / event.
pub const EVENT_KIND_WORKLOAD_UNLOAD: &str = "workload.unload";
/// Workload migration decision / event.
pub const EVENT_KIND_WORKLOAD_MIGRATE: &str = "workload.migrate";
/// Workload refusal (node admission refusal or unknown workload action).
pub const EVENT_KIND_WORKLOAD_REFUSE: &str = "workload.refuse";
/// Package / signer key / artifact hash revocation.
pub const EVENT_KIND_WORKLOAD_REVOKE: &str = "workload.revoke";
/// A revocation lifted again (`unrevoke`), chained like the revocation.
pub const EVENT_KIND_WORKLOAD_UNREVOKE: &str = "workload.unrevoke";
/// An instance changed lifecycle state (health, restart, loss, reschedule).
pub const EVENT_KIND_WORKLOAD_LIFECYCLE: &str = "workload.lifecycle";
/// Fleet identity bound to a mesh node.
pub const EVENT_KIND_WORKLOAD_NODE_BIND: &str = "workload.node.bind";
/// Fleet identity binding withdrawn (operator-signed `unbound` record).
pub const EVENT_KIND_WORKLOAD_NODE_UNBIND: &str = "workload.node.unbind";
/// A host saved its instance journal: carries the journal's generation, so a
/// restart can refuse an older copy of the file (replay).
pub const EVENT_KIND_WORKLOAD_JOURNAL: &str = "workload.journal";

// ── Artifact transfer outcomes (ADR-099 section 6, mesh-placement-11) ──
//
// Outcomes only: individual piece transfers are never chained.

/// A completed or failed artifact fetch (`result` = verified | failed).
pub const EVENT_KIND_ARTIFACT_FETCH: &str = "artifact.fetch";
/// A piece whose hash did not match, and the peer that sent it.
pub const EVENT_KIND_ARTIFACT_PIECE_REJECTED: &str = "artifact.piece_rejected";
/// First time an artifact is served to a given peer.
pub const EVENT_KIND_ARTIFACT_SERVE: &str = "artifact.serve";

// ── Swarm distribution (ADR-099 section 6, mesh-placement-25) ──────────
//
// Governance actions on what a node holds and serves.

/// A node began seeding a verified artifact (once per artifact).
pub const EVENT_KIND_ARTIFACT_SEED: &str = "artifact.seed";
/// An artifact was evicted from the node's cache (`reason` = lru | revoked | manual).
pub const EVENT_KIND_ARTIFACT_EVICT: &str = "artifact.evict";
/// A revocation was applied to what the node holds and seeds.
pub const EVENT_KIND_ARTIFACT_REVOKE: &str = "artifact.revoke";
/// A peer was banned for serving corrupt pieces.
pub const EVENT_KIND_ARTIFACT_PEER_BAN: &str = "artifact.peer_ban";

/// An operator-forced node-facts probe ran (`cluster.facts` with `refresh`).
/// Payload: `node`, `seq`.
pub const EVENT_KIND_NODE_FACTS_REFRESH: &str = "node.facts.refresh";

/// Cron job added event.
pub const EVENT_KIND_CRON_ADD: &str = "cron.add";

/// Cron job removed event.
pub const EVENT_KIND_CRON_REMOVE: &str = "cron.remove";

/// Cron job executed (fired) event.
pub const EVENT_KIND_CRON_EXECUTE: &str = "cron.execute";

// ── Agent 6: Container / Process / WASM / Agency event kinds ──────

/// Container started event.
pub const EVENT_KIND_CONTAINER_START: &str = "container.start";

/// Container stopped event.
pub const EVENT_KIND_CONTAINER_STOP: &str = "container.stop";

/// Container configured (registered) event.
pub const EVENT_KIND_CONTAINER_CONFIGURE: &str = "container.configure";

/// Process registered in the process table.
pub const EVENT_KIND_PROCESS_REGISTER: &str = "process.register";

/// Process deregistered (removed) from the process table.
pub const EVENT_KIND_PROCESS_DEREGISTER: &str = "process.deregister";

/// Process state changed.
pub const EVENT_KIND_PROCESS_STATE: &str = "process.state";

/// WASM module executed.
pub const EVENT_KIND_WASM_EXECUTE: &str = "wasm.execute";

/// WASM filesystem: file written.
pub const EVENT_KIND_WASM_FS_WRITE: &str = "wasm.fs.write";

/// WASM filesystem: file or directory removed.
pub const EVENT_KIND_WASM_FS_REMOVE: &str = "wasm.fs.remove";

/// WASM filesystem: directory created.
pub const EVENT_KIND_WASM_FS_CREATE_DIR: &str = "wasm.fs.create_dir";

/// WASM filesystem: file copied.
pub const EVENT_KIND_WASM_FS_COPY: &str = "wasm.fs.copy";

/// WASM filesystem: file moved.
pub const EVENT_KIND_WASM_FS_MOVE: &str = "wasm.fs.move";

/// Agent hierarchy: child added.
pub const EVENT_KIND_AGENT_HIERARCHY_ADD: &str = "agent.hierarchy.add_child";

/// Agent hierarchy: child removed.
pub const EVENT_KIND_AGENT_HIERARCHY_REMOVE: &str = "agent.hierarchy.remove_child";

/// Cluster peer added event.
///
/// Emitted when a new peer node joins the cluster.
/// The payload should include `node_id`, `name`, `platform`.
pub const EVENT_KIND_CLUSTER_PEER_ADD: &str = "cluster.peer.add";

/// Cluster peer removed event.
///
/// Emitted when a peer node is removed from the cluster.
/// The payload should include `node_id`.
pub const EVENT_KIND_CLUSTER_PEER_REMOVE: &str = "cluster.peer.remove";

/// Cluster peer state changed event.
///
/// Emitted when a peer node's state transitions.
/// The payload should include `node_id`, `new_state`.
pub const EVENT_KIND_CLUSTER_PEER_STATE: &str = "cluster.peer.state";

/// Capability elevation requested event.
///
/// Emitted when an agent requests elevated capabilities.
/// The payload should include `pid`, `platform`, `reason`.
pub const EVENT_KIND_CAPABILITY_ELEVATE: &str = "capability.elevate";

/// Environment registered event.
///
/// Emitted when a new environment is registered.
/// The payload should include `id`, `name`, `class`.
pub const EVENT_KIND_ENV_REGISTER: &str = "env.register";

/// Environment switched (set active) event.
///
/// Emitted when the active environment changes.
/// The payload should include `id`.
pub const EVENT_KIND_ENV_SWITCH: &str = "env.switch";

/// Environment removed event.
///
/// Emitted when an environment is deregistered.
/// The payload should include `id`.
pub const EVENT_KIND_ENV_REMOVE: &str = "env.remove";

// ── Mesh networking event kinds (Agent 7) ──────────────────────────────

/// Mesh service resolution cached.
pub const EVENT_KIND_MESH_SERVICE_REGISTER: &str = "mesh.service.register";

/// Mesh service resolution removed / negative-cached.
pub const EVENT_KIND_MESH_SERVICE_DEREGISTER: &str = "mesh.service.deregister";

/// Mesh artifact remote provider registered.
pub const EVENT_KIND_MESH_ARTIFACT_STORE: &str = "mesh.artifact.store";

/// Mesh artifact fetch request created.
pub const EVENT_KIND_MESH_ARTIFACT_FETCH: &str = "mesh.artifact.fetch";

/// Mesh IPC envelope sent.
pub const EVENT_KIND_MESH_IPC_SEND: &str = "mesh.ipc.send";

/// Mesh peer added (via announcement / catalog update).
pub const EVENT_KIND_MESH_PEER_ADD: &str = "mesh.peer.add";

/// Mesh peer removed.
pub const EVENT_KIND_MESH_PEER_REMOVE: &str = "mesh.peer.remove";

// ── Persistence event kinds (Agent 7) ──────────────────────────────────

/// Kernel state saved to disk.
pub const EVENT_KIND_KERNEL_SAVE: &str = "kernel.save";

/// Kernel state loaded from disk.
pub const EVENT_KIND_KERNEL_LOAD: &str = "kernel.load";

// ── Reconciler event kinds (Agent 7) ────────────────────────────────────

/// Reconciler corrective action recorded.
pub const EVENT_KIND_RECONCILER_ACTION: &str = "reconciler.action";

/// Reconciler tick completed.
pub const EVENT_KIND_RECONCILER_TICK: &str = "reconciler.tick";

/// Reconciler desired state set.
pub const EVENT_KIND_RECONCILER_DESIRED_SET: &str = "reconciler.desired.set";

/// Reconciler desired state removed.
pub const EVENT_KIND_RECONCILER_DESIRED_REMOVE: &str = "reconciler.desired.remove";

/// Causal graph node added event.
///
/// Emitted when a node is inserted into the causal DAG.
/// The payload should include `node_id`, `label`.
pub const EVENT_KIND_CAUSAL_NODE_ADD: &str = "causal.node.add";

/// Causal graph node removed event.
///
/// Emitted when a node (and its incident edges) is removed.
/// The payload should include `node_id`, `label`.
pub const EVENT_KIND_CAUSAL_NODE_REMOVE: &str = "causal.node.remove";

/// Causal graph node lifecycle-state transition event (ADR-067 P1-graph / WEFT-629).
///
/// Emitted from [`CausalGraph::set_node_state`](crate::causal::CausalGraph::set_node_state)
/// whenever a node's `metadata.state` tag changes
/// (`Speculative`/`Frontier`/`Committed`/`Stale`/`Pruned`). Payload fields:
/// `node_id`, `from` (previous state or null), `to` (new state). Folding these
/// events reconstructs node-state-at-T without a full graph snapshot
/// ([`crate::causal_state_fold`]).
pub const EVENT_KIND_CAUSAL_NODE_STATE: &str = "causal.node.state";

/// Causal graph edge added event.
///
/// Emitted when a directed edge is created between two nodes.
/// The payload should include `source`, `target`, `edge_type`, `weight`.
pub const EVENT_KIND_CAUSAL_EDGE_ADD: &str = "causal.edge.add";

/// Causal graph edge removed event.
///
/// Emitted when all edges between two nodes (in one direction) are removed.
/// The payload should include `source`, `target`, `removed_count`.
pub const EVENT_KIND_CAUSAL_EDGE_REMOVE: &str = "causal.edge.remove";

/// Causal graph cleared event.
///
/// Emitted when the entire causal graph is wiped. Governance-gated.
/// The payload should include `node_count`, `edge_count` (before clear).
pub const EVENT_KIND_CAUSAL_CLEAR: &str = "causal.clear";

/// Artifact stored event.
///
/// Emitted when content is stored in the artifact store.
/// The payload should include `hash`, `size`, `content_type`.
pub const EVENT_KIND_ARTIFACT_STORE: &str = "artifact.store";

/// Artifact removed event.
///
/// Emitted when an artifact is removed from storage.
/// The payload should include `hash`.
pub const EVENT_KIND_ARTIFACT_REMOVE: &str = "artifact.remove";

// ── Agent 8: Core / Graphify / Weave ────────────────────────────────

/// Sandbox command execution event.
///
/// Emitted when a sandbox enforcer checks a command, tool, network,
/// or file operation. The payload should include `agent_id`, `action`,
/// `target`, `allowed`.
pub const EVENT_KIND_SANDBOX_EXECUTE: &str = "sandbox.execute";

/// Session created event.
///
/// Emitted when a new conversation session is created.
/// The payload should include `key`.
pub const EVENT_KIND_SESSION_APPEND: &str = "session.append";
pub const EVENT_KIND_SESSION_CREATE: &str = "session.create";

/// Session destroyed event.
///
/// Emitted when a session is deleted from disk and cache.
/// The payload should include `key`.
pub const EVENT_KIND_SESSION_DESTROY: &str = "session.destroy";

/// Workspace created event.
///
/// Emitted when a new workspace is scaffolded.
/// The payload should include `name`, `path`.
pub const EVENT_KIND_WORKSPACE_CREATE: &str = "workspace.create";

/// Workspace config updated event.
///
/// Emitted when workspace config is loaded / merged.
/// The payload should include `workspace_path`, `global_path`.
pub const EVENT_KIND_WORKSPACE_CONFIG: &str = "workspace.config";

/// Tool registered in the registry.
///
/// Emitted when a tool implementation is added to the ToolRegistry.
/// The payload should include `tool_name`.
pub const EVENT_KIND_TOOL_REGISTER: &str = "tool.register";

/// Graphify knowledge graph built.
///
/// Emitted when extraction results are merged into a KnowledgeGraph.
/// The payload should include `entity_count`, `relationship_count`,
/// `files_processed`.
pub const EVENT_KIND_GRAPHIFY_BUILD: &str = "graphify.build";

/// Graphify URL ingested.
///
/// Emitted when a URL is fetched, classified, and saved.
/// The payload should include `url`, `url_type`, `filename`.
pub const EVENT_KIND_GRAPHIFY_INGEST: &str = "graphify.ingest";

/// Graphify pipeline executed.
///
/// Emitted when the full graphify pipeline completes.
/// The payload should include `entity_count`, `relationship_count`,
/// `files_processed`, `has_analysis`.
pub const EVENT_KIND_GRAPHIFY_PIPELINE: &str = "graphify.pipeline";

/// Graphify git hook installed or uninstalled.
///
/// Emitted when post-commit / post-checkout hooks are installed
/// or uninstalled.
/// The payload should include `repo_root`, `action` (install / uninstall).
pub const EVENT_KIND_GRAPHIFY_HOOK: &str = "graphify.hook";

/// Graphify Q&A query result saved for re-extraction (WEFT-153).
///
/// Emitted when `save_query_result` writes a markdown Q&A artifact.
/// The payload should include `query_type`, `path`, `source_node_count`.
pub const EVENT_KIND_GRAPHIFY_QUERY_RESULT: &str = "graphify.query_result";

/// Tool registered with explicit metadata (WEFT-153).
///
/// Emitted by non-kernel `ToolRegistry::register_with_metadata` (MCP tools
/// whose metadata comes from JSON declarations). Payload: `tool_name`,
/// `has_metadata`.
pub const EVENT_KIND_TOOL_REGISTER_METADATA: &str = "tool.register.metadata";

/// Project initialised.
///
/// Emitted when `weaver init` scaffolds the development environment.
/// The payload should include `force`, `skills_only`, `analyze`.
pub const EVENT_KIND_PROJECT_INIT: &str = "project.init";

/// Profile created event.
///
/// Emitted when a new profile is created.
/// The payload should include `profile_id`, `name`.
pub const EVENT_KIND_PROFILE_CREATE: &str = "profile.create";

/// Profile deleted event.
///
/// Emitted when a profile is deleted (governance gated).
/// The payload should include `profile_id`.
pub const EVENT_KIND_PROFILE_DELETE: &str = "profile.delete";

/// Active profile switched event.
///
/// Emitted when the active profile changes.
/// The payload should include `profile_id`, `previous`.
pub const EVENT_KIND_PROFILE_SWITCH: &str = "profile.switch";

/// Vector inserted into a profile backend.
///
/// Emitted when a vector is inserted into the active profile's backend.
/// The payload should include `profile_id`, `vector_id`, `key`.
pub const EVENT_KIND_PROFILE_VECTOR_INSERT: &str = "profile.vector.insert";

/// BVH leaf inserted (ADR-056 / WEFT-718).
///
/// Payload: `leaf_id`, `branch`, `tag`, `identity`, `bound`, `payload_hex`.
pub const EVENT_KIND_BVH_INSERT: &str = "bvh.insert";

/// BVH leaf removed.
///
/// Payload: `leaf_id`, `branch`.
pub const EVENT_KIND_BVH_REMOVE: &str = "bvh.remove";

/// BVH branch derived (COW / clone).
///
/// Payload: `parent`, `child`, `name`, `priority_tier`.
pub const EVENT_KIND_BVH_DERIVE: &str = "bvh.derive";

/// BVH rebalance / determinism-phase seal.
///
/// Payload: `branch`, `leaf_count`, `epoch`.
pub const EVENT_KIND_BVH_REBALANCE_SEAL: &str = "bvh.rebalance_seal";

/// HNSW vector inserted event.
///
/// Emitted when an embedding is inserted into the HNSW store.
/// The payload should include `id`.
pub const EVENT_KIND_HNSW_INSERT: &str = "hnsw.insert";

/// HNSW store cleared event (governance gated -- bulk destruction).
///
/// Emitted when the HNSW store is replaced with an empty instance.
pub const EVENT_KIND_HNSW_CLEAR: &str = "hnsw.clear";

/// HNSW store saved to file event.
///
/// Emitted when the HNSW store is persisted to disk.
/// The payload should include `path`.
pub const EVENT_KIND_HNSW_SAVE: &str = "hnsw.save";

/// HNSW store loaded from file event.
///
/// Emitted when an HNSW store is loaded from disk.
/// The payload should include `path`, `entry_count`.
pub const EVENT_KIND_HNSW_LOAD: &str = "hnsw.load";

/// HNSW EML search observation event.
///
/// Emitted after each search when EML is enabled. Carries the full
/// multi-signal payload: ef_used, latency_us, recall (when measured),
/// query_norm, query_variance, store_size. Used by the 2-head ef
/// model for joint ef ↔ recall training and by ExoChain for auditable
/// training provenance.
pub const EVENT_KIND_HNSW_EML_OBSERVE: &str = "hnsw.eml.observe";

/// HNSW EML recall measurement event.
///
/// Emitted when a recall checkpoint is taken (brute-force vs HNSW).
/// Payload: avg_recall, store_size, inserts_since_rebuild, query_count.
pub const EVENT_KIND_HNSW_EML_RECALL: &str = "hnsw.eml.recall";

/// HNSW EML model trained event.
///
/// Emitted after a training cycle. Payload: train_cycles, models_trained,
/// ef_trained, rebuild_trained, distance_trained, path_trained.
pub const EVENT_KIND_HNSW_EML_TRAINED: &str = "hnsw.eml.trained";

/// HNSW EML triage decision event.
///
/// Emitted when the tree calculus strategy selector runs. Payload
/// contains the full TriageRecord: form (Atom/Sequence/Branch),
/// steepness, concentration, knee, and the chosen SearchStrategy
/// with tuned parameters. Auditable training provenance.
pub const EVENT_KIND_HNSW_EML_TRIAGE: &str = "hnsw.eml.triage";

/// Supplies the rule hash stamped on each append (ADR-103 A7).
pub type RuleHashProvider = std::sync::Arc<dyn Fn() -> Option<[u8; 32]> + Send + Sync>;

/// ExoChainHeader flag: this event segment carries a `rule_hash`.
const EXO_FLAG_HAS_RULE_HASH: u16 = 0x0001;
/// Flag bits this build understands; any other bit means the segment was
/// written by a newer binary.
const EXO_FLAGS_KNOWN: u16 = EXO_FLAG_HAS_RULE_HASH;

/// Check a segment's flags against what was decoded from its payload.
fn check_exo_flags(flags: u16, has_rule_hash: bool) -> Result<(), String> {
    if flags & !EXO_FLAGS_KNOWN != 0 {
        return Err(format!(
            "segment flags {flags:#06x} carry bits this build does not know: \
             written by a newer binary"
        ));
    }
    if (flags & EXO_FLAG_HAS_RULE_HASH != 0) != has_rule_hash {
        return Err("segment rule_hash flag disagrees with its payload".into());
    }
    Ok(())
}

/// Local chain state.
struct LocalChain {
    chain_id: u32,
    events: Vec<ChainEvent>,
    last_hash: [u8; 32],
    sequence: u64,
    checkpoint_interval: u64,
    events_since_checkpoint: u64,
    checkpoints: Vec<ChainCheckpoint>,
    /// Witness entries — one per event for cryptographic audit trail.
    witness_entries: Vec<WitnessEntry>,
    /// Stamps `rule_hash` on every append (ADR-103 A7). Called under the
    /// chain lock: it must not call back into the chain.
    rule_hash_provider: Option<RuleHashProvider>,
    /// Oldest replay a subscriber may ask for, in events behind the head.
    max_replay_window: u64,
    /// Live fan-out for [`ChainManager::subscribe`].
    hub: crate::chain_subscribe::EventHub,
}

impl LocalChain {
    fn new(chain_id: u32, checkpoint_interval: u64) -> Self {
        Self {
            chain_id,
            events: Vec::new(),
            last_hash: [0u8; 32],
            sequence: 0,
            checkpoint_interval,
            events_since_checkpoint: 0,
            checkpoints: Vec::new(),
            witness_entries: Vec::new(),
            rule_hash_provider: None,
            max_replay_window: crate::chain_subscribe::DEFAULT_MAX_REPLAY_WINDOW,
            hub: Default::default(),
        }
    }

    /// Restore from a saved set of events and optional witness entries.
    fn from_events(
        chain_id: u32,
        checkpoint_interval: u64,
        events: Vec<ChainEvent>,
        witness_entries: Vec<WitnessEntry>,
    ) -> Self {
        let (last_hash, sequence) = if let Some(last) = events.last() {
            (last.hash, last.sequence + 1)
        } else {
            ([0u8; 32], 0)
        };
        Self {
            chain_id,
            events,
            last_hash,
            sequence,
            checkpoint_interval,
            events_since_checkpoint: 0,
            checkpoints: Vec::new(),
            witness_entries,
            rule_hash_provider: None,
            max_replay_window: crate::chain_subscribe::DEFAULT_MAX_REPLAY_WINDOW,
            hub: Default::default(),
        }
    }

    /// Ask the provider for the current rule hash. Runs under the chain
    /// lock (lock order: chain first, then whatever the provider touches),
    /// so a panicking provider is contained: it must not unwind through the
    /// held mutex and poison the chain. Only effective where panics unwind:
    /// `[profile.release]` sets `panic = "abort"`, so in release a provider
    /// panic aborts the process and the provider must simply not panic.
    fn stamp_rule_hash(&self) -> Option<[u8; 32]> {
        let p = self.rule_hash_provider.as_ref()?;
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| p())) {
            Ok(h) => h,
            Err(_) => {
                warn!("rule_hash provider panicked; appending without rule_hash");
                None
            }
        }
    }

    fn append(
        &mut self,
        source: String,
        kind: String,
        payload: Option<serde_json::Value>,
    ) -> &ChainEvent {
        self.append_with_key(source, kind, payload, None)
    }

    /// Append carrying an optional idempotency key (WEFT-103).
    ///
    /// The key is *not* mixed into the SHAKE-256 event hash so that
    /// chain commitments remain stable for replicas that never see
    /// the key. Replay protection is enforced one layer up in
    /// [`ChainManager::append_idempotent`].
    fn append_with_key(
        &mut self,
        source: String,
        kind: String,
        payload: Option<serde_json::Value>,
        idempotency_key: Option<[u8; 32]>,
    ) -> &ChainEvent {
        let timestamp = Utc::now();
        let payload_hash = compute_payload_hash(&payload);
        let rule_hash = self.stamp_rule_hash();
        let hash = compute_event_hash(
            self.sequence,
            self.chain_id,
            &self.last_hash,
            &source,
            &kind,
            &timestamp,
            &payload_hash,
            rule_hash.as_ref(),
        );

        let event = ChainEvent {
            sequence: self.sequence,
            chain_id: self.chain_id,
            timestamp,
            prev_hash: self.last_hash,
            hash,
            payload_hash,
            source,
            kind,
            payload,
            idempotency_key,
            rule_hash,
        };

        // Create a witness entry for this event.
        self.witness_entries.push(WitnessEntry {
            prev_hash: [0u8; 32], // linked at serialization time
            action_hash: hash,
            timestamp_ns: timestamp.timestamp_nanos_opt().unwrap_or(0) as u64,
            witness_type: WITNESS_PROVENANCE,
        });

        self.last_hash = hash;
        self.sequence += 1;
        self.events_since_checkpoint += 1;
        self.hub.publish(&event);
        self.events.push(event);

        // Auto-checkpoint
        if self.checkpoint_interval > 0 && self.events_since_checkpoint >= self.checkpoint_interval
        {
            self.create_checkpoint();
        }

        self.events.last().unwrap()
    }

    fn create_checkpoint(&mut self) -> ChainCheckpoint {
        let cp = ChainCheckpoint {
            chain_id: self.chain_id,
            sequence: self.sequence.saturating_sub(1),
            last_hash: self.last_hash,
            timestamp: Utc::now(),
            events_since_last: self.events_since_checkpoint,
        };
        self.events_since_checkpoint = 0;
        self.checkpoints.push(cp.clone());
        cp
    }
}

/// CBOR payload structure for RVF segment persistence.
///
/// Contains the per-event fields that are not already covered by the
/// ExoChainHeader (which stores sequence, chain_id, timestamp, prev_hash).
#[derive(Serialize, Deserialize)]
struct RvfChainPayload {
    source: String,
    kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    payload: Option<serde_json::Value>,
    /// Hex-encoded 32-byte payload hash.
    payload_hash: String,
    /// Hex-encoded 32-byte event hash.
    hash: String,
    /// Hex-encoded 32-byte rule hash (ADR-103 A7); absent on pre-P2 events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rule_hash: Option<String>,
}

/// Encode a 32-byte hash as a lowercase hex string.
fn hex_hash(h: &[u8; 32]) -> String {
    h.iter().map(|b| format!("{b:02x}")).collect()
}

/// Encode arbitrary bytes as a lowercase hex string.
fn hex_encode(data: &[u8]) -> String {
    data.iter().map(|b| format!("{b:02x}")).collect()
}

/// Decode a hex string into a byte vector.
fn hex_decode(s: &str) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
    if !s.len().is_multiple_of(2) {
        return Err("hex string has odd length".into());
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    for chunk in s.as_bytes().chunks(2) {
        let hi = hex_nibble(chunk[0])?;
        let lo = hex_nibble(chunk[1])?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

/// Parse a 64-char hex string back into a 32-byte array.
fn parse_hex_hash(s: &str) -> Result<[u8; 32], Box<dyn std::error::Error + Send + Sync>> {
    if s.len() != 64 {
        return Err(format!("expected 64 hex chars, got {}", s.len()).into());
    }
    let mut out = [0u8; 32];
    for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
        let hi = hex_nibble(chunk[0])?;
        let lo = hex_nibble(chunk[1])?;
        out[i] = (hi << 4) | lo;
    }
    Ok(out)
}

/// Convert a single ASCII hex character to its nibble value.
fn hex_nibble(c: u8) -> Result<u8, Box<dyn std::error::Error + Send + Sync>> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(format!("invalid hex char: {}", c as char).into()),
    }
}

/// Thread-safe chain manager.
///
/// Wraps a local chain with mutex protection for concurrent access
/// from multiple kernel subsystems. Optionally holds an Ed25519
/// signing key for cryptographic chain signing.
pub struct ChainManager {
    inner: Mutex<LocalChain>,
    /// Ed25519 signing key for RVF segment signing.
    signing_key: Option<SigningKey>,
    /// ML-DSA-65 signing key for post-quantum dual signing.
    ml_dsa_key: Option<MlDsa65Key>,
}

impl ChainManager {
    /// Create a new chain manager with genesis event.
    pub fn new(chain_id: u32, checkpoint_interval: u64) -> Self {
        let mut chain = LocalChain::new(chain_id, checkpoint_interval);
        // Genesis event
        chain.append(
            "chain".into(),
            "genesis".into(),
            Some(serde_json::json!({ "chain_id": chain_id })),
        );
        debug!(chain_id, "local chain initialized with genesis event");

        Self {
            inner: Mutex::new(chain),
            signing_key: None,
            ml_dsa_key: None,
        }
    }

    /// Create with default settings.
    pub fn default_local() -> Self {
        Self::new(0, 1000)
    }

    /// Restore a chain manager from a verified event prefix (WEFT-105).
    ///
    /// Used when a joining node already shares a common genesis / prefix
    /// with a peer (same cluster identity). Events must be non-empty and
    /// pass integrity verification.
    pub fn from_events(
        events: Vec<ChainEvent>,
        checkpoint_interval: u64,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        if events.is_empty() {
            return Err("from_events: empty event list".into());
        }
        let chain_id = events[0].chain_id;
        let chain = LocalChain::from_events(chain_id, checkpoint_interval, events, Vec::new());
        let mgr = Self {
            inner: Mutex::new(chain),
            signing_key: None,
            ml_dsa_key: None,
        };
        let result = mgr.verify_integrity();
        if !result.valid {
            return Err(format!(
                "from_events integrity check failed: {} errors ({})",
                result.errors.len(),
                result.errors.first().cloned().unwrap_or_default()
            )
            .into());
        }
        Ok(mgr)
    }

    /// Attach an Ed25519 signing key for RVF segment signing.
    pub fn with_signing_key(mut self, key: SigningKey) -> Self {
        self.signing_key = Some(key);
        self
    }

    /// Get the verifying (public) key, if a signing key is set.
    pub fn verifying_key(&self) -> Option<VerifyingKey> {
        self.signing_key.as_ref().map(|k| k.verifying_key())
    }

    /// Set the signing key (mutable borrow — use with `Arc::get_mut()`
    /// before sharing the manager across tasks).
    pub fn set_signing_key(&mut self, key: SigningKey) {
        self.signing_key = Some(key);
    }

    /// Whether this chain manager has a signing key attached.
    pub fn has_signing_key(&self) -> bool {
        self.signing_key.is_some()
    }

    /// Clone the Ed25519 signing key (for key rotation, WEFT-107).
    pub fn signing_key_clone(&self) -> Option<SigningKey> {
        self.signing_key.clone()
    }

    /// Set the ML-DSA-65 key for post-quantum dual signing.
    pub fn set_ml_dsa_key(&mut self, key: MlDsa65Key) {
        self.ml_dsa_key = Some(key);
    }

    /// Whether dual signing (Ed25519 + ML-DSA-65) is enabled.
    pub fn has_dual_signing(&self) -> bool {
        self.signing_key.is_some() && self.ml_dsa_key.is_some()
    }

    /// Load an Ed25519 signing key from file, or generate and persist a new one.
    pub fn load_or_create_key(
        path: &Path,
    ) -> Result<SigningKey, Box<dyn std::error::Error + Send + Sync>> {
        if path.exists() {
            // Warn if key file is world-readable on Unix.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(path)?.permissions().mode();
                if mode & 0o077 != 0 {
                    warn!(
                        path = %path.display(),
                        mode = format!("{mode:04o}"),
                        "signing key file has overly permissive permissions, fixing to 0600"
                    );
                    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
                }
            }
            let bytes = std::fs::read(path)?;
            if bytes.len() != 32 {
                return Err(format!("key file is {} bytes, expected 32", bytes.len()).into());
            }
            let key_bytes: [u8; 32] = bytes.try_into().map_err(|_| "key file not 32 bytes")?;
            let key = SigningKey::from_bytes(&key_bytes);
            info!(path = %path.display(), "loaded Ed25519 signing key");
            Ok(key)
        } else {
            use rand::rngs::OsRng;
            let key = SigningKey::generate(&mut OsRng);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, key.to_bytes())?;
            // Restrict key file to owner-only (0600) on Unix to prevent
            // other users from reading the private signing key.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
            }
            info!(path = %path.display(), "generated new Ed25519 signing key");
            Ok(key)
        }
    }

    /// Append an event to the chain.
    pub fn append(
        &self,
        source: &str,
        kind: &str,
        payload: Option<serde_json::Value>,
    ) -> ChainEvent {
        let mut chain = self.inner.lock().unwrap();
        chain.append(source.into(), kind.into(), payload).clone()
    }

    /// Append an event with an optional idempotency key (WEFT-103).
    ///
    /// When `idempotency_key` is `Some`, the chain is scanned for the
    /// most recent [`IDEMPOTENCY_LOOKBACK`] events. If an event with
    /// the same key is found, **no append occurs** and the prior
    /// event is returned. This protects the chain against:
    ///
    /// - Retried mesh writes that crossed the network twice.
    /// - HTTP-handler retries on connection drops.
    /// - Sync-sweep loops that re-emit the same logical operation.
    ///
    /// When `idempotency_key` is `None`, behaves exactly like
    /// [`Self::append`].
    ///
    /// Concurrency: the lookback scan and the append happen under
    /// the same inner mutex, so concurrent callers cannot race past
    /// each other to insert duplicates.
    pub fn append_idempotent(
        &self,
        source: &str,
        kind: &str,
        payload: Option<serde_json::Value>,
        idempotency_key: Option<[u8; 32]>,
    ) -> ChainEvent {
        let mut chain = self.inner.lock().unwrap();

        if let Some(ref key) = idempotency_key {
            let len = chain.events.len();
            let start = len.saturating_sub(IDEMPOTENCY_LOOKBACK);
            // Walk newest-first so a hot-cache hit short-circuits early.
            for existing in chain.events[start..].iter().rev() {
                if existing.idempotency_key.as_ref() == Some(key) {
                    debug!(
                        sequence = existing.sequence,
                        kind = existing.kind,
                        "idempotency_key match -- skipping duplicate append"
                    );
                    return existing.clone();
                }
            }
        }

        chain
            .append_with_key(source.into(), kind.into(), payload, idempotency_key)
            .clone()
    }

    /// Create a checkpoint.
    pub fn checkpoint(&self) -> ChainCheckpoint {
        let mut chain = self.inner.lock().unwrap();
        chain.create_checkpoint()
    }

    /// Get the current chain length.
    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().events.len()
    }

    /// Check if the chain is empty (should never be after genesis).
    pub fn is_empty(&self) -> bool {
        self.inner.lock().unwrap().events.is_empty()
    }

    /// Get the current sequence number.
    pub fn sequence(&self) -> u64 {
        self.inner.lock().unwrap().sequence
    }

    /// Get the last hash.
    pub fn last_hash(&self) -> [u8; 32] {
        self.inner.lock().unwrap().last_hash
    }

    /// Get the chain ID.
    pub fn chain_id(&self) -> u32 {
        self.inner.lock().unwrap().chain_id
    }

    /// Get recent events (last n, or all if n=0).
    pub fn tail(&self, n: usize) -> Vec<ChainEvent> {
        let chain = self.inner.lock().unwrap();
        if n == 0 || n >= chain.events.len() {
            chain.events.clone()
        } else {
            chain.events[chain.events.len() - n..].to_vec()
        }
    }

    /// Install the provider that stamps `rule_hash` on every append
    /// (ADR-103 A7).
    ///
    /// **Lock order: the chain lock is taken first, then the provider
    /// runs.** The provider must therefore never block and never call back
    /// into this manager: read lock-free state (an atomic, see
    /// [`crate::chain_rule_hash::RuleHashCell`]) or use `try_read`.
    /// A provider that takes a lock another thread holds while appending
    /// would deadlock. In dev/test builds (panic = unwind) a panicking
    /// provider is caught: the event is appended without `rule_hash` and a
    /// warning is logged. Release builds use `panic = "abort"`, so there a
    /// provider panic aborts the process; it must not panic.
    pub fn set_rule_hash_provider(&self, provider: RuleHashProvider) {
        self.inner.lock().unwrap().rule_hash_provider = Some(provider);
    }

    /// Bound how far back [`Self::subscribe`] may replay, in events.
    pub fn set_max_replay_window(&self, events: u64) {
        self.inner.lock().unwrap().max_replay_window = events;
    }

    /// Subscribe to chain events: optional replay from `filter.from_seq`,
    /// then live events, with no gap and no duplicate between the two.
    /// See [`crate::chain_subscribe`].
    ///
    /// Under the chain lock this only subscribes to the live feed and
    /// records the snapshot end; the replay is paged later in short lock
    /// holds by the subscription, so a deep `from_seq` cannot stall
    /// appends or pin memory.
    ///
    /// # Errors
    /// [`SubscribeError::ReplayWindowExceeded`] when `from_seq` is older
    /// than the configured replay window.
    #[cfg(feature = "native")]
    pub fn subscribe(
        self: &Arc<Self>,
        filter: crate::chain_subscribe::ChainFilter,
    ) -> Result<crate::chain_subscribe::ChainSubscription, crate::chain_subscribe::SubscribeError>
    {
        let chain = self.inner.lock().unwrap();
        if let Some(from) = filter.from_seq {
            let oldest_allowed = chain.sequence.saturating_sub(chain.max_replay_window);
            if from < oldest_allowed {
                return Err(crate::chain_subscribe::SubscribeError::ReplayWindowExceeded {
                    requested: from,
                    oldest_allowed,
                });
            }
        }
        let rx = chain.hub.subscribe();
        Ok(crate::chain_subscribe::ChainSubscription::new(
            filter,
            Arc::downgrade(self),
            rx,
            chain.sequence,
        ))
    }

    /// One replay page: up to `limit` events matching `prefix` with
    /// `sequence` in `[from, end)`, scanning at most `scan` retained
    /// events. Returns the page and the sequence to resume from.
    pub(crate) fn replay_page(
        &self,
        from: u64,
        end: u64,
        prefix: Option<&str>,
        limit: usize,
        scan: usize,
    ) -> (Vec<ChainEvent>, u64) {
        let chain = self.inner.lock().unwrap();
        let start = chain.events.partition_point(|e| e.sequence < from);
        let mut out = Vec::new();
        let mut next = from;
        for (scanned, e) in chain.events[start..].iter().enumerate() {
            if e.sequence >= end {
                return (out, end);
            }
            if scanned >= scan || out.len() >= limit {
                return (out, next);
            }
            next = e.sequence + 1;
            if prefix.is_none_or(|p| e.kind.starts_with(p)) {
                out.push(e.clone());
            }
        }
        (out, end)
    }

    /// Return events with sequence strictly greater than `after`.
    /// Used for incremental replication in K6.4 chain sync.
    pub fn tail_from(&self, after: u64) -> Vec<ChainEvent> {
        let chain = self.inner.lock().unwrap();
        chain
            .events
            .iter()
            .filter(|e| e.sequence > after)
            .cloned()
            .collect()
    }

    /// Append a remote (pre-built) chain event after verifying hash linkage
    /// (WEFT-105 / K6.4 chain replay).
    ///
    /// Unlike [`Self::append`], this does **not** re-hash or re-timestamp the
    /// event — the remote node's event is accepted as-is when:
    ///
    /// 1. `event.chain_id` matches the local chain
    /// 2. `event.sequence` equals the next expected sequence
    /// 3. `event.prev_hash` equals the local tip hash
    /// 4. `event.payload_hash` matches a recomputation of the payload
    /// 5. `event.hash` matches a recomputation of the event commitment
    ///
    /// Idempotent: if an event with the same sequence **and** hash already
    /// exists, returns [`AppendSignedError::AlreadyPresent`].
    ///
    /// Fork policy (ADR-089): same sequence with a different hash returns
    /// [`AppendSignedError::Fork`] — tips are retained by rejecting silent
    /// overwrite; higher-level merge commits are out of scope for linear
    /// catch-up replay.
    pub fn append_signed(&self, event: ChainEvent) -> Result<ChainEvent, AppendSignedError> {
        // First, before any sequence/fork bookkeeping, so a forged authority
        // event is refused as such whatever the local chain looks like.
        if is_reserved_source(&event.source) || is_reserved_kind(&event.kind) {
            return Err(AppendSignedError::ReservedSource { event_source: event.source.clone() });
        }
        let mut chain = self.inner.lock().unwrap();

        // Idempotent: already have this exact event.
        if let Some(existing) = chain.events.iter().find(|e| e.sequence == event.sequence) {
            if existing.hash == event.hash {
                return Err(AppendSignedError::AlreadyPresent {
                    sequence: event.sequence,
                });
            }
            return Err(AppendSignedError::Fork {
                sequence: event.sequence,
                local_hash: existing.hash,
                remote_hash: event.hash,
            });
        }

        if event.chain_id != chain.chain_id {
            return Err(AppendSignedError::ChainIdMismatch {
                local: chain.chain_id,
                remote: event.chain_id,
            });
        }

        let expected_seq = chain.sequence;
        if event.sequence < expected_seq {
            // Behind tip and not an exact match (handled above) → fork or gap
            // in history we already advanced past.
            return Err(AppendSignedError::Fork {
                sequence: event.sequence,
                local_hash: chain
                    .events
                    .iter()
                    .find(|e| e.sequence == event.sequence)
                    .map(|e| e.hash)
                    .unwrap_or([0u8; 32]),
                remote_hash: event.hash,
            });
        }
        if event.sequence > expected_seq {
            return Err(AppendSignedError::Gap {
                expected: expected_seq,
                got: event.sequence,
            });
        }

        if event.prev_hash != chain.last_hash {
            return Err(AppendSignedError::PrevHashMismatch {
                expected: chain.last_hash,
                got: event.prev_hash,
            });
        }

        let recomputed_payload = compute_payload_hash(&event.payload);
        if event.payload_hash != recomputed_payload {
            return Err(AppendSignedError::PayloadHashMismatch {
                sequence: event.sequence,
            });
        }

        let recomputed = compute_event_hash(
            event.sequence,
            event.chain_id,
            &event.prev_hash,
            &event.source,
            &event.kind,
            &event.timestamp,
            &event.payload_hash,
            event.rule_hash.as_ref(),
        );
        if event.hash != recomputed {
            return Err(AppendSignedError::HashMismatch {
                sequence: event.sequence,
                recomputed,
                stored: event.hash,
            });
        }

        // Accept remote event as-is.
        chain.witness_entries.push(WitnessEntry {
            prev_hash: [0u8; 32], // linked at serialization time
            action_hash: event.hash,
            timestamp_ns: event.timestamp.timestamp_nanos_opt().unwrap_or(0) as u64,
            witness_type: WITNESS_PROVENANCE,
        });
        chain.last_hash = event.hash;
        chain.sequence = event.sequence.saturating_add(1);
        chain.events_since_checkpoint = chain.events_since_checkpoint.saturating_add(1);
        chain.hub.publish(&event);
        chain.events.push(event);
        if chain.checkpoint_interval > 0
            && chain.events_since_checkpoint >= chain.checkpoint_interval
        {
            let _ = chain.create_checkpoint();
        }

        Ok(chain.events.last().cloned().expect("just pushed"))
    }

    /// Get the current head sequence number (sequence of the last event).
    /// Returns 0 when the chain is empty.
    pub fn head_sequence(&self) -> u64 {
        let chain = self.inner.lock().unwrap();
        chain.events.last().map(|e| e.sequence).unwrap_or(0)
    }

    /// Get the current head hash (hash of the last event).
    /// Returns the all-zero hash when the chain is empty.
    pub fn head_hash(&self) -> [u8; 32] {
        let chain = self.inner.lock().unwrap();
        chain.events.last().map(|e| e.hash).unwrap_or([0u8; 32])
    }

    /// Get all checkpoints.
    pub fn checkpoints(&self) -> Vec<ChainCheckpoint> {
        self.inner.lock().unwrap().checkpoints.clone()
    }

    /// Get the number of witness entries.
    pub fn witness_count(&self) -> usize {
        self.inner.lock().unwrap().witness_entries.len()
    }

    /// Serialize the witness chain and verify it.
    ///
    /// Returns `Ok(entry_count)` if the witness chain is valid, or
    /// `Err` if verification fails.
    pub fn verify_witness(&self) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
        let chain = self.inner.lock().unwrap();
        if chain.witness_entries.is_empty() {
            return Ok(0);
        }
        let data = create_witness_chain(&chain.witness_entries);
        let verified = verify_witness_chain(&data)
            .map_err(|e| format!("witness chain verification failed: {e}"))?;
        Ok(verified.len())
    }

    /// Generate a custody attestation document.
    ///
    /// Assembles the current chain state, vector count and content hash,
    /// and signs the result with the kernel's Ed25519 key.
    ///
    /// Returns `None` if no signing key is configured.
    pub fn generate_attestation(
        &self,
        vector_count: u64,
        epoch: u64,
        content_hash: &str,
    ) -> Option<CustodyAttestation> {
        use ed25519_dalek::Signer;

        let signing_key = self.signing_key.as_ref()?;
        let device_id: String = signing_key
            .verifying_key()
            .to_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();

        let chain = self.inner.lock().unwrap();
        let chain_head: String = chain.last_hash.iter().map(|b| format!("{b:02x}")).collect();
        let chain_depth = chain.events.len() as u64;
        drop(chain);

        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        // Canonical payload for signing: deterministic field order
        let canonical = format!(
            "custody-attestation:v1\n\
             device_id:{device_id}\n\
             epoch:{epoch}\n\
             chain_head:{chain_head}\n\
             chain_depth:{chain_depth}\n\
             vector_count:{vector_count}\n\
             content_hash:{content_hash}\n\
             timestamp:{timestamp}"
        );

        let signature = signing_key.sign(canonical.as_bytes());

        Some(CustodyAttestation {
            device_id,
            epoch,
            chain_head,
            chain_depth,
            vector_count,
            content_hash: content_hash.to_owned(),
            timestamp,
            signature: signature.to_bytes().to_vec(),
        })
    }

    /// Verify the integrity of the entire chain.
    ///
    /// Walks all events and verifies:
    /// 1. Each event's `prev_hash` matches the prior event's `hash`
    /// 2. Each event's `payload_hash` matches the recomputed payload hash
    /// 3. Each event's `hash` matches the recomputed event hash
    pub fn verify_integrity(&self) -> ChainVerifyResult {
        let chain = self.inner.lock().unwrap();
        let mut errors = Vec::new();

        for (i, event) in chain.events.iter().enumerate() {
            // 1. Verify prev_hash linkage
            let expected_prev = if i == 0 {
                [0u8; 32]
            } else {
                chain.events[i - 1].hash
            };
            if event.prev_hash != expected_prev {
                errors.push(format!(
                    "seq {}: prev_hash mismatch (expected {:02x}{:02x}..., got {:02x}{:02x}...)",
                    event.sequence,
                    expected_prev[0],
                    expected_prev[1],
                    event.prev_hash[0],
                    event.prev_hash[1],
                ));
            }

            // 2. Verify payload_hash
            let recomputed_payload = compute_payload_hash(&event.payload);
            if event.payload_hash != recomputed_payload {
                errors.push(format!(
                    "seq {}: payload_hash mismatch (recomputed {:02x}{:02x}..., stored {:02x}{:02x}...)",
                    event.sequence,
                    recomputed_payload[0], recomputed_payload[1],
                    event.payload_hash[0], event.payload_hash[1],
                ));
            }

            // 3. Recompute and verify event hash
            let recomputed = compute_event_hash(
                event.sequence,
                event.chain_id,
                &event.prev_hash,
                &event.source,
                &event.kind,
                &event.timestamp,
                &event.payload_hash,
                event.rule_hash.as_ref(),
            );
            if event.hash != recomputed {
                errors.push(format!(
                    "seq {}: hash mismatch (recomputed {:02x}{:02x}..., stored {:02x}{:02x}...)",
                    event.sequence, recomputed[0], recomputed[1], event.hash[0], event.hash[1],
                ));
            }
        }

        ChainVerifyResult {
            valid: errors.is_empty(),
            event_count: chain.events.len(),
            errors,
            signature_verified: None,
        }
    }

    /// Save the chain to a file (line-delimited JSON).
    ///
    /// Writes all events as newline-delimited JSON to the given path.
    /// Creates parent directories if they don't exist.
    pub fn save_to_file(
        &self,
        path: &Path,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let chain = self.inner.lock().map_err(|e| format!("lock: {e}"))?;

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let mut output = String::new();
        for event in &chain.events {
            let line = serde_json::to_string(event)?;
            output.push_str(&line);
            output.push('\n');
        }

        std::fs::write(path, output)?;
        info!(
            path = %path.display(),
            events = chain.events.len(),
            sequence = chain.sequence,
            "chain saved to file"
        );
        Ok(())
    }

    /// Load a chain from a file (line-delimited JSON).
    ///
    /// Reads events, verifies integrity, and restores state so that
    /// new events continue from the last sequence number.
    pub fn load_from_file(
        path: &Path,
        checkpoint_interval: u64,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let contents = std::fs::read_to_string(path)?;
        let mut events = Vec::new();

        for line in contents.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let event: ChainEvent = serde_json::from_str(trimmed)?;
            events.push(event);
        }

        if events.is_empty() {
            return Err("chain file is empty (no events)".into());
        }

        let chain_id = events[0].chain_id;
        let chain = LocalChain::from_events(chain_id, checkpoint_interval, events, Vec::new());

        let mgr = Self {
            inner: Mutex::new(chain),
            signing_key: None,
            ml_dsa_key: None,
        };

        // Verify integrity of the loaded chain
        let result = mgr.verify_integrity();
        if !result.valid {
            warn!(
                errors = result.errors.len(),
                "loaded chain has integrity errors"
            );
            return Err(format!(
                "chain integrity check failed: {} errors",
                result.errors.len()
            )
            .into());
        }

        info!(
            path = %path.display(),
            events = result.event_count,
            chain_id,
            "chain restored from file"
        );
        Ok(mgr)
    }

    /// Save the chain as a concatenation of RVF segments.
    ///
    /// Each event is serialized as an ExochainEvent segment (subtype 0x40)
    /// containing a 64-byte ExoChainHeader + CBOR payload. A trailing
    /// ExochainCheckpoint segment (subtype 0x41) records the final chain
    /// state for external verification.
    pub fn save_to_rvf(&self, path: &Path) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let chain = self.inner.lock().map_err(|e| format!("lock: {e}"))?;

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let mut output = Vec::new();

        for event in &chain.events {
            // Build the ExoChainHeader from event fields.
            let exo_header = ExoChainHeader {
                magic: EXOCHAIN_MAGIC,
                version: 1,
                subtype: 0x40, // ExochainEvent
                flags: if event.rule_hash.is_some() { EXO_FLAG_HAS_RULE_HASH } else { 0 },
                chain_id: event.chain_id,
                _reserved: 0,
                sequence: event.sequence,
                timestamp_secs: event.timestamp.timestamp() as u64,
                prev_hash: event.prev_hash,
            };

            // Serialize the remaining fields as CBOR.
            let rvf_payload = RvfChainPayload {
                source: event.source.clone(),
                kind: event.kind.clone(),
                payload: event.payload.clone(),
                payload_hash: hex_hash(&event.payload_hash),
                hash: hex_hash(&event.hash),
                rule_hash: event.rule_hash.as_ref().map(hex_hash),
            };

            let mut cbor_bytes = Vec::new();
            ciborium::into_writer(&rvf_payload, &mut cbor_bytes)
                .map_err(|e| format!("cbor encode: {e}"))?;

            // Write the full RVF segment (header + exo header + cbor + padding).
            let segment = write_exochain_event(&exo_header, &cbor_bytes, event.sequence);
            output.extend_from_slice(&segment);
        }

        // Write a trailing checkpoint segment (subtype 0x41).
        let checkpoint_header = ExoChainHeader {
            magic: EXOCHAIN_MAGIC,
            version: 1,
            subtype: 0x41, // ExochainCheckpoint
            flags: 0,
            chain_id: chain.chain_id,
            _reserved: 0,
            sequence: chain.sequence.saturating_sub(1),
            timestamp_secs: Utc::now().timestamp() as u64,
            prev_hash: chain.last_hash,
        };

        // Serialize and include the witness chain in the checkpoint.
        let witness_hex = if !chain.witness_entries.is_empty() {
            let wc_data = create_witness_chain(&chain.witness_entries);
            Some(hex_encode(&wc_data))
        } else {
            None
        };

        let cp_payload = serde_json::json!({
            "event_count": chain.events.len(),
            "last_hash": hex_hash(&chain.last_hash),
            "witness_chain": witness_hex,
            "witness_entries": chain.witness_entries.len(),
        });
        let mut cp_cbor = Vec::new();
        ciborium::into_writer(&cp_payload, &mut cp_cbor)
            .map_err(|e| format!("cbor encode checkpoint: {e}"))?;

        let cp_segment = write_exochain_event(
            &checkpoint_header,
            &cp_cbor,
            chain.sequence, // use next sequence as segment_id
        );
        output.extend_from_slice(&cp_segment);

        // Sign the checkpoint segment if a signing key is available.
        let signed = if let Some(ref signing_key) = self.signing_key {
            let (cp_seg_header, cp_seg_payload) = read_segment(&cp_segment)
                .map_err(|e| format!("re-read checkpoint for signing: {e}"))?;
            let footer = sign_segment(&cp_seg_header, cp_seg_payload, signing_key);
            let footer_bytes = encode_signature_footer(&footer);
            output.extend_from_slice(&footer_bytes);

            // Dual-sign with ML-DSA-65 if the key is available.
            if let Some(ref ml_key) = self.ml_dsa_key {
                let ml_footer = sign_segment_ml_dsa(&cp_seg_header, cp_seg_payload, ml_key);
                let ml_footer_bytes = encode_signature_footer(&ml_footer);
                output.extend_from_slice(&ml_footer_bytes);
            }

            true
        } else {
            false
        };
        let dual_signed = signed && self.ml_dsa_key.is_some();

        std::fs::write(path, &output)?;
        info!(
            path = %path.display(),
            events = chain.events.len(),
            bytes = output.len(),
            signed,
            dual_signed,
            "chain saved to RVF file"
        );
        Ok(())
    }

    /// Load a chain from an RVF segment file.
    ///
    /// Reads concatenated RVF segments, validates each segment's content
    /// hash, decodes ExoChainHeader + CBOR payload, and reconstructs the
    /// chain events. Checkpoint segments (subtype 0x41) are skipped.
    pub fn load_from_rvf(
        path: &Path,
        checkpoint_interval: u64,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let data = std::fs::read(path)?;
        let mut offset = 0;
        let mut events = Vec::new();
        let mut witness_entries = Vec::new();

        while offset < data.len() {
            // Need at least a segment header.
            if data.len() - offset < SEGMENT_HEADER_SIZE {
                break;
            }

            // Try reading the next segment. If it fails, the remaining
            // bytes may be a signature footer — break out of the loop.
            let (seg_header, seg_payload) = match read_segment(&data[offset..]) {
                Ok(result) => result,
                Err(_) => break,
            };

            // Validate the content hash.
            validate_segment(&seg_header, seg_payload)
                .map_err(|e| format!("validate segment at offset {offset}: {e}"))?;

            // Decode the ExoChainHeader + CBOR from the segment payload.
            let (exo_header, cbor_bytes) = decode_exochain_payload(seg_payload)
                .ok_or_else(|| format!("decode exochain payload at offset {offset}"))?;

            if exo_header.subtype == 0x40 {
                // ExochainEvent -- deserialize the CBOR payload.
                let rvf_payload: RvfChainPayload = ciborium::from_reader(cbor_bytes)
                    .map_err(|e| format!("cbor decode at offset {offset}: {e}"))?;

                let payload_hash = parse_hex_hash(&rvf_payload.payload_hash)?;
                let hash = parse_hex_hash(&rvf_payload.hash)?;
                let rule_hash = rvf_payload
                    .rule_hash
                    .as_deref()
                    .map(parse_hex_hash)
                    .transpose()?;

                check_exo_flags(exo_header.flags, rule_hash.is_some())
                    .map_err(|e| format!("segment at offset {offset}: {e}"))?;

                let timestamp = DateTime::from_timestamp(exo_header.timestamp_secs as i64, 0)
                    .ok_or_else(|| {
                        format!(
                            "invalid timestamp {} at offset {offset}",
                            exo_header.timestamp_secs
                        )
                    })?;

                events.push(ChainEvent {
                    sequence: exo_header.sequence,
                    chain_id: exo_header.chain_id,
                    timestamp,
                    prev_hash: exo_header.prev_hash,
                    hash,
                    payload_hash,
                    source: rvf_payload.source,
                    kind: rvf_payload.kind,
                    payload: rvf_payload.payload,
                    // Idempotency keys are an in-memory dedup hint;
                    // they are not persisted to RVF segments
                    // (preserves chain-hash stability across restores).
                    idempotency_key: None,
                    rule_hash,
                });
            } else if exo_header.subtype == 0x41 {
                // Checkpoint — extract witness chain if present.
                let cp_obj: serde_json::Value =
                    ciborium::from_reader(cbor_bytes).unwrap_or_default();
                if let Some(wc_hex) = cp_obj.get("witness_chain").and_then(|v| v.as_str())
                    && let Ok(wc_bytes) = hex_decode(wc_hex)
                {
                    match verify_witness_chain(&wc_bytes) {
                        Ok(entries) => {
                            witness_entries = entries;
                            debug!(
                                count = witness_entries.len(),
                                "restored witness chain from checkpoint"
                            );
                        }
                        Err(e) => {
                            warn!("witness chain verification failed on load: {e}");
                        }
                    }
                }
            }
            // subtype 0x42 (Proof) is skipped.

            // Advance past the segment: header + payload padded to 64 bytes.
            let padded =
                calculate_padded_size(SEGMENT_HEADER_SIZE, seg_header.payload_length as usize);
            offset += padded;
        }

        // Check for trailing signature footer(s).
        // There may be one (Ed25519) or two (Ed25519 + ML-DSA-65) footers.
        let mut has_signature = false;
        let mut has_dual_signature = false;
        if offset < data.len()
            && let Ok(first_footer) = decode_signature_footer(&data[offset..])
        {
            has_signature = true;
            let first_footer_size = first_footer.footer_length as usize;
            let next_offset = offset + first_footer_size;
            if next_offset < data.len() && decode_signature_footer(&data[next_offset..]).is_ok() {
                has_dual_signature = true;
            }
        }

        if events.is_empty() {
            return Err("RVF file contains no chain events".into());
        }

        let chain_id = events[0].chain_id;
        let chain = LocalChain::from_events(chain_id, checkpoint_interval, events, witness_entries);

        let mgr = Self {
            inner: Mutex::new(chain),
            signing_key: None,
            ml_dsa_key: None,
        };

        // Verify integrity of the loaded chain.
        let result = mgr.verify_integrity();
        if !result.valid {
            warn!(
                errors = result.errors.len(),
                "loaded RVF chain has integrity errors"
            );
            return Err(format!(
                "RVF chain integrity check failed: {} errors",
                result.errors.len()
            )
            .into());
        }

        info!(
            path = %path.display(),
            events = result.event_count,
            chain_id,
            has_signature,
            has_dual_signature,
            "chain restored from RVF file"
        );
        Ok(mgr)
    }

    // ── Lineage tracking ─────────────────────────────────────────
    //
    // DNA-style provenance for agent spawn and resource derivation.
    // Uses rvf-crypto's lineage module to create verifiable derivation
    // records that link parent → child with hash verification.

    /// Record a lineage derivation event in the chain.
    ///
    /// Creates a `LineageRecord` from the given parameters, serializes it,
    /// adds a lineage witness entry, and appends a `lineage.derivation`
    /// chain event with the full record in the payload.
    ///
    /// - `child_id`: UUID of the derived entity (agent, resource)
    /// - `parent_id`: UUID of the parent entity (zero for root)
    /// - `parent_hash`: hash of the parent's state at derivation time
    /// - `derivation_type`: how the child was produced
    /// - `mutation_count`: number of mutations/changes applied
    /// - `description`: human-readable description (max 47 chars)
    pub fn record_lineage(
        &self,
        child_id: [u8; 16],
        parent_id: [u8; 16],
        parent_hash: [u8; 32],
        derivation_type: rvf_types::DerivationType,
        mutation_count: u32,
        description: &str,
    ) -> ChainEvent {
        let timestamp_ns = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0) as u64;

        let record = rvf_types::LineageRecord::new(
            child_id,
            parent_id,
            parent_hash,
            derivation_type,
            mutation_count,
            timestamp_ns,
            description,
        );

        // Serialize the record and create a witness entry.
        let record_bytes = lineage_record_to_bytes(&record);

        // Add a lineage witness entry to the witness chain.
        {
            let mut chain = self.inner.lock().unwrap();
            let prev_hash = if let Some(last) = chain.witness_entries.last() {
                last.action_hash
            } else {
                [0u8; 32]
            };
            let witness = lineage_witness_entry(&record, prev_hash);
            chain.witness_entries.push(witness);
        }

        let payload = serde_json::json!({
            "child_id": hex_encode(&child_id),
            "parent_id": hex_encode(&parent_id),
            "parent_hash": hex_hash(&parent_hash),
            "derivation_type": derivation_type as u8,
            "mutation_count": mutation_count,
            "description": description,
            "record_hex": hex_encode(&record_bytes),
        });

        self.append("lineage", "lineage.derivation", Some(payload))
    }

    /// Extract lineage records from chain events and verify the chain.
    ///
    /// Returns `Ok(count)` if all lineage records form a valid chain,
    /// or `Err` describing the verification failure.
    pub fn verify_lineage(&self) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
        let events = self.tail(0);
        let mut identities: Vec<(rvf_types::FileIdentity, [u8; 32])> = Vec::new();

        for event in &events {
            if event.kind != "lineage.derivation" {
                continue;
            }
            let Some(ref payload) = event.payload else {
                continue;
            };

            let child_id_hex = payload
                .get("child_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let parent_id_hex = payload
                .get("parent_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let parent_hash_hex = payload
                .get("parent_hash")
                .and_then(|v| v.as_str())
                .unwrap_or("");

            let Ok(child_bytes) = hex_decode(child_id_hex) else {
                continue;
            };
            let Ok(parent_bytes) = hex_decode(parent_id_hex) else {
                continue;
            };
            let Ok(parent_hash) = parse_hex_hash(parent_hash_hex) else {
                continue;
            };

            if child_bytes.len() != 16 || parent_bytes.len() != 16 {
                continue;
            }

            let mut child_id = [0u8; 16];
            child_id.copy_from_slice(&child_bytes);
            let mut parent_id = [0u8; 16];
            parent_id.copy_from_slice(&parent_bytes);

            let depth = identities
                .iter()
                .filter(|(fi, _)| fi.file_id == parent_id)
                .map(|(fi, _)| fi.lineage_depth + 1)
                .next()
                .unwrap_or(0);

            let fi = rvf_types::FileIdentity {
                file_id: child_id,
                parent_id,
                parent_hash,
                lineage_depth: depth,
            };

            // Use the event hash as the manifest hash for this identity.
            identities.push((fi, event.hash));
        }

        if identities.is_empty() {
            return Ok(0);
        }

        // Lineage chain verification requires root → leaf ordering.
        // Our records are already in append order, so roots come first.
        // verify_lineage_chain expects consecutive parent→child pairs,
        // but our records may be from different lineage trees. Verify
        // each parent→child pair independently.
        let count = identities.len();
        for (fi, _hash) in &identities {
            if fi.is_root() {
                continue;
            }
            // Find the parent in identities.
            let parent_found = identities
                .iter()
                .any(|(pfi, _)| pfi.file_id == fi.parent_id);
            if !parent_found && fi.parent_id != [0u8; 16] {
                return Err(format!(
                    "lineage record for {} references unknown parent {}",
                    hex_encode(&fi.file_id),
                    hex_encode(&fi.parent_id),
                )
                .into());
            }
        }

        Ok(count)
    }

    // ── Witness bundle integration ─────────────────────────────
    //
    // RVF witness bundles are the atomic proof unit for agent task
    // execution. These methods bridge the gap between the kernel's
    // event chain and the rvf-runtime WitnessBuilder.

    /// Record a completed witness bundle as a chain event.
    ///
    /// Takes the raw bundle bytes and parsed header produced by
    /// `WitnessBuilder::build()`, stores them as a `witness.bundle`
    /// chain event with the bundle hex-encoded in the payload.
    pub fn record_witness_bundle(
        &self,
        bundle_bytes: &[u8],
        header: &rvf_types::witness::WitnessHeader,
        policy_violations: u32,
        rollback_count: u32,
    ) -> ChainEvent {
        let payload = serde_json::json!({
            "task_id": hex_encode(&header.task_id),
            "outcome": header.outcome,
            "governance_mode": header.governance_mode,
            "tool_call_count": header.tool_call_count,
            "total_cost_microdollars": header.total_cost_microdollars,
            "total_latency_ms": header.total_latency_ms,
            "total_tokens": header.total_tokens,
            "bundle_size": bundle_bytes.len(),
            "policy_violations": policy_violations,
            "rollback_count": rollback_count,
            "bundle": hex_encode(bundle_bytes),
        });
        self.append("witness", "witness.bundle", Some(payload))
    }

    /// Aggregate witness bundles from recent chain events into a scorecard.
    ///
    /// Scans the last `n` events (0 = all) for `witness.bundle` events,
    /// parses each bundle, and produces an aggregate `Scorecard`.
    pub fn aggregate_scorecard(&self, n: usize) -> rvf_types::witness::Scorecard {
        let events = self.tail(n);
        let mut builder = rvf_runtime::ScorecardBuilder::new();

        for event in &events {
            if event.kind != "witness.bundle" {
                continue;
            }
            let Some(ref payload) = event.payload else {
                continue;
            };
            let Some(hex_str) = payload.get("bundle").and_then(|v| v.as_str()) else {
                continue;
            };
            let Ok(bytes) = hex_decode(hex_str) else {
                continue;
            };
            let Ok(parsed) = rvf_runtime::ParsedWitness::parse(&bytes) else {
                continue;
            };
            let violations = payload
                .get("policy_violations")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32;
            let rollbacks = payload
                .get("rollback_count")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32;
            builder.add_witness(&parsed, violations, rollbacks);
        }

        builder.finish()
    }

    /// Find the most recent tree root hash recorded in chain events.
    ///
    /// Scans backwards through the event log looking for payloads
    /// that contain `tree_root_hash` (shutdown, boot.ready, boot.manifest)
    /// or `root_hash` on tree-sourced events (tree.checkpoint, checkpoint).
    ///
    /// Returns the hash as a hex string if found.
    pub fn last_tree_root_hash(&self) -> Option<String> {
        let chain = self.inner.lock().unwrap();
        for event in chain.events.iter().rev() {
            let Some(ref payload) = event.payload else {
                continue;
            };
            // Shutdown/boot events record "tree_root_hash" directly.
            if let Some(hash) = payload.get("tree_root_hash").and_then(|v| v.as_str()) {
                return Some(hash.to_string());
            }
            // Tree events record "root_hash".
            if event.source == "tree"
                && matches!(event.kind.as_str(), "tree.checkpoint" | "checkpoint")
                && let Some(hash) = payload.get("root_hash").and_then(|v| v.as_str())
            {
                return Some(hash.to_string());
            }
        }
        None
    }

    /// Get a status summary.
    pub fn status(&self) -> ChainStatus {
        let chain = self.inner.lock().unwrap();
        ChainStatus {
            chain_id: chain.chain_id,
            sequence: chain.sequence,
            last_hash: chain.last_hash,
            event_count: chain.events.len(),
            checkpoint_count: chain.checkpoints.len(),
            events_since_checkpoint: chain.events_since_checkpoint,
        }
    }

    /// Verify the Ed25519 signature on an RVF chain file.
    ///
    /// Reads the file, locates the checkpoint segment and trailing
    /// signature footer, and verifies the signature against the
    /// provided public key.
    ///
    /// Returns `Ok(true)` if signature is valid, `Ok(false)` if
    /// invalid, and `Err` if the file has no signature or can't be read.
    pub fn verify_rvf_signature(
        path: &Path,
        verifying_key: &VerifyingKey,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        let data = std::fs::read(path)?;
        let mut offset = 0;
        let mut last_seg_start = 0;

        // Walk all segments to find the last one (checkpoint).
        while offset < data.len() {
            if data.len() - offset < SEGMENT_HEADER_SIZE {
                break;
            }
            let (seg_header, _seg_payload) = match read_segment(&data[offset..]) {
                Ok(result) => result,
                Err(_) => break,
            };
            last_seg_start = offset;
            let padded =
                calculate_padded_size(SEGMENT_HEADER_SIZE, seg_header.payload_length as usize);
            offset += padded;
        }

        // Remaining bytes should be the signature footer.
        if offset >= data.len() {
            return Err("no signature footer found in RVF file".into());
        }
        let footer = decode_signature_footer(&data[offset..])
            .map_err(|e| format!("decode signature footer: {e}"))?;

        // Re-read the checkpoint segment for verification.
        let (cp_seg_header, cp_seg_payload) = read_segment(&data[last_seg_start..])
            .map_err(|e| format!("re-read checkpoint segment: {e}"))?;

        Ok(verify_segment(
            &cp_seg_header,
            cp_seg_payload,
            &footer,
            verifying_key,
        ))
    }

    /// Verify dual (Ed25519 + ML-DSA-65) signatures on an RVF chain file.
    ///
    /// Returns `Ok(true)` if both signatures are valid, `Ok(false)` if
    /// either is invalid, and `Err` if the file has no dual signature.
    pub fn verify_rvf_dual_signature(
        path: &Path,
        ed_key: &VerifyingKey,
        ml_key: &MlDsa65VerifyKey,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        let data = std::fs::read(path)?;
        let mut offset = 0;
        let mut last_seg_start = 0;

        while offset < data.len() {
            if data.len() - offset < SEGMENT_HEADER_SIZE {
                break;
            }
            let (seg_header, _) = match read_segment(&data[offset..]) {
                Ok(result) => result,
                Err(_) => break,
            };
            last_seg_start = offset;
            let padded =
                calculate_padded_size(SEGMENT_HEADER_SIZE, seg_header.payload_length as usize);
            offset += padded;
        }

        if offset >= data.len() {
            return Err("no signature footer found in RVF file".into());
        }

        let ed_footer = decode_signature_footer(&data[offset..])
            .map_err(|e| format!("decode Ed25519 signature footer: {e}"))?;
        let ed_footer_size = ed_footer.footer_length as usize;
        let ml_offset = offset + ed_footer_size;

        if ml_offset >= data.len() {
            return Err("no ML-DSA-65 signature footer found (single-signed file)".into());
        }
        let ml_footer = decode_signature_footer(&data[ml_offset..])
            .map_err(|e| format!("decode ML-DSA-65 signature footer: {e}"))?;

        let (cp_seg_header, cp_seg_payload) = read_segment(&data[last_seg_start..])
            .map_err(|e| format!("re-read checkpoint segment: {e}"))?;

        let ed_ok = verify_segment(&cp_seg_header, cp_seg_payload, &ed_footer, ed_key);
        let ml_ok = verify_segment_ml_dsa(&cp_seg_header, cp_seg_payload, &ml_footer, ml_key);

        Ok(ed_ok && ml_ok)
    }

    // ── Dual signing for cross-node chain events ───────────────

    /// Sign data with both Ed25519 and ML-DSA-65 (if configured).
    ///
    /// Returns `Some(DualSignature)` when at least the Ed25519 key is
    /// present. The ML-DSA-65 half is populated only when the ML-DSA key
    /// has been set via [`set_ml_dsa_key`].
    pub fn dual_sign(&self, data: &[u8]) -> Option<DualSignature> {
        use ed25519_dalek::Signer;

        if signs_project_domain(data) {
            tracing::error!("dual_sign refused bytes tagged for a project statement");
            return None;
        }
        let signing_key = self.signing_key.as_ref()?;
        let ed_sig = signing_key.sign(data);
        let ed_bytes = ed_sig.to_bytes().to_vec();

        let ml_sig = self.ml_dsa_key.as_ref().map(|ml_key| {
            // Use SHAKE-256 HMAC-like construction matching rvf-crypto placeholder.
            ml_dsa_sign_raw(ml_key, data)
        });

        Some(DualSignature {
            ed25519: ed_bytes,
            ml_dsa65: ml_sig,
        })
    }

    /// Verify a dual signature against the given data and public keys.
    ///
    /// Ed25519 verification is mandatory. ML-DSA-65 verification is
    /// performed only when both the signature and verification key are
    /// present. Returns `false` if any present signature is invalid.
    pub fn verify_dual_signature(
        data: &[u8],
        sig: &DualSignature,
        ed25519_pubkey: &VerifyingKey,
        ml_dsa_pubkey: Option<&MlDsa65VerifyKey>,
    ) -> bool {
        use ed25519_dalek::{Signature, Verifier};

        // Ed25519 is mandatory.
        if sig.ed25519.len() != 64 {
            return false;
        }
        let ed_sig = Signature::from_bytes(sig.ed25519.as_slice().try_into().unwrap_or(&[0u8; 64]));
        if ed25519_pubkey.verify(data, &ed_sig).is_err() {
            return false;
        }

        // ML-DSA-65 when both signature and key are present.
        if let (Some(ml_sig), Some(ml_key)) = (&sig.ml_dsa65, ml_dsa_pubkey)
            && !ml_dsa_verify_raw(ml_key, data, ml_sig)
        {
            return false;
        }

        true
    }
}

// ── Raw ML-DSA-65 placeholder signing for arbitrary data ──────────
//
// These mirror the HMAC-SHA3-256 placeholder in rvf-crypto's dual_sign
// module but operate on raw byte slices instead of RVF segments.

/// ML-DSA-65 placeholder signature length (FIPS 204).
const ML_DSA_RAW_SIG_LEN: usize = 3309;

/// Sign arbitrary data with the ML-DSA-65 placeholder (HMAC-SHAKE-256).
fn ml_dsa_sign_raw(key: &MlDsa65Key, data: &[u8]) -> Vec<u8> {
    // Extract 32-byte key material via verifying_key round-trip.
    let vk = key.verifying_key();
    let key_bytes = ml_dsa_vk_bytes(&vk);

    let mut input = Vec::with_capacity(32 + data.len() + 32);
    input.extend_from_slice(&key_bytes);
    input.extend_from_slice(data);
    input.extend_from_slice(&key_bytes);

    let mut sig = Vec::with_capacity(ML_DSA_RAW_SIG_LEN);
    let mut block = shake256_256(&input);
    while sig.len() < ML_DSA_RAW_SIG_LEN {
        sig.extend_from_slice(&block);
        let mut next = Vec::with_capacity(64);
        next.extend_from_slice(&block);
        next.extend_from_slice(&key_bytes);
        block = shake256_256(&next);
    }
    sig.truncate(ML_DSA_RAW_SIG_LEN);
    sig
}

/// Verify an ML-DSA-65 placeholder signature on arbitrary data.
fn ml_dsa_verify_raw(pubkey: &MlDsa65VerifyKey, data: &[u8], sig: &[u8]) -> bool {
    let key_bytes = ml_dsa_vk_bytes(pubkey);

    let mut input = Vec::with_capacity(32 + data.len() + 32);
    input.extend_from_slice(&key_bytes);
    input.extend_from_slice(data);
    input.extend_from_slice(&key_bytes);

    let mut expected = Vec::with_capacity(ML_DSA_RAW_SIG_LEN);
    let mut block = shake256_256(&input);
    while expected.len() < ML_DSA_RAW_SIG_LEN {
        expected.extend_from_slice(&block);
        let mut next = Vec::with_capacity(64);
        next.extend_from_slice(&block);
        next.extend_from_slice(&key_bytes);
        block = shake256_256(&next);
    }
    expected.truncate(ML_DSA_RAW_SIG_LEN);

    sig.len() == ML_DSA_RAW_SIG_LEN && sig == expected.as_slice()
}

/// Extract the 32-byte key material from an `MlDsa65VerifyKey`.
///
/// Since `MlDsa65VerifyKey` does not expose its inner bytes directly,
/// we regenerate via the same seed path used in generate(). In
/// placeholder mode the signing key and verify key share the same
/// 32-byte SHAKE-256 digest, so `verifying_key()` round-trips cleanly.
fn ml_dsa_vk_bytes(vk: &MlDsa65VerifyKey) -> [u8; 32] {
    // MlDsa65VerifyKey is Clone; generate a fresh one from a known seed
    // and compare — but we cannot peek inside. Instead we use a simple
    // workaround: the test-time key generation uses `generate(seed)` which
    // produces `key = SHAKE-256(seed)`. The verify key holds the same bytes.
    // Since the struct is opaque, we sign a sentinel and derive the key
    // bytes from the output (the first 32 bytes of the HMAC chain are
    // `SHAKE-256(key || sentinel || key)` which IS deterministic).
    //
    // However, the simplest correct approach: we know in placeholder mode
    // signing_key bytes == verify_key bytes. The MlDsa65Key::generate
    // returns both with the same 32-byte value. So we reconstruct via
    // a zero-length sign and extract the block. This works because the
    // HMAC construction in rvf-crypto uses the key bytes directly.
    //
    // For a clean API, we add a compile-time assertion that verifying_key
    // is 32 bytes and access it through the known memory layout.
    //
    // In practice, both MlDsa65Key and MlDsa65VerifyKey wrap `key: [u8; 32]`.
    // We use unsafe transmute in a controlled, size-asserted way.
    assert_eq!(
        std::mem::size_of::<MlDsa65VerifyKey>(),
        32,
        "MlDsa65VerifyKey must be exactly 32 bytes"
    );
    // SAFETY: MlDsa65VerifyKey is a repr(Rust) struct containing only
    // `key: [u8; 32]`. We assert the size matches before transmuting.
    unsafe { std::mem::transmute_copy(vk) }
}

// ── Cross-node dual signature types ───────────────────────────────

/// Configuration for dual Ed25519 + ML-DSA-65 signing.
pub struct DualSigningConfig {
    /// Ed25519 signing key.
    pub ed25519_key: SigningKey,
    /// ML-DSA-65 signing key (if available).
    pub ml_dsa_key: Option<MlDsa65Key>,
}

/// A dual signature (Ed25519 + optional ML-DSA-65).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DualSignature {
    /// Ed25519 signature (64 bytes).
    pub ed25519: Vec<u8>,
    /// ML-DSA-65 signature (optional, ~3309 bytes).
    pub ml_dsa65: Option<Vec<u8>>,
}

/// Chain status summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainStatus {
    pub chain_id: u32,
    pub sequence: u64,
    pub last_hash: [u8; 32],
    pub event_count: usize,
    pub checkpoint_count: usize,
    pub events_since_checkpoint: u64,
}

/// A single signed proof of system state.
///
/// Assembles chain state, vector store metrics, and a content hash
/// into a document signed by the kernel's Ed25519 key.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustodyAttestation {
    /// Ed25519 public key hex (device identity).
    pub device_id: String,
    /// Current vector store epoch.
    pub epoch: u64,
    /// Hash of the latest chain event (hex).
    pub chain_head: String,
    /// Number of chain events.
    pub chain_depth: u64,
    /// Number of vectors in the store.
    pub vector_count: u64,
    /// BLAKE3 hash of all vector IDs (hex, or "none" if no backend).
    pub content_hash: String,
    /// Unix timestamp (seconds).
    pub timestamp: u64,
    /// Ed25519 signature over the canonical attestation payload.
    pub signature: Vec<u8>,
}

impl std::fmt::Debug for ChainManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let status = self.status();
        f.debug_struct("ChainManager")
            .field("chain_id", &status.chain_id)
            .field("sequence", &status.sequence)
            .field("event_count", &status.event_count)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// K4 G1: ChainAnchor trait — external anchoring abstraction (K2 C7)
// ---------------------------------------------------------------------------

/// Receipt returned by a successful [`ChainAnchor::anchor`] call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnchorReceipt {
    /// The hash that was anchored.
    pub hash: [u8; 32],
    /// Backend-specific transaction/receipt ID.
    pub tx_id: String,
    /// Timestamp of the anchoring operation.
    pub anchored_at: DateTime<Utc>,
}

/// Trait for anchoring chain state to an external ledger or store.
///
/// Implementations:
/// - [`MockAnchor`] — always-succeeds (tests)
/// - [`crate::chain_anchor::FileLedgerAnchor`] — append-only hash-linked
///   local ledger (production-useful default; WEFT-137)
/// - [`crate::chain_anchor::ExternalLedgerAnchor`] — external-ledger stub
///   with intent log + endpoint wiring (until a public ledger is chosen)
///
/// Frequency policy and config wiring live in
/// [`crate::chain_anchor::AnchoringController`].
pub trait ChainAnchor: Send + Sync {
    /// Anchor the given hash to the external backend.
    fn anchor(&self, hash: &[u8; 32]) -> Result<AnchorReceipt, String>;

    /// Verify a previously anchored hash against its receipt.
    fn verify(&self, receipt: &AnchorReceipt) -> Result<bool, String>;

    /// Return the backend name for display/logging.
    fn backend_name(&self) -> &str;
}

/// A mock anchor that always succeeds (for testing).
pub struct MockAnchor;

impl ChainAnchor for MockAnchor {
    fn anchor(&self, hash: &[u8; 32]) -> Result<AnchorReceipt, String> {
        Ok(AnchorReceipt {
            hash: *hash,
            tx_id: format!(
                "mock-{}",
                hex_hash(hash).chars().take(16).collect::<String>()
            ),
            anchored_at: Utc::now(),
        })
    }

    fn verify(&self, _receipt: &AnchorReceipt) -> Result<bool, String> {
        Ok(true)
    }

    fn backend_name(&self) -> &str {
        "mock"
    }
}

// ── ChainLoggable trait ─────────────────────────────────────────────

/// Trait for types that can be logged to the ExoChain audit trail.
///
/// Implementors define how they map to a chain event kind string and
/// a JSON payload. The [`ChainManager::append_loggable`] convenience
/// method uses these to append an event without the caller needing to
/// hand-craft the source/kind/payload triple.
pub trait ChainLoggable {
    /// The source subsystem (e.g. "supervisor", "governance", "ipc").
    fn chain_event_source(&self) -> &str;

    /// The event kind string (e.g. "agent.restart", "governance.deny").
    fn chain_event_kind(&self) -> &str;

    /// Build the JSON payload for the chain event.
    fn chain_event_payload(&self) -> serde_json::Value;
}

impl ChainManager {
    /// Append an event from any [`ChainLoggable`] implementor.
    pub fn append_loggable(&self, event: &dyn ChainLoggable) -> ChainEvent {
        self.append(
            event.chain_event_source(),
            event.chain_event_kind(),
            Some(event.chain_event_payload()),
        )
    }
}

// ── ChainLoggable implementations ───────────────────────────────────

/// A restart event suitable for chain logging.
///
/// Created by the supervisor after successfully restarting an agent.
pub struct RestartEvent {
    /// The agent identifier that was restarted.
    pub agent_id: String,
    /// PID of the process that crashed / was stopped.
    pub old_pid: u64,
    /// PID of the newly spawned replacement process.
    pub new_pid: u64,
    /// Exit code that triggered the restart.
    pub exit_code: i32,
    /// Restart strategy that was applied.
    pub strategy: String,
    /// Backoff delay in milliseconds before the restart was attempted.
    pub backoff_ms: u64,
    /// Timestamp of the restart.
    pub timestamp: DateTime<Utc>,
}

impl ChainLoggable for RestartEvent {
    fn chain_event_source(&self) -> &str {
        "supervisor"
    }

    fn chain_event_kind(&self) -> &str {
        "supervisor.restart"
    }

    fn chain_event_payload(&self) -> serde_json::Value {
        serde_json::json!({
            "agent_id": self.agent_id,
            "old_pid": self.old_pid,
            "new_pid": self.new_pid,
            "exit_code": self.exit_code,
            "strategy": self.strategy,
            "backoff_ms": self.backoff_ms,
            "timestamp": self.timestamp.to_rfc3339(),
        })
    }
}

/// A governance decision event suitable for chain logging.
///
/// Captures the result of `GovernanceEngine::evaluate` for audit.
pub struct GovernanceDecisionEvent {
    /// Agent that made the request.
    pub agent_id: String,
    /// Action that was evaluated.
    pub action: String,
    /// The governance decision outcome.
    pub decision: String,
    /// Effect vector magnitude.
    pub effect_magnitude: f64,
    /// Whether the risk threshold was exceeded.
    pub threshold_exceeded: bool,
    /// Rules that were evaluated.
    pub evaluated_rules: Vec<String>,
    /// Timestamp of the evaluation.
    pub timestamp: DateTime<Utc>,
    /// Attributed principal for the verdict (WEFT-636). Optional for
    /// backward-compat with older constructors; when set, appears in the
    /// chain payload for audit attribution.
    pub principal: Option<crate::governance::GatePrincipal>,
}

impl ChainLoggable for GovernanceDecisionEvent {
    fn chain_event_source(&self) -> &str {
        "governance"
    }

    fn chain_event_kind(&self) -> &str {
        match self.decision.as_str() {
            "Permit" => "governance.permit",
            "PermitWithWarning" => "governance.warn",
            "EscalateToHuman" => "governance.defer",
            "Deny" => "governance.deny",
            _ => "governance.unknown",
        }
    }

    fn chain_event_payload(&self) -> serde_json::Value {
        let mut payload = serde_json::json!({
            "agent_id": self.agent_id,
            "action": self.action,
            "decision": self.decision,
            "effect_magnitude": self.effect_magnitude,
            "threshold_exceeded": self.threshold_exceeded,
            "evaluated_rules": self.evaluated_rules,
            "timestamp": self.timestamp.to_rfc3339(),
        });
        if let Some(p) = &self.principal
            && let Some(obj) = payload.as_object_mut()
        {
            obj.insert(
                "principal".into(),
                serde_json::to_value(p).unwrap_or(serde_json::Value::Null),
            );
        }
        payload
    }
}

/// An IPC dead-letter event suitable for chain logging.
///
/// Captures when a message is routed to the dead letter queue.
pub struct IpcDeadLetterEvent {
    /// Original message ID.
    pub message_id: String,
    /// Sender PID.
    pub from_pid: u64,
    /// Target description (formatted from MessageTarget).
    pub target: String,
    /// Payload type name.
    pub payload_type: String,
    /// Reason delivery failed.
    pub reason: String,
    /// Timestamp of the dead-lettering.
    pub timestamp: DateTime<Utc>,
}

impl ChainLoggable for IpcDeadLetterEvent {
    fn chain_event_source(&self) -> &str {
        "ipc"
    }

    fn chain_event_kind(&self) -> &str {
        "ipc.dead_letter"
    }

    fn chain_event_payload(&self) -> serde_json::Value {
        serde_json::json!({
            "message_id": self.message_id,
            "from_pid": self.from_pid,
            "target": self.target,
            "payload_type": self.payload_type,
            "reason": self.reason,
            "timestamp": self.timestamp.to_rfc3339(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn genesis_event() {
        let cm = ChainManager::new(0, 1000);
        assert_eq!(cm.len(), 1);
        assert_eq!(cm.sequence(), 1); // genesis consumed seq 0
        let events = cm.tail(0);
        assert_eq!(events[0].kind, "genesis");
        assert_eq!(events[0].sequence, 0);
        assert_eq!(events[0].prev_hash, [0u8; 32]);
    }

    #[test]
    fn append_links_hashes() {
        let cm = ChainManager::new(0, 1000);
        let genesis_hash = cm.last_hash();

        let e1 = cm.append("test", "event.one", None);
        assert_eq!(e1.prev_hash, genesis_hash);
        assert_ne!(e1.hash, [0u8; 32]);

        let e2 = cm.append(
            "test",
            "event.two",
            Some(serde_json::json!({"key": "value"})),
        );
        assert_eq!(e2.prev_hash, e1.hash);
    }

    /// WEFT-103: an idempotency_key match within the lookback window
    /// must short-circuit the append and return the prior event
    /// rather than producing a duplicate ledger entry.
    #[test]
    fn append_idempotent_dedups_within_window() {
        let cm = ChainManager::new(0, 1000);
        let key = [0xABu8; 32];

        let starting_len = cm.len();
        let first = cm.append_idempotent(
            "test",
            "idempotent.event",
            Some(serde_json::json!({"n": 1})),
            Some(key),
        );
        assert_eq!(cm.len(), starting_len + 1);
        assert_eq!(first.idempotency_key, Some(key));

        // Replay -- payload differs but key matches: must be a no-op.
        let replay = cm.append_idempotent(
            "test",
            "idempotent.event",
            Some(serde_json::json!({"n": 2})),
            Some(key),
        );
        assert_eq!(cm.len(), starting_len + 1, "replay must not append");
        assert_eq!(replay.sequence, first.sequence);
        assert_eq!(replay.hash, first.hash);
    }

    /// WEFT-103: distinct keys must each produce a fresh event, and
    /// an absent key must always append (default-on behaviour).
    #[test]
    fn append_idempotent_distinct_keys_append() {
        let cm = ChainManager::new(0, 1000);
        let starting_len = cm.len();

        let _ = cm.append_idempotent("test", "k.a", None, Some([0x01u8; 32]));
        let _ = cm.append_idempotent("test", "k.b", None, Some([0x02u8; 32]));
        // No key -- always appends.
        let _ = cm.append_idempotent("test", "k.none", None, None);
        let _ = cm.append_idempotent("test", "k.none", None, None);

        assert_eq!(cm.len(), starting_len + 4);
    }

    #[test]
    fn checkpoint() {
        let cm = ChainManager::new(0, 1000);
        cm.append("test", "event", None);

        let cp = cm.checkpoint();
        assert_eq!(cp.chain_id, 0);
        assert_eq!(cp.sequence, 1);
        assert_eq!(cm.checkpoints().len(), 1);
    }

    #[test]
    fn tail_from_zero_returns_all() {
        let cm = ChainManager::new(0, 1000);
        cm.append("test", "event.one", None);
        cm.append("test", "event.two", None);

        // tail_from(0) should skip genesis (seq 0) and return seq 1, 2
        // But actually: genesis is seq 0 so tail_from(0) returns events
        // with sequence > 0 i.e. the two appended events.
        let all = cm.tail_from(0);
        // We have genesis(0), event.one(1), event.two(2) — 2 events after 0
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn tail_from_n_returns_after() {
        let cm = ChainManager::new(0, 1000);
        let e1 = cm.append("test", "event.one", None);
        let _e2 = cm.append("test", "event.two", None);

        let after = cm.tail_from(e1.sequence);
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].kind, "event.two");
    }

    #[test]
    fn tail_from_head_returns_empty() {
        let cm = ChainManager::new(0, 1000);
        cm.append("test", "event.one", None);

        let head_seq = cm.head_sequence();
        let after = cm.tail_from(head_seq);
        assert!(after.is_empty());
    }

    /// WEFT-105: remote events apply via append_signed with hash verification.
    /// Sink shares the source genesis (same cluster prefix).
    #[test]
    fn append_signed_replays_remote_events() {
        let source = ChainManager::new(0, 1000);
        source.append("test", "a", Some(serde_json::json!({"n": 1})));
        source.append("test", "b", Some(serde_json::json!({"n": 2})));
        let remote_events = source.tail_from(0);
        assert_eq!(remote_events.len(), 2);

        let genesis = source.tail(0)[0].clone();
        let sink = ChainManager::from_events(vec![genesis], 1000).unwrap();

        for ev in remote_events {
            sink.append_signed(ev).unwrap();
        }
        assert_eq!(sink.head_sequence(), source.head_sequence());
        assert_eq!(sink.head_hash(), source.head_hash());
        assert_eq!(sink.len(), source.len());
    }

    #[test]
    fn append_signed_idempotent_and_rejects_tamper() {
        let source = ChainManager::new(0, 1000);
        let e = source.append("test", "x", None);

        let genesis = source.tail(0)[0].clone();
        let sink = ChainManager::from_events(vec![genesis], 1000).unwrap();
        sink.append_signed(e.clone()).unwrap();
        let err = sink.append_signed(e.clone()).unwrap_err();
        assert!(matches!(err, AppendSignedError::AlreadyPresent { .. }));

        let mut tampered = e;
        tampered.hash[0] ^= 0xFF;
        // sequence already present with different hash → fork
        let err = sink.append_signed(tampered).unwrap_err();
        assert!(matches!(err, AppendSignedError::Fork { .. }));
    }

    #[test]
    fn append_signed_gap_when_skip_sequence() {
        let source = ChainManager::new(0, 1000);
        source.append("test", "a", None);
        let e2 = source.append("test", "b", None);

        let genesis = source.tail(0)[0].clone();
        let sink = ChainManager::from_events(vec![genesis], 1000).unwrap();
        // Skip event "a" (seq 1), try to append "b" (seq 2)
        let err = sink.append_signed(e2).unwrap_err();
        assert!(matches!(err, AppendSignedError::Gap { expected: 1, got: 2 }));
    }

    #[test]
    fn head_sequence_empty_chain() {
        // ChainManager::new always creates a genesis event, so we
        // verify head_sequence returns the genesis sequence (0).
        let cm = ChainManager::new(0, 1000);
        assert_eq!(cm.head_sequence(), 0);
    }

    #[test]
    fn head_sequence_after_appends() {
        let cm = ChainManager::new(0, 1000);
        cm.append("test", "a", None);
        cm.append("test", "b", None);
        // genesis=0, a=1, b=2
        assert_eq!(cm.head_sequence(), 2);
    }

    #[test]
    fn head_hash_matches_last_event() {
        let cm = ChainManager::new(0, 1000);
        let e = cm.append("test", "event", None);
        assert_eq!(cm.head_hash(), e.hash);
        assert_ne!(cm.head_hash(), [0u8; 32]);
    }

    #[test]
    fn auto_checkpoint() {
        let cm = ChainManager::new(0, 5); // checkpoint every 5 events
        // Genesis is event 0 (1 event since checkpoint)
        for i in 0..4 {
            cm.append("test", &format!("event.{i}"), None);
        }
        // 5 total events (genesis + 4) -> should auto-checkpoint
        assert_eq!(cm.checkpoints().len(), 1);
    }

    #[test]
    fn status() {
        let cm = ChainManager::new(0, 1000);
        cm.append("test", "event", None);
        let status = cm.status();
        assert_eq!(status.chain_id, 0);
        assert_eq!(status.sequence, 2);
        assert_eq!(status.event_count, 2);
    }

    #[test]
    fn verify_integrity_valid() {
        let cm = ChainManager::new(0, 1000);
        cm.append("kernel", "boot.init", None);
        cm.append("tree", "bootstrap", Some(serde_json::json!({"nodes": 8})));
        cm.append("kernel", "boot.ready", None);

        let result = cm.verify_integrity();
        assert!(result.valid);
        assert_eq!(result.event_count, 4); // genesis + 3
        assert!(result.errors.is_empty());
    }

    #[test]
    fn save_and_load_roundtrip() {
        let cm = ChainManager::new(0, 1000);
        cm.append("kernel", "boot.init", None);
        cm.append("tree", "bootstrap", Some(serde_json::json!({"nodes": 8})));
        cm.append("kernel", "boot.ready", None);

        let original_seq = cm.sequence();
        let original_hash = cm.last_hash();
        let original_len = cm.len();

        let dir = std::env::temp_dir().join("clawft-chain-test");
        let path = dir.join("test-chain.json");
        cm.save_to_file(&path).unwrap();

        let restored = ChainManager::load_from_file(&path, 1000).unwrap();
        assert_eq!(restored.sequence(), original_seq);
        assert_eq!(restored.last_hash(), original_hash);
        assert_eq!(restored.len(), original_len);
        assert_eq!(restored.chain_id(), 0);

        // Verify restored chain integrity
        let result = restored.verify_integrity();
        assert!(result.valid);

        // New events continue from restored state
        let new_event = restored.append("test", "after.restore", None);
        assert_eq!(new_event.sequence, original_seq);
        assert_eq!(new_event.prev_hash, original_hash);

        // Clean up
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_from_nonexistent_file_fails() {
        let result = ChainManager::load_from_file(
            &std::path::PathBuf::from("/tmp/nonexistent-chain-file.json"),
            1000,
        );
        assert!(result.is_err());
    }

    #[test]
    fn tail() {
        let cm = ChainManager::new(0, 1000);
        cm.append("a", "1", None);
        cm.append("b", "2", None);
        cm.append("c", "3", None);

        let last2 = cm.tail(2);
        assert_eq!(last2.len(), 2);
        assert_eq!(last2[0].kind, "2");
        assert_eq!(last2[1].kind, "3");

        let all = cm.tail(0);
        assert_eq!(all.len(), 4); // genesis + 3
    }

    #[test]
    fn save_and_load_rvf_roundtrip() {
        let cm = ChainManager::new(0, 1000);
        cm.append("kernel", "boot.init", None);
        cm.append("tree", "bootstrap", Some(serde_json::json!({"nodes": 8})));
        cm.append("kernel", "boot.ready", None);

        let original_seq = cm.sequence();
        let original_hash = cm.last_hash();
        let original_len = cm.len();

        let dir = std::env::temp_dir().join("clawft-chain-rvf-test");
        let path = dir.join("test-chain.rvf");
        cm.save_to_rvf(&path).unwrap();

        let restored = ChainManager::load_from_rvf(&path, 1000).unwrap();
        assert_eq!(restored.sequence(), original_seq);
        assert_eq!(restored.last_hash(), original_hash);
        assert_eq!(restored.len(), original_len);
        assert_eq!(restored.chain_id(), 0);

        // Verify restored chain integrity.
        let result = restored.verify_integrity();
        assert!(result.valid, "integrity errors: {:?}", result.errors);
        assert_eq!(result.event_count, original_len);

        // New events continue from restored state.
        let new_event = restored.append("test", "after.rvf.restore", None);
        assert_eq!(new_event.sequence, original_seq);
        assert_eq!(new_event.prev_hash, original_hash);

        // Clean up.
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rvf_validates_on_load() {
        let cm = ChainManager::new(0, 1000);
        cm.append("kernel", "boot", None);

        let dir = std::env::temp_dir().join("clawft-chain-rvf-validate");
        let path = dir.join("corrupt.rvf");
        cm.save_to_rvf(&path).unwrap();

        // Corrupt a byte in the first segment's payload area.
        let mut data = std::fs::read(&path).unwrap();
        // The payload starts at SEGMENT_HEADER_SIZE (64). Flip a byte
        // inside the ExoChainHeader portion of the payload.
        if data.len() > SEGMENT_HEADER_SIZE + 10 {
            data[SEGMENT_HEADER_SIZE + 10] ^= 0xFF;
        }
        std::fs::write(&path, &data).unwrap();

        let result = ChainManager::load_from_rvf(&path, 1000);
        assert!(
            result.is_err(),
            "expected validation error on corrupted RVF"
        );

        // Clean up.
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rvf_migration_from_json() {
        // Create a chain via the normal API.
        let cm = ChainManager::new(0, 1000);
        cm.append("kernel", "boot.init", None);
        cm.append(
            "tree",
            "bootstrap",
            Some(serde_json::json!({"nodes": 4, "name": "test"})),
        );
        cm.append("kernel", "boot.ready", None);

        let dir = std::env::temp_dir().join("clawft-chain-migrate-test");
        let json_path = dir.join("chain.json");
        let rvf_path = dir.join("chain.rvf");

        // Save as JSON, load as JSON.
        cm.save_to_file(&json_path).unwrap();
        let from_json = ChainManager::load_from_file(&json_path, 1000).unwrap();

        // Save the JSON-loaded chain as RVF.
        from_json.save_to_rvf(&rvf_path).unwrap();

        // Load from RVF and compare.
        let from_rvf = ChainManager::load_from_rvf(&rvf_path, 1000).unwrap();

        assert_eq!(from_rvf.sequence(), cm.sequence());
        assert_eq!(from_rvf.last_hash(), cm.last_hash());
        assert_eq!(from_rvf.len(), cm.len());
        assert_eq!(from_rvf.chain_id(), cm.chain_id());

        // Compare event-by-event.
        let original_events = cm.tail(0);
        let rvf_events = from_rvf.tail(0);
        assert_eq!(original_events.len(), rvf_events.len());
        for (orig, loaded) in original_events.iter().zip(rvf_events.iter()) {
            assert_eq!(orig.sequence, loaded.sequence);
            assert_eq!(orig.chain_id, loaded.chain_id);
            assert_eq!(orig.hash, loaded.hash);
            assert_eq!(orig.prev_hash, loaded.prev_hash);
            assert_eq!(orig.payload_hash, loaded.payload_hash);
            assert_eq!(orig.source, loaded.source);
            assert_eq!(orig.kind, loaded.kind);
            assert_eq!(orig.payload, loaded.payload);
        }

        // Verify integrity of the RVF-loaded chain.
        let result = from_rvf.verify_integrity();
        assert!(result.valid, "integrity errors: {:?}", result.errors);

        // Clean up.
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ed25519_signed_rvf_roundtrip() {
        use ed25519_dalek::SigningKey;
        use rand::rngs::OsRng;

        let key = SigningKey::generate(&mut OsRng);

        let cm = ChainManager::new(0, 1000).with_signing_key(key.clone());
        assert!(cm.has_signing_key());
        cm.append("kernel", "boot.init", None);
        cm.append("tree", "bootstrap", Some(serde_json::json!({"nodes": 8})));
        cm.append("kernel", "boot.ready", None);

        let original_seq = cm.sequence();
        let original_hash = cm.last_hash();
        let original_len = cm.len();

        let dir = std::env::temp_dir().join("clawft-chain-signed-test");
        let path = dir.join("signed-chain.rvf");
        cm.save_to_rvf(&path).unwrap();

        // File should be larger than unsigned (72 extra bytes for Ed25519 footer).
        let _file_size = std::fs::metadata(&path).unwrap().len();

        // Load the signed file (should work even without a key).
        let restored = ChainManager::load_from_rvf(&path, 1000).unwrap();
        assert_eq!(restored.sequence(), original_seq);
        assert_eq!(restored.last_hash(), original_hash);
        assert_eq!(restored.len(), original_len);

        let result = restored.verify_integrity();
        assert!(result.valid, "integrity errors: {:?}", result.errors);

        // Verify the signature.
        let pubkey = key.verifying_key();
        let sig_valid = ChainManager::verify_rvf_signature(&path, &pubkey).unwrap();
        assert!(sig_valid, "signature should be valid");

        // Verify with wrong key fails.
        let wrong_key = SigningKey::generate(&mut OsRng);
        let wrong_pubkey = wrong_key.verifying_key();
        let sig_wrong = ChainManager::verify_rvf_signature(&path, &wrong_pubkey).unwrap();
        assert!(!sig_wrong, "signature should fail with wrong key");

        // Clean up.
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ed25519_tampered_checkpoint_fails_verification() {
        use ed25519_dalek::SigningKey;
        use rand::rngs::OsRng;

        let key = SigningKey::generate(&mut OsRng);
        let cm = ChainManager::new(0, 1000).with_signing_key(key.clone());
        cm.append("kernel", "boot", None);

        let dir = std::env::temp_dir().join("clawft-chain-tampered-sig");
        let path = dir.join("tampered.rvf");
        cm.save_to_rvf(&path).unwrap();

        let mut data = std::fs::read(&path).unwrap();
        let footer_size = 72; // Ed25519: 2 + 2 + 64 + 4

        // Walk segments to find the checkpoint segment start.
        let mut offset = 0;
        let mut last_seg_start = 0;
        while offset + SEGMENT_HEADER_SIZE <= data.len() - footer_size {
            match read_segment(&data[offset..]) {
                Ok((seg_header, _)) => {
                    last_seg_start = offset;
                    let padded = calculate_padded_size(
                        SEGMENT_HEADER_SIZE,
                        seg_header.payload_length as usize,
                    );
                    offset += padded;
                }
                Err(_) => break,
            }
        }

        // Tamper with the first byte of the checkpoint segment's payload
        // (the ExoChainHeader magic). This is covered by the signature.
        data[last_seg_start + SEGMENT_HEADER_SIZE] ^= 0xFF;
        std::fs::write(&path, &data).unwrap();

        // Signature verification should fail (checkpoint payload was tampered).
        let pubkey = key.verifying_key();
        // Err is also acceptable — a tampered segment can't be parsed.
        if let Ok(valid) = ChainManager::verify_rvf_signature(&path, &pubkey) {
            assert!(!valid, "tampered checkpoint should not verify");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unsigned_rvf_loads_successfully() {
        // An unsigned chain should still load fine (no signing key).
        let cm = ChainManager::new(0, 1000); // no signing key
        assert!(!cm.has_signing_key());
        cm.append("kernel", "boot", None);
        cm.append("test", "event", Some(serde_json::json!({"x": 1})));

        let dir = std::env::temp_dir().join("clawft-chain-unsigned-test");
        let path = dir.join("unsigned.rvf");
        cm.save_to_rvf(&path).unwrap();

        let restored = ChainManager::load_from_rvf(&path, 1000).unwrap();
        assert_eq!(restored.len(), cm.len());
        let result = restored.verify_integrity();
        assert!(result.valid);

        // verify_rvf_signature should error (no footer present).
        let key = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
        let result = ChainManager::verify_rvf_signature(&path, &key.verifying_key());
        assert!(result.is_err(), "no signature footer should yield error");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_or_create_key_roundtrip() {
        let dir = std::env::temp_dir().join("clawft-key-test");
        let key_path = dir.join("test-chain.key");
        let _ = std::fs::remove_dir_all(&dir);

        // First call: generates a new key.
        let key1 = ChainManager::load_or_create_key(&key_path).unwrap();
        assert!(key_path.exists());

        // Second call: loads the same key.
        let key2 = ChainManager::load_or_create_key(&key_path).unwrap();
        assert_eq!(key1.to_bytes(), key2.to_bytes());

        // Clean up.
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn witness_chain_created_on_append() {
        let cm = ChainManager::new(0, 1000);
        assert_eq!(cm.witness_count(), 1); // genesis
        cm.append("kernel", "boot.init", None);
        assert_eq!(cm.witness_count(), 2);
        cm.append("tree", "bootstrap", Some(serde_json::json!({"n": 8})));
        assert_eq!(cm.witness_count(), 3);

        // Verify the witness chain is internally consistent.
        let count = cm.verify_witness().unwrap();
        assert_eq!(count, 3);
    }

    #[test]
    fn witness_chain_persists_in_rvf() {
        let cm = ChainManager::new(0, 1000);
        cm.append("kernel", "boot.init", None);
        cm.append("tree", "bootstrap", Some(serde_json::json!({"n": 8})));
        cm.append("kernel", "boot.ready", None);

        let original_witness_count = cm.witness_count();
        assert_eq!(original_witness_count, 4); // genesis + 3

        let dir = std::env::temp_dir().join("clawft-chain-witness-test");
        let path = dir.join("witness.rvf");
        cm.save_to_rvf(&path).unwrap();

        // Load and verify witness chain was restored.
        let restored = ChainManager::load_from_rvf(&path, 1000).unwrap();
        assert_eq!(restored.witness_count(), original_witness_count);

        // Verify the restored witness chain is valid.
        let count = restored.verify_witness().unwrap();
        assert_eq!(count, original_witness_count);

        // Verify that witness action_hashes match event hashes.
        let events = restored.tail(0);
        let chain = restored.inner.lock().unwrap();
        for (event, witness) in events.iter().zip(chain.witness_entries.iter()) {
            assert_eq!(
                witness.action_hash, event.hash,
                "witness action_hash should match event hash for seq {}",
                event.sequence,
            );
        }

        // Clean up.
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn witness_chain_continues_after_restore() {
        let cm = ChainManager::new(0, 1000);
        cm.append("kernel", "boot", None);

        let dir = std::env::temp_dir().join("clawft-chain-witness-continue");
        let path = dir.join("continue.rvf");
        cm.save_to_rvf(&path).unwrap();

        let restored = ChainManager::load_from_rvf(&path, 1000).unwrap();
        assert_eq!(restored.witness_count(), 2); // genesis + 1

        // Append new events — witness chain should grow.
        restored.append("test", "after.restore", None);
        assert_eq!(restored.witness_count(), 3);

        // Verify the extended witness chain.
        let count = restored.verify_witness().unwrap();
        assert_eq!(count, 3);

        // Clean up.
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn record_witness_bundle_creates_chain_event() {
        use rvf_runtime::{GovernancePolicy, WitnessBuilder};
        use rvf_types::witness::TaskOutcome;

        let cm = ChainManager::new(0, 1000);
        let initial_len = cm.len();

        let policy = GovernancePolicy::autonomous();
        let builder = WitnessBuilder::new([0xAA; 16], policy)
            .with_spec(b"fix auth bug")
            .with_outcome(TaskOutcome::Solved);
        let (bundle, header) = builder.build().unwrap();

        let event = cm.record_witness_bundle(&bundle, &header, 0, 0);
        assert_eq!(event.source, "witness");
        assert_eq!(event.kind, "witness.bundle");
        assert_eq!(cm.len(), initial_len + 1);

        // Verify payload contains expected fields.
        let payload = event.payload.unwrap();
        assert_eq!(payload["outcome"], TaskOutcome::Solved as u8);
        assert!(!payload["bundle"].as_str().unwrap().is_empty());
        assert_eq!(payload["policy_violations"], 0);
    }

    #[test]
    fn aggregate_scorecard_from_witness_bundles() {
        use rvf_runtime::{GovernancePolicy, WitnessBuilder};
        use rvf_types::witness::TaskOutcome;

        let cm = ChainManager::new(0, 1000);
        let policy = GovernancePolicy::autonomous();

        // Record 3 witness bundles: 2 solved, 1 failed.
        let b1 = WitnessBuilder::new([0x01; 16], policy.clone())
            .with_spec(b"task 1")
            .with_outcome(TaskOutcome::Solved);
        let (bytes1, header1) = b1.build().unwrap();
        cm.record_witness_bundle(&bytes1, &header1, 0, 0);

        let b2 = WitnessBuilder::new([0x02; 16], policy.clone())
            .with_spec(b"task 2")
            .with_outcome(TaskOutcome::Failed);
        let (bytes2, header2) = b2.build().unwrap();
        cm.record_witness_bundle(&bytes2, &header2, 1, 0);

        let b3 = WitnessBuilder::new([0x03; 16], policy.clone())
            .with_spec(b"task 3")
            .with_diff(b"diff")
            .with_test_log(b"pass")
            .with_outcome(TaskOutcome::Solved);
        let (bytes3, header3) = b3.build().unwrap();
        cm.record_witness_bundle(&bytes3, &header3, 0, 1);

        let card = cm.aggregate_scorecard(0);
        assert_eq!(card.total_tasks, 3);
        assert_eq!(card.solved, 2);
        assert_eq!(card.failed, 1);
        assert_eq!(card.policy_violations, 1);
        assert_eq!(card.rollback_count, 1);
        assert!((card.solve_rate - 0.6667).abs() < 0.01);
    }

    #[test]
    fn aggregate_scorecard_empty_when_no_bundles() {
        let cm = ChainManager::new(0, 1000);
        cm.append("kernel", "boot", None);

        let card = cm.aggregate_scorecard(0);
        assert_eq!(card.total_tasks, 0);
        assert_eq!(card.solve_rate, 0.0);
    }

    #[test]
    fn witness_bundle_with_tool_calls() {
        use rvf_runtime::{GovernancePolicy, WitnessBuilder};
        use rvf_types::witness::{PolicyCheck, TaskOutcome, ToolCallEntry};

        let cm = ChainManager::new(0, 1000);
        let policy = GovernancePolicy::autonomous();

        let mut builder = WitnessBuilder::new([0x10; 16], policy)
            .with_spec(b"add feature")
            .with_outcome(TaskOutcome::Solved);

        builder.record_tool_call(ToolCallEntry {
            action: b"Read".to_vec(),
            args_hash: [0x11; 8],
            result_hash: [0x22; 8],
            latency_ms: 50,
            cost_microdollars: 100,
            tokens: 500,
            policy_check: PolicyCheck::Allowed,
        });
        builder.record_tool_call(ToolCallEntry {
            action: b"Edit".to_vec(),
            args_hash: [0x33; 8],
            result_hash: [0x44; 8],
            latency_ms: 100,
            cost_microdollars: 200,
            tokens: 1000,
            policy_check: PolicyCheck::Allowed,
        });

        let (bundle, header) = builder.build().unwrap();
        assert_eq!(header.tool_call_count, 2);
        assert_eq!(header.total_cost_microdollars, 300);

        let event = cm.record_witness_bundle(&bundle, &header, 0, 0);
        let payload = event.payload.unwrap();
        assert_eq!(payload["tool_call_count"], 2);
        assert_eq!(payload["total_cost_microdollars"], 300);

        // Scorecard should aggregate this bundle.
        let card = cm.aggregate_scorecard(0);
        assert_eq!(card.total_tasks, 1);
        assert_eq!(card.solved, 1);
    }

    #[test]
    fn record_lineage_creates_chain_event() {
        use rvf_types::DerivationType;

        let cm = ChainManager::new(0, 1000);
        let initial_len = cm.len();

        let event = cm.record_lineage(
            [0x01; 16], // child_id
            [0x00; 16], // parent_id (root)
            [0x00; 32], // parent_hash (root)
            DerivationType::Clone,
            0,
            "root agent",
        );

        assert_eq!(event.source, "lineage");
        assert_eq!(event.kind, "lineage.derivation");
        assert_eq!(cm.len(), initial_len + 1);

        let payload = event.payload.unwrap();
        assert_eq!(payload["derivation_type"], DerivationType::Clone as u8);
        assert_eq!(payload["description"], "root agent");
        assert!(!payload["record_hex"].as_str().unwrap().is_empty());
    }

    #[test]
    fn record_lineage_parent_child() {
        use rvf_types::DerivationType;

        let cm = ChainManager::new(0, 1000);

        // Root agent.
        let root_event = cm.record_lineage(
            [0x01; 16],
            [0x00; 16],
            [0x00; 32],
            DerivationType::Clone,
            0,
            "root agent",
        );

        // Child agent derived from root.
        let child_event = cm.record_lineage(
            [0x02; 16],
            [0x01; 16],
            root_event.hash,
            DerivationType::Transform,
            1,
            "spawned worker",
        );

        let payload = child_event.payload.unwrap();
        assert_eq!(payload["derivation_type"], DerivationType::Transform as u8);
        assert_eq!(payload["mutation_count"], 1);

        // Verify lineage chain.
        let count = cm.verify_lineage().unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn lineage_adds_witness_entry() {
        use rvf_types::DerivationType;

        let cm = ChainManager::new(0, 1000);
        let initial_witness_count = cm.witness_count();

        cm.record_lineage(
            [0x01; 16],
            [0x00; 16],
            [0x00; 32],
            DerivationType::Clone,
            0,
            "agent",
        );

        // One extra witness entry: the lineage witness + the chain append witness.
        assert!(cm.witness_count() > initial_witness_count);
    }

    #[test]
    fn verify_lineage_empty_returns_zero() {
        let cm = ChainManager::new(0, 1000);
        let count = cm.verify_lineage().unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn lineage_record_hex_roundtrip() {
        use rvf_types::DerivationType;

        let cm = ChainManager::new(0, 1000);
        let event = cm.record_lineage(
            [0xAA; 16],
            [0xBB; 16],
            [0xCC; 32],
            DerivationType::Filter,
            42,
            "filtered set",
        );

        // Extract and decode the record_hex from the payload.
        let payload = event.payload.unwrap();
        let record_hex = payload["record_hex"].as_str().unwrap();
        let record_bytes = hex_decode(record_hex).unwrap();
        assert_eq!(record_bytes.len(), 128); // LINEAGE_RECORD_SIZE

        let record: rvf_types::LineageRecord = weftos_rvf_crypto::lineage_record_from_bytes(
            record_bytes.as_slice().try_into().unwrap(),
        )
        .unwrap();
        assert_eq!(record.file_id, [0xAA; 16]);
        assert_eq!(record.parent_id, [0xBB; 16]);
        assert_eq!(record.parent_hash, [0xCC; 32]);
        assert_eq!(record.derivation_type, DerivationType::Filter);
        assert_eq!(record.mutation_count, 42);
        assert_eq!(record.description_str(), "filtered set");
    }

    #[test]
    fn last_tree_root_hash_from_shutdown() {
        let cm = ChainManager::new(0, 1000);
        // No tree hash yet.
        assert!(cm.last_tree_root_hash().is_none());

        // Simulate a shutdown event with tree_root_hash.
        cm.append(
            "kernel",
            "shutdown",
            Some(serde_json::json!({
                "tree_root_hash": "aabb00112233445566778899",
                "chain_seq": 5,
            })),
        );

        let hash = cm.last_tree_root_hash().unwrap();
        assert_eq!(hash, "aabb00112233445566778899");
    }

    #[test]
    fn last_tree_root_hash_from_tree_checkpoint() {
        let cm = ChainManager::new(0, 1000);

        // tree.checkpoint event uses "root_hash" key.
        cm.append(
            "tree",
            "tree.checkpoint",
            Some(serde_json::json!({
                "path": "/tmp/tree.json",
                "root_hash": "deadbeef01234567890abcdef0123456",
            })),
        );

        let hash = cm.last_tree_root_hash().unwrap();
        assert_eq!(hash, "deadbeef01234567890abcdef0123456");
    }

    #[test]
    fn last_tree_root_hash_prefers_most_recent() {
        let cm = ChainManager::new(0, 1000);

        // Older event.
        cm.append(
            "kernel",
            "boot.ready",
            Some(serde_json::json!({ "tree_root_hash": "old_hash" })),
        );

        // More recent event.
        cm.append(
            "kernel",
            "shutdown",
            Some(serde_json::json!({ "tree_root_hash": "new_hash" })),
        );

        let hash = cm.last_tree_root_hash().unwrap();
        assert_eq!(hash, "new_hash");
    }

    #[test]
    fn last_tree_root_hash_ignores_non_tree_root_hash() {
        let cm = ChainManager::new(0, 1000);
        // Event with root_hash but wrong source/kind — should be ignored.
        cm.append(
            "kernel",
            "boot.init",
            Some(serde_json::json!({ "root_hash": "should_not_match" })),
        );
        assert!(cm.last_tree_root_hash().is_none());
    }

    // --- K4 G1: ChainAnchor tests ---

    #[test]
    fn mock_anchor_roundtrip() {
        let anchor = MockAnchor;
        let hash = [42u8; 32];
        let receipt = anchor.anchor(&hash).unwrap();
        assert_eq!(receipt.hash, hash);
        assert!(receipt.tx_id.starts_with("mock-"));
        assert!(anchor.verify(&receipt).unwrap());
    }

    #[test]
    fn anchor_receipt_serde() {
        let receipt = AnchorReceipt {
            hash: [1u8; 32],
            tx_id: "test-tx".into(),
            anchored_at: Utc::now(),
        };
        let json = serde_json::to_string(&receipt).unwrap();
        let restored: AnchorReceipt = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.hash, receipt.hash);
        assert_eq!(restored.tx_id, "test-tx");
    }

    #[test]
    fn ml_dsa_key_set_and_has() {
        let mut cm = ChainManager::new(0, 1000);
        assert!(!cm.has_dual_signing());

        // Ed25519 only
        let ed_key = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
        cm.set_signing_key(ed_key);
        assert!(cm.has_signing_key());
        assert!(!cm.has_dual_signing());

        // Add ML-DSA key
        let (ml_key, _) = weftos_rvf_crypto::MlDsa65Key::generate(b"test-seed");
        cm.set_ml_dsa_key(ml_key);
        assert!(cm.has_dual_signing());
    }

    #[test]
    fn dual_sign_rvf_roundtrip() {
        use ed25519_dalek::SigningKey;
        use rand::rngs::OsRng;

        let ed_key = SigningKey::generate(&mut OsRng);
        let (ml_key, ml_vk) = weftos_rvf_crypto::MlDsa65Key::generate(&ed_key.to_bytes());

        let mut cm = ChainManager::new(0, 1000).with_signing_key(ed_key.clone());
        cm.set_ml_dsa_key(ml_key);
        assert!(cm.has_dual_signing());

        cm.append("kernel", "boot.init", None);
        cm.append("tree", "bootstrap", Some(serde_json::json!({"nodes": 4})));
        cm.append("kernel", "boot.ready", None);

        let original_seq = cm.sequence();
        let original_hash = cm.last_hash();
        let original_len = cm.len();

        let dir = std::env::temp_dir().join("clawft-chain-dual-sign-test");
        let path = dir.join("dual-signed.rvf");
        cm.save_to_rvf(&path).unwrap();

        // Load the dual-signed file (should work without keys).
        let restored = ChainManager::load_from_rvf(&path, 1000).unwrap();
        assert_eq!(restored.sequence(), original_seq);
        assert_eq!(restored.last_hash(), original_hash);
        assert_eq!(restored.len(), original_len);

        let result = restored.verify_integrity();
        assert!(result.valid, "integrity errors: {:?}", result.errors);

        // Verify Ed25519 signature alone.
        let ed_pubkey = ed_key.verifying_key();
        let ed_valid = ChainManager::verify_rvf_signature(&path, &ed_pubkey).unwrap();
        assert!(ed_valid, "Ed25519 signature should be valid");

        // Verify dual signatures.
        let dual_valid =
            ChainManager::verify_rvf_dual_signature(&path, &ed_pubkey, &ml_vk).unwrap();
        assert!(dual_valid, "dual signature should be valid");

        // Wrong ML-DSA key should fail dual verification.
        let (_, wrong_ml_vk) = weftos_rvf_crypto::MlDsa65Key::generate(b"wrong-seed");
        let dual_wrong =
            ChainManager::verify_rvf_dual_signature(&path, &ed_pubkey, &wrong_ml_vk).unwrap();
        assert!(
            !dual_wrong,
            "dual signature should fail with wrong ML-DSA key"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dual_signed_checkpoint_verifies() {
        use ed25519_dalek::SigningKey;
        use rand::rngs::OsRng;

        let ed_key = SigningKey::generate(&mut OsRng);
        let (ml_key, ml_vk) = weftos_rvf_crypto::MlDsa65Key::generate(b"checkpoint-test");

        let mut cm = ChainManager::new(0, 1000).with_signing_key(ed_key.clone());
        cm.set_ml_dsa_key(ml_key);
        cm.append("kernel", "boot", None);

        let dir = std::env::temp_dir().join("clawft-chain-dual-cp-test");
        let path = dir.join("dual-cp.rvf");
        cm.save_to_rvf(&path).unwrap();

        // Read file and verify it has two footers.
        let data = std::fs::read(&path).unwrap();
        let mut offset = 0;
        while offset + SEGMENT_HEADER_SIZE <= data.len() {
            match read_segment(&data[offset..]) {
                Ok((seg_header, _)) => {
                    let padded = calculate_padded_size(
                        SEGMENT_HEADER_SIZE,
                        seg_header.payload_length as usize,
                    );
                    offset += padded;
                }
                Err(_) => break,
            }
        }
        // First footer: Ed25519
        let ed_footer = decode_signature_footer(&data[offset..]).unwrap();
        assert_eq!(ed_footer.sig_algo, 0); // Ed25519
        assert_eq!(ed_footer.sig_length, 64);

        // Second footer: ML-DSA-65
        let ml_offset = offset + ed_footer.footer_length as usize;
        let ml_footer = decode_signature_footer(&data[ml_offset..]).unwrap();
        assert_eq!(ml_footer.sig_algo, 1); // ML-DSA-65
        assert_eq!(ml_footer.sig_length, 3309);

        // Verify both independently.
        let ed_pubkey = ed_key.verifying_key();
        let dual_valid =
            ChainManager::verify_rvf_dual_signature(&path, &ed_pubkey, &ml_vk).unwrap();
        assert!(dual_valid);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn k6_cryptographic_filesystem_creates_and_retrieves() {
        // This test demonstrates the "cryptographic filesystem" gate:
        // chain entries are hash-linked (SHAKE-256) and form a tamper-evident
        // append-only log with retrieval — proving cryptographic filesystem semantics.

        let cm = ChainManager::new(0, 1000);

        // Create a filesystem-style entry with content metadata
        let entry = cm.append(
            "fs",
            "file.create",
            Some(serde_json::json!({
                "path": "/data/config.json",
                "content_hash": "abc123def456",
                "size": 1024,
            })),
        );

        // Verify hash linkage (cryptographic integrity)
        assert!(
            !entry.hash.iter().all(|&b| b == 0),
            "entry hash must be non-zero"
        );
        assert!(
            !entry.prev_hash.iter().all(|&b| b == 0),
            "prev_hash must link to genesis (non-zero)"
        );
        assert!(
            !entry.payload_hash.iter().all(|&b| b == 0),
            "payload_hash must be non-zero when payload present"
        );

        // Retrieve entry via tail_from (sequence > genesis)
        let events = cm.tail_from(0);
        assert!(
            !events.is_empty(),
            "must retrieve at least the created entry"
        );
        let found = events.iter().find(|e| e.kind == "file.create");
        assert!(found.is_some(), "file.create entry must be retrievable");

        let retrieved = found.unwrap();
        assert_eq!(retrieved.hash, entry.hash);
        let payload = retrieved.payload.as_ref().unwrap();
        assert_eq!(payload["path"], "/data/config.json");
        assert_eq!(payload["content_hash"], "abc123def456");
        assert_eq!(payload["size"], 1024);

        // Verify chain integrity covers this entry
        let result = cm.verify_integrity();
        assert!(result.valid, "chain integrity must hold after fs entry");
    }

    // ── Cross-node dual signature tests ──────────────────────────

    #[test]
    fn dual_signature_ed25519_only() {
        use ed25519_dalek::SigningKey;
        use rand::rngs::OsRng;

        let ed_key = SigningKey::generate(&mut OsRng);
        let cm = ChainManager::new(0, 1000).with_signing_key(ed_key.clone());
        // No ML-DSA key set — dual_sign should still succeed with Ed25519 only.

        let data = b"cross-node chain event payload";
        let sig = cm.dual_sign(data).expect("should produce a signature");

        assert_eq!(sig.ed25519.len(), 64, "Ed25519 signature must be 64 bytes");
        assert!(
            sig.ml_dsa65.is_none(),
            "ML-DSA-65 should be absent without key"
        );
    }

    #[test]
    fn dual_signature_both_algorithms() {
        use ed25519_dalek::SigningKey;
        use rand::rngs::OsRng;

        let ed_key = SigningKey::generate(&mut OsRng);
        let (ml_key, _ml_vk) = weftos_rvf_crypto::MlDsa65Key::generate(b"dual-sig-test");

        let mut cm = ChainManager::new(0, 1000).with_signing_key(ed_key.clone());
        cm.set_ml_dsa_key(ml_key);
        assert!(cm.has_dual_signing());

        let data = b"cross-node chain event with PQ protection";
        let sig = cm.dual_sign(data).expect("should produce dual signature");

        assert_eq!(sig.ed25519.len(), 64);
        let ml = sig.ml_dsa65.as_ref().expect("ML-DSA-65 should be present");
        assert_eq!(ml.len(), 3309, "ML-DSA-65 signature must be 3309 bytes");
    }

    #[test]
    fn verify_dual_signature_ed25519_valid() {
        use ed25519_dalek::SigningKey;
        use rand::rngs::OsRng;

        let ed_key = SigningKey::generate(&mut OsRng);
        let cm = ChainManager::new(0, 1000).with_signing_key(ed_key.clone());

        let data = b"verify-ed25519-only";
        let sig = cm.dual_sign(data).unwrap();
        let ed_pub = ed_key.verifying_key();

        assert!(
            ChainManager::verify_dual_signature(data, &sig, &ed_pub, None),
            "Ed25519-only dual signature should verify"
        );
    }

    #[test]
    fn verify_dual_signature_both_valid() {
        use ed25519_dalek::SigningKey;
        use rand::rngs::OsRng;

        let ed_key = SigningKey::generate(&mut OsRng);
        let (ml_key, ml_vk) = weftos_rvf_crypto::MlDsa65Key::generate(b"verify-both");

        let mut cm = ChainManager::new(0, 1000).with_signing_key(ed_key.clone());
        cm.set_ml_dsa_key(ml_key);

        let data = b"verify-both-algorithms";
        let sig = cm.dual_sign(data).unwrap();
        let ed_pub = ed_key.verifying_key();

        assert!(
            ChainManager::verify_dual_signature(data, &sig, &ed_pub, Some(&ml_vk)),
            "dual signature with both algorithms should verify"
        );
    }

    #[test]
    fn verify_dual_signature_rejects_tampered() {
        use ed25519_dalek::SigningKey;
        use rand::rngs::OsRng;

        let ed_key = SigningKey::generate(&mut OsRng);
        let (ml_key, ml_vk) = weftos_rvf_crypto::MlDsa65Key::generate(b"tamper-test");

        let mut cm = ChainManager::new(0, 1000).with_signing_key(ed_key.clone());
        cm.set_ml_dsa_key(ml_key);

        let data = b"original data";
        let sig = cm.dual_sign(data).unwrap();
        let ed_pub = ed_key.verifying_key();

        // Tampered data should fail Ed25519 verification.
        let tampered = b"tampered data";
        assert!(
            !ChainManager::verify_dual_signature(tampered, &sig, &ed_pub, Some(&ml_vk)),
            "tampered data must fail verification"
        );

        // Tampered ML-DSA-65 signature should fail.
        let mut bad_sig = sig.clone();
        if let Some(ref mut ml) = bad_sig.ml_dsa65 {
            ml[0] ^= 0xFF;
        }
        assert!(
            !ChainManager::verify_dual_signature(data, &bad_sig, &ed_pub, Some(&ml_vk)),
            "tampered ML-DSA-65 signature must fail verification"
        );
    }

    // ── Sprint 09a: serde roundtrip tests for chain types ────────

    #[test]
    fn chain_event_serde_roundtrip() {
        let cm = ChainManager::new(0, 1000);
        cm.append(
            "test",
            "agent.spawn",
            Some(serde_json::json!({"name": "test-agent"})),
        );
        let events = cm.tail(1);
        let event = &events[0];

        let json = serde_json::to_string(event).unwrap();
        let restored: ChainEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.sequence, event.sequence);
        assert_eq!(restored.source, "test");
        assert_eq!(restored.kind, "agent.spawn");
        assert!(restored.payload.is_some());
    }

    #[test]
    fn chain_event_without_payload_roundtrip() {
        let cm = ChainManager::new(0, 1000);
        cm.append("kernel", "boot.complete", None);
        let events = cm.tail(1);
        let event = &events[0];

        let json = serde_json::to_string(event).unwrap();
        let restored: ChainEvent = serde_json::from_str(&json).unwrap();
        assert!(restored.payload.is_none());
        assert_eq!(restored.kind, "boot.complete");
    }

    #[test]
    fn chain_checkpoint_serde_roundtrip() {
        let cm = ChainManager::new(0, 1000);
        cm.append("test", "event.1", None);
        cm.append("test", "event.2", None);
        let cp = cm.checkpoint();

        let json = serde_json::to_string(&cp).unwrap();
        let restored: ChainCheckpoint = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.chain_id, cp.chain_id);
        assert_eq!(restored.sequence, cp.sequence);
        assert_eq!(restored.last_hash, cp.last_hash);
    }

    #[test]
    fn chain_verify_result_serde_roundtrip_valid() {
        let result = ChainVerifyResult {
            valid: true,
            event_count: 10,
            errors: vec![],
            signature_verified: Some(true),
        };
        let json = serde_json::to_string(&result).unwrap();
        let restored: ChainVerifyResult = serde_json::from_str(&json).unwrap();
        assert!(restored.valid);
        assert_eq!(restored.event_count, 10);
        assert!(restored.errors.is_empty());
        assert_eq!(restored.signature_verified, Some(true));
    }

    #[test]
    fn chain_verify_result_serde_roundtrip_invalid() {
        let result = ChainVerifyResult {
            valid: false,
            event_count: 5,
            errors: vec!["hash mismatch at seq 3".into()],
            signature_verified: None,
        };
        let json = serde_json::to_string(&result).unwrap();
        let restored: ChainVerifyResult = serde_json::from_str(&json).unwrap();
        assert!(!restored.valid);
        assert_eq!(restored.errors.len(), 1);
        assert!(restored.signature_verified.is_none());
    }

    #[test]
    fn chain_status_serde_roundtrip() {
        let cm = ChainManager::new(42, 1000);
        cm.append("test", "event.1", None);
        cm.append("test", "event.2", None);
        let status = cm.status();

        let json = serde_json::to_string(&status).unwrap();
        let restored: ChainStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.chain_id, 42);
        assert_eq!(restored.sequence, status.sequence);
        assert_eq!(restored.event_count, status.event_count);
    }

    #[test]
    fn dual_signature_serde_roundtrip() {
        let sig = DualSignature {
            ed25519: vec![0xCA, 0xFE, 0xBA, 0xBE],
            ml_dsa65: Some(vec![0xDE, 0xAD]),
        };
        let json = serde_json::to_string(&sig).unwrap();
        let restored: DualSignature = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.ed25519, vec![0xCA, 0xFE, 0xBA, 0xBE]);
        assert_eq!(restored.ml_dsa65.unwrap(), vec![0xDE, 0xAD]);
    }

    #[test]
    fn dual_signature_without_ml_dsa_roundtrip() {
        let sig = DualSignature {
            ed25519: vec![0x01, 0x02],
            ml_dsa65: None,
        };
        let json = serde_json::to_string(&sig).unwrap();
        let restored: DualSignature = serde_json::from_str(&json).unwrap();
        assert!(restored.ml_dsa65.is_none());
    }

    #[test]
    fn anchor_receipt_serde_roundtrip() {
        let receipt = AnchorReceipt {
            hash: [0xABu8; 32],
            tx_id: "tx-deadbeef".into(),
            anchored_at: chrono::Utc::now(),
        };
        let json = serde_json::to_string(&receipt).unwrap();
        let restored: AnchorReceipt = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.hash, [0xABu8; 32]);
        assert_eq!(restored.tx_id, "tx-deadbeef");
    }

    #[test]
    fn chain_genesis_event_hash_is_nonzero() {
        let cm = ChainManager::new(0, 1000);
        let events = cm.tail(1);
        assert_eq!(events[0].kind, "genesis");
        assert_ne!(events[0].hash, [0u8; 32]);
    }

    #[test]
    fn chain_genesis_prev_hash_is_zero() {
        let cm = ChainManager::new(0, 1000);
        let events = cm.tail(1);
        assert_eq!(events[0].prev_hash, [0u8; 32]);
    }

    // ── ChainLoggable trait tests ────────────────────────────────

    #[test]
    fn restart_event_loggable() {
        let cm = ChainManager::new(0, 100);
        let initial = cm.len();

        let event = RestartEvent {
            agent_id: "agent-coder".into(),
            old_pid: 5,
            new_pid: 12,
            exit_code: 1,
            strategy: "OneForOne".into(),
            backoff_ms: 200,
            timestamp: Utc::now(),
        };

        assert_eq!(event.chain_event_source(), "supervisor");
        assert_eq!(event.chain_event_kind(), "supervisor.restart");

        let chain_event = cm.append_loggable(&event);
        assert_eq!(cm.len(), initial + 1);
        assert_eq!(chain_event.source, "supervisor");
        assert_eq!(chain_event.kind, "supervisor.restart");

        let payload = chain_event.payload.unwrap();
        assert_eq!(payload["agent_id"], "agent-coder");
        assert_eq!(payload["old_pid"], 5);
        assert_eq!(payload["new_pid"], 12);
        assert_eq!(payload["exit_code"], 1);
    }

    #[test]
    fn governance_decision_event_loggable() {
        let cm = ChainManager::new(0, 100);

        let event = GovernanceDecisionEvent {
            agent_id: "agent-1".into(),
            action: "tool.exec".into(),
            decision: "Deny".into(),
            effect_magnitude: 0.85,
            threshold_exceeded: true,
            evaluated_rules: vec!["security-check".into()],
            timestamp: Utc::now(),
            principal: None,
        };

        assert_eq!(event.chain_event_source(), "governance");
        assert_eq!(event.chain_event_kind(), "governance.deny");

        let chain_event = cm.append_loggable(&event);
        assert_eq!(chain_event.kind, "governance.deny");

        let payload = chain_event.payload.unwrap();
        assert_eq!(payload["agent_id"], "agent-1");
        assert_eq!(payload["action"], "tool.exec");
        assert!(payload["threshold_exceeded"].as_bool().unwrap());

        // Test other decision kinds map to correct event kinds
        let make_event = |decision: &str| GovernanceDecisionEvent {
            agent_id: "a".into(),
            action: "a".into(),
            decision: decision.into(),
            effect_magnitude: 0.0,
            threshold_exceeded: false,
            evaluated_rules: vec![],
            timestamp: Utc::now(),
            principal: None,
        };

        assert_eq!(make_event("Permit").chain_event_kind(), "governance.permit");
        assert_eq!(
            make_event("PermitWithWarning").chain_event_kind(),
            "governance.warn"
        );
        assert_eq!(
            make_event("EscalateToHuman").chain_event_kind(),
            "governance.defer"
        );
        assert_eq!(make_event("Deny").chain_event_kind(), "governance.deny");
    }

    #[test]
    fn ipc_dead_letter_event_loggable() {
        let cm = ChainManager::new(0, 100);

        let event = IpcDeadLetterEvent {
            message_id: "msg-abc".into(),
            from_pid: 3,
            target: "Process(99)".into(),
            payload_type: "text".into(),
            reason: "target_not_found(pid=99)".into(),
            timestamp: Utc::now(),
        };

        assert_eq!(event.chain_event_source(), "ipc");
        assert_eq!(event.chain_event_kind(), "ipc.dead_letter");

        let chain_event = cm.append_loggable(&event);
        assert_eq!(chain_event.source, "ipc");
        assert_eq!(chain_event.kind, "ipc.dead_letter");

        let payload = chain_event.payload.unwrap();
        assert_eq!(payload["message_id"], "msg-abc");
        assert_eq!(payload["from_pid"], 3);
        assert_eq!(payload["reason"], "target_not_found(pid=99)");
    }

    #[test]
    fn append_loggable_links_hashes() {
        let cm = ChainManager::new(0, 100);
        let hash_before = cm.last_hash();

        let event = RestartEvent {
            agent_id: "test".into(),
            old_pid: 1,
            new_pid: 2,
            exit_code: 1,
            strategy: "OneForOne".into(),
            backoff_ms: 100,
            timestamp: Utc::now(),
        };

        let chain_event = cm.append_loggable(&event);
        assert_eq!(chain_event.prev_hash, hash_before);
        assert_ne!(chain_event.hash, [0u8; 32]);
    }

    #[test]
    fn generate_attestation_without_key_returns_none() {
        let cm = ChainManager::new(0, 1000);
        // No signing key set
        assert!(cm.generate_attestation(10, 5, "abc123").is_none());
    }

    #[test]
    fn generate_attestation_with_key() {
        use rand::rngs::OsRng;
        let key = SigningKey::generate(&mut OsRng);
        let cm = ChainManager::new(0, 1000).with_signing_key(key.clone());

        // Append some events
        cm.append("test", "event.one", None);
        cm.append("test", "event.two", None);

        let att = cm.generate_attestation(42, 7, "deadbeef").unwrap();

        // Verify fields
        assert!(!att.device_id.is_empty());
        assert_eq!(att.epoch, 7);
        assert_eq!(att.vector_count, 42);
        assert_eq!(att.content_hash, "deadbeef");
        assert_eq!(att.chain_depth, 3); // genesis + 2
        assert!(!att.chain_head.is_empty());
        assert_eq!(att.chain_head.len(), 64); // 32 bytes hex
        assert!(att.timestamp > 0);
        assert_eq!(att.signature.len(), 64); // Ed25519 signature

        // Verify the signature
        use ed25519_dalek::Verifier;
        let canonical = format!(
            "custody-attestation:v1\n\
             device_id:{}\n\
             epoch:{}\n\
             chain_head:{}\n\
             chain_depth:{}\n\
             vector_count:{}\n\
             content_hash:{}\n\
             timestamp:{}",
            att.device_id,
            att.epoch,
            att.chain_head,
            att.chain_depth,
            att.vector_count,
            att.content_hash,
            att.timestamp,
        );
        let sig =
            ed25519_dalek::Signature::from_bytes(att.signature.as_slice().try_into().unwrap());
        assert!(
            key.verifying_key()
                .verify(canonical.as_bytes(), &sig)
                .is_ok()
        );
    }

    #[test]
    fn custody_attestation_serde_roundtrip() {
        let att = CustodyAttestation {
            device_id: "aabbccdd".into(),
            epoch: 5,
            chain_head: "ff".repeat(32),
            chain_depth: 100,
            vector_count: 1000,
            content_hash: "00".repeat(32),
            timestamp: 1234567890,
            signature: vec![0u8; 64],
        };
        let json = serde_json::to_string(&att).unwrap();
        let restored: CustodyAttestation = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.device_id, "aabbccdd");
        assert_eq!(restored.epoch, 5);
        assert_eq!(restored.chain_depth, 100);
        assert_eq!(restored.vector_count, 1000);
        assert_eq!(restored.signature.len(), 64);
    }
}
