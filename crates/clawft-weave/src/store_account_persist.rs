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
            xmesh: self
                .xmesh
                .iter()
                .map(|g| SXmesh {
                    owner: hex::encode(g.owner_mesh),
                    receiver: hex::encode(g.receiver_mesh),
                    store: g.store.clone(),
                    access: access_str(g.access).into(),
                    seq: g.seq,
                    issued_at: g.issued_at,
                    expires_at: g.expires_at,
                    sig: hex::encode(g.sig),
                })
                .collect(),
            revoked: self.revoked.iter().map(hex::encode).collect(),
            seqs: self
                .seqs
                .iter()
                .map(|((o, r, s), n)| SSeq { owner: hex::encode(o), receiver: hex::encode(r), store: s.clone(), seq: *n })
                .collect(),
        }
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
            let grant = CrossMeshGrant {
                owner_mesh: hex_n::<32>(&x.owner)?,
                receiver_mesh: hex_n::<32>(&x.receiver)?,
                store: x.store,
                access: access_of(&x.access)?,
                seq: x.seq,
                issued_at: x.issued_at,
                expires_at: x.expires_at,
                sig: hex_n::<64>(&x.sig)?,
            };
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
