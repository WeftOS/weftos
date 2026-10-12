//! The node's instance registry (ADR-121 §4, O2): one record per running agent
//! or team run, chained and summarised in the dashboard heartbeat.
//!
//! Runners register in two ways: push (`start`, `set_state`, `stop`, ...) for a
//! runner that owns its lifecycle, or pull by implementing
//! [`crate::instance_sources::InstanceSource`] and adding it with
//! [`InstanceRegistry::add_source`]. Every start, stop, failure, version change,
//! config change and digest drift is a chain record (source `instance`); a
//! record never carries a transcript, a goal or a config body.
//!
//! Starting an instance that names a package directory and a pinned digest
//! first verifies the files ([`crate::instance_digest`]); a mismatch refuses the
//! start, chains `instance.drift` and is kept for `report.instances.drift`.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use clawft_kernel::chain::ChainManager;
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::instance_digest::{DigestError, PackageDigest, Sha256SortedFiles, verify};
use crate::instance_sources::InstanceSource;

/// Chain source of every instance record.
pub const CHAIN_SOURCE: &str = "instance";
/// Chain kinds.
pub const CHAIN_START: &str = "instance.start";
pub const CHAIN_STOP: &str = "instance.stop";
pub const CHAIN_FAIL: &str = "instance.fail";
pub const CHAIN_VERSION: &str = "instance.version";
pub const CHAIN_CONFIG: &str = "instance.config";
pub const CHAIN_DRIFT: &str = "instance.drift";
/// Most records kept (oldest finished are evicted first).
pub const MAX_RECORDS: usize = 512;
/// Most drift records kept.
pub const MAX_DRIFT: usize = 32;
/// Most instances in the heartbeat summary.
pub const SUMMARY_RECENT: usize = 20;

/// Lifecycle state (ADR-121 / ai-org-model §6.3, collapsed for the report).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum InstanceState {
    Starting,
    Idle,
    Working,
    Exited,
    Failed,
}

impl InstanceState {
    /// Exited or failed.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Exited | Self::Failed)
    }
}

/// The package an instance runs: id plus the digest the installation pinned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PackageRef {
    pub id: String,
    pub digest: String,
}

/// A request to register a new instance.
#[derive(Debug, Clone, Default)]
pub struct NewInstance {
    /// Which runner started it (`daemon-subagent`, `harness-job`, `team-run`, ...).
    pub runner: String,
    pub package: Option<PackageRef>,
    /// Installed package directory; with `package`, verified against the pin before start.
    pub package_dir: Option<PathBuf>,
    /// Version label of the package.
    pub version: Option<String>,
    pub project: Option<String>,
    pub slot: Option<String>,
    pub model_role: Option<String>,
    pub model: Option<String>,
    pub card: Option<String>,
    /// Instance this one replaces.
    pub restart_of: Option<String>,
}

/// One registered instance (the heartbeat shape; no transcript).
#[derive(Debug, Clone, Serialize)]
pub struct InstanceRecord {
    pub instance_id: String,
    pub runner: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub package: Option<PackageRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slot: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub state: InstanceState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub card: Option<String>,
    pub started_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restart_of: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_hash: Option<String>,
}

/// A refused start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartRefusal {
    /// The package files do not match the pinned digest (or could not be read).
    Drift(DigestError),
    /// A package directory was given without a package pin.
    NoPin,
}

impl std::fmt::Display for StartRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Drift(e) => write!(f, "start refused: {e}"),
            Self::NoPin => write!(f, "start refused: package_dir given without a pinned package digest"),
        }
    }
}

/// The id names no known instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownInstance(pub String);

/// A refused start, kept so the dashboard can show the drift.
#[derive(Debug, Clone, Serialize)]
pub struct DriftRecord {
    pub package: String,
    pub expected: String,
    /// What the files hash to; absent when they could not be read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual: Option<String>,
    pub reason: String,
    pub at: String,
}

#[derive(Default)]
pub(crate) struct Inner {
    pub(crate) records: Vec<InstanceRecord>,
    pub(crate) drift: Vec<DriftRecord>,
    /// `(runner, source key)` -> instance id, for pulled sources.
    pub(crate) keys: std::collections::HashMap<(String, String), String>,
}

/// The registry. Cheap to share; all methods take `&self`.
pub struct InstanceRegistry {
    pub(crate) inner: Mutex<Inner>,
    chain: Option<Arc<ChainManager>>,
    digester: Arc<dyn PackageDigest>,
    node: OnceLock<String>,
    pub(crate) sources: Mutex<Vec<Arc<dyn InstanceSource>>>,
}

