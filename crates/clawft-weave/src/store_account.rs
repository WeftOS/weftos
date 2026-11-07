//! Per-client-account store instance and its access gate (ADR-126 S5).
//!
//! One RustFS instance per client account, run as that account's unix user with
//! its data under `/data1/<account>/store` ([`ClientStoreSpec`]). One instance
//! never holds two accounts' data, so the PG client-isolation rules (an account
//! reads only its own tree) hold for the store the same as for the checkout.
//!
//! Access is a grant of (principal, store, read|write|admin) (ADR-126 §3). The
//! gate ([`StoreGate::authorize`]) has no default grants and no inheritance:
//!
//! * a **WeftOS-account principal** (the `weftos` account, or anything acting for
//!   it) is refused unless a grant names it, and `admin` elsewhere never counts;
//! * a principal of **another mesh** needs a [`CrossMeshGrant`] signed by the
//!   owning mesh (ADR-114 N5), naming this mesh and the exact store, unexpired
//!   and not revoked. Revoking stops the next call;
//! * every refusal is the same [`Refused`], so a caller cannot probe which
//!   rule fired. The reason goes to the returned [`Audit`] line for the log.
//!
//! This module defines no store client. It sits on the ADR-126 §7 names
//! (`weftos://<mesh>/projects/<ULID>/store`) and leaves bucket mapping and the
//! RustFS admin calls to `weftos-store` (S1).

use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Component, Path, PathBuf};

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};

use crate::weftos_uri::{Authority, Kind, WeftosUri};

/// First uid of a client account (ADR-113: promoted projects start at 2102).
pub const MIN_CLIENT_UID: u32 = 2102;
/// Root every client store lives under.
pub const DATA_ROOT: &str = "/data1";
/// The mesh's own account; never a client.
pub const WEFTOS_ACCOUNT: &str = "weftos";
const GRANT_DOMAIN: &[u8] = b"weftos-store-xmesh-grant-v1\0";
/// Longest a cross-mesh grant may live, in seconds (one year).
pub const MAX_GRANT_TTL_SECS: u64 = 365 * 24 * 3600;

/// Why a spec or path was refused (setup time, so the detail is fine to show).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SpecError {
    #[error("account name must be 1-32 chars of [a-z0-9_-], starting with a letter")]
    Account,
    #[error("the weftos account is not a client account")]
    NotClient,
    #[error("uid {0} is below the client range")]
    Uid(u32),
    #[error("store root must be {DATA_ROOT}/<account>/store with no '..' or links")]
    Root,
    #[error("store must listen on loopback or a tailnet address, not a wildcard")]
    Listen,
    #[error("store root is not owned by the client user with mode 0700")]
    Ownership,
}

/// One client account's store instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientStoreSpec {
    pub account: String,
    pub uid: u32,
    pub root: PathBuf,
    pub listen: SocketAddr,
}

fn account_ok(a: &str) -> bool {
    let mut c = a.chars();
    a.len() <= 32
        && c.next().is_some_and(|f| f.is_ascii_lowercase())
        && a.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

impl ClientStoreSpec {
    /// The standard instance for `account`: data under `/data1/<account>/store`,
    /// loopback on `port`.
    pub fn standard(account: &str, uid: u32, port: u16) -> Result<Self, SpecError> {
        let root = Path::new(DATA_ROOT).join(account).join("store");
        Self::new(account, uid, root, SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port))
    }

    /// Validate and build. The root must be exactly `/data1/<account>/store`.
    pub fn new(account: &str, uid: u32, root: PathBuf, listen: SocketAddr) -> Result<Self, SpecError> {
        if !account_ok(account) {
            return Err(SpecError::Account);
        }
        if account == WEFTOS_ACCOUNT {
            return Err(SpecError::NotClient);
        }
        if uid < MIN_CLIENT_UID {
            return Err(SpecError::Uid(uid));
        }
        if root.components().any(|c| matches!(c, Component::ParentDir | Component::CurDir))
            || root != Path::new(DATA_ROOT).join(account).join("store")
        {
            return Err(SpecError::Root);
        }
        if listen.ip().is_unspecified() || listen.port() == 0 {
            return Err(SpecError::Listen);
        }
        Ok(Self { account: account.to_owned(), uid, root, listen })
    }

    /// Check the directory on disk: no symlink, owned by the client uid, mode 0700.
    #[cfg(unix)]
    pub fn verify_on_disk(&self) -> Result<(), SpecError> {
        use std::os::unix::fs::MetadataExt;
        let m = std::fs::symlink_metadata(&self.root).map_err(|_| SpecError::Ownership)?;
        if !m.is_dir() || m.uid() != self.uid || m.mode() & 0o777 != 0o700 {
            return Err(SpecError::Ownership);
        }
        Ok(())
    }

    /// The systemd unit: runs as the client user, sees only its own tree.
    pub fn systemd_unit(&self, binary: &Path) -> String {
        let root = self.root.display();
        format!(
            "[Unit]\nDescription=WeftOS store for account {acct} (ADR-126 S5)\nAfter=network-online.target\n\n\
             [Service]\nUser={acct}\nUMask=0077\nEnvironmentFile=-{data_root}/{acct}/store.env\n\
             Environment=RUSTFS_ADDRESS={listen}\nEnvironment=RUSTFS_AUDIT_ENABLE=true\n\
             ExecStart={bin} server {root}/data\nRestart=on-failure\n\
             NoNewPrivileges=true\nProtectSystem=strict\nProtectHome=true\nPrivateTmp=true\n\
             TemporaryFileSystem={data_root}:ro\nBindPaths={root}\n\
             IPAddressAllow=localhost\nIPAddressDeny=any\n\n[Install]\nWantedBy=multi-user.target\n",
            acct = self.account,
            listen = self.listen,
            bin = binary.display(),
            data_root = DATA_ROOT,
        )
    }
}

