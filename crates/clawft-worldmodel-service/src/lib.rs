//! `clawft-worldmodel-service` — LeWM WorldModelService host (WEFT-524).
//!
//! Boots a [`weftos_worldmodel::DefaultWorldModel`] facade under one of three
//! deployment topologies (ADR-054 marketing contract):
//!
//! - [`DeploymentTopology::Single`] — one primary for the fleet (default)
//! - [`DeploymentTopology::HotStandby`] — primary plus a standby that replays
//!   its chain and promotes on a missed lease (a lease stand-in, not Raft)
//! - [`DeploymentTopology::PeerToPeer`] — every node runs its own service and
//!   may observe peer latents (observation only, ADR-090)
//!
//! Single-node mode boots end-to-end against a fake sensor pipeline and
//! attests every frame onto an in-memory ExoChain sink (WEFT-533). Standby
//! replay, lease failover and peer consensus are proven in-process only; real
//! election, network heartbeats, chain transport and `mesh.sensor.v1` wire
//! publishing are not implemented.
//!
//! Modules: [`topology`] (config), [`service`] (frame loop, planner),
//! [`replica`] (standby replay + lease), [`peers`] (peer latents).

#![deny(missing_docs)]

pub mod peers;
pub mod replica;
pub mod service;
pub mod topology;

pub use peers::{latent_to_le_bytes, PeerError, PeerLatent, PeerLatents};
pub use replica::{LeaseTracker, ReplayOutcome, EVENT_KIND_ROLE_CHANGE};
pub use service::{
    BootReport, FrameResult, ServiceError, WorldModelService, PLANNER_HZ, PLANNER_PERIOD_MS,
};
pub use topology::{
    DeploymentTopology, ServiceConfig, ServiceRole, DEFAULT_LEASE_MS,
};

#[cfg(test)]
mod tests;
