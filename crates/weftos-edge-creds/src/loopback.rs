//! A loopback chain for the compaction spike (ADR-126, "Chain compaction").
//!
//! It stands in for the real chain so snapshot publication, recovery,
//! archive and prune can be exercised end to end without one: an in-memory,
//! append-only list of grant events and signed snapshots. No I/O, no clock
//! (the caller passes `now`), and it exists only behind the `loopback-chain`
//! feature (or in tests). It is not a model of the real chain's storage or
//! signing; the chain owner wires `GrantLedger::snapshot` to the real thing.
//!
//! Nothing here runs on its own. A snapshot is taken only by
//! [`LoopbackChain::snapshot_now`], or by [`LoopbackChain::append`] when the
//! chain was built with an enabled [`SnapshotSchedule`]; the default is off.

use crate::compaction::{verify_against_history, SignedSnapshot, SnapshotError, SnapshotSchedule, SnapshotVerifier};
use crate::grants::{Applied, GrantEvent, GrantLedger};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChainEntry {
    Grant(GrantEvent),
    Snapshot(SignedSnapshot),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ChainError {
    #[error("event seq {0} is not after the chain's last grant event")]
    OutOfOrder(u64),
    #[error("no snapshot to take: nothing has been appended")]
    Empty,
    #[error("the newest usable snapshot is not archived, or the archive does not reproduce it")]
    NotArchived,
    #[error("no usable snapshot, and the events before it were pruned")]
    NoUsableSnapshot,
    #[error("snapshot: {0}")]
    Snapshot(#[from] SnapshotError),
}

/// What recovery did, so a test can see that the tail was short.
pub struct Recovered {
    pub ledger: GrantLedger,
    /// `through_seq` of the snapshot used, or `None` for a full replay.
    pub from_snapshot: Option<u64>,
    /// Snapshots newer than the one used that were refused.
    pub refused: usize,
    /// Grant events replayed on top.
    pub tail_len: usize,
}

/// What one `compact` call did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Compacted {
    pub snapshot_taken: bool,
    pub archived_through: u64,
    pub pruned_entries: usize,
}

type Signer = Box<dyn Fn(&[u8]) -> Vec<u8> + Send + Sync>;

pub struct LoopbackChain {
    scope: String,
    signer_id: String,
    sign: Signer,
    schedule: SnapshotSchedule,
    entries: Vec<ChainEntry>,
    ledger: GrantLedger,
    since_snapshot: u64,
    last_snapshot_at: u64,
    /// Cold storage: every grant event up to `archived_through`, in order.
    archive: Vec<GrantEvent>,
    archived_through: u64,
    /// Grant events with `seq <= pruned_through` are gone from `entries`.
    pruned_through: u64,
}

impl LoopbackChain {
    /// A chain with the schedule off.
    pub fn new(scope: &str, signer_id: &str, sign: impl Fn(&[u8]) -> Vec<u8> + Send + Sync + 'static) -> Self {
        Self {
            scope: scope.into(),
            signer_id: signer_id.into(),
            sign: Box::new(sign),
            schedule: SnapshotSchedule::off(),
            entries: Vec::new(),
            ledger: GrantLedger::new(),
            since_snapshot: 0,
            last_snapshot_at: 0,
            archive: Vec::new(),
            archived_through: 0,
            pruned_through: 0,
        }
    }

    pub fn with_schedule(mut self, schedule: SnapshotSchedule) -> Self {
        self.schedule = schedule;
        self
    }

    pub fn entries(&self) -> &[ChainEntry] {
        &self.entries
    }

    pub fn ledger(&self) -> &GrantLedger {
        &self.ledger
    }

    pub fn snapshots(&self) -> Vec<&SignedSnapshot> {
        self.entries
            .iter()
            .filter_map(|e| if let ChainEntry::Snapshot(s) = e { Some(s) } else { None })
            .collect()
    }

    /// Append a grant event in chain order, then take a snapshot if the
    /// schedule says it is due. A stale or repeated `seq` is refused: the real
    /// chain does not reorder.
    pub fn append(&mut self, ev: GrantEvent, now: u64) -> Result<Applied, ChainError> {
        let applied = self.ledger.apply(&ev);
        if matches!(applied, Applied::Stale | Applied::OutOfRange) {
            return Err(ChainError::OutOfOrder(ev.seq));
        }
        self.entries.push(ChainEntry::Grant(ev));
        if self.since_snapshot == 0 && self.last_snapshot_at == 0 {
            self.last_snapshot_at = now;
        }
        self.since_snapshot += 1;
        if self.schedule.due(self.since_snapshot, now.saturating_sub(self.last_snapshot_at)) {
            self.snapshot_now(now)?;
        }
        Ok(applied)
    }

    /// Take a snapshot if the schedule says one is due. For a host timer: the
    /// time rule can come due with no append. Off by default; `due` is never true when
    /// nothing is new since the last snapshot. Returns whether one was taken.
    pub fn tick(&mut self, now: u64) -> Result<bool, ChainError> {
        if !self.schedule.due(self.since_snapshot, now.saturating_sub(self.last_snapshot_at)) {
            return Ok(false);
        }
        self.snapshot_now(now)?;
        Ok(true)
    }

    /// The owner-run compaction step: snapshot (if anything is new), archive,
    /// then prune. The schedule never calls this; only a person or an
    /// explicitly enabled job does. Each stage refuses to go on if the one
    /// before it did not hold, so a failure leaves the chain whole.
    pub fn compact(&mut self, verifier: &dyn SnapshotVerifier, now: u64) -> Result<Compacted, ChainError> {
        let snapshot_taken = self.since_snapshot > 0 && self.snapshot_now(now).is_ok();
        let archived_through = self.archive(verifier)?;
        let pruned_entries = self.prune(verifier)?;
        Ok(Compacted { snapshot_taken, archived_through, pruned_entries })
    }

    /// Fold, sign and append a snapshot of the current ledger. A second call
    /// with nothing new returns the existing newest snapshot instead of
    /// appending a duplicate.
    pub fn snapshot_now(&mut self, now: u64) -> Result<SignedSnapshot, ChainError> {
        if self.ledger.snapshot(&self.scope).through_seq == 0 {
            return Err(ChainError::Empty);
        }
        if let (0, Some(last)) = (self.since_snapshot, self.snapshots().last()) {
            return Ok((*last).clone());
        }
        let signed = self.ledger.signed_snapshot(&self.scope, &self.signer_id, |m| (self.sign)(m));
        self.entries.push(ChainEntry::Snapshot(signed.clone()));
        self.since_snapshot = 0;
        self.last_snapshot_at = now;
        Ok(signed)
    }

    fn grants_after(&self, seq: u64) -> Vec<GrantEvent> {
        self.entries
            .iter()
            .filter_map(|e| if let ChainEntry::Grant(g) = e { Some(g) } else { None })
            .filter(|g| g.seq > seq)
            .cloned()
            .collect()
    }

    /// Newest snapshot that passes the follower checks and whose tail is
    /// still on the chain.
    fn newest_usable(&self, verifier: &dyn SnapshotVerifier) -> (Option<&SignedSnapshot>, usize) {
        let mut refused = 0;
        for s in self.snapshots().into_iter().rev() {
            let ok = GrantLedger::check(s, &self.scope, verifier).is_ok() && s.snapshot.through_seq >= self.pruned_through;
            if ok {
                return (Some(s), refused);
            }
            refused += 1;
        }
        (None, refused)
    }

    /// Rebuild the ledger the way a fresh follower would: newest snapshot it
    /// will accept, plus the tail. Falls back to an older snapshot, then to a
    /// full replay while nothing has been pruned.
    pub fn recover(&self, verifier: &dyn SnapshotVerifier) -> Result<Recovered, ChainError> {
        let (found, refused) = self.newest_usable(verifier);
        if let Some(s) = found {
            let tail = self.grants_after(s.snapshot.through_seq);
            let ledger = GrantLedger::restore(s, &self.scope, verifier, &tail)?;
            return Ok(Recovered { ledger, from_snapshot: Some(s.snapshot.through_seq), refused, tail_len: tail.len() });
        }
        if self.pruned_through > 0 {
            return Err(ChainError::NoUsableSnapshot);
        }
        let all = self.grants_after(0);
        Ok(Recovered { ledger: GrantLedger::replay(all.iter().cloned()), from_snapshot: None, refused, tail_len: all.len() })
    }

    /// Copy the events up to the newest usable snapshot to cold storage, and
    /// check that they reproduce it (`verify_against_history`). Archive first,
    /// prune second; this never removes anything from the chain.
    pub fn archive(&mut self, verifier: &dyn SnapshotVerifier) -> Result<u64, ChainError> {
        let (found, _) = self.newest_usable(verifier);
        let snap = found.ok_or(ChainError::NoUsableSnapshot)?.snapshot.clone();
        let mut staged = self.archive.clone();
        staged.extend(self.grants_after(self.archived_through).into_iter().filter(|g| g.seq <= snap.through_seq));
        verify_against_history(&snap, &staged)?;
        self.archive = staged;
        self.archived_through = snap.through_seq;
        Ok(snap.through_seq)
    }

    /// Drop the grant events covered by the newest usable snapshot, and only
    /// if the archive reproduces it. Returns how many entries were removed.
    pub fn prune(&mut self, verifier: &dyn SnapshotVerifier) -> Result<usize, ChainError> {
        let (found, _) = self.newest_usable(verifier);
        let snap = found.ok_or(ChainError::NoUsableSnapshot)?.snapshot.clone();
        let archived: Vec<GrantEvent> = self.archive.iter().filter(|g| g.seq <= snap.through_seq).cloned().collect();
        if self.archived_through < snap.through_seq || verify_against_history(&snap, &archived).is_err() {
            return Err(ChainError::NotArchived);
        }
        let before = self.entries.len();
        self.entries.retain(|e| !matches!(e, ChainEntry::Grant(g) if g.seq <= snap.through_seq));
        self.pruned_through = snap.through_seq;
        Ok(before - self.entries.len())
    }

    #[cfg(test)]
    pub(crate) fn entries_mut(&mut self) -> &mut Vec<ChainEntry> {
        &mut self.entries
    }
    #[cfg(test)]
    pub(crate) fn archive_mut(&mut self) -> &mut Vec<GrantEvent> {
        &mut self.archive
    }
}
