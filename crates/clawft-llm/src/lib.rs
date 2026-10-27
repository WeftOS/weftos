//! LLM provider abstraction for clawft.
//!
//! This crate provides a unified interface for calling LLM APIs via
//! OpenAI-compatible endpoints. It is a standalone library with no
//! dependencies on other clawft crates.
//!
//! # Architecture
//!
//! - [`Provider`] trait defines the chat completion interface
//! - [`OpenAiCompatProvider`] implements it for any OpenAI-compatible API
//! - [`ProviderRouter`] routes model names (e.g. "openai/gpt-4o") to providers
//! - [`LlmProviderConfig`] describes how to connect to a provider
//!
//! # Quick Start
//!
//! ```rust,ignore
//! use clawft_llm::{ProviderRouter, ChatRequest, ChatMessage};
//!
//! let router = ProviderRouter::with_builtins();
//! let (provider, model_name) = router.route("openai/gpt-4o").unwrap();
//!
//! let request = ChatRequest::new(model_name, vec![
//!     ChatMessage::system("You are a helpful assistant."),
//!     ChatMessage::user("What is Rust?"),
//! ]);
//!
//! let response = provider.complete(&request).await?;
//! println!("{}", response.choices[0].message.content);
//! ```
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

pub mod config;
pub mod error;
pub mod sse;
pub mod types;

#[cfg(feature = "native")]
pub mod failover;
// Pure Hermes dialect helpers (`strip_think`, tool-call recovery, request
// knobs). No I/O — available under browser/WASM as well as native so shared
// pipeline code (e.g. clawft-core transport) can strip `<think>` without a
// native-only cfg (WEFT-672).
pub mod hermes;
#[cfg(feature = "native")]
pub mod local_provider;
#[cfg(feature = "native")]
pub mod openai_compat;
#[cfg(feature = "native")]
pub mod placement;
#[cfg(feature = "native")]
pub mod provider;
#[cfg(feature = "native")]
pub mod retry;
#[cfg(feature = "native")]
pub mod router;

#[cfg(feature = "native")]
pub mod eml_retry;
#[cfg(feature = "native")]
pub use eml_retry::{retry_model_path, RetryModel, RETRY_MODEL_FILENAME};

#[cfg(feature = "browser")]
pub mod browser_transport;

pub use config::LlmProviderConfig;
/// Backward-compatible alias for [`LlmProviderConfig`].
#[deprecated(
    since = "0.2.0",
    note = "renamed to LlmProviderConfig to avoid collision"
)]
pub type ProviderConfig = LlmProviderConfig;
pub use error::{ProviderError, Result};
pub use sse::parse_sse_line;
pub use types::{ChatMessage, ChatRequest, ChatResponse, StreamChunk, ToolCall, Usage};

#[cfg(feature = "native")]
pub use failover::FailoverChain;
pub use hermes::ReasoningMode;
#[cfg(feature = "native")]
pub use local_provider::LocalProvider;
#[cfg(feature = "native")]
pub use openai_compat::OpenAiCompatProvider;
#[cfg(feature = "native")]
pub use placement::{CachedResolver, PlacedProvider, PlacementResolver, ProviderFactory};
#[cfg(feature = "native")]
pub use provider::Provider;
#[cfg(feature = "native")]
pub use retry::{RetryConfig, RetryPolicy};
#[cfg(feature = "native")]
pub use router::ProviderRouter;

#[cfg(feature = "browser")]
pub use browser_transport::BrowserLlmClient;
