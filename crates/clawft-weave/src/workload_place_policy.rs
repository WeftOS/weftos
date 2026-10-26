//! Operator policy files for the daemon's placement control plane and
//! `workload-host` (ADR-099 sections 3, 4 and 7; card mesh-placement-12).
//! Every file lives in the daemon's runtime directory, is size-capped and
//! validated here, at the boundary, and is refused unless it is owned by the
//! daemon's user (or root) and not group- or world-writable:
//!
//! - `workload-permits.json`: `[WorkloadPermitRule, ...]`; missing means no
//!   permits (default deny). The catalog verbs (`workload install` /
//!   `unload`) decide as the principal `catalog` on `unsigned` packages, so a
//!   permit for them reads `{"id": "catalog", "actions": ["workload.install",
//!   "workload.unload"], "kinds": ["cog"], "min_package_trust": "unsigned",
//!   "principals": ["catalog"]}`; a permit that accepts unsigned packages and
//!   names no principals is refused when the file is read;
//! - `workload-trust.json`: package signers; missing means the compiled-in
//!   WeftOS signer set only;
//! - `workload-peers.json`: `[{"addr": "host:port", "tier": "paired",
//!   "key": "<64 hex>"}]`. The tier is given to the node `key` names, or,
//!   without a key, to the key first seen at the address; a different key
//!   answering there later never inherits it. `addr` may be left out when
//!   `key` is given: the peer is trusted (project fetch) but never dialed;
//! - `workload-container.json`: serve a container adapter
//!   (`{"engine": "docker", "base_image": "name@sha256:<64 hex>",
//!   "arches_native": ["aarch64"], "arches_emulated": [],
//!   "allow_emulated": false}`); missing means native only, and the
//!   controller never offers this node a container variant;
//! - `workload-seeds.json`: Cognitum Seeds on operator-assigned node ids
//!   (`[{"node_id": "seed-kitchen", "url": "https://seed:8443",
//!   "tls_spki_sha256": "spki-sha256:<64 hex>" (or the older "tls_sha256"),
//!   "tier": "paired", (a Seed with no pin is refused unless
//!   "allow_unpinned_lab_link": true, for a USB or lab link only),
//!   "pins": [{"id": "fall-detect", "version": "1.0.0"}]}]`); tokens are
//!   read from `secrets/workload.seed/<node_id>.token` under the runtime dir.

use std::path::Path;
use std::sync::Arc;

use clawft_kernel::chain::ChainManager;
use clawft_kernel::workload_ctl::governance_tier;
use clawft_kernel::workload_ctl::{MEM_SCHEME, OperatorPeer};
use clawft_kernel::workload_governance::{NodeTrustTier, WorkloadGate, WorkloadPermitRule};
use clawft_kernel::workload_pkg::TrustAnchors;
use clawft_kernel::workload_pkg::codec::hex_decode_exact;
use clawft_kernel::workload_runtime::seed_creds::SEED_SECRETS_SUBDIR;
use clawft_kernel::workload_runtime::seed_tls::SeedTls;
use clawft_kernel::workload_runtime::{
    ContainerRuntime, ContainerRuntimeConfig, Engine, FileCredentials, HttpSeedTransport,
    SeedApiRuntime, SeedConfig, SeedPin, SystemRunner, WorkloadHost,
};
use clawft_types::placement::TrustTier;
use serde::Deserialize;

/// Permit file under the runtime dir.
pub const PERMITS_FILE: &str = "workload-permits.json";
/// Trust file under the runtime dir.
pub const TRUST_FILE: &str = "workload-trust.json";
/// Peers file under the runtime dir.
pub const PEERS_FILE: &str = "workload-peers.json";
/// Container adapter file under the runtime dir.
pub const CONTAINER_FILE: &str = "workload-container.json";
/// Seed file under the runtime dir.
pub const SEEDS_FILE: &str = "workload-seeds.json";
/// Controller state (targets, placements) under the runtime dir.
pub const STATE_FILE: &str = "workload-placements.json";
const MAX_POLICY_BYTES: u64 = 256 * 1024;
const MAX_PEERS: usize = 256;
const MAX_SEEDS: usize = 64;

