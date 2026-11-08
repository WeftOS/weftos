//! Compacting the gate's chain history (ADR-126 "Chain compaction", S3's design applied to S5).
//!
//! The gate's journal grows with every change. A [`GateSnapshot`] is the folded gate at
//! `through_seq` (the chain sequence of the last event folded in), plus `events_head`, a hash
//! chain over every event folded so far, and `event_count`. The daemon appends it to the chain
//! as one signed `store.snapshot` event; a restart then starts from the newest snapshot and
//! replays only the events after it.
//!
//! Checks, in the order [`crate::client_store_state::JournalState`] applies them:
//! 1. `version` and `scope` (`gate/<account>/<mesh>`) match: never mixed across accounts.
//! 2. `state_hash` is the SHA-256 of the canonical state: a hand-edited table fails.
//! 3. The Ed25519 signature over `SIG_DOMAIN` + the canonical snapshot verifies under a key the
//!    daemon already trusts for chain events (`verify_strict`).
//! 4. If the chain still holds every event up to `through_seq` (their count equals
//!    `event_count`), the history is re-folded and `events_head` and the state must match, so
//!    the signer is not trusted for the past. If part of the history was pruned, the signer is
//!    trusted for it, and the pruned segment must have been archived first.
//!
//! What differs from the grant-ledger spike: the gate's events are not ordered by a sequence
//! number of their own, so `through_seq` is the chain's; whether an event applies depends on the
//! clock (expiry), so `events_head` covers every well-formed event for the scope, applied or
//! not, and an install event records the second it was checked at (`GateEvent::XmeshInstall`).
//! Cross-mesh grants past their end at `taken_at` are left out of the snapshot.

use ed25519_dalek::{Signature, Signer, SigningKey};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::store_account::{GateEvent, StoreGate};
use crate::weftos_uri::Authority;

pub const SNAPSHOT_VERSION: u32 = 1;
/// Chain event kind of a published snapshot.
pub const SNAPSHOT_KIND: &str = "store.snapshot";
const GENESIS: &str = "weftos/gate-state/genesis/v1";
const SIG_DOMAIN: &str = "weftos/gate-state/snapshot/v1\n";
/// The chain's project-key domain prefix; this signer must never sign it (see the kernel chain).
const PROJECT_DOMAIN_PREFIX: &[u8] = b"weftos-project-";

fn sha256_hex(b: &[u8]) -> String {
    hex::encode(Sha256::digest(b))
}

/// The head before any event.
pub fn genesis_head() -> String {
    sha256_hex(GENESIS.as_bytes())
}

/// Next head after folding an event's canonical bytes: `SHA256(head || "\n" || bytes)`.
pub fn head_extend(head: &str, canonical_event: &[u8]) -> String {
    let mut buf = head.as_bytes().to_vec();
    buf.push(b'\n');
    buf.extend_from_slice(canonical_event);
    sha256_hex(&buf)
}

/// Compact JSON with object keys sorted at every level, so the bytes do not depend on how a
/// value was built or read back.
pub fn canonical(v: &Value) -> String {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<_> = m.keys().collect();
            keys.sort();
            let parts: Vec<String> =
                keys.into_iter().map(|k| format!("{}:{}", serde_json::to_string(k).unwrap_or_default(), canonical(&m[k]))).collect();
            format!("{{{}}}", parts.join(","))
        }
        Value::Array(a) => format!("[{}]", a.iter().map(canonical).collect::<Vec<_>>().join(",")),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// The bytes one gate event adds to `events_head`.
pub fn event_bytes(kind: &str, ev: &Value) -> Vec<u8> {
    canonical(&json!({"kind": kind, "ev": ev})).into_bytes()
}

