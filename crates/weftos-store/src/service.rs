//! The store service: creates stores, applies grants into the backend, and runs every governed
//! request through exposure, grant and audit (ADR-126 sections 3 and 6).
//!
//! Enforcement is in two places. The service refuses a request whose grant is missing or
//! disabled before the backend sees it, and every grant change is pushed into the backend as a
//! key state and a policy, so a direct S3 request with that key is refused as well.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::access::{Access, Grant, GrantTable, Op, Principal};
use crate::audit::{AuditEvent, AuditSink, Outcome, Source};
use crate::backend::{AdminBackend, BackendError, ObjectBackend, ObjectMeta};
use crate::name::StoreName;
use crate::{StoreError, policy};

mod ingest;
mod lifecycle;

/// The bucket archived stores are copied into, under `<bucket>/<key>`.
pub const ARCHIVE_BUCKET: &str = "weftos-archive";

/// Key of an archived object: `<bucket>@<archive time ms>/<key>`, so a second archive of a
/// re-created store never overwrites the first.
pub fn archive_key(bucket: &str, ts_ms: u64, key: &str) -> String {
    format!("{bucket}@{ts_ms}/{key}")
}

/// ADR-120 exposure of a store on this host. The daemon backs it with its exposure registry.
pub trait ExposureGate: Send + Sync {
    /// Make the store an exposure (switchable, listed). Called when a store is created or loaded.
    fn register(&self, id: &str, resource: &str);
    /// May this store be served to the mesh now? `Err` carries the reason.
    fn check(&self, id: &str) -> Result<(), String>;
}

/// Where grant and lifecycle decisions are recorded on the chain.
pub trait ChainSink: Send + Sync {
    fn record(&self, kind: &str, payload: Value);
}

/// An exposure gate that allows everything (tests, and a daemon with no exposure registry).
pub struct OpenGate;
impl ExposureGate for OpenGate {
    fn register(&self, _: &str, _: &str) {}
    fn check(&self, _: &str) -> Result<(), String> {
        Ok(())
    }
}

/// A chain sink that drops everything.
pub struct NoChain;
impl ChainSink for NoChain {
    fn record(&self, _: &str, _: Value) {}
}

/// Who is asking: a principal, and whether the request arrived from the mesh.
#[derive(Debug, Clone)]
pub struct Caller {
    pub principal: Principal,
    /// From another node. A remote request needs the store's exposure to be on.
    pub remote: bool,
}

impl Caller {
    pub fn local(principal: Principal) -> Self {
        Self { principal, remote: false }
    }
    pub fn mesh(principal: Principal) -> Self {
        Self { principal, remote: true }
    }
}

/// Who gets what when a store is created.
#[derive(Debug, Clone, Default)]
pub struct Parties {
    pub admins: Vec<Principal>,
    pub writers: Vec<Principal>,
    pub readers: Vec<Principal>,
}

impl Parties {
    /// The defaults of ADR-126 section 3 that the name alone decides: a node's own principal
    /// writes its node store. Everything else is named by the caller.
    pub fn defaults_for(name: &StoreName) -> Self {
        let mut p = Self::default();
        if let StoreName::Node { node, .. } = name
            && let Ok(pr) = Principal::new(crate::PrincipalKind::Node, node)
        {
            p.writers.push(pr);
        }
        p
    }
}

#[derive(Serialize, Deserialize, Clone)]
struct StoreRec {
    name: String,
    created_ms: u64,
    #[serde(default)]
    archived: bool,
}

#[derive(Serialize, Deserialize, Default)]
struct State {
    grants: GrantTable,
    stores: BTreeMap<String, StoreRec>,
    /// principal -> access key id
    keys: BTreeMap<String, String>,
    suspended: BTreeSet<Principal>,
}

/// One store as `weaver store ls` shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StoreInfo {
    pub name: String,
    pub bucket: String,
    pub kind: String,
    pub objects: u64,
    pub bytes: u64,
    pub grants: Vec<Grant>,
    pub archived: bool,
    pub exposure: String,
}

/// What `promote` did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct PromoteReport {
    pub promoted: u64,
    /// Already in the project store with the same content.
    pub unchanged: u64,
    /// The project store held other content under the same key and it was replaced.
    pub replaced: Vec<String>,
    /// Not under a promotable prefix, and left in the worktree store.
    pub left: u64,
    /// Not under a promotable prefix, and copied to the archive.
    pub archived: u64,
}

