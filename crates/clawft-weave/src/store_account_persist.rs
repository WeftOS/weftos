//! Durable state for [`StoreGate`] (ADR-126 S5).
//!
//! The gate keeps its held projects, local grants, trusted mesh keys, cross-mesh grants,
//! revocations and replay counters in memory; this module writes them to one JSON file under
//! the client's own root and reads them back. A missing file is an empty gate. A file that
//! does not parse, or that names another mesh or account, is an error, so a store never
//! opens on state it cannot account for. Cross-mesh grants are re-verified against the
//! trusted owner key on load and dropped if they no longer verify.
//!
//! The file is owned by the client user (mode 0600), like everything else under the root.
//! The chain stays the intended source of truth for grants (ADR-126 §3); this file is the
//! local cache the instance restarts from.

use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::*;

const VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
struct SPrincipal {
    kind: String,
    account: Option<String>,
    mesh: Option<String>,
    id: String,
}

#[derive(Serialize, Deserialize)]
struct SGrant {
    principal: SPrincipal,
    store: String,
    access: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SXmesh {
    owner: String,
    receiver: String,
    store: String,
    access: String,
    seq: u64,
    issued_at: u64,
    expires_at: u64,
    sig: String,
}

#[derive(Serialize, Deserialize)]
struct SSeq {
    owner: String,
    receiver: String,
    store: String,
    seq: u64,
}

#[derive(Serialize, Deserialize)]
struct Snap {
    version: u32,
    mesh: String,
    account: String,
    projects: Vec<String>,
    grants: Vec<SGrant>,
    mesh_keys: Vec<(String, String)>,
    xmesh: Vec<SXmesh>,
    revoked: Vec<String>,
    seqs: Vec<SSeq>,
}

impl SXmesh {
    fn from_grant(g: &CrossMeshGrant) -> Self {
        Self {
            owner: hex::encode(g.owner_mesh),
            receiver: hex::encode(g.receiver_mesh),
            store: g.store.clone(),
            access: access_str(g.access).into(),
            seq: g.seq,
            issued_at: g.issued_at,
            expires_at: g.expires_at,
            sig: hex::encode(g.sig),
        }
    }

    fn into_grant(self) -> Result<CrossMeshGrant, String> {
        Ok(CrossMeshGrant {
            owner_mesh: hex_n::<32>(&self.owner)?,
            receiver_mesh: hex_n::<32>(&self.receiver)?,
            store: self.store,
            access: access_of(&self.access)?,
            seq: self.seq,
            issued_at: self.issued_at,
            expires_at: self.expires_at,
            sig: hex_n::<64>(&self.sig)?,
        })
    }
}

impl CrossMeshGrant {
    /// The grant as JSON (hex for meshes and signature), the form `weaver store client` carries.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(SXmesh::from_grant(self)).unwrap_or(serde_json::Value::Null)
    }

    /// Parse the JSON form. This only checks shape; [`StoreGate::install_xmesh`] checks the signature.
    pub fn from_json(v: &serde_json::Value) -> Result<Self, String> {
        serde_json::from_value::<SXmesh>(v.clone()).map_err(|e| e.to_string())?.into_grant()
    }
}

fn access_str(a: Access) -> &'static str {
    match a {
        Access::Read => "read",
        Access::Write => "write",
        Access::Admin => "admin",
    }
}

fn access_of(s: &str) -> Result<Access, String> {
    match s {
        "read" => Ok(Access::Read),
        "write" => Ok(Access::Write),
        "admin" => Ok(Access::Admin),
        o => Err(format!("bad access {o:?}")),
    }
}

fn hex_n<const N: usize>(s: &str) -> Result<[u8; N], String> {
    let v = hex::decode(s).map_err(|e| e.to_string())?;
    v.try_into().map_err(|_| format!("expected {N} bytes"))
}

fn sprincipal(p: &Principal) -> SPrincipal {
    match p {
        Principal::Client { account, id } => {
            SPrincipal { kind: "client".into(), account: Some(account.clone()), mesh: None, id: id.clone() }
        }
        Principal::WeftosAccount { id } => SPrincipal { kind: "weftos".into(), account: None, mesh: None, id: id.clone() },
        Principal::Foreign { mesh, id } => {
            SPrincipal { kind: "foreign".into(), account: None, mesh: Some(hex::encode(mesh)), id: id.clone() }
        }
    }
}

