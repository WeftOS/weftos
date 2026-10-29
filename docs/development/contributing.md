# Development and Contributing Guide

## Development Setup

### Prerequisites

- **Rust 1.95+** (edition 2024; `rust-version` in the root `Cargo.toml`) -- install via [rustup](https://rustup.rs/)
- **Cargo** (ships with rustup)
- **wasm32-unknown-unknown target** (optional, for WASM builds)

### Getting Started

```bash
git clone https://github.com/WeftOS/weftos.git
cd weftos
scripts/build.sh native-debug
scripts/build.sh test
```

Verify everything passes before making any changes:

```bash
scripts/build.sh clippy
cargo fmt --all -- --check
```

### Editor Setup

Any editor with rust-analyzer support works well. Recommended settings:

- Enable `clippy` as the check command in rust-analyzer.
- Enable format-on-save using `rustfmt`.


## Project Structure

The workspace has 65 member crates (`members` in the root `Cargo.toml`). The agent-layer core is nine of them, shown below with their dependency boundaries; the WeftOS kernel layer (`clawft-kernel`, `clawft-weave`, mesh, voice, world-model, cog and leaf crates) is described in the [architecture overview](../architecture/overview.md#the-weftos-layer-v084). Use `scripts/build.sh` for builds, tests and lint; run `scripts/build.sh gate` before committing.

```
weftos/
  Cargo.toml                # Workspace root
  crates/
    clawft-types/           # Core types: Config, events, errors
    clawft-platform/        # Platform abstraction (fs, http, env, process)
    clawft-core/            # Agent engine: loop, bus, pipeline, sessions, memory, security
    clawft-llm/             # LLM provider abstraction and routing
    clawft-tools/           # Tool implementations (file, shell, memory, web, spawn)
    clawft-channels/        # Channel plugins (Telegram, Slack, Discord)
    clawft-services/        # Background services (cron, heartbeat, MCP)
    clawft-cli/             # CLI binary (`weft`)
    clawft-wasm/            # WASM entrypoint
  docs/                     # Documentation
```

### Dependency Flow

Dependencies flow in one direction, roughly bottom-to-top:

```
clawft-types       (no internal deps)
clawft-platform    (depends on types)
clawft-llm         (depends on types)
clawft-core        (depends on types, platform)
clawft-tools       (depends on types, platform, core)
clawft-channels    (depends on types)
clawft-services    (depends on types, core)
clawft-cli         (depends on all of the above)
clawft-wasm        (depends on types)
```

### Key Architectural Patterns

- **`Platform` trait** (`clawft-platform`): Abstracts filesystem, HTTP, environment variables, and process spawning so that the same codebase runs on native and WASM targets.
- **`Tool` trait** (`clawft-core`): Extensible function-calling interface for the agent. Each tool declares its name, description, JSON Schema parameters, and an async `execute` method.
- **`Channel` / `ChannelFactory` traits** (`clawft-channels`): Plugin system for chat platforms. A factory builds a channel from JSON config; a channel handles start/stop lifecycle and bidirectional messaging.
- **`Provider` trait** (`clawft-llm`): Unified interface for LLM completions. The `ProviderRouter` dispatches by model prefix (e.g., `"openai/gpt-4o"` routes to the OpenAI provider).
- **`AgentLoop` pattern** (`clawft-core`): consume message -> build context -> run pipeline -> invoke tools -> dispatch response.
- **Feature gates**: The `vector-memory` feature enables optional advanced memory capabilities.


## Building and Testing

### Unified Build Script (Recommended)

The `scripts/build.sh` script provides subcommands for all build workflows:

```bash
scripts/build.sh native          # Release CLI binary
scripts/build.sh test            # Run all tests
scripts/build.sh clippy          # Clippy with -D warnings
scripts/build.sh gate            # Full phase gate; check 0 is the pre-build diff gate
scripts/build.sh diff-gate       # Just the diff gate (see diff-gate.md)
scripts/build.sh all             # Build everything (native + WASI + browser + UI)
```

Run `scripts/build.sh --help` for the full list. The `gate` subcommand runs all checks from the phase gate protocol and reports PASS/FAIL per check.

### Workspace Check

```bash
scripts/build.sh check
```

### Running Tests

Run the whole workspace test suite:

```bash
scripts/build.sh test
```

Run tests for a specific crate:

```bash
scripts/build.sh test clawft-core
scripts/build.sh test clawft-tools
```

Run a single test by name:

```bash
scripts/build.sh test clawft-core --filter test_name
```

### Linting

Zero warnings are required. Clippy must pass cleanly:

```bash
scripts/build.sh clippy
```

### Formatting

All code must be formatted with `rustfmt`:

```bash
# Check formatting (CI mode)
cargo fmt --all -- --check

# Apply formatting
cargo fmt --all
```

### Pre-Commit Checklist

Before every commit, run the full suite. The fastest way is via the build script:

```bash
scripts/build.sh gate
```

Or run the checks individually:

```bash
cargo fmt --all -- --check
scripts/build.sh clippy
scripts/build.sh test
scripts/build.sh check
```

All commands must succeed with zero errors and zero warnings.

### Optional: K3–K6 phase rehearsal (`scripts/k6-gate.sh`)

**Status (WEFT-464): developer-rehearsal only — not part of CI.**

`scripts/k6-gate.sh` runs a short list of named kernel acceptance tests for
historical phases K3 (WASM sandbox), K4 (containers), K5 (app framework), and
K6 (distributed fabric). Use it when you are changing those subsystems and
want a focused local checklist.

| Gate | Authority |
|------|-----------|
| Merge / production | `scripts/build.sh gate` + `.github/workflows/pr-gates.yml` |
| Local K3–K6 rehearsal | `scripts/k6-gate.sh` (optional; never required for merge) |

A green `k6-gate.sh` run is **not** a substitute for `scripts/build.sh gate`.
Details and rationale live in the script header and
[`docs/deployment/release.md`](../deployment/release.md) (CI gates section).


## Code Style Guidelines

### General Rules

- **Files under 500 lines.** Split large files into submodules.
- **No hardcoded secrets.** API keys come from environment variables.
- **All public APIs return `Result`.** Use `thiserror` enums for error types.
- **Async-first.** Use Tokio and `async-trait` for async interfaces.
- **Use `tracing` for logging.** Not `println!` or `eprintln!`.
- **Workspace dependencies.** Declare shared dependencies once in the root `Cargo.toml` `[workspace.dependencies]` section, then reference them with `{ workspace = true }` in crate-level `Cargo.toml` files.

### Naming Conventions

- Types: `PascalCase` (`ToolRegistry`, `ChannelMetadata`)
- Functions and methods: `snake_case` (`register_all`, `route`)
- Constants: `SCREAMING_SNAKE_CASE`
- Crate names: `clawft-{module}` (e.g., `clawft-core`, `clawft-llm`)

### Error Handling

Define error enums with `thiserror` in each crate:

```rust
use thiserror::Error;

#[derive(Debug, Error)]
pub enum MyError {
    #[error("not found: {0}")]
    NotFound(String),

    #[error("invalid input: {0}")]
    InvalidInput(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}
```

### Testing

Write tests alongside code in `#[cfg(test)] mod tests` blocks:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_valid_config() {
        let config: Config = serde_json::from_str(r#"{"name": "test"}"#).unwrap();
        assert_eq!(config.name, "test");
    }

    #[tokio::test]
    async fn execute_tool() {
        let tool = MyTool::new();
        let result = tool.execute(serde_json::json!({"key": "value"})).await;
        assert!(result.is_ok());
    }
}
```

### Documentation

Document all public items. Include examples for complex APIs:

```rust
/// Route a model name to its provider.
///
/// Returns the provider and the model name with the prefix stripped.
/// Falls back to the default provider if no prefix matches.
///
/// # Examples
///
/// ```rust,ignore
/// let router = ProviderRouter::with_builtins();
/// let (provider, model) = router.route("openai/gpt-4o").unwrap();
/// assert_eq!(provider.name(), "openai");
/// ```
pub fn route(&self, model: &str) -> Option<(&dyn Provider, String)> {
    // ...
}
```


## Adding MCP Tool Providers

To expose a custom tool source over MCP, implement the `ToolProvider` trait
defined in `clawft-services/src/mcp/provider.rs`:

```rust
pub trait ToolProvider: Send + Sync {
    fn namespace(&self) -> &str;
    fn list_tools(&self) -> Vec<ToolDefinition>;
    async fn call_tool(&self, name: &str, args: serde_json::Value) -> Result<CallToolResult>;
}
```

Register your provider with the MCP server shell:

```rust
let provider = Box::new(MyProvider::new());
shell.register_provider(provider);
```

The provider's tools will be served under the `{namespace}__{tool}` naming
convention and pass through the standard middleware pipeline (SecurityGuard,
PermissionFilter, ResultGuard, AuditLog). For proxying an external MCP server,
use the built-in `ProxyToolProvider` wrapping an `McpClient`.

---

## Adding Tools

Tools extend the agent's capabilities through LLM function calling. Each tool is a struct that implements the `Tool` trait.

### Step 1: Create the Tool Module

Create a new file in `crates/clawft-tools/src/`:

```rust
// crates/clawft-tools/src/my_tool.rs

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use clawft_core::tools::registry::{Tool, ToolError};
use clawft_platform::Platform;

pub struct MyTool<P: Platform> {
    platform: Arc<P>,
}

impl<P: Platform> MyTool<P> {
    pub fn new(platform: Arc<P>) -> Self {
        Self { platform }
    }
}

#[async_trait]
impl<P: Platform + 'static> Tool for MyTool<P> {
    fn name(&self) -> &str {
        "my_tool"
    }

    fn description(&self) -> &str {
        "A short description of what this tool does."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "input": {
                    "type": "string",
                    "description": "The input to process"
                }
            },
            "required": ["input"]
        })
    }

    async fn execute(&self, args: Value) -> Result<Value, ToolError> {
        let input = args
            .get("input")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("missing 'input'".into()))?;

        // Tool logic here
        Ok(json!({ "result": input }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Write tests for your tool
}
```

### Step 2: Export the Module

Add the module to `crates/clawft-tools/src/lib.rs`:

```rust
pub mod my_tool;
```

### Step 3: Register the Tool

Add the registration call in the `register_all` function in `crates/clawft-tools/src/lib.rs`:

```rust
registry.register(Arc::new(my_tool::MyTool::new(
    platform.clone(),
)));
```

If the tool requires additional constructor arguments (e.g., `workspace_dir`), thread them through `register_all`.

### Step 4: Write Tests

At a minimum, test:

- `name()` returns the expected identifier.
- `parameters()` returns valid JSON Schema.
- `execute()` succeeds with valid arguments.
- `execute()` returns the appropriate `ToolError` for invalid arguments.


## Adding Channels

Channels connect the agent to chat platforms. Each channel plugin implements two traits: `ChannelFactory` and `Channel`.

### Step 1: Create the Channel Module

Create a directory under `crates/clawft-channels/src/`:

```
crates/clawft-channels/src/
  my_platform/
    mod.rs       # Module root, re-exports
    channel.rs   # Channel trait implementation
    factory.rs   # ChannelFactory trait implementation
```

### Step 2: Implement `ChannelFactory`

The factory creates a `Channel` from JSON configuration:

```rust
// crates/clawft-channels/src/my_platform/factory.rs

use std::sync::Arc;

use clawft_types::error::ChannelError;

use crate::traits::{Channel, ChannelFactory};

pub struct MyPlatformFactory;

impl ChannelFactory for MyPlatformFactory {
    fn channel_name(&self) -> &str {
        "my_platform"
    }

    fn build(
        &self,
        config: &serde_json::Value,
    ) -> Result<Arc<dyn Channel>, ChannelError> {
        // Parse config, validate required fields, construct channel
        let token = config
            .get("token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ChannelError::ConfigError("missing 'token'".into()))?;

        Ok(Arc::new(MyPlatformChannel::new(token.to_string())))
    }
}
```

### Step 3: Implement `Channel`

```rust
// crates/clawft-channels/src/my_platform/channel.rs

use std::sync::Arc;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use clawft_types::error::ChannelError;
use clawft_types::event::OutboundMessage;

use crate::traits::{
    Channel, ChannelHost, ChannelMetadata, ChannelStatus, MessageId,
};

pub struct MyPlatformChannel {
    token: String,
    // ...
}

impl MyPlatformChannel {
    pub fn new(token: String) -> Self {
        Self { token }
    }
}

#[async_trait]
impl Channel for MyPlatformChannel {
    fn name(&self) -> &str {
        "my_platform"
    }

    fn metadata(&self) -> ChannelMetadata {
        ChannelMetadata {
            name: "my_platform".into(),
            display_name: "My Platform".into(),
            supports_threads: false,
            supports_media: true,
        }
    }

    fn status(&self) -> ChannelStatus {
        ChannelStatus::Stopped
    }

    fn is_allowed(&self, _sender_id: &str) -> bool {
        true // Or check against an allow-list
    }

    async fn start(
        &self,
        host: Arc<dyn ChannelHost>,
        cancel: CancellationToken,
    ) -> Result<(), ChannelError> {
        // Long-running loop: poll for messages, deliver via host.
        // Exit when cancel is triggered.
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                // msg = self.poll_messages() => { ... }
            }
        }
        Ok(())
    }

    async fn send(&self, msg: &OutboundMessage) -> Result<MessageId, ChannelError> {
        // Send message to the platform API
        todo!()
    }
}
```

### Step 4: Register the Factory

In `crates/clawft-channels/src/host.rs` (or wherever your gateway setup lives), register the factory with the `PluginHost`:

```rust
plugin_host
    .register_factory(Arc::new(my_platform::MyPlatformFactory))
    .await;