/// Refuse a policy file someone other than its owner (or root, or the
/// daemon's user) could have rewritten: a group- or world-writable file, or
/// one owned by another user. Permits and trust decide what runs, so the
/// file is as sensitive as a key.
fn check_policy_perms(path: &Path, m: &std::fs::Metadata) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    if m.mode() & 0o022 != 0 {
        return Err(format!(
            "{} is group- or world-writable (mode {:o}); chmod 600 it",
            path.display(),
            m.mode() & 0o7777
        ));
    }
    let me = nix::unistd::geteuid().as_raw();
    if m.uid() != me && m.uid() != 0 {
        return Err(format!(
            "{} is owned by another user (uid {}); it must be owned by the daemon's user",
            path.display(),
            m.uid()
        ));
    }
    Ok(())
}

fn read_policy(path: &Path) -> Result<Option<String>, String> {
    let Ok(m) = std::fs::metadata(path) else {
        return Ok(None);
    };
    check_policy_perms(path, &m)?;
    if m.len() > MAX_POLICY_BYTES {
        return Err(format!("{} is too large", path.display()));
    }
    std::fs::read_to_string(path)
        .map(Some)
        .map_err(|e| e.to_string())
}

pub(crate) fn parse<T: serde::de::DeserializeOwned>(dir: &Path, file: &str) -> Result<Option<T>, String> {
    match read_policy(&dir.join(file))? {
        None => Ok(None),
        Some(t) => serde_json::from_str(&t)
            .map(Some)
            .map_err(|e| format!("{file}: {e}")),
    }
}

/// Permits from the runtime dir (none if the file is absent).
pub fn load_permits(dir: &Path) -> Result<Vec<WorkloadPermitRule>, String> {
    let permits: Vec<WorkloadPermitRule> = parse(dir, PERMITS_FILE)?.unwrap_or_default();
    for w in permits.iter().flat_map(WorkloadPermitRule::warnings) {
        tracing::warn!("{PERMITS_FILE}: {w}");
    }
    Ok(permits)
}

