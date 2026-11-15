//! Deployment topology, role and configuration types (ADR-054 / WEFT-524).

use serde::{Deserialize, Serialize};

/// Default lease length (ms) a hot standby waits for a primary heartbeat.
pub const DEFAULT_LEASE_MS: u64 = 3000;

fn default_lease_ms() -> u64 {
    DEFAULT_LEASE_MS
}

/// Three deployment topologies for WorldModelService (ADR-054 / WEFT-524).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DeploymentTopology {
    /// One WorldModelService for the entire fleet (1–10 nodes).
    #[default]
    Single,
    /// Active primary with hot standby (10–500 nodes). Lease failover only
    /// (no consensus crate in the workspace); see [`crate::replica`].
    HotStandby,
    /// Every node runs its own service; peer latents are observed only.
    PeerToPeer,
}

impl DeploymentTopology {
    /// Parse from CLI / config string (`single`, `hot_standby`, `standby`, `p2p`, `peer_to_peer`).
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "single" | "primary" => Some(Self::Single),
            "hot_standby" | "standby" | "raft" | "hot-standby" => Some(Self::HotStandby),
            "peer_to_peer" | "p2p" | "peer-to-peer" | "mesh" => Some(Self::PeerToPeer),
            _ => None,
        }
    }

    /// Stable config / wire name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Single => "single",
            Self::HotStandby => "hot_standby",
            Self::PeerToPeer => "peer_to_peer",
        }
    }

    /// Human label for logs.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Single => "single primary",
            Self::HotStandby => "raft-elected primary + standby",
            Self::PeerToPeer => "peer-to-peer mesh",
        }
    }
}

/// Role within a topology (single always Primary).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ServiceRole {
    /// Authoritative writer for the cluster / process.
    #[default]
    Primary,
    /// Hot standby replaying the primary's ExoChain (hot_standby only).
    Standby,
    /// Peer participant (peer_to_peer topology).
    Peer,
}

impl ServiceRole {
    /// Stable wire name (used in `lewm.role_change` events).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Standby => "standby",
            Self::Peer => "peer",
        }
    }
}

/// Service configuration selectable via CLI / env / file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceConfig {
    /// Deployment topology.
    pub topology: DeploymentTopology,
    /// Role within the topology.
    pub role: ServiceRole,
    /// Optional node / peer identifier for multi-node scaffolds.
    pub node_id: String,
    /// When true, run the fake sensor smoke loop after boot.
    pub smoke_frames: u64,
    /// Heartbeat lease (ms) a standby grants the primary before it may promote.
    #[serde(default = "default_lease_ms")]
    pub lease_ms: u64,
}

impl Default for ServiceConfig {
    fn default() -> Self {
        Self {
            topology: DeploymentTopology::Single,
            role: ServiceRole::Primary,
            node_id: "wm-0".into(),
            smoke_frames: 0,
            lease_ms: DEFAULT_LEASE_MS,
        }
    }
}

impl ServiceConfig {
    /// Build config from topology, applying default role for that topology.
    pub fn for_topology(topology: DeploymentTopology) -> Self {
        let role = match topology {
            DeploymentTopology::Single => ServiceRole::Primary,
            DeploymentTopology::HotStandby => ServiceRole::Primary,
            DeploymentTopology::PeerToPeer => ServiceRole::Peer,
        };
        Self {
            topology,
            role,
            ..Self::default()
        }
    }

    /// Validate topology × role combinations (soft rules for the scaffold).
    pub fn validate(&self) -> Result<(), String> {
        match (self.topology, self.role) {
            (DeploymentTopology::Single, ServiceRole::Primary) => Ok(()),
            (DeploymentTopology::Single, _) => Err(
                "single topology requires role=primary (standby/peer not valid)".into(),
            ),
            (DeploymentTopology::HotStandby, ServiceRole::Primary | ServiceRole::Standby) => Ok(()),
            (DeploymentTopology::HotStandby, ServiceRole::Peer) => {
                Err("hot_standby does not use peer role; use primary or standby".into())
            }
            (DeploymentTopology::PeerToPeer, ServiceRole::Peer | ServiceRole::Primary) => Ok(()),
            (DeploymentTopology::PeerToPeer, ServiceRole::Standby) => {
                Err("peer_to_peer does not use standby role; use peer".into())
            }
        }
    }
}
