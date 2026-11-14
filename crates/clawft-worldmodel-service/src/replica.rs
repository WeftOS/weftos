//! Hot-standby replay and lease failover (in-process, no network).
//!
//! # Standby replay
//!
//! A standby never runs its own frame loop. It applies the primary's chain
//! entries in order: frame attestations update the frame count, last tuple,
//! the Welford SIGReg monitor and the four-condition gate exactly as the
//! primary did, so both reach the same statistics. SIGReg-health and
//! rollback-gate entries are counted but not re-applied, because the standby
//! derives them itself from the replayed frames. The standby never appends to
//! its own chain while replaying: a replica adds no authority (ADR-090).
//!
//! The SIGReg monitor on the primary ingests `z_t` (not `z_{t+1}`), so replay
//! feeds `z_t` too; anything else would drift from the primary's statistics.
//!
//! # Lease failover: a stand-in, not Raft
//!
//! [`LeaseTracker`] is a deterministic lease on a caller-supplied clock. The
//! standby calls [`LeaseTracker::heartbeat`] whenever it hears from the
//! primary and promotes itself once [`LeaseTracker::expired`]. There is no
//! election, no term, no quorum and no fencing, so two nodes can both believe
//! they are primary after a partition. A real Raft or equivalent consensus
//! layer is not in the workspace, and adding one is an owner decision (new
//! dependency gate). Network heartbeats and chain transport are likewise not
//! implemented; callers pass heartbeats and chain entries in directly.

use serde::Serialize;
use weftos_worldmodel::{
    AttestationPayload, ChainEntry, ChainSink, MemoryChainSink,
    EVENT_KIND_LEWM_FRAME_ATTESTATION, EVENT_KIND_ROLLBACK_GATE, EVENT_KIND_SIGREG_HEALTH,
    AttestationError,
};

use crate::service::{ServiceError, WorldModelService};
use crate::topology::{ServiceConfig, ServiceRole};

/// Chain event kind attested on every role change.
pub const EVENT_KIND_ROLE_CHANGE: &str = "lewm.role_change";

/// What a replay call applied or skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReplayOutcome {
    /// Frame attestations applied.
    pub frames: u64,
    /// SIGReg health entries counted (not re-applied).
    pub sigreg_events: u64,
    /// Rollback-gate verdict entries counted (not re-applied).
    pub gate_events: u64,
    /// Role-change entries counted.
    pub role_events: u64,
    /// Entries of any other kind.
    pub other_events: u64,
    /// Entries skipped because their sequence was already replayed.
    pub skipped: u64,
    /// Highest chain sequence seen in this call.
    pub last_seq: Option<u64>,
}

impl ReplayOutcome {
    fn absorb(&mut self, o: ReplayOutcome) {
        self.frames += o.frames;
        self.sigreg_events += o.sigreg_events;
        self.gate_events += o.gate_events;
        self.role_events += o.role_events;
        self.other_events += o.other_events;
        self.skipped += o.skipped;
        self.last_seq = o.last_seq.or(self.last_seq);
    }
}

/// Heartbeat lease on a simulated or host clock (milliseconds).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeaseTracker {
    /// How long the primary may stay silent before the lease lapses.
    pub lease_ms: u64,
    /// Timestamp of the most recent heartbeat (or of tracker creation).
    pub last_heartbeat_ms: u64,
}

impl LeaseTracker {
    /// New tracker whose first lease runs from `now_ms`.
    pub fn new(lease_ms: u64, now_ms: u64) -> Self {
        Self {
            lease_ms,
            last_heartbeat_ms: now_ms,
        }
    }

    /// New tracker using the configured `lease_ms`.
    pub fn from_config(config: &ServiceConfig, now_ms: u64) -> Self {
        Self::new(config.lease_ms, now_ms)
    }

    /// Record a primary heartbeat. Out-of-order timestamps are ignored.
    pub fn heartbeat(&mut self, timestamp_ms: u64) {
        self.last_heartbeat_ms = self.last_heartbeat_ms.max(timestamp_ms);
    }

    /// True once more than `lease_ms` has passed since the last heartbeat.
    pub fn expired(&self, timestamp_ms: u64) -> bool {
        timestamp_ms.saturating_sub(self.last_heartbeat_ms) > self.lease_ms
    }
}

#[derive(Serialize)]
struct RoleChange<'a> {
    from: &'a str,
    to: &'a str,
    ts: u64,
    node_id: &'a str,
}

