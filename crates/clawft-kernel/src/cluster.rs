//! Cluster membership and node fabric.
//!
//! Defines types for multi-node WeftOS clusters where agents
//! can migrate between nodes. Each node is a WeftOS kernel
//! instance -- native binary on cloud/edge, or WASM in a browser.
//!
//! # Feature Gate
//!
//! All types compile unconditionally. Actual peer discovery,
//! health monitoring, and cross-node communication require the
//! `cluster` feature flag and a distributed networking layer.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
#[cfg(any(feature = "exochain", all(feature = "mesh", feature = "native")))]
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

/// Unique node identifier (UUID or DID string).
pub type NodeId = String;

/// Node platform type.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodePlatform {
    /// Native binary on a cloud VM or bare metal server.
    CloudNative,
    /// Native binary on an edge device.
    Edge,
    /// WASM module running in a browser tab.
    Browser,
    /// WASI module in a container.
    Wasi,
    /// Custom platform label.
    Custom(String),
}

impl std::fmt::Display for NodePlatform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NodePlatform::CloudNative => write!(f, "cloud-native"),
            NodePlatform::Edge => write!(f, "edge"),
            NodePlatform::Browser => write!(f, "browser"),
            NodePlatform::Wasi => write!(f, "wasi"),
            NodePlatform::Custom(name) => write!(f, "custom({name})"),
        }
    }
}

/// How long an `Unverified` mesh peer may go unheard before membership drops
/// it (see [`ClusterMembership::expire_unverified`]).
pub const UNVERIFIED_TTL: std::time::Duration = std::time::Duration::from_secs(3600);
/// How often the mesh listeners look for expired `Unverified` peers.
pub const UNVERIFIED_REAP_EVERY: std::time::Duration = std::time::Duration::from_secs(60);

/// How long a peer may go without a heartbeat before it is marked
/// `Unreachable` (see [`ClusterMembership::expire_stale`]).
pub const STALE_UNREACHABLE_AFTER: std::time::Duration = std::time::Duration::from_secs(600);
/// How long a peer may go without a heartbeat before membership removes it
/// (and before a persisted entry is dropped on load). A real mesh member that
/// comes back re-joins through the mesh peer-event path.
pub const STALE_REMOVE_AFTER: std::time::Duration = std::time::Duration::from_secs(24 * 3600);

/// Node health state.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeState {
    /// Node is joining the cluster.
    Joining,
    /// Node is healthy and active.
    Active,
    /// Node is suspected unreachable (missed heartbeats).
    Suspect,
    /// Node has been confirmed unreachable.
    Unreachable,
    /// Node is gracefully leaving the cluster.
    Leaving,
    /// Node has left the cluster.
    Left,
    /// A connected mesh peer whose node id admission did not verify (every
    /// peer under `off` / `observe`, and dialled seeds): its routes work, but
    /// the id is only a claim, so it is not a member. Only a verified join
    /// makes it `Active`; heartbeats and health events never do.
    Unverified,
}

impl std::fmt::Display for NodeState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NodeState::Joining => write!(f, "joining"),
            NodeState::Active => write!(f, "active"),
            NodeState::Suspect => write!(f, "suspect"),
            NodeState::Unreachable => write!(f, "unreachable"),
            NodeState::Leaving => write!(f, "leaving"),
            NodeState::Left => write!(f, "left"),
            NodeState::Unverified => write!(f, "unverified"),
        }
    }
}

/// Information about a peer node in the cluster.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerNode {
    /// Unique node identifier.
    pub id: NodeId,

    /// Human-readable node name.
    pub name: String,

    /// Node platform.
    pub platform: NodePlatform,

    /// Current state in the cluster.
    pub state: NodeState,

    /// Network address for direct communication.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,

    /// When this node was first seen.
    pub first_seen: DateTime<Utc>,

    /// When the last heartbeat was received.
    pub last_heartbeat: DateTime<Utc>,

    /// Capabilities this node advertises.
    #[serde(default)]
    pub capabilities: Vec<String>,

    /// Labels for scheduling and filtering.
    #[serde(default)]
    pub labels: HashMap<String, String>,
}

/// Cluster configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterConfig {
    /// This node's identifier.
    pub node_id: NodeId,

    /// This node's display name.
    #[serde(default = "default_node_name")]
    pub node_name: String,

    /// This node's platform type.
    #[serde(default = "default_platform")]
    pub platform: NodePlatform,

    /// Heartbeat interval in seconds.
    #[serde(default = "default_heartbeat_interval")]
    pub heartbeat_interval_secs: u64,

    /// How many missed heartbeats before marking a node suspect.
    #[serde(default = "default_suspect_threshold")]
    pub suspect_threshold: u32,

    /// How many missed heartbeats before marking a node unreachable.
    #[serde(default = "default_unreachable_threshold")]
    pub unreachable_threshold: u32,

    /// Maximum cluster size (0 = unlimited).
    #[serde(default)]
    pub max_nodes: u32,

    /// Address to bind the mesh listener (e.g., "0.0.0.0:9489").
    #[serde(default)]
    pub bind_address: Option<String>,

    /// Seed peers for bootstrap discovery.
    #[serde(default)]
    pub seed_peers: Vec<String>,

    /// Path to Ed25519 identity key file.
    #[serde(default)]
    pub identity_key_path: Option<std::path::PathBuf>,

    /// Max `add_peer` calls allowed per source within
    /// [`Self::peer_add_window_ms`] (WEFT-148).
    ///
    /// `0` disables peer-add rate limiting. Default: 1.
    #[serde(default = "default_peer_add_max_per_source")]
    pub peer_add_max_per_source: u32,

    /// Sliding window for peer-add rate limiting, in milliseconds.
    /// Default: 100.
    #[serde(default = "default_peer_add_window_ms")]
    pub peer_add_window_ms: u64,
}

fn default_node_name() -> String {
    "local".into()
}

fn default_platform() -> NodePlatform {
    NodePlatform::CloudNative
}

fn default_heartbeat_interval() -> u64 {
    5
}

fn default_suspect_threshold() -> u32 {
    3
}

fn default_unreachable_threshold() -> u32 {
    10
}

fn default_peer_add_max_per_source() -> u32 {
    crate::rate_limit::DEFAULT_PEER_ADD_RATE.max_per_window
}

fn default_peer_add_window_ms() -> u64 {
    crate::rate_limit::DEFAULT_PEER_ADD_RATE.window.as_millis() as u64
}

impl Default for ClusterConfig {
    fn default() -> Self {
        Self {
            node_id: uuid::Uuid::new_v4().to_string(),
            node_name: default_node_name(),
            platform: default_platform(),
            heartbeat_interval_secs: default_heartbeat_interval(),
            suspect_threshold: default_suspect_threshold(),
            unreachable_threshold: default_unreachable_threshold(),
            max_nodes: 0,
            bind_address: None,
            seed_peers: Vec::new(),
            identity_key_path: None,
            peer_add_max_per_source: default_peer_add_max_per_source(),
            peer_add_window_ms: default_peer_add_window_ms(),
        }
    }
}

impl ClusterConfig {
    /// Build a [`crate::rate_limit::RateLimitConfig`] from the peer-add
    /// rate-limit fields (WEFT-148).
    pub fn peer_add_rate_config(&self) -> crate::rate_limit::RateLimitConfig {
        crate::rate_limit::RateLimitConfig {
            max_per_window: self.peer_add_max_per_source,
            window: Duration::from_millis(self.peer_add_window_ms),
        }
    }

    /// Recommend the heartbeat / gossip interval (seconds), consulting
    /// an optional learned
    /// [`GossipTimingModel`](crate::eml_kernel::GossipTimingModel).
    ///
    /// When `model` is `None` or untrained, returns
    /// `self.heartbeat_interval_secs` unchanged (current behaviour).
    /// When trained, the model's per-context recommendation is used,
    /// rounded to whole seconds and clamped to the model's [1, 60]
    /// range.
    ///
    /// `peer_count` and `network_latency_ms` and `update_frequency_hz`
    /// feed the model when trained; pass `(0, 0.0, 0.0)` if not
    /// available.
    ///
    /// Without the `ecc` feature this always returns
    /// `self.heartbeat_interval_secs` (WEFT-114).
    ///
    /// NOTE(eml-swap): wired — Finding #5 (GossipTimingModel).
    #[cfg(feature = "ecc")]
    pub fn recommended_heartbeat_secs(
        &self,
        model: Option<&crate::eml_kernel::GossipTimingModel>,
        peer_count: usize,
        network_latency_ms: f64,
        update_frequency_hz: f64,
    ) -> u64 {
        match model {
            Some(m) if m.is_trained() => {
                let secs = m.predict(peer_count, network_latency_ms, update_frequency_hz);
                secs.round().max(1.0) as u64
            }
            _ => self.heartbeat_interval_secs,
        }
    }

    /// Recommend heartbeat interval without EML (no `ecc` feature).
    #[cfg(not(feature = "ecc"))]
    pub fn recommended_heartbeat_secs(&self) -> u64 {
        self.heartbeat_interval_secs
    }
}

// ── ECC capability advertisement (K3c) ────────────────────────────

/// ECC capabilities advertised by a cluster node.
///
/// Populated during boot-time calibration and advertised to peers
/// so they can route ECC-related requests to capable nodes.
#[cfg(feature = "ecc")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeEccCapability {
    /// Calibrated cognitive tick interval (milliseconds).
    pub tick_interval_ms: u32,
    /// 95th percentile compute time per tick (microseconds).
    pub compute_p95_us: u32,
    /// Headroom ratio (actual_compute / budget).
    pub headroom_ratio: f32,
    /// Number of vectors in the HNSW index.
    pub hnsw_vector_count: u32,
    /// Number of edges in the causal graph.
    pub causal_edge_count: u32,
    /// Whether this node can perform spectral analysis.
    pub spectral_capable: bool,
    /// Unix timestamp when calibration was performed.
    pub calibrated_at: u64,
}

// ── Node identity (K6 mesh networking) ─────────────────────────────

/// Node identity derived from Ed25519 keypair.
///
/// The `node_id` is derived as `hex(SHA-256(pubkey)[0..16])` (ADR-103 D11),
/// providing a stable, compact identifier tied to the cryptographic key.
#[cfg(any(feature = "mesh", feature = "exochain"))]
pub struct NodeIdentity {
    /// Ed25519 signing key (private).
    keypair: ed25519_dalek::SigningKey,
    /// Derived node identifier.
    node_id: String,
}

#[cfg(any(feature = "mesh", feature = "exochain"))]
impl NodeIdentity {
    /// Generate a new random identity.
    pub fn generate() -> Self {
        let mut csprng = rand::thread_rng();
        Self::from_signing_key(ed25519_dalek::SigningKey::generate(&mut csprng))
    }

    /// Build an identity from an existing Ed25519 signing key (for
    /// example the persisted `<runtime>/node.key`). The node id comes
    /// from [`crate::node_id::node_id_from_pubkey`], the one derivation.
    pub fn from_signing_key(keypair: ed25519_dalek::SigningKey) -> Self {
        let node_id = crate::node_id::node_id_from_pubkey(&keypair.verifying_key().to_bytes());
        Self { keypair, node_id }
    }

    /// Get this node's identifier.
    pub fn node_id(&self) -> &str {
        &self.node_id
    }

    /// Get the public verification key.
    pub fn public_key(&self) -> ed25519_dalek::VerifyingKey {
        self.keypair.verifying_key()
    }

    /// Sign arbitrary data with this node's private key.
    pub fn sign(&self, data: &[u8]) -> ed25519_dalek::Signature {
        use ed25519_dalek::Signer;
        self.keypair.sign(data)
    }
}

/// Cluster membership errors.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum ClusterError {
    /// Node already in the cluster.
    #[error("node already exists: '{node_id}'")]
    NodeAlreadyExists {
        /// Node ID.
        node_id: NodeId,
    },

    /// Node not found.
    #[error("node not found: '{node_id}'")]
    NodeNotFound {
        /// Node ID.
        node_id: NodeId,
    },

    /// Cluster is at maximum capacity.
    #[error("cluster full: max {max} nodes")]
    ClusterFull {
        /// Maximum node count.
        max: u32,
    },

    /// Mesh networking error.
    #[error("mesh error: {0}")]
    Mesh(String),

    /// Authentication failed during cluster join.
    #[error("authentication failed: {0}")]
    AuthFailed(String),

    /// Invalid state transition.
    #[error("invalid node state transition: {from} -> {to}")]
    InvalidTransition {
        /// Current state.
        from: String,
        /// Requested state.
        to: String,
    },

    /// Peer additions are too frequent (rate limited).
    #[error("rate limited: peer additions too frequent")]
    RateLimited,
}

