//! Spike: compaction of the grant ledger's chain history (ADR-126, "Chain
//! compaction" proposal).
//!
//! Replaying every grant event gets slower for ever. A [`Snapshot`] is the
//! folded table at `through_seq`, plus `events_head`, a hash chain over the
//! events folded so far. The daemon appends it to the chain as a signed
//! event; a follower then starts from the newest snapshot and replays only
//! the tail.
//!
//! How a follower checks one:
//! 1. `version` and `scope` match what it expects.
//! 2. `state_hash` is the SHA-256 of the canonical state (a corrupted or
//!    hand-edited table fails).
//! 3. The signature over the canonical snapshot verifies under a key the
//!    follower already trusts for chain events (the host supplies
//!    [`SnapshotVerifier`]; use `verify_strict` for Ed25519).
//! 4. A follower that still holds the old events can do better:
//!    [`verify_against_history`] re-folds them and compares `events_head` and
//!    `state_hash`, so it does not have to trust the signer for the past.
//!
//! The same shape serves the S5 gate state: implement the fold for its
//! events and reuse [`head_extend`] and the hashing rules.
//!
//! Only the state is kept in the snapshot, so a revoke is carried by the
//! absence of the grant; a stale pre-snapshot event is ignored by the
//! existing sequence rule. The compacted segment must be archived, since the
//! snapshot alone cannot answer "who held what, when".

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::grants::{GrantEvent, GrantLedger};
use crate::policy::Access;
use crate::sigv4::sha256_hex;

pub const SNAPSHOT_VERSION: u32 = 1;
const GENESIS: &str = "weftos/grant-ledger/genesis/v1";
const SIG_DOMAIN: &str = "weftos/grant-ledger/snapshot/v1\n";

pub(crate) fn genesis_head() -> String {
    sha256_hex(GENESIS.as_bytes())
}

/// Next head after folding `ev`. Hash of the previous head and the event's
/// canonical JSON (fields are in a fixed order, so this is deterministic).
pub(crate) fn extend_head(head: &str, ev: &GrantEvent) -> String {
    head_extend(head, serde_json::to_string(ev).expect("event serializes").as_bytes())
}

/// Generic head extension for any event type's canonical bytes.
pub fn head_extend(head: &str, canonical_event: &[u8]) -> String {
    let mut buf = head.as_bytes().to_vec();
    buf.push(b'\n');
    buf.extend_from_slice(canonical_event);
    sha256_hex(&buf)
}

/// When to take a snapshot. **Off by default**: nothing in this crate takes
/// one unless the host builds an enabled schedule and asks. The schedule
/// reads no clock; the host passes the counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SnapshotSchedule {
    every_events: Option<u64>,
    every_secs: Option<u64>,
}

impl SnapshotSchedule {
    /// Never due. Same as `default()`.
    pub fn off() -> Self {
        Self::default()
    }
    /// Due after `n` events since the last snapshot (0 is treated as off).
    pub fn every_events(n: u64) -> Self {
        Self { every_events: (n > 0).then_some(n), every_secs: None }
    }
    /// Due after `secs` since the last snapshot, if at least one event arrived.
    pub fn every_secs(secs: u64) -> Self {
        Self { every_events: None, every_secs: (secs > 0).then_some(secs) }
    }
    /// Either trigger.
    pub fn or_every_secs(mut self, secs: u64) -> Self {
        if secs > 0 {
            self.every_secs = Some(secs);
        }
        self
    }
    pub fn is_enabled(&self) -> bool {
        self.every_events.is_some() || self.every_secs.is_some()
    }
    /// A snapshot with nothing new since the last one is never due.
    pub fn due(&self, events_since: u64, secs_since: u64) -> bool {
        if events_since == 0 {
            return false;
        }
        self.every_events.is_some_and(|n| events_since >= n) || self.every_secs.is_some_and(|t| secs_since >= t)
    }
}

/// One held grant, sorted for a canonical encoding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeldGrant {
    pub principal: String,
    pub store: String,
    pub access: Vec<Access>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub version: u32,
    /// What the snapshot is for, e.g. `grants/<mesh>`. Never mixed across scopes.
    pub scope: String,
    /// Last event folded in.
    pub through_seq: u64,
    pub events_head: String,
    pub state: Vec<HeldGrant>,
    /// SHA-256 over the canonical JSON of `state`.
    pub state_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedSnapshot {
    pub snapshot: Snapshot,
    /// The chain-event signer's key id.
    pub signer: String,
    pub signature: Vec<u8>,
}

