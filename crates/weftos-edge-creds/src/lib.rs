//! ADR-126 S3: edge and Wasm nodes reach their node bucket through the parent
//! daemon with short-lived credentials, and never hold a long-lived key.
//!
//! Two halves, joined by [`wire::ParentLink`]:
//!
//! - [`broker::EdgeBroker`] runs in the parent daemon. Given a node the link
//!   has authenticated, it checks the revoked set, the rate limit and the
//!   chain-recorded grant, derives the bucket from the node id, and asks RustFS
//!   (via [`broker::StsMinter`]) for an STS session scoped to that bucket, or
//!   for a one-object session behind a presigned URL.
//! - [`edge::EdgeNodeStore`] runs on the node (native or `wasm32`). It holds no
//!   key, takes a lease when it needs one, signs with SigV4 and the session
//!   token, refreshes before expiry, and refuses credentials with no session
//!   token or a lifetime above the 15-minute cap.
//!
//! The crate does no I/O and reads no clock: the host supplies
//! [`wire::ObjectTransport`], [`wire::ParentLink`] and [`clock::Clock`], so it
//! builds for `wasm32-unknown-unknown`. S1's `weftos-store` owns bucket
//! naming, the control-plane client and grants; this crate depends on none of
//! it and is wired to it through [`broker::StsMinter`] and
//! [`broker::GrantCheck`]. The live implementations are [`rustfs::RustfsMinter`]
//! (STS AssumeRole and admin user status over the host's HTTP client) and
//! [`grants::GrantLedger`] (chain grant events folded into a table).

// The traits below are implemented by the host in the same build (the parent
// daemon or the edge tab), so the caller sees the concrete future. The futures
// are not required to be `Send`: the edge side runs on a single-threaded
// browser executor, and a `Send` bound there would exclude `fetch`-based
// transports. Daemon hosts that spawn broker futures need a `Send` implementor.
#![allow(async_fn_in_trait, reason = "single-threaded edge transports cannot promise Send futures")]

pub mod authchain;
pub mod broker;
pub mod clock;
pub mod compaction;
pub mod creds;
pub mod edge;
pub mod grants;
pub mod keygen;
pub mod lifecycle;
pub mod policy;
pub mod rustfs;
pub mod sigv4;
pub mod wire;

#[cfg(any(test, feature = "mock"))]
#[allow(missing_docs, reason = "test-util stand-in, not part of the supported surface")]
pub mod mock;

#[cfg(any(test, feature = "loopback-chain"))]
#[allow(missing_docs, reason = "spike stand-in for the chain, not part of the supported surface")]
pub mod loopback;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_live;
#[cfg(test)]
mod tests_loopback;
#[cfg(test)]
mod tests_authchain;
#[cfg(test)]
mod tests_broker_state;
#[cfg(test)]
mod tests_keys;