/// Cluster membership tracker.
///
/// Tracks which nodes are part of the cluster, their health state,
/// and capabilities. Actual peer discovery and heartbeat monitoring
/// require a networking layer not included here.
/// Known valid capabilities for cluster nodes.
const KNOWN_CAPABILITIES: &[&str] = &[
    "ipc",
    "chain",
    "tree",
    "governance",
    "ecc",
    "wasm",
    "containers",
    "apps",
    "mesh",
    "discovery",
    "heartbeat",
    "compute",
];

/// Validate peer capabilities against the known set.
/// Returns unknown capabilities (if any).
fn validate_capabilities(capabilities: &[String]) -> Vec<String> {
    capabilities
        .iter()
        .filter(|c| !KNOWN_CAPABILITIES.contains(&c.as_str()))
        .cloned()
        .collect()
}

/// On-disk format for persisted cluster peers.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ClusterPeersFile {
    /// Schema version for forward compatibility.
    #[serde(default = "default_peers_file_version")]
    pub version: u32,
    /// Persisted peer records.
    #[serde(default)]
    pub peers: Vec<PeerNode>,
}

fn default_peers_file_version() -> u32 {
    1
}

pub struct ClusterMembership {
    config: ClusterConfig,
    peers: DashMap<NodeId, PeerNode>,
    /// Per-source rate limiter for `add_peer` (WEFT-148).
    peer_add_limiter: crate::rate_limit::KeyedRateLimiter,
    /// Optional path for peer membership persistence.
    ///
    /// When set, mutations (add/remove/state-change) are reflected to disk
    /// so the cluster survives kernel restarts. A `Mutex` serialises writers;
    /// the lock is held only for the JSON encode + atomic rename.
    persist_path: std::sync::Mutex<Option<PathBuf>>,
    /// Optional chain manager for exochain audit logging.
    #[cfg(feature = "exochain")]
    chain: Option<Arc<crate::chain::ChainManager>>,
    /// Optional governance gate for policy enforcement.
    #[cfg(feature = "exochain")]
    gate: Option<Arc<crate::gate::GovernanceGate>>,
    /// Verified, signed node facts with TTL (ADR-099 section 2).
    #[cfg(all(feature = "native", any(feature = "mesh", feature = "exochain")))]
    facts: crate::node_facts::NodeFactsCache,
}

impl ClusterMembership {
    /// Create a new cluster membership tracker.
    ///
    /// Peer-add rate limiting is initialised from
    /// [`ClusterConfig::peer_add_rate_config`] (defaults: 1 per source
    /// per 100 ms).
    pub fn new(config: ClusterConfig) -> Self {
        let peer_add_limiter =
            crate::rate_limit::KeyedRateLimiter::new(config.peer_add_rate_config());
        Self {
            config,
            peers: DashMap::new(),
            peer_add_limiter,
            persist_path: std::sync::Mutex::new(None),
            #[cfg(feature = "exochain")]
            chain: None,
            #[cfg(feature = "exochain")]
            gate: None,
            #[cfg(all(feature = "native", any(feature = "mesh", feature = "exochain")))]
            facts: crate::node_facts::NodeFactsCache::new(),
        }
    }

    /// Verified node facts (local and peers) with TTL (card mesh-placement-03).
    #[cfg(all(feature = "native", any(feature = "mesh", feature = "exochain")))]
    pub fn facts(&self) -> &crate::node_facts::NodeFactsCache {
        &self.facts
    }

    /// Attach a chain manager for audit logging (builder style).
    #[cfg(feature = "exochain")]
    pub fn with_chain(mut self, cm: Arc<crate::chain::ChainManager>) -> Self {
        self.chain = Some(cm);
        self
    }

    /// Attach a governance gate for policy enforcement (builder style).
    #[cfg(feature = "exochain")]
    pub fn with_gate(mut self, gate: Arc<crate::gate::GovernanceGate>) -> Self {
        self.gate = Some(gate);
        self
    }

    /// Set the minimum interval between peer additions from the *same*
    /// source (builder style).
    ///
    /// `Duration::ZERO` disables rate limiting (used by unit tests).
    /// Non-zero intervals configure a per-source limit of 1 addition
    /// per `interval`. Prefer [`Self::with_peer_add_rate_limit`] for
    /// multi-count windows.
    pub fn with_min_peer_interval(self, interval: std::time::Duration) -> Self {
        if interval.is_zero() {
            self.with_peer_add_rate_limit(crate::rate_limit::RateLimitConfig::unlimited())
        } else {
            self.with_peer_add_rate_limit(crate::rate_limit::RateLimitConfig {
                max_per_window: 1,
                window: interval,
            })
        }
    }

    /// Configure the per-source `add_peer` rate limit (WEFT-148).
    pub fn with_peer_add_rate_limit(
        self,
        config: crate::rate_limit::RateLimitConfig,
    ) -> Self {
        self.peer_add_limiter.set_config(config);
        self
    }

    /// Current peer-add rate-limit configuration.
    pub fn peer_add_rate_limit(&self) -> crate::rate_limit::RateLimitConfig {
        self.peer_add_limiter.config()
    }

    /// Resolve the rate-limit source key for a peer.
    ///
    /// Prefers network `address` when present (so one host flooding many
    /// node IDs still hits the same bucket); falls back to peer `id`.
    pub fn peer_add_source_key(peer: &PeerNode) -> &str {
        match &peer.address {
            Some(addr) if !addr.is_empty() => addr.as_str(),
            _ => peer.id.as_str(),
        }
    }

    /// Enable peer persistence at the given path (builder style).
    ///
    /// If the file exists and is readable, peers are rehydrated into the
    /// in-memory map so joins survive kernel restarts. Parse errors log a
    /// warning and leave the map empty. Subsequent mutations write back to
    /// the same path via [`Self::persist`].
    pub fn with_persist_path(self, path: impl AsRef<Path>) -> Self {
        let path = path.as_ref().to_path_buf();
        let mut prune_file = false;
        if path.exists() {
            match std::fs::read_to_string(&path) {
                Ok(data) => match serde_json::from_str::<ClusterPeersFile>(&data) {
                    Ok(file) => {
                        let mut legacy = Vec::new();
                        let now = Utc::now();
                        let mut stale = 0usize;
                        for peer in file.peers {
                            // The local node is re-registered at every boot; a persisted
                            // copy is an earlier boot's (possibly ephemeral) id.
                            if peer.id == self.config.node_id {
                                continue;
                            }
                            // Persisted junk: no heartbeat within the removal TTL.
                            if Self::is_stale_at(&peer, now, STALE_REMOVE_AFTER) {
                                stale += 1;
                                continue;
                            }
                            // Pre-0.8 daemons keyed peers by UUID; node ids are now
                            // 32-hex from the node key, so a UUID entry can never match a
                            // live peer again (phase-0 review R8).
                            if is_legacy_uuid_id(&peer.id) {
                                legacy.push(peer.id);
                                continue;
                            }
                            // Bypass rate-limiting and duplicate checks during rehydration.
                            self.peers.insert(peer.id.clone(), peer);
                        }
                        info!(
                            count = self.peers.len(),
                            path = %path.display(),
                            "rehydrated cluster peers from disk"
                        );
                        if stale > 0 {
                            warn!(
                                dropped = stale,
                                path = %path.display(),
                                "dropped stale peers from the cluster peers file"
                            );
                            prune_file = true;
                        }
                        if !legacy.is_empty() {
                            warn!(
                                dropped = legacy.len(),
                                ids = ?legacy,
                                path = %path.display(),
                                "dropped legacy UUID peer ids from the cluster peers file"
                            );
                            prune_file = true;
                        }
                    }
                    Err(e) => {
                        warn!(
                            error = %e,
                            path = %path.display(),
                            "failed to parse cluster peers file; starting empty"
                        );
                    }
                },
                Err(e) => {
                    warn!(
                        error = %e,
                        path = %path.display(),
                        "failed to read cluster peers file; starting empty"
                    );
                }
            }
        }
        *self
            .persist_path
            .lock()
            .expect("persist_path lock poisoned") = Some(path);
        if prune_file {
            // Write the cleaned list back so the legacy ids are gone from disk too.
            self.persist();
        }
        self
    }

    /// Snapshot current peers to disk (no-op when persistence is disabled).
    ///
    /// Writes to a sibling `.tmp` file then renames for atomic replacement.
    /// Errors are logged but not propagated — membership stays correct
    /// in memory, and the next successful mutation will re-sync the file.
    fn persist(&self) {
        let path_opt = self
            .persist_path
            .lock()
            .expect("persist_path lock poisoned")
            .clone();
        let Some(path) = path_opt else {
            return;
        };

        // The local node is re-registered at boot, never persisted: otherwise every
        // boot with a fresh node id would leave a permanent entry behind.
        let peers: Vec<PeerNode> = self
            .peers
            .iter()
            .filter(|e| e.key() != &self.config.node_id)
            .map(|e| e.value().clone())
            .collect();
        let file = ClusterPeersFile {
            version: default_peers_file_version(),
            peers,
        };
        let json = match serde_json::to_string_pretty(&file) {
            Ok(j) => j,
            Err(e) => {
                warn!(error = %e, "failed to serialise cluster peers");
                return;
            }
        };

        if let Some(parent) = path.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            warn!(error = %e, path = %parent.display(), "failed to create persist parent");
            return;
        }