/// Who is asking.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Principal {
    /// A member, agent or cog of the client account itself.
    Client { account: String, id: String },
    /// The WeftOS account, or anything running as it.
    WeftosAccount { id: String },
    /// A principal of another mesh (reached through a cross-mesh grant).
    Foreign { mesh: [u8; 32], id: String },
}

/// What a grant allows. Ordered: admin implies write implies read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Access {
    Read,
    Write,
    Admin,
}

/// A local grant, recorded on the chain by the daemon (ADR-126 §3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreGrant {
    pub principal: Principal,
    /// The store's name, e.g. `weftos://<mesh>/projects/<ULID>/store`. Exact match.
    pub store: String,
    pub access: Access,
}

/// A grant signed by the owning mesh for one other mesh (ADR-114 N5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrossMeshGrant {
    pub owner_mesh: [u8; 32],
    pub receiver_mesh: [u8; 32],
    pub store: String,
    pub access: Access,
    pub seq: u64,
    pub issued_at: u64,
    pub expires_at: u64,
    pub sig: [u8; 64],
}

impl CrossMeshGrant {
    fn signing_bytes(&self) -> Vec<u8> {
        let mut b = GRANT_DOMAIN.to_vec();
        b.extend_from_slice(&self.owner_mesh);
        b.extend_from_slice(&self.receiver_mesh);
        b.extend_from_slice(&(self.store.len() as u32).to_be_bytes());
        b.extend_from_slice(self.store.as_bytes());
        b.push(self.access as u8);
        b.extend_from_slice(&self.seq.to_be_bytes());
        b.extend_from_slice(&self.issued_at.to_be_bytes());
        b.extend_from_slice(&self.expires_at.to_be_bytes());
        b
    }

    /// Sign as the owning mesh.
    #[allow(clippy::too_many_arguments)]
    pub fn sign(
        key: &SigningKey, owner_mesh: [u8; 32], receiver_mesh: [u8; 32], store: &str, access: Access,
        seq: u64, issued_at: u64, expires_at: u64,
    ) -> Self {
        let mut g = Self {
            owner_mesh, receiver_mesh, store: store.to_owned(), access, seq, issued_at, expires_at, sig: [0; 64],
        };
        g.sig = key.sign(&g.signing_bytes()).to_bytes();
        g
    }

    /// Identity for revocation: the signature itself, so a re-issue (new seq) is a new grant.
    pub fn id(&self) -> [u8; 64] {
        self.sig
    }
}

/// The one refusal every denial returns. It carries no reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("refused")]
pub struct Refused;

/// What the log gets: the real reason, never returned to the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Audit {
    pub principal: Principal,
    pub store: String,
    pub access: Access,
    pub allowed: bool,
    pub reason: &'static str,
}

/// The gate for one client instance.
pub struct StoreGate {
    pub mesh: Authority,
    pub account: String,
    /// Project ULIDs this instance holds. A store of any other project is not here.
    projects: BTreeSet<String>,
    grants: Vec<StoreGrant>,
    /// Owning mesh -> its verifying key (from pairing, ADR-108).
    mesh_keys: BTreeMap<[u8; 32], VerifyingKey>,
    xmesh: Vec<CrossMeshGrant>,
    revoked: BTreeSet<[u8; 64]>,
    /// Latest seq seen per (owner, receiver, store); an older one is a replay.
    seqs: BTreeMap<([u8; 32], [u8; 32], String), u64>,
}

impl StoreGate {
    pub fn new(mesh: Authority, account: &str) -> Self {
        Self {
            mesh, account: account.to_owned(), projects: BTreeSet::new(), grants: Vec::new(),
            mesh_keys: BTreeMap::new(), xmesh: Vec::new(), revoked: BTreeSet::new(), seqs: BTreeMap::new(),
        }
    }

    pub fn hold_project(&mut self, ulid: &str) {
        self.projects.insert(ulid.to_owned());
    }

    pub fn grant(&mut self, g: StoreGrant) {
        self.grants.push(g);
    }

    /// Remove every local grant for `principal` on `store`.
    pub fn revoke(&mut self, principal: &Principal, store: &str) {
        self.grants.retain(|g| !(g.principal == *principal && g.store == store));
    }

    pub fn trust_mesh(&mut self, mesh: [u8; 32], key: VerifyingKey) {
        self.mesh_keys.insert(mesh, key);
    }

