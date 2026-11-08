//! WeftOS-side computer hub.
//!
//! The hub is the WebSocket rendezvous that Tapestry's `workspace-server`
//! (role `tool_server`) and harness (role `harness`) both connect *through*.
//! It speaks the `xai-tool-protocol` wire format, authenticates every
//! connection with a WeftOS-minted pass key ([`passkey`]), and relays tool
//! calls between a harness session and the tool server bound to it
//! ([`hub`]). It never dials out.

pub mod config;
pub mod hub;
pub mod passkey;
mod relay;
pub mod server;
pub mod supervision;
pub mod wire;

pub use config::{HubConfig, Machine, Minter};
pub use hub::Hub;
pub use passkey::{Claims, PassKeyError, Role};