        let tmp = path.with_extension("json.tmp");
        if let Err(e) = std::fs::write(&tmp, &json) {
            warn!(error = %e, path = %tmp.display(), "failed to write cluster peers tmp");
            return;
        }
        if let Err(e) = std::fs::rename(&tmp, &path) {
            warn!(error = %e, path = %path.display(), "failed to rename cluster peers file");
            let _ = std::fs::remove_file(&tmp);
        }
    }

    /// Get the cluster configuration.
    pub fn config(&self) -> &ClusterConfig {
        &self.config
    }

    /// Get this node's ID.
    pub fn local_node_id(&self) -> &str {
        &self.config.node_id
    }

    /// Register a peer node.
    ///
    /// Rate-limited **per source** (WEFT-148): by default at most one
    /// addition per source per 100 ms. Source key is the peer's network
    /// address when set, otherwise its node id. Thresholds are
    /// configurable via [`ClusterConfig`] /
    /// [`Self::with_peer_add_rate_limit`].
    pub fn add_peer(&self, peer: PeerNode) -> Result<(), ClusterError> {
        // Governance gate: check policy before allowing peer addition.
        #[cfg(feature = "exochain")]
        if let Some(ref gate) = self.gate {
            use crate::gate::GateBackend;
            let decision = gate.check(
                &self.config.node_id,
                "cluster.peer.add",
                &serde_json::json!({
                    "peer_id": peer.id,
                    "peer_name": peer.name,
                    "platform": peer.platform.to_string(),
                    "effect": { "risk": 0.4, "security": 0.3 },
                }),
            );
            if decision.is_deny() {
                return Err(ClusterError::AuthFailed(
                    "governance denied peer addition".into(),
                ));
            }
        }

        // Rate-limit peer additions per source (WEFT-148).
        let source = Self::peer_add_source_key(&peer);
        if !self.peer_add_limiter.check(source) {
            return Err(ClusterError::RateLimited);
        }

        if self.peers.contains_key(&peer.id) {
            return Err(ClusterError::NodeAlreadyExists { node_id: peer.id });
        }

        if self.config.max_nodes > 0 && self.peers.len() as u32 >= self.config.max_nodes {
            return Err(ClusterError::ClusterFull {
                max: self.config.max_nodes,
            });
        }

        // Validate capabilities -- warn on unknown but still allow (forward compat).
        let unknown = validate_capabilities(&peer.capabilities);
        if !unknown.is_empty() {
            warn!(
                node_id = %peer.id,
                unknown_capabilities = ?unknown,
                "peer advertises unknown capabilities"
            );
        }

        // Chain logging: record peer addition.
        #[cfg(feature = "exochain")]
        if let Some(ref cm) = self.chain {
            cm.append(
                "cluster",
                crate::chain::EVENT_KIND_CLUSTER_PEER_ADD,
                Some(serde_json::json!({
                    "node_id": peer.id,
                    "name": peer.name,
                    "platform": peer.platform.to_string(),
                })),
            );
        }

        debug!(node_id = %peer.id, name = %peer.name, "adding peer to cluster");
        self.peers.insert(peer.id.clone(), peer);
        self.persist();
        Ok(())
    }

    /// Remove a peer node.
    pub fn remove_peer(&self, node_id: &str) -> Result<PeerNode, ClusterError> {
        // Governance gate: check policy before allowing peer removal.
        #[cfg(feature = "exochain")]
        if let Some(ref gate) = self.gate {
            use crate::gate::GateBackend;
            let decision = gate.check(
                &self.config.node_id,
                "cluster.peer.remove",
                &serde_json::json!({
                    "peer_id": node_id,
                    "effect": { "risk": 0.3, "security": 0.2 },
                }),
            );
            if decision.is_deny() {
                return Err(ClusterError::AuthFailed(
                    "governance denied peer removal".into(),
                ));
            }
        }

        let result = self
            .peers
            .remove(node_id)
            .map(|(_, peer)| peer)
            .ok_or_else(|| ClusterError::NodeNotFound {
                node_id: node_id.to_owned(),
            });

        if result.is_ok() {
            self.persist();
        }

        // Chain logging: record peer removal on success.
        #[cfg(feature = "exochain")]
        if let (Ok(peer), Some(cm)) = (&result, &self.chain) {
            cm.append(
                "cluster",
                crate::chain::EVENT_KIND_CLUSTER_PEER_REMOVE,
                Some(serde_json::json!({
                    "node_id": peer.id,
                    "name": peer.name,
                })),
            );
        }

        result
    }

    /// Update a peer's state.
    pub fn update_state(&self, node_id: &str, new_state: NodeState) -> Result<(), ClusterError> {
        let mut entry = self
            .peers
            .get_mut(node_id)
            .ok_or_else(|| ClusterError::NodeNotFound {
                node_id: node_id.to_owned(),
            })?;

        // Chain logging: record state transition.
        #[cfg(feature = "exochain")]
        if let Some(ref cm) = self.chain {
            cm.append(
                "cluster",
                crate::chain::EVENT_KIND_CLUSTER_PEER_STATE,
                Some(serde_json::json!({
                    "node_id": node_id,
                    "from": entry.state.to_string(),
                    "to": new_state.to_string(),
                })),
            );
        }

        entry.state = new_state;
        drop(entry);
        self.persist();
        Ok(())
    }

    /// Record a heartbeat from a peer.
    pub fn heartbeat(&self, node_id: &str) -> Result<(), ClusterError> {
        let mut entry = self
            .peers
            .get_mut(node_id)
            .ok_or_else(|| ClusterError::NodeNotFound {
                node_id: node_id.to_owned(),
            })?;
        entry.last_heartbeat = Utc::now();
        if entry.state == NodeState::Suspect {
            entry.state = NodeState::Active;
        }
        Ok(())
    }

    /// Get a snapshot of a peer's state.
    pub fn get_peer(&self, node_id: &str) -> Option<PeerNode> {
        self.peers.get(node_id).map(|e| e.value().clone())
    }

    /// List all peers with their states.
    pub fn list_peers(&self) -> Vec<(NodeId, NodeState, NodePlatform)> {
        self.peers
            .iter()
            .map(|e| (e.key().clone(), e.state.clone(), e.platform.clone()))
            .collect()
    }

    /// Count peers by state.
    pub fn count_by_state(&self, state: &NodeState) -> usize {
        self.peers.iter().filter(|e| &e.state == state).count()
    }

    /// Count total peers.
    pub fn len(&self) -> usize {
        self.peers.len()
    }

    /// Check if cluster has no peers.
    pub fn is_empty(&self) -> bool {
        self.peers.is_empty()
    }

    /// Add a peer and optionally create a resource tree node.
    #[cfg(feature = "exochain")]
    pub fn add_peer_with_tree(
        &self,
        peer: PeerNode,
        tree: &std::sync::Mutex<exo_resource_tree::ResourceTree>,
    ) -> Result<(), ClusterError> {
        let peer_name = peer.name.clone();
        self.add_peer(peer)?;

        // Create tree node for this peer
        let mut tree = tree.lock().unwrap();
        let peer_id = exo_resource_tree::ResourceId::new(format!("/network/peers/{peer_name}"));
        let parent = exo_resource_tree::ResourceId::new("/network/peers");
        if let Err(e) = tree.insert(peer_id, exo_resource_tree::ResourceKind::Device, parent) {
            tracing::debug!(peer = %peer_name, error = %e, "failed to create tree node for peer");
        }

        Ok(())
    }

    /// Register a peer node, checking the revocation list first.
    ///
    /// If the peer's ID appears in the revocation list, the add is
    /// rejected with [`ClusterError::AuthFailed`].
    pub fn add_peer_checked(
        &self,
        peer: PeerNode,
        revocation_list: &crate::revocation::RevocationList,
    ) -> Result<(), ClusterError> {
        if revocation_list.is_revoked(&peer.id) {
            return Err(ClusterError::AuthFailed(format!(
                "host '{}' is revoked",
                peer.id,
            )));
        }
        self.add_peer(peer)
    }

    /// Get all active peer node IDs.
    pub fn active_peers(&self) -> Vec<NodeId> {
        self.peers
            .iter()
            .filter(|e| e.state == NodeState::Active)
            .map(|e| e.key().clone())
            .collect()
    }

    /// Drop `Unverified` peers not heard from for `max_age`. They are not
    /// members, so nothing else ages them out (a peer that never sends a
    /// `Left`, or whose route vanished without an event, would linger).
    /// Returns how many were dropped.
    pub fn expire_unverified(&self, max_age: std::time::Duration) -> usize {
        let cutoff = Utc::now() - chrono::Duration::from_std(max_age).unwrap_or(chrono::Duration::MAX);
        let stale: Vec<NodeId> = self
            .peers
            .iter()
            .filter(|e| e.state == NodeState::Unverified && e.last_heartbeat < cutoff)
            .map(|e| e.key().clone())
            .collect();
        stale.iter().filter(|id| self.remove_peer(id).is_ok()).count()
    }

    fn is_stale_at(peer: &PeerNode, now: DateTime<Utc>, max_age: std::time::Duration) -> bool {
        let age = chrono::Duration::from_std(max_age).unwrap_or(chrono::Duration::MAX);
        now.signed_duration_since(peer.last_heartbeat) > age
    }

    /// Reap peers that stopped heartbeating: past `unreachable_after` a live peer
    /// becomes `Unreachable`; past `remove_after` any non-local peer is removed.
    /// The local node is never reaped. Returns the number of peers removed.
    pub fn expire_stale(
        &self,
        unreachable_after: std::time::Duration,
        remove_after: std::time::Duration,
    ) -> usize {
        self.expire_stale_at(Utc::now(), unreachable_after, remove_after)
    }

    /// [`Self::expire_stale`] against an explicit clock.
    pub fn expire_stale_at(
        &self,
        now: DateTime<Utc>,
        unreachable_after: std::time::Duration,
        remove_after: std::time::Duration,
    ) -> usize {
        let local = self.config.node_id.as_str();
        let mut remove = Vec::new();
        let mut demote = Vec::new();
        for e in self.peers.iter() {
            if e.key() == local {
                continue;
            }
            if Self::is_stale_at(&e, now, remove_after) {
                remove.push(e.key().clone());
            } else if Self::is_stale_at(&e, now, unreachable_after)
                && matches!(e.state, NodeState::Active | NodeState::Suspect | NodeState::Joining)
            {
                demote.push(e.key().clone());
            }
        }
        for id in &demote {
            let _ = self.update_state(id, NodeState::Unreachable);
        }
        remove.iter().filter(|id| self.remove_peer(id).is_ok()).count()
    }

    /// Apply a live mesh peer event to membership **atomically** (WEFT-120).
    ///
    /// Mesh is the authority for connect/health; this path upserts peers
    /// without the operator-facing `add_peer` rate limit (mesh already
    /// authenticated the session). A single event produces one consistent
    /// membership mutation + one persist.
    ///
    /// Returns `true` when membership changed.
    #[cfg(feature = "mesh")]
    pub fn apply_mesh_peer_event(
        &self,
        event: &crate::mesh_discovery::MeshPeerEvent,
    ) -> Result<bool, ClusterError> {
        use crate::mesh_discovery::MeshPeerEvent;
        match event {
            MeshPeerEvent::Joined {
                node_id,
                address,
                platform,
                verified,
            } => self.mesh_upsert(node_id, address.as_deref(), platform.as_deref(), *verified),
            MeshPeerEvent::Recovered {
                node_id,
                address,
                verified,
            } => self.mesh_upsert(node_id, address.as_deref(), None, *verified),
            MeshPeerEvent::Alive { node_id } => {
                // An unverified id never gets liveness credit.
                if self
                    .get_peer(node_id)
                    .is_some_and(|p| p.state == NodeState::Unverified)
                {
                    return Ok(false);
                }
                if self.peers.contains_key(node_id) {
                    let _ = self.heartbeat(node_id);
                    if self
                        .get_peer(node_id)
                        .map(|p| !matches!(p.state, NodeState::Active | NodeState::Unverified))
                        .unwrap_or(false)
                    {
                        let _ = self.update_state(node_id, NodeState::Active);
                        return Ok(true);
                    }
                    // heartbeat already updated last_heartbeat in-place
                    Ok(true)
                } else {
                    // Alive without prior join — treat as soft join. Nothing
                    // here says the id was verified.
                    self.mesh_upsert(node_id, None, None, false)
                }
            }
            // An unverified peer's health is not tracked here: a Suspect state
            // would be turned into Active by the next heartbeat.
            MeshPeerEvent::Suspect { node_id } => {
                if self.peers.get(node_id).is_some_and(|p| p.state != NodeState::Unverified) {
                    self.update_state(node_id, NodeState::Suspect)?;
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            MeshPeerEvent::Unreachable { node_id } => {
                match self.peers.get(node_id).map(|p| p.state.clone()) {
                    // Its route is gone and it was never a member: drop it,
                    // or it would sit in the list until a TTL.
                    Some(NodeState::Unverified) => {
                        let _ = self.remove_peer(node_id);
                        Ok(true)
                    }
                    Some(_) => {
                        self.update_state(node_id, NodeState::Unreachable)?;
                        Ok(true)
                    }
                    None => Ok(false),
                }
            }
            MeshPeerEvent::Left { node_id } => {
                if self.peers.contains_key(node_id) {
                    // Mark Left then drop so membership no longer lists the peer.
                    let _ = self.update_state(node_id, NodeState::Left);
                    let _ = self.remove_peer(node_id);
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
        }
    }

    /// Insert-or-activate a peer from mesh (bypasses rate limit).
    ///
    /// `verified`: admission verified the node id. A verified join makes the
    /// peer `Active`. An unverified one adds a new peer as `Unverified`, and
    /// on a peer already held only revives one that was down; it never
    /// demotes an `Active` peer (an operator-added or earlier verified one).
    #[cfg(feature = "mesh")]
    fn mesh_upsert(
        &self,
        node_id: &str,
        address: Option<&str>,
        platform: Option<&str>,
        verified: bool,
    ) -> Result<bool, ClusterError> {
        if let Some(mut entry) = self.peers.get_mut(node_id) {
            let mut changed = false;
            let to = match (verified, &entry.state) {
                (true, s) if *s != NodeState::Active => Some(NodeState::Active),
                (false, NodeState::Active | NodeState::Unverified) => None,
                (false, _) => Some(NodeState::Unverified),
                _ => None,
            };
            if let Some(to) = to {
                entry.state = to;
                changed = true;
            }
            // An unverified announce must not refresh a verified peer's
            // last-seen time or move its address (it could be spoofed); it
            // may only touch an entry that is itself unverified.
            let may_refresh = verified || entry.state == NodeState::Unverified;
            if may_refresh {
                entry.last_heartbeat = Utc::now();
            }
            if may_refresh
                && let Some(addr) = address
                && entry.address.as_deref() != Some(addr)
            {
                entry.address = Some(addr.to_owned());
                changed = true;
            }
            drop(entry);
            if changed {
                self.persist();
            }
            return Ok(true);
        }

        if self.config.max_nodes > 0 && self.peers.len() as u32 >= self.config.max_nodes {
            return Err(ClusterError::ClusterFull {
                max: self.config.max_nodes,
            });
        }

        let platform = match platform {
            Some("edge") => NodePlatform::Edge,
            Some("browser") => NodePlatform::Browser,
            Some("wasi") => NodePlatform::Wasi,
            Some(other) if other.starts_with("custom:") => {
                NodePlatform::Custom(other.trim_start_matches("custom:").to_owned())
            }
            Some(other) if other != "cloud-native" && other != "linux" => {
                NodePlatform::Custom(other.to_owned())
            }
            _ => NodePlatform::CloudNative,
        };

        let now = Utc::now();
        let peer = PeerNode {
            id: node_id.to_owned(),
            name: node_id.to_owned(),
            platform,
            state: if verified { NodeState::Active } else { NodeState::Unverified },
            address: address.map(|s| s.to_owned()),
            first_seen: now,
            last_heartbeat: now,
            capabilities: vec!["mesh".into()],
            labels: HashMap::from([("source".into(), "mesh".into())]),
        };

        debug!(node_id = %node_id, "mesh peer join → cluster membership");
        self.peers.insert(node_id.to_owned(), peer);
        self.persist();
        Ok(true)
    }

    /// Drain a mesh peer-event receiver and apply each event (WEFT-120).
    ///
    /// Spawns a background task. Intended for boot when `ClusterService`
    /// is not available (mesh-only builds) or as a shared membership
    /// path. Prefer [`ClusterService::spawn_mesh_peer_listener`] when
    /// the cluster feature is on so ruvector nodes stay in sync too.
    #[cfg(all(feature = "mesh", feature = "native"))]
    pub fn spawn_mesh_peer_listener(
        self: &Arc<Self>,
        mut rx: tokio::sync::broadcast::Receiver<crate::mesh_discovery::MeshPeerEvent>,
    ) {
        let membership = Arc::clone(self);
        tokio::spawn(async move {
            let mut reap = tokio::time::interval(UNVERIFIED_REAP_EVERY);
            loop {
                let received = tokio::select! {
                    _ = reap.tick() => {
                        membership.expire_unverified(UNVERIFIED_TTL);
                        membership.expire_stale(STALE_UNREACHABLE_AFTER, STALE_REMOVE_AFTER);
                        continue;
                    }
                    r = rx.recv() => r,
                };
                match received {
                    Ok(event) => {
                        if let Err(e) = membership.apply_mesh_peer_event(&event) {
                            debug!(
                                error = %e,
                                node = %event.node_id(),
                                "cluster membership: mesh peer event apply failed"
                            );
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        warn!(skipped = n, "cluster membership: mesh peer event bus lagged");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        info!("cluster membership: mesh peer event bus closed");
                        break;
                    }
                }
            }
        });
    }
}

// ── Time-windowed pairing (Cognitum Seed Gap #2) ────────────────────
//
// Default: mesh rejects all new peer connections.
// `open_pairing_window(duration)` opens a time-limited enrollment window.
// During the window, new Noise XX handshakes are accepted.
// After the window closes, new peers are rejected.
// Paired peers are remembered persistently in `paired_hosts.json`.

/// Result returned when a pairing window is opened.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairingWindowResult {
    /// How long the window will remain open.
    pub window_secs: u64,
    /// Peers that were paired during previous windows.
    pub already_paired: Vec<String>,
}

/// Persistent record of paired hosts.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PairedHostsFile {
    /// Set of peer IDs that have been paired.
    pub hosts: Vec<PairedHost>,
}

/// A single paired host record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairedHost {
    /// Peer identifier.
    pub peer_id: String,
    /// When the pairing was established.
    pub paired_at: DateTime<Utc>,
}

/// Manages the pairing window state and persistent paired host list.
pub struct PairingGate {
    /// When the current window expires (if open).
    window_until: std::sync::Mutex<Option<Instant>>,
    /// Peers paired during the current open window.
    pending_pairs: std::sync::Mutex<Vec<String>>,
    /// All persistently paired peer IDs.
    paired_hosts: std::sync::Mutex<HashSet<String>>,
    /// Path to the paired hosts persistence file.
    persist_path: PathBuf,
}

impl PairingGate {
    /// Create a new pairing gate.
    ///
    /// `persist_path` should point to e.g. `.weftos/runtime/paired_hosts.json`.
    pub fn new(persist_path: PathBuf) -> Self {
        Self {
            window_until: std::sync::Mutex::new(None),
            pending_pairs: std::sync::Mutex::new(Vec::new()),
            paired_hosts: std::sync::Mutex::new(HashSet::new()),
            persist_path,
        }
    }

    /// Load paired hosts from disk. Creates file if missing.
    pub fn load(&self) -> Result<usize, std::io::Error> {
        if !self.persist_path.exists() {
            if let Some(parent) = self.persist_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            self.save()?;
            return Ok(0);
        }

        let data = std::fs::read_to_string(&self.persist_path)?;
        let file: PairedHostsFile = serde_json::from_str(&data)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

        let mut hosts = self
            .paired_hosts
            .lock()
            .expect("paired_hosts lock poisoned");
        for h in &file.hosts {
            hosts.insert(h.peer_id.clone());
        }

        let count = hosts.len();
        info!(count, "loaded paired hosts from disk");
        Ok(count)
    }

    /// Persist paired hosts to disk.
    pub fn save(&self) -> Result<(), std::io::Error> {
        let hosts = self
            .paired_hosts
            .lock()
            .expect("paired_hosts lock poisoned");
        let file = PairedHostsFile {
            hosts: hosts
                .iter()
                .map(|id| PairedHost {
                    peer_id: id.clone(),
                    paired_at: Utc::now(),
                })
                .collect(),
        };
        let json = serde_json::to_string_pretty(&file).map_err(std::io::Error::other)?;
        if let Some(parent) = self.persist_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&self.persist_path, json)
    }

    /// Open a pairing window for the given duration.
    pub fn open_pairing_window(&self, duration: Duration) -> PairingWindowResult {
        let mut until = self.window_until.lock().expect("window lock poisoned");
        *until = Some(Instant::now() + duration);

        let mut pending = self.pending_pairs.lock().expect("pending lock poisoned");
        pending.clear();

        let already: Vec<String> = self
            .paired_hosts
            .lock()
            .expect("paired_hosts lock poisoned")
            .iter()
            .cloned()
            .collect();

        info!(window_secs = duration.as_secs(), "pairing window opened");

        PairingWindowResult {
            window_secs: duration.as_secs(),
            already_paired: already,
        }
    }

    /// Check whether the pairing window is currently open.
    pub fn is_pairing_open(&self) -> bool {
        let until = self.window_until.lock().expect("window lock poisoned");
        match *until {
            Some(deadline) => Instant::now() < deadline,
            None => false,
        }
    }

    /// Determine whether a peer should be accepted.
    ///
    /// Returns `true` if:
    /// - The peer is already paired, OR
    /// - The pairing window is currently open (and records the peer).
    pub fn should_accept_peer(&self, peer_id: &str) -> bool {
        // Already paired: always accept.
        {
            let hosts = self
                .paired_hosts
                .lock()
                .expect("paired_hosts lock poisoned");
            if hosts.contains(peer_id) {
                return true;
            }
        }

        // Check if window is open.
        if self.is_pairing_open() {
            // Pair this host.
            let mut hosts = self
                .paired_hosts
                .lock()
                .expect("paired_hosts lock poisoned");
            hosts.insert(peer_id.to_owned());

            let mut pending = self.pending_pairs.lock().expect("pending lock poisoned");
            pending.push(peer_id.to_owned());

            info!(peer_id, "new peer paired during open window");

            // Best-effort persist.
            drop(hosts);
            drop(pending);
            if let Err(e) = self.save() {
                warn!(error = %e, "failed to persist paired hosts");
            }

            return true;
        }

        debug!(
            peer_id,
            "rejecting peer: pairing window closed and not previously paired"
        );
        false
    }

    /// List all persistently paired host IDs.
    pub fn paired_hosts(&self) -> Vec<String> {
        self.paired_hosts
            .lock()
            .expect("paired_hosts lock poisoned")
            .iter()
            .cloned()
            .collect()
    }

    /// Remove a previously paired host. Returns `true` if it was present.
    pub fn unpair(&self, peer_id: &str) -> bool {
        let mut hosts = self
            .paired_hosts
            .lock()
            .expect("paired_hosts lock poisoned");
        let removed = hosts.remove(peer_id);
        if removed {
            drop(hosts);
            if let Err(e) = self.save() {
                warn!(error = %e, "failed to persist after unpair");
            }
            info!(peer_id, "unpaired host");
        }
        removed
    }

    /// Return the path used for persistence.
    pub fn persist_path(&self) -> &Path {
        &self.persist_path
    }
}

