//! # clawft-types
//!
//! Core type definitions for the clawft AI assistant framework.
//!
//! This crate is the foundation of the dependency graph -- all other
//! clawft crates depend on it. It contains:
//!
//! - **[`error`]** -- [`ClawftError`] and [`ChannelError`] error types
//! - **[`config`]** -- Configuration schema (ported from Python `schema.py`)
//! - **[`event`]** -- Inbound/outbound message events
//! - **[`provider`]** -- LLM response types and the 15-provider registry
//! - **[`session`]** -- Conversation session state
//! - **[`cron`]** -- Scheduled job types
//!
//! ## Crate Ecosystem
//!
//! WeftOS is built from these crates:
//!
//! | Crate | Role |
//! |-------|------|
//! | [`weftos`](https://crates.io/crates/weftos) | Product facade -- re-exports kernel, core, types |
//! | [`clawft-kernel`](https://crates.io/crates/clawft-kernel) | Kernel: processes, services, governance, mesh, ExoChain |
//! | [`clawft-core`](https://crates.io/crates/clawft-core) | Agent framework: pipeline, context, tools, skills |
//! | [`clawft-types`](https://crates.io/crates/clawft-types) | Shared type definitions |
//! | [`clawft-platform`](https://crates.io/crates/clawft-platform) | Platform abstraction (native/WASM/browser) |
//! | [`clawft-plugin`](https://crates.io/crates/clawft-plugin) | Plugin SDK for tools, channels, and extensions |
//! | [`clawft-llm`](https://crates.io/crates/clawft-llm) | LLM provider abstraction (11 providers + local) |
//! | [`exo-resource-tree`](https://crates.io/crates/exo-resource-tree) | Hierarchical resource namespace with Merkle integrity |
//!
//! Source: <https://github.com/weave-logic-ai/weftos>

// WEFT-397: native ⊥ browser — fail loud if both feature flags are enabled.
#[cfg(all(feature = "native", feature = "browser"))]
compile_error!(
    "features `native` and `browser` are mutually exclusive; \
     use --no-default-features --features browser for browser/WASM builds"
);

pub mod agent_bus;
pub mod agent_chat;
pub mod agent_routing;
/// SC-2 secure PCM buffers (WEFT-223) — zeroize-on-drop raw audio holders.
pub mod audio_buffer;
/// Multimodal turn content + voice chat metadata keys (WEFT-350).
pub mod turn_content;
pub mod canvas;
pub mod cog_topics;
pub mod company;
pub mod config;
pub mod cron;
pub mod delegation;
pub mod error;
pub mod event;
pub mod goal;
pub mod placement;
/// Project identity model (ADR-103, Phase 1 package B). Native only (ULID/fs).
#[cfg(feature = "native")]
pub mod project;
pub mod provider;
pub mod registry;
pub mod routing;
pub mod runtime_paths;
pub mod secret;
pub mod security;
pub mod session;
pub mod skill;
/// WindowIntent bus (ADR-073 Phase D / WEFT-688) — shared by GUI + MCP.
pub mod window_intent;
pub mod workspace;

pub use audio_buffer::{
    zeroize_samples, zeroize_vec, SecureAudioBuffer, SecureAudioRing,
};
pub use error::{ChannelError, ClawftError, Result};
pub use registry::Registry;
pub use turn_content::{
    audio_from_metadata, AudioRef, TurnContent, TurnContentPart,
};
