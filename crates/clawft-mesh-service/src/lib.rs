//! WeftOS machine mesh service building blocks (ADR-103, Phase 3).
//!
//! Package J provides the machine journal ([`journal`]) and the bindings
//! folded from it ([`bindings`]). Package S is the service itself: the
//! mesh-local server ([`local_server`]), registrations ([`registry`]), tenant
//! routing ([`router`]), cluster-owner verdicts ([`verdicts`]), the admission
//! gate ([`gate`]), signed facts ([`facts`]), the licence control topics to
//! the owner's daemon ([`licence_forward`]), the loopback health endpoint
//! ([`health`]) and start-up ([`main_loop`]).
//!
//! # Must not own
//!
//! The service constructs no `Kernel`, `ChainManager`, `GateBackend`, token or
//! secret type. The kernel is used only for its mesh modules, built without
//! `exochain`, `cluster`, `tilezero` or `ecc`; `scripts/build.sh
//! check-mesh-no-owned-state` enforces that on the dependency tree. No
//! mesh-local handler evaluates governance (verdicts are forwarded to the
//! cluster-owner daemon and cached), and a test lists the state directory
//! after a session against the plan 1.1 allow-list.

// The `testing` feature adds a write-failure seam; it must never ship.
#[cfg(all(feature = "testing", not(debug_assertions)))]
compile_error!("clawft-mesh-service: the `testing` feature must not be enabled in release builds");

mod bind_events;
pub mod bindings;
mod bindings_view;
mod chain;
pub mod config;
pub mod facts;
pub mod force_revoked;
mod fsutil;
pub mod gate;
pub mod journal;
pub mod licence_forward;
pub mod limits;
mod lost;
pub mod registry;
pub mod router;
pub mod state;
pub mod verdicts;

#[cfg(unix)]
mod admin;
#[cfg(unix)]
pub mod admin_client;
#[cfg(unix)]
mod handlers;
#[cfg(unix)]
pub mod health;
#[cfg(unix)]
pub mod local_server;
#[cfg(unix)]
pub mod main_loop;
#[cfg(unix)]
mod register;

pub use chain::{verify_dir, VerifyReport};
pub use bindings::{BindError, BindHow, BindMeta, Bindings, Check, ConflictReason};
pub use journal::{AdminAck, Head, Journal, JournalError, JournalOptions, Record};
pub use lost::LostInfo;
pub use config::{BindPolicy, ConfigError, MeshServiceConfig, Overrides};
pub use state::ServiceState;
#[cfg(unix)]
pub use main_loop::{run, start, wait_for_signal, RunningService, StartError};
#[cfg(all(unix, feature = "testing"))]
pub use main_loop::start_with;

#[cfg(test)]
mod tests_internal;
