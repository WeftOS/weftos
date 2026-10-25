//! cogbus: the cog data plane broker of the node daemon (ADR-122).
//!
//! A placed cog finds the broker through two environment variables the
//! workload runtime injects, [`ENV_SOCKET`] and [`ENV_TOKEN`], and talks to
//! it over a Unix socket with length-prefixed JSON frames ([`proto`]).
//!
//! - [`proto`]: requests, responses, deliveries and the frame codec.
//! - [`broker`]: the transport-independent core: instances registered at
//!   place time, topics `<kind>/<source_id>`, subscriptions with bounded
//!   per-subscriber queues, and the publish/subscribe rules.
//! - [`server`]: the Unix socket listener (peer uid check, hello first).
//! - [`client`]: the async client a cog links.
//! - [`policy`]: what the daemon plugs in (B3): governance on top of the
//!   manifest rules, cross-project grants, the activity sink.
//! - [`audit`]: governance calls, the refusal rate limit and the
//!   repeated-refusal stop.
//!
//! The broker carries `reading` topics (B1), the daemon's built-in sinks
//! and internal publishers (B4: the ingest compatibility listener publishes
//! `store.vector`, the store sink takes `store.vector/*`; see
//! [`Broker::add_sink`] and [`Broker::publish_internal`]), and governance,
//! grants and the activity log (B3). Stream rings (B2), blobs (B5) and mesh
//! topics (B6) are other phases.

pub mod audit;
pub mod broker;
#[cfg(unix)]
pub mod client;
pub mod policy;
pub mod proto;
#[cfg(unix)]
pub mod server;

mod queue;
mod types;
#[cfg(test)]
mod tests_broker;
#[cfg(test)]
mod tests_policy;
#[cfg(test)]
mod tests_proto;
#[cfg(test)]
mod tests_sink;

pub use broker::{
    Broker, BrokerConfig, BusError, InstanceSpec, Reading, Session, SinkReading, TopicRow,
};
#[cfg(unix)]
pub use client::{Client, ClientError, Subscription};
pub use policy::{
    BusAction, BusActivity, BusHooks, BusPolicy, BusRequest, BusSink, Grant, Outcome, RefusalLimits,
};
pub use proto::{Delivery, ErrorCode, Event, InputSpec, OutputSpec, Request};

/// Env var: path of the broker socket.
pub const ENV_SOCKET: &str = "WEFTOS_COGBUS";
/// Env var: the instance's broker token.
pub const ENV_TOKEN: &str = "WEFTOS_COGBUS_TOKEN";
/// Socket file name in the daemon's runtime dir.
pub const SOCKET_NAME: &str = "cogbus.sock";
