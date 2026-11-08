//! Where a client store's gate state lives (ADR-126 S5).
//!
//! The gate is rebuilt at open and every change is made durable through a [`GateState`]:
//!
//! - [`JournalState`] writes each change as a `store.client.gate.<kind>` event to an
//!   [`EventLog`] (the daemon's chain) and rebuilds the gate by replaying them in order. The
//!   chain is hash-linked and signed by the daemon, so the gate's history is tamper-evident
//!   and is the source of truth. To keep replay short it publishes signed snapshots
//!   ([`crate::client_store_snapshot`]) and restores from the newest one that verifies.
//! - [`FileState`] keeps a snapshot in `gate.json` under the client root. It is the fallback for
//!   a daemon with no chain, and what most tests use. It has no history to compact.
//!
//! Replay applies each event through [`StoreGate::apply`], the same checks as a live change.
//! Events carry the mesh and account they were written for, and replay ignores any other, so a
//! shared chain cannot feed one account's gate another's grants.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use crate::client_store_snapshot::{
    GateSnapshot, SNAPSHOT_KIND, SNAPSHOT_VERSION, SignedGateSnapshot, SnapshotError, SnapshotKey, event_bytes, genesis_head,
    head_extend, scope_of, signing_bytes, state_hash, verify_against_history, verify_snapshot,
};
use crate::client_store_archive::{ArchiveHeader, ArchiveReport, read_archive, write_archive};
use crate::store_account::{GateEvent, StoreGate};
use crate::weftos_uri::Authority;

/// The chain event source for gate changes and snapshots.
pub const SOURCE: &str = "store.client";
const KIND_PREFIX: &str = "store.client.gate.";

/// Durable gate state.
pub trait GateState: Send + Sync {
    /// Rebuild the gate for `mesh` and `account`. `now` is unix seconds.
    fn load(&self, mesh: Authority, account: &str, now: u64) -> Result<StoreGate, String>;
    /// Make one change durable. `gate` is the state after the change.
    fn record(&self, gate: &StoreGate, ev: &GateEvent) -> Result<(), String>;
    /// Publish a snapshot of `gate` taken at `now`; returns the chain sequence it covers, or
    /// `None` when this state has no history to compact.
    fn snapshot(&self, _gate: &StoreGate, _now: u64) -> Result<Option<u64>, String> {
        Ok(None)
    }
    /// Events recorded since the last snapshot (or since open, with none).
    fn events_since_snapshot(&self) -> u64 {
        0
    }
    /// Write the segment the newest verified snapshot covers to `dir`; `None` when there is
    /// nothing to archive (no journal, or no verified snapshot yet).
    fn archive(&self, _dir: &std::path::Path) -> Result<Option<ArchiveReport>, String> {
        Ok(None)
    }
    /// Check an archive file against the snapshots on the chain.
    fn verify_archive(&self, _path: &std::path::Path) -> Result<ArchiveHeader, String> {
        Err("this client store keeps its gate in gate.json; there is no chain history to archive".into())
    }
}

/// A snapshot file; ignores the individual event.
pub struct FileState {
    pub path: PathBuf,
}

impl GateState for FileState {
    fn load(&self, mesh: Authority, account: &str, _now: u64) -> Result<StoreGate, String> {
        StoreGate::load(mesh, account, &self.path)
    }
    fn record(&self, gate: &StoreGate, _ev: &GateEvent) -> Result<(), String> {
        gate.save(&self.path).map_err(|e| e.to_string())
    }
}

/// One event read back from the log.
#[derive(Debug, Clone)]
pub struct LoggedEvent {
    /// The log's own sequence number; increasing in log order.
    pub seq: u64,
    pub source: String,
    pub kind: String,
    pub payload: Option<Value>,
}

