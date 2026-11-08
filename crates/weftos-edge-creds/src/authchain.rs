//! Chain authentication for the events fed to the grant ledger (ADR-126 S3).
//!
//! [`GrantLedger::apply`] trusts what it is given. A grant event that did not
//! come from the chain, or arrives out of order, would change who may reach
//! which bucket, so the daemon feeds the ledger through a [`ChainFeed`], which
//! accepts an event only if:
//!
//! 1. it is for this feed's scope (a mesh; no replay across meshes);
//! 2. its signature verifies under a key the host trusts for chain events
//!    ([`SnapshotVerifier`], the same interface as snapshots, with its own
//!    signing domain). The signature covers the scope, the event and the
//!    ledger's head before it (`prev_head`);
//! 3. it is not older than what the ledger already applied (a replay is a
//!    benign [`FeedError::Replay`], never applied);
//! 4. its `prev_head` is the ledger's current `events_head`, so a reordered or
//!    dropped event breaks the link and nothing after the gap is accepted.
//!
//! The link reuses the head the ledger already keeps for compaction, so it
//! survives a snapshot restore. A refused event changes nothing.
//!
//! The real signer, how the chain wraps events and the cadence are the chain
//! owner's. This is the consuming side, written against that interface and
//! tested with a stand-in signer.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::broker::{AuditSink, EdgeBroker, StsMinter};
use crate::clock::Clock;
use crate::compaction::SnapshotVerifier;
use crate::grants::{Applied, GrantEvent, GrantLedger};

const SIG_DOMAIN: &str = "weftos/grant-ledger/event/v1\n";

/// A grant event as the chain carries it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChainGrantEvent {
    pub scope: String,
    pub event: GrantEvent,
    /// The ledger's `events_head` immediately before this event.
    pub prev_head: String,
    pub signer: String,
    pub signature: Vec<u8>,
}

/// What the signer signs.
pub fn event_signing_bytes(scope: &str, event: &GrantEvent, prev_head: &str) -> Vec<u8> {
    let mut v = SIG_DOMAIN.as_bytes().to_vec();
    v.extend_from_slice(scope.as_bytes());
    v.push(b'\n');
    v.extend_from_slice(prev_head.as_bytes());
    v.push(b'\n');
    v.extend_from_slice(serde_json::to_string(event).expect("event serializes").as_bytes());
    v
}

impl ChainGrantEvent {
    /// For the chain side and for tests: sign `event` as following `prev_head`.
    pub fn sign(scope: &str, event: GrantEvent, prev_head: &str, signer: &str, sign: impl Fn(&[u8]) -> Vec<u8>) -> Self {
        let signature = sign(&event_signing_bytes(scope, &event, prev_head));
        Self { scope: scope.into(), event, prev_head: prev_head.into(), signer: signer.into(), signature }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FeedError {
    #[error("event is for scope {got:?}, this feed is {want:?}")]
    Scope { want: String, got: String },
    #[error("event signature rejected")]
    BadSignature,
    /// `seq` is not after the last applied event: already seen. Safe to drop.
    #[error("event seq {0} was already applied or is older")]
    Replay(u64),
    /// The event does not follow the ledger's head: reordered, or an earlier event is missing.
    #[error("event seq {0} does not follow the ledger head (reordered or missing earlier event)")]
    BrokenLink(u64),
    #[error("event seq {0} jumps too far (seq guard)")]
    OutOfRange(u64),
}

pub struct ChainFeed<V> {
    ledger: Arc<GrantLedger>,
    scope: String,
    verifier: V,
    /// One event at a time through this feed, so the head check and the apply are one step.
    gate: std::sync::Mutex<()>,
}

impl<V: SnapshotVerifier> ChainFeed<V> {
    pub fn new(ledger: Arc<GrantLedger>, scope: &str, verifier: V) -> Self {
        Self { ledger, scope: scope.into(), verifier, gate: std::sync::Mutex::new(()) }
    }

    pub fn ledger(&self) -> &Arc<GrantLedger> {
        &self.ledger
    }

    /// The head the next event must name as `prev_head`.
    pub fn head(&self) -> String {
        self.ledger.snapshot(&self.scope).events_head
    }

    /// Authenticate and apply. On any error the ledger is unchanged.
    pub fn ingest(&self, ev: &ChainGrantEvent) -> Result<Applied, FeedError> {
        if ev.scope != self.scope {
            return Err(FeedError::Scope { want: self.scope.clone(), got: ev.scope.clone() });
        }
        if !self.verifier.verify(&ev.signer, &event_signing_bytes(&ev.scope, &ev.event, &ev.prev_head), &ev.signature) {
            return Err(FeedError::BadSignature);
        }
        let _one_at_a_time = self.gate.lock().expect("feed gate");
        let snap = self.ledger.snapshot(&self.scope);
        if ev.event.seq <= snap.through_seq {
            return Err(FeedError::Replay(ev.event.seq));
        }
        if ev.prev_head != snap.events_head {
            return Err(FeedError::BrokenLink(ev.event.seq));
        }
        match self.ledger.apply(&ev.event) {
            Applied::OutOfRange => Err(FeedError::OutOfRange(ev.event.seq)),
            Applied::Stale => Err(FeedError::Replay(ev.event.seq)),
            ok => Ok(ok),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IngestError {
    #[error("{0}")]
    Feed(#[from] FeedError),
    /// The event was applied and the node is in the revoked set, but disabling its RustFS user failed.
    #[error("event applied but revoking the node failed: {0}")]
    Revoke(String),
}

/// Authenticate, apply, and cut the node off at once when it lost write
/// (the authenticated twin of `apply_and_enforce`).
pub async fn ingest_and_enforce<V: SnapshotVerifier, M: StsMinter, A: AuditSink, C: Clock>(
    feed: &ChainFeed<V>,
    broker: &EdgeBroker<M, Arc<GrantLedger>, A, C>,
    ev: &ChainGrantEvent,
) -> Result<Applied, IngestError> {
    let applied = feed.ingest(ev)?;
    if matches!(applied, Applied::Changed { lost_write: true }) {
        broker.revoke(&ev.event.principal).await.map_err(|e| IngestError::Revoke(e.0))?;
    }
    Ok(applied)
}
