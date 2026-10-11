//! Pairing two nodes for project work from the dashboard (ADR-108 decision 6,
//! phase P2b; wire shapes in `docs/plans/adr-108-p2b-p3-contract.md`).
//!
//! Identity. A node's mesh identity is the key its `workload-host` signs
//! with: the node key, or the control key in service mode (ADR-106 phase 3).
//! `node` is `node_id_from_pubkey` of that key (32 hex) and `fingerprint` is
//! its first 16 hex characters, which is exactly the first 16 hex of the
//! SHA-256 of the key (ADR-025). The key itself is not derivable from the
//! id, so the `pair` payload also carries `peer_ed25519` (64 hex), and the
//! receiver verifies both the id and the fingerprint against it before it
//! writes anything.
//!
//! Trust files. On `add` the **member** writes the primary into
//! `workload-peers.json` at tier `pinned`, with the payload's `advertise`
//! as its address (required: a member must be able to dial its primary); the
//! **primary** writes the member at tier `paired`, keyed, with the payload's
//! `advertise` when there is one and no address otherwise (a member whose
//! mesh listens on loopback; it is trusted for project fetch but never
//! dialed), and a grant into `project-fetch.json`
//! ([`crate::project_fetch_grants`]). Neither
//! side ever touches `workload-host.json`: a paired peer is never a
//! controller. `remove` undoes both. The placement control plane reads
//! `workload-peers.json` on every call (`sync_peers`), so a write takes
//! effect on the next placement or node-admin call; no restart.
//!
//! Every add and remove is chained (`mesh.pair.add`, `mesh.pair.remove`)
//! with the request id and the dashboard action id. Nothing here logs,
//! chains or returns key material.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use clawft_kernel::chain::ChainManager;
use clawft_types::placement::TrustTier;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::dashboard_actions::{Action, ActionHandler, ActionOutcome};
use crate::mesh_pair_requests::{self, PairRequest};
use crate::project_fetch_grants::{self, Grant};

/// The action kind this module answers.
pub const KIND: &str = "pair";
/// The peers file under the runtime dir (the placement policy reads it).
pub const PEERS_FILE: &str = "workload-peers.json";
/// Chain source of the pairing records.
pub const CHAIN_SOURCE: &str = "mesh.pair";
/// Chain kinds.
pub const CHAIN_ADD: &str = "mesh.pair.add";
pub const CHAIN_REMOVE: &str = "mesh.pair.remove";
/// Hex characters in a fingerprint.
pub const FINGERPRINT_HEX: usize = 16;
const MAX_PEERS: usize = 256;

/// `report.mesh_identity` (contract section 1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MeshIdentity {
    /// Mesh node id (32 hex), derived from `ed25519`.
    pub node: String,
    /// First 16 hex of `node` (= first 16 hex of SHA-256 of the key).
    pub fingerprint: String,
    /// The signing key's public half, 64 hex; what a peer pins.
    pub ed25519: String,
    /// Address peers should dial; absent when this node has none to offer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub advertise: Option<String>,
}

/// The fingerprint of a public key.
pub fn fingerprint_of(pk: &[u8; 32]) -> String {
    clawft_kernel::node_id_from_pubkey(pk)[..FINGERPRINT_HEX].to_owned()
}

impl MeshIdentity {
    pub fn from_pubkey(pk: &[u8; 32], advertise: Option<String>) -> Self {
        let node = clawft_kernel::node_id_from_pubkey(pk);
        Self { fingerprint: node[..FINGERPRINT_HEX].to_owned(), node, ed25519: hex::encode(pk), advertise }
    }
}

/// What the reporter asks for on every beat.
#[async_trait]
pub trait PairSource: Send + Sync {
    /// This node's identity, or `None` when the mesh is off.
    async fn mesh_identity(&self) -> Option<MeshIdentity>;
    /// Pending requests, oldest first (the reporter caps them).
    async fn pair_requests(&self) -> Vec<PairRequest>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Op {
    Add,
    Remove,
}

/// The **other** side's role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Primary,
    Member,
}

