# WeftOS Quick Start

This guide gets you from zero to a running WeftOS kernel in a project
directory.

## Prerequisites

- **Rust 1.95+** -- install via [rustup](https://rustup.rs/):

  ```bash
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
  rustup update
  ```

- **Git**

## Install from Source

```bash
git clone https://github.com/WeftOS/weftos.git
cd weftos

# Build the two main binaries
scripts/build.sh native

# The binaries are in target/release/
# - weaver  (WeftOS daemon and operator CLI, crate clawft-weave)
# - weft    (agent CLI, crate clawft-cli)

# Copy to your PATH (optional)
cp target/release/weaver target/release/weft ~/.local/bin/
```

`scripts/build.sh native` builds only `weft` and `weaver`. The `weftos`
command used in the next sections (`init`, `boot`, `status`) is a third
binary from the `weftos` crate: `cargo install weftos`, or
`cargo build -p weftos` in a checkout. `weaver init` also writes a
`weave.toml`; see [the CLI reference](../reference/cli.md).

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

## Kernel Management with `weaver`

The `weaver` CLI provides detailed kernel management:

```bash
# Start kernel as a background daemon
weaver kernel start

# Start in foreground (blocking)
weaver kernel start --foreground

# Check kernel state, uptime, process/service counts
weaver kernel status

# List registered services
weaver kernel services

# List process table entries
weaver kernel ps

# Stream logs in real time
weaver kernel attach

# Show recent log entries
weaver kernel logs -n 50

# Stop the daemon
weaver kernel stop

# Restart the daemon
weaver kernel restart
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
