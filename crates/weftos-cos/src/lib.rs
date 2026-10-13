//! Chief-of-staff agent host (ADR-118 §3, lane C of the console contract).
//!
//! What lives here:
//! - [`conversation`]: the in-process [`Conversation`] trait and the event
//!   shapes the console host consumes (contract §3);
//! - [`agent`]: the turn loop with tool gating and a confirmation turn before
//!   anything outward-facing;
//! - [`routing`]: quick vs plan model selection on top of WeftOS's existing
//!   provider routing;
//! - [`tools`]: `projects.activity`, `board.propose`, `notes.record`,
//!   `work.kickoff`, `nodes.health`;
//! - [`server`]: the loopback HTTP/SSE server on `127.0.0.1:18300`;
//! - [`voice`]: push-to-talk capture from the default input, local STT, and
//!   the post as `{text, source: "voice"}` (contract §4).
//!
//! Secrets never cross the event stream: tool events carry name, summary and
//! status only, and every outbound event passes through [`events::redact`].

pub mod agent;
pub mod brain;
pub mod bridge;
pub mod cli;
pub mod config;
pub mod endpoints;
pub mod conversation;
pub mod events;
pub mod glance;
pub mod guard;
pub mod guide;
pub mod host;
pub mod mcp_catalog;
pub mod mcp_server;
pub mod model;
pub mod prompt;
pub mod routing;
pub mod server;
pub mod speak;
pub mod tools;
pub mod traces;
pub mod voice;

pub use agent::CosAgent;
pub use config::CosConfig;
pub use conversation::{AnswerKind, Conversation, GuideAnswer, Source, TurnId, UserUtterance};
pub use events::{AgentState, Event};

/// Errors surfaced by the agent, its tools and the voice path.
#[derive(Debug, thiserror::Error)]
pub enum CosError {
    /// Configuration or credential problem (bad bind address, token file
    /// permissions, missing staging directory).
    #[error("config: {0}")]
    Config(String),
    /// A tool failed; the message is already redacted.
    #[error("tool {name}: {reason}")]
    Tool { name: String, reason: String },
    /// The model call failed.
    #[error("model: {0}")]
    Model(String),
    /// Microphone capture or STT failed.
    #[error("voice: {0}")]
    Voice(String),
    /// The conversation is shut down.
    #[error("conversation closed")]
    Closed,
}