```

### Step 5: Add Channel Config

If your channel requires configuration fields, add them to the appropriate config struct in `clawft-types`.


## Adding LLM Providers

There are three ways to add a new LLM provider, depending on how closely it follows the OpenAI-compatible chat completions API format.

### Option A: Config-Only Built-in Provider (Most Common)

If the provider exposes an OpenAI-compatible `/v1/chat/completions` endpoint, you only need a configuration entry. No new code is required.

The router ships with 9 built-in providers: OpenAI, Anthropic, Groq, DeepSeek, Mistral, Together, OpenRouter, Gemini, and xAI. Each is an `LlmProviderConfig` in `crates/clawft-llm/src/config.rs`.

Add a new entry to the `builtin_providers()` function:

```rust
ProviderConfig {
    name: "my_provider".into(),
    base_url: "https://api.my-provider.com/v1".into(),
    api_key_env: "MY_PROVIDER_API_KEY".into(),
    model_prefix: Some("my_provider/".into()),
    default_model: Some("my-model-v1".into()),
    headers: HashMap::new(),
},
```

`ProviderConfig` fields:

| Field | Description |
|---|---|
| `name` | Unique identifier for the provider (e.g. `"my_provider"`) |
| `base_url` | Base URL for the OpenAI-compatible API endpoint |
| `api_key_env` | Environment variable name that holds the API key |
| `model_prefix` | Prefix for routing (e.g., `"my_provider/"` routes `"my_provider/model-x"`) |
| `default_model` | Fallback model when none is specified |
| `headers` | Extra HTTP headers for every request (e.g., API version headers) |

The prefix must end with `/`. The `ProviderRouter` strips the prefix before sending the model name to the API. For example, Anthropic adds a required version header:

```rust
headers: HashMap::from([("anthropic-version".into(), "2023-06-01".into())]),
```

### Option B: User-Configured Provider

Users can add providers at runtime without modifying the codebase. Add a provider entry to the clawft configuration file (e.g. `config.json`):

```json
{
  "providers": [
    {
      "name": "my_provider",
      "base_url": "https://api.my-provider.com/v1",
      "api_key_env": "MY_PROVIDER_API_KEY",
      "model_prefix": "my_provider/",
      "default_model": "my-model-v1",
      "headers": {}
    }
  ]
}
```

This follows the same `ProviderConfig` schema as Option A. User-configured providers are merged with the built-in set at startup and can override built-in providers by matching on `name`.

### Option C: Custom Provider Trait Implementation

If a provider does not follow the OpenAI-compatible API format, you must implement the `Provider` trait directly in `crates/clawft-llm/`:

```rust
use async_trait::async_trait;
use clawft_llm::provider::Provider;
use clawft_llm::types::{ChatRequest, ChatResponse};
use clawft_llm::Result;

