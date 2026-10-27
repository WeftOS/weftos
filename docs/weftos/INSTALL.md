# WeftOS Installation Guide

## Prerequisites

- Rust 1.93+ (`rustup install 1.93`)
- Git
- (Optional) Docker and Docker Compose
- (Optional) Node.js 18+ for the K8 GUI

## From Source

```bash
git clone https://github.com/WeftOS/weftos.git
cd clawft

# Build the CLI in release mode
scripts/build.sh native

# Or build directly with cargo (not recommended for regular use):
cargo build --release -p clawft-cli

# Install the CLI binary
cargo install --path crates/clawft-cli

# Verify
weft --version
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
# Initialize WeftOS in your project
cd my-project
weftos init

# Start the kernel
weave boot

# Check status
weave status
```

### K8 GUI (optional)

```bash
cd gui
npm install
npm run dev
# Open http://localhost:5173
```

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
| `rustup` reports Rust < 1.93 | `rustup update stable` |
| Missing `tokio` or `futures` | The `native` feature is not enabled (it is the default) |
| ONNX model not found | Run `scripts/download-model.sh` first |
| `scripts/build.sh` not found | You are not in the project root directory |

## Next Steps

- [FEATURE_GATES.md](FEATURE_GATES.md) -- all compile-time feature flags
- [CONFIGURATION.md](CONFIGURATION.md) -- runtime configuration reference
