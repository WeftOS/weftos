# Feature Flag Reference

> **Status (2026-10-27).** Stub candidate. It covers 22 workspace crates; the workspace now has 65 members, so most crates' flags are not listed. The kernel-only reference is [FEATURE_GATES.md](FEATURE_GATES.md) and the developer-oriented catalogue is [../development/feature-flags.md](../development/feature-flags.md). Proposed: prune this page in favour of those two once a ruling is given; nothing has been deleted.

This document catalogs every Cargo feature flag across all 22 workspace crates.

## Workspace Crates Overview

The workspace contains 22 crates. Feature flags are defined in each crate's
`Cargo.toml` under `[features]`.

---

## clawft-kernel

The kernel crate has the most feature flags, controlling which subsystems are
compiled in.

| Feature | What it enables | Key dependencies |
|---------|----------------|------------------|
| `native` (default) | Tokio async runtime, full kernel boot | tokio, tokio-util, futures |
| `ecc` | Cognitive substrate: causal graph, HNSW vector memory, tick loop | blake3, clawft-core/vector-memory |
| `exochain` | Cryptographic audit trail, resource tree, governance events | rvf-crypto, rvf-wire, rvf-types, rvf-runtime, exo-resource-tree, ed25519-dalek, ciborium |
| `mesh` | P2P WebSocket networking between nodes | ed25519-dalek, tokio-tungstenite, futures-util |
| `cluster` | Distributed coordination: Raft consensus, sharding, replication | ruvector-cluster, ruvector-raft, ruvector-replication, parking_lot |
| `os-patterns` | Self-healing, dead-letter queue, metrics, structured logging | (implies `exochain`) |
| `tilezero` | TileZero governance gate | cognitum-gate-tilezero (implies `exochain`) |
| `onnx-embeddings` | Real ONNX model inference for embeddings | ort, ndarray |
| `wasm-sandbox` | Wasmtime-based tool execution sandbox | wasmtime, wasmtime-wasi |
| `containers` | Container runtime integration markers | (none extra) |

### Dependency Graph

```
tilezero ──> exochain
os-patterns ──> exochain
cluster (independent)
mesh (independent)
ecc (independent)
onnx-embeddings (independent)
wasm-sandbox (independent)
containers (independent)
```

---

## weftos

The top-level WeftOS binary crate. All features delegate to `clawft-kernel`.

| Feature | What it enables |
|---------|----------------|
| `native` (default) | Tokio runtime via clawft-kernel/native |
| `exochain` | Audit trail via clawft-kernel/exochain |
| `cluster` | Clustering via clawft-kernel/cluster |
| `mesh` | Mesh networking via clawft-kernel/mesh |
| `ecc` | Cognitive substrate via clawft-kernel/ecc |
| `wasm-sandbox` | WASM sandbox via clawft-kernel/wasm-sandbox |
| `containers` | Containers via clawft-kernel/containers |
| `os-patterns` | OS patterns via clawft-kernel/os-patterns |
| `full` | All of the above |

---

## clawft-core

| Feature | What it enables | Key dependencies |
|---------|----------------|------------------|
| `full` (default) | Full feature set marker | (none extra) |
| `native` (default) | Tokio runtime, native filesystem, file watcher | tokio, tokio-util, dirs, notify |
| `browser` | Browser/WASM target support | wasm-bindgen-futures, js-sys, futures-channel |
| `vector-memory` | In-memory HNSW vector store | rand, instant-distance |
| `rvf` | RVF deep integration | rvf-runtime, rvf-types, sha2, reqwest (implies vector-memory) |
| `signing` | Ed25519 cryptographic signing | ed25519-dalek, sha2, rand |

---

## clawft-cli

| Feature | What it enables |
|---------|----------------|
| `channels` (default) | Channel integrations (Telegram, Slack, etc.) |
| `services` (default) | Background services (scheduler, delegation) |
| `delegate` (default) | Agent delegation support |
| `api` (default) | REST/WebSocket API server |
| `vector-memory` | Vector memory for semantic search |
| `voice` | Voice input/output (VAD, wake word) |

---

## clawft-weave

The `weave` CLI binary for kernel management.

| Feature | What it enables |
|---------|----------------|
| `cluster` | Cluster management commands |
| `exochain` | Chain inspection commands |
| `rvf-rpc` | RVF wire protocol for remote queries |

---

## clawft-types

| Feature | What it enables |
|---------|----------------|
| `native` (default) | Home directory resolution via dirs |
| `browser` | UUID generation via JS, getrandom for WASM |

---

## clawft-platform

| Feature | What it enables |
|---------|----------------|
| `native` (default) | Tokio runtime, reqwest HTTP, dirs for home directory |
| `browser` | wasm-bindgen, web-sys, js-sys for browser targets |

---

## clawft-llm

| Feature | What it enables |
|---------|----------------|
| `native` (default) | reqwest with rustls-tls, Tokio runtime |
| `browser` | Browser-compatible HTTP via Fetch API |

---

## clawft-tools

| Feature | What it enables |
|---------|----------------|
| `native-exec` (default) | Native shell command execution |
| `native` (default) | Tokio runtime, native platform |
| `browser` | Browser target support |
| `canvas` | Canvas rendering support |
| `vector-memory` | Vector memory via clawft-core |
| `delegate` | Agent delegation via clawft-services |
| `voice` | Voice tools via clawft-plugin |