pub struct CustomProvider {
    api_key: String,
    // ... provider-specific fields
}

#[async_trait]
impl Provider for CustomProvider {
    fn name(&self) -> &str {
        "custom"
    }

    async fn complete(&self, request: &ChatRequest) -> Result<ChatResponse> {
        // 1. Translate ChatRequest (messages, tools, model, temperature, max_tokens)
        //    into the provider's native request format.
        // 2. Make the HTTP call to the provider's API.
        // 3. Translate the provider's response back into ChatResponse.
        todo!()
    }
}
```

Then register the provider with the `ProviderRouter` by extending `from_configs` or constructing the router manually.


## Adding Pipeline Stages

The pipeline system (`crates/clawft-core/src/pipeline/`) processes every LLM request through 6 stages, each defined by a trait. You can replace any stage with a custom implementation.

### Pipeline Architecture

The 6 stages execute in order:

| Stage | Trait | Purpose |
|---|---|---|
| 1 | `TaskClassifier` | Classify the request by task type and complexity |
| 2 | `ModelRouter` | Select the best provider/model for the task |
| 3 | `ContextAssembler` | Assemble context (system prompt, memory, history) |
| 4 | `LlmTransport` | Execute the LLM call via HTTP |
| 5 | `QualityScorer` | Score the response quality |
| 6 | `LearningBackend` | Record the interaction for future learning |

All traits are defined in `crates/clawft-core/src/pipeline/traits.rs`. Each has a Level 0 (baseline) implementation shipped with the crate.

### Trait Signatures

**TaskClassifier** -- synchronous, stateless classification:

```rust
pub trait TaskClassifier: Send + Sync {
    fn classify(&self, request: &ChatRequest) -> TaskProfile;
}
```

`TaskProfile` contains a `TaskType` enum (Chat, CodeGeneration, CodeReview, Research, Creative, Analysis, ToolUse, Unknown), a `complexity: f32` score (0.0--1.0), and matched `keywords`.

**ModelRouter** -- async routing with feedback loop:

```rust
#[async_trait]
pub trait ModelRouter: Send + Sync {
    async fn route(&self, request: &ChatRequest, profile: &TaskProfile) -> RoutingDecision;
    fn update(&self, decision: &RoutingDecision, outcome: &ResponseOutcome);
}
```

`RoutingDecision` contains `provider`, `model`, and `reason` fields. The `update` method receives post-response feedback for adaptive routers.

**QualityScorer** -- synchronous response scoring:

```rust
pub trait QualityScorer: Send + Sync {
    fn score(&self, request: &ChatRequest, response: &LlmResponse) -> QualityScore;
}
```

`QualityScore` contains `overall`, `relevance`, and `coherence` fields, each on a 0.0--1.0 scale.

**LearningBackend** -- synchronous trajectory recording:

```rust
pub trait LearningBackend: Send + Sync {
    fn record(&self, trajectory: &Trajectory);
    fn adapt(&self, signal: &LearningSignal);
}
```

`Trajectory` bundles the original request, routing decision, LLM response, and quality score. `LearningSignal` carries user feedback.

**ContextAssembler** and **LlmTransport** are async:

```rust
#[async_trait]
pub trait ContextAssembler: Send + Sync {
    async fn assemble(&self, request: &ChatRequest, profile: &TaskProfile) -> AssembledContext;
}