static GLOBAL: OnceLock<Arc<InstanceRegistry>> = OnceLock::new();

/// The daemon's registry, once [`install_global`] ran.
pub fn global() -> Option<Arc<InstanceRegistry>> {
    GLOBAL.get().cloned()
}

/// Make `r` the daemon's registry (first call wins).
pub fn install_global(r: Arc<InstanceRegistry>) -> bool {
    GLOBAL.set(r).is_ok()
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

impl InstanceRegistry {
    /// A registry chaining on `chain` (`None`: nothing is chained, a warning is logged per event).
    pub fn new(chain: Option<Arc<ChainManager>>) -> Arc<Self> {
        Self::with_digester(chain, Arc::new(Sha256SortedFiles))
    }

    /// [`Self::new`] with the catalog's digest definition.
    pub fn with_digester(chain: Option<Arc<ChainManager>>, digester: Arc<dyn PackageDigest>) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner::default()),
            chain,
            digester,
            node: OnceLock::new(),
            sources: Mutex::new(Vec::new()),
        })
    }

    /// This node's id as the dashboard knows it (first call wins); stamped on new records.
    pub fn set_node(&self, node: &str) {
        let _ = self.node.set(node.to_owned());
    }

    pub(crate) fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn chain(&self, kind: &str, payload: Value) {
        match &self.chain {
            Some(cm) => {
                cm.append(CHAIN_SOURCE, kind, Some(payload));
            }
            None => tracing::warn!(kind, "no chain on this node: instance event not chained"),
        }
    }

    /// Register and start an instance. With a package directory, the files are
    /// verified against the pin first; a mismatch refuses the start.
    pub fn start(&self, new: NewInstance) -> Result<String, StartRefusal> {
        let mut verified = None;
        if let Some(dir) = &new.package_dir {
            let Some(pkg) = &new.package else { return Err(StartRefusal::NoPin) };
            match verify(self.digester.as_ref(), dir, &pkg.digest) {
                Ok(d) => verified = Some(d),
                Err(e) => {
                    self.record_drift(&pkg.id, &pkg.digest, &e);
                    return Err(StartRefusal::Drift(e));
                }
            }
        }
        let id = ulid::Ulid::new().to_string();
        let rec = InstanceRecord {
            instance_id: id.clone(),
            runner: new.runner,
            package: new.package,
            version: new.version,
            project: new.project,
            node: self.node.get().cloned(),
            slot: new.slot,
            model_role: new.model_role,
            model: new.model,
            state: InstanceState::Starting,
            card: new.card,
            started_at: now(),
            ended_at: None,
            restart_of: new.restart_of,
            config_hash: None,
        };
        self.chain(CHAIN_START, json!({ "record": rec, "digest_verified": verified.is_some() }));
        let mut g = self.lock();
        if g.records.len() >= MAX_RECORDS {
            if let Some(i) = g.records.iter().position(|r| r.state.is_terminal()) {
                g.records.remove(i);
            } else {
                g.records.remove(0);
            }
        }
        g.records.push(rec);
        if let Some(p) = g.records.last().and_then(|r| r.package.as_ref()).map(|p| p.id.clone()) {
            g.drift.retain(|d| d.package != p);
        }
        Ok(id)
    }

    fn record_drift(&self, package: &str, expected: &str, e: &DigestError) {
        let (actual, reason) = match e {
            DigestError::Mismatch { actual, .. } => (Some(actual.clone()), "digest mismatch"),
            DigestError::Unreadable(_) => (None, "package files unreadable"),
            DigestError::BadPin(_) => (None, "pinned digest malformed"),
        };
        let d = DriftRecord {
            package: package.to_owned(),
            expected: expected.to_owned(),
            actual,
            reason: reason.to_owned(),
            at: now(),
        };
        self.chain(CHAIN_DRIFT, json!({ "drift": d, "refused": true }));
        let mut g = self.lock();
        g.drift.retain(|x| x.package != package);
        if g.drift.len() >= MAX_DRIFT {
            g.drift.remove(0);
        }
        g.drift.push(d);
    }

    fn update<T>(&self, id: &str, f: impl FnOnce(&mut InstanceRecord) -> T) -> Result<T, UnknownInstance> {
        let mut g = self.lock();
        g.records.iter_mut().find(|r| r.instance_id == id).map(f).ok_or_else(|| UnknownInstance(id.to_owned()))
    }

    /// Move a live instance between `starting`, `idle` and `working`. A terminal
    /// state goes through [`Self::stop`] / [`Self::fail`]; setting one here, or
    /// moving a finished instance, is ignored (`Ok(false)`).
    pub fn set_state(&self, id: &str, state: InstanceState) -> Result<bool, UnknownInstance> {
        self.update(id, |r| {
            if r.state.is_terminal() || state.is_terminal() {
                return false;
            }
            r.state = state;
            true
        })
    }

    /// Set (or clear) the card the instance works on.
    pub fn set_card(&self, id: &str, card: Option<String>) -> Result<(), UnknownInstance> {
        self.update(id, |r| r.card = card)
    }

    /// The instance exited normally (or was stopped); `outcome` is `done`, `recycled` or `stopped`.
    pub fn stop(&self, id: &str, outcome: &str) -> Result<(), UnknownInstance> {
        self.finish(id, InstanceState::Exited, CHAIN_STOP, json!({ "outcome": outcome }))
    }

    /// The instance failed; `reason` is a short message, never a transcript.
    pub fn fail(&self, id: &str, reason: &str) -> Result<(), UnknownInstance> {
        let reason: String = reason.chars().take(200).collect();
        self.finish(id, InstanceState::Failed, CHAIN_FAIL, json!({ "reason": reason }))
    }

    fn finish(&self, id: &str, state: InstanceState, kind: &str, extra: Value) -> Result<(), UnknownInstance> {
        let rec = self.update(id, |r| {
            if r.state.is_terminal() {
                return None;
            }
            r.state = state;
            r.ended_at = Some(now());
            Some(r.clone())
        })?;
        if let Some(r) = rec {
            let mut p = json!({ "instance_id": r.instance_id, "runner": r.runner, "package": r.package, "project": r.project, "slot": r.slot, "card": r.card, "started_at": r.started_at });
            if let (Some(o), Some(x)) = (p.as_object_mut(), extra.as_object()) {
                o.extend(x.clone());
            }
            self.chain(kind, p);
        }
        Ok(())
    }

    /// The running instance moved to another package version/digest or model.
    pub fn set_version(&self, id: &str, version: Option<String>, package: Option<PackageRef>, model: Option<String>) -> Result<(), UnknownInstance> {
        let (from, to) = self.update(id, |r| {
            let from = json!({ "version": r.version, "package": r.package, "model": r.model });
            r.version = version;
            r.package = package;
            r.model = model;
            (from, json!({ "version": r.version, "package": r.package, "model": r.model }))
        })?;
        self.chain(CHAIN_VERSION, json!({ "instance_id": id, "from": from, "to": to }));
        Ok(())
    }

    /// The instance's configuration changed. Only a hash of `config` is kept or chained.
    pub fn set_config(&self, id: &str, config: &str) -> Result<(), UnknownInstance> {
        let hash = format!("sha256:{}", hex::encode(Sha256::digest(config.as_bytes())));
        let from = self.update(id, |r| r.config_hash.replace(hash.clone()))?;
        if from.as_deref() != Some(hash.as_str()) {
            self.chain(CHAIN_CONFIG, json!({ "instance_id": id, "from": from, "to": hash }));
        }
        Ok(())
    }

    /// Every record, oldest first.
    pub fn list(&self) -> Vec<InstanceRecord> {
        self.lock().records.clone()
    }

    /// One record.
    pub fn get(&self, id: &str) -> Option<InstanceRecord> {
        self.lock().records.iter().find(|r| r.instance_id == id).cloned()
    }

    /// Current drift records.
    pub fn drift(&self) -> Vec<DriftRecord> {
        self.lock().drift.clone()
    }

    /// `report.instances`: counts per state, the newest [`SUMMARY_RECENT`]
    /// instances (live first), and current drift. No transcripts.
    pub fn summary(&self) -> Value {
        let g = self.lock();
        let n = |s: InstanceState| g.records.iter().filter(|r| r.state == s).count();
        let mut recent: Vec<&InstanceRecord> = g.records.iter().rev().collect();
        recent.sort_by_key(|r| r.state.is_terminal());
        recent.truncate(SUMMARY_RECENT);
        json!({
            "total": g.records.len(),
            "counts": {
                "starting": n(InstanceState::Starting), "idle": n(InstanceState::Idle),
                "working": n(InstanceState::Working), "exited": n(InstanceState::Exited),
                "failed": n(InstanceState::Failed),
            },
            "recent": recent,
            "drift": g.drift,
        })
    }
}

#[cfg(test)]
#[path = "instance_registry_tests.rs"]
mod tests;