/// A change a notification reports, for cache invalidation on nodes and mounts.
pub type Invalidator = Arc<dyn Fn(&str, &str) + Send + Sync>;

/// Settings the service needs.
pub struct Config {
    pub node_id: String,
    pub state_path: Option<PathBuf>,
    pub clock: fn() -> u64,
}

/// The service.
pub struct StoreService {
    cfg: Config,
    admin: Arc<dyn AdminBackend>,
    objects: Arc<dyn ObjectBackend>,
    exposure: Arc<dyn ExposureGate>,
    audit: Arc<dyn AuditSink>,
    chain: Arc<dyn ChainSink>,
    invalidator: Mutex<Option<Invalidator>>,
    st: Mutex<State>,
}

/// The exposure id of a store.
pub fn exposure_id(name: &StoreName) -> String {
    format!("store.{}", name.bucket())
}

impl StoreService {
    /// Start the service, loading state from `cfg.state_path` (a broken file is an error).
    pub fn open(
        cfg: Config,
        admin: Arc<dyn AdminBackend>,
        objects: Arc<dyn ObjectBackend>,
        exposure: Arc<dyn ExposureGate>,
        audit: Arc<dyn AuditSink>,
        chain: Arc<dyn ChainSink>,
    ) -> Result<Self, StoreError> {
        let st: State = match &cfg.state_path {
            Some(p) => match std::fs::read(p) {
                Ok(b) => serde_json::from_slice(&b).map_err(|e| StoreError::State(format!("{}: {e}", p.display())))?,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => State::default(),
                Err(e) => return Err(StoreError::State(format!("{}: {e}", p.display()))),
            },
            None => State::default(),
        };
        for r in st.stores.values().filter(|r| !r.archived) {
            if let Ok(n) = StoreName::parse(&r.name) {
                exposure.register(&exposure_id(&n), &n.to_string());
            }
        }
        Ok(Self { cfg, admin, objects, exposure, audit, chain, invalidator: Mutex::new(None), st: Mutex::new(st) })
    }