#[async_trait]
pub trait LlmTransport: Send + Sync {
    async fn complete(&self, request: &TransportRequest) -> clawft_types::Result<LlmResponse>;
}
```

### Registering via PipelineRegistry

A `Pipeline` struct wires all 6 stages together:

```rust
let pipeline = Pipeline {
    classifier: Arc::new(MyClassifier::new()),
    router: Arc::new(MyRouter::new()),
    assembler: Arc::new(TokenBudgetAssembler::new(4096)),
    transport: Arc::new(OpenAiCompatTransport::new()),
    scorer: Arc::new(MyScorer::new()),
    learner: Arc::new(MyLearner::new()),
};
```

A `PipelineRegistry` maps `TaskType` variants to specialized pipelines, with a fallback default:

```rust
let mut registry = PipelineRegistry::new(default_pipeline);
registry.register(TaskType::CodeGeneration, code_pipeline);
registry.register(TaskType::Research, research_pipeline);
```

When `registry.complete(&request)` is called, the default classifier identifies the task type, then the registry dispatches to the matching specialized pipeline (or the default).

### Injecting a Custom Pipeline via AppContext

`AppContext` (in `crates/clawft-core/src/bootstrap.rs`) provides `set_pipeline()` to replace the entire pipeline registry before creating the agent loop:

```rust
let mut ctx = AppContext::new(config, platform).await?;

