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

mod archive_gc;
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
///
/// Grant-affecting events (they carry a `seq`, see [`crate::grant_seq`]) are recorded while the
/// service holds its state lock, so that chain order is the order of the changes. An implementation
/// must therefore be quick and must not call back into the `StoreService`.
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
    /// Write-once (`append`): CI on a release store.
    pub appenders: Vec<Principal>,
    /// Delete-only (`sweep`): the retention sweeper of a release store.
    pub sweepers: Vec<Principal>,
}

/// The well-known principal CI runs as when it writes a release store.
pub const CI_PRINCIPAL: &str = "agent:ci";
/// The well-known principal of the retention sweeper.
pub const SWEEPER_PRINCIPAL: &str = "agent:store-sweeper";

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
        // A release store (ADR-126 section 3): CI writes once, the sweeper only deletes. Who may
        // read is the project's members, which the caller names.
        if matches!(name, StoreName::Release { .. }) {
            p.appenders.extend(Principal::parse(CI_PRINCIPAL));
            p.sweepers.extend(Principal::parse(SWEEPER_PRINCIPAL));
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
    /// Made with Object Lock (retention and legal hold are available).
    #[serde(default)]
    locked: bool,
    /// When the archive copy was started; a resumed run reuses it, so the archive keys are the
    /// same and a repeat overwrites rather than duplicates.
    #[serde(default)]
    archive_ts: Option<u64>,
    /// Every object has been copied to the archive. `false` while `archived` means a run was
    /// cut short and must be resumed (records from before this field read as done).
    #[serde(default = "yes")]
    archive_done: bool,
    /// Objects the finished archive holds.
    #[serde(default)]
    archive_count: u64,
    /// Per-store override of how long this store's archives are kept (beats the kind's limit).
    #[serde(default)]
    archive_keep: Option<archive_gc::KeepFor>,
    /// The `store.close` event is decided (every grant is off in the saved state) but not yet
    /// recorded: it is recorded once the principals' keys in the backend match. Holds the actor
    /// and the number of grants switched off.
    #[serde(default)]
    close_pending: Option<(String, usize)>,
}

fn yes() -> bool {
    true
}

/// One activity-log line that goes with a [`Pending`] event.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq)]
struct Act {
    who: String,
    store: String,
    op: String,
    key: Option<String>,
    refused: Option<String>,
}

/// A grant-affecting change that is decided and saved but whose chain event and activity lines are
/// not out yet. They go out once the principal's backend key and policy match the saved grants, so
/// a push that failed is never announced as done; a retry of the same change, or `reconcile` (at
/// start, and when the backend comes back), pushes and then records it, once.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq)]
struct Pending {
    principal: String,
    kind: String,
    /// The chain payload without its `seq`, which is stamped when the event is recorded.
    payload: Value,
    activity: Vec<Act>,
}

#[derive(Serialize, Deserialize, Default)]
struct State {
    #[serde(default)]
    pending: Vec<Pending>,
    grants: GrantTable,
    stores: BTreeMap<String, StoreRec>,
    /// principal -> access key id
    keys: BTreeMap<String, String>,
    suspended: BTreeSet<Principal>,
    /// Node stores of nodes that were unpaired: the id is not handed out again.
    #[serde(default)]
    retired: BTreeSet<String>,
    /// The `seq` of the last grant-affecting chain event (see `grant_seq`); 0 before the first.
    #[serde(default)]
    grant_seq: u64,
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
    /// Serialises backend pushes of a principal's grants.
    apply: Mutex<()>,
    policy_opts: Mutex<policy::PolicyOpts>,
    gc_sched: Mutex<archive_gc::Sched>,
    st: Mutex<State>,
}

pub use archive_gc::{ArchiveGcReport, ArchiveKeep, ArchiveSchedule, ArchiveSet, KeepFor};