/// An append-only, ordered event log. The daemon's chain implements it.
pub trait EventLog: Send + Sync {
    /// Append an event; returns the sequence number it was given.
    fn append(&self, source: &str, kind: &str, payload: Value) -> u64;
    /// Every event, oldest first.
    fn all(&self) -> Vec<LoggedEvent>;
}

/// What has been folded into the gate so far.
struct Folded {
    head: String,
    count: u64,
    through_seq: u64,
    since_snapshot: u64,
}

/// The gate as an event journal on an [`EventLog`].
pub struct JournalState {
    log: Arc<dyn EventLog>,
    account: String,
    mesh: Authority,
    key: Option<Arc<dyn SnapshotKey>>,
    folded: Mutex<Folded>,
}

fn bad(e: SnapshotError) -> String {
    e.to_string()
}

impl JournalState {
    pub fn new(log: Arc<dyn EventLog>, mesh: Authority, account: &str) -> Self {
        let folded = Mutex::new(Folded { head: genesis_head(), count: 0, through_seq: 0, since_snapshot: 0 });
        Self { log, account: account.to_owned(), mesh, key: None, folded }
    }

    /// Sign and verify snapshots with this key. Without one, none is published and every
    /// snapshot on the chain is skipped, so the gate replays from the first event.
    pub fn with_key(mut self, key: Arc<dyn SnapshotKey>) -> Self {
        self.key = Some(key);
        self
    }

    fn folded(&self) -> std::sync::MutexGuard<'_, Folded> {
        self.folded.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// A scope event read from the log: `(seq, kind suffix, ev)`.
type ScopeEvent = (u64, String, Value);

type Scan = (Vec<ScopeEvent>, Vec<(u64, SignedGateSnapshot)>);

impl JournalState {
    /// This account's gate events and snapshot records, in log order.
    fn scan(&self) -> Result<Scan, String> {
        let mesh_hex = hex::encode(self.mesh.0);
        let mut events: Vec<ScopeEvent> = Vec::new();
        let mut snaps: Vec<(u64, SignedGateSnapshot)> = Vec::new();
        for e in self.log.all() {
            if e.source != SOURCE {
                continue;
            }
            let Some(p) = e.payload else { continue };
            if p.get("account").and_then(Value::as_str) != Some(&self.account) || p.get("mesh").and_then(Value::as_str) != Some(&mesh_hex) {
                continue;
            }
            if e.kind == SNAPSHOT_KIND {
                // A snapshot record for this account that does not parse is a corrupt record.
                let signed = serde_json::from_value(p.get("signed").cloned().unwrap_or(Value::Null)).map_err(|e| format!("snapshot record: {e}"))?;
                snaps.push((e.seq, signed));
            } else if let Some(kind) = e.kind.strip_prefix(KIND_PREFIX) {
                events.push((e.seq, kind.to_owned(), p.get("ev").cloned().unwrap_or(Value::Null)));
            }
        }
        Ok((events, snaps))
    }

    /// The verified snapshot that covers the most (greatest `through_seq`; the last one on a
    /// tie), not the newest by position, so replaying an older record cannot roll the gate back.
    /// One signed by a key we no longer hold is passed over; any other failure means the chain's
    /// record is wrong.
    fn best_snapshot(&self, snaps: &[(u64, SignedGateSnapshot)]) -> Result<Option<usize>, String> {
        let (key, scope) = (self.key.as_deref(), scope_of(&self.account, &self.mesh));
        let mut best: Option<usize> = None;
        for (i, (at, s)) in snaps.iter().enumerate() {
            match verify_snapshot(s, &scope, key, *at) {
                Ok(()) => {
                    if best.is_none_or(|b| s.snapshot.through_seq >= snaps[b].1.snapshot.through_seq) {
                        best = Some(i);
                    }
                }
                Err(SnapshotError::BadSignature | SnapshotError::NoKey) => {}
                Err(e) => return Err(bad(e)),
            }
        }
        Ok(best)
    }

