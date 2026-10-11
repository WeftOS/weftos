//! Node facts over the mesh (ADR-099 sections 2 and 8, card mesh-placement-03).
//!
//! [`FactsExchange`] is the node-local side of facts distribution:
//!
//! - **Publish.** The local node signs its [`NodeFacts`] and sends the
//!   signed base to every peer ([`FactsExchange::publish`]). When only live
//!   state changed (capability states, free memory) it sends a small signed
//!   [`FactsDelta`] instead ([`FactsExchange::update_live`]); a changed
//!   capability set, or a base nearing its TTL, produces a new base. A peer
//!   that joins, or that reports a missing base, is sent the current base
//!   and latest delta.
//! - **Receive.** Facts arrive as `mesh.node_facts` control messages
//!   ([`FACTS_TOPIC`]) and are handled through [`PeerControlSink`] with the
//!   connection's authenticated identity. Nothing is trusted on arrival:
//!   the signature, the key-to-node-id binding, the TTL window and the
//!   `seq` ordering are all checked by [`NodeFactsCache`], and a peer can
//!   only speak for itself (`node_id` must be the sender).
//! - **Budget.** Each connection has a frame and byte budget, charged on the
//!   whole frame and spent before anything is parsed or verified; one
//!   connection holds at most [`MAX_DISCOVERED_PER_CONN`] `Discovered`
//!   entries and the cache holds at most 512 in all (the heaviest connection
//!   loses its oldest entry first). A connection that reconnects gets a fresh
//!   id and a fresh quota, so a patient attacker can still churn the
//!   `Discovered` class; `Paired` and `Pinned` entries are never touched.
//! - **Trust.** The receiver assigns the [`TrustTier`]. A peer whose node id
//!   admission verified (ADR-103 A10) gets [`FactsTrustPolicy::verified_tier`];
//!   any other connection (including every peer under `observe`, where none
//!   is marked verified) gets the lowest tier. A node's tier is never
//!   lowered by a later frame, and the local node's facts can never be
//!   replaced from the wire.
//! - **Provenance.** The receiver did not probe or measure remote data, so
//!   the cached copy caps every capability's provenance: `probed` for a
//!   verified peer, `claimed` otherwise. `measured` is never held for a
//!   remote node. The signed envelope is kept as received.

use std::sync::{Arc, Mutex};

use clawft_types::placement::{
    CapabilityState, FactsDelta, NodeFacts, Provenance, StateChange, TrustTier,
};
use ed25519_dalek::SigningKey;
use serde::{Deserialize, Serialize};

use crate::cluster::ClusterMembership;
use crate::ipc::{KernelMessage, MessagePayload, MessageTarget};
use crate::mesh_delivery::PeerCtx;
use crate::mesh_discovery::MeshPeerEvent;
pub use crate::mesh_runtime::{FACTS_TOPIC, PeerControlSink};
use crate::mesh_runtime::MeshRuntime;
use crate::node_facts::{CacheError, InsertOutcome};
use crate::node_facts_advert::{
    NodeFactsAdvertError, SignedFactsDelta, SignedNodeFacts, sign_facts_delta, sign_node_facts,
    verify_facts_delta,
};
use crate::node_registry::node_id_from_pubkey;

/// Most `Discovered` entries one connection may hold: an unverified
/// connection can claim any source id, so it cannot fill the cache.
pub const MAX_DISCOVERED_PER_CONN: usize = 4;
/// Frames per second one connection may send on the facts topic (burst 2x).
pub const FRAMES_PER_SEC: f64 = 20.0;
/// Payload bytes per second one connection may send (burst 4x).
pub const BYTES_PER_SEC: f64 = 256.0 * 1024.0;
/// Connections whose budgets are remembered at once.
const MAX_TRACKED_CONNS: usize = 1024;

/// Token buckets for one connection.
struct Budget {
    frames: f64,
    bytes: f64,
    last: std::time::Instant,
}

/// A new base is signed once the current one has used this share of its TTL
/// (deltas do not extend a base's lifetime).
const REBASE_NUMERATOR: u64 = 4;
const REBASE_DENOMINATOR: u64 = 5;

/// Wire body of a `mesh.node_facts` message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum FactsWire {
    /// Signed base facts.
    Facts {
        /// The signed block.
        signed: SignedNodeFacts,
    },
    /// Signed live-state delta.
    Delta {
        /// The signed delta.
        signed: SignedFactsDelta,
    },
    /// "Send me your facts": the sender holds no usable base for you.
    Request,
}

