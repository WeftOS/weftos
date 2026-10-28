# Building ClawFT

This guide covers every way to build the `weft` binary and the `clawft-wasm` module, including feature flags, cross-compilation, WASM targets, Docker images, size optimization, and CI/CD considerations.

## Prerequisites

- **Rust toolchain**: Edition 2024, minimum `rustc 1.95` (see `rust-version` in workspace `Cargo.toml`)
- **Cargo**: Ships with rustup
- Optional: `cross` for cross-compilation, `binaryen` for `wasm-opt`, `wasm-tools` for component model support

Install the Rust toolchain:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

## Workspace Structure

The root `Cargo.toml` lists 65 workspace member crates (40 published to crates.io). The table shows the nine crates that make up the `weft` binary and the WASM entrypoint. The rest are the WeftOS kernel layer (`clawft-kernel`, `clawft-weave` which builds `weaver`, mesh, voice, world-model and cog crates); see [the architecture overview](../architecture/overview.md#the-weftos-layer-v084). Cogs themselves build in [WeftOS/weftos-cogs](https://github.com/WeftOS/weftos-cogs), which WeftOS consumes by pinned git rev.

| Crate | Purpose | Optional |
|-------|---------|----------|
| `clawft-types` | Shared types and config structs | No |
| `clawft-platform` | Platform abstraction (filesystem, HTTP, env) | No |
| `clawft-core` | Agent loop, pipeline, tools, embeddings | No |
| `clawft-llm` | LLM provider abstraction (OpenAI-compat + native) | No |
| `clawft-tools` | Tool implementations (file, shell, memory, web) | No |
| `clawft-channels` | Channel plugins (Telegram, Slack, Discord) | Yes (feature: `channels`) |
| `clawft-services` | Services (cron, heartbeat, MCP, delegation) | Yes (feature: `services`) |
| `clawft-cli` | CLI binary (`weft`) | No (the main binary) |
| `clawft-wasm` | WASM entrypoint for edge/browser deployment | Separate target |

The binary is always `weft` (from `clawft-cli`).

## Unified Build Script

The `scripts/build.sh` script wraps all build workflows behind simple subcommands:

```bash
scripts/build.sh native          # Release CLI binary
scripts/build.sh native-debug    # Debug build (fast)
scripts/build.sh wasi            # WASM for WASI (wasm32-wasip2)
scripts/build.sh browser         # WASM for browser (wasm32-unknown-unknown)
scripts/build.sh ui              # React dashboard frontend (tsc + vite); skips when clawft-ui/ is absent
scripts/build.sh ui-docker       # Multi-stage clawft-ui Docker image (WEFT-317)
scripts/build.sh ui-e2e          # Run the clawft-ui Playwright E2E suite (WEFT-314)
scripts/build.sh all             # Build everything
scripts/build.sh test            # cargo test --workspace
scripts/build.sh check           # cargo check --workspace
scripts/build.sh clippy          # Clippy with warnings-as-errors
scripts/build.sh gate            # Full phase gate (11 checks with PASS/FAIL)
scripts/build.sh clean           # Clean all artifacts
```

Options: `--features <f>`, `--profile <p>`, `--verbose`, `--dry-run`, `--help`.

```bash
# Examples
scripts/build.sh native --features voice          # CLI with voice
scripts/build.sh native --features voice,channels # CLI with voice + Discord
scripts/build.sh native --dry-run                 # Preview commands
scripts/build.sh gate                             # Run all 11 phase gate checks

# Dashboard (clawft-ui) sub-commands
scripts/build.sh ui-docker                        # Builds the multi-stage
                                                  # clawft-ui Docker image
                                                  # via clawft-ui/Dockerfile;
                                                  # tag overridable via
                                                  # CLAWFT_UI_DOCKER_TAG
                                                  # (default: clawft-ui:dev).
scripts/build.sh ui-e2e                           # Installs Playwright +
                                                  # chromium and runs the
                                                  # clawft-ui E2E suite.
```

### Release gates and `--all-features`

Keep `--all-features` off release gates and release builds. Some crates carry
test-only features (for example `clawft-mesh-service`'s `testing`, which adds
injectable peer credentials and fails to compile in release profiles on
purpose); `--all-features` would enable them. Use the named commands
(`scripts/build.sh gate`, `check`, `test`) instead.

## Quick Reference (raw cargo)

```bash
# Dev build (fast compile, no optimizations)
cargo build -p clawft-cli

# Release build (optimized, stripped, small)
cargo build --release -p clawft-cli

# Run tests for the whole workspace
cargo test --workspace

# Run clippy
cargo clippy --workspace --all-targets

# Build WASM
cargo wasm -p clawft-wasm
```

## Feature Flags

Feature flags control which optional components are compiled in. They propagate through the crate dependency chain.

### `clawft-cli` Features

| Feature | Default | Enables | Description |
|---------|---------|---------|-------------|
| `channels` | Yes | `dep:clawft-channels` | Telegram, Slack, Discord channel adapters |
| `services` | Yes | `dep:clawft-services` | Cron, heartbeat, MCP server/client |
| `vector-memory` | No | `clawft-core/vector-memory` | Vector store for semantic memory |
| `delegate` | No | `clawft-services/delegate`, `clawft-tools/delegate` | Task delegation to Claude AI / Claude Flow |

### `clawft-core` Features

| Feature | Default | Enables | Description |
|---------|---------|---------|-------------|
| `full` | Yes | (marker) | Default feature set |
| `vector-memory` | No | `dep:rand` | In-memory vector store with cosine similarity |
| `rvf` | No | `vector-memory` + `dep:rvf-runtime` + `dep:rvf-types` + `dep:sha2` + `dep:reqwest` | RuVector Format persistence for embeddings |

### `clawft-services` Features

| Feature | Default | Enables | Description |
|---------|---------|---------|-------------|
| `delegate` | No | `dep:regex` | Delegation engine with regex-based routing rules |

### `clawft-tools` Features

| Feature | Default | Enables | Description |
|---------|---------|---------|-------------|
| `native-exec` | Yes | (marker) | Shell execution and process spawning |
| `vector-memory` | No | `clawft-core/vector-memory` | Vector memory tool |
| `delegate` | No | `clawft-services/delegate` | Delegation tool for Claude sub-agent |

### `clawft-wasm` Features

| Feature | Default | Enables | Description |
|---------|---------|---------|-------------|
| `alloc-talc` | No | `dep:talc` | Talc allocator (fast, small footprint) |
| `alloc-lol` | No | `dep:lol_alloc` | lol_alloc bump allocator (minimal code, never frees) |
| `alloc-tracing` | No | (internal) | Allocation counting over dlmalloc |

Default WASM allocator is `dlmalloc` (no feature flag needed).

### Feature Flag Chains

Features propagate through dependencies:

```
clawft-cli --features vector-memory
  -> clawft-core/vector-memory
    -> dep:rand

clawft-cli --features delegate
  -> clawft-services/delegate
    -> dep:regex
  -> clawft-tools/delegate
    -> clawft-services/delegate

clawft-core --features rvf
  -> vector-memory (dep:rand)
  -> dep:rvf-runtime
  -> dep:rvf-types
  -> dep:sha2
  -> dep:reqwest
```

## Build Profiles

### Debug (default)

```bash
cargo build -p clawft-cli
```

Fast compilation, no optimizations, includes debug symbols. Binary is large (~50-100 MB). Use for development.

### Release

```bash
cargo build --release -p clawft-cli
```

The workspace `Cargo.toml` defines an aggressive release profile:

```toml
[profile.release]
opt-level = "z"    # Optimize for size (smallest binary)
lto = true         # Link-time optimization (cross-crate inlining)
strip = true       # Strip debug symbols and metadata
codegen-units = 1  # Single codegen unit (better optimization, slower compile)
panic = "abort"    # No unwinding (smaller binary, no catch_unwind)
```

This produces binaries under 15 MB. Compile time is significantly longer than debug.

### Release-WASM

```toml
[profile.release-wasm]
inherits = "release"
opt-level = "z"    # Size-optimized for WASM
```

Used exclusively for WASM targets. Inherits all release settings.

## Build Variants

### Smallest Binary (channels disabled)

The default feature set (`channels` + `services`) produces the standard binary. To produce a smaller binary, disable the `channels` feature (which excludes Telegram, Slack, and Discord adapters):

```bash
cargo build --release -p clawft-cli --no-default-features --features services
```

> **Note:** The `services` feature is currently required. The CLI unconditionally
> imports `clawft-services` for MCP tool registration, so
> `--no-default-features` alone does not compile. This is tracked as a known
> issue -- the MCP imports in `mcp_tools.rs` need `#[cfg(feature = "services")]`
> gating before a fully minimal build is possible.

### Largest Binary (all features)

Enable every optional feature:

```bash
cargo build --release -p clawft-cli --features vector-memory,delegate
```

This includes the full feature set: channels, services, vector memory, and delegation.

To also include RVF support (builds against `clawft-core` directly):

```bash
cargo build --release -p clawft-cli --features vector-memory,delegate
# RVF is a clawft-core feature, enable it when building core directly:
cargo build --release -p clawft-core --features rvf
```

### Agent-Only (no channels)

For deployments that only need the interactive agent or MCP server without messaging channels:

```bash
cargo build --release -p clawft-cli --no-default-features --features services
```

### With Delegation

To enable the Claude AI / Claude Flow delegation engine:

```bash
cargo build --release -p clawft-cli --features delegate
```

This compiles in the `DelegationEngine`, `ClaudeDelegator`, and `DelegateTaskTool`. At runtime, delegation also requires:
- `ANTHROPIC_API_KEY` environment variable
- `delegation.claude_enabled = true` in config

## WASM Builds

The `clawft-wasm` crate produces a WebAssembly module for edge and browser deployments. It depends only on `clawft-types`, `serde`, and `serde_json` -- it does **not** pull in `tokio`, `reqwest`, or any native-only crates.

### Supported WASM Targets

| Target | WASI Version | Use Case |
|--------|-------------|----------|
| `wasm32-wasip1` | Preview 1 | Wasmtime, Wasmer, WASI runtimes |
| `wasm32-wasip2` | Preview 2 (Component Model) | Modern WASI runtimes, composable components |

### Building WASM

Install the target first:

```bash
# WASI Preview 1
rustup target add wasm32-wasip1

# WASI Preview 2 (component model)
rustup target add wasm32-wasip2
```

Build using the cargo alias (defined in `.cargo/config.toml`):

```bash
# Uses the alias: cargo build --target wasm32-wasip1 --profile release-wasm -p clawft-wasm
cargo wasm -p clawft-wasm
```

Or build for wasip2 directly:

```bash
cargo build --target wasm32-wasip2 --profile release-wasm -p clawft-wasm
```

The `.cargo/config.toml` applies size-optimized rustflags for WASM targets:

```toml
[target.wasm32-wasip1]
rustflags = ["-C", "opt-level=z"]

[alias]
wasm = "build --target wasm32-wasip1 --profile release-wasm"
```

### WASM Size Budget

The CI enforces strict size gates:

| Metric | Budget |
|--------|--------|
| Uncompressed | < 300 KB |
| Gzipped (level 9) | < 120 KB |

### WASM Optimization with wasm-opt

After building, run `wasm-opt` from binaryen for further size reduction (typically 15-30%):

```bash
# Install binaryen
sudo apt-get install binaryen    # Debian/Ubuntu
brew install binaryen             # macOS

# Run the optimization script
bash scripts/build/wasm-opt.sh
```

The script auto-detects whether the binary is a core module (wasip1) or component model (wasip2) and handles each appropriately. For component model binaries, it extracts core modules with `wasm-tools`, optimizes them individually, and reports the savings.

Manual invocation:

```bash
wasm-opt -Oz \
  --enable-bulk-memory \
  --enable-sign-ext \
  -o target/wasm32-wasip1/release-wasm/clawft_wasm.opt.wasm \
  target/wasm32-wasip1/release-wasm/clawft_wasm.wasm
```

### WASM Allocator Selection

The WASM crate supports three allocators, selectable via feature flags:

| Allocator | Feature Flag | Code Size | Behavior | Best For |
|-----------|-------------|-----------|----------|----------|
| dlmalloc | (default) | ~2-10 KB | Full malloc/free | General use, long-running modules |
| talc | `alloc-talc` | ~1-2 KB | Full malloc/free, Rust-native | Size-sensitive, long-running |
| lol_alloc | `alloc-lol` | ~200 bytes | Bump-only, never frees | Short-lived request/response |

```bash
# Build with talc allocator
cargo build --target wasm32-wasip1 --profile release-wasm -p clawft-wasm --features alloc-talc

# Build with lol_alloc (WARNING: memory grows unbounded)
cargo build --target wasm32-wasip1 --profile release-wasm -p clawft-wasm --features alloc-lol

# Build with allocation tracing (for profiling)
cargo build --target wasm32-wasip1 --profile release-wasm -p clawft-wasm --features alloc-tracing
```

Compare allocator sizes with the benchmark script:

```bash
bash scripts/bench/alloc-compare.sh
```

### WASM Limitations

The WASM build excludes:
- Shell execution (`exec_shell`, `spawn`)
- Channel plugins (Telegram, Slack, Discord)
- Native CLI terminal I/O
- Process spawning

Available tools in WASM: `read_file`, `write_file`, `edit_file`, `list_directory`, `memory_read`, `memory_write`, `web_fetch`, `web_search`.

## Cross-Compilation

The `scripts/build/cross-compile.sh` script handles cross-compilation for any Rust target:

```bash
# Linux static (musl) -- recommended for containers
./scripts/build/cross-compile.sh x86_64-unknown-linux-musl --use-cross

# macOS ARM
./scripts/build/cross-compile.sh aarch64-apple-darwin

# Windows
./scripts/build/cross-compile.sh x86_64-pc-windows-msvc

# WASM (auto-selects clawft-wasm crate and release-wasm profile)
./scripts/build/cross-compile.sh wasm32-wasip1
```

The `--use-cross` flag uses [`cross`](https://github.com/cross-rs/cross) instead of cargo, which provides pre-built Docker containers with the required linkers and sysroots. Recommended for musl targets on non-Linux hosts.

Install cross:

```bash
cargo install cross
```

### Common Cross-Compilation Targets

| Target | OS | Arch | Notes |
|--------|----|------|-------|
| `x86_64-unknown-linux-gnu` | Linux | x86_64 | Dynamic linking, needs glibc |
| `x86_64-unknown-linux-musl` | Linux | x86_64 | Static binary, no runtime deps |
| `aarch64-unknown-linux-musl` | Linux | ARM64 | Static, for ARM servers/Raspberry Pi |
| `aarch64-apple-darwin` | macOS | ARM64 | Apple Silicon |
| `x86_64-apple-darwin` | macOS | x86_64 | Intel Macs |
| `x86_64-pc-windows-msvc` | Windows | x86_64 | Requires MSVC toolchain. **Daemon (WEFT-11 + WEFT-559):** `DaemonClient` + weave named-pipe accept loop; cargo-dist target re-enabled. Soak-test on a Windows host: `cargo test -p clawft-rpc --target x86_64-pc-windows-msvc` and `weaver kernel start --foreground`. |
| `wasm32-wasip1` | WASI | WASM | WASI Preview 1 |
| `wasm32-wasip2` | WASI | WASM | WASI Preview 2 (Component Model) |

### Windows / CI matrix note (WEFT-11 + WEFT-559)

| Surface | Status |
|---------|--------|
| `clawft-rpc` named-pipe **client** | Implemented (`cfg(windows)`) |
| `clawft-rpc` named-pipe **server helpers** | Implemented (`named_pipe` module) |
| Weave daemon accept loop | Wired under `cfg(windows)` (WEFT-559) |
| cargo-dist `x86_64-pc-windows-msvc` | Re-enabled in root `Cargo.toml` |
| GitHub Actions Windows/macOS runners in pr-gates | **WEFT-457**: `test` job matrix runs light package set on `macos-latest` + `windows-latest` (`clawft-rpc`, `clawft-platform`, `clawft-types`, `clawft-cow-memory`); full workspace stays on `ubuntu-latest` |

**cargo-dist re-enable checklist** (already applied for WEFT-559):

1. `"x86_64-pc-windows-msvc"` in `[workspace.metadata.dist].targets`
2. Release pipeline produces zip + powershell installer
3. Residual: optional dedicated job for
   `cargo test -p clawft-rpc --target x86_64-pc-windows-msvc` so the
   `cfg(windows)` named-pipe roundtrip runs in CI

See also [`weftos-deferred-requirements.md`](./weftos-deferred-requirements.md) (Windows transport section) and [`kernel.md`](./kernel.md) (Windows transport).

## Docker Builds

The project includes a minimal `FROM scratch` Dockerfile that produces images under 20 MB.

### Build the Docker Image

```bash
./scripts/build/docker-build.sh
```

This:
1. Cross-compiles a static musl binary for `x86_64-unknown-linux-musl`
2. Copies it into a `scratch` image (no OS, no shell, just the binary)
3. Validates the image is under 20 MB
4. Tags as `clawft:latest`

With a custom tag:

```bash
./scripts/build/docker-build.sh --tag v0.1.0
```

Push to a registry:

```bash
./scripts/build/docker-build.sh --tag v0.1.0 --push
```

### Running the Docker Image

```bash
# Run the gateway
docker run -v ~/.clawft:/root/.clawft clawft:latest gateway

# Run interactive agent (requires TTY)
docker run -it -v ~/.clawft:/root/.clawft clawft:latest agent

# Run as MCP server
docker run -i clawft:latest mcp-server
```

The image exposes `/root/.clawft` as a volume for persistent configuration and sessions.

## Release Packaging

Release archives are produced by [`cargo-dist`](https://opensource.axo.dev/cargo-dist/)
inside the `Release` workflow (`.github/workflows/release.yml`). The
target matrix and per-archive contents come from
`[workspace.metadata.dist]` in the workspace `Cargo.toml`. The flow is
documented end-to-end in [`docs/deployment/release.md`](../deployment/release.md).

The legacy hand-rolled `scripts/release/package.sh` and
`scripts/release/package-all.sh` were retired in WEFT-449 — cargo-dist
now owns archive packaging, sigstore attestations, and the universal
installer in lockstep.

## CI/CD

### GitHub Actions Workflows

Two workflows are defined in `.github/workflows/`:

**`wasm-build.yml`** -- WASM Build & Size Gate
- Triggers on push/PR to `main`
- Builds `clawft-wasm` for `wasm32-wasip2`
- Runs `wasm-opt` optimization
- Enforces the 300 KB / 120 KB (gzip) size gate
- Profiles with `twiggy` (optional)
- Uploads WASM binaries and profile reports as artifacts

**`benchmarks.yml`** -- Benchmarks & Regression Check
- Triggers on push/PR to `main`
- Builds release binary
- Runs all benchmark scripts (`scripts/bench/run-all.sh`)
- Compares against baseline (`scripts/bench/baseline.json`)
- Comments on PR if any metric regresses beyond 10%
- Uploads benchmark results as artifacts

### Dev vs Production Builds

| Concern | Development | Production / CI |
|---------|-------------|-----------------|
| Profile | `debug` (default) | `release` |
| Feature flags | All (for testing) | Only what's needed |
| Compile time | Fast (incremental) | Slow (LTO, single codegen unit) |
| Binary size | Large (~50-100 MB) | Small (<15 MB native, <300 KB WASM) |
| Debug symbols | Included | Stripped |
| Panic behavior | Unwind (backtrace) | Abort (smaller binary) |
| Test command | `cargo test --workspace` | `cargo test --workspace --release` |

### CI Build Matrix Recommendations

For a comprehensive CI pipeline:

```yaml
# Native builds
- target: x86_64-unknown-linux-musl
  features: "default"
- target: x86_64-unknown-linux-musl
  features: "default,vector-memory,delegate"
- target: aarch64-unknown-linux-musl
  features: "default"

# WASM builds
- target: wasm32-wasip2
  crate: clawft-wasm

# Feature flag validation (catch dead code behind features)
- name: "no-default-features"
  command: cargo build -p clawft-cli --no-default-features
- name: "all-features"
  command: cargo build -p clawft-cli --features vector-memory,delegate
```

### Testing Feature Combinations

Different feature combinations can expose compilation errors. Test these in CI:

```bash
# Minimal (services required, channels optional)
cargo check -p clawft-cli --no-default-features --features services

# Default
cargo check -p clawft-cli

# Each optional feature in isolation
cargo check -p clawft-cli --features vector-memory
cargo check -p clawft-cli --features delegate

# All features
cargo check -p clawft-cli --features vector-memory,delegate

# WASM crate (separate target)
cargo check -p clawft-wasm --target wasm32-wasip1

# WASM with each allocator
cargo check -p clawft-wasm --target wasm32-wasip1 --features alloc-talc
cargo check -p clawft-wasm --target wasm32-wasip1 --features alloc-lol
```

## Benchmarks

The `scripts/bench/` directory contains benchmark scripts:

| Script | Purpose |
|--------|---------|
| `run-all.sh` | Run all benchmarks |
| `startup-time.sh` | Measure cold start time |
| `throughput.sh` | Message throughput test |
| `memory-usage.sh` | Peak memory usage |
| `wasm-size.sh` | WASM binary size report |
| `wasm-size-gate.sh` | Enforce WASM size budgets |
| `wasm-startup.sh` | WASM module instantiation time |
| `wasm-twiggy.sh` | WASM code size profiling |
| `wasm-feature-check.sh` | Validate WASM feature support |
| `alloc-compare.sh` | Compare WASM allocator sizes |
| `regression-check.sh` | Compare against baseline |
| `save-results.sh` | Save benchmark results to JSON |

Run all benchmarks:

```bash
cargo build --release -p clawft-cli
bash scripts/bench/run-all.sh target/release/weft
```

### Size Targets

| Artifact | Budget |
|----------|--------|
| Native release binary | < 15 MB |
| Docker image | < 20 MB |
| WASM uncompressed | < 300 KB |
| WASM gzipped | < 120 KB |

Check native binary size:

```bash
bash scripts/build/size-check.sh target/release/weft
```

## Troubleshooting

### `cargo build --features vector-memory` fails

The `vector-memory` feature is defined on `clawft-cli` and `clawft-tools`, not at the workspace level. Specify the package explicitly:

```bash
# Correct
cargo build --release --features vector-memory -p clawft-cli

# Also correct
cargo build --release -p clawft-cli --features vector-memory
```

Without `-p`, cargo may try to apply the feature to the workspace root or to crates that don't define it.

### `cargo build --no-default-features` fails

The CLI currently requires the `services` feature because `mcp_tools.rs` unconditionally imports `clawft-services`. Use:

```bash
cargo build --release -p clawft-cli --no-default-features --features services
```

### WASM build fails with "target not installed"

```bash
rustup target add wasm32-wasip1
# or
rustup target add wasm32-wasip2
```

### Cross-compilation fails with missing linker

Install `cross` for hassle-free cross-compilation:

```bash
cargo install cross
./scripts/build/cross-compile.sh x86_64-unknown-linux-musl --use-cross
```

### LTO build is slow

LTO (`lto = true`) and single codegen unit (`codegen-units = 1`) produce smaller binaries but increase compile time significantly. For faster iteration during development, use the debug profile (the default).

If you need a release build faster, you can temporarily override in a local `Cargo.toml` patch or use:

```bash
# Faster release build (larger binary)
cargo build --release -p clawft-cli
# Override LTO for this build only:
CARGO_PROFILE_RELEASE_LTO=false cargo build --release -p clawft-cli
```

### Docker build requires `cross`

The Docker build script cross-compiles for `x86_64-unknown-linux-musl`. If you don't have `cross` installed, it falls back to plain `cargo`, which requires the musl target and toolchain to be installed locally:

```bash
rustup target add x86_64-unknown-linux-musl
# On Ubuntu/Debian:
sudo apt-get install musl-tools
```
