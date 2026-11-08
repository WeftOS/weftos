//! A client account's store: S5's isolation gate in front of S1's `StoreService`
//! (ADR-126 S5).
//!
//! The gate ([`StoreGate`]) decides first; only a request it passes reaches the
//! service, which then applies its own grant table, exposure and audit (two
//! layers, so a bug in one does not open the store). The service is built on
//! this account's own backend, rooted under `/data1/<account>/store`, so its
//! buckets and keys exist only there.
//!
//! WeftOS-account and other-mesh callers have no S1 principal kind. They are
//! mapped to reserved S1 members (`weftos-<id>`, `xmesh-<mesh>`), and the
//! reserved prefixes are refused from [`ClientCaller::Local`], so a client
//! principal cannot name its way into one. A mapped member holds an S1 grant
//! only while the gate's matching grant is live: [`ClientStore::grant_weftos`]
//! and [`ClientStore::install_xmesh`] write both, and revoke, expiry
//! ([`ClientStore::sweep`]) and [`ClientStore::revoke_xmesh`] drop both.

use std::path::Path;
use std::sync::Arc;

use weftos_store::audit::{AuditEvent, AuditSink, Outcome, Source};
use weftos_store::service::{Caller, Config, ExposureGate, NoChain, Parties};
use weftos_store::{
    Access as S1Access, AdminBackend, FsBackend, ObjectBackend, ObjectMeta, Op, Principal as S1Principal,
    PrincipalKind, StoreError, StoreName, StoreService,
};

use crate::store_account::{
    Access, ClientStoreSpec, CrossMeshGrant, Principal, Refused, SpecError, StoreGate, StoreGrant,
};
use crate::weftos_uri::Authority;
use ed25519_dalek::VerifyingKey;

const WEFTOS_PREFIX: &str = "weftos-";
const XMESH_PREFIX: &str = "xmesh-";

/// Who is calling a client store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientCaller {
    /// A member, agent or cog of the client account.
    Local(S1Principal),
    /// The WeftOS account (or anything running as it).
    Weftos { id: String },
    /// A principal of another mesh.
    Foreign { mesh: [u8; 32], id: String },
}

fn to_s1_access(a: Access) -> S1Access {
    match a {
        Access::Read => S1Access::Read,
        Access::Write => S1Access::Write,
        Access::Admin => S1Access::Admin,
    }
}

fn mapped(kind: &str, id: &str) -> Result<S1Principal, Refused> {
    S1Principal::new(PrincipalKind::Member, &format!("{kind}{id}")).map_err(|_| Refused)
}

fn xmesh_member(mesh: &[u8; 32]) -> Result<S1Principal, Refused> {
    mapped(XMESH_PREFIX, &hex::encode(mesh))
}

/// The service's own refusal text names the rule that fired; a caller gets the same
/// "refused" as from the gate.
fn uni<T>(r: Result<T, StoreError>) -> Result<T, StoreError> {
    r.map_err(|e| match e {
        StoreError::Refused(_) => StoreError::Refused("refused".into()),
        o => o,
    })
}

/// One client account's governed store.
pub struct ClientStore {
    pub spec: ClientStoreSpec,
    gate: std::sync::Mutex<StoreGate>,
    svc: StoreService,
    audit: Arc<dyn AuditSink>,
    clock: fn() -> u64,
}

impl ClientStore {
    /// Open on this account's own directory. Refuses a root that is not owned by the
    /// client uid with mode 0700.
    #[cfg(unix)]
    pub fn open_fs(
        spec: ClientStoreSpec, mesh: Authority, exposure: Arc<dyn ExposureGate>, audit: Arc<dyn AuditSink>,
        clock: fn() -> u64,
    ) -> Result<Self, StoreError> {
        spec.verify_on_disk().map_err(|e: SpecError| StoreError::State(e.to_string()))?;
        // Nothing inside the root may be a link: a client-writable link could aim the daemon
        // at another account's tree.
        for leaf in ["data", "state.json", "gate.json", "gate.json.tmp"] {
            if std::fs::symlink_metadata(spec.root.join(leaf)).is_ok_and(|m| m.file_type().is_symlink()) {
                return Err(StoreError::State(format!("{leaf} under the store root is a symlink")));
            }
        }
        let be = Arc::new(FsBackend::open(&spec.root.join("data"))?);
        Self::open(spec, mesh, be.clone(), be, exposure, audit, clock)
    }

    /// Open on explicit backends (RustFS, or memory in tests). The caller has already
    /// pointed them at this account's tree.
    pub fn open(
        spec: ClientStoreSpec, mesh: Authority, admin: Arc<dyn AdminBackend>, objects: Arc<dyn ObjectBackend>,
        exposure: Arc<dyn ExposureGate>, audit: Arc<dyn AuditSink>, clock: fn() -> u64,
    ) -> Result<Self, StoreError> {
        let state_path = spec.root.join("state.json");
        let cfg = Config { node_id: format!("store-{}", spec.account), state_path: Some(state_path), clock };
        let svc = StoreService::open(cfg, admin, objects, exposure, audit.clone(), Arc::new(NoChain))?;
        let gate = StoreGate::load(mesh, &spec.account, &spec.root.join("gate.json")).map_err(StoreError::State)?;
        Ok(Self { spec, gate: std::sync::Mutex::new(gate), svc, audit, clock })
    }