impl WorldModelService {
    /// Apply one primary chain entry to this standby.
    pub fn replay_entry(&mut self, entry: &ChainEntry) -> Result<ReplayOutcome, ServiceError> {
        if self.config.role != ServiceRole::Standby {
            return Err(ServiceError::NotStandby);
        }
        if !self.booted {
            return Err(ServiceError::NotBooted);
        }
        let mut out = ReplayOutcome {
            last_seq: Some(entry.seq),
            ..ReplayOutcome::default()
        };
        if self.last_replayed_seq.is_some_and(|last| entry.seq <= last) {
            out.skipped = 1;
            return Ok(out);
        }
        match entry.kind.as_str() {
            EVENT_KIND_LEWM_FRAME_ATTESTATION => {
                self.apply_replayed_frame(entry)?;
                out.frames = 1;
            }
            EVENT_KIND_SIGREG_HEALTH => out.sigreg_events = 1,
            EVENT_KIND_ROLLBACK_GATE => out.gate_events = 1,
            EVENT_KIND_ROLE_CHANGE => out.role_events = 1,
            _ => out.other_events = 1,
        }
        self.last_replayed_seq = Some(entry.seq);
        Ok(out)
    }

    /// Replay every entry of a primary's in-memory chain, in order.
    pub fn replay_from(&mut self, sink: &MemoryChainSink) -> Result<ReplayOutcome, ServiceError> {
        let mut total = ReplayOutcome::default();
        for entry in &sink.entries {
            total.absorb(self.replay_entry(entry)?);
        }
        Ok(total)
    }

    fn apply_replayed_frame(&mut self, entry: &ChainEntry) -> Result<(), ServiceError> {
        let tuple = AttestationPayload::from_json_bytes(&entry.payload)
            .and_then(|p| p.to_tuple())
            .map_err(|e| ServiceError::Replay(format!("seq {}: {e}", entry.seq)))?;
        if tuple.frame_seq != self.frames {
            return Err(ServiceError::Replay(format!(
                "frame gap: expected frame_seq {}, chain has {}",
                self.frames, tuple.frame_seq
            )));
        }
        let health = self
            .model
            .sigreg
            .update_at(&tuple.z_t, tuple.timestamp_ms)
            .map_err(ServiceError::WorldModel)?;
        if self.model.sigreg.last_rolled_back() {
            self.sigreg_rollbacks = self.sigreg_rollbacks.saturating_add(1);
        }
        let gate = self.model.evaluate_transition(
            &tuple.z_t,
            &tuple.z_hat,
            &tuple.z_tp1,
            Some(health.score),
        );
        self.last_gate = Some(gate);
        if gate.promote {
            self.gate_promotes = self.gate_promotes.saturating_add(1);
        } else {
            self.gate_vetoes = self.gate_vetoes.saturating_add(1);
        }
        self.last_tuple = Some(tuple);
        self.frames = self.frames.saturating_add(1);
        Ok(())
    }

    /// Promote a standby to primary and attest a `lewm.role_change` event.
    pub fn promote_to_primary(&mut self, timestamp_ms: u64) -> Result<(), ServiceError> {
        if self.config.role != ServiceRole::Standby {
            return Err(ServiceError::NotStandby);
        }
        self.change_role(ServiceRole::Primary, timestamp_ms)
    }

    /// Demote a primary to standby and attest a `lewm.role_change` event.
    pub fn demote_to_standby(&mut self, timestamp_ms: u64) -> Result<(), ServiceError> {
        if self.config.role != ServiceRole::Primary {
            return Err(ServiceError::Topology(
                "only a primary can be demoted to standby".into(),
            ));
        }
        self.change_role(ServiceRole::Standby, timestamp_ms)
    }

    /// Promote when `lease` has lapsed at `timestamp_ms`; returns whether it did.
    pub fn maybe_failover(
        &mut self,
        lease: &LeaseTracker,
        timestamp_ms: u64,
    ) -> Result<bool, ServiceError> {
        if self.config.role == ServiceRole::Standby && lease.expired(timestamp_ms) {
            self.promote_to_primary(timestamp_ms)?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Validate, attest, then switch role. The role is unchanged on error.
    fn change_role(&mut self, to: ServiceRole, timestamp_ms: u64) -> Result<(), ServiceError> {
        let from = self.config.role;
        let mut next = self.config.clone();
        next.role = to;
        next.validate().map_err(ServiceError::Topology)?;
        let payload = serde_json::to_vec(&RoleChange {
            from: from.as_str(),
            to: to.as_str(),
            ts: timestamp_ms,
            node_id: &self.config.node_id,
        })
        .map_err(|e| ServiceError::Attestation(AttestationError::Serialize(e.to_string())))?;
        self.chain
            .append_event(EVENT_KIND_ROLE_CHANGE, &payload)
            .map_err(ServiceError::Attestation)?;
        self.config = next;
        Ok(())
    }
}
