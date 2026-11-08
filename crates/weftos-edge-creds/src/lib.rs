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

#![allow(async_fn_in_trait)]

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
pub mod mock;

#[cfg(any(test, feature = "loopback-chain"))]
pub mod loopback;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_live;
#[cfg(test)]
mod tests_loopback;
