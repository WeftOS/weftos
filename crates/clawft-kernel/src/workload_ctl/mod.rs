//! Placement control plane (ADR-099 sections 3 and 7; card
//! mesh-placement-12): the pure engine (card 04) wired to the mesh.
//!
//! - [`facts`]: card 03's cached, verified [`NodeFacts`] as card 04's
//!   `PlacementFacts` (liveness from membership or direct contact,
//!   receiver-assigned trust tier, load from live state, measured `perf.*`).
//! - [`cog_kind`]: the cog kind's requirements (ADR-100 section 2).
//! - [`msg`]: the signed `workload.ctl` message set (nonce, expiry,
//!   decision id; unknown methods denied).
//! - [`session`]: the message set on the wire as `MeshIpcEnvelope` /
//!   `MeshRequest`, with the artifact piece protocol (card 11) multiplexed
//!   for fetch-before-load.
//! - [`host_service`]: a node's `workload-host` (advertised through
//!   `ServiceAdvertisement`), driving card 09's adapters under governance.
//! - [`plane`] / [`plane_place`]: the controller: learn targets, decide,
//!   chain, dispatch, retry the next candidate on refusal.
//! - [`transport`]: mesh TCP (Noise XX) and in-process connections; the
//!   in-process path serves this node's own host and Seed adapters on an
//!   operator-assigned node id.
//!
//! [`NodeFacts`]: clawft_types::placement::NodeFacts

pub mod cog_kind;
pub mod device_owner;
mod device_sys;
pub mod host_cogbus;
pub mod host_devices;
pub mod facts;
mod host_instances;
mod host_journal;
pub mod host_licence;
pub mod host_node_admin;
pub mod host_revoke;
pub mod host_service;
pub mod host_supervise;
pub mod lifecycle;
pub mod msg;
pub mod plane;
pub mod plane_node_admin;
pub mod plane_peers;
pub mod plane_session;
pub mod plane_lifecycle;
pub mod plane_place;
mod plane_reschedule;
mod plane_prepare;
mod plane_reconcile;
pub mod plane_seed;
mod plane_state;
pub use crate::refusal_budget;
pub use host_journal::{JournalEntry, RestoreReport};
pub mod session;
pub mod transport;

#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests_facts;
#[cfg(test)]
mod tests_flood;
#[cfg(test)]
mod tests_cogbus;
#[cfg(test)]
mod tests_device_owner;
#[cfg(test)]
mod tests_ingest;
#[cfg(test)]
mod tests_interfaces;
#[cfg(test)]
mod tests_kind;
#[cfg(test)]
mod tests_kind_followups;
#[cfg(test)]
mod tests_life;
#[cfg(test)]
mod tests_life_fix;
#[cfg(test)]
mod tests_life_node;
#[cfg(test)]
mod tests_life_plane;
#[cfg(test)]
mod tests_life_revoke;
#[cfg(test)]
mod tests_link;
#[cfg(test)]
mod tests_peers;
#[cfg(test)]
mod tests_reconcile;
#[cfg(test)]
mod tests_restart;
#[cfg(test)]
mod tests_revoke;
#[cfg(test)]
mod tests_seed;
#[cfg(test)]
mod tests_state;
#[cfg(test)]
mod tests_teardown;
#[cfg(test)]
mod tests_two_node;
#[cfg(test)]
mod tests_licence;
#[cfg(test)]
mod tests_node_admin;

pub use cog_kind::cog_workload_spec;
pub use facts::{LiveNodeFacts, engine_tier, governance_tier, liveness_of, placement_view};
pub use host_node_admin::NodeAdmin;
pub use host_revoke::ForcedTeardown;
pub use host_supervise::Supervised;
pub use lifecycle::{
    InstanceLife, LifecyclePolicy, LifecycleState, RestartDecision, RestartPolicy, can_transition,
};
pub use host_service::{CtlConfig, FactsSource, HOST_CHAIN_SOURCE, WorkloadHostService};
pub use msg::{
    CtlRequest, CtlResponse, NonceGuard, Refusal, RefusalCode, SignedCtl, WORKLOAD_HOST_SERVICE,
};
pub use plane::{
    CallFailure, PLANE_CHAIN_SOURCE, PlacementControlPlane, PlacementRecord, PlaneConfig,
    PlaneError, TargetInfo,
};
pub use plane_peers::OperatorPeer;
pub use plane_session::CtlSession;
pub use plane_place::{Attempt, PlaceOrder, PlaceReport, render};
pub use plane_seed::{SEED_ROUTE, StorePinOrder};
pub use refusal_budget::RefusalBudget;
pub use transport::{
    CtlConnector, MAX_SESSIONS, MEM_SCHEME, MeshConnector, listen_tcp, serve_listener,
    serve_listener_with,
};