    /// Write the segment the newest verified snapshot covers to `dir` (see
    /// [`crate::client_store_archive`]). The chain's copy is the owner's to prune afterwards,
    /// once [`verify_archive`](Self::verify_archive) agrees.
    pub fn archive(&self, dir: &std::path::Path) -> Result<Option<ArchiveReport>, String> {
        let (events, snaps) = self.scan()?;
        let Some(i) = self.best_snapshot(&snaps)? else { return Ok(None) };
        let snap = &snaps[i].1.snapshot;
        let seg: Vec<(u64, &str, &Value)> = events.iter().filter(|(q, ..)| *q <= snap.through_seq).map(|(q, k, v)| (*q, k.as_str(), v)).collect();
        write_archive(dir, snap, &seg).map(Some)
    }

    /// Does the archive file at `path` hold exactly what a verified snapshot on the chain covers?
    /// This is the check before pruning: the file is self-consistent and matches the signed
    /// snapshot's range, head and count.
    pub fn verify_archive(&self, path: &std::path::Path) -> Result<ArchiveHeader, String> {
        let (header, _) = read_archive(path)?;
        let (_, snaps) = self.scan()?;
        let scope = scope_of(&self.account, &self.mesh);
        let key = self.key.as_deref();
        let matched = snaps.iter().any(|(at, s)| {
            let n = &s.snapshot;
            verify_snapshot(s, &scope, key, *at).is_ok()
                && (n.through_seq, n.event_count, n.events_head.as_str()) == (header.through_seq, header.event_count, header.events_head.as_str())
        });
        if !matched {
            return Err("no verified snapshot on the chain matches this archive".into());
        }
        Ok(header)
    }
}

impl GateState for JournalState {
    fn load(&self, mesh: Authority, account: &str, now: u64) -> Result<StoreGate, String> {
        if mesh != self.mesh || account != self.account {
            return Err("journal belongs to another mesh or account".into());
        }
        let (events, snaps) = self.scan()?;
        let best = self.best_snapshot(&snaps)?;
        let start = best.map(|i| &snaps[i].1.snapshot);

        // The events between where we start and any snapshot that claims more must all still be
        // here. Unverified snapshots are used only for this, only to say "something is missing":
        // a lie can make the store refuse to open (an appender can already do that with a garbled
        // event), never open on less than the chain holds.
        let upto = |q: u64| events.iter().filter(|(s, ..)| *s <= q).count() as u64;
        let (from, from_count) = start.map_or((0, 0), |s| (s.through_seq, s.event_count));
        for s in snaps.iter().map(|(_, s)| &s.snapshot).filter(|s| s.through_seq > from || start.is_none()) {
            if upto(s.through_seq) - upto(from) != s.event_count.saturating_sub(from_count) {
                return Err("a snapshot covers events that are no longer here (history pruned past the snapshot we can verify)".into());
            }
        }

        let (mut gate, mut folded) = match start {
            Some(snap) => {
                let history: Vec<(&str, &Value)> = events.iter().filter(|(q, ..)| *q <= snap.through_seq).map(|(_, k, v)| (k.as_str(), v)).collect();
                if history.len() as u64 > snap.event_count {
                    return Err(bad(SnapshotError::HistoryMismatch("more history than the snapshot counts")));
                }
                // Every event is still here: do not trust the signer for the past.
                if history.len() as u64 == snap.event_count {
                    verify_against_history(snap, mesh, account, &history).map_err(bad)?;
                }
                let gate = StoreGate::from_state_json(mesh, account, &snap.state)?;
                (gate, Folded { head: snap.events_head.clone(), count: snap.event_count, through_seq: snap.through_seq, since_snapshot: 0 })
            }
            None => (StoreGate::new(mesh, account), Folded { head: genesis_head(), count: 0, through_seq: 0, since_snapshot: 0 }),
        };

        for (seq, kind, ev) in events.iter().filter(|(q, ..)| *q > from) {
            // An event for this account that does not parse is corrupt: refuse to open.
            let parsed = GateEvent::from_json(kind, ev)?;
            // One that parses but is refused (a replayed or forged install) is history, not an error.
            let _ = gate.apply(&parsed);
            folded.head = head_extend(&folded.head, &event_bytes(kind, ev));
            folded.count += 1;
            folded.through_seq = *seq;
            folded.since_snapshot += 1;
        }
        let _ = now;
        *self.folded() = folded;
        Ok(gate)
    }

