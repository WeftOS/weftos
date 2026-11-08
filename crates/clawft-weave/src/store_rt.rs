//! The daemon's object store (ADR-126 S1): `weftos-store` wired to this node's exposure
//! registry (ADR-120), activity log and chain.
//!
//! - Stores live under `<runtime dir>/store/` (a local-directory backend) unless
//!   `WEFTOS_STORE_RUSTFS_ENV` names an env file (`MESH_S3_ENDPOINT`, `MESH_S3_ACCESS_KEY`,
//!   `MESH_S3_SECRET_KEY`, optional `MESH_S3_REGION`), which selects the RustFS backend; per-key
//!   secrets are then written 0600 under `<runtime dir>/store-secrets/`. The daemon is the only
//!   door either way.
//! - Each store is an exposure `store.<bucket>`, audience `local` until an operator widens it.
//! - Every governed request, grant and refusal is a record in the exposure activity log
//!   (`weaver expose log --id store.<bucket>`); grants and lifecycle changes also go on the chain.
//! - At start the daemon creates its own node store.

use std::path::Path;
use std::sync::{Arc, OnceLock};

use clawft_kernel::exposure::registry::now_ms;
use clawft_kernel::exposure::{AuditFn, ExposureKind, ExposureRegistry, ExposureSpec, NewActivity};
use serde_json::Value;
use weftos_store::audit::{AuditEvent, AuditSink};
use weftos_store::service::{Config, ChainSink, ExposureGate};
use weftos_store::{AdminBackend, FsBackend, ObjectBackend, RustfsBackend, RustfsConfig, StoreName, StoreService};

static GLOBAL: OnceLock<Arc<StoreService>> = OnceLock::new();

/// The daemon's store service, once started.
pub fn global() -> Option<Arc<StoreService>> {
    GLOBAL.get().cloned()
}

static PROTECTED: OnceLock<Vec<std::path::PathBuf>> = OnceLock::new();

/// Paths the `store.worktree.*` verbs must never read from, write into or contain: the daemon's
/// runtime directory (it holds `store-secrets/`) and the RustFS environment file. Empty until
/// [`start`] has run.
pub fn protected_paths() -> Vec<std::path::PathBuf> {
    PROTECTED.get().cloned().unwrap_or_default()
}

/// ADR-120 exposure for each store.
pub struct RegistryGate(pub Arc<ExposureRegistry>);

impl ExposureGate for RegistryGate {
    fn register(&self, id: &str, resource: &str) {
        self.0.register(ExposureSpec::new(id, ExposureKind::Other, resource, "s3"));
    }
    fn check(&self, id: &str) -> Result<(), String> {
        self.0.check_serve(id).map_err(|r| r.reason().to_owned())
    }
}

/// Store events into the exposure activity log: exposure `store.<bucket>`, caller = principal,
/// user label = `<op> <key>`, bytes in for writes and out for reads.
pub struct ActivitySink(pub Arc<ExposureRegistry>);

impl AuditSink for ActivitySink {
    fn record(&self, e: &AuditEvent) {
        let label = match &e.key {
            Some(k) => format!("{} {k}", e.op),
            None => e.op.clone(),
        };
        let read = matches!(e.op.as_str(), "GetObject" | "HeadObject");
        self.0.record(NewActivity {
            exposure: format!("store.{}", e.bucket),
            caller_node: e.principal.clone(),
            caller_user: Some(label),
            bytes_in: if read { 0 } else { e.bytes },
            bytes_out: if read { e.bytes } else { 0 },
            duration_ms: 0,
            outcome: e.outcome.text(),
        });
    }
}

/// Grants and lifecycle on the chain, through the same audit function the exposure registry uses.
pub struct ChainAudit(pub Option<AuditFn>);

impl ChainSink for ChainAudit {
    fn record(&self, kind: &str, payload: Value) {
        if let Some(f) = &self.0 {
            f(kind, payload);
        }
    }
}

/// A node id as a topic source id: lowercase, anything else `-`, at most 61 characters. Lossy on
/// purpose (`pg.1` and `pg-1` give the same one): it names a topic, never storage. A node's
/// *store* uses [`node_token`], which refuses an id that would collide.
pub fn topic_token(node_id: &str) -> String {
    let t: String = node_id.to_lowercase().chars().map(|c| if c.is_ascii_lowercase() || c.is_ascii_digit() { c } else { '-' }).take(61).collect();
    if t.is_empty() { "node".into() } else { t }
}

/// A node id as the token of its store (`weftos://<mesh>/nodes/<token>/store`, bucket
/// `n-<token>`). The bucket is the store's identity, so the token must be the id itself, never a
/// cleaned-up one: `pg.1` and `pg-1` (or `PG-1` and `pg-1`) would be one bucket. An id that is not
/// already 1 to 61 of lowercase letters, digits and `-` is refused, with the rule in the error.
pub fn node_token(node_id: &str) -> Result<String, String> {
    if !node_id.is_empty() && node_id.len() <= 61 && node_id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') {
        return Ok(node_id.to_owned());
    }
    Err(format!("node id {node_id:?} cannot name a node store without colliding with another id's: it must be 1 to 61 of lowercase letters, digits and '-' as it is (a cleaned-up form would put {:?} and other ids in one bucket)", topic_token(node_id)))
}