/// The longest retention an `append` principal may stamp: 5 years (a Compliance hold cannot be
/// shortened by anyone, root included, so what CI can name is bounded; write and admin are not).
pub const APPEND_MAX_RETENTION_S: u64 = 5 * 365 * 86_400;

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
        if st.grant_seq > crate::grant_seq::CEILING {
            return Err(StoreError::State(format!("grant_seq {} is beyond the ceiling {}: the state file is damaged", st.grant_seq, crate::grant_seq::CEILING)));
        }
        for r in st.stores.values().filter(|r| !r.archived) {
            if let Ok(n) = StoreName::parse(&r.name) {
                exposure.register(&exposure_id(&n), &n.to_string());
            }
        }
        Ok(Self { cfg, admin, objects, exposure, audit, chain, invalidator: Mutex::new(None), apply: Mutex::new(()), policy_opts: Mutex::new(policy::PolicyOpts::default()), gc_sched: Mutex::new(archive_gc::Sched::default()), st: Mutex::new(st) })
    }

    /// Refuse before changing anything once the grant sequence is spent.
    fn seq_room(st: &State) -> Result<(), StoreError> {
        if st.grant_seq >= crate::grant_seq::CEILING {
            return Err(StoreError::Refused("the grant event sequence has reached its ceiling; no further grant changes".into()));
        }
        Ok(())
    }

    /// Stamp the next grant-feed `seq` on `payload`, then (once the caller has persisted the state)
    /// record it. Called with the state lock held, so chain order is change order.
    fn stamp(&self, st: &mut State) -> u64 {
        st.grant_seq += 1;
        st.grant_seq
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

    fn known(&self, name: &StoreName) -> Result<StoreRec, StoreError> {
        self.lock().stores.get(&name.to_string()).cloned().ok_or_else(|| StoreError::UnknownStore(name.to_string()))
    }

    // ---- lifecycle ----------------------------------------------------------------------------

    /// Create a store: its bucket (tagged), default and given grants, its exposure. Idempotent
    /// callers use [`Self::ensure_store`].
    pub fn create_store(&self, name: &StoreName, actor: &str, extra: Parties) -> Result<StoreInfo, StoreError> {
        self.create_store_with(name, actor, extra, false)
    }

    /// Create a store whose bucket has Object Lock enabled (versioned; retention and legal hold
    /// on puts, see [`Self::put_locked`]). Only the RustFS backend can; the local ones refuse. The
    /// choice is made once, at creation.
    pub fn create_locked_store(&self, name: &StoreName, actor: &str, extra: Parties) -> Result<StoreInfo, StoreError> {
        self.create_store_with(name, actor, extra, true)
    }

    fn create_store_with(&self, name: &StoreName, actor: &str, extra: Parties, locked: bool) -> Result<StoreInfo, StoreError> {
        if self.lock().retired.contains(&name.to_string()) {
            return Err(StoreError::Refused(format!("{name} belongs to a node that was unpaired and its store archived: a new node needs a new id")));
        }
        if self.lock().stores.contains_key(&name.to_string()) {
            return Err(StoreError::Exists(name.to_string()));
        }
        // The bucket is the identity of a store and it does not carry the mesh: a second name for
        // the same bucket (`weftos://local/...` and `weftos://<mesh id>/...`) is the same store,
        // never a new one. `ensure_store` moves a `local` store to its mesh name.
        if let Some(other) = self.same_bucket(name) {
            return Err(StoreError::Exists(format!("{name}: its bucket {} is already the bucket of {other}", name.bucket())));
        }
        let mut tags = name.tags();
        tags.push(("created".into(), self.now().to_string()));
        if locked {
            tags.push(("object-lock".into(), "on".into()));
        }
        let made = if locked { self.admin.create_locked_bucket(&name.bucket(), &tags) } else { self.admin.create_bucket(&name.bucket(), &tags) };
        match made {
            Ok(()) => {}
            // A bucket this daemon made but never recorded (a crash between the two steps, or a
            // state file lost): adopt it when its tags are this store's, refuse anything else.
            Err(BackendError::BucketExists(_)) if self.bucket_is_ours(name) => {}
            Err(BackendError::BucketExists(_)) => return Err(StoreError::Exists(format!("{name} (bucket {} is already there)", name.bucket()))),
            Err(o) => return Err(o.into()),
        }
        {
            let mut g = self.lock();
            // The check at the top and this insert are separate lock holds: a retire that landed
            // between them must still win. Undo the bucket if it was made just now.
            if g.retired.contains(&name.to_string()) {
                drop(g);
                let _ = self.admin.delete_bucket(&name.bucket());
                return Err(StoreError::Refused(format!("{name} belongs to a node that was unpaired and its store archived: a new node needs a new id")));
            }
            g.stores.insert(name.to_string(), StoreRec { name: name.to_string(), created_ms: self.now(), archived: false, locked, archive_ts: None, archive_done: true, archive_count: 0, archive_keep: None, close_pending: None });
            let d = Parties::defaults_for(name);
            let ts = self.now();
            for (list, acc) in [(d.admins.iter().chain(&extra.admins), Access::Admin), (d.writers.iter().chain(&extra.writers), Access::Write), (d.appenders.iter().chain(&extra.appenders), Access::Append), (d.sweepers.iter().chain(&extra.sweepers), Access::Sweep), (d.readers.iter().chain(&extra.readers), Access::Read)] {
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

    /// Whether the existing bucket of `name` carries exactly this store's identity tags.
    fn bucket_is_ours(&self, name: &StoreName) -> bool {
        // Not the `name` tag: it carries the mesh, which is not part of the bucket's identity.
        let want: Vec<(String, String)> = name.tags().into_iter().filter(|(k, _)| k != "name").collect();
        self.admin
            .buckets()
            .ok()
            .and_then(|bs| bs.into_iter().find(|(b, _)| *b == name.bucket()))
            .is_some_and(|(_, have)| want.iter().all(|t| have.contains(t)))
    }

    /// The store, created if it is not there yet. A store this daemon made under the `local` mesh
    /// name, asked for again under a mesh id, is that same store: it is renamed (see
    /// [`Self::migrate_store`]) rather than created a second time.
    pub fn ensure_store(&self, name: &StoreName, actor: &str, parties: Parties) -> Result<StoreInfo, StoreError> {
        if !self.has_store(name)
            && let Some(other) = self.same_bucket(name)
        {
            let from = StoreName::parse(&other)?;
            self.migrate_store(&from, name, actor)?;
        }
        match self.create_store(name, actor, parties) {
            // Already there: bring its principals' keys in line again (a create whose push failed
            // is finished by asking for the store a second time), then describe it.
            Err(StoreError::Exists(_)) if self.lock().stores.contains_key(&name.to_string()) => {
                for p in self.principals_of(name) {
                    self.apply_principal(&p)?;
                }
                self.info(name)
            }
            r => r,
        }
    }

    /// The name of a store (or retired node store) this service already holds under the same
    /// bucket as `name` but a different name.
    fn same_bucket(&self, name: &StoreName) -> Option<String> {
        let (me, b) = (name.to_string(), name.bucket());
        let g = self.lock();
        g.stores.keys().chain(g.retired.iter()).find(|n| **n != me && StoreName::parse(n).is_ok_and(|x| x.bucket() == b)).cloned()
    }

    /// The mesh id appeared: the store held as `from` (a `local` name) is now `to`, the same name
    /// with the mesh id. Same bucket, so nothing moves in the backend and every key's policy stays
    /// as it is; the store's record, its grants, its retired mark and its unannounced events take
    /// the new name, one stamped `store.migrate` event says so, and the exposure is registered
    /// under the new name. Idempotent. Refused unless `from` is on the `local` mesh, `to` is not,
    /// and both are the same bucket: a store never changes owner this way.
    pub fn migrate_store(&self, from: &StoreName, to: &StoreName, actor: &str) -> Result<bool, StoreError> {
        if from.bucket() != to.bucket() || from.kind() != to.kind() || from.mesh() != "local" || to.mesh() == "local" {
            return Err(StoreError::Refused(format!("{to} cannot take the place of {from}: only a `local` store moves to a mesh id, and it stays the same store")));
        }
        let _one = self.apply.lock().unwrap_or_else(|e| e.into_inner());
        {
            let mut g = self.lock();
            let (f, t) = (from.to_string(), to.to_string());
            if g.stores.contains_key(&t) && !g.stores.contains_key(&f) {
                return Ok(false);
            }
            if g.stores.contains_key(&t) {
                return Err(StoreError::Refused(format!("{from} and {to} are both held: one bucket cannot be two stores")));
            }
            if !g.stores.contains_key(&f) {
                if g.retired.remove(&f) {
                    g.retired.insert(t);
                    self.persist(&g)?;
                    return Ok(true);
                }
                return Err(StoreError::UnknownStore(f));
            }
            Self::seq_room(&g)?;
            let Some(mut rec) = g.stores.remove(&f) else { return Err(StoreError::UnknownStore(f)) };
            rec.name.clone_from(&t);
            g.stores.insert(t.clone(), rec);
            let moved = g.grants.rename_store(from, to);
            if g.retired.remove(&f) {
                g.retired.insert(t.clone());
            }
            for pd in &mut g.pending {
                if pd.payload["store"] == json!(f) {
                    pd.payload["store"] = json!(t);
                }
                for a in &mut pd.activity {
                    if a.store == f {
                        a.store.clone_from(&t);
                    }
                }
            }
            let seq = self.stamp(&mut g);
            self.persist(&g)?;
            self.chain.record("store.migrate", json!({"seq": seq, "from": f, "to": t, "bucket": to.bucket(), "grants": moved, "actor": actor}));
        }
        self.exposure.register(&exposure_id(to), &to.to_string());
        self.emit(actor, to, "Migrate", Some(&from.to_string()), 0, Outcome::Ok, Source::Governed);
        Ok(true)
    }

    /// Move every store held under the `local` mesh name to `mesh` (the daemon calls this when
    /// the mesh id is known). Returns the new names; a store that cannot move is reported, not
    /// skipped silently.
    pub fn migrate_mesh(&self, mesh: &str, actor: &str) -> Vec<Result<String, String>> {
        if mesh == "local" {
            return Vec::new();
        }
        let names: Vec<StoreName> = {
            let g = self.lock();
            g.stores.keys().chain(g.retired.iter()).filter_map(|n| StoreName::parse(n).ok()).filter(|n| n.mesh() == "local").collect()
        };
        let mut seen = BTreeSet::new();
        names
            .into_iter()
            .filter(|n| seen.insert(n.to_string()))
            .map(|from| {
                let to = StoreName::parse(&from.to_string().replacen("weftos://local/", &format!("weftos://{mesh}/"), 1)).map_err(|e| e.to_string())?;
                self.migrate_store(&from, &to, actor).map(|_| to.to_string()).map_err(|e| format!("{from}: {e}"))
            })
            .collect()
    }

    /// A node was unpaired: archive its node store (grants off, objects copied to the archive
    /// bucket, nothing deleted) and remember the id as retired, so a different node that comes up
    /// under the same id cannot be handed the old node's store, grants or keys. `create_store`
    /// and `ensure_store` refuse a retired node's store from now on, and this survives a restart.
    /// Call it from the unpair path; calling it again is harmless. A node store that was never
    /// created is simply marked. Returns the number of objects archived.
    pub fn retire_node_store(&self, name: &StoreName, actor: &str) -> Result<u64, StoreError> {
        if name.kind() != "node" {
            return Err(StoreError::Refused(format!("{name} is not a node store")));
        }
        // Archived only counts once every object is copied: a run cut short (RustFS went away
        // part-way) is resumed here, never reported as done. The caller may delete the node's
        // user on `Ok`, so `Ok` has to mean the whole archive exists.
        let archived = match self.known(name) {
            Ok(rec) if !rec.archived || !rec.archive_done => self.archive(name, false, actor)?,
            Ok(rec) => rec.archive_count,
            Err(_) => 0,
        };
        let mut g = self.lock();
        if !g.retired.contains(&name.to_string()) {
            Self::seq_room(&g)?;
            g.retired.insert(name.to_string());
            let seq = self.stamp(&mut g);
            self.persist(&g)?;
            self.chain.record("store.node.retired", json!({"seq": seq, "store": name.to_string(), "actor": actor, "archived_objects": archived}));
        }
        drop(g);
        Ok(archived)
    }

    /// The node stores whose ids are retired.
    pub fn retired_nodes(&self) -> Vec<String> {
        self.lock().retired.iter().cloned().collect()
    }

    fn principals_of(&self, name: &StoreName) -> Vec<Principal> {
        self.lock().grants.for_store(name).into_iter().map(|g| g.principal).collect()
    }

    /// Push a principal's grants into the backend: its key (created on first need), the key's
    /// policy, and whether the key is enabled (not while the principal is suspended, and not when
    /// no enabled grant is left). Once the backend matches, the principal's pending events go out.
    fn apply_principal(&self, p: &Principal) -> Result<(), StoreError> {
        // One push at a time, and the grants are read inside it: an older snapshot can never
        // overwrite a newer policy (and so bring a revoked grant back in the backend).
        let _one = self.apply.lock().unwrap_or_else(|e| e.into_inner());
        self.apply_locked(p)
    }

    /// [`Self::apply_principal`] with the `apply` lock already held by the caller.
    fn apply_locked(&self, p: &Principal) -> Result<(), StoreError> {
        let (grants, suspended, key) = {
            let g = self.lock();
            (g.grants.for_principal(p), g.suspended.contains(p), g.keys.get(&p.to_string()).cloned())
        };
        let key = match key {
            Some(k) => k,
            None if grants.is_empty() => return self.flush_pending(p),
            None => {
                let k = self.admin.create_access_key(&p.to_string())?.id;
                let mut g = self.lock();
                g.keys.insert(p.to_string(), k.clone());
                self.persist(&g)?;
                k
            }
        };
        self.admin.put_policy(&key, &policy::for_grants_with(&grants, &self.policy_opts()))?;
        let live = !suspended && grants.iter().any(|g| g.enabled);
        self.admin.set_key_enabled(&key, live)?;
        self.flush_pending(p)
    }

    /// Record the chain events and activity lines of `p`'s saved changes, now that its backend
    /// key matches them. Called with the `apply` lock held. Each is recorded once: they leave the
    /// pending list in the same step that stamps them.
    fn flush_pending(&self, p: &Principal) -> Result<(), StoreError> {
        let mut st = self.lock();
        let me = p.to_string();
        if !st.pending.iter().any(|x| x.principal == me) {
            return Ok(());
        }
        let (mine, rest): (Vec<Pending>, Vec<Pending>) = std::mem::take(&mut st.pending).into_iter().partition(|x| x.principal == me);
        st.pending = rest;
        let mut acts = Vec::new();
        for pd in mine {
            if Self::seq_room(&st).is_err() {
                st.pending.push(pd);
                continue;
            }
            let seq = self.stamp(&mut st);
            let mut payload = pd.payload.clone();
            payload["seq"] = json!(seq);
            self.chain.record(&pd.kind, payload);
            acts.extend(pd.activity);
        }
        self.persist(&st)?;
        drop(st);
        for a in acts {
            if let Ok(n) = StoreName::parse(&a.store) {
                let outcome = a.refused.map_or(Outcome::Ok, Outcome::Refused);
                self.emit(&a.who, &n, &a.op, a.key.as_deref(), 0, outcome, Source::Governed);
            }
        }
        Ok(())
    }

    /// Record `store.close` once every principal of the store has its backend state. Called with
    /// the `apply` lock held.
    fn flush_close(&self, name: &StoreName) -> Result<(), StoreError> {
        let mut st = self.lock();
        let Some((actor, n)) = st.stores.get_mut(&name.to_string()).and_then(|r| r.close_pending.take()) else { return Ok(()) };
        if Self::seq_room(&st).is_err() {
            if let Some(r) = st.stores.get_mut(&name.to_string()) {
                r.close_pending = Some((actor, n));
            }
            return Ok(());
        }
        let seq = self.stamp(&mut st);
        self.persist(&st)?;
        self.chain.record("store.close", json!({"seq": seq, "store": name.to_string(), "actor": actor, "grants_disabled": n}));
        Ok(())
    }

    /// One grant-affecting change to `p`: `f` changes the saved state (and names the event that
    /// goes with it), the state is saved, the backend is brought in line, and only then do the
    /// chain event and activity lines go out. All of it under the `apply` lock, so two changes to
    /// the same principal cannot interleave their backend pushes. If the push fails the change
    /// stays saved (the service enforces it at once), the event stays pending, and the error is
    /// returned; doing the same change again, or `reconcile`, finishes it.
    fn change<R>(&self, p: &Principal, f: impl FnOnce(&mut State) -> Result<(R, Option<Pending>), StoreError>) -> Result<R, StoreError> {
        let _one = self.apply.lock().unwrap_or_else(|e| e.into_inner());
        let r = {
            let mut st = self.lock();
            Self::seq_room(&st)?;
            let (r, pend) = f(&mut st)?;
            if let Some(pd) = pend
                && !st.pending.contains(&pd)
            {
                st.pending.push(pd);
            }
            self.persist(&st)?;
            r
        };
        self.apply_locked(p)?;
        Ok(r)
    }

    /// The owner's switches for the direct keys of the `append` and `sweep` roles.
    pub fn policy_opts(&self) -> policy::PolicyOpts {
        *self.policy_opts.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Set the owner's switches; takes effect for keys as their grants are pushed, so call
    /// [`Self::reconcile`] after changing it to bring existing keys in line.
    pub fn set_policy_opts(&self, opts: policy::PolicyOpts) {
        *self.policy_opts.lock().unwrap_or_else(|e| e.into_inner()) = opts;
    }

    /// Push every principal's grants into the backend again, and record the events of changes
    /// whose push had failed. A grant change is saved before it is pushed, so a push that failed
    /// (RustFS down, a crash between the two) leaves the backend behind the saved state; call this
    /// at start and after the backend comes back. Covers every principal that has a key, a grant,
    /// a suspension or an unannounced change, and every archived store whose close is unannounced.
    /// Returns the principals whose push failed, with the reason.
    pub fn reconcile(&self) -> Vec<(Principal, String)> {
        let (mut ps, stores, closing): (BTreeSet<Principal>, Vec<StoreName>, Vec<StoreName>) = {
            let g = self.lock();
            let mut ps: BTreeSet<Principal> = g.keys.keys().filter_map(|k| Principal::parse(k).ok()).collect();
            ps.extend(g.suspended.iter().cloned());
            ps.extend(g.pending.iter().filter_map(|x| Principal::parse(&x.principal).ok()));
            let stores = g.stores.values().filter_map(|r| StoreName::parse(&r.name).ok()).collect();
            let closing = g.stores.values().filter(|r| r.close_pending.is_some()).filter_map(|r| StoreName::parse(&r.name).ok()).collect();
            (ps, stores, closing)
        };
        for s in &stores {
            ps.extend(self.principals_of(s));
        }
        let failed: Vec<(Principal, String)> = ps.into_iter().filter_map(|p| self.apply_principal(&p).err().map(|e| (p, e.to_string()))).collect();
        if failed.is_empty() {
            let _one = self.apply.lock().unwrap_or_else(|e| e.into_inner());
            for s in &closing {
                let _ = self.flush_close(s);
            }
        }
        failed
    }

    /// Grant (or change) access. Recorded on the chain and in the activity log once the backend
    /// holds it.
    pub fn grant(&self, name: &StoreName, principal: Principal, access: Access, actor: &str) -> Result<Grant, StoreError> {
        let rec = self.known(name)?;
        if rec.archived {
            return Err(StoreError::Refused(format!("{name} is archived")));
        }
        self.change(&principal, |st| {
            let g = st.grants.grant(principal.clone(), name, access, actor, self.now());
            let pd = Pending {
                principal: principal.to_string(),
                kind: "store.grant".into(),
                payload: json!({"store": name.to_string(), "principal": principal.to_string(), "access": access.as_str(), "actor": actor}),
                activity: vec![Act { who: actor.to_owned(), store: name.to_string(), op: "Grant".into(), key: Some(principal.to_string()), refused: None }],
            };
            Ok((g, Some(pd)))
        })
    }

    /// Remove a grant. The principal's next request on the store is refused. If the backend push
    /// failed, the grant is already gone here and the error says so; calling `revoke` again
    /// finishes the push and records the event (it does not answer "holds no grant").
    pub fn revoke(&self, name: &StoreName, principal: &Principal, actor: &str) -> Result<(), StoreError> {
        self.known(name)?;
        self.change(principal, |st| {
            let pd = Pending {
                principal: principal.to_string(),
                kind: "store.revoke".into(),
                payload: json!({"store": name.to_string(), "principal": principal.to_string(), "actor": actor}),
                activity: vec![Act { who: actor.to_owned(), store: name.to_string(), op: "Revoke".into(), key: Some(principal.to_string()), refused: None }],
            };
            if !st.grants.revoke(principal, name) && !st.pending.contains(&pd) {
                return Err(StoreError::Refused(format!("{principal} holds no grant on {name}")));
            }
            Ok(((), Some(pd)))
        })
    }

    /// Disable or re-enable one grant without losing it.
    pub fn set_grant_enabled(&self, name: &StoreName, principal: &Principal, enabled: bool, actor: &str) -> Result<(), StoreError> {
        if enabled && self.known(name)?.archived {
            return Err(StoreError::Refused(format!("{name} is archived")));
        }
        self.known(name)?;
        self.change(principal, |st| {
            if !st.grants.set_enabled(principal, name, enabled) {
                return Err(StoreError::Refused(format!("{principal} holds no grant on {name}")));
            }
            let (kind, op) = if enabled { ("store.grant.enable", "GrantEnable") } else { ("store.grant.disable", "GrantDisable") };
            let pd = Pending {
                principal: principal.to_string(),
                kind: kind.into(),
                payload: json!({"store": name.to_string(), "principal": principal.to_string(), "actor": actor}),
                activity: vec![Act { who: actor.to_owned(), store: name.to_string(), op: op.into(), key: Some(principal.to_string()), refused: None }],
            };
            Ok(((), Some(pd)))
        })
    }

    /// A rule hit: disable the principal's key and every grant it holds. Proxied sessions are the
    /// caller's to drop; this refuses the next request.
    pub fn suspend_principal(&self, principal: &Principal, reason: &str) -> Result<(), StoreError> {
        self.change(principal, |st| {
            st.suspended.insert(principal.clone());
            let activity = st.grants.for_principal(principal).into_iter().map(|g| Act { who: principal.to_string(), store: g.store, op: "Suspend".into(), key: None, refused: Some(reason.to_owned()) }).collect();
            let pd = Pending { principal: principal.to_string(), kind: "store.principal.suspend".into(), payload: json!({"principal": principal.to_string(), "reason": reason}), activity };
            Ok(((), Some(pd)))
        })
    }

    /// Undo [`Self::suspend_principal`].
    pub fn resume_principal(&self, principal: &Principal) -> Result<(), StoreError> {
        self.change(principal, |st| {
            st.suspended.remove(principal);
            let activity = st.grants.for_principal(principal).into_iter().map(|g| Act { who: principal.to_string(), store: g.store, op: "Resume".into(), key: None, refused: None }).collect();
            let pd = Pending { principal: principal.to_string(), kind: "store.principal.resume".into(), payload: json!({"principal": principal.to_string()}), activity };
            Ok(((), Some(pd)))
        })
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

    /// Whether the store is known to this daemon.
    /// Record on the chain that an operator made a request *as* `principal` (`weaver store put|get
    /// --as`). The activity log shows the request under that principal; this is the line that says who really asked.
    pub fn note_operator(&self, actor: &str, verb: &str, principal: &Principal, name: &StoreName, key: &str) {
        self.chain.record("store.as", json!({"actor": actor, "verb": verb, "principal": principal.to_string(), "store": name.to_string(), "key": key}));
    }

    /// The names of the stores this daemon knows, from its own state: no backend call.
    pub fn store_names(&self) -> Vec<String> {
        self.lock().stores.values().filter(|r| !r.archived).map(|r| r.name.clone()).collect()
    }

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
            Some(a) if op.allowed_by(a) => Ok(()),
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

    /// An `append` grant (CI on a release store) writes once: a put that could replace an
    /// object is refused. Anyone with more than `append` is not held to it.
    fn write_once(&self, who: &Caller, name: &StoreName, key: &str, bytes: u64, conditional_create: bool) -> Result<(), StoreError> {
        let held = self.lock().grants.effective(&who.principal, name);
        if held == Some(Access::Append) && !conditional_create {
            let why = format!("{} holds append on {name}: a put must carry if_none_match, nothing is overwritten", who.principal);
            self.emit(&who.principal.to_string(), name, Op::Put.name(), Some(key), bytes, Outcome::Refused(why.clone()), Source::Governed);
            return Err(StoreError::Refused(why));
        }
        Ok(())
    }

    /// PutObject on behalf of `who`.
    pub fn put(&self, who: &Caller, name: &StoreName, key: &str, data: &[u8], if_none_match: bool) -> Result<ObjectMeta, StoreError> {
        self.gate(who, name, Op::Put, Some(key), data.len() as u64)?;
        self.write_once(who, name, key, data.len() as u64, if_none_match)?;
        let r = self.objects.put(&name.bucket(), key, data, if_none_match);
        self.finish(who, name, Op::Put, Some(key), data.len() as u64, r)
    }

    /// Whether the store was made with Object Lock.
    pub fn is_locked(&self, name: &StoreName) -> bool {
        self.lock().stores.get(&name.to_string()).is_some_and(|r| r.locked)
    }

    /// Provision a store's lifecycle rules (replacing any; an empty list removes them), so the
    /// backend expires old objects, old versions and abandoned multipart uploads by itself.
    /// Recorded on the chain. Object Lock still wins: nothing under retention or a legal hold is
    /// removed. The local backends answer `Unsupported`.
    pub fn set_lifecycle(&self, name: &StoreName, rules: &[crate::backend::LifecycleRule], actor: &str) -> Result<(), StoreError> {
        self.known(name)?;
        self.admin.put_lifecycle(&name.bucket(), rules)?;
        let ids: Vec<&str> = rules.iter().map(|r| r.id.as_str()).collect();
        self.chain.record("store.lifecycle", json!({"actor": actor, "store": name.to_string(), "rules": ids}));
        Ok(())
    }

    /// The lifecycle rules the backend holds for a store.
    pub fn lifecycle(&self, name: &StoreName) -> Result<Vec<crate::backend::LifecycleRule>, StoreError> {
        self.known(name)?;
        Ok(self.admin.get_lifecycle(&name.bucket())?)
    }

    /// Read back the Object Lock configuration the backend holds for a store's bucket (does it
    /// have lock on, and what default retention), as the daemon itself: an operator's check that
    /// the store is what the daemon recorded. The local backends answer `Unsupported`.
    pub fn lock_config(&self, name: &StoreName) -> Result<crate::backend::LockConfig, StoreError> {
        self.known(name)?;
        Ok(self.objects.get_lock_config(&name.bucket())?)
    }

    /// PutObject with Object Lock retention and/or a legal hold, in a store made by
    /// [`Self::create_locked_store`]. Gated and logged like `put`; an `append` principal (CI) must
    /// set `opts.if_none_match`. Returns the version written. A locked version cannot be deleted
    /// by anyone (compliance) or without a bypass (governance) until its date.
    pub fn put_locked(&self, who: &Caller, name: &StoreName, key: &str, data: &[u8], opts: &crate::backend::PutOpts) -> Result<crate::backend::LockedPut, StoreError> {
        self.gate(who, name, Op::Put, Some(key), data.len() as u64)?;
        if !self.is_locked(name) {
            return Err(StoreError::Refused(format!("{name} was not made with object lock")));
        }
        self.write_once(who, name, key, data.len() as u64, opts.if_none_match)?;
        let r = self.objects.put_locked(&name.bucket(), key, data, opts);
        self.finish(who, name, Op::Put, Some(key), data.len() as u64, r)
    }

    /// PutObject from a reader, in parts, so an object larger than memory can be stored (the
    /// 512 MiB limit of a whole-body put does not apply). Lock options in `opts` need a store made
    /// by [`Self::create_locked_store`]. Gated and logged like `put`, with the size known only at the
    /// end; an `append` principal must set `opts.if_none_match`. On any failure nothing is visible.
    pub fn put_stream(&self, who: &Caller, name: &StoreName, key: &str, src: &mut dyn std::io::Read, opts: &crate::backend::PutOpts) -> Result<crate::backend::LockedPut, StoreError> {
        self.gate(who, name, Op::Put, Some(key), 0)?;
        if (opts.retention.is_some() || opts.legal_hold) && !self.is_locked(name) {
            return Err(StoreError::Refused(format!("{name} was not made with object lock")));
        }
        self.write_once(who, name, key, 0, opts.if_none_match)?;
        let r = self.objects.put_stream(&name.bucket(), key, src, crate::backend::DEFAULT_PART, opts);
        let bytes = r.as_ref().map_or(0, |p| p.meta.size);
        self.finish(who, name, Op::Put, Some(key), bytes, r)
    }

    /// Stamp or lengthen the retention of one version after the upload is committed (CI puts the
    /// artifact, then holds it). Only ever lengthens: a date not later than the current one is
    /// refused, so the `append` role (CI) may call it, since it can only add protection. Gated and
    /// logged as a put. Returns the retention now in force.
    pub fn extend_retention(&self, who: &Caller, name: &StoreName, key: &str, version_id: &str, until_unix_s: u64, mode: crate::backend::LockMode) -> Result<crate::backend::Retention, StoreError> {
        self.gate(who, name, Op::Put, Some(key), 0)?;
        // An append principal (CI) cannot name how long: a Compliance hold cannot be undone by
        // anyone, so its reach is bounded. Write and admin are not.
        let horizon = self.now() / 1000 + APPEND_MAX_RETENTION_S;
        if self.lock().grants.effective(&who.principal, name) == Some(Access::Append) && until_unix_s > horizon {
            let why = format!("{} holds append on {name}: retention may run at most {} days from now", who.principal, APPEND_MAX_RETENTION_S / 86_400);
            self.emit(&who.principal.to_string(), name, Op::Put.name(), Some(key), 0, Outcome::Refused(why.clone()), Source::Governed);
            return Err(StoreError::Refused(why));
        }
        if !self.is_locked(name) {
            return Err(StoreError::Refused(format!("{name} was not made with object lock")));
        }
        let r = self.objects.extend_retention(&name.bucket(), key, version_id, until_unix_s, mode);
        self.finish(who, name, Op::Put, Some(key), 0, r)
    }

    /// Conditional PutObject: write only if the key still has the etag the caller read (S3
    /// `If-Match`). A changed key is `BackendError::Stale`; read again and retry. Unlike `put`,
    /// a concurrent writer can never be overwritten blind.
    pub fn put_if_match(&self, who: &Caller, name: &StoreName, key: &str, data: &[u8], etag: &str) -> Result<ObjectMeta, StoreError> {
        self.gate(who, name, Op::Put, Some(key), data.len() as u64)?;
        self.write_once(who, name, key, data.len() as u64, false)?;
        let r = self.objects.put_if_match(&name.bucket(), key, data, etag);
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

    /// Every stored version and delete marker under `prefix` (a locked store keeps them all), on
    /// behalf of `who`. Needs read access, like a listing. A key with two or more non-marker
    /// versions was replaced after its first put; compare `etag` or `version_id` against what you
    /// recorded to see which. Backends without versioning answer `Unsupported`.
    pub fn list_versions(&self, who: &Caller, name: &StoreName, prefix: &str) -> Result<Vec<crate::backend::ObjectVersion>, StoreError> {
        self.gate(who, name, Op::List, None, 0)?;
        let r = self.objects.list_versions(&name.bucket(), prefix);
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
