# WeftOS Quick Start

This guide gets you from zero to a running WeftOS kernel in a project
directory.

## Prerequisites

- **Rust 1.93+** -- install via [rustup](https://rustup.rs/):

  ```bash
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
  rustup update
  ```

- **Git**

## Install from Source

```bash
git clone https://github.com/WeftOS/weftos.git
cd clawft

# Build the WeftOS binary
scripts/build.sh native

# The binaries are in target/release/
# - weftos  (WeftOS daemon)
# - weave   (kernel management CLI)
# - weft    (agent CLI)

# Copy to your PATH (optional)
cp target/release/weftos target/release/weave ~/.local/bin/
```

## Initialize a Project

Navigate to any project directory and run `weftos init`:

```bash
cd ~/my-project
weftos init
```

This creates two things:

1. **`weave.toml`** -- project configuration (tick rate, sources, embedding,
   governance, mesh settings). See
   [configuration-reference.md](configuration-reference.md) for all fields.

2. **`.weftos/`** -- runtime state directory containing:
   - `chain/` -- local chain checkpoints
   - `tree/` -- resource tree state
   - `logs/` -- kernel logs
   - `artifacts/` -- build/analysis artifacts

The `.weftos/` directory is automatically added to `.gitignore` if one exists.

If the project is already initialized, use `--force` to reinitialize:

```bash
weftos init --force
```

## Boot the Kernel

```bash
weftos boot
```

Output:

```
Booting WeftOS in /home/user/my-project...
WeftOS running
  State: Running
  Services: 3
  Processes: 0

Press Ctrl+C to stop.
```

The kernel runs in the foreground. Press Ctrl+C for graceful shutdown.

## Check Status

```bash
weftos status
```

Output when initialized:

```
WeftOS initialized in current directory
  Config: weave.toml
  Runtime: .weftos/
```

## Kernel Management with `weave`

The `weave` CLI provides detailed kernel management:

```bash
# Start kernel as a background daemon
weave kernel start

# Start in foreground (blocking)
weave kernel start --foreground

# Check kernel state, uptime, process/service counts
weave kernel status

# List registered services
weave kernel services

# List process table entries
weave kernel ps

# Stream logs in real time
weave kernel attach

# Show recent log entries
weave kernel logs -n 50

# Stop the daemon
weave kernel stop

# Restart the daemon
weave kernel restart
```

## Build with More Features

The default build includes only the `native` feature (process table, agent
supervision, IPC). For production use, enable additional subsystems:

```bash
# Single-node production: cognitive substrate + audit trail + self-healing
scripts/build.sh native --features ecc,exochain,os-patterns

# Multi-node: add mesh networking and clustering
scripts/build.sh native --features ecc,exochain,os-patterns,mesh,cluster

# Everything
scripts/build.sh native --features ecc,exochain,os-patterns,mesh,cluster,onnx-embeddings,wasm-sandbox
```

See [feature-flags.md](feature-flags.md) for the complete reference.

## Project Structure After Init

```
my-project/
  weave.toml          # WeftOS configuration
  .weftos/            # Runtime state (gitignored)
    chain/            # Chain checkpoints
    tree/             # Resource tree
    logs/             # Kernel logs
    artifacts/        # Analysis output
  .gitignore          # Updated with .weftos/ entry
  ... your code ...
```

## What Next

- [configuration-reference.md](configuration-reference.md) -- every
  `weave.toml` field with types, defaults, and examples
- [feature-flags.md](feature-flags.md) -- compile-time feature flags across
  all crates
- [INSTALL.md](INSTALL.md) -- detailed installation (Docker, ONNX embeddings,
  debug builds)
- [k-phases.md](k-phases.md) -- kernel phase architecture (K0-K6)
- [kernel-modules.md](kernel-modules.md) -- kernel module descriptions
- [architecture.md](architecture.md) -- system architecture overview