impl FactsWire {
    fn to_value(&self) -> Option<serde_json::Value> {
        serde_json::to_value(self).ok()
    }
}

/// What tier and provenance a peer's facts earn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FactsTrustPolicy {
    /// Tier for a peer whose node id admission verified. `Paired` here is the
    /// cache's class (protected from `Discovered` churn); it is not an
    /// operator pairing and placement never reads it. `weaver cluster nodes
    /// --facts` shows it as "mesh-verified" for that reason.
    pub verified_tier: TrustTier,
    /// Tier for every other connection.
    pub unverified_tier: TrustTier,
    /// Provenance ceiling for a verified peer's capabilities.
    pub verified_cap: Provenance,
    /// Provenance ceiling for every other connection's capabilities.
    pub unverified_cap: Provenance,
}

impl Default for FactsTrustPolicy {
    fn default() -> Self {
        Self {
            verified_tier: TrustTier::Paired,
            unverified_tier: TrustTier::Discovered,
            verified_cap: Provenance::Probed,
            unverified_cap: Provenance::Claimed,
        }
    }
}

/// What receiving one facts message did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IngestOutcome {
    /// Facts cached under `tier`.
    Accepted {
        /// Node the facts describe.
        node_id: String,
        /// Tier the receiver assigned.
        tier: TrustTier,
        /// Whether a node was added, replaced or unchanged.
        outcome: InsertOutcome,
    },
    /// Delta applied to the cached base.
    DeltaApplied {
        /// Node the delta describes.
        node_id: String,
    },
    /// The peer asked for our facts; they were queued as replies.
    Requested,
}

/// Why a facts message was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IngestError {
    /// The message is not a well-formed [`FactsWire`].
    #[error("malformed facts message: {0}")]
    Malformed(String),
    /// The facts name a node other than the sender.
    #[error("facts for {subject} sent by {sender}")]
    NotSender {
        /// Node the facts describe.
        subject: String,
        /// Connection identity.
        sender: String,
    },
    /// A peer tried to replace this node's own facts.
    #[error("refusing remote facts for the local node")]
    LocalNode,
    /// One connection holds its maximum of `Discovered` entries.
    #[error("connection {conn} already supplied {limit} unverified nodes")]
    Quota {
        /// Connection id.
        conn: u64,
        /// The per-connection limit.
        limit: usize,
    },
    /// Signature, binding, TTL, ordering or delta check failed.
    #[error(transparent)]
    Cache(#[from] CacheError),
}

/// The latest facts this node published.
struct Local {
    base: Option<SignedNodeFacts>,
    facts: Option<NodeFacts>,
    delta: Option<SignedFactsDelta>,
    delta_seq: u64,
    last_seq: u64,
}

/// What [`FactsExchange::update_live`] sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Published {
    /// A new signed base with this `seq`.
    Base(u64),
    /// A delta with this `seq` carrying this many state changes.
    Delta {
        /// Delta sequence.
        seq: u64,
        /// Capability state changes in it.
        changes: usize,
    },
    /// Nothing changed; nothing sent.
    Nothing,
}

/// Node-local facts distribution.
pub struct FactsExchange {
    node_id: String,
    key: SigningKey,
    membership: Arc<ClusterMembership>,
    runtime: Arc<MeshRuntime>,
    policy: FactsTrustPolicy,
    local: Mutex<Local>,
    budgets: dashmap::DashMap<u64, Budget>,
}

impl FactsExchange {
    /// Exchange for the node owning `key`, sending over `runtime` and
    /// caching in `membership`.
    pub fn new(
        key: SigningKey,
        membership: Arc<ClusterMembership>,
        runtime: Arc<MeshRuntime>,
        policy: FactsTrustPolicy,
    ) -> Arc<Self> {
        let node_id = node_id_from_pubkey(&key.verifying_key().to_bytes());
        Arc::new(Self {
            node_id,
            key,
            membership,
            runtime,
            policy,
            local: Mutex::new(Local {
                base: None,
                facts: None,
                delta: None,
                delta_seq: 0,
                last_seq: 0,
            }),
            budgets: dashmap::DashMap::new(),
        })
    }