    fn record(&self, _gate: &StoreGate, ev: &GateEvent) -> Result<(), String> {
        let (kind, body) = (ev.kind(), ev.to_json());
        let seq = self.log.append(
            SOURCE,
            &format!("{KIND_PREFIX}{kind}"),
            json!({"account": self.account, "mesh": hex::encode(self.mesh.0), "ev": body}),
        );
        let mut f = self.folded();
        f.head = head_extend(&f.head, &event_bytes(kind, &body));
        f.count += 1;
        f.through_seq = seq;
        f.since_snapshot += 1;
        Ok(())
    }

    fn snapshot(&self, gate: &StoreGate, now: u64) -> Result<Option<u64>, String> {
        let key = self.key.as_ref().ok_or("no snapshot key: this daemon has no chain signing key")?;
        let mut f = self.folded();
        let state = gate.state_json(now);
        let snapshot = GateSnapshot {
            version: SNAPSHOT_VERSION,
            scope: scope_of(&self.account, &self.mesh),
            through_seq: f.through_seq,
            event_count: f.count,
            events_head: f.head.clone(),
            taken_at: now,
            state_hash: state_hash(&state),
            state,
        };
        let (signer, sig) = key.sign(&signing_bytes(&snapshot)).ok_or("the chain key refused to sign the snapshot")?;
        let through = snapshot.through_seq;
        let signed = SignedGateSnapshot { snapshot, signer, signature: hex::encode(sig) };
        self.log.append(
            SOURCE,
            SNAPSHOT_KIND,
            json!({"account": self.account, "mesh": hex::encode(self.mesh.0), "signed": serde_json::to_value(&signed).map_err(|e| e.to_string())?}),
        );
        f.since_snapshot = 0;
        Ok(Some(through))
    }

    fn events_since_snapshot(&self) -> u64 {
        self.folded().since_snapshot
    }

    fn archive(&self, dir: &std::path::Path) -> Result<Option<ArchiveReport>, String> {
        JournalState::archive(self, dir)
    }

    fn verify_archive(&self, path: &std::path::Path) -> Result<ArchiveHeader, String> {
        JournalState::verify_archive(self, path)
    }
}

/// An in-memory log, for tests. Sequence numbers only go up, as on a real chain, even if
/// events are removed (pruned).
#[derive(Default)]
pub struct MemLog {
    events: Mutex<Vec<LoggedEvent>>,
    last_seq: std::sync::atomic::AtomicU64,
}

impl MemLog {
    pub fn len(&self) -> usize {
        self.events.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Append a raw record, as another writer on a shared chain would; its `seq` is assigned.
    pub fn push(&self, mut e: LoggedEvent) -> u64 {
        let mut v = self.events.lock().unwrap_or_else(|x| x.into_inner());
        e.seq = self.last_seq.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        let seq = e.seq;
        v.push(e);
        seq
    }
    /// Edit the stored events in place (tests that tamper with the chain's record).
    pub fn edit(&self, f: impl FnOnce(&mut Vec<LoggedEvent>)) {
        f(&mut self.events.lock().unwrap_or_else(|x| x.into_inner()));
    }
}

impl EventLog for MemLog {
    fn append(&self, source: &str, kind: &str, payload: Value) -> u64 {
        self.push(LoggedEvent { seq: 0, source: source.into(), kind: kind.into(), payload: Some(payload) })
    }
    fn all(&self) -> Vec<LoggedEvent> {
        self.events.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}
