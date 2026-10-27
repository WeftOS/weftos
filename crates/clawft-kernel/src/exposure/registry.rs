//! The exposure registry (ADR-120 section 2 and 4).
//!
//! Definitions come from code and configuration ([`ExposureRegistry::register`]);
//! operator decisions (switch off, pause, narrow the audience) are overrides
//! that are persisted and win over a later registration, so a locally
//! switched-off exposure stays off across restarts and re-registration.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::activity::{ActivityLog, ActivityRecord, NewActivity, Retention};
use super::{Audience, Exposure, ExposureKind, ExposureState, Limits, Refusal};

/// Sink for chain audit events (kind, payload).
pub type AuditFn = std::sync::Arc<dyn Fn(&str, Value) + Send + Sync>;

/// Persisted operator decisions.
const STATE_FILE: &str = "exposures.json";
/// Activity log file.
const LOG_FILE: &str = "exposure-activity.jsonl";
/// A refusal or every Nth record is anchored on the chain.
const ANCHOR_EVERY: u64 = 64;
/// Window of the report's counts.
pub const DAY_MS: u64 = 24 * 3600 * 1000;

/// Unix milliseconds now.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// What code or configuration wants an exposure to be.
#[derive(Debug, Clone, PartialEq)]
pub struct ExposureSpec {
    /// Stable id.
    pub id: String,
    /// Kind.
    pub kind: ExposureKind,
    /// Resource used.
    pub resource: String,
    /// Estimated load, percent.
    pub load_pct: Option<u8>,
    /// Audience.
    pub audience: Audience,
    /// Protocol.
    pub api: String,
    /// Limits.
    pub limits: Limits,
    /// Desired state.
    pub state: ExposureState,
}

