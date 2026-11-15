//! CRDT-backed store for vault hyperedge proposals (WEFT-378).
//!
//! Last-writer-wins (LWW) map keyed by proposal id, with a per-replica
//! logical clock. Concurrent updates converge deterministically: higher
//! clock wins; equal clocks break ties by lexicographic actor id (remote
//! wins on equal clock + greater actor, local kept otherwise — same rule as
//! kernel mesh gossip).
//!
//! The store is intentionally dependency-free (no kernel-bridge required)
//! so vault cultivation works in standalone graphify builds.

use serde::{Deserialize, Serialize};

use super::hyperedge::{HyperedgeProposal, ProposalLifecycle};

// ---------------------------------------------------------------------------
// Entry + delta
// ---------------------------------------------------------------------------

/// One CRDT entry: a proposal snapshot + metadata for LWW merge.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CrdtEntry {
    pub proposal: HyperedgeProposal,
    /// Logical clock when this version was written.
    pub clock: u64,
    /// Replica / actor that wrote this version.
    pub actor: String,
}

/// Wire format for gossip / multi-replica merge.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CrdtDelta {
    pub entries: Vec<CrdtEntry>,
    /// Sender's clock after producing the delta.
    pub sender_clock: u64,
    pub sender_actor: String,
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

/// LWW-map CRDT for vault hyperedge proposals.
#[derive(Debug, Clone)]
pub struct CrdtHyperedgeStore {
    entries: std::collections::HashMap<String, CrdtEntry>,
    clock: u64,
    actor: String,
}

impl CrdtHyperedgeStore {
    /// Create an empty store for `actor` (replica id).
    pub fn new(actor: impl Into<String>) -> Self {
        Self {
            entries: std::collections::HashMap::new(),
            clock: 0,
            actor: actor.into(),
        }
    }

    pub fn actor(&self) -> &str {
        &self.actor
    }

    pub fn clock(&self) -> u64 {
        self.clock
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Insert or replace a proposal (local write; advances clock).
    pub fn put(&mut self, proposal: HyperedgeProposal) {
        self.clock += 1;
        let id = proposal.id.clone();
        self.entries.insert(
            id,
            CrdtEntry {
                proposal,
                clock: self.clock,
                actor: self.actor.clone(),
            },
        );
    }

    /// Batch put of SUGGEST output. Skips ids that already exist with a
    /// non-Suggested status (do not clobber ratified/rejected decisions).
    pub fn put_suggestions(&mut self, proposals: Vec<HyperedgeProposal>) -> usize {
        let mut written = 0;
        for p in proposals {
            if let Some(existing) = self.entries.get(&p.id)
                && existing.proposal.status != ProposalLifecycle::Suggested
            {
                continue;
            }
            self.put(p);
            written += 1;
        }
        written
    }

    pub fn get(&self, id: &str) -> Option<&CrdtEntry> {
        self.entries.get(id)
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut CrdtEntry> {
        self.entries.get_mut(id)
    }

    /// All entries (unordered).
    pub fn entries(&self) -> impl Iterator<Item = &CrdtEntry> {
        self.entries.values()
    }

    /// Update lifecycle status (local write).
    pub fn set_status(
        &mut self,
        id: &str,
        status: ProposalLifecycle,
    ) -> Result<&HyperedgeProposal, CrdtError> {
        let entry = self
            .entries
            .get(id)
            .ok_or_else(|| CrdtError::NotFound(id.to_string()))?;
        let mut proposal = entry.proposal.clone();
        proposal.status = status;
        self.put(proposal);
        Ok(&self.entries.get(id).expect("just inserted").proposal)
    }

    /// Merge a remote delta (LWW).
    pub fn merge(&mut self, delta: &CrdtDelta) -> MergeStats {
        let mut stats = MergeStats::default();
        for remote in &delta.entries {
            stats.considered += 1;
            match self.entries.get(&remote.proposal.id) {
                Some(local) if !remote_wins(local, remote) => {
                    stats.kept_local += 1;
                }
                _ => {
                    self.entries
                        .insert(remote.proposal.id.clone(), remote.clone());
                    if remote.clock > self.clock {
                        self.clock = remote.clock;
                    }
                    stats.applied += 1;
                }
            }
        }
        stats
    }

    /// Entries with clock > `since` (for gossip).
    pub fn delta_since(&self, since: u64) -> CrdtDelta {
        let entries: Vec<CrdtEntry> = self
            .entries
            .values()
            .filter(|e| e.clock > since)
            .cloned()
            .collect();
        CrdtDelta {
            entries,
            sender_clock: self.clock,
            sender_actor: self.actor.clone(),
        }
    }

    /// Full snapshot as a delta (clock 0 → now).
    pub fn snapshot(&self) -> CrdtDelta {
        self.delta_since(0)
    }

    /// Proposals currently in `Suggested` state (for ratify UI).
    pub fn pending(&self) -> Vec<&HyperedgeProposal> {
        let mut v: Vec<_> = self
            .entries
            .values()
            .filter(|e| e.proposal.status == ProposalLifecycle::Suggested)
            .map(|e| &e.proposal)
            .collect();
        v.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.id.cmp(&b.id))
        });
        v
    }

    /// Ratified proposals only.
    pub fn ratified(&self) -> Vec<&HyperedgeProposal> {
        self.entries
            .values()
            .filter(|e| e.proposal.status == ProposalLifecycle::Ratified)
            .map(|e| &e.proposal)
            .collect()
    }
}

impl Default for CrdtHyperedgeStore {
    fn default() -> Self {
        Self::new("local")
    }
}

