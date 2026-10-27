//! Platform abstraction layer for clawft.
//!
//! Provides traits for all platform-dependent operations (HTTP, filesystem,
//! environment, process spawning) so the core engine can be platform-agnostic
//! and WASM-compatible.
//!
//! # Architecture
//!
//! The [`Platform`] trait bundles all platform capabilities via accessor methods.
//! Each sub-capability has its own trait ([`http::HttpClient`], [`fs::FileSystem`],
//! [`env::Environment`], [`process::ProcessSpawner`]) with a corresponding native
//! implementation.
//!
//! [`ProcessSpawner`](process::ProcessSpawner) is intentionally excluded from the
//! [`Platform`] bundle because it is unavailable in WASM environments. It is
//! accessible via [`Platform::process`] which returns `Option`.
//!
//! # Example
//!
//! ```rust,no_run
//! use clawft_platform::{Platform, NativePlatform};
//! use clawft_platform::http::HttpClient;
//! use std::collections::HashMap;
//!
//! # async fn example() {
//! let platform = NativePlatform::new();
//! let response = platform.http()
//!     .get("https://example.com", &HashMap::new())
//!     .await
//!     .unwrap();
//! assert!(response.is_success());
//! # }
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

pub mod config_loader;
pub mod env;
pub mod fs;
pub mod http;
pub mod process;

#[cfg(feature = "browser")]
pub mod browser;
#[cfg(feature = "browser")]
pub use browser::BrowserPlatform;

use async_trait::async_trait;

/// Bundle of all platform capabilities.
///
/// Implementors provide concrete implementations of all platform traits.
/// The native implementation ([`NativePlatform`]) uses reqwest, tokio::fs,
/// std::env, and tokio::process. A WASM implementation would use the fetch
/// API, WASI filesystem, and an in-memory environment.
///
/// The `async_trait` bound differs based on target:
/// - Native: `Send + Sync` required (multi-threaded tokio runtime).
/// - Browser: `?Send` (single-threaded WASM runtime).
#[cfg_attr(not(feature = "browser"), async_trait)]
#[cfg_attr(feature = "browser", async_trait(?Send))]
pub trait Platform: Send + Sync {
    /// HTTP client for making API requests.
    fn http(&self) -> &dyn http::HttpClient;

    /// Filesystem operations.
    fn fs(&self) -> &dyn fs::FileSystem;

    /// Environment variable access.
    fn env(&self) -> &dyn env::Environment;

    /// Process spawning capability.
    ///
    /// Returns `None` in environments where process spawning is unavailable
    /// (e.g., WASM).
    fn process(&self) -> Option<&dyn process::ProcessSpawner>;
}

/// Native platform implementation using std, tokio, and reqwest.
///
/// This is the standard platform for server-side and CLI usage. It provides:
/// - HTTP via [`reqwest`] with connection pooling and TLS.
/// - Filesystem via [`tokio::fs`].
/// - Environment via [`std::env`].
/// - Process spawning via [`tokio::process`].
#[cfg(feature = "native")]
pub struct NativePlatform {
    http: http::NativeHttpClient,
    fs: fs::NativeFileSystem,
    env: env::NativeEnvironment,
    process: process::NativeProcessSpawner,
}

#[cfg(feature = "native")]
impl NativePlatform {
    /// Create a new native platform with default configuration.
    pub fn new() -> Self {
        Self {
            http: http::NativeHttpClient::new(),
            fs: fs::NativeFileSystem,
            env: env::NativeEnvironment,
            process: process::NativeProcessSpawner,
        }
    }
}

#[cfg(feature = "native")]
impl Default for NativePlatform {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "native")]
#[async_trait]
impl Platform for NativePlatform {
    fn http(&self) -> &dyn http::HttpClient {
        &self.http
    }

    fn fs(&self) -> &dyn fs::FileSystem {
        &self.fs
    }

    fn env(&self) -> &dyn env::Environment {
        &self.env
    }

    fn process(&self) -> Option<&dyn process::ProcessSpawner> {
        Some(&self.process)
    }
}

#[cfg(all(test, feature = "native"))]
mod tests {
    use super::*;

    #[test]
    fn test_native_platform_creation() {
        let platform = NativePlatform::new();
        // Verify all accessors return valid references
        let _http = platform.http();
        let _fs = platform.fs();
        let _env = platform.env();
        assert!(platform.process().is_some());
    }

    #[test]
    fn test_native_platform_default() {
        let platform = NativePlatform::default();
        assert!(platform.process().is_some());
    }

    #[test]
    fn test_platform_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<NativePlatform>();
    }
}