/// The mesh part of store names: the licence store's mesh id (ADR-114), or `local` before one exists.
#[cfg(all(feature = "placement", unix))]
pub fn mesh_token() -> String {
    crate::mesh_names::MeshNames::local().mesh.map_or_else(|| "local".into(), hex::encode)
}

/// Without placement there is no licence store to ask: `local`.
#[cfg(not(all(feature = "placement", unix)))]
pub fn mesh_token() -> String {
    "local".into()
}

/// Build the service under `dir` and create this node's store. Idempotent: the first call wins.
pub fn start(node_id: &str, dir: &Path, exposures: Arc<ExposureRegistry>, chain: Option<AuditFn>) -> Result<Arc<StoreService>, String> {
    if let Some(g) = global() {
        return Ok(g);
    }
    let mut protect = vec![dir.to_path_buf()];
    protect.extend(std::env::var_os("WEFTOS_STORE_RUSTFS_ENV").filter(|v| !v.is_empty()).map(std::path::PathBuf::from));
    let _ = PROTECTED.set(protect);
    // The RustFS backend uses blocking HTTP, which must not run on (or be built inside) an async
    // task; a plain thread keeps both backends safe to start from the daemon's runtime.
    let svc = std::thread::scope(|s| {
        s.spawn(|| open_service(node_id, dir, exposures, chain, std::env::var_os("WEFTOS_STORE_RUSTFS_ENV").filter(|v| !v.is_empty()).map(Into::into))).join().map_err(|_| "object store start panicked".to_owned())?
    })?;
    Ok(GLOBAL.get_or_init(|| svc).clone())
}

/// Create (or find) the project store `…/projects/<ulid>/store` on `svc`. Returns its name.
pub fn ensure_project_store(svc: &StoreService, ulid: &str) -> Result<String, String> {
    let name = StoreName::project(&mesh_token(), ulid).map_err(|e| e.to_string())?;
    svc.ensure_store(&name, "project-add", weftos_store::service::Parties::defaults_for(&name)).map_err(|e| e.to_string())?;
    Ok(name.to_string())
}

/// The `project add` hook: make the project's store if the daemon runs an object store.
/// `Ok(None)` when it does not. Runs on a plain thread because the RustFS backend is blocking.
pub fn project_added(ulid: &str) -> Result<Option<String>, String> {
    let Some(svc) = global() else { return Ok(None) };
    std::thread::scope(|s| s.spawn(|| ensure_project_store(&svc, ulid)).join().map_err(|_| "project store creation panicked".to_owned())?).map(Some)
}

type Backends = (Arc<dyn AdminBackend>, Arc<dyn ObjectBackend>);

fn backends(dir: &Path, rustfs_env: Option<&Path>) -> Result<Backends, String> {
    if let Some(env_file) = rustfs_env {
        let cfg = RustfsConfig::from_env_file(env_file, dir.join("store-secrets")).map_err(|e| e.to_string())?;
        let be = Arc::new(RustfsBackend::new(cfg).map_err(|e| e.to_string())?);
        return Ok((be.clone(), be));
    }
    let be = Arc::new(FsBackend::open(&dir.join("store")).map_err(|e| e.to_string())?);
    Ok((be.clone(), be))
}

/// The owner's decisions on the release roles' direct keys: `WEFTOS_STORE_SWEEPER_PUT=1` lets the
/// sweeper key put, `WEFTOS_STORE_CI_DIRECT_PUT=1` lets the CI key put straight to RustFS. Both off
/// unless set to `1` or `true`. See ADR-126, "Owner decisions on the release roles".
pub(crate) fn policy_opts_from_env() -> weftos_store::policy::PolicyOpts {
    let on = |k: &str| std::env::var(k).is_ok_and(|v| matches!(v.trim(), "1" | "true" | "on"));
    weftos_store::policy::PolicyOpts { sweeper_put: on("WEFTOS_STORE_SWEEPER_PUT"), ci_direct_put: on("WEFTOS_STORE_CI_DIRECT_PUT") }
}

