//! Where a client store's gate state lives (ADR-126 S5).
//!
//! The gate is rebuilt at open and every change is made durable through a [`GateState`]:
//!
//! - [`JournalState`] writes each change as a `store.client.gate.<kind>` event to an
//!   [`EventLog`] (the daemon's chain) and rebuilds the gate by replaying them in order. The
//!   chain is hash-linked and signed by the daemon, so the gate's history is tamper-evident
//!   and is the source of truth.
//! - [`FileState`] keeps a snapshot in `gate.json` under the client root. It is the fallback for
//!   a daemon with no chain, and what the tests use.
//!
//! Replay applies each event through [`StoreGate::apply`], the same checks as a live change, and
//! skips events that no longer apply (an expired cross-mesh grant). Events carry the mesh and
//! account they were written for, and replay ignores any other, so a shared chain cannot feed
//! one account's gate another's grants.

use std::path::PathBuf;
use std::sync::Mutex;

use serde_json::{Value, json};

use crate::store_account::{GateEvent, StoreGate};
use crate::weftos_uri::Authority;

/// The chain event source for gate changes.
pub const SOURCE: &str = "store.client";
const KIND_PREFIX: &str = "store.client.gate.";

/// Durable gate state.
pub trait GateState: Send + Sync {
    /// Rebuild the gate for `mesh` and `account`. `now` is unix seconds.
    fn load(&self, mesh: Authority, account: &str, now: u64) -> Result<StoreGate, String>;
    /// Make one change durable. `gate` is the state after the change.
    fn record(&self, gate: &StoreGate, ev: &GateEvent) -> Result<(), String>;
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
    pub source: String,
    pub kind: String,
    pub payload: Option<Value>,
}

/// An append-only, ordered event log. The daemon's chain implements it.
pub trait EventLog: Send + Sync {
    fn append(&self, source: &str, kind: &str, payload: Value);
    /// Every event, oldest first.
    fn all(&self) -> Vec<LoggedEvent>;
}

/// The gate as an event journal on an [`EventLog`].
pub struct JournalState {
    log: std::sync::Arc<dyn EventLog>,
    account: String,
    mesh: Authority,
}

impl JournalState {
    pub fn new(log: std::sync::Arc<dyn EventLog>, mesh: Authority, account: &str) -> Self {
        Self { log, account: account.to_owned(), mesh }
    }
}

impl GateState for JournalState {
    fn load(&self, mesh: Authority, account: &str, now: u64) -> Result<StoreGate, String> {
        if mesh != self.mesh || account != self.account {
            return Err("journal belongs to another mesh or account".into());
        }
        let mut gate = StoreGate::new(mesh, account);
        let want_mesh = hex::encode(mesh.0);
        for e in self.log.all() {
            let Some(kind) = e.kind.strip_prefix(KIND_PREFIX).filter(|_| e.source == SOURCE) else { continue };
            let Some(p) = e.payload else { continue };
            if p.get("account").and_then(Value::as_str) != Some(account) || p.get("mesh").and_then(Value::as_str) != Some(&want_mesh) {
                continue;
            }
            // An event that does not parse is a corrupt record for this account: refuse to open.
            let ev = GateEvent::from_json(kind, p.get("ev").unwrap_or(&Value::Null))?;
            // One that parses but no longer applies (expired, replayed) is history, not an error.
            let _ = gate.apply(&ev, now);
        }
        Ok(gate)
    }

    fn record(&self, _gate: &StoreGate, ev: &GateEvent) -> Result<(), String> {
        let kind = format!("{KIND_PREFIX}{}", ev.kind());
        self.log.append(SOURCE, &kind, json!({"account": self.account, "mesh": hex::encode(self.mesh.0), "ev": ev.to_json()}));
        Ok(())
    }
}

/// An in-memory log, for tests.
#[derive(Default)]
pub struct MemLog(Mutex<Vec<LoggedEvent>>);

impl MemLog {
    pub fn len(&self) -> usize {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Append a raw record, as another writer on a shared chain would.
    pub fn push(&self, e: LoggedEvent) {
        self.0.lock().unwrap_or_else(|x| x.into_inner()).push(e);
    }
}

impl EventLog for MemLog {
    fn append(&self, source: &str, kind: &str, payload: Value) {
        self.push(LoggedEvent { source: source.into(), kind: kind.into(), payload: Some(payload) });
    }
    fn all(&self) -> Vec<LoggedEvent> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}
