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

/// A node id as a store-name token: lowercase, anything else `-`, at most 61 characters.
pub fn node_token(node_id: &str) -> String {
    let t: String = node_id.to_lowercase().chars().map(|c| if c.is_ascii_lowercase() || c.is_ascii_digit() { c } else { '-' }).take(61).collect();
    if t.is_empty() { "node".into() } else { t }
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
    let node = StoreName::node(&mesh_token(), &node_token(node_id)).map_err(|e| e.to_string())?;
    // A node store that cannot be created (for example the mesh id appeared since the bucket was
    // made under `local`, or RustFS is down) must not take the other stores down: say so and carry on.
    if let Err(e) = svc.ensure_store(&node, "daemon", weftos_store::service::Parties::defaults_for(&node)) {
        tracing::warn!(error = %e, store = %node, "node store not created; other stores are served");
    }
    Ok(svc)
}

#[cfg(test)]
#[path = "store_rt_tests.rs"]
mod tests;
