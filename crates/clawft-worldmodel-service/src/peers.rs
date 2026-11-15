//! Peer latents and consensus as observation only (ADR-090 R3 / R5).
//!
//! In [`DeploymentTopology::PeerToPeer`] a node may receive latents from
//! peers. They are collected per peer (last write wins), reduced to a
//! consensus latent (per-dimension median or mean) and handed to the lattice
//! through `observe_node`. They never reach the predictor, planner, SIGReg
//! monitor, gate or chain, so a node's own behaviour is identical with and
//! without peers (R2 / R5). Transport is out of scope: callers push latents
//! in-process.

use std::collections::BTreeMap;

use weftos_worldmodel::{LatticeApi, Latent, NodeId, ObservationFrame, LATENT_DIM, LATENT_DIM_U16};

use crate::service::{ServiceError, WorldModelService};
use crate::topology::DeploymentTopology;

/// Reasons a peer latent is rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerError {
    /// Latent length is not [`LATENT_DIM`].
    Dim {
        /// Expected length.
        expected: usize,
        /// Length received.
        got: usize,
    },
    /// Latent contains NaN or infinity.
    NonFinite,
}

impl core::fmt::Display for PeerError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Dim { expected, got } => {
                write!(f, "peer latent length: expected {expected}, got {got}")
            }
            Self::NonFinite => write!(f, "peer latent contains a non-finite value"),
        }
    }
}

impl std::error::Error for PeerError {}

/// Latest latent received from one peer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PeerLatent {
    /// Peer that produced the latent.
    pub node: NodeId,
    /// Latent value.
    pub latent: Latent,
    /// Timestamp (ms) supplied by the caller.
    pub timestamp_ms: u64,
}

/// Per-peer latest latents (last write wins per node).
#[derive(Debug, Clone, Default)]
pub struct PeerLatents {
    peers: BTreeMap<u64, PeerLatent>,
}

impl PeerLatents {
    /// Empty collection.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a peer latent, replacing any earlier one from the same node.
    pub fn ingest(
        &mut self,
        node: NodeId,
        latent: &[f32],
        timestamp_ms: u64,
    ) -> Result<(), PeerError> {
        if latent.len() != LATENT_DIM {
            return Err(PeerError::Dim {
                expected: LATENT_DIM,
                got: latent.len(),
            });
        }
        if latent.iter().any(|v| !v.is_finite()) {
            return Err(PeerError::NonFinite);
        }
        let mut z = [0.0f32; LATENT_DIM];
        z.copy_from_slice(latent);
        self.peers.insert(
            node.0,
            PeerLatent {
                node,
                latent: z,
                timestamp_ms,
            },
        );
        Ok(())
    }

    /// Number of distinct peers seen.
    pub fn len(&self) -> usize {
        self.peers.len()
    }

    /// True when no peer has reported.
    pub fn is_empty(&self) -> bool {
        self.peers.is_empty()
    }

    /// Latest latent for one peer.
    pub fn get(&self, node: NodeId) -> Option<&PeerLatent> {
        self.peers.get(&node.0)
    }

    /// Per-dimension median across peers (mean of the two middle values for
    /// an even count). `None` when empty. Robust to a single bad peer.
    pub fn consensus_median(&self) -> Option<Latent> {
        if self.peers.is_empty() {
            return None;
        }
        let mut out = [0.0f32; LATENT_DIM];
        let mut col: Vec<f32> = Vec::with_capacity(self.peers.len());
        for (d, slot) in out.iter_mut().enumerate() {
            col.clear();
            col.extend(self.peers.values().map(|p| p.latent[d]));
            col.sort_by(f32::total_cmp);
            let n = col.len();
            *slot = if n % 2 == 1 {
                col[n / 2]
            } else {
                (col[n / 2 - 1] + col[n / 2]) / 2.0
            };
        }
        Some(out)
    }

    /// Per-dimension mean across peers. `None` when empty.
    pub fn consensus_mean(&self) -> Option<Latent> {
        if self.peers.is_empty() {
            return None;
        }
        let n = self.peers.len() as f32;
        let mut out = [0.0f32; LATENT_DIM];
        for p in self.peers.values() {
            for (o, v) in out.iter_mut().zip(p.latent.iter()) {
                *o += v;
            }
        }
        for o in out.iter_mut() {
            *o /= n;
        }
        Some(out)
    }
}

/// Encode a latent as little-endian f32 bytes (observation frame payload).
pub fn latent_to_le_bytes(z: &Latent) -> Vec<u8> {
    z.iter().flat_map(|v| v.to_le_bytes()).collect()
}

impl WorldModelService {
    /// Ingest a peer's latent (PeerToPeer topology only).
    ///
    /// Updates the peer table, then feeds the median consensus through
    /// `lattice.observe_node`. Does not touch predictor, planner, SIGReg,
    /// gate or chain (ADR-090 R3 / R5).
    pub fn ingest_peer_latent(
        &mut self,
        node: NodeId,
        latent: &[f32],
        timestamp_ms: u64,
    ) -> Result<(), ServiceError> {
        if self.config.topology != DeploymentTopology::PeerToPeer {
            return Err(ServiceError::Topology(format!(
                "peer latents require peer_to_peer topology, not {}",
                self.config.topology.as_str()
            )));
        }
        if !self.booted {
            return Err(ServiceError::NotBooted);
        }
        self.peers
            .ingest(node, latent, timestamp_ms)
            .map_err(ServiceError::Peer)?;
        self.peers_seen = self.peers.len() as u64;

        if let Some(consensus) = self.peers.consensus_median() {
            let bytes = latent_to_le_bytes(&consensus);
            self.model
                .lattice
                .observe_node(
                    node,
                    ObservationFrame {
                        bytes: &bytes,
                        latent_dim: LATENT_DIM_U16,
                        timestamp_ms,
                    },
                )
                .map_err(ServiceError::WorldModel)?;
            self.last_consensus = Some(consensus);
        }
        Ok(())
    }
}