---

## clawft-plugin

| Feature | What it enables |
|---------|----------------|
| `native` (default) | tokio-util for native cancellation |
| `voice` | Voice pipeline (VAD + wake word detection) |
| `voice-stt` | Speech-to-text marker |
| `voice-tts` | Text-to-speech marker |
| `voice-vad` | Voice activity detection |
| `voice-wake` | Wake word detection |

---

## clawft-channels

| Feature | What it enables |
|---------|----------------|
| `email` | Email channel integration |
| `whatsapp` | WhatsApp channel integration |
| `signal` | Signal channel integration |
| `matrix` | Matrix channel integration |
| `google-chat` | Google Chat channel integration |
| `irc` | IRC channel integration |
| `teams` | Microsoft Teams channel integration |
| `voice` | Voice channel (VAD, STT/TTS substrate client, Talk-Mode controller) |
| `voice-real-audio` | Real cpal capture/playback (implies `voice`) |
| `real-audio-test` | cpal-touching tests (implies `voice-real-audio`) |
| `voice-xai` | ADR-074 V0 xAI Realtime WebSocket client (implies `voice`; WEFT-689) |

---

## clawft-services

| Feature | What it enables |
|---------|----------------|
| `delegate` | Regex-based delegation routing |
| `rvf` | RVF integration markers |
| `test-utils` | Test utility helpers |
| `clawhub` | ClawHub skill marketplace |
| `api` | REST/WS API server (axum, tower-http) |

---

## clawft-wasm

| Feature | What it enables |
|---------|----------------|
| `browser` | Full browser WASM build (clawft-core, clawft-llm, clawft-tools, wasm-bindgen) |
| `wasm-plugins` | WASM plugin host (wasmtime) |
| `alloc-talc` | Talc allocator for WASM |
| `alloc-lol` | lol_alloc allocator for WASM |
| `alloc-tracing` | Allocation tracing |

---

## clawft-plugin-treesitter

| Feature | What it enables |
|---------|----------------|
| `rust` | Rust grammar for tree-sitter |
| `typescript` | TypeScript grammar for tree-sitter |
| `python` | Python grammar for tree-sitter |
| `javascript` | JavaScript grammar for tree-sitter |

---

## Crates with No Feature Flags

The following crates define no feature flags (empty `[features]` or no section):

- clawft-security
- clawft-plugin-browser
- clawft-plugin-calendar
- clawft-plugin-cargo
- clawft-plugin-containers
- clawft-plugin-git
- clawft-plugin-oauth2
- exo-resource-tree

---

## Recommended Combinations

### Minimal (development, fast builds)

```bash
scripts/build.sh native
# Only the default `native` feature. Fastest compile times.
```

**What you get**: Process table, agent supervision, IPC, basic tools.
**What you miss**: No audit trail, no cognitive substrate, no mesh.

### Standard (single-node production)

```bash
scripts/build.sh native --features ecc,exochain,os-patterns
```

**What you get**: Cognitive substrate (causal graph, HNSW, tick loop),
cryptographic audit trail, self-healing, structured metrics.
**What you miss**: No mesh networking, no clustering, no WASM sandbox.

### Full cluster

```bash
scripts/build.sh native --features ecc,exochain,os-patterns,mesh,cluster
```

**What you get**: Everything in Standard plus P2P mesh networking and
Raft-based distributed consensus.

### Everything

```bash
scripts/build.sh native --features ecc,exochain,os-patterns,mesh,cluster,onnx-embeddings,wasm-sandbox,containers,tilezero
```

Or use the `weftos` crate's `full` feature:

```bash
cargo build -p weftos --features full
```

**Requirements**: ONNX model downloaded (`scripts/download-model.sh`),
Wasmtime-compatible host.

---

## K-Phase Feature Mapping

| K-Phase | Features Required |
|---------|------------------|
| K0 (Boot) | `native` |
| K1 (Supervisor) | `native` |
| K2 (IPC) | `native` |
| K3 (WASM) | `native`, `wasm-sandbox` |
| K3c (ECC) | `native`, `ecc` |
| K4 (Containers) | `native`, `containers`, `exochain` |
| K5 (Apps) | `native`, `exochain` |
| K6 (Mesh) | `native`, `mesh`, `cluster` |
| OS Patterns | `native`, `exochain`, `os-patterns` |

---

## Checking Active Features

At runtime, `weave kernel status` reports which feature gates were compiled
in. During development, `scripts/build.sh check` validates that the selected
feature combination compiles cleanly.

## Adding a New Feature Gate

1. Add the feature and its optional deps to `crates/clawft-kernel/Cargo.toml`.
2. Guard code with `#[cfg(feature = "your-feature")]`.
3. Add a row to the tables above.
4. Run `scripts/build.sh gate` to verify all combinations still compile.

## See Also

- [configuration-reference.md](configuration-reference.md) -- runtime config
- [INSTALL.md](INSTALL.md) -- build instructions with feature examples
- [k-phases.md](k-phases.md) -- kernel phase descriptions