/// The scope a gate snapshot is for.
pub fn scope_of(account: &str, mesh: &Authority) -> String {
    format!("gate/{account}/{}", hex::encode(mesh.0))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateSnapshot {
    pub version: u32,
    pub scope: String,
    /// Chain sequence of the last scope event folded in.
    pub through_seq: u64,
    /// How many scope events are folded in.
    pub event_count: u64,
    pub events_head: String,
    /// Unix second the state was taken at; grants past their end then are left out.
    pub taken_at: u64,
    pub state: Value,
    /// SHA-256 over the canonical JSON of `state`.
    pub state_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedGateSnapshot {
    pub snapshot: GateSnapshot,
    /// The signer's key id.
    pub signer: String,
    /// Hex.
    pub signature: String,
}

/// Signs and verifies snapshots with the key the daemon uses for its chain.
pub trait SnapshotKey: Send + Sync {
    /// `(signer id, signature)`, or `None` if this key will not sign these bytes.
    fn sign(&self, msg: &[u8]) -> Option<(String, Vec<u8>)>;
    fn verify(&self, signer: &str, msg: &[u8], sig: &[u8]) -> bool;
}

/// An Ed25519 key as a [`SnapshotKey`]; verifies only its own signatures, with `verify_strict`.
pub struct Ed25519Key(pub SigningKey);

impl SnapshotKey for Ed25519Key {
    fn sign(&self, msg: &[u8]) -> Option<(String, Vec<u8>)> {
        if msg.starts_with(PROJECT_DOMAIN_PREFIX) {
            return None;
        }
        Some((hex::encode(self.0.verifying_key().to_bytes()), self.0.sign(msg).to_bytes().to_vec()))
    }
    fn verify(&self, signer: &str, msg: &[u8], sig: &[u8]) -> bool {
        let vk = self.0.verifying_key();
        signer == hex::encode(vk.to_bytes()) && Signature::from_slice(sig).is_ok_and(|s| vk.verify_strict(msg, &s).is_ok())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SnapshotError {
    #[error("snapshot version {0} is not supported")]
    Version(u32),
    #[error("snapshot is for scope {got:?}, expected {want:?}")]
    Scope { want: String, got: String },
    #[error("state does not match its hash")]
    StateHash,
    #[error("snapshot signature rejected")]
    BadSignature,
    #[error("no snapshot key is configured")]
    NoKey,
    #[error("the history does not reproduce the snapshot ({0})")]
    HistoryMismatch(&'static str),
}

/// The bytes a signer signs: a domain, then the snapshot's canonical JSON.
pub fn signing_bytes(s: &GateSnapshot) -> Vec<u8> {
    let mut v = SIG_DOMAIN.as_bytes().to_vec();
    v.extend_from_slice(canonical(&serde_json::to_value(s).unwrap_or(Value::Null)).as_bytes());
    v
}

/// The hash a snapshot carries for `state`.
pub fn state_hash(state: &Value) -> String {
    sha256_hex(canonical(state).as_bytes())
}

/// Checks 1 to 3. `key` is `None` when the daemon has no chain key.
pub fn verify_snapshot(signed: &SignedGateSnapshot, scope: &str, key: Option<&dyn SnapshotKey>) -> Result<(), SnapshotError> {
    let s = &signed.snapshot;
    if s.version != SNAPSHOT_VERSION {
        return Err(SnapshotError::Version(s.version));
    }
    if s.scope != scope {
        return Err(SnapshotError::Scope { want: scope.into(), got: s.scope.clone() });
    }
    if s.state_hash != state_hash(&s.state) {
        return Err(SnapshotError::StateHash);
    }
    let key = key.ok_or(SnapshotError::NoKey)?;
    let sig = hex::decode(&signed.signature).map_err(|_| SnapshotError::BadSignature)?;
    if !key.verify(&signed.signer, &signing_bytes(s), &sig) {
        return Err(SnapshotError::BadSignature);
    }
    Ok(())
}

/// Check 4: re-fold `history` (every scope event up to `through_seq`, as `(kind, ev)`) and
/// compare the head, the count and the state.
pub fn verify_against_history(
    snap: &GateSnapshot, mesh: Authority, account: &str, history: &[(&str, &Value)],
) -> Result<(), SnapshotError> {
    if history.len() as u64 != snap.event_count {
        return Err(SnapshotError::HistoryMismatch("event_count"));
    }
    let mut head = genesis_head();
    let mut gate = StoreGate::new(mesh, account);
    for (kind, ev) in history {
        head = head_extend(&head, &event_bytes(kind, ev));
        let parsed = GateEvent::from_json(kind, ev).map_err(|_| SnapshotError::HistoryMismatch("event"))?;
        let _ = gate.apply(&parsed);
    }
    if head != snap.events_head {
        return Err(SnapshotError::HistoryMismatch("events_head"));
    }
    if state_hash(&gate.state_json(snap.taken_at)) != snap.state_hash {
        return Err(SnapshotError::HistoryMismatch("state"));
    }
    Ok(())
}