    fn gate(&self) -> std::sync::MutexGuard<'_, StoreGate> {
        self.gate.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Write the gate's state so a restart picks it up.
    fn persist(&self) -> Result<(), StoreError> {
        self.gate().save(&self.spec.root.join("gate.json")).map_err(|e| StoreError::State(e.to_string()))
    }

    fn mesh_hex(&self) -> String {
        self.gate().mesh.to_string()
    }

    /// The store of a project this account holds. Creates it if it is not there.
    pub fn hold_project(&self, ulid: &str, actor: &str) -> Result<StoreName, StoreError> {
        let name = StoreName::project(&self.mesh_hex(), ulid)?;
        self.svc.ensure_store(&name, actor, Parties::default())?;
        self.gate().hold_project(ulid);
        self.persist()?;
        Ok(name)
    }

    fn gate_principal(&self, who: &ClientCaller) -> Result<(Principal, S1Principal, bool), Refused> {
        match who {
            ClientCaller::Local(p) => {
                if p.id.starts_with(WEFTOS_PREFIX) || p.id.starts_with(XMESH_PREFIX) {
                    return Err(Refused);
                }
                Ok((Principal::Client { account: self.spec.account.clone(), id: p.to_string() }, p.clone(), false))
            }
            ClientCaller::Weftos { id } => {
                Ok((Principal::WeftosAccount { id: id.clone() }, mapped(WEFTOS_PREFIX, id)?, false))
            }
            ClientCaller::Foreign { mesh, id } => {
                Ok((Principal::Foreign { mesh: *mesh, id: id.clone() }, xmesh_member(mesh)?, true))
            }
        }
    }

    /// Grant a local principal. Goes to the gate and to the service.
    pub fn grant_local(&self, p: &S1Principal, name: &StoreName, a: Access, actor: &str) -> Result<(), StoreError> {
        if p.id.starts_with(WEFTOS_PREFIX) || p.id.starts_with(XMESH_PREFIX) {
            return Err(StoreError::Refused("refused".into()));
        }
        let gp = Principal::Client { account: self.spec.account.clone(), id: p.to_string() };
        self.gate().grant(StoreGrant { principal: gp, store: name.to_string(), access: a });
        self.persist()?;
        self.svc.grant(name, p.clone(), to_s1_access(a), actor).map(|_| ())
    }

    /// Revoke a local principal's grant. Drops it from the gate and the service.
    pub fn revoke_local(&self, p: &S1Principal, name: &StoreName, actor: &str) -> Result<(), StoreError> {
        if p.id.starts_with(WEFTOS_PREFIX) || p.id.starts_with(XMESH_PREFIX) {
            return Err(StoreError::Refused("refused".into()));
        }
        let gp = Principal::Client { account: self.spec.account.clone(), id: p.to_string() };
        self.gate().revoke(&gp, &name.to_string());
        self.persist()?;
        self.svc.revoke(name, p, actor)
    }

    /// Grant the WeftOS account. This is the only way it gets in.
    pub fn grant_weftos(&self, id: &str, name: &StoreName, a: Access, actor: &str) -> Result<(), StoreError> {
        let m = mapped(WEFTOS_PREFIX, id).map_err(|_| StoreError::Refused("refused".into()))?;
        self.gate().grant(StoreGrant { principal: Principal::WeftosAccount { id: id.into() }, store: name.to_string(), access: a });
        self.persist()?;
        self.svc.grant(name, m, to_s1_access(a), actor).map(|_| ())
    }

    /// Revoke the WeftOS account's grant.
    pub fn revoke_weftos(&self, id: &str, name: &StoreName, actor: &str) -> Result<(), StoreError> {
        let m = mapped(WEFTOS_PREFIX, id).map_err(|_| StoreError::Refused("refused".into()))?;
        self.gate().revoke(&Principal::WeftosAccount { id: id.into() }, &name.to_string());
        self.persist()?;
        self.svc.revoke(name, &m, actor)
    }

    pub fn trust_mesh(&self, mesh: [u8; 32], key: VerifyingKey) -> Result<(), StoreError> {
        self.gate().trust_mesh(mesh, key);
        self.persist()
    }

    /// Install a cross-mesh grant (ADR-114 N5) and give its mesh member the matching S1 grant.
    pub fn install_xmesh(&self, g: CrossMeshGrant, actor: &str) -> Result<(), StoreError> {
        let now = (self.clock)() / 1000;
        let (name, member, access) = (StoreName::parse(&g.store)?, xmesh_member(&g.receiver_mesh), g.access);
        let (own, id) = (self.gate().mesh.0, g.id());
        // The S1 member exists only for grants where we own the store; on the receiving side
        // our own principals are granted with `grant_local`.
        let member = if g.owner_mesh == own { Some(member.map_err(|_| StoreError::Refused("refused".into()))?) } else { None };
        self.gate().install_xmesh(g, now).map_err(|_| StoreError::Refused("refused".into()))?;
        self.persist()?;
        let granted = member.map_or(Ok(()), |m| self.svc.grant(&name, m, to_s1_access(access), actor).map(|_| ()));
        if granted.is_err() {
            // Leave nothing half-installed: the gate must not allow what the service does not.
            self.gate().revoke_xmesh(id);
            let _ = self.persist();
        }
        granted
    }