impl ExposureSpec {
    /// A spec with the safe defaults: audience `local`, no limits, state on.
    pub fn new(id: &str, kind: ExposureKind, resource: &str, api: &str) -> Self {
        Self {
            id: id.into(),
            kind,
            resource: resource.into(),
            load_pct: None,
            audience: Audience::Local,
            api: api.into(),
            limits: Limits::default(),
            state: ExposureState::On,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct Override {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    state: Option<ExposureState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    audience: Option<Audience>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    raw: Option<bool>,
}

#[derive(Default, Serialize, Deserialize)]
struct StateFile {
    #[serde(default)]
    overrides: BTreeMap<String, Override>,
}

#[derive(Default)]
struct Inner {
    map: BTreeMap<String, Exposure>,
    overrides: BTreeMap<String, Override>,
    /// Advertisement names to withdraw from peers.
    withdrawn: Vec<String>,
    since_anchor: u64,
}

/// The registry.
pub struct ExposureRegistry {
    node_id: String,
    master: AtomicBool,
    inner: Mutex<Inner>,
    state_path: Option<PathBuf>,
    log: ActivityLog,
    audit: Mutex<Option<AuditFn>>,
}

fn servable(master: bool, e: &Exposure) -> Result<(), Refusal> {
    if !master {
        return Err(Refusal::MasterOff);
    }
    match e.state {
        ExposureState::Off => return Err(Refusal::Off),
        ExposureState::Paused => return Err(Refusal::Paused),
        ExposureState::On => {}
    }
    if !e.audience.is_remote() {
        return Err(Refusal::LocalOnly);
    }
    if !e.announced {
        return Err(Refusal::NotAnnounced);
    }
    Ok(())
}

impl ExposureRegistry {
    /// In-memory registry (tests, nodes without a runtime directory).
    pub fn new(node_id: &str) -> Self {
        Self {
            node_id: node_id.into(),
            master: AtomicBool::new(true),
            inner: Mutex::new(Inner::default()),
            state_path: None,
            log: ActivityLog::memory(Retention::default()),
            audit: Mutex::new(None),
        }
    }

    /// Registry persisted under `dir`. A corrupt state file or a broken
    /// activity chain is an `Err`, never a silent reset.
    pub fn open(node_id: &str, dir: &Path, retention: Retention, now_ms: u64) -> Result<Self, String> {
        let state_path = dir.join(STATE_FILE);
        let mut inner = Inner::default();
        if state_path.exists() {
            let text = std::fs::read_to_string(&state_path).map_err(|e| format!("{}: {e}", state_path.display()))?;
            let f: StateFile = serde_json::from_str(&text).map_err(|e| format!("{}: {e}", state_path.display()))?;
            inner.overrides = f.overrides;
        }
        let log = ActivityLog::open(&dir.join(LOG_FILE), retention, now_ms)?;
        Ok(Self {
            node_id: node_id.into(),
            master: AtomicBool::new(true),
            inner: Mutex::new(inner),
            state_path: Some(state_path),
            log,
            audit: Mutex::new(None),
        })
    }

    /// Where chain audit events go.
    pub fn set_audit(&self, f: AuditFn) {
        *self.audit.lock().unwrap_or_else(|e| e.into_inner()) = Some(f);
    }

    fn audit(&self, kind: &str, payload: Value) {
        let f = self.audit.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(f) = f {
            f(kind, payload);
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn persist(&self, g: &Inner) {
        let Some(p) = &self.state_path else { return };
        let body = serde_json::to_string_pretty(&StateFile { overrides: g.overrides.clone() }).unwrap_or_default();
        let tmp = p.with_extension("json.tmp");
        if let Err(e) = std::fs::write(&tmp, body).and_then(|()| std::fs::rename(&tmp, p)) {
            tracing::warn!(path = %p.display(), error = %e, "exposure state write failed");
        }
    }

    /// This node's id.
    pub fn node_id(&self) -> &str {
        &self.node_id
    }

    /// Master switch (`[mesh.expose] enabled`).
    pub fn master_enabled(&self) -> bool {
        self.master.load(Ordering::SeqCst)
    }

    /// Set the master switch. Turning it off withdraws every announced exposure.
    pub fn set_master(&self, on: bool) {
        let was = self.master.swap(on, Ordering::SeqCst);
        if was && !on {
            let mut g = self.lock();
            let ids: Vec<String> = g.map.values().filter(|e| e.announced).map(|e| e.id.clone()).collect();
            g.withdrawn.extend(ids);
        }
        if was != on {
            self.audit("exposure.master", json!({"node_id": self.node_id, "enabled": on}));
        }
    }

    /// Define or update an exposure. An operator override of state or
    /// audience wins over the spec. Returns the exposure as registered.
    pub fn register(&self, spec: ExposureSpec) -> Exposure {
        let mut g = self.lock();
        let ov = g.overrides.get(&spec.id).cloned().unwrap_or_default();
        let announced = g.map.get(&spec.id).is_some_and(|e| e.announced);
        let e = Exposure {
            id: spec.id.clone(),
            kind: spec.kind,
            resource: spec.resource,
            load_pct: spec.load_pct,
            audience: ov.audience.unwrap_or(spec.audience),
            api: spec.api,
            limits: spec.limits,
            state: ov.state.unwrap_or(spec.state),
            announced,
            raw: ov.raw.unwrap_or(false),
        };
        g.map.insert(spec.id, e.clone());
        e
    }

    /// Record that the announcer can (or no longer can) publish an
    /// advertisement for `id`. False if `id` is not registered.
    pub fn set_announced(&self, id: &str, announced: bool) -> bool {
        match self.lock().map.get_mut(id) {
            Some(e) => {
                e.announced = announced;
                true
            }
            None => false,
        }
    }

    /// May `id` be served now? The one answer every serving path consults.
    pub fn check_serve(&self, id: &str) -> Result<(), Refusal> {
        let master = self.master_enabled();
        match self.lock().map.get(id) {
            Some(e) => servable(master, e),
            None => Err(if master { Refusal::Unregistered } else { Refusal::MasterOff }),
        }
    }

    /// One exposure.
    pub fn get(&self, id: &str) -> Option<Exposure> {
        self.lock().map.get(id).cloned()
    }

    /// Every exposure, sorted by id.
    pub fn list(&self) -> Vec<Exposure> {
        self.lock().map.values().cloned().collect()
    }

    fn mutate(&self, id: &str, actor: &str, f: impl FnOnce(&mut Exposure, &mut Override)) -> Result<Exposure, String> {
        let master = self.master_enabled();
        let mut g = self.lock();
        let Some(mut e) = g.map.get(id).cloned() else {
            return Err(format!("unknown exposure {id:?}"));
        };
        let was = servable(master, &e).is_ok();
        let mut ov = g.overrides.get(id).cloned().unwrap_or_default();
        f(&mut e, &mut ov);
        let now = servable(master, &e).is_ok();
        if was && !now {
            g.withdrawn.push(id.to_owned());
        }
        g.overrides.insert(id.to_owned(), ov);
        g.map.insert(id.to_owned(), e.clone());
        self.persist(&g);
        drop(g);
        self.audit(
            "exposure.change",
            json!({"node_id": self.node_id, "id": id, "state": e.state.as_str(), "audience": e.audience.label(), "raw": e.raw, "actor": actor}),
        );
        Ok(e)
    }

    /// Operator decision on the state of `id`; persisted, wins over config.
    pub fn set_state(&self, id: &str, state: ExposureState, actor: &str) -> Result<Exposure, String> {
        self.mutate(id, actor, |e, ov| {
            e.state = state;
            ov.state = Some(state);
        })
    }

    /// Operator decision on the audience of `id`.
    pub fn set_audience(&self, id: &str, audience: Audience, actor: &str) -> Result<Exposure, String> {
        self.mutate(id, actor, |e, ov| {
            e.audience = audience.clone();
            ov.audience = Some(audience);
        })
    }

    /// Operator decision on raw data for `id` (a topic exposure); persisted
    /// and audited like the others. Turning it off withdraws raw subscribers.
    pub fn set_raw(&self, id: &str, raw: bool, actor: &str) -> Result<Exposure, String> {
        let was = self.get(id).is_some_and(|e| e.raw);
        let e = self.mutate(id, actor, |e, ov| {
            e.raw = raw;
            ov.raw = Some(raw);
        })?;
        if was && !raw {
            self.queue_withdrawal(id);
        }
        Ok(e)
    }

    /// Switch every exposure off. Returns the ids that were not already off.
    pub fn off_all(&self, actor: &str) -> Vec<String> {
        let ids: Vec<String> = self.list().into_iter().filter(|e| e.state != ExposureState::Off).map(|e| e.id).collect();
        for id in &ids {
            let _ = self.set_state(id, ExposureState::Off, actor);
        }
        ids
    }

    /// Ask for `id`'s advertisement to be withdrawn from peers (it stopped
    /// being exposed by a route other than a state change).
    pub fn queue_withdrawal(&self, id: &str) {
        self.lock().withdrawn.push(id.to_owned());
    }

    /// Advertisement names to withdraw from peers since the last call.
    pub fn drain_withdrawals(&self) -> Vec<String> {
        let mut v = std::mem::take(&mut self.lock().withdrawn);
        v.sort();
        v.dedup();
        v
    }

    /// Record one request (never its body) and return the chained record.
    pub fn record(&self, a: NewActivity) -> ActivityRecord {
        let r = self.log.append(a, now_ms());
        let anchor = {
            let mut g = self.lock();
            g.since_anchor += 1;
            let due = r.failed() || g.since_anchor >= ANCHOR_EVERY;
            if due {
                g.since_anchor = 0;
            }
            due
        };
        if anchor {
            self.audit(
                "exposure.activity.anchor",
                json!({"node_id": self.node_id, "exposure": r.exposure, "seq": r.seq, "hash": r.hash, "outcome": r.outcome}),
            );
        }
        r
    }

    /// The activity log.
    pub fn activity(&self) -> &ActivityLog {
        &self.log
    }

    /// Compact report section `exposures`: id, kind, audience, state,
    /// last_used and 24 h counts. Full logs stay on the node.
    pub fn report(&self, now_ms: u64) -> Value {
        let counts = self.log.counts(now_ms, DAY_MS);
        let items: Vec<Value> = self
            .list()
            .into_iter()
            .map(|e| {
                let c = counts.get(&e.id).cloned().unwrap_or_default();
                json!({
                    "id": e.id,
                    "kind": e.kind,
                    "audience": e.audience.label(),
                    "state": e.state.as_str(),
                    "announced": e.announced,
                    "last_used": c.last_ts_ms,
                    "n24": c.total,
                    "err24": c.failed,
                })
            })
            .collect();
        json!({"enabled": self.master_enabled(), "items": items})
    }
}