/// The `pair` action payload (contract section 2, plus `peer_ed25519`).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairPayload {
    pub op: Op,
    pub request_id: String,
    pub peer_node: String,
    pub fingerprint: String,
    pub peer_ed25519: String,
    /// Where the peer can be dialed. Optional when this node is the primary
    /// (the member is recorded by key and not dialed); required when this
    /// node is the member, which must be able to reach its primary.
    #[serde(default)]
    pub advertise: Option<String>,
    pub role: Role,
    #[serde(default)]
    pub projects: Vec<String>,
}

fn lower_hex(s: &str, n: usize) -> bool {
    s.len() == n && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn id_ok(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// `host:port` as the placement policy accepts it.
fn host_port(a: &str) -> bool {
    !a.is_empty()
        && a.len() <= 260
        && a.contains(':')
        && !a.starts_with("mem://")
        && a.chars().all(|c| c.is_ascii_alphanumeric() || ".-:[]_".contains(c))
}

impl PairPayload {
    /// Parse and validate the shape; nothing is trusted yet.
    pub fn parse(v: &Value) -> Result<Self, String> {
        let p: Self = serde_json::from_value(v.clone()).map_err(|e| format!("pair payload: {e}"))?;
        if !id_ok(&p.request_id) {
            return Err("pair payload: request_id must be [A-Za-z0-9_-]{1,64}".into());
        }
        if !clawft_kernel::is_node_id(&p.peer_node) {
            return Err("pair payload: peer_node is not a mesh node id (32 lower-case hex)".into());
        }
        if !lower_hex(&p.fingerprint, FINGERPRINT_HEX) {
            return Err(format!("pair payload: fingerprint must be {FINGERPRINT_HEX} lower-case hex"));
        }
        if !lower_hex(&p.peer_ed25519.to_ascii_lowercase(), 64) {
            return Err("pair payload: peer_ed25519 must be 64 hex".into());
        }
        if p.advertise.as_deref().is_some_and(|a| !host_port(a)) {
            return Err("pair payload: advertise is not host:port".into());
        }
        mesh_pair_requests::projects_ok(&p.projects).map_err(|e| format!("pair payload: projects: {e}"))?;
        Ok(p)
    }

    /// The peer's key, once `peer_node` and `fingerprint` are shown to be
    /// its own. Refused otherwise: nothing is written on a mismatch.
    pub fn verify(&self) -> Result<[u8; 32], String> {
        let pk: [u8; 32] = hex::decode(&self.peer_ed25519)
            .ok()
            .and_then(|b| b.try_into().ok())
            .ok_or("pair payload: peer_ed25519 must be 64 hex")?;
        ed25519_dalek::VerifyingKey::from_bytes(&pk).map_err(|_| "pair payload: peer_ed25519 is not an Ed25519 public key")?;
        let derived = clawft_kernel::node_id_from_pubkey(&pk);
        if derived != self.peer_node {
            return Err("pair refused: peer_ed25519 is not the key of peer_node".into());
        }
        if derived[..FINGERPRINT_HEX] != self.fingerprint {
            return Err("pair refused: fingerprint does not match peer_node".into());
        }
        Ok(pk)
    }

    /// The address to record for the peer. A member (other side is the
    /// primary) must have one to add the pairing; the primary may not.
    fn advertise_for_add(&self) -> Result<Option<&str>, String> {
        match (self.advertise.as_deref(), self.we_are_primary()) {
            (None, false) => Err("pair payload: missing field advertise: a member must be able to dial its primary, \
                                  and the primary reported no address (set [kernel.mesh] listen_addr to a tailnet address there)"
                .into()),
            (a, _) => Ok(a),
        }
    }

    /// The tier this node gives the peer: a primary is `pinned` on its
    /// members; a member is `paired` on the primary.
    pub fn tier(&self) -> TrustTier {
        match self.role {
            Role::Primary => TrustTier::Pinned,
            Role::Member => TrustTier::Paired,
        }
    }

    /// This node is the primary (the other side is a member).
    pub fn we_are_primary(&self) -> bool {
        self.role == Role::Member
    }
}

fn tier_name(t: TrustTier) -> &'static str {
    match t {
        TrustTier::Pinned => "pinned",
        TrustTier::Paired => "paired",
        TrustTier::Discovered => "discovered",
    }
}

fn read_peers(dir: &Path) -> Result<Vec<Value>, String> {
    let path = dir.join(PEERS_FILE);
    let Ok(m) = std::fs::metadata(&path) else { return Ok(Vec::new()) };
    project_fetch_grants::private_file_ok(&path, &m)?;
    if m.len() > 256 * 1024 {
        return Err(format!("{PEERS_FILE} is too large"));
    }
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{PEERS_FILE}: {e}"))?;
    match serde_json::from_str::<Value>(&text).map_err(|e| format!("{PEERS_FILE}: {e}"))? {
        Value::Array(a) => Ok(a),
        _ => Err(format!("{PEERS_FILE}: not an array")),
    }
}

fn write_peers(dir: &Path, peers: &[Value]) -> Result<(), String> {
    let text = serde_json::to_string_pretty(peers).map_err(|e| e.to_string())?;
    crate::dashboard_token::write_atomic(&dir.join(PEERS_FILE), &text).map_err(|e| format!("{PEERS_FILE}: {e}"))
}

fn has_key(entry: &Value, key_hex: &str) -> bool {
    entry.get("key").and_then(Value::as_str).is_some_and(|k| k.eq_ignore_ascii_case(key_hex))
}

/// Give `key_hex` the tier at `addr`: the entry for that key is updated,
/// or one is appended. Every other entry is kept as it is.
/// `addr` is `None` for a peer with no address to dial: it is recorded by key only.
pub fn peers_add(dir: &Path, key_hex: &str, tier: TrustTier, addr: Option<&str>) -> Result<(), String> {
    let mut peers = read_peers(dir)?;
    let mut entry = json!({ "tier": tier_name(tier), "key": key_hex.to_ascii_lowercase() });
    if let Some(a) = addr {
        entry["addr"] = json!(a);
    }
    match peers.iter().position(|e| has_key(e, key_hex)) {
        Some(i) => peers[i] = entry,
        None if peers.len() >= MAX_PEERS => return Err(format!("{PEERS_FILE}: more than {MAX_PEERS} peers")),
        None => peers.push(entry),
    }
    write_peers(dir, &peers)
}

/// Drop every entry for `key_hex`. False when there was none.
pub fn peers_remove(dir: &Path, key_hex: &str) -> Result<bool, String> {
    let mut peers = read_peers(dir)?;
    let before = peers.len();
    peers.retain(|e| !has_key(e, key_hex));
    if peers.len() == before {
        return Ok(false);
    }
    write_peers(dir, &peers)?;
    Ok(true)
}

/// What the handler needs; set once at daemon boot ([`init`]).
pub struct PairDeps {
    /// Where the trust files are.
    pub runtime_dir: PathBuf,
    /// The daemon's chain; `None` only in a build without one (then nothing is chained and a warning is logged).
    pub chain: Option<Arc<ChainManager>>,
    /// This node's own mesh node id, to refuse pairing with itself.
    pub self_node: Option<String>,
}

static DEPS: OnceLock<Arc<PairDeps>> = OnceLock::new();

/// Record the handler's dependencies (first call wins).
pub fn init(deps: PairDeps) -> bool {
    DEPS.set(Arc::new(deps)).is_ok()
}

/// The dependencies, once [`init`] ran.
pub fn deps() -> Option<Arc<PairDeps>> {
    DEPS.get().cloned()
}

/// Apply one verified `pair` action to the trust files and the chain.
pub fn apply(deps: &PairDeps, action_id: &str, payload: &Value) -> Result<Value, String> {
    let p = PairPayload::parse(payload)?;
    let pk = p.verify()?;
    if deps.self_node.as_deref() == Some(p.peer_node.as_str()) {
        return Err("pair refused: peer_node is this node".into());
    }
    let dir = &deps.runtime_dir;
    let key_hex = hex::encode(pk);
    let tier = p.tier();
    let kind = match p.op {
        Op::Add => {
            let advertise = p.advertise_for_add()?;
            peers_add(dir, &key_hex, tier, advertise)?;
            if let (false, Some(a)) = (p.we_are_primary(), advertise) {
                // The member remembers which primary serves which projects (ADR-114 names).
                crate::mesh_pairings::upsert(dir, &p.peer_node, "primary", &p.projects, a)?;
            }
            if p.we_are_primary() {
                project_fetch_grants::grant(
                    dir,
                    &Grant {
                        peer_node: p.peer_node.clone(),
                        projects: p.projects.clone(),
                        granted_at: Some(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
                        source: Some(format!("dashboard-pair:{action_id}")),
                        peer_ed25519: Some(key_hex.clone()),
                    },
                )?;
            }
            CHAIN_ADD
        }
        Op::Remove => {
            peers_remove(dir, &key_hex)?;
            crate::mesh_pairings::remove(dir, &p.peer_node)?;
            project_fetch_grants::revoke(dir, &p.peer_node)?;
            CHAIN_REMOVE
        }
    };
    if let Err(e) = mesh_pair_requests::settle(dir, &p.peer_node, Some(&p.request_id)) {
        tracing::warn!(error = %e, "pair applied but pending requests could not be settled");
    }
    let record = json!({
        "request_id": p.request_id, "action_id": action_id, "peer_node": p.peer_node,
        "fingerprint": p.fingerprint, "tier": tier_name(tier), "role": p.role,
        "advertise": p.advertise, "advertise_absent": p.advertise.is_none(), "projects": p.projects,
    });
    match &deps.chain {
        Some(cm) => {
            cm.append(CHAIN_SOURCE, kind, Some(record));
        }
        None => tracing::warn!(kind, "no chain on this node: pairing applied but not chained"),
    }
    tracing::info!(op = ?p.op, peer = %p.peer_node, tier = tier_name(tier), "mesh pairing updated from the dashboard");
    Ok(json!({ "op": p.op, "peer_node": p.peer_node, "fingerprint": p.fingerprint, "tier": tier_name(tier), "projects": p.projects }))
}

/// The `pair` [`ActionHandler`].
pub struct PairHandler {
    deps: Option<Arc<PairDeps>>,
}

impl PairHandler {
    /// Uses the dependencies [`init`] recorded (fails clearly until then).
    pub fn global() -> Self {
        Self { deps: None }
    }

    /// Explicit dependencies (tests, embedding).
    pub fn with_deps(deps: PairDeps) -> Self {
        Self { deps: Some(Arc::new(deps)) }
    }
}

#[async_trait]
impl ActionHandler for PairHandler {
    fn kind(&self) -> &str {
        KIND
    }

    async fn handle(&self, action: &Action) -> ActionOutcome {
        let Some(deps) = self.deps.clone().or_else(deps) else {
            return ActionOutcome::failed("pairing is not initialised on this node (no placement control plane)", KIND);
        };
        let (id, payload) = (action.id.clone(), action.payload.clone());
        let out = tokio::task::spawn_blocking(move || apply(&deps, &id, &payload)).await;
        match out {
            Ok(Ok(result)) => ActionOutcome { status: "succeeded", result },
            Ok(Err(e)) => ActionOutcome::failed(e, KIND),
            Err(e) => ActionOutcome::failed(format!("pair handler panicked: {e}"), KIND),
        }
    }
}

#[cfg(test)]
#[path = "mesh_pair_tests.rs"]
mod tests;