impl std::fmt::Debug for PairingGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self.paired_hosts.lock().map(|h| h.len()).unwrap_or(0);
        f.debug_struct("PairingGate")
            .field("open", &self.is_pairing_open())
            .field("paired_hosts", &count)
            .field("persist_path", &self.persist_path)
            .finish()
    }
}

// ── ClusterService (native coordinator layer) ────────────────────────
//
// Wraps ruvector's ClusterManager behind the `cluster` feature flag.
// Only runs on native coordinator nodes; browser/edge nodes participate
// through the universal ClusterMembership layer via WebSocket.

#[cfg(feature = "cluster")]
mod cluster_service {
    use std::sync::Arc;
    use std::time::Duration;

    use async_trait::async_trait;
    use tracing::{debug, info};

    use ruvector_cluster::{
        ClusterConfig as RuvectorClusterConfig, ClusterManager, ClusterNode, DiscoveryService,
        NodeStatus, ShardInfo, StaticDiscovery,
    };

    use crate::cluster::{ClusterMembership, NodePlatform, NodeState, PeerNode};
    use crate::health::HealthStatus;
    use crate::service::{ServiceType, SystemService};
    use clawft_types::config::ClusterNetworkConfig;

    /// Native coordinator cluster service.
    ///
    /// Wraps ruvector's [`ClusterManager`] and syncs discovered nodes
    /// into the kernel's universal [`ClusterMembership`] tracker.
    pub struct ClusterService {
        manager: ClusterManager,
        membership: Arc<ClusterMembership>,
        config: ClusterNetworkConfig,
        /// True once a mesh peer-event listener has been spawned (WEFT-120).
        mesh_listener_started: std::sync::atomic::AtomicBool,
    }

    impl ClusterService {
        /// Create a new cluster service.
        ///
        /// `membership` is the kernel's universal peer tracker that
        /// all platforms share. The ClusterService syncs ruvector
        /// native node state into it.
        pub fn new(
            config: ClusterNetworkConfig,
            node_id: String,
            discovery: Box<dyn DiscoveryService>,
            membership: Arc<ClusterMembership>,
        ) -> Result<Self, ruvector_cluster::ClusterError> {
            let ruvector_config = RuvectorClusterConfig {
                replication_factor: config.replication_factor,
                shard_count: config.shard_count,
                heartbeat_interval: Duration::from_secs(config.heartbeat_interval_secs),
                node_timeout: Duration::from_secs(config.node_timeout_secs),
                enable_consensus: config.enable_consensus,
                min_quorum_size: config.min_quorum_size,
            };

            let manager = ClusterManager::new(ruvector_config, node_id, discovery)?;

            Ok(Self {
                manager,
                membership,
                config,
                mesh_listener_started: std::sync::atomic::AtomicBool::new(false),
            })
        }

        /// Create with default config and static (empty) discovery.
        pub fn with_defaults(
            node_id: String,
            membership: Arc<ClusterMembership>,
        ) -> Result<Self, ruvector_cluster::ClusterError> {
            let config = ClusterNetworkConfig::default();
            let discovery = Box::new(StaticDiscovery::new(vec![]));
            Self::new(config, node_id, discovery, membership)
        }

        /// Sync ruvector's native node list into the kernel's
        /// [`ClusterMembership`] tracker.
        ///
        /// Converts ruvector [`ClusterNode`] entries into kernel
        /// [`PeerNode`] entries, mapping `SocketAddr` → `String`
        /// and `NodeStatus` → `NodeState`.
        ///
        /// Prefer the mesh peer-event path ([`Self::apply_mesh_peer_event`] /
        /// [`Self::spawn_mesh_peer_listener`]) for live membership; this
        /// pull-based method remains for bootstrap and fallback.
        pub fn sync_to_membership(&self) {
            let nodes = self.manager.list_nodes();
            for node in &nodes {
                let peer = Self::cluster_node_to_peer(node);
                if self.membership.get_peer(&peer.id).is_some() {
                    // Update existing peer's state
                    let new_state = Self::map_status(node.status);
                    let _ = self.membership.update_state(&peer.id, new_state);
                    let _ = self.membership.heartbeat(&peer.id);
                } else {
                    // Add new peer
                    if let Err(e) = self.membership.add_peer(peer) {
                        debug!(error = %e, "failed to sync node to membership");
                    }
                }
            }
        }