// Build a custom pipeline registry
let mut registry = PipelineRegistry::new(my_default_pipeline);
registry.register(TaskType::CodeGeneration, my_code_pipeline);

ctx.set_pipeline(registry);

let agent_loop = ctx.into_agent_loop();
```

For the common case of enabling real LLM calls (replacing the stub transport with a live provider), use the convenience method:

```rust
ctx.enable_live_llm();
```


## Release Process

### Debug Build

```bash
scripts/build.sh native-debug
```

### Release Build

```bash
scripts/build.sh native
```

The release profile is tuned for minimal binary size:

| Setting | Value | Purpose |
|---|---|---|
| `opt-level` | `"z"` | Optimize for size |
| `lto` | `true` | Link-time optimization across crates |
| `strip` | `true` | Strip debug symbols from binary |
| `codegen-units` | `1` | Single codegen unit for best optimization |
| `panic` | `"abort"` | Abort on panic (smaller than unwind tables) |

### Release Checklist

1. All tests pass: `scripts/build.sh test`
2. No clippy warnings: `scripts/build.sh clippy`
3. Formatting is clean: `cargo fmt --all -- --check`
4. Release build succeeds: `cargo build --workspace --release`
5. WASM build succeeds (if applicable): `cargo build -p clawft-wasm --target wasm32-unknown-unknown --release`


## WASM Target

The `clawft-wasm` crate provides the entrypoint for WebAssembly builds. The `Platform` trait abstraction ensures the same core logic works across native and WASM targets.

### Building for WASM

Install the target if not already present:

```bash
rustup target add wasm32-unknown-unknown
```

Build the WASM binary:

```bash
cargo build -p clawft-wasm --target wasm32-unknown-unknown --release
```

The release-wasm profile inherits from the release profile and applies the same size optimizations.

### Platform Abstraction

WASM builds use a different `Platform` implementation that maps filesystem calls, HTTP requests, and environment access to browser or host APIs. When adding new features, ensure they work behind the `Platform` trait rather than calling native APIs directly. This keeps the codebase portable.

### Feature Flags

The workspace uses Cargo feature flags to gate optional subsystems. Features are defined per-crate in each crate's `Cargo.toml`.

#### `vector-memory`

Defined in `clawft-core`. Enables advanced memory and routing capabilities:

| Module | Description |
|---|---|
| `IntelligentRouter` | 3-tier routing with complexity scoring (ADR-026) |
| `VectorStore` | In-memory vector similarity search for cached routing policies |
| `SessionIndexer` | Session-level embedding index for memory retrieval |
| `HashEmbedder` | Deterministic hash-based embedder (no external model needed) |

Build with this feature:

```bash
cargo build -p clawft-core --features vector-memory
cargo test -p clawft-core --features vector-memory
```

Or from the workspace root:

```bash
cargo build --workspace --features clawft-core/vector-memory
```

#### Planned Feature Flags

The following feature flags are planned but not yet implemented:

| Flag | Scope | Description |
|---|---|---|
| `rvf` | `clawft-core` | Relevance Vector Filtering for context pruning |
| `ruvllm` | `clawft-llm` | Custom LLM runtime integration |
| `sona` | `clawft-core` | Self-Organizing Neural Architecture for pipeline adaptation |
| `attention` | `clawft-core` | Attention-based context weighting in the assembler |

#### Writing Feature-Gated Code

Gate modules or items behind features using `cfg` attributes:

```rust
// Gate an entire module
#[cfg(feature = "vector-memory")]
pub mod intelligent_router;

// Gate a struct field or method
impl MyStruct {
    #[cfg(feature = "vector-memory")]
    pub fn advanced_search(&self, query: &str) -> Vec<Result> {
        // ...
    }
}
```

Some features are not available on WASM targets. Use target gates for native-only functionality:

```rust
#[cfg(not(target_arch = "wasm32"))]
pub mod native_only_module;
```

When adding a new feature flag, declare it in the relevant crate's `Cargo.toml` under `[features]` and gate any new dependencies with `dep:`:

```toml
[features]
my-feature = ["dep:some-crate"]
```


---

*clawft is developed and maintained as part of the [clawft](https://github.com/WeftOS/weftos) project.*