/// Supplied by the host: checks a signature under a key trusted for chain events.
pub trait SnapshotVerifier {
    fn verify(&self, signer: &str, message: &[u8], signature: &[u8]) -> bool;
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
    #[error("the history does not reproduce the snapshot ({0})")]
    HistoryMismatch(&'static str),
    #[error("tail event seq {seq} is not after the snapshot ({through})")]
    TailBeforeSnapshot { seq: u64, through: u64 },
}

fn canonical_state(state: &[HeldGrant]) -> String {
    serde_json::to_string(state).expect("state serializes")
}

fn state_hash(state: &[HeldGrant]) -> String {
    sha256_hex(canonical_state(state).as_bytes())
}

/// The bytes a signer signs: a domain, then the snapshot's canonical JSON.
pub fn signing_bytes(s: &Snapshot) -> Vec<u8> {
    let mut v = SIG_DOMAIN.as_bytes().to_vec();
    v.extend_from_slice(serde_json::to_string(s).expect("snapshot serializes").as_bytes());
    v
}

impl GrantLedger {
    /// Fold the current table into a snapshot for `scope`.
    pub fn snapshot(&self, scope: &str) -> Snapshot {
        let t = self.0.lock().expect("ledger");
        let by_key: BTreeMap<_, BTreeSet<Access>> = t
            .held
            .iter()
            .map(|((p, s), a)| ((p.clone(), s.clone()), a.iter().copied().collect::<BTreeSet<_>>()))
            .collect();
        let state: Vec<HeldGrant> = by_key
            .into_iter()
            .map(|((principal, store), a)| HeldGrant { principal, store, access: a.into_iter().collect() })
            .collect();
        Snapshot {
            version: SNAPSHOT_VERSION,
            scope: scope.into(),
            through_seq: t.last_seq,
            events_head: t.events_head.clone(),
            state_hash: state_hash(&state),
            state,
        }
    }

    /// Sign with the host's chain-event signer.
    pub fn signed_snapshot(&self, scope: &str, signer: &str, sign: impl Fn(&[u8]) -> Vec<u8>) -> SignedSnapshot {
        let snapshot = self.snapshot(scope);
        let signature = sign(&signing_bytes(&snapshot));
        SignedSnapshot { snapshot, signer: signer.into(), signature }
    }

    /// The checks a follower makes on a snapshot with no history to hand:
    /// version, scope, state hash, then the signature.
    pub fn check(signed: &SignedSnapshot, scope: &str, verifier: &dyn SnapshotVerifier) -> Result<(), SnapshotError> {
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
        if !verifier.verify(&signed.signer, &signing_bytes(s), &signed.signature) {
            return Err(SnapshotError::BadSignature);
        }
        Ok(())
    }

    /// Start from a verified snapshot, then apply the tail. Events at or
    /// before the snapshot (a whole log handed in by mistake) are an error,
    /// not silently skipped, because they mean the caller mixed two histories.
    pub fn restore(
        signed: &SignedSnapshot,
        scope: &str,
        verifier: &dyn SnapshotVerifier,
        tail: &[GrantEvent],
    ) -> Result<GrantLedger, SnapshotError> {
        Self::check(signed, scope, verifier)?;
        let s = &signed.snapshot;
        if let Some(e) = tail.iter().find(|e| e.seq <= s.through_seq) {
            return Err(SnapshotError::TailBeforeSnapshot { seq: e.seq, through: s.through_seq });
        }
        let ledger = GrantLedger::new();
        {
            let mut t = ledger.0.lock().expect("ledger");
            t.last_seq = s.through_seq;
            t.events_head = s.events_head.clone();
            for g in &s.state {
                t.held.insert((g.principal.clone(), g.store.clone()), g.access.iter().copied().collect());
            }
        }
        for e in tail {
            ledger.apply(e);
        }
        Ok(ledger)
    }
}

/// A follower that kept the old events checks the snapshot without trusting
/// its signer for the past: the re-folded history must give the same head,
/// state and `through_seq`.
pub fn verify_against_history(snap: &Snapshot, history: &[GrantEvent]) -> Result<(), SnapshotError> {
    let l = GrantLedger::replay(history.iter().filter(|e| e.seq <= snap.through_seq).cloned());
    let again = l.snapshot(&snap.scope);
    if again.through_seq != snap.through_seq {
        return Err(SnapshotError::HistoryMismatch("through_seq"));
    }
    if again.events_head != snap.events_head {
        return Err(SnapshotError::HistoryMismatch("events_head"));
    }
    if again.state_hash != snap.state_hash || again.state != snap.state {
        return Err(SnapshotError::HistoryMismatch("state"));
    }
    Ok(())
}
