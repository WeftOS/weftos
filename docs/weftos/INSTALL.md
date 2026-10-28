# WeftOS Installation Guide

## Prerequisites

- Rust 1.95+ (`rustup install 1.95`; the workspace `rust-version` and `rust-toolchain.toml` both say 1.95)
- Git
- (Optional) Docker and Docker Compose
- (Optional) Node.js, only for the docs site and the dashboard

## From Source

```bash
git clone https://github.com/WeftOS/weftos.git
cd weftos

# Build weft and weaver in release mode
scripts/build.sh native

# Or build and install both to ~/.cargo/bin
scripts/build.sh install

# Verify
weft --version
weaver version
```

### Debug Builds (faster iteration)

```bash
scripts/build.sh native-debug
```

### Build with Extra Features

```bash
# Single-node production
scripts/build.sh native --features ecc,exochain,os-patterns

# Full feature set
scripts/build.sh native --features ecc,exochain,os-patterns,mesh,cluster,onnx-embeddings,wasm-sandbox
```

See [FEATURE_GATES.md](FEATURE_GATES.md) for the complete feature reference.

## From Docker

```bash
# Start a WeftOS node with docker compose
docker compose up weftos-node

# Or build and run manually
docker build -t weftos .
docker run -p 8080:8080 weftos
```

## Quick Start

```bash
# Initialize WeftOS in your project (the `weftos` binary is from the
# `weftos` crate: `cargo install weftos`)
cd my-project
weftos init

# Start the kernel as a background daemon
weaver kernel start

# Check status
weaver kernel status
```

The native GUI is `weft-gui-egui` (`scripts/build.sh gui-egui`). The old
`gui/` Tauri and React shells no longer exist.

## ONNX Embeddings (optional)

The `onnx-embeddings` feature enables real ONNX model inference for semantic
search instead of the default mock embeddings.

```bash
# Download the default model (all-MiniLM-L6-v2)
scripts/download-model.sh

# Build with ONNX support
scripts/build.sh native --features onnx-embeddings
```

The model is stored in `.weftos/models/` and is not checked into git.

## Compile-time Checks

Before committing, run the full phase gate:

```bash
scripts/build.sh gate
```

This executes 11 checks: build, test, clippy, formatting, and more.

## Troubleshooting

| Problem | Fix |
|---------|-----|
| `rustup` reports Rust < 1.95 | `rustup update stable` |
| Missing `tokio` or `futures` | The `native` feature is not enabled (it is the default) |
| ONNX model not found | Run `scripts/download-model.sh` first |
| `scripts/build.sh` not found | You are not in the project root directory |

## Next Steps

- [FEATURE_GATES.md](FEATURE_GATES.md) -- all compile-time feature flags
- [CONFIGURATION.md](CONFIGURATION.md) -- runtime configuration reference