    /// Install a cross-mesh grant. Refused when the signature, owner, receiver, store,
    /// ttl or seq is wrong.
    pub fn install_xmesh(&mut self, g: CrossMeshGrant, now: u64) -> Result<(), Refused> {
        let key = self.mesh_keys.get(&g.owner_mesh).ok_or(Refused)?;
        let sig = Signature::from_bytes(&g.sig);
        key.verify(&g.signing_bytes(), &sig).map_err(|_| Refused)?;
        let name = WeftosUri::parse(&g.store).map_err(|_| Refused)?;
        let ok_ttl = g.expires_at > g.issued_at && g.expires_at - g.issued_at <= MAX_GRANT_TTL_SECS;
        // Receiving side: the owner is another mesh. Owning side: the owner is us.
        let ours = g.receiver_mesh == self.mesh.0 || g.owner_mesh == self.mesh.0;
        if !ours || name.authority.0 != g.owner_mesh || !ok_ttl || g.expires_at <= now {
            return Err(Refused);
        }
        let k = (g.owner_mesh, g.receiver_mesh, g.store.clone());
        if self.seqs.get(&k).is_some_and(|s| *s >= g.seq) {
            return Err(Refused);
        }
        self.seqs.insert(k, g.seq);
        // A newer grant replaces the older one for the same pair and store, so a downgrade sticks.
        self.xmesh.retain(|o| !(o.owner_mesh == g.owner_mesh && o.receiver_mesh == g.receiver_mesh && o.store == g.store));
        self.xmesh.push(g);
        Ok(())
    }

    /// Revoke a cross-mesh grant; takes effect on the next [`authorize`](Self::authorize).
    pub fn revoke_xmesh(&mut self, id: [u8; 64]) {
        self.revoked.insert(id);
    }

    /// Decide one request. `now` is unix seconds.
    pub fn authorize(&self, who: &Principal, store: &str, want: Access, now: u64) -> (Result<(), Refused>, Audit) {
        let reason = self.decide(who, store, want, now);
        let allowed = reason == "granted";
        let audit = Audit { principal: who.clone(), store: store.to_owned(), access: want, allowed, reason };
        (if allowed { Ok(()) } else { Err(Refused) }, audit)
    }

    fn decide(&self, who: &Principal, store: &str, want: Access, now: u64) -> &'static str {
        let Ok(name) = WeftosUri::parse(store) else { return "bad_name" };
        // The store must be a project store, on this mesh, held by this instance.
        if name.kind != Kind::Projects || name.path.as_slice() != ["store"] {
            return "not_a_project_store";
        }
        let ours = name.authority == self.mesh;
        if ours && !self.projects.contains(&name.id) {
            return "not_held_here";
        }
        let xmesh = |receiver: &[u8; 32]| {
            self.xmesh.iter().any(|g| {
                g.owner_mesh == name.authority.0 && g.receiver_mesh == *receiver && g.store == store
                    && g.access >= want && g.expires_at > now && !self.revoked.contains(&g.id())
            })
        };
        match who {
            Principal::Client { account, .. } if *account != self.account => "other_account",
            Principal::Client { .. } | Principal::WeftosAccount { .. } => {
                // A name owned by another mesh also needs that mesh's signed grant (ADR-114 N5).
                if !ours && !xmesh(&self.mesh.0) {
                    return "no_xmesh_grant";
                }
                let ok = self.grants.iter().any(|g| g.principal == *who && g.store == store && g.access >= want);
                if ok {
                    "granted"
                } else if matches!(who, Principal::WeftosAccount { .. }) {
                    "weftos_not_granted"
                } else {
                    "no_grant"
                }
            }
            // Another mesh's principal in our store: the grant we signed for its mesh is the ticket.
            Principal::Foreign { mesh, .. } => {
                if ours && xmesh(mesh) { "granted" } else { "no_xmesh_grant" }
            }
        }
    }
}

/// The S3 policy RustFS IAM gets for one grant (ADR-126 §3): one bucket, no others.
pub fn iam_policy(bucket: &str, access: Access) -> Result<String, SpecError> {
    let ok = !bucket.is_empty()
        && bucket.len() <= 63
        && bucket.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if !ok {
        return Err(SpecError::Root);
    }
    let actions: &[&str] = match access {
        Access::Read => &["s3:GetObject", "s3:ListBucket"],
        Access::Write => &["s3:GetObject", "s3:ListBucket", "s3:PutObject", "s3:DeleteObject", "s3:AbortMultipartUpload"],
        Access::Admin => &["s3:*"],
    };
    let acts = actions.iter().map(|a| format!("\"{a}\"")).collect::<Vec<_>>().join(",");
    Ok(format!(
        "{{\"Version\":\"2012-10-17\",\"Statement\":[{{\"Effect\":\"Allow\",\"Action\":[{acts}],\"Resource\":[\"arn:aws:s3:::{bucket}\",\"arn:aws:s3:::{bucket}/*\"]}}]}}"
    ))
}

#[cfg(test)]
#[path = "store_account_tests.rs"]
mod tests;