    /// Install as the runtime's facts sink and announce the current facts to
    /// every peer that joins or recovers. Call once.
    pub fn start(self: &Arc<Self>) {
        self.runtime.set_control_sink(FACTS_TOPIC, self.clone());
        let me = self.clone();
        let mut events = self.runtime.subscribe_peer_events();
        tokio::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(MeshPeerEvent::Joined { node_id, .. })
                    | Ok(MeshPeerEvent::Recovered { node_id, .. }) => {
                        me.send_current(&node_id).await;
                    }
                    // The connection is gone: the mesh-derived tier goes with it
                    // (an admission revocation closes the connection).
                    Ok(MeshPeerEvent::Left { node_id }) => {
                        me.on_peer_gone(&node_id);
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(_) => break,
                }
            }
        });
    }

    /// A peer's connection ended or its admission was revoked: lower its
    /// mesh-derived tier to `Discovered`. An operator-set tier stays.
    pub fn on_peer_gone(&self, node_id: &str) -> bool {
        self.membership.facts().demote_mesh_tier(node_id)
    }

    /// Charge one frame of `bytes` to connection `conn`. False = over budget.
    fn allow(&self, conn: u64, bytes: usize) -> bool {
        if self.budgets.len() >= MAX_TRACKED_CONNS && !self.budgets.contains_key(&conn) {
            let oldest = self.budgets.iter().min_by_key(|b| b.last).map(|b| *b.key());
            if let Some(k) = oldest {
                self.budgets.remove(&k);
            }
        }
        let now = std::time::Instant::now();
        let mut b = self.budgets.entry(conn).or_insert(Budget {
            frames: 2.0 * FRAMES_PER_SEC,
            bytes: 4.0 * BYTES_PER_SEC,
            last: now,
        });
        let dt = now.duration_since(b.last).as_secs_f64();
        b.last = now;
        b.frames = (b.frames + dt * FRAMES_PER_SEC).min(2.0 * FRAMES_PER_SEC);
        b.bytes = (b.bytes + dt * BYTES_PER_SEC).min(4.0 * BYTES_PER_SEC);
        if b.frames < 1.0 || b.bytes < bytes as f64 {
            return false;
        }
        b.frames -= 1.0;
        b.bytes -= bytes as f64;
        true
    }

    /// This node's id.
    pub fn node_id(&self) -> &str {
        &self.node_id
    }

    /// The membership whose cache this exchange fills.
    pub fn membership(&self) -> &Arc<ClusterMembership> {
        &self.membership
    }

    fn now() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Local> {
        self.local.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Sign `facts` as the new base (its `seq` is replaced by the next one),
    /// cache them as the local node (`pinned`) and send them to every peer.
    pub async fn publish(&self, facts: NodeFacts, now: u64) -> Result<u64, NodeFactsAdvertError> {
        let (seq, signed) = self.install_base(facts, now)?;
        self.broadcast(FactsWire::Facts { signed }).await;
        Ok(seq)
    }

    fn install_base(
        &self,
        mut facts: NodeFacts,
        now: u64,
    ) -> Result<(u64, SignedNodeFacts), NodeFactsAdvertError> {
        let mut local = self.lock();
        let seq = (local.last_seq + 1).max(now);
        facts.node_id = self.node_id.clone();
        facts.issued_at = now;
        facts.seq = seq;
        let signed = sign_node_facts(&facts, &self.key)?;
        self.membership
            .facts()
            .insert(signed.clone(), TrustTier::Pinned, now)
            .map_err(|e| NodeFactsAdvertError::Malformed(e.to_string()))?;
        local.last_seq = seq;
        local.base = Some(signed.clone());
        local.facts = Some(facts);
        local.delta = None;
        local.delta_seq = 0;
        Ok((seq, signed))
    }

    /// Publish a fresh probe of the local node: a delta when only live state
    /// (capability states, free memory) changed since the last base, a new
    /// base when the capability set changed or the base is nearing its TTL,
    /// nothing when nothing changed.
    pub async fn update_live(
        &self,
        probed: NodeFacts,
        now: u64,
    ) -> Result<Published, NodeFactsAdvertError> {
        enum Step {
            Base(NodeFacts),
            Delta(SignedFactsDelta, usize),
            Nothing,
        }
        let step = {
            let mut local = self.lock();
            match local.facts.clone() {
                Some(base)
                    if now < base.issued_at + base.ttl_secs * REBASE_NUMERATOR / REBASE_DENOMINATOR
                        && same_shape(&base, &probed) =>
                {
                    let delta = live_delta(&base, &probed, local.delta_seq + 1, now);
                    if delta.changes.is_empty() && delta.mem_free.is_none() {
                        Step::Nothing
                    } else {
                        let n = delta.changes.len();
                        let signed = sign_facts_delta(&delta, &self.key)?;
                        local.delta_seq = delta.seq;
                        local.delta = Some(signed.clone());
                        // Track the new live state so the next diff is against it.
                        if let Some(f) = local.facts.as_mut() {
                            f.apply_delta(&delta).map_err(NodeFactsAdvertError::Facts)?;
                        }
                        let _ = self.membership.facts().apply_delta(&signed, now);
                        Step::Delta(signed, n)
                    }
                }
                _ => Step::Base(probed),
            }
        };
        match step {
            Step::Nothing => Ok(Published::Nothing),
            Step::Base(f) => self.publish(f, now).await.map(Published::Base),
            Step::Delta(signed, changes) => {
                let seq = {
                    let (d, _) = verify_facts_delta(&signed, now)?;
                    d.seq
                };
                self.broadcast(FactsWire::Delta { signed }).await;
                Ok(Published::Delta { seq, changes })
            }
        }
    }

    async fn broadcast(&self, wire: FactsWire) {
        for peer in self.runtime.peer_ids() {
            self.send_wire(&peer, &wire).await;
        }
    }

    async fn send_wire(&self, peer: &str, wire: &FactsWire) {
        let Some(value) = wire.to_value() else { return };
        let msg = KernelMessage::new(
            0,
            MessageTarget::Topic(FACTS_TOPIC.to_string()),
            MessagePayload::Json(value),
        );
        if let Err(e) = self.runtime.route_to_remote(peer, msg).await {
            tracing::debug!(peer, error = %e, "node facts send failed");
        }
    }

    fn current(&self) -> Vec<FactsWire> {
        let local = self.lock();
        let mut out = Vec::new();
        if let Some(signed) = local.base.clone() {
            out.push(FactsWire::Facts { signed });
        }
        if let Some(signed) = local.delta.clone() {
            out.push(FactsWire::Delta { signed });
        }
        out
    }

    async fn send_current(&self, peer: &str) {
        for wire in self.current() {
            self.send_wire(peer, &wire).await;
        }
    }

    /// [`Self::ingest_from`] with no connection id (no per-connection quota).
    pub fn ingest(
        &self,
        ctx: &PeerCtx,
        wire: FactsWire,
        now: u64,
        replies: &mut Vec<FactsWire>,
    ) -> Result<IngestOutcome, IngestError> {
        self.ingest_from(ctx, 0, wire, now, replies)
    }

    /// Receive one facts message from connection `conn` (identity `ctx`), at
    /// `now`. Replies to send back to the peer are appended to `replies`.
    ///
    /// The tier is derived from `ctx` on every frame: a node that is no
    /// longer admitted drops back to `Discovered` with its next frame (or
    /// when its connection goes, see [`Self::start`]); only a tier an
    /// operator set is kept.
    pub fn ingest_from(
        &self,
        ctx: &PeerCtx,
        conn: u64,
        wire: FactsWire,
        now: u64,
        replies: &mut Vec<FactsWire>,
    ) -> Result<IngestOutcome, IngestError> {
        let cache = self.membership.facts();
        let (tier, cap) = if ctx.node_verified {
            (self.policy.verified_tier, self.policy.verified_cap)
        } else {
            (self.policy.unverified_tier, self.policy.unverified_cap)
        };
        match wire {
            FactsWire::Request => {
                replies.extend(self.current());
                Ok(IngestOutcome::Requested)
            }
            FactsWire::Facts { signed } => {
                let facts = crate::node_facts_advert::verify_node_facts(&signed, now)
                    .map_err(CacheError::from)?;
                self.check_subject(ctx, &facts.node_id)?;
                // One unverified connection can claim any number of source ids:
                // cap how many `Discovered` entries it may hold.
                if tier == TrustTier::Discovered
                    && conn != 0
                    && cache.get(&facts.node_id, now).is_none_or(|h| h.origin != conn)
                    && cache.discovered_from(conn, now) >= MAX_DISCOVERED_PER_CONN
                {
                    return Err(IngestError::Quota {
                        conn,
                        limit: MAX_DISCOVERED_PER_CONN,
                    });
                }
                let outcome = cache.insert_remote(signed, tier, cap, conn, now)?;
                // The tier actually held: an operator-set one wins.
                let tier = cache.get(&facts.node_id, now).map_or(tier, |h| h.trust_tier);
                Ok(IngestOutcome::Accepted {
                    node_id: facts.node_id,
                    tier,
                    outcome,
                })
            }
            FactsWire::Delta { signed } => {
                let (delta, _) = verify_facts_delta(&signed, now).map_err(CacheError::from)?;
                self.check_subject(ctx, &delta.node_id)?;
                if let Err(e) = cache.apply_delta(&signed, now) {
                    // Missing or older base than the delta names: ask for it.
                    let behind = cache
                        .get(&delta.node_id, now)
                        .is_none_or(|c| c.facts.seq < delta.base_seq);
                    if behind {
                        replies.push(FactsWire::Request);
                    }
                    return Err(e.into());
                }
                Ok(IngestOutcome::DeltaApplied {
                    node_id: delta.node_id,
                })
            }
        }
    }

    fn check_subject(&self, ctx: &PeerCtx, subject: &str) -> Result<(), IngestError> {
        if subject == self.node_id {
            return Err(IngestError::LocalNode);
        }
        if subject != ctx.peer_id {
            return Err(IngestError::NotSender {
                subject: subject.to_string(),
                sender: ctx.peer_id.clone(),
            });
        }
        Ok(())
    }
}

