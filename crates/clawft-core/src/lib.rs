//! # clawft-core
//!
//! Core engine for the clawft AI assistant framework.
//!
//! Contains the agent loop, message bus, session management, tool registry,
//! context builder, memory store, and the 6-stage pipeline system.
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
//! Source: <https://github.com/WeftOS/weftos>

// WEFT-397: native ⊥ browser — fail loud if both feature flags are enabled.
#[cfg(all(feature = "native", feature = "browser"))]
compile_error!(
    "features `native` and `browser` are mutually exclusive; \
     use --no-default-features --features browser for browser/WASM builds"
);

pub mod agent;
#[cfg(feature = "native")]
pub mod agent_bus;
pub mod agent_routing;
pub mod bootstrap;
/// Re-export so daemon wiring can name `BranchableMemory` without a direct
/// dependency on the cow crate (WEFT-616 Phase 2 late wiring).
#[cfg(feature = "rvf")]
pub use clawft_cow_memory;
pub mod bus;
pub mod chain_event;
pub mod clawft_md;
pub mod config_merge;
pub mod json_repair;
/// WEFT-604: unify local-LLM endpoint/model for daemon + agent + voice.
pub mod local_llm_bridge;
#[cfg(feature = "native")]
pub mod placement_hook;
pub mod pipeline;
// `planning` uses `tokio::time::{Instant, timeout}` directly. Until those
// callsites get a runtime abstraction, the module only compiles for native
// builds. Browser builds skip it (no consumers cross-crate; see audit
// 2026-04-28 in `.planning/reviews/0.7.0-release-gate/16-browser-wasm.md`).
#[cfg(feature = "native")]
pub mod planning;
pub mod routing_validation;
pub mod runtime;
pub mod observation_pack;
pub mod security;
pub mod session;
pub mod tools;
pub mod workspace;

#[cfg(feature = "vector-memory")]
pub mod embeddings;
#[cfg(feature = "vector-memory")]
pub mod intelligent_router;
#[cfg(feature = "vector-memory")]
pub mod policy_kernel;
#[cfg(feature = "vector-memory")]
pub mod session_indexer;
#[cfg(feature = "vector-memory")]
pub mod vector_store;

#[cfg(feature = "rvf")]
pub mod complexity;
#[cfg(feature = "rvf")]
pub mod memory_bootstrap;
#[cfg(feature = "rvf")]
pub mod scoring;