/// Whether remote should replace local under LWW + actor tie-break.
fn remote_wins(local: &CrdtEntry, remote: &CrdtEntry) -> bool {
    if remote.clock > local.clock {
        return true;
    }
    if remote.clock < local.clock {
        return false;
    }
    // Equal clock: higher actor string wins (deterministic).
    remote.actor > local.actor
}

/// Stats from a merge operation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MergeStats {
    pub considered: usize,
    pub applied: usize,
    pub kept_local: usize,
}

/// CRDT store errors.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CrdtError {
    #[error("proposal not found: {0}")]
    NotFound(String),
    #[error("invalid transition: {0}")]
    InvalidTransition(String),
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::hyperedge::{HyperedgeKind, HyperedgeProposal, ProposalLifecycle};

    fn sample(id_suffix: &str, status: ProposalLifecycle) -> HyperedgeProposal {
        let mut p = HyperedgeProposal::new(
            HyperedgeKind::SharedTags,
            vec![
                format!("a{id_suffix}.md"),
                format!("b{id_suffix}.md"),
                format!("c{id_suffix}.md"),
            ],
            format!("tag:test{id_suffix}"),
            8.0,
            "test",
            vec!["test".into()],
        );
        p.status = status;
        p
    }

    #[test]
    fn put_and_get() {
        let mut store = CrdtHyperedgeStore::new("alice");
        let p = sample("1", ProposalLifecycle::Suggested);
        let id = p.id.clone();
        store.put(p);
        assert_eq!(store.len(), 1);
        assert_eq!(store.clock(), 1);
        assert_eq!(
            store.get(&id).unwrap().proposal.status,
            ProposalLifecycle::Suggested
        );
    }

    #[test]
    fn ratify_advances_clock() {
        let mut store = CrdtHyperedgeStore::new("alice");
        let p = sample("1", ProposalLifecycle::Suggested);
        let id = p.id.clone();
        store.put(p);
        store.set_status(&id, ProposalLifecycle::Ratified).unwrap();
        assert_eq!(store.clock(), 2);
        assert_eq!(
            store.get(&id).unwrap().proposal.status,
            ProposalLifecycle::Ratified
        );
        assert_eq!(store.ratified().len(), 1);
        assert!(store.pending().is_empty());
    }

    #[test]
    fn concurrent_replicas_converge() {
        let mut a = CrdtHyperedgeStore::new("alice");
        let mut b = CrdtHyperedgeStore::new("bob");
        let p = sample("x", ProposalLifecycle::Suggested);
        let id = p.id.clone();

        a.put(p.clone());
        b.put(p);

        // Alice ratifies; Bob rejects — higher clock after merge of both.
        a.set_status(&id, ProposalLifecycle::Ratified).unwrap();
        // Bob's reject at same relative step; after merge of a's delta into b
        // and b's into a, clocks differ per replica history.
        b.set_status(&id, ProposalLifecycle::Rejected).unwrap();

        // Full sync both ways.
        let da = a.snapshot();
        let db = b.snapshot();
        a.merge(&db);
        b.merge(&da);

        // After full mesh, both should agree on the entry with the higher
        // (clock, actor) — bob > alice at equal clock, so Rejected wins when
        // both advanced one step from the same put clock... Actually:
        // alice: put c=1, ratify c=2
        // bob:   put c=1, reject c=2
        // equal clock 2, bob > alice → Rejected
        assert_eq!(
            a.get(&id).unwrap().proposal.status,
            ProposalLifecycle::Rejected
        );
        assert_eq!(
            b.get(&id).unwrap().proposal.status,
            ProposalLifecycle::Rejected
        );
    }

    #[test]
    fn higher_clock_wins() {
        let mut a = CrdtHyperedgeStore::new("alice");
        let mut b = CrdtHyperedgeStore::new("bob");
        let p = sample("y", ProposalLifecycle::Suggested);
        let id = p.id.clone();
        a.put(p.clone());
        b.put(p);

        // Alice does two status flips → clock 3.
        a.set_status(&id, ProposalLifecycle::Rejected).unwrap();
        a.set_status(&id, ProposalLifecycle::Ratified).unwrap();
        // Bob only rejects once → clock 2.
        b.set_status(&id, ProposalLifecycle::Rejected).unwrap();

        b.merge(&a.snapshot());
        a.merge(&b.snapshot());
        assert_eq!(
            a.get(&id).unwrap().proposal.status,
            ProposalLifecycle::Ratified
        );
        assert_eq!(
            b.get(&id).unwrap().proposal.status,
            ProposalLifecycle::Ratified
        );
    }

    #[test]
    fn put_suggestions_skips_ratified() {
        let mut store = CrdtHyperedgeStore::new("local");
        let p = sample("z", ProposalLifecycle::Suggested);
        let id = p.id.clone();
        store.put(p.clone());
        store.set_status(&id, ProposalLifecycle::Ratified).unwrap();

        let mut again = p;
        again.status = ProposalLifecycle::Suggested;
        again.score = 99.0;
        let n = store.put_suggestions(vec![again]);
        assert_eq!(n, 0);
        assert_eq!(
            store.get(&id).unwrap().proposal.status,
            ProposalLifecycle::Ratified
        );
    }

    #[test]
    fn delta_since_filters() {
        let mut store = CrdtHyperedgeStore::new("local");
        store.put(sample("1", ProposalLifecycle::Suggested));
        let c1 = store.clock();
        store.put(sample("2", ProposalLifecycle::Suggested));
        let d = store.delta_since(c1);
        assert_eq!(d.entries.len(), 1);
    }
}
