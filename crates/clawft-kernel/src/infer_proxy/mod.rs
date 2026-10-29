//! Stable inference address, proxy and resolution (card mesh-placement-19;
//! ADR-101 section 5).
//!
//! - [`table`]: the [`PlacementTable`], role to serving node, fed by the
//!   card-18 adapters (local) and `infer.<role>` [`ServiceAdvertisement`]s
//!   of admitted peers (remote). Resolution for in-process consumers
//!   (`base_url_for_role`, wired to `clawft_llm::PlacementResolver`).
//! - [`listener`]: [`InferProxy`], the loopback endpoint proxy that keeps
//!   `127.0.0.1:<role port>` stable while the instance moves.
//! - [`mesh_forward`] / [`wire`]: forwarding over the authenticated mesh
//!   (frame types `InferRequest` / `InferResponse`).
//! - [`http`] / [`upstream`]: the HTTP edge and the loopback client.
//!
//! The proxy is not a general proxy. It binds loopback only, takes the
//! destination from the placement table and never from the request, allows
//! GET and POST on an inference path allowlist, refuses a non-loopback
//! `Host` and a cross-origin `Origin`, and bounds sizes and times
//! ([`ProxyLimits`]). Mesh forwarding reaches only admitted, verified peers
//! and only roles a node chose to expose.
//!
//! [`ServiceAdvertisement`]: crate::mesh_service_adv::ServiceAdvertisement

pub mod http;
pub mod hub;
pub mod listener;
pub mod svc;
pub mod mesh_forward;
pub mod mesh_policy;
pub mod table;
pub mod types;
pub mod upstream;
pub mod wire;

#[cfg(test)]
mod support;
#[cfg(test)]
mod tests_expose;
#[cfg(test)]
mod tests_exposure;
#[cfg(test)]
mod tests_http;
#[cfg(test)]
mod tests_hub;
#[cfg(test)]
mod tests_mesh;
#[cfg(test)]
mod tests_mesh_serve;
#[cfg(test)]
mod tests_proxy;
#[cfg(test)]
mod tests_svc;
#[cfg(test)]
mod tests_advert;

#[cfg(feature = "exochain")]
pub use types::ChainAudit;
pub use hub::InferHub;
pub use listener::{ExposureAuth, ExposurePermit, InferProxy, OccupiedPolicy, ProxyStats, Started, check_bind};
pub use mesh_forward::{InferPeer, ServeGate, ServePolicy, Served, forward_remote, serve_infer};
pub use table::{DEFAULT_ADVERT_TTL, MeshLocal, PlacementTable, SERVICE_PREFIX, SyncOutcome};
pub use types::{
    MeshDialer, Method, ProxyAudit, ProxyError, ProxyLimits, ProxyRequest, ResponseSink, Target,
};
pub use svc::{SvcBackend, SvcImage, SvcRoute};
pub use upstream::Upstream;