fn principal_of(s: SPrincipal) -> Result<Principal, String> {
    match (s.kind.as_str(), s.account, s.mesh) {
        ("client", Some(account), None) => Ok(Principal::Client { account, id: s.id }),
        ("weftos", None, None) => Ok(Principal::WeftosAccount { id: s.id }),
        ("foreign", None, Some(m)) => Ok(Principal::Foreign { mesh: hex_n::<32>(&m)?, id: s.id }),
        _ => Err("bad principal".into()),
    }
}

impl StoreGate {
    fn snapshot(&self) -> Snap {
        Snap {
            version: VERSION,
            mesh: hex::encode(self.mesh.0),
            account: self.account.clone(),
            projects: self.projects.iter().cloned().collect(),
            grants: self
                .grants
                .iter()
                .map(|g| SGrant { principal: sprincipal(&g.principal), store: g.store.clone(), access: access_str(g.access).into() })
                .collect(),
            mesh_keys: self.mesh_keys.iter().map(|(m, k)| (hex::encode(m), hex::encode(k.to_bytes()))).collect(),
            xmesh: self.xmesh.iter().map(SXmesh::from_grant).collect(),
            revoked: self.revoked.iter().map(hex::encode).collect(),
            seqs: self
                .seqs
                .iter()
                .map(|((o, r, s), n)| SSeq { owner: hex::encode(o), receiver: hex::encode(r), store: s.clone(), seq: *n })
                .collect(),
        }
    }

    /// The gate's state in a canonical order, for hashing and snapshots. Cross-mesh grants past
    /// their end at `now` are left out: they cannot be used, and a snapshot must not carry them.
    fn canonical_snap(&self, now: u64) -> Snap {
        let mut s = self.snapshot();
        s.grants.sort_by_cached_key(|g| (serde_json::to_string(&g.principal).unwrap_or_default(), g.store.clone()));
        s.xmesh.retain(|x| x.expires_at > now);
        s.xmesh.sort_by(|a, b| a.sig.cmp(&b.sig));
        s.revoked.sort();
        s
    }

    /// The canonical state as JSON (see [`canonical_snap`](Self::canonical_snap)).
    pub fn state_json(&self, now: u64) -> serde_json::Value {
        serde_json::to_value(self.canonical_snap(now)).unwrap_or(serde_json::Value::Null)
    }

    /// Rebuild a gate from [`state_json`](Self::state_json). Cross-mesh grants are re-verified
    /// against the trusted keys in the same document.
    pub fn from_state_json(mesh: Authority, account: &str, v: &serde_json::Value) -> Result<Self, String> {
        Self::from_snap(mesh, account, serde_json::from_value(v.clone()).map_err(|e| e.to_string())?)
    }

    /// Write the gate to `path` atomically (temp file, fsync, rename), mode 0600.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let json = serde_json::to_vec_pretty(&self.snapshot()).map_err(std::io::Error::other)?;
        let tmp = path.with_extension("json.tmp");
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        f.write_all(&json)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)
    }

    /// Read the gate for `mesh` and `account` from `path`. A missing file is an empty gate.
    pub fn load(mesh: Authority, account: &str, path: &Path) -> Result<Self, String> {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::new(mesh, account)),
            Err(e) => return Err(e.to_string()),
        };
        let s: Snap = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        Self::from_snap(mesh, account, s)
    }

    /// Build a gate from a state document for `mesh` and `account`.
    fn from_snap(mesh: Authority, account: &str, s: Snap) -> Result<Self, String> {
        if s.version != VERSION {
            return Err(format!("gate state version {}", s.version));
        }
        if s.mesh != hex::encode(mesh.0) || s.account != account {
            return Err("gate state belongs to another mesh or account".into());
        }
        let mut g = Self::new(mesh, account);
        g.projects = s.projects.into_iter().collect();
        for (m, k) in s.mesh_keys {
            let key = VerifyingKey::from_bytes(&hex_n::<32>(&k)?).map_err(|e| e.to_string())?;
            g.mesh_keys.insert(hex_n::<32>(&m)?, key);
        }
        for sg in s.grants {
            g.grants.push(StoreGrant { principal: principal_of(sg.principal)?, store: sg.store, access: access_of(&sg.access)? });
        }
        for x in s.xmesh {
            let grant = x.into_grant()?;
            // A grant that no longer verifies against a trusted key is not carried over.
            let verifies = g.mesh_keys.get(&grant.owner_mesh).is_some_and(|k| {
                k.verify(&grant.signing_bytes(), &Signature::from_bytes(&grant.sig)).is_ok()
            });
            if verifies {
                g.xmesh.push(grant);
            }
        }
        for r in s.revoked {
            g.revoked.insert(hex_n::<64>(&r)?);
        }
        for q in s.seqs {
            g.seqs.insert((hex_n::<32>(&q.owner)?, hex_n::<32>(&q.receiver)?, q.store), q.seq);
        }
        Ok(g)
    }
}

