//! The host's instance journal: what a node needs to put its placed
//! workloads back after the daemon restarts.
//!
//! A host keeps its instances in memory, so a restart used to empty the
//! node: the staged binaries stayed on disk, nothing ran, and placing the
//! cog again failed on the stale stage directory. The journal records, for
//! each placed instance, the `place` body, who placed it and whether it was
//! meant to be running. At startup [`WorkloadHostService::restore_instances`]
//! places each one again from the package already in this node's artifact
//! store, through the same path (and the same governance) as the original
//! `place`: a new instance id is never invented, a new cogbus token and
//! ingest lease are issued, and a cog that was running is started.
//!
//! The file holds no secrets: no token, no key, only the request body.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::json;

use super::host_service::{PlaceBody, WorkloadHostService};
use super::msg::{CTL_VERSION, CtlRequest, method};
use crate::mesh_artifact_transfer::PeerSet;

/// Journal file version.
const VERSION: u32 = 1;
/// Largest journal read, bytes.
const MAX_BYTES: u64 = 4 * 1024 * 1024;

/// One placed instance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JournalEntry {
    pub instance_id: String,
    pub decision_id: Option<String>,
    /// Controller node that placed it.
    pub requester: String,
    /// What was placed (its `start` is replaced by `desired_running` on restore).
    pub body: PlaceBody,
    /// It was meant to be running.
    pub desired_running: bool,
}

#[derive(Serialize, Deserialize)]
struct File {
    version: u32,
    entries: Vec<JournalEntry>,
}

pub(super) struct Journal {
    path: PathBuf,
    entries: Mutex<BTreeMap<String, JournalEntry>>,
}

impl Journal {
    /// Open the journal at `path`. A missing file is empty; one that does
    /// not parse is set aside (`.bad`) and the journal starts empty, so a
    /// damaged file never blocks the daemon.
    pub(super) fn open(path: &Path) -> Self {
        let entries = match read(path) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "instance journal unreadable; starting empty");
                let _ = std::fs::rename(path, path.with_extension("json.bad"));
                BTreeMap::new()
            }
        };
        Self {
            path: path.to_path_buf(),
            entries: Mutex::new(entries),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, JournalEntry>> {
        self.entries.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub(super) fn entries(&self) -> Vec<JournalEntry> {
        self.lock().values().cloned().collect()
    }

    pub(super) fn record(&self, e: JournalEntry) {
        let mut g = self.lock();
        g.insert(e.instance_id.clone(), e);
        self.save(&g);
    }

    pub(super) fn set_desired(&self, instance_id: &str, running: bool) {
        let mut g = self.lock();
        if let Some(e) = g.get_mut(instance_id)
            && e.desired_running != running
        {
            e.desired_running = running;
            self.save(&g);
        }
    }

    pub(super) fn remove(&self, instance_id: &str) {
        let mut g = self.lock();
        if g.remove(instance_id).is_some() {
            self.save(&g);
        }
    }

    fn save(&self, g: &BTreeMap<String, JournalEntry>) {
        let file = File {
            version: VERSION,
            entries: g.values().cloned().collect(),
        };
        let tmp = self.path.with_extension("json.tmp");
        let write = || -> std::io::Result<()> {
            use std::os::unix::fs::PermissionsExt;
            let text = serde_json::to_vec_pretty(&file).map_err(std::io::Error::other)?;
            std::fs::write(&tmp, text)?;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
            std::fs::rename(&tmp, &self.path)
        };
        if let Err(e) = write() {
            tracing::warn!(path = %self.path.display(), error = %e, "instance journal not saved");
        }
    }
}

fn read(path: &Path) -> Result<BTreeMap<String, JournalEntry>, String> {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(e) => return Err(e.to_string()),
    };
    if meta.len() > MAX_BYTES {
        return Err("journal too large".into());
    }
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let f: File = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    if f.version != VERSION {
        return Err(format!("journal version {}", f.version));
    }
    Ok(f.entries.into_iter().map(|e| (e.instance_id.clone(), e)).collect())
}

/// What a restore did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RestoreReport {
    /// Placed again.
    pub restored: Vec<String>,
    /// Could not be placed again, with the reason (dropped from the journal).
    pub failed: Vec<(String, String)>,
}

impl WorkloadHostService {
    /// Journal placed instances at `path`, so [`Self::restore_instances`]
    /// can put them back after a restart.
    pub fn with_journal(mut self, path: &Path) -> Self {
        self.journal = Some(Journal::open(path));
        self
    }

    /// Place every journaled instance again, started if it was meant to be
    /// running. Call once at startup, after the cogbus and ingest wiring are
    /// set. An instance that cannot be placed again (its package is gone
    /// from the store, governance now refuses it) is dropped and reported;
    /// the controller's reconcile re-places it.
    pub async fn restore_instances(&self) -> RestoreReport {
        let mut report = RestoreReport::default();
        let Some(j) = &self.journal else { return report };
        for e in j.entries() {
            let mut body = e.body.clone();
            body.start = e.desired_running;
            let now = super::host_service::now_ms();
            let req = CtlRequest {
                version: CTL_VERSION,
                method: method::PLACE.to_string(),
                requester: e.requester.clone(),
                target: self.node_id.clone(),
                nonce: "0".repeat(32),
                issued_at_ms: now,
                expires_at_ms: now + 60_000,
                decision_id: e.decision_id.clone(),
                body: json!(body),
            };
            // The package is in this node's own artifact store: no peers.
            let mut none = PeerSet::new();
            match self.place(&req, Some(&mut none)).await {
                Ok(_) => {
                    tracing::info!(instance = %e.instance_id, running = e.desired_running, "placed instance restored");
                    report.restored.push(e.instance_id);
                }
                Err(r) => {
                    tracing::warn!(instance = %e.instance_id, reason = %r.reason, "placed instance not restored; dropped from the journal");
                    j.remove(&e.instance_id);
                    report.failed.push((e.instance_id, r.reason));
                }
            }
        }
        report
    }

    /// Instances the host holds: `(id, desired_running)`.
    #[cfg(test)]
    pub(super) async fn held(&self) -> Vec<(String, bool)> {
        let mut v: Vec<_> = self
            .instances
            .lock()
            .await
            .iter()
            .map(|(i, p)| (i.clone(), p.desired_running))
            .collect();
        v.sort();
        v
    }
}
