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
//! - [`stream`]: `stream` topics as shared-memory rings (phase B2).
//!
//! Phases B1 and B2 carry `reading` and `stream` topics. Governance
//! actions and the activity log (B3), the ingest compatibility listener
//! (B4), blobs (B5) and mesh topics (B6) are later phases.

pub mod broker;
#[cfg(unix)]
pub mod client;
pub mod proto;
#[cfg(unix)]
pub mod server;
pub mod stream;

mod queue;
mod types;
#[cfg(test)]
mod tests_broker;
#[cfg(test)]
mod tests_proto;

pub use broker::{Broker, BrokerConfig, BusError, InstanceSpec, Session, TopicRow};
#[cfg(unix)]
pub use client::{Client, ClientError, Subscription};
pub use proto::{Delivery, ErrorCode, Event, InputSpec, OutputSpec, Request};
pub use stream::{StreamConfig, StreamNotice, StreamRecord};
#[cfg(unix)]
pub use stream::{StreamOptions, StreamReader, StreamWriter};

/// Env var: path of the broker socket.
pub const ENV_SOCKET: &str = "WEFTOS_COGBUS";
/// Env var: the instance's broker token.
pub const ENV_TOKEN: &str = "WEFTOS_COGBUS_TOKEN";
/// Socket file name in the daemon's runtime dir.
pub const SOCKET_NAME: &str = "cogbus.sock";
