//! The live [`GrantCheck`]: grants as chain events, folded into a table
//! (ADR-126 section 3: "a grant of (principal, store, read | write | admin),
//! recorded on the chain. A name grants nothing").
//!
//! The daemon appends a [`GrantEvent`] to the chain for every grant and
//! revoke and feeds the same events here, in chain order, so the check is a
//! pure function of the chain: replaying the chain rebuilds the table.
//! Events carry a sequence number; a replayed or out-of-order event is
//! ignored, never applied twice.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::broker::{AuditSink, EdgeBroker, GrantCheck, StsMinter};
use crate::clock::Clock;
use crate::policy::Access;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrantOp {
    Grant,
    /// Remove one access level. Removing `write` leaves an existing `read`.
    Revoke,
    /// Remove every access the principal holds on the store (unpair).
    RevokeAll,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantEvent {
    /// Chain order. Strictly increasing.
    pub seq: u64,
    /// The principal: a node id here.
    pub principal: String,
    /// `weftos://<mesh>/nodes/<node id>/store`
    pub store: String,
    pub access: Access,
    pub op: GrantOp,
}

impl GrantEvent {
    /// Pairing creates the node's bucket and its default grant: the node writes.
    pub fn pairing(seq: u64, mesh: &str, node_id: &str) -> Self {
        Self { seq, principal: node_id.into(), store: node_store_name(mesh, node_id), access: Access::Write, op: GrantOp::Grant }
    }

    /// Unpairing: take back everything on the node's own store.
    pub fn unpair(seq: u64, mesh: &str, node_id: &str) -> Self {
        Self { seq, principal: node_id.into(), store: node_store_name(mesh, node_id), access: Access::Write, op: GrantOp::RevokeAll }
    }
}

/// ADR-126 section 2 name for a node's store.
pub fn node_store_name(mesh: &str, node_id: &str) -> String {
    format!("weftos://{mesh}/nodes/{node_id}/store")
}

/// The node id in `weftos://<mesh>/nodes/<id>/store`, if it is one.
fn node_of_store(store: &str) -> Option<&str> {
    let rest = store.strip_prefix("weftos://")?;
    let mut it = rest.split('/');
    let (_mesh, nodes, id, store_seg, extra) = (it.next()?, it.next()?, it.next()?, it.next()?, it.next());
    (nodes == "nodes" && store_seg == "store" && extra.is_none() && !id.is_empty()).then_some(id)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Applied {
    /// Seq not newer than the last applied event: ignored.
    Stale,
    /// Applied. `lost_write` is true when the principal could write its own
    /// node store before this event and cannot now: the caller must revoke.
    Changed { lost_write: bool },
}

pub(crate) struct Table {
    pub(crate) last_seq: u64,
    pub(crate) held: HashMap<(String, String), HashSet<Access>>,
    /// Hash chain over the events applied so far (compaction, ADR-126).
    pub(crate) events_head: String,
}

impl Default for Table {
    fn default() -> Self {
        Self { last_seq: 0, held: HashMap::new(), events_head: crate::compaction::genesis_head() }
    }
}

#[derive(Default)]
pub struct GrantLedger(pub(crate) Mutex<Table>);

impl GrantLedger {
    pub fn new() -> Self {
        Self::default()
    }

    /// Rebuild from chain events, in chain order.
    pub fn replay(events: impl IntoIterator<Item = GrantEvent>) -> Self {
        let l = Self::new();
        for e in events {
            l.apply(&e);
        }
        l
    }

    fn can(t: &Table, principal: &str, store: &str, access: Access) -> bool {
        t.held.get(&(principal.into(), store.into())).is_some_and(|h| {
            // Write implies read.
            h.contains(&access) || (access == Access::Read && h.contains(&Access::Write))
        })
    }

    /// Does `principal` hold `access` on `store`?
    pub fn holds(&self, principal: &str, store: &str, access: Access) -> bool {
        Self::can(&self.0.lock().expect("ledger"), principal, store, access)
    }

    pub fn apply(&self, ev: &GrantEvent) -> Applied {
        let mut t = self.0.lock().expect("ledger");
        if ev.seq <= t.last_seq {
            return Applied::Stale;
        }
        t.last_seq = ev.seq;
        t.events_head = crate::compaction::extend_head(&t.events_head, ev);
        let own = node_of_store(&ev.store) == Some(ev.principal.as_str());
        let could_write = own && Self::can(&t, &ev.principal, &ev.store, Access::Write);
        let key = (ev.principal.clone(), ev.store.clone());
        match ev.op {
            GrantOp::Grant => {
                t.held.entry(key).or_default().insert(ev.access);
            }
            GrantOp::Revoke => {
                if let Some(h) = t.held.get_mut(&key) {
                    h.remove(&ev.access);
                    if h.is_empty() {
                        t.held.remove(&key);
                    }
                }
            }
            GrantOp::RevokeAll => {
                t.held.remove(&key);
            }
        }
        let lost_write = could_write && !Self::can(&t, &ev.principal, &ev.store, Access::Write);
        Applied::Changed { lost_write }
    }
}

/// A node may use its own store when it holds the grant on it.
impl GrantCheck for GrantLedger {
    fn allows(&self, node_id: &str, access: Access) -> bool {
        let t = self.0.lock().expect("ledger");
        t.held.keys().any(|(p, store)| p == node_id && node_of_store(store) == Some(node_id) && Self::can(&t, p, store, access))
    }
}

impl GrantCheck for Arc<GrantLedger> {
    fn allows(&self, node_id: &str, access: Access) -> bool {
        GrantLedger::allows(self, node_id, access)
    }
}

/// Apply a chain event and enforce it: when the node lost write access, cut
/// it off now (revoked set plus RustFS user disable), without waiting for its
/// session to expire.
pub async fn apply_and_enforce<M: StsMinter, A: AuditSink, C: Clock>(
    ledger: &Arc<GrantLedger>,
    broker: &EdgeBroker<M, Arc<GrantLedger>, A, C>,
    ev: &GrantEvent,
) -> Result<Applied, crate::broker::StsError> {
    let applied = ledger.apply(ev);
    if matches!(applied, Applied::Changed { lost_write: true }) {
        broker.revoke(&ev.principal).await?;
    }
    Ok(applied)
}