        /// Apply one mesh peer event: membership (atomic) + ruvector manager.
        ///
        /// Membership is updated synchronously; manager `add_node` /
        /// `remove_node` are best-effort fire-and-forget via the runtime
        /// (call from async context via [`Self::apply_mesh_peer_event_async`]
        /// when manager updates must complete).
        #[cfg(feature = "mesh")]
        pub fn apply_mesh_peer_event(
            &self,
            event: &crate::mesh_discovery::MeshPeerEvent,
        ) -> Result<bool, crate::cluster::ClusterError> {
            self.membership.apply_mesh_peer_event(event)
        }

        /// Apply mesh peer event and await ruvector manager join/leave.
        #[cfg(feature = "mesh")]
        pub async fn apply_mesh_peer_event_async(
            &self,
            event: &crate::mesh_discovery::MeshPeerEvent,
        ) -> Result<bool, crate::cluster::ClusterError> {
            use crate::mesh_discovery::MeshPeerEvent;
            let changed = self.membership.apply_mesh_peer_event(event)?;
            match event {
                // The ruvector manager only learns of nodes admission verified:
                // an unverified claim is not a replication peer.
                MeshPeerEvent::Joined {
                    node_id,
                    address,
                    verified: true,
                    ..
                }
                | MeshPeerEvent::Recovered {
                    node_id,
                    address,
                    verified: true,
                } => {
                    let addr: std::net::SocketAddr = address
                        .as_deref()
                        .and_then(|s| s.parse().ok())
                        .unwrap_or_else(|| "0.0.0.0:0".parse().unwrap());
                    let node = ClusterNode::new(node_id.clone(), addr);
                    if let Err(e) = self.manager.add_node(node).await {
                        debug!(error = %e, node = %node_id, "cluster manager: add_node from mesh");
                    }
                }
                MeshPeerEvent::Left { node_id } | MeshPeerEvent::Unreachable { node_id } => {
                    if let Err(e) = self.manager.remove_node(node_id).await {
                        debug!(
                            error = %e,
                            node = %node_id,
                            "cluster manager: remove_node from mesh"
                        );
                    }
                }
                MeshPeerEvent::Alive { .. }
                | MeshPeerEvent::Suspect { .. }
                | MeshPeerEvent::Joined { .. }
                | MeshPeerEvent::Recovered { .. } => {}
            }
            Ok(changed)
        }

        /// Subscribe to the mesh peer-event stream and keep membership +
        /// ruvector manager aligned with live mesh state (WEFT-120).
        ///
        /// Idempotent: only one listener task is spawned per service.
        #[cfg(feature = "mesh")]
        pub fn spawn_mesh_peer_listener(
            self: &Arc<Self>,
            mut rx: tokio::sync::broadcast::Receiver<crate::mesh_discovery::MeshPeerEvent>,
        ) {
            use std::sync::atomic::Ordering;
            if self
                .mesh_listener_started
                .swap(true, Ordering::SeqCst)
            {
                debug!("cluster service mesh peer listener already started");
                return;
            }
            let svc = Arc::clone(self);
            tokio::spawn(async move {
                info!("cluster service subscribed to mesh peer-event stream");
                let mut reap = tokio::time::interval(super::UNVERIFIED_REAP_EVERY);
                loop {
                    let received = tokio::select! {
                        _ = reap.tick() => {
                            svc.membership.expire_unverified(super::UNVERIFIED_TTL);
                            svc.membership.expire_stale(
                                super::STALE_UNREACHABLE_AFTER,
                                super::STALE_REMOVE_AFTER,
                            );
                            continue;
                        }
                        r = rx.recv() => r,
                    };
                    match received {
                        Ok(event) => {
                            if let Err(e) = svc.apply_mesh_peer_event_async(&event).await {
                                debug!(
                                    error = %e,
                                    node = %event.node_id(),
                                    "cluster service: mesh peer event apply failed"
                                );
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!(
                                skipped = n,
                                "cluster service: mesh peer event bus lagged"
                            );
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            info!("cluster service: mesh peer event bus closed");
                            break;
                        }
                    }
                }
            });
        }

        /// Whether a mesh peer-event listener is attached.
        #[cfg(feature = "mesh")]
        pub fn mesh_listener_started(&self) -> bool {
            self.mesh_listener_started
                .load(std::sync::atomic::Ordering::SeqCst)
        }

        /// Shared membership handle (tests / boot introspection).
        pub fn membership(&self) -> &Arc<ClusterMembership> {
            &self.membership
        }

        /// Get the cluster network configuration.
        pub fn config(&self) -> &ClusterNetworkConfig {
            &self.config
        }

        /// Get the underlying cluster manager (for advanced operations).
        pub fn manager(&self) -> &ClusterManager {
            &self.manager
        }

        /// Get cluster statistics.
        pub fn stats(&self) -> ruvector_cluster::ClusterStats {
            self.manager.get_stats()
        }

        /// List all shards.
        pub fn list_shards(&self) -> Vec<ShardInfo> {
            self.manager.list_shards()
        }

        /// List all ruvector nodes.
        pub fn list_nodes(&self) -> Vec<ClusterNode> {
            self.manager.list_nodes()
        }

        /// Convert a ruvector `NodeStatus` to a kernel `NodeState`.
        fn map_status(status: NodeStatus) -> NodeState {
            match status {
                NodeStatus::Leader | NodeStatus::Follower | NodeStatus::Candidate => {
                    NodeState::Active
                }
                NodeStatus::Offline => NodeState::Unreachable,
            }
        }

        /// Convert a ruvector `ClusterNode` to a kernel `PeerNode`.
        fn cluster_node_to_peer(node: &ClusterNode) -> PeerNode {
            PeerNode {
                id: node.node_id.clone(),
                name: node
                    .metadata
                    .get("name")
                    .cloned()
                    .unwrap_or_else(|| node.node_id.clone()),
                platform: NodePlatform::CloudNative,
                state: Self::map_status(node.status),
                address: Some(node.address.to_string()),
                first_seen: node.last_seen, // best approximation
                last_heartbeat: node.last_seen,
                capabilities: Vec::new(),
                labels: node.metadata.clone(),
            }
        }
    }

    #[async_trait]
    impl SystemService for ClusterService {
        fn name(&self) -> &str {
            "cluster"
        }

        fn service_type(&self) -> ServiceType {
            ServiceType::Core
        }

        async fn start(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            info!("starting cluster service");

            // ruvector's ClusterManager.start() initializes shards by
            // hashing each shard_id against the consistent-hash ring;
            // an empty ring fails with "No nodes available for shard
            // assignment". The manager never auto-registers its own
            // node, so a single-node cluster (or a coordinator that
            // boots before any peer is discovered) hits the failure
            // every time.
            //
            // Register the local node first so the ring always has at
            // least one entry. Address is a synthetic loopback
            // placeholder — ruvector itself doesn't dial it, the
            // address only matters for cross-node routing once peers
            // join, and the kernel's mesh layer is the authoritative
            // bind. add_node also internally calls rebalance_shards;
            // ignore the rebalance error path because the ring is
            // guaranteed non-empty after the insert above.
            let local_id = self.membership.local_node_id().to_owned();
            let placeholder_addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
            let self_node = ClusterNode::new(local_id, placeholder_addr);
            if let Err(e) = self.manager.add_node(self_node).await {
                debug!(error = %e, "cluster: failed to pre-register self (continuing)");
            }

            self.manager
                .start()
                .await
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })?;
            self.sync_to_membership();
            info!(
                nodes = self.manager.list_nodes().len(),
                shards = self.manager.list_shards().len(),
                "cluster service started"
            );
            Ok(())
        }

        async fn stop(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            info!("stopping cluster service");
            // Mark self as leaving in membership
            let local_id = self.membership.local_node_id().to_owned();
            let _ = self.membership.update_state(&local_id, NodeState::Left);
            Ok(())
        }

        async fn health_check(&self) -> HealthStatus {
            let stats = self.manager.get_stats();
            if stats.healthy_nodes > 0 {
                HealthStatus::Healthy
            } else {
                HealthStatus::Degraded("no healthy cluster nodes".into())
            }
        }
    }
}

#[cfg(feature = "cluster")]
pub use cluster_service::ClusterService;