    /// The raw data plane, for operator-level work that spans stores (ADR-126 S2 worktree sharing,
    /// promotion with conflict checks). Not a principal's path: it skips grants and exposure, so
    /// only an Admin verb on the daemon may use it, and must record what it did itself.
    pub fn operator_objects(&self) -> Arc<dyn ObjectBackend> {
        self.objects.clone()
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn now(&self) -> u64 {
        (self.cfg.clock)()
    }

    fn persist(&self, st: &State) -> Result<(), StoreError> {
        match &self.cfg.state_path {
            Some(p) => crate::fs::write_atomic(p, &serde_json::to_vec_pretty(st).map_err(|e| StoreError::State(e.to_string()))?),
            None => Ok(()),
        }
    }

    /// Where cache invalidations from bucket notifications go.
    pub fn set_invalidator(&self, f: Invalidator) {
        *self.invalidator.lock().unwrap_or_else(|e| e.into_inner()) = Some(f);
    }

    #[allow(clippy::too_many_arguments)]
    fn emit(&self, principal: &str, name: &StoreName, op: &str, key: Option<&str>, bytes: u64, outcome: Outcome, source: Source) {
        self.audit.record(&AuditEvent {
            ts_ms: self.now(),
            principal: principal.to_owned(),
            store: name.to_string(),
            bucket: name.bucket(),
            op: op.to_owned(),
            key: key.map(str::to_owned),
            bytes,
            outcome,
            source,
        });
    }

    /// An event about a principal rather than one store: one per store it holds a grant on.
    fn emit_principal(&self, p: &Principal, op: &str, outcome: Outcome) {
        let stores: Vec<String> = self.lock().grants.for_principal(p).into_iter().map(|g| g.store).collect();
        for s in stores.iter().filter_map(|s| StoreName::parse(s).ok()) {
            self.emit(&p.to_string(), &s, op, None, 0, outcome.clone(), Source::Governed);
        }
    }

    fn known(&self, name: &StoreName) -> Result<StoreRec, StoreError> {
        self.lock().stores.get(&name.to_string()).cloned().ok_or_else(|| StoreError::UnknownStore(name.to_string()))
    }

    // ---- lifecycle ----------------------------------------------------------------------------

    /// Create a store: its bucket (tagged), default and given grants, its exposure. Idempotent
    /// callers use [`Self::ensure_store`].
    pub fn create_store(&self, name: &StoreName, actor: &str, extra: Parties) -> Result<StoreInfo, StoreError> {
        if self.lock().stores.contains_key(&name.to_string()) {
            return Err(StoreError::Exists(name.to_string()));
        }
        let mut tags = name.tags();
        tags.push(("created".into(), self.now().to_string()));
        self.admin.create_bucket(&name.bucket(), &tags).map_err(|e| match e {
            BackendError::BucketExists(_) => StoreError::Exists(format!("{name} (bucket {} is already there)", name.bucket())),
            o => o.into(),
        })?;
        {
            let mut g = self.lock();
            g.stores.insert(name.to_string(), StoreRec { name: name.to_string(), created_ms: self.now(), archived: false });
            let d = Parties::defaults_for(name);
            let ts = self.now();
            for (list, acc) in [(d.admins.iter().chain(&extra.admins), Access::Admin), (d.writers.iter().chain(&extra.writers), Access::Write), (d.readers.iter().chain(&extra.readers), Access::Read)] {
                for p in list {
                    g.grants.grant(p.clone(), name, acc, if actor.is_empty() { "default" } else { actor }, ts);
                }
            }
            self.persist(&g)?;
        }
        self.exposure.register(&exposure_id(name), &name.to_string());
        self.chain.record("store.create", json!({"store": name.to_string(), "bucket": name.bucket(), "actor": actor}));
        self.emit(actor, name, "CreateBucket", None, 0, Outcome::Ok, Source::Governed);
        for p in self.principals_of(name) {
            self.apply_principal(&p)?;
        }
        self.info(name)
    }

    /// The store, created if it is not there yet.
    pub fn ensure_store(&self, name: &StoreName, actor: &str, parties: Parties) -> Result<StoreInfo, StoreError> {
        match self.create_store(name, actor, parties) {
            Err(StoreError::Exists(_)) if self.lock().stores.contains_key(&name.to_string()) => self.info(name),
            r => r,
        }
    }

    fn principals_of(&self, name: &StoreName) -> Vec<Principal> {
        self.lock().grants.for_store(name).into_iter().map(|g| g.principal).collect()
    }

    /// Push a principal's grants into the backend: its key (created on first need), the key's
    /// policy, and whether the key is enabled (not while the principal is suspended, and not when
    /// no enabled grant is left).
    fn apply_principal(&self, p: &Principal) -> Result<(), StoreError> {
        let (grants, suspended, key) = {
            let g = self.lock();
            (g.grants.for_principal(p), g.suspended.contains(p), g.keys.get(&p.to_string()).cloned())
        };
        let key = match key {
            Some(k) => k,
            None if grants.is_empty() => return Ok(()),
            None => {
                let k = self.admin.create_access_key(&p.to_string())?.id;
                let mut g = self.lock();
                g.keys.insert(p.to_string(), k.clone());
                self.persist(&g)?;
                k
            }
        };
        self.admin.put_policy(&key, &policy::for_grants(&grants))?;
        let live = !suspended && grants.iter().any(|g| g.enabled);
        self.admin.set_key_enabled(&key, live)?;
        Ok(())
    }

    /// Grant (or change) access. Recorded on the chain and in the activity log.
    pub fn grant(&self, name: &StoreName, principal: Principal, access: Access, actor: &str) -> Result<Grant, StoreError> {
        let rec = self.known(name)?;
        if rec.archived {
            return Err(StoreError::Refused(format!("{name} is archived")));
        }
        let g = {
            let mut st = self.lock();
            let g = st.grants.grant(principal.clone(), name, access, actor, self.now());
            self.persist(&st)?;
            g
        };
        self.chain.record("store.grant", json!({"store": name.to_string(), "principal": principal.to_string(), "access": access.as_str(), "actor": actor}));
        self.emit(actor, name, "Grant", Some(&principal.to_string()), 0, Outcome::Ok, Source::Governed);
        self.apply_principal(&principal)?;
        Ok(g)
    }

    /// Remove a grant. The principal's next request on the store is refused.
    pub fn revoke(&self, name: &StoreName, principal: &Principal, actor: &str) -> Result<(), StoreError> {
        self.known(name)?;
        {
            let mut st = self.lock();
            if !st.grants.revoke(principal, name) {
                return Err(StoreError::Refused(format!("{principal} holds no grant on {name}")));
            }
            self.persist(&st)?;
        }
        self.chain.record("store.revoke", json!({"store": name.to_string(), "principal": principal.to_string(), "actor": actor}));
        self.emit(actor, name, "Revoke", Some(&principal.to_string()), 0, Outcome::Ok, Source::Governed);
        self.apply_principal(principal)?;
        Ok(())
    }

    /// Disable or re-enable one grant without losing it.
    pub fn set_grant_enabled(&self, name: &StoreName, principal: &Principal, enabled: bool, actor: &str) -> Result<(), StoreError> {
        self.known(name)?;
        {
            let mut st = self.lock();
            if !st.grants.set_enabled(principal, name, enabled) {
                return Err(StoreError::Refused(format!("{principal} holds no grant on {name}")));
            }
            self.persist(&st)?;
        }
        let kind = if enabled { "store.grant.enable" } else { "store.grant.disable" };
        self.chain.record(kind, json!({"store": name.to_string(), "principal": principal.to_string(), "actor": actor}));
        self.emit(actor, name, if enabled { "GrantEnable" } else { "GrantDisable" }, Some(&principal.to_string()), 0, Outcome::Ok, Source::Governed);
        self.apply_principal(principal)?;
        Ok(())
    }

    /// A rule hit: disable the principal's key and every grant it holds. Proxied sessions are the
    /// caller's to drop; this refuses the next request.
    pub fn suspend_principal(&self, principal: &Principal, reason: &str) -> Result<(), StoreError> {
        {
            let mut st = self.lock();
            st.suspended.insert(principal.clone());
            self.persist(&st)?;
        }
        self.chain.record("store.principal.suspend", json!({"principal": principal.to_string(), "reason": reason}));
        self.emit_principal(principal, "Suspend", Outcome::Refused(reason.to_owned()));
        self.apply_principal(principal)?;
        Ok(())
    }

    /// Undo [`Self::suspend_principal`].
    pub fn resume_principal(&self, principal: &Principal) -> Result<(), StoreError> {
        {
            let mut st = self.lock();
            st.suspended.remove(principal);
            self.persist(&st)?;
        }
        self.chain.record("store.principal.resume", json!({"principal": principal.to_string()}));
        self.emit_principal(principal, "Resume", Outcome::Ok);
        self.apply_principal(principal)?;
        Ok(())
    }

    // ---- reading ------------------------------------------------------------------------------

    /// One store's description.
    pub fn info(&self, name: &StoreName) -> Result<StoreInfo, StoreError> {
        let rec = self.known(name)?;
        let (objects, bytes) = match self.objects.list(&name.bucket(), "") {
            Ok(l) => (l.len() as u64, l.iter().map(|m| m.size).sum()),
            Err(BackendError::NoBucket(_)) if rec.archived => (0, 0),
            Err(e) => return Err(e.into()),
        };
        let grants = self.lock().grants.for_store(name);
        Ok(StoreInfo { name: name.to_string(), bucket: name.bucket(), kind: name.kind().into(), objects, bytes, grants, archived: rec.archived, exposure: exposure_id(name) })
    }

    /// Every store this daemon holds.
    pub fn list_stores(&self) -> Vec<StoreInfo> {
        let names: Vec<String> = self.lock().stores.keys().cloned().collect();
        names.iter().filter_map(|n| StoreName::parse(n).ok()).filter_map(|n| self.info(&n).ok()).collect()
    }

    /// `Some(archived)` if this daemon holds the store, `None` if it does not know it.
    pub fn is_archived(&self, name: &StoreName) -> Option<bool> {
        self.lock().stores.get(&name.to_string()).map(|r| r.archived)
    }

    /// Whether the store is known to this daemon.
    pub fn has_store(&self, name: &StoreName) -> bool {
        self.lock().stores.contains_key(&name.to_string())
    }

    // ---- governed requests --------------------------------------------------------------------

    fn gate(&self, who: &Caller, name: &StoreName, op: Op, key: Option<&str>, bytes: u64) -> Result<(), StoreError> {
        let refuse = |why: String| {
            self.emit(&who.principal.to_string(), name, op.name(), key, bytes, Outcome::Refused(why.clone()), Source::Governed);
            Err(StoreError::Refused(why))
        };
        let rec = match self.known(name) {
            Ok(r) => r,
            Err(_) => return refuse(format!("unknown store {name}")),
        };
        if rec.archived {
            return refuse(format!("{name} is archived"));
        }
        // Control state is the daemon's alone (review F4): a worktree's `.wt/` (promotion record,
        // intent, bases, metadata) and a project's `worktrees/` registry. The S2 verbs write them
        // on the operator plane; a principal that could write them could forge a promotion.
        if matches!(op, Op::Put | Op::Copy | Op::Delete)
            && let Some(k) = key
            && ((matches!(name, StoreName::Worktree { .. }) && k.starts_with(".wt/"))
                || (matches!(name, StoreName::Project { .. }) && k.starts_with("worktrees/")))
        {
            return refuse(format!("{k} is reserved control state of {name}; only the daemon writes it"));
        }
        if who.remote
            && let Err(why) = self.exposure.check(&exposure_id(name))
        {
            return refuse(format!("exposure: {why}"));
        }
        let (held, suspended) = {
            let g = self.lock();
            (g.grants.effective(&who.principal, name), g.suspended.contains(&who.principal))
        };
        if suspended {
            return refuse(format!("{} is suspended", who.principal));
        }
        match held {
            Some(a) if a >= op.needs() => Ok(()),
            Some(a) => refuse(format!("{} holds {} on {name}; {} needs {}", who.principal, a.as_str(), op.name(), op.needs().as_str())),
            None => refuse(format!("{} holds no enabled grant on {name}", who.principal)),
        }
    }

    fn finish<T>(&self, who: &Caller, name: &StoreName, op: Op, key: Option<&str>, bytes: u64, r: Result<T, BackendError>) -> Result<T, StoreError> {
        let p = who.principal.to_string();
        // Listings are not logged when they succeed (owner, 2026-10-29); everything else is.
        match r {
            Ok(v) => {
                if op != Op::List {
                    self.emit(&p, name, op.name(), key, bytes, Outcome::Ok, Source::Governed);
                }
                Ok(v)
            }
            Err(e) => {
                self.emit(&p, name, op.name(), key, bytes, Outcome::Error(e.to_string()), Source::Governed);
                Err(e.into())
            }
        }
    }

    /// PutObject on behalf of `who`.
    pub fn put(&self, who: &Caller, name: &StoreName, key: &str, data: &[u8], if_none_match: bool) -> Result<ObjectMeta, StoreError> {
        self.gate(who, name, Op::Put, Some(key), data.len() as u64)?;
        let r = self.objects.put(&name.bucket(), key, data, if_none_match);
        self.finish(who, name, Op::Put, Some(key), data.len() as u64, r)
    }

    /// GetObject on behalf of `who`.
    pub fn get(&self, who: &Caller, name: &StoreName, key: &str) -> Result<Vec<u8>, StoreError> {
        self.gate(who, name, Op::Get, Some(key), 0)?;
        let r = self.objects.get(&name.bucket(), key);
        let n = r.as_ref().map_or(0, |d| d.len() as u64);
        self.finish(who, name, Op::Get, Some(key), n, r)
    }

    /// HeadObject on behalf of `who`.
    pub fn head(&self, who: &Caller, name: &StoreName, key: &str) -> Result<ObjectMeta, StoreError> {
        self.gate(who, name, Op::Head, Some(key), 0)?;
        let r = self.objects.head(&name.bucket(), key);
        self.finish(who, name, Op::Head, Some(key), 0, r)
    }

    /// DeleteObject on behalf of `who`.
    pub fn delete(&self, who: &Caller, name: &StoreName, key: &str) -> Result<(), StoreError> {
        self.gate(who, name, Op::Delete, Some(key), 0)?;
        let r = self.objects.delete(&name.bucket(), key);
        self.finish(who, name, Op::Delete, Some(key), 0, r)
    }

    /// List objects on behalf of `who`.
    pub fn list(&self, who: &Caller, name: &StoreName, prefix: &str) -> Result<Vec<ObjectMeta>, StoreError> {
        self.gate(who, name, Op::List, None, 0)?;
        let r = self.objects.list(&name.bucket(), prefix);
        self.finish(who, name, Op::List, None, 0, r)
    }

    /// List objects as the daemon itself (an operator reading metadata, `weaver store ls <name>`).
    /// Not a principal's request, so it needs no grant and, like any listing, is not logged.
    pub fn list_as_operator(&self, name: &StoreName, prefix: &str) -> Result<Vec<ObjectMeta>, StoreError> {
        self.known(name)?;
        Ok(self.objects.list(&name.bucket(), prefix)?)
    }

}

#[cfg(test)]
#[path = "service_tests.rs"]
mod tests;