/// One change to the gate. The chain journal records these in order; replaying them through
/// [`StoreGate::apply`] rebuilds the gate (ADR-126 S5), checked by the same rules as a live change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateEvent {
    Hold { ulid: String },
    Grant(StoreGrant),
    Revoke { principal: Principal, store: String },
    Trust { mesh: [u8; 32], key: [u8; 32] },
    /// `at` is the unix second the install was checked at, so replay decides exactly as the live
    /// call did, whatever the clock says now.
    XmeshInstall { grant: CrossMeshGrant, at: u64 },
    XmeshRevoke { id: [u8; 64] },
}

fn str_of<'a>(v: &'a serde_json::Value, k: &str) -> Result<&'a str, String> {
    v.get(k).and_then(|x| x.as_str()).ok_or_else(|| format!("missing {k}"))
}

impl GateEvent {
    /// The kind suffix: the chain event is `store.client.gate.<kind>`.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Hold { .. } => "hold",
            Self::Grant(_) => "grant",
            Self::Revoke { .. } => "revoke",
            Self::Trust { .. } => "trust",
            Self::XmeshInstall { .. } => "xmesh_install",
            Self::XmeshRevoke { .. } => "xmesh_revoke",
        }
    }

    pub fn to_json(&self) -> serde_json::Value {
        use serde_json::json;
        match self {
            Self::Hold { ulid } => json!({ "ulid": ulid }),
            Self::Grant(g) => json!({
                "principal": serde_json::to_value(sprincipal(&g.principal)).unwrap_or_default(),
                "store": g.store, "access": access_str(g.access),
            }),
            Self::Revoke { principal, store } => json!({
                "principal": serde_json::to_value(sprincipal(principal)).unwrap_or_default(), "store": store,
            }),
            Self::Trust { mesh, key } => json!({ "mesh": hex::encode(mesh), "key": hex::encode(key) }),
            Self::XmeshInstall { grant, at } => json!({ "grant": grant.to_json(), "at": at }),
            Self::XmeshRevoke { id } => json!({ "id": hex::encode(id) }),
        }
    }

    pub fn from_json(kind: &str, v: &serde_json::Value) -> Result<Self, String> {
        let principal = || -> Result<Principal, String> {
            principal_of(serde_json::from_value(v.get("principal").cloned().unwrap_or_default()).map_err(|e| e.to_string())?)
        };
        match kind {
            "hold" => Ok(Self::Hold { ulid: str_of(v, "ulid")?.to_owned() }),
            "grant" => Ok(Self::Grant(StoreGrant {
                principal: principal()?,
                store: str_of(v, "store")?.to_owned(),
                access: access_of(str_of(v, "access")?)?,
            })),
            "revoke" => Ok(Self::Revoke { principal: principal()?, store: str_of(v, "store")?.to_owned() }),
            "trust" => Ok(Self::Trust { mesh: hex_n::<32>(str_of(v, "mesh")?)?, key: hex_n::<32>(str_of(v, "key")?)? }),
            "xmesh_install" => Ok(Self::XmeshInstall {
                grant: CrossMeshGrant::from_json(v.get("grant").ok_or("missing grant")?)?,
                at: v.get("at").and_then(|x| x.as_u64()).ok_or("missing at")?,
            }),
            "xmesh_revoke" => Ok(Self::XmeshRevoke { id: hex_n::<64>(str_of(v, "id")?)? }),
            o => Err(format!("unknown gate event {o:?}")),
        }
    }
}

impl StoreGate {
    /// Apply one journalled change. A cross-mesh install is checked at the time it recorded
    /// (`at`), not the clock now, so replay reproduces the live decision exactly. The grant
    /// stays installed after it expires and is refused by time until swept, as it was live.
    pub fn apply(&mut self, ev: &GateEvent) -> Result<(), Refused> {
        match ev {
            GateEvent::Hold { ulid } => self.hold_project(ulid),
            GateEvent::Grant(g) => self.grant(g.clone()),
            GateEvent::Revoke { principal, store } => self.revoke(principal, store),
            GateEvent::Trust { mesh, key } => self.trust_mesh(*mesh, VerifyingKey::from_bytes(key).map_err(|_| Refused)?),
            GateEvent::XmeshInstall { grant, at } => self.install_xmesh(grant.clone(), *at)?,
            GateEvent::XmeshRevoke { id } => self.revoke_xmesh(*id),
        }
        Ok(())
    }
}