impl PeerControlSink for FactsExchange {
    fn on_peer_control(
        &self,
        ctx: &PeerCtx,
        conn: u64,
        payload: &serde_json::Value,
    ) -> Vec<serde_json::Value> {
        // Budget first: nothing below (parse, signature check) runs for a
        // connection that is over its frame or byte budget. The size is read
        // off the signed payload string, which is what the verify cost follows.
        let size = json_size(payload);
        if !self.allow(conn, size) {
            tracing::debug!(peer = %ctx.peer_id, conn, "node facts frame dropped: over budget");
            return Vec::new();
        }
        let wire: FactsWire = match serde_json::from_value(payload.clone()) {
            Ok(w) => w,
            Err(e) => {
                tracing::warn!(peer = %ctx.peer_id, error = %e, "malformed node facts message");
                return Vec::new();
            }
        };
        let mut replies = Vec::new();
        if let Err(e) = self.ingest_from(ctx, conn, wire, Self::now(), &mut replies) {
            if matches!(&e, IngestError::Cache(CacheError::DuplicateDelta { .. })) {
                tracing::debug!(peer = %ctx.peer_id, error = %e, "node facts delta replayed; ignored");
            } else if matches!(
                &e,
                IngestError::Cache(CacheError::Verify(NodeFactsAdvertError::Facts(
                    clawft_types::placement::FactsError::FromFuture { .. }
                )))
            ) {
                tracing::warn!(peer = %ctx.peer_id, error = %e,
                    "node facts refused: issued in the future, the peer's clock is ahead of ours \
                     by more than the allowed skew");
            } else {
                tracing::warn!(peer = %ctx.peer_id, verified = ctx.node_verified, error = %e,
                    "node facts refused");
            }
        }
        replies.iter().filter_map(FactsWire::to_value).collect()
    }
}