/// The scheduled archive sweep (ADR-126 open question 2), from env. Off unless
/// `WEFTOS_STORE_ARCHIVE_GC_EVERY_HOURS` is set. No day numbers are built in: name them with
/// `WEFTOS_STORE_ARCHIVE_{NODE,PROJECT,RELEASE}_DAYS` (a kind left out keeps for ever; a per-store
/// override from `weaver store archive-keep` also counts). Without `WEFTOS_STORE_ARCHIVE_GC_CONFIRM`
/// = `1`/`true`/`on` a scheduled run only reports (to the log).
pub(crate) fn archive_schedule_from(get: impl Fn(&str) -> Option<String>) -> Result<Option<weftos_store::service::ArchiveSchedule>, String> {
    let num = |k: &str| -> Result<Option<u64>, String> {
        match get(k).map(|v| v.trim().to_owned()).filter(|v| !v.is_empty()) {
            None => Ok(None),
            Some(v) => v.parse::<u64>().map(Some).map_err(|_| format!("{k}={v:?} is not a whole number")),
        }
    };
    let Some(every_hours) = num("WEFTOS_STORE_ARCHIVE_GC_EVERY_HOURS")? else { return Ok(None) };
    let confirmed = get("WEFTOS_STORE_ARCHIVE_GC_CONFIRM").is_some_and(|v| matches!(v.trim(), "1" | "true" | "on"));
    let keep = weftos_store::service::ArchiveKeep {
        node_days: num("WEFTOS_STORE_ARCHIVE_NODE_DAYS")?,
        project_days: num("WEFTOS_STORE_ARCHIVE_PROJECT_DAYS")?,
        release_days: num("WEFTOS_STORE_ARCHIVE_RELEASE_DAYS")?,
    };
    let s = weftos_store::service::ArchiveSchedule { every_hours, keep, confirmed };
    s.validate().map_err(|e| e.to_string())?;
    Ok(Some(s))
}

/// Run the due sweep once a minute for as long as the service lives. Only started when a schedule is set.
fn spawn_archive_gc(svc: &Arc<StoreService>) {
    let weak = Arc::downgrade(svc);
    let _ = std::thread::Builder::new().name("store-archive-gc".into()).spawn(move || {
        loop {
            std::thread::sleep(std::time::Duration::from_secs(60));
            let Some(svc) = weak.upgrade() else { return };
            match svc.archive_gc_tick() {
                Ok(Some(r)) => tracing::info!(dry_run = r.dry_run, expired = r.expired.len(), deleted_objects = r.deleted_objects, in_progress = r.in_progress.len(), "scheduled archive sweep"),
                Ok(None) => {}
                Err(e) => tracing::warn!(error = %e, "scheduled archive sweep failed; retried next interval"),
            }
        }
    });
}

pub(crate) fn open_service(
    node_id: &str,
    dir: &Path,
    exposures: Arc<ExposureRegistry>,
    chain: Option<AuditFn>,
    rustfs_env: Option<std::path::PathBuf>,
) -> Result<Arc<StoreService>, String> {
    let (admin, objects) = backends(dir, rustfs_env.as_deref())?;
    let svc = StoreService::open(
        Config { node_id: node_id.to_owned(), state_path: Some(dir.join("store-state.json")), clock: now_ms },
        admin,
        objects,
        Arc::new(RegistryGate(exposures.clone())),
        Arc::new(ActivitySink(exposures)),
        Arc::new(ChainAudit(chain)),
    )
    .map_err(|e| e.to_string())?;
    let svc = Arc::new(svc);
    svc.set_policy_opts(policy_opts_from_env());
    match archive_schedule_from(|k| std::env::var(k).ok()) {
        Ok(Some(s)) => match svc.set_archive_schedule(Some(s)) {
            Ok(()) => {
                tracing::info!(every_hours = s.every_hours, confirmed = s.confirmed, "scheduled archive sweep on");
                spawn_archive_gc(&svc);
            }
            Err(e) => tracing::warn!(error = %e, "archive sweep schedule refused; left off"),
        },
        Ok(None) => {}
        Err(e) => tracing::warn!(error = %e, "archive sweep schedule not read; left off"),
    }
    // A grant change is saved before it is pushed to RustFS; make the backend match what was saved.
    for (p, why) in svc.reconcile() {
        tracing::warn!(principal = %p, error = %why, "grants not pushed to the object store backend; refused by the service, but the backend key is behind");
    }
    // The mesh id may have appeared since the stores were made under `local`: same buckets, so
    // the stores, their grants and their keys just take the new name.
    let mesh = mesh_token();
    for r in svc.migrate_mesh(&mesh, "daemon") {
        match r {
            Ok(to) => tracing::info!(store = %to, "store moved from the local mesh name to the mesh id"),
            Err(e) => tracing::warn!(error = %e, "a store could not move to the mesh id; it is served under its old name"),
        }
    }
    // A node store that cannot be created (an id that is not a clean token, or RustFS is down)
    // must not take the other stores down: say so and carry on.
    match node_token(node_id).and_then(|t| StoreName::node(&mesh, &t).map_err(|e| e.to_string())) {
        Ok(node) => {
            if let Err(e) = svc.ensure_store(&node, "daemon", weftos_store::service::Parties::defaults_for(&node)) {
                tracing::warn!(error = %e, store = %node, "node store not created; other stores are served");
            }
        }
        Err(e) => tracing::warn!(error = %e, "node store not created; other stores are served"),
    }
    Ok(svc)
}

#[cfg(test)]
#[path = "store_rt_tests.rs"]
mod tests;