/// A pre-0.8 UUID peer id (`8-4-4-4-12` hex). Current node ids are 32 lowercase hex.
fn is_legacy_uuid_id(id: &str) -> bool {
    let parts: Vec<&str> = id.split('-').collect();
    parts.len() == 5
        && parts.iter().zip([8, 4, 4, 4, 12]).all(|(p, n)| p.len() == n && p.bytes().all(|b| b.is_ascii_hexdigit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_peer(id: &str, name: &str) -> PeerNode {
        PeerNode {
            id: id.into(),
            name: name.into(),
            platform: NodePlatform::CloudNative,
            state: NodeState::Active,
            address: Some("10.0.0.1:8080".into()),
            first_seen: Utc::now(),
            last_heartbeat: Utc::now(),
            capabilities: vec!["compute".into()],
            labels: HashMap::from([("region".into(), "us-east".into())]),
        }
    }

    #[test]
    fn default_config() {
        let config = ClusterConfig::default();
        assert_eq!(config.node_name, "local");
        assert_eq!(config.heartbeat_interval_secs, 5);
        assert_eq!(config.suspect_threshold, 3);
    }

    #[test]
    #[cfg(feature = "ecc")]
    fn recommended_heartbeat_falls_back_when_no_model() {
        // Finding #5: with no model, the recommendation must equal
        // the configured heartbeat_interval_secs.
        let config = ClusterConfig {
            heartbeat_interval_secs: 7,
            ..Default::default()
        };
        assert_eq!(config.recommended_heartbeat_secs(None, 0, 0.0, 0.0), 7);
    }

    #[test]
    #[cfg(feature = "ecc")]
    fn recommended_heartbeat_falls_back_when_untrained() {
        // Finding #5: an untrained model must not override the config.
        let config = ClusterConfig {
            heartbeat_interval_secs: 9,
            ..Default::default()
        };
        let model = crate::eml_kernel::GossipTimingModel::new();
        assert!(!model.is_trained());
        assert_eq!(
            config.recommended_heartbeat_secs(Some(&model), 5, 50.0, 1.0),
            9,
        );
    }

    #[test]
    #[cfg(feature = "ecc")]
    fn recommended_heartbeat_uses_trained_model() {
        // Finding #5: with a trained model the recommendation comes
        // from the model's predict, not the config.
        let model = crate::eml_kernel::GossipTimingModel::new();
        let mut json = serde_json::to_value(&model).unwrap();
        if let Some(inner) = json.get_mut("inner").and_then(|v| v.as_object_mut()) {
            inner.insert("trained".into(), serde_json::Value::Bool(true));
        }
        let forced: crate::eml_kernel::GossipTimingModel = serde_json::from_value(json).unwrap();
        assert!(forced.is_trained());

        let config = ClusterConfig {
            heartbeat_interval_secs: 5,
            ..Default::default()
        };
        let recommended = config.recommended_heartbeat_secs(Some(&forced), 10, 50.0, 1.0);
        // Clamped to [1, 60] by the model.
        assert!((1..=60).contains(&recommended));
    }

    #[test]
    fn config_serde_roundtrip() {
        let config = ClusterConfig {
            node_id: "node-1".into(),
            node_name: "primary".into(),
            platform: NodePlatform::Edge,
            heartbeat_interval_secs: 10,
            suspect_threshold: 5,
            unreachable_threshold: 15,
            max_nodes: 100,
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        let restored: ClusterConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.node_name, "primary");
        assert_eq!(restored.max_nodes, 100);
    }

    #[test]
    fn node_platform_display() {
        assert_eq!(NodePlatform::CloudNative.to_string(), "cloud-native");
        assert_eq!(NodePlatform::Browser.to_string(), "browser");
        assert_eq!(
            NodePlatform::Custom("k8s".into()).to_string(),
            "custom(k8s)"
        );
    }

    #[test]
    fn node_state_display() {
        assert_eq!(NodeState::Active.to_string(), "active");
        assert_eq!(NodeState::Suspect.to_string(), "suspect");
        assert_eq!(NodeState::Unreachable.to_string(), "unreachable");
    }

    /// Helper: create a ClusterMembership with rate limiting disabled for tests.
    fn make_cluster(config: ClusterConfig) -> ClusterMembership {
        ClusterMembership::new(config).with_min_peer_interval(std::time::Duration::ZERO)
    }

    #[test]
    fn add_and_list_peers() {
        let cluster = make_cluster(ClusterConfig::default());
        cluster.add_peer(make_peer("node-1", "alpha")).unwrap();
        cluster.add_peer(make_peer("node-2", "beta")).unwrap();

        let peers = cluster.list_peers();
        assert_eq!(peers.len(), 2);
    }

    #[test]
    fn add_duplicate_fails() {
        let cluster = make_cluster(ClusterConfig::default());
        cluster.add_peer(make_peer("node-1", "alpha")).unwrap();
        assert!(matches!(
            cluster.add_peer(make_peer("node-1", "alpha-dup")),
            Err(ClusterError::NodeAlreadyExists { .. })
        ));
    }

    #[test]
    fn cluster_full() {
        let config = ClusterConfig {
            max_nodes: 1,
            ..Default::default()
        };
        let cluster = make_cluster(config);
        cluster.add_peer(make_peer("node-1", "alpha")).unwrap();
        assert!(matches!(
            cluster.add_peer(make_peer("node-2", "beta")),
            Err(ClusterError::ClusterFull { .. })
        ));
    }

    #[test]
    fn remove_peer() {
        let cluster = make_cluster(ClusterConfig::default());
        cluster.add_peer(make_peer("node-1", "alpha")).unwrap();
        let removed = cluster.remove_peer("node-1").unwrap();
        assert_eq!(removed.name, "alpha");
        assert!(cluster.is_empty());
    }

    #[test]
    fn remove_nonexistent_fails() {
        let cluster = make_cluster(ClusterConfig::default());
        assert!(matches!(
            cluster.remove_peer("nope"),
            Err(ClusterError::NodeNotFound { .. })
        ));
    }

    #[test]
    fn update_state() {
        let cluster = make_cluster(ClusterConfig::default());
        cluster.add_peer(make_peer("node-1", "alpha")).unwrap();
        cluster.update_state("node-1", NodeState::Suspect).unwrap();
        let peer = cluster.get_peer("node-1").unwrap();
        assert_eq!(peer.state, NodeState::Suspect);
    }

    #[test]
    fn heartbeat_clears_suspect() {
        let cluster = make_cluster(ClusterConfig::default());
        cluster.add_peer(make_peer("node-1", "alpha")).unwrap();
        cluster.update_state("node-1", NodeState::Suspect).unwrap();
        cluster.heartbeat("node-1").unwrap();
        let peer = cluster.get_peer("node-1").unwrap();
        assert_eq!(peer.state, NodeState::Active);
    }

    #[test]
    fn count_by_state() {
        let cluster = make_cluster(ClusterConfig::default());
        cluster.add_peer(make_peer("node-1", "alpha")).unwrap();
        cluster.add_peer(make_peer("node-2", "beta")).unwrap();
        cluster.update_state("node-2", NodeState::Suspect).unwrap();
        assert_eq!(cluster.count_by_state(&NodeState::Active), 1);
        assert_eq!(cluster.count_by_state(&NodeState::Suspect), 1);
    }

    #[test]
    fn active_peers() {
        let cluster = make_cluster(ClusterConfig::default());
        cluster.add_peer(make_peer("node-1", "alpha")).unwrap();
        cluster.add_peer(make_peer("node-2", "beta")).unwrap();
        cluster.update_state("node-2", NodeState::Leaving).unwrap();
        let active = cluster.active_peers();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0], "node-1");
    }

    #[test]
    fn peer_serde_roundtrip() {
        let peer = make_peer("node-1", "alpha");
        let json = serde_json::to_string(&peer).unwrap();
        let restored: PeerNode = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.id, "node-1");
        assert_eq!(restored.capabilities, vec!["compute"]);
    }

    #[test]
    fn cluster_error_display() {
        let err = ClusterError::NodeNotFound {
            node_id: "node-1".into(),
        };
        assert!(err.to_string().contains("node-1"));

        let err = ClusterError::ClusterFull { max: 10 };
        assert!(err.to_string().contains("10"));
    }

    #[test]
    fn mesh_and_auth_error_display() {
        let err = ClusterError::Mesh("connection refused".into());
        assert!(err.to_string().contains("connection refused"));

        let err = ClusterError::AuthFailed("bad signature".into());
        assert!(err.to_string().contains("bad signature"));
    }

    #[test]
    fn default_config_new_fields() {
        let config = ClusterConfig::default();
        assert!(config.bind_address.is_none());
        assert!(config.seed_peers.is_empty());
        assert!(config.identity_key_path.is_none());
        // WEFT-148 defaults
        assert_eq!(config.peer_add_max_per_source, 1);
        assert_eq!(config.peer_add_window_ms, 100);
        assert!(config.peer_add_rate_config().is_enabled());
    }

    #[test]
    fn rate_limited_peer_additions() {
        // Use the default per-source rate limit (do NOT use make_cluster here).
        // make_peer assigns the same address → same source key.
        let cluster = ClusterMembership::new(ClusterConfig::default());
        cluster.add_peer(make_peer("node-1", "alpha")).unwrap();

        // Second add from the same source immediately should be rate limited.
        let result = cluster.add_peer(make_peer("node-2", "beta"));
        assert!(
            matches!(result, Err(ClusterError::RateLimited)),
            "expected RateLimited, got {result:?}"
        );
    }

    #[test]
    fn rate_limit_is_per_source_not_global() {
        // Different addresses are independent sources (WEFT-148).
        let cluster = ClusterMembership::new(ClusterConfig::default());
        let mut a = make_peer("node-1", "alpha");
        a.address = Some("10.0.0.1:8080".into());
        let mut b = make_peer("node-2", "beta");
        b.address = Some("10.0.0.2:8080".into());

        cluster.add_peer(a).unwrap();
        // Different source must still succeed under default 1/100ms limit.
        cluster.add_peer(b).unwrap();
        assert_eq!(cluster.len(), 2);
    }

    #[test]
    fn rate_limit_configurable_threshold() {
        // Allow 2 adds per source per long window.
        let cluster = ClusterMembership::new(ClusterConfig::default())
            .with_peer_add_rate_limit(crate::rate_limit::RateLimitConfig {
                max_per_window: 2,
                window: Duration::from_secs(60),
            });
        cluster.add_peer(make_peer("node-1", "alpha")).unwrap();
        cluster.add_peer(make_peer("node-2", "beta")).unwrap();
        let third = cluster.add_peer(make_peer("node-3", "gamma"));
        assert!(
            matches!(third, Err(ClusterError::RateLimited)),
            "expected RateLimited on third same-source add, got {third:?}"
        );
    }

    #[test]
    fn rate_limit_source_key_prefers_address() {
        let mut peer = make_peer("node-1", "alpha");
        peer.address = Some("192.168.1.5:9489".into());
        assert_eq!(
            ClusterMembership::peer_add_source_key(&peer),
            "192.168.1.5:9489"
        );
        peer.address = None;
        assert_eq!(ClusterMembership::peer_add_source_key(&peer), "node-1");
        peer.address = Some(String::new());
        assert_eq!(ClusterMembership::peer_add_source_key(&peer), "node-1");
    }

    #[test]
    fn rate_limited_error_display() {
        let err = ClusterError::RateLimited;
        assert!(err.to_string().contains("rate limited"));
    }

    #[test]
    fn validate_known_capabilities() {
        let known = vec!["ipc".into(), "mesh".into(), "chain".into()];
        let unknown = validate_capabilities(&known);
        assert!(unknown.is_empty());
    }

    #[test]
    fn validate_unknown_capabilities() {
        let caps = vec!["ipc".into(), "teleport".into(), "quantum".into()];
        let unknown = validate_capabilities(&caps);
        assert_eq!(unknown, vec!["teleport", "quantum"]);
    }

    #[test]
    fn add_peer_with_unknown_capabilities_succeeds() {
        let cluster = make_cluster(ClusterConfig::default());
        let mut peer = make_peer("node-1", "alpha");
        peer.capabilities = vec!["ipc".into(), "teleport".into()];
        // Should succeed (warning logged, but not rejected).
        cluster.add_peer(peer).unwrap();
        assert_eq!(cluster.len(), 1);
    }

    #[test]
    fn add_peer_checked_allows_clean_host() {
        let cluster = make_cluster(ClusterConfig::default());
        let dir = tempfile::tempdir().unwrap();
        let list = crate::revocation::RevocationList::new(dir.path().join("r.json"));
        let peer = make_peer("node-1", "alpha");
        assert!(cluster.add_peer_checked(peer, &list).is_ok());
        assert_eq!(cluster.len(), 1);
    }

    #[test]
    fn add_peer_checked_rejects_revoked_host() {
        let cluster = make_cluster(ClusterConfig::default());
        let dir = tempfile::tempdir().unwrap();
        let list = crate::revocation::RevocationList::new(dir.path().join("r.json"));
        list.revoke_host("node-1", "banned");
        let peer = make_peer("node-1", "alpha");
        let result = cluster.add_peer_checked(peer, &list);
        assert!(matches!(result, Err(ClusterError::AuthFailed(_))));
        assert_eq!(cluster.len(), 0);
    }

    // ── PairingGate tests ───────────────────────────────────────────

    fn pairing_tmp_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "pairing_test_{name}_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn cleanup_pairing(path: &std::path::Path) {
        if let Some(parent) = path.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }

    #[test]
    fn pairing_gate_default_closed() {
        let path = pairing_tmp_path("default_closed").join("paired_hosts.json");
        let gate = PairingGate::new(path.clone());
        assert!(!gate.is_pairing_open());
        assert!(!gate.should_accept_peer("unknown-peer"));
        cleanup_pairing(&path);
    }

    #[test]
    fn pairing_gate_open_window_accepts() {
        let path = pairing_tmp_path("open_window").join("paired_hosts.json");
        let gate = PairingGate::new(path.clone());

        let result = gate.open_pairing_window(Duration::from_secs(30));
        assert_eq!(result.window_secs, 30);
        assert!(result.already_paired.is_empty());
        assert!(gate.is_pairing_open());

        // Should accept new peers during window.
        assert!(gate.should_accept_peer("peer-1"));
        assert!(gate.should_accept_peer("peer-2"));

        // Paired hosts should now include them.
        let hosts = gate.paired_hosts();
        assert!(hosts.contains(&"peer-1".to_owned()));
        assert!(hosts.contains(&"peer-2".to_owned()));

        cleanup_pairing(&path);
    }

    #[test]
    fn pairing_gate_already_paired_always_accepted() {
        let path = pairing_tmp_path("already_paired").join("paired_hosts.json");
        let gate = PairingGate::new(path.clone());

        // Open window and pair a host.
        gate.open_pairing_window(Duration::from_secs(30));
        assert!(gate.should_accept_peer("peer-1"));

        // Close window by setting it to zero duration.
        gate.open_pairing_window(Duration::from_secs(0));
        std::thread::sleep(Duration::from_millis(10));
        assert!(!gate.is_pairing_open());

        // Already paired host should still be accepted.
        assert!(gate.should_accept_peer("peer-1"));

        // Unknown host should be rejected (window closed).
        assert!(!gate.should_accept_peer("unknown"));

        cleanup_pairing(&path);
    }

    #[test]
    fn pairing_gate_unpair() {
        let path = pairing_tmp_path("unpair").join("paired_hosts.json");
        let gate = PairingGate::new(path.clone());

        gate.open_pairing_window(Duration::from_secs(30));
        gate.should_accept_peer("peer-1");

        assert!(gate.unpair("peer-1"));
        assert!(!gate.unpair("peer-1")); // already removed

        cleanup_pairing(&path);
    }

    #[test]
    fn pairing_gate_persist_and_load() {
        let dir = pairing_tmp_path("persist_load");
        let path = dir.join("paired_hosts.json");

        // Create and pair.
        {
            let gate = PairingGate::new(path.clone());
            gate.open_pairing_window(Duration::from_secs(30));
            gate.should_accept_peer("peer-a");
            gate.should_accept_peer("peer-b");
            gate.save().unwrap();
        }

        // Load into new gate.
        {
            let gate = PairingGate::new(path.clone());
            let count = gate.load().unwrap();
            assert_eq!(count, 2);
            assert!(gate.paired_hosts().contains(&"peer-a".to_owned()));
            assert!(gate.paired_hosts().contains(&"peer-b".to_owned()));
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pairing_gate_load_creates_file_if_missing() {
        let dir = pairing_tmp_path("load_creates");
        let path = dir.join("runtime").join("paired_hosts.json");

        let gate = PairingGate::new(path.clone());
        let count = gate.load().unwrap();
        assert_eq!(count, 0);
        assert!(path.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pairing_window_result_serde() {
        let result = PairingWindowResult {
            window_secs: 30,
            already_paired: vec!["peer-1".into()],
        };
        let json = serde_json::to_string(&result).unwrap();
        let restored: PairingWindowResult = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.window_secs, 30);
        assert_eq!(restored.already_paired, vec!["peer-1"]);
    }

    fn persist_tmp_path(suffix: &str) -> PathBuf {
        let pid = std::process::id();
        let ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("weftos-cluster-{pid}-{ns}-{suffix}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("cluster_peers.json")
    }

    #[test]
    fn persisted_peers_rehydrate_on_restart() {
        let path = persist_tmp_path("rehydrate");

        {
            let cluster = ClusterMembership::new(ClusterConfig::default())
                .with_min_peer_interval(std::time::Duration::ZERO)
                .with_persist_path(&path);
            cluster.add_peer(make_peer("node-1", "alpha")).unwrap();
            cluster.add_peer(make_peer("node-2", "beta")).unwrap();
            assert_eq!(cluster.len(), 2);
        }

        assert!(path.exists(), "persist path should exist after add_peer");

        let restored = ClusterMembership::new(ClusterConfig::default()).with_persist_path(&path);
        assert_eq!(restored.len(), 2, "peers should rehydrate from disk");
        assert!(restored.get_peer("node-1").is_some());
        assert!(restored.get_peer("node-2").is_some());

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn legacy_uuid_peer_ids_are_dropped_on_load_and_from_disk() {
        let path = persist_tmp_path("legacy-uuid");
        let uuid = "e6182c9e-2dc9-45c3-a93f-e93d978e3dca";
        let hex = "c12684d4ee48f647f0468b233a24b2c2";
        {
            let cluster = ClusterMembership::new(ClusterConfig::default())
                .with_min_peer_interval(std::time::Duration::ZERO)
                .with_persist_path(&path);
            for id in [uuid, hex, "node-1"] {
                cluster.add_peer(make_peer(id, id)).unwrap();
            }
        }
        let restored = ClusterMembership::new(ClusterConfig::default()).with_persist_path(&path);
        assert!(restored.get_peer(uuid).is_none(), "the UUID entry is dropped");
        assert!(restored.get_peer(hex).is_some() && restored.get_peer("node-1").is_some(), "other ids are kept");
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(!on_disk.contains(uuid), "and it is gone from the file");
        assert!(is_legacy_uuid_id(uuid) && !is_legacy_uuid_id(hex) && !is_legacy_uuid_id("node-1"));
        assert!(!is_legacy_uuid_id("e6182c9e-2dc9-45c3-a93f-e93d978e3dcz"), "non-hex is not a UUID");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    fn aged_peer(id: &str, age: chrono::Duration) -> PeerNode {
        let mut p = make_peer(id, id);
        p.last_heartbeat = Utc::now() - age;
        p
    }

    fn cfg_with_id(id: &str) -> ClusterConfig {
        ClusterConfig { node_id: id.into(), ..ClusterConfig::default() }
    }

    #[test]
    fn repeated_boots_with_fresh_node_ids_do_not_accumulate() {
        let path = persist_tmp_path("boots");
        for i in 0..20 {
            let id = format!("ephemeral-{i}");
            let c = ClusterMembership::new(cfg_with_id(&id))
                .with_min_peer_interval(std::time::Duration::ZERO)
                .with_persist_path(&path);
            c.add_peer(make_peer("real-peer", "real")).ok();
            // What ClusterService::start / sync_to_membership does for the local node.
            c.add_peer(make_peer(&id, &id)).unwrap();
        }
        let last = ClusterMembership::new(cfg_with_id("ephemeral-final")).with_persist_path(&path);
        assert_eq!(last.len(), 1, "only the real peer persists");
        assert!(last.get_peer("real-peer").is_some());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn stale_persisted_peers_are_dropped_on_load_tolerantly() {
        let path = persist_tmp_path("stale-load");
        let file = ClusterPeersFile {
            version: 1,
            peers: vec![
                aged_peer("old-junk", chrono::Duration::days(11)),
                aged_peer("recent", chrono::Duration::minutes(5)),
            ],
        };
        std::fs::write(&path, serde_json::to_string(&file).unwrap()).unwrap();
        let c = ClusterMembership::new(cfg_with_id("me")).with_persist_path(&path);
        assert!(c.get_peer("old-junk").is_none());
        assert!(c.get_peer("recent").is_some());
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(!on_disk.contains("old-junk"), "pruned from the file too");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn expire_stale_walks_active_to_unreachable_to_removed() {
        let c = ClusterMembership::new(cfg_with_id("me"))
            .with_min_peer_interval(std::time::Duration::ZERO);
        c.add_peer(make_peer("me", "me")).unwrap();
        c.add_peer(make_peer("member", "member")).unwrap();
        let t0 = Utc::now();
        let at = |d: chrono::Duration| t0 + d;
        let (un, rm) = (STALE_UNREACHABLE_AFTER, STALE_REMOVE_AFTER);

        assert_eq!(c.expire_stale_at(at(chrono::Duration::seconds(30)), un, rm), 0);
        assert_eq!(c.get_peer("member").unwrap().state, NodeState::Active);

        assert_eq!(c.expire_stale_at(at(chrono::Duration::hours(1)), un, rm), 0);
        assert_eq!(c.get_peer("member").unwrap().state, NodeState::Unreachable);

        // A heartbeat revives it through the mesh path; here just check the removal step.
        assert_eq!(c.expire_stale_at(at(chrono::Duration::hours(25)), un, rm), 1);
        assert!(c.get_peer("member").is_none());
        assert!(c.get_peer("me").is_some(), "the local node is never reaped");
    }

    #[test]
    fn a_heartbeating_peer_survives_the_reaper() {
        let c = ClusterMembership::new(cfg_with_id("me"))
            .with_min_peer_interval(std::time::Duration::ZERO);
        c.add_peer(aged_peer("member", chrono::Duration::hours(2))).unwrap();
        c.heartbeat("member").unwrap();
        assert_eq!(c.expire_stale(STALE_UNREACHABLE_AFTER, STALE_REMOVE_AFTER), 0);
        assert_eq!(c.get_peer("member").unwrap().state, NodeState::Active);
    }

    #[test]
    fn persist_reflects_remove_and_state_change() {
        let path = persist_tmp_path("mutations");

        let cluster = ClusterMembership::new(ClusterConfig::default())
            .with_min_peer_interval(std::time::Duration::ZERO)
            .with_persist_path(&path);
        cluster.add_peer(make_peer("node-1", "alpha")).unwrap();
        cluster.add_peer(make_peer("node-2", "beta")).unwrap();
        cluster.update_state("node-1", NodeState::Suspect).unwrap();
        cluster.remove_peer("node-2").unwrap();

        let data = std::fs::read_to_string(&path).unwrap();
        let file: ClusterPeersFile = serde_json::from_str(&data).unwrap();
        assert_eq!(file.peers.len(), 1);
        assert_eq!(file.peers[0].id, "node-1");
        assert_eq!(file.peers[0].state, NodeState::Suspect);

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn with_persist_path_missing_file_is_ok() {
        let path = persist_tmp_path("missing")
            .parent()
            .unwrap()
            .join("nope.json");
        let cluster = ClusterMembership::new(ClusterConfig::default()).with_persist_path(&path);
        assert_eq!(cluster.len(), 0);

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    // ── WEFT-120: mesh peer-event → membership ────────────────────

    #[cfg(feature = "mesh")]
    mod mesh_peer_events {
        use super::*;
        use crate::mesh_discovery::MeshPeerEvent;

        fn joined(id: &str, verified: bool) -> MeshPeerEvent {
            MeshPeerEvent::Joined {
                node_id: id.into(),
                address: Some("10.0.0.6:9489".into()),
                platform: None,
                verified,
            }
        }

        #[test]
        fn an_unverified_join_is_held_unverified_not_active() {
            let cluster = make_cluster(ClusterConfig::default());
            assert!(cluster.apply_mesh_peer_event(&joined("claimed", false)).unwrap());
            assert_eq!(cluster.get_peer("claimed").unwrap().state, NodeState::Unverified);
            assert!(cluster.active_peers().is_empty(), "an unverified claim is not a member");
            // Its route stays: it is listed, with its address.
            assert_eq!(cluster.get_peer("claimed").unwrap().address.as_deref(), Some("10.0.0.6:9489"));
            assert_eq!(cluster.count_by_state(&NodeState::Unverified), 1);
        }

        #[test]
        fn an_unverified_announce_cannot_refresh_last_seen_or_move_a_verified_peers_address() {
            let cluster = make_cluster(ClusterConfig::default());
            cluster.apply_mesh_peer_event(&joined("real", true)).unwrap();
            let old = chrono::DateTime::from_timestamp(1_000_000, 0).unwrap();
            cluster.peers.get_mut("real").unwrap().last_heartbeat = old;
            let spoof = MeshPeerEvent::Joined {
                node_id: "real".into(),
                address: Some("6.6.6.6:1".into()),
                platform: None,
                verified: false,
            };
            cluster.apply_mesh_peer_event(&spoof).unwrap();
            let p = cluster.get_peer("real").unwrap();
            assert_eq!(p.state, NodeState::Active);
            assert_eq!(p.last_heartbeat, old);
            assert_eq!(p.address.as_deref(), Some("10.0.0.6:9489"));
            // A verified announce does refresh it.
            cluster.apply_mesh_peer_event(&joined("real", true)).unwrap();
            assert!(cluster.get_peer("real").unwrap().last_heartbeat > old);
        }

        #[test]
        fn an_alive_event_for_an_unverified_id_gets_no_liveness_credit() {
            let cluster = make_cluster(ClusterConfig::default());
            cluster.apply_mesh_peer_event(&joined("claimed", false)).unwrap();
            let old = chrono::DateTime::from_timestamp(1_000_000, 0).unwrap();
            cluster.peers.get_mut("claimed").unwrap().last_heartbeat = old;
            assert!(!cluster
                .apply_mesh_peer_event(&MeshPeerEvent::Alive { node_id: "claimed".into() })
                .unwrap());
            assert_eq!(cluster.get_peer("claimed").unwrap().last_heartbeat, old);
            // A verified peer's Alive still counts.
            cluster.apply_mesh_peer_event(&joined("real", true)).unwrap();
            cluster.peers.get_mut("real").unwrap().last_heartbeat = old;
            cluster.apply_mesh_peer_event(&MeshPeerEvent::Alive { node_id: "real".into() }).unwrap();
            assert!(cluster.get_peer("real").unwrap().last_heartbeat > old);
        }

        #[test]
        fn heartbeats_and_health_events_never_promote_an_unverified_peer() {
            let cluster = make_cluster(ClusterConfig::default());
            cluster.apply_mesh_peer_event(&joined("claimed", false)).unwrap();
            for ev in [
                MeshPeerEvent::Alive { node_id: "claimed".into() },
                MeshPeerEvent::Suspect { node_id: "claimed".into() },
                MeshPeerEvent::Alive { node_id: "claimed".into() },
                MeshPeerEvent::Recovered {
                    node_id: "claimed".into(),
                    address: None,
                    verified: false,
                },
            ] {
                cluster.apply_mesh_peer_event(&ev).unwrap();
                assert_eq!(cluster.get_peer("claimed").unwrap().state, NodeState::Unverified, "{ev:?}");
            }
            // It still goes when its connection does.
            cluster
                .apply_mesh_peer_event(&MeshPeerEvent::Left { node_id: "claimed".into() })
                .unwrap();
            assert!(cluster.get_peer("claimed").is_none());
        }

        #[test]
        fn only_a_verified_join_makes_an_unverified_peer_active() {
            let cluster = make_cluster(ClusterConfig::default());
            cluster.apply_mesh_peer_event(&joined("p", false)).unwrap();
            cluster.apply_mesh_peer_event(&joined("p", true)).unwrap();
            assert_eq!(cluster.get_peer("p").unwrap().state, NodeState::Active);
            assert_eq!(cluster.active_peers(), vec!["p".to_string()]);
        }

        #[test]
        fn an_unverified_event_does_not_demote_an_active_peer() {
            // An operator-added (or earlier verified) peer keeps its state when
            // a legacy connection for the same id comes and goes.
            let cluster = make_cluster(ClusterConfig::default());
            cluster.apply_mesh_peer_event(&joined("op", true)).unwrap();
            cluster.apply_mesh_peer_event(&joined("op", false)).unwrap();
            assert_eq!(cluster.get_peer("op").unwrap().state, NodeState::Active);
            // A down peer that comes back unverified is not Active either.
            cluster.update_state("op", NodeState::Unreachable).unwrap();
            cluster.apply_mesh_peer_event(&joined("op", false)).unwrap();
            assert_eq!(cluster.get_peer("op").unwrap().state, NodeState::Unverified);
        }

        #[test]
        fn an_unreachable_unverified_peer_is_dropped_not_left_listed() {
            let cluster = make_cluster(ClusterConfig::default());
            cluster.apply_mesh_peer_event(&joined("claimed", false)).unwrap();
            cluster
                .apply_mesh_peer_event(&MeshPeerEvent::Unreachable { node_id: "claimed".into() })
                .unwrap();
            assert!(cluster.get_peer("claimed").is_none());
            // A member that goes unreachable is still kept (as Unreachable).
            cluster.apply_mesh_peer_event(&joined("member", true)).unwrap();
            cluster
                .apply_mesh_peer_event(&MeshPeerEvent::Unreachable { node_id: "member".into() })
                .unwrap();
            assert_eq!(cluster.get_peer("member").unwrap().state, NodeState::Unreachable);
        }

        #[test]
        fn the_reaper_expires_only_stale_unverified_peers() {
            let cluster = make_cluster(ClusterConfig::default());
            cluster.apply_mesh_peer_event(&joined("old-claim", false)).unwrap();
            cluster.apply_mesh_peer_event(&joined("fresh-claim", false)).unwrap();
            cluster.apply_mesh_peer_event(&joined("old-member", true)).unwrap();
            let long_ago = Utc::now() - chrono::Duration::hours(3);
            for id in ["old-claim", "old-member"] {
                cluster.peers.get_mut(id).unwrap().last_heartbeat = long_ago;
            }
            assert_eq!(cluster.expire_unverified(UNVERIFIED_TTL), 1);
            assert!(cluster.get_peer("old-claim").is_none());
            assert!(cluster.get_peer("fresh-claim").is_some(), "recently heard from");
            assert!(cluster.get_peer("old-member").is_some(), "members are not the reaper's");
        }

        #[test]
        fn an_older_event_without_the_flag_reads_as_unverified() {
            let ev: MeshPeerEvent =
                serde_json::from_str(r#"{"Joined":{"node_id":"n","address":null,"platform":null}}"#).unwrap();
            assert!(matches!(ev, MeshPeerEvent::Joined { verified: false, .. }));
        }

        #[cfg(feature = "cluster")]
        #[tokio::test]
        async fn the_ruvector_manager_only_learns_of_verified_nodes() {
            use crate::cluster::ClusterService;
            use clawft_types::config::ClusterNetworkConfig;
            use ruvector_cluster::StaticDiscovery;

            let membership = Arc::new(make_cluster(ClusterConfig {
                node_id: "local".into(),
                ..Default::default()
            }));
            let svc = Arc::new(
                ClusterService::new(
                    ClusterNetworkConfig::default(),
                    "local".into(),
                    Box::new(StaticDiscovery::new(vec![])),
                    Arc::clone(&membership),
                )
                .expect("cluster service"),
            );
            svc.apply_mesh_peer_event_async(&joined("claimed", false)).await.unwrap();
            assert!(membership.get_peer("claimed").is_some());
            assert!(!svc.list_nodes().iter().any(|n| n.node_id == "claimed"));
            svc.apply_mesh_peer_event_async(&joined("real", true)).await.unwrap();
            assert!(svc.list_nodes().iter().any(|n| n.node_id == "real"));
        }

        #[test]
        fn mesh_join_adds_active_peer() {
            let cluster = make_cluster(ClusterConfig::default());
            let changed = cluster
                .apply_mesh_peer_event(&MeshPeerEvent::Joined {
                    node_id: "peer-a".into(),
                    address: Some("10.0.0.5:9489".into()),
                    platform: Some("edge".into()),
                    verified: true,
                })
                .unwrap();
            assert!(changed);
            let peer = cluster.get_peer("peer-a").expect("joined peer");
            assert_eq!(peer.state, NodeState::Active);
            assert_eq!(peer.address.as_deref(), Some("10.0.0.5:9489"));
            assert_eq!(peer.platform, NodePlatform::Edge);
            assert!(peer.capabilities.contains(&"mesh".into()));
        }

        #[test]
        fn mesh_leave_removes_peer() {
            let cluster = make_cluster(ClusterConfig::default());
            cluster
                .apply_mesh_peer_event(&MeshPeerEvent::Joined {
                    node_id: "peer-b".into(),
                    address: None,
                    platform: None,
                    verified: true,
                })
                .unwrap();
            assert_eq!(cluster.len(), 1);

            let changed = cluster
                .apply_mesh_peer_event(&MeshPeerEvent::Left {
                    node_id: "peer-b".into(),
                })
                .unwrap();
            assert!(changed);
            assert!(cluster.get_peer("peer-b").is_none());
            assert_eq!(cluster.len(), 0);
        }

        #[test]
        fn mesh_partition_marks_unreachable_then_recovery() {
            let cluster = make_cluster(ClusterConfig::default());
            cluster
                .apply_mesh_peer_event(&MeshPeerEvent::Joined {
                    node_id: "peer-p".into(),
                    address: Some("10.1.0.1:9489".into()),
                    platform: None,
                    verified: true,
                })
                .unwrap();

            cluster
                .apply_mesh_peer_event(&MeshPeerEvent::Unreachable {
                    node_id: "peer-p".into(),
                })
                .unwrap();
            assert_eq!(
                cluster.get_peer("peer-p").unwrap().state,
                NodeState::Unreachable
            );

            // Partition recovery: peer comes back.
            cluster
                .apply_mesh_peer_event(&MeshPeerEvent::Recovered {
                    node_id: "peer-p".into(),
                    address: Some("10.1.0.1:9489".into()),
                    verified: true,
                })
                .unwrap();
            let peer = cluster.get_peer("peer-p").unwrap();
            assert_eq!(peer.state, NodeState::Active);
            assert_eq!(cluster.len(), 1);
        }

        #[test]
        fn mesh_suspect_then_alive() {
            let cluster = make_cluster(ClusterConfig::default());
            cluster
                .apply_mesh_peer_event(&MeshPeerEvent::Joined {
                    node_id: "peer-s".into(),
                    address: None,
                    platform: None,
                    verified: true,
                })
                .unwrap();
            cluster
                .apply_mesh_peer_event(&MeshPeerEvent::Suspect {
                    node_id: "peer-s".into(),
                })
                .unwrap();
            assert_eq!(
                cluster.get_peer("peer-s").unwrap().state,
                NodeState::Suspect
            );
            cluster
                .apply_mesh_peer_event(&MeshPeerEvent::Alive {
                    node_id: "peer-s".into(),
                })
                .unwrap();
            assert_eq!(cluster.get_peer("peer-s").unwrap().state, NodeState::Active);
        }

        #[test]
        fn mesh_join_is_idempotent_upsert() {
            let cluster = make_cluster(ClusterConfig::default());
            for _ in 0..3 {
                cluster
                    .apply_mesh_peer_event(&MeshPeerEvent::Joined {
                        node_id: "peer-i".into(),
                        address: Some("1.2.3.4:9".into()),
                        platform: None,
                        verified: true,
                    })
                    .unwrap();
            }
            assert_eq!(cluster.len(), 1);
            assert_eq!(
                cluster.get_peer("peer-i").unwrap().state,
                NodeState::Active
            );
        }

        #[tokio::test]
        async fn mesh_event_bus_drives_membership_listener() {
            use crate::mesh_discovery::MeshPeerEventBus;

            let cluster = Arc::new(make_cluster(ClusterConfig::default()));
            let bus = MeshPeerEventBus::new();
            let rx = bus.subscribe();
            cluster.spawn_mesh_peer_listener(rx);

            bus.emit(MeshPeerEvent::Joined {
                node_id: "async-peer".into(),
                address: Some("127.0.0.1:1".into()),
                platform: None,
                verified: true,
            });
            // Allow the spawned task to apply the event.
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            assert!(cluster.get_peer("async-peer").is_some());

            bus.emit(MeshPeerEvent::Left {
                node_id: "async-peer".into(),
            });
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            assert!(cluster.get_peer("async-peer").is_none());
        }

        #[cfg(feature = "cluster")]
        #[tokio::test]
        async fn cluster_service_applies_mesh_join_leave_async() {
            use crate::cluster::ClusterService;
            use clawft_types::config::ClusterNetworkConfig;
            use ruvector_cluster::StaticDiscovery;

            let membership = Arc::new(make_cluster(ClusterConfig {
                node_id: "local".into(),
                ..Default::default()
            }));
            let svc = Arc::new(
                ClusterService::new(
                    ClusterNetworkConfig::default(),
                    "local".into(),
                    Box::new(StaticDiscovery::new(vec![])),
                    Arc::clone(&membership),
                )
                .expect("cluster service"),
            );

            svc.apply_mesh_peer_event_async(&MeshPeerEvent::Joined {
                node_id: "remote-1".into(),
                address: Some("127.0.0.1:9471".into()),
                platform: None,
                verified: true,
            })
            .await
            .unwrap();
            assert!(membership.get_peer("remote-1").is_some());
            assert!(svc.list_nodes().iter().any(|n| n.node_id == "remote-1"));

            svc.apply_mesh_peer_event_async(&MeshPeerEvent::Left {
                node_id: "remote-1".into(),
            })
            .await
            .unwrap();
            assert!(membership.get_peer("remote-1").is_none());
        }

        #[cfg(feature = "cluster")]
        #[tokio::test]
        async fn cluster_service_subscribes_to_mesh_stream() {
            use crate::cluster::ClusterService;
            use crate::mesh_discovery::MeshPeerEventBus;
            use clawft_types::config::ClusterNetworkConfig;
            use ruvector_cluster::StaticDiscovery;

            let membership = Arc::new(make_cluster(ClusterConfig {
                node_id: "coord".into(),
                ..Default::default()
            }));
            let svc = Arc::new(
                ClusterService::new(
                    ClusterNetworkConfig::default(),
                    "coord".into(),
                    Box::new(StaticDiscovery::new(vec![])),
                    Arc::clone(&membership),
                )
                .expect("cluster service"),
            );

            let bus = MeshPeerEventBus::new();
            svc.spawn_mesh_peer_listener(bus.subscribe());
            assert!(svc.mesh_listener_started());

            bus.emit(MeshPeerEvent::Joined {
                node_id: "stream-peer".into(),
                address: Some("10.0.0.9:9489".into()),
                platform: None,
                verified: true,
            });
            tokio::time::sleep(std::time::Duration::from_millis(80)).await;
            assert_eq!(
                membership.get_peer("stream-peer").unwrap().state,
                NodeState::Active
            );

            // Partition then recovery via bus.
            bus.emit(MeshPeerEvent::Unreachable {
                node_id: "stream-peer".into(),
            });
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            assert_eq!(
                membership.get_peer("stream-peer").unwrap().state,
                NodeState::Unreachable
            );

            bus.emit(MeshPeerEvent::Recovered {
                node_id: "stream-peer".into(),
                address: Some("10.0.0.9:9489".into()),
                verified: true,
            });
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            assert_eq!(
                membership.get_peer("stream-peer").unwrap().state,
                NodeState::Active
            );
        }
    }
}

#[cfg(test)]
#[cfg(any(feature = "mesh", feature = "exochain"))]
mod mesh_tests {
    use super::*;

    #[test]
    fn node_identity_unique_ids() {
        let id1 = NodeIdentity::generate();
        let id2 = NodeIdentity::generate();
        assert_ne!(id1.node_id(), id2.node_id());
    }

    #[test]
    fn node_identity_sign_verify() {
        use ed25519_dalek::Verifier;

        let identity = NodeIdentity::generate();
        let data = b"hello mesh";
        let sig = identity.sign(data);
        assert!(identity.public_key().verify(data, &sig).is_ok());
    }

    #[test]
    fn node_identity_id_is_32_hex_chars() {
        let identity = NodeIdentity::generate();
        let nid = identity.node_id();
        assert_eq!(nid.len(), 32, "node_id should be 32 hex chars (16 bytes)");
        assert!(
            nid.chars().all(|c| c.is_ascii_hexdigit()),
            "node_id must be hex: {nid}"
        );
    }
}