/// Size of a JSON value in bytes (strings, keys and scalars), walked without
/// allocating: the budget is charged for the whole frame, not for one field.
fn json_size(v: &serde_json::Value) -> usize {
    match v {
        serde_json::Value::String(s) => s.len() + 2,
        serde_json::Value::Array(a) => 2 + a.iter().map(|x| json_size(x) + 1).sum::<usize>(),
        serde_json::Value::Object(o) => {
            2 + o.iter().map(|(k, x)| k.len() + 4 + json_size(x)).sum::<usize>()
        }
        _ => 8,
    }
}

/// True when `a` and `b` differ only in live state (capability states and
/// the free-memory attribute), so a delta can express the change.
fn same_shape(a: &NodeFacts, b: &NodeFacts) -> bool {
    let norm = |f: &NodeFacts| {
        let mut c = f.capabilities.clone();
        for cap in &mut c {
            cap.state = CapabilityState::Available;
            if matches!(cap.id.as_str(), "mem.unified" | "mem.system") {
                cap.attrs.remove("free");
            }
        }
        (c, f.ttl_secs)
    };
    norm(a) == norm(b)
}

/// The delta that takes `base` to `now_state` (same shape).
fn live_delta(base: &NodeFacts, now_state: &NodeFacts, seq: u64, now: u64) -> FactsDelta {
    let changes = base
        .capabilities
        .iter()
        .zip(&now_state.capabilities)
        .enumerate()
        .filter(|(_, (b, n))| b.state != n.state)
        .map(|(i, (_, n))| StateChange {
            index: i as u32,
            id: n.id.clone(),
            state: n.state,
        })
        .collect();
    let free = |f: &NodeFacts| f.load().mem_free;
    let mem_free = (free(base) != free(now_state)).then(|| free(now_state)).flatten();
    FactsDelta {
        node_id: base.node_id.clone(),
        base_seq: base.seq,
        seq,
        issued_at: now,
        changes,
        mem_free,
    }
}

#[cfg(test)]
#[path = "node_facts_exchange_tests.rs"]
mod tests;