/// Trust anchors from the runtime dir (WeftOS defaults if absent).
pub fn load_anchors(dir: &Path) -> Result<TrustAnchors, String> {
    match read_policy(&dir.join(TRUST_FILE))? {
        None => TrustAnchors::weftos_default(),
        Some(t) => TrustAnchors::from_trust_json(t.as_bytes()),
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct PeerEntry {
    /// Absent for a peer that has no address to dial (a paired member whose
    /// mesh listens on loopback): it is trusted by key, never dialed.
    #[serde(default)]
    addr: Option<String>,
    #[serde(default = "paired")]
    tier: TrustTier,
    /// The node key (64 hex) this tier is for.
    #[serde(default)]
    key: Option<String>,
}

fn paired() -> TrustTier {
    TrustTier::Paired
}

fn host_port(a: &str) -> bool {
    !a.is_empty()
        && a.len() <= 260
        && a.contains(':')
        && !a.starts_with(MEM_SCHEME)
        && a.chars()
            .all(|c| c.is_ascii_alphanumeric() || ".-:[]_".contains(c))
}

fn read_entries(dir: &Path) -> Result<Vec<PeerEntry>, String> {
    let peers: Vec<PeerEntry> = parse(dir, PEERS_FILE)?.unwrap_or_default();
    if peers.len() > MAX_PEERS {
        return Err(format!("{PEERS_FILE}: more than {MAX_PEERS} peers"));
    }
    for p in &peers {
        if let Some(a) = p.addr.as_deref().filter(|a| !host_port(a)) {
            return Err(format!("{PEERS_FILE}: {a:?} is not host:port"));
        }
        if p.addr.is_none() && p.key.is_none() {
            return Err(format!("{PEERS_FILE}: an entry needs an addr or a key"));
        }
    }
    Ok(peers)
}

fn entry_key(p: &PeerEntry) -> Result<Option<[u8; 32]>, String> {
    p.key.as_deref().map_or(Ok(None), |k| {
        hex_decode_exact::<32>(k)
            .map(Some)
            .ok_or_else(|| format!("{PEERS_FILE}: key for {:?} is not 64 hex", p.addr.as_deref().unwrap_or("(no addr)")))
    })
}

/// Operator peers from the runtime dir (none if the file is absent). An
/// entry without an address is trusted by key only ([`load_peer_keys`]) and
/// is skipped here: nothing may try to dial it.
pub fn load_peers(dir: &Path) -> Result<Vec<OperatorPeer>, String> {
    let mut out = Vec::new();
    for p in read_entries(dir)? {
        let key = entry_key(&p)?;
        let Some(addr) = p.addr else { continue };
        let peer = OperatorPeer::new(addr, p.tier);
        out.push(match key {
            Some(k) => peer.with_key(k),
            None => peer,
        });
    }
    Ok(out)
}

/// `(tier, key)` of every keyed entry, dialable or not (project fetch
/// authorises callers by key).
pub fn load_peer_keys(dir: &Path) -> Result<Vec<(TrustTier, [u8; 32])>, String> {
    let mut out = Vec::new();
    for p in read_entries(dir)? {
        if let Some(k) = entry_key(&p)? {
            out.push((p.tier, k));
        }
    }
    Ok(out)
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ContainerFile {
    engine: String,
    base_image: String,
    #[serde(default = "aarch64")]
    arches_native: Vec<String>,
    #[serde(default)]
    arches_emulated: Vec<String>,
    #[serde(default)]
    allow_emulated: bool,
}

fn aarch64() -> Vec<String> {
    vec!["aarch64".into()]
}

const ARCHES: &[&str] = &["aarch64", "armv7", "x86_64"];

/// The operator's container adapter config (none if the file is absent).
pub fn load_container(
    dir: &Path,
    work_root: &Path,
) -> Result<Option<ContainerRuntimeConfig>, String> {
    let Some(f) = parse::<ContainerFile>(dir, CONTAINER_FILE)? else {
        return Ok(None);
    };
    let engine = match f.engine.as_str() {
        "docker" => Engine::Docker,
        "podman" => Engine::Podman,
        "apple" => Engine::Apple,
        other => return Err(format!("{CONTAINER_FILE}: unknown engine {other:?}")),
    };
    clawft_kernel::workload_runtime::container_cmd::validate_base_image(&f.base_image)
        .map_err(|e| format!("{CONTAINER_FILE}: {e}"))?;
    let known = |v: &[String]| v.iter().all(|a| ARCHES.contains(&a.as_str()));
    if f.arches_native.is_empty() || !known(&f.arches_native) || !known(&f.arches_emulated) {
        return Err(format!("{CONTAINER_FILE}: arches must be from {ARCHES:?}"));
    }
    let mut cfg = ContainerRuntimeConfig::new(engine, f.base_image, work_root);
    cfg.arches_native = f.arches_native;
    cfg.arches_emulated = f.arches_emulated;
    cfg.allow_emulated = f.allow_emulated;
    Ok(Some(cfg))
}

/// The container adapter under this node's governance, and the routes it
/// serves (`container`, plus `emulated` when the operator opted in).
pub fn container_host(
    cfg: ContainerRuntimeConfig,
    gate: Arc<WorkloadGate>,
    node_id: &str,
    chain: &Arc<ChainManager>,
) -> (Arc<WorkloadHost>, Vec<&'static str>) {
    let mut routes = vec!["container"];
    if cfg.allow_emulated && !cfg.arches_emulated.is_empty() {
        routes.push("emulated");
    }
    let rt = ContainerRuntime::new(cfg, Arc::new(SystemRunner));
    let host = WorkloadHost::new(Arc::new(rt), gate, node_id, NodeTrustTier::Pinned)
        .with_chain(chain.clone());
    (Arc::new(host), routes)
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct SeedEntry {
    node_id: String,
    url: String,
    #[serde(default)]
    tls_sha256: Option<String>,
    /// `spki-sha256:<hex>`: pin the Seed's public key (survives the
    /// Seed's certificate renewals). Preferred over `tls_sha256`.
    #[serde(default)]
    tls_spki_sha256: Option<String>,
    /// A USB or lab link that cannot be pinned (plain http). Off by
    /// default: a Seed without a pin cannot be bound or placed on.
    #[serde(default)]
    allow_unpinned_lab_link: bool,
    #[serde(default = "paired")]
    tier: TrustTier,
    pins: Vec<SeedPin>,
}

/// One operator-assigned Seed, ready for `PlacementControlPlane::add_seed`.
pub struct SeedTarget {
    /// Operator-assigned node id.
    pub node_id: String,
    /// Operator-assigned tier.
    pub tier: TrustTier,
    /// Card 09's `remote.api` adapter under this node's governance.
    pub host: Arc<WorkloadHost>,
}

fn seed_runtime(
    s: SeedEntry,
    creds: Arc<FileCredentials>,
) -> Result<(String, TrustTier, SeedApiRuntime), String> {
    let tls = match (&s.tls_spki_sha256, &s.tls_sha256) {
        (Some(_), Some(_)) => {
            return Err(format!(
                "{SEEDS_FILE}: {}: give tls_spki_sha256 or tls_sha256, not both",
                s.node_id
            ));
        }
        (Some(f), None) => SeedTls::pinned_spki(f).map_err(|e| format!("{SEEDS_FILE}: {e}"))?,
        (None, Some(f)) => SeedTls::pinned(f).map_err(|e| format!("{SEEDS_FILE}: {e}"))?,
        (None, None) => SeedTls::WebPki,
    };
    let mut transport = HttpSeedTransport::new(&s.url, tls)
        .map_err(|e| format!("{SEEDS_FILE}: {}: {e}", s.node_id))?;
    if s.allow_unpinned_lab_link {
        transport = transport.allow_unpinned_lab_link();
    }
    let rt = SeedApiRuntime::new(
        SeedConfig {
            node_id: s.node_id.clone(),
            pins: s.pins,
            concurrency_cap: clawft_kernel::workload_runtime::seed::SEED_CONCURRENCY_CAP,
        },
        Arc::new(transport),
        creds,
    )
    .map_err(|e| format!("{SEEDS_FILE}: {}: {e}", s.node_id))?;
    Ok((s.node_id, s.tier, rt))
}

/// The adapter runtime of one Seed named in `workload-seeds.json`, for
/// `workload.node.bind` (which needs the runtime itself, not a host around it).
pub fn load_seed_runtime(dir: &Path, node_id: &str) -> Result<SeedApiRuntime, String> {
    let seeds: Vec<SeedEntry> = parse(dir, SEEDS_FILE)?.unwrap_or_default();
    let entry = seeds
        .into_iter()
        .find(|s| s.node_id == node_id)
        .ok_or_else(|| format!("{SEEDS_FILE}: no Seed with node id {node_id:?}"))?;
    let creds = Arc::new(FileCredentials::new(dir.join(SEED_SECRETS_SUBDIR)));
    seed_runtime(entry, creds).map(|(_, _, rt)| rt)
}

/// Seeds from the runtime dir (none if the file is absent).
pub fn load_seeds(
    dir: &Path,
    gate: Arc<WorkloadGate>,
    chain: &Arc<ChainManager>,
) -> Result<Vec<SeedTarget>, String> {
    let seeds: Vec<SeedEntry> = parse(dir, SEEDS_FILE)?.unwrap_or_default();
    if seeds.len() > MAX_SEEDS {
        return Err(format!("{SEEDS_FILE}: more than {MAX_SEEDS} seeds"));
    }
    let creds = Arc::new(FileCredentials::new(dir.join(SEED_SECRETS_SUBDIR)));
    seeds
        .into_iter()
        .map(|s| {
            let (node_id, tier, rt) = seed_runtime(s, creds.clone())?;
            let host = WorkloadHost::new(
                Arc::new(rt),
                gate.clone(),
                node_id.clone(),
                governance_tier(tier),
            )
            .with_chain(chain.clone());
            Ok(SeedTarget { node_id, tier, host: Arc::new(host) })
        })
        .collect()
}