    /// Revoke one cross-mesh grant now: the gate and the service both drop it.
    pub fn revoke_xmesh(&self, g: &CrossMeshGrant, actor: &str) -> Result<(), StoreError> {
        // A grant that a newer one replaced no longer owns the service-side member; dropping it
        // would cut off the newer grant.
        let live = self.gate().xmesh_installed(g.id());
        self.gate().revoke_xmesh(g.id());
        self.persist()?;
        if live {
            self.drop_member(g, actor)?;
        }
        Ok(())
    }

    fn drop_member(&self, g: &CrossMeshGrant, actor: &str) -> Result<(), StoreError> {
        let name = StoreName::parse(&g.store)?;
        if g.owner_mesh == self.gate().mesh.0 {
            let m = xmesh_member(&g.receiver_mesh).map_err(|_| StoreError::Refused("refused".into()))?;
            // No grant left is fine: the point is that none remains.
            let _ = self.svc.revoke(&name, &m, actor);
        }
        Ok(())
    }

    /// Drop the S1 side of every expired cross-mesh grant. The gate already refuses them by
    /// time; this removes the backend policy too. Returns how many.
    pub fn sweep(&self, actor: &str) -> usize {
        let now = (self.clock)() / 1000;
        let ex = self.gate().take_expired(now);
        for g in &ex {
            self.gate().revoke_xmesh(g.id());
            let _ = self.drop_member(g, actor);
        }
        // If this write fails the old file still lists the grants; they are expired, so the
        // gate refuses them by time after a reload and the next sweep drops them again.
        let _ = self.persist();
        ex.len()
    }

    /// Gate one request. A refusal is the same value for every cause; the cause goes to the log.
    fn pass(&self, who: &ClientCaller, name: &StoreName, op: Op, key: &str) -> Result<Caller, StoreError> {
        let now = (self.clock)() / 1000;
        let Ok((gp, sp, remote)) = self.gate_principal(who) else {
            self.audit.record(&AuditEvent {
                ts_ms: (self.clock)(),
                principal: match who {
                    ClientCaller::Local(p) => p.to_string(),
                    ClientCaller::Weftos { id } => format!("weftos-account:{id}"),
                    ClientCaller::Foreign { mesh, id } => format!("mesh:{}:{id}", hex::encode(mesh)),
                },
                store: name.to_string(),
                bucket: name.bucket(),
                op: op.name().into(),
                key: Some(key.into()),
                bytes: 0,
                outcome: Outcome::Refused("gate:reserved_or_invalid_principal".into()),
                source: Source::Governed,
            });
            return Err(StoreError::Refused("refused".into()));
        };
        let (r, a) = self.gate().authorize(&gp, &name.to_string(), match op.needs() {
            S1Access::Read => Access::Read,
            S1Access::Write => Access::Write,
            S1Access::Admin => Access::Admin,
        }, now);
        if r.is_err() {
            self.audit.record(&AuditEvent {
                ts_ms: (self.clock)(),
                principal: sp.to_string(),
                store: name.to_string(),
                bucket: name.bucket(),
                op: op.name().into(),
                key: Some(key.into()),
                bytes: 0,
                outcome: Outcome::Refused(format!("gate:{}", a.reason)),
                source: Source::Governed,
            });
            return Err(StoreError::Refused("refused".into()));
        }
        Ok(if remote { Caller::mesh(sp) } else { Caller::local(sp) })
    }

    pub fn put(&self, who: &ClientCaller, name: &StoreName, key: &str, data: &[u8], if_none_match: bool) -> Result<ObjectMeta, StoreError> {
        let c = self.pass(who, name, Op::Put, key)?;
        uni(self.svc.put(&c, name, key, data, if_none_match))
    }

    pub fn get(&self, who: &ClientCaller, name: &StoreName, key: &str) -> Result<Vec<u8>, StoreError> {
        let c = self.pass(who, name, Op::Get, key)?;
        uni(self.svc.get(&c, name, key))
    }

    pub fn delete(&self, who: &ClientCaller, name: &StoreName, key: &str) -> Result<(), StoreError> {
        let c = self.pass(who, name, Op::Delete, key)?;
        uni(self.svc.delete(&c, name, key))
    }

    pub fn list(&self, who: &ClientCaller, name: &StoreName, prefix: &str) -> Result<Vec<ObjectMeta>, StoreError> {
        let c = self.pass(who, name, Op::List, prefix)?;
        uni(self.svc.list(&c, name, prefix))
    }

    /// Where this account's bytes live.
    pub fn root(&self) -> &Path {
        &self.spec.root
    }
}

#[cfg(test)]
#[path = "client_store_tests.rs"]
mod tests;
