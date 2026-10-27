# clawft + WeftOS

[![Release](https://img.shields.io/github/v/release/WeftOS/weftos)](https://github.com/WeftOS/weftos/releases)
[![crates.io](https://img.shields.io/crates/v/weftos)](https://crates.io/crates/weftos)
[![npm](https://img.shields.io/npm/v/@weftos/core)](https://www.npmjs.com/package/@weftos/core)
[![License](https://img.shields.io/crates/l/weftos)](LICENSE)

**AI that remembers everything, runs anywhere, trusts no one, and never stops learning.**

clawft + WeftOS is a secure, ephemeral AI operating system built in Rust. It
replaces the fragile pattern of cloud-dependent chatbots with something
fundamentally different: a distributed kernel that gives AI agents persistent
memory, verifiable reasoning, and constitutional governance — running on
everything from a Raspberry Pi to a browser tab to an air-gapped data center.

## Agent harness (developers & coding agents)

This monorepo **already** carries its agent/development harness — MetaHarness
tasks, Graph ViewSpecs, Plane tracking, and Ruflo/claude-flow orchestration.
You do not scaffold a blank harness onto an empty crate tree.

| Layer | Role |
|-------|------|
| **Rust product** | `weft` / kernel / mesh / governance — ships without Node |
| **In-repo harness** | `.metaharness/` tasks, `config/views/`, AgentDB patterns |
| **Host orchestration** | Ruflo / claude-flow (optional MCP via `.mcp.json`) |
| **Primary score** | `weftosFoundationScore` via `scripts/metaharness/weftos-score.sh` (harness **+** crate/target surface) |
| **Genome readiness** | `metaharness genome` (7-section; often shows READY when scorecard alone looks mid) |
| **Secondary canary** | ADR-041 `metaharness score` (shallow inventory; not multi-OS crate graph) |

**Platform surface (summary):** 50+ workspace crates; cargo-dist across Linux
(gnu/musl × arch), macOS, Windows; WASM/WASI; edge/Android; leaf firmware
hardware crates; sensor / world-model / voice stacks. Full matrix:
[CONTRIBUTING.md](CONTRIBUTING.md#platform-matrix-os-surface).

```bash
scripts/build.sh gate                         # release readiness
scripts/metaharness/score.sh                  # scorecard + genome → .metaharness/
scripts/metaharness/weftos-score.sh           # foundation + OS surface (load-bearing)
scripts/metaharness/run-task.sh gate          # harness-wrapped gate
scripts/metaharness/validate-views.sh         # fusion ViewSpecs (evaluate-only)
```

Policy evolution: **evaluate → receipt → promote** (ADR-096). Freeze the model;
evolve harness and ViewSpecs. MCP is one host integration surface, not the
product genus. Contributing: [CONTRIBUTING.md](CONTRIBUTING.md).

## What's New in v0.2

- **Lego Block Engine** -- 10 composable GUI blocks (Text, Code, Status, Table, Tree, Terminal, Button, Column, Row, Grid, Tabs) with recursive rendering, BlockRegistry, and Zustand+Tauri state management
- **4 Built-in Themes** -- ocean-dark, midnight, paper-light, and high-contrast (WCAG AAA). CSS variable bridge, ThemeProvider, ANSI palette mapping, Tailwind integration via `--weftos-*` custom properties
- **Context Compression** -- Sliding-window context management with first-sentence summarization. Configurable token budget (default 8192). Opt-in via `builder.with_compression(config)`
- **GEPA Prompt Evolution** -- Production-ready Scorer and Learner pipeline stages. FitnessScorer with 4-dimension weighted scoring. TrajectoryLearner with pattern extraction and 4 prompt mutation strategies
- **Local LLM Support** -- OpenAI-compatible provider for Ollama, vLLM, llama.cpp, and LM Studio. Key-optional auth, streaming, model listing. Run `weft agent --model local/llama3.1` to use local inference

---

## The Problems We Solve

### No more context windows

LLMs forget. Every conversation starts from zero. clawft eliminates this with a
**vector-indexed knowledge graph** (ECC) that grows continuously. The causal DAG
tracks how every idea, decision, and outcome relates to every other. HNSW
approximate nearest neighbor search retrieves relevant context in sub-millisecond
time. Your agents don't have a context window — they have a growing, searchable
memory that spans sessions, conversations, and nodes.

### No more hallucination without accountability

Every agent action is recorded in an **append-only hash chain** (ExoChain) with
SHAKE-256 hash linking and Ed25519 + ML-DSA-65 dual signatures. Every tool call,
every IPC message, every governance decision is witnessed. When an agent claims
it did something, you can verify it cryptographically. The chain is not just a
log — it is a tamper-evident audit trail that makes hallucination detectable and
provenance traceable.

### No more trusting the cloud with your data

WeftOS runs **entirely on your hardware**. Local LLM inference via Ollama,
llama.cpp, or vLLM means your prompts never leave your network. The mesh network
uses **Noise Protocol encryption** with post-quantum ML-KEM-768 key exchange —
even a future quantum computer cannot decrypt intercepted traffic. Air-gapped
deployments work identically to connected ones. Your data, your models, your
control.

### No more single points of failure

Agents form **encrypted peer-to-peer mesh clusters** across heterogeneous
devices. A $35 Raspberry Pi running on your desk, a browser tab on your laptop,
and a GPU server in your rack all join the same cluster over mDNS, Kademlia DHT,
or static seed peers. If one node goes down, SWIM heartbeats detect it in
seconds. Services automatically resolve to surviving replicas. Chain state
replicates incrementally. There is no central server to fail.

### No more uncontrolled AI

Every privileged operation passes through a **three-branch constitutional
governance engine** — Legislative (what agents can do), Executive (runtime
approval), Judicial (post-hoc audit). A 5-dimensional effect vector scores every
action on risk, fairness, privacy, novelty, and security. Production environments
enforce strict thresholds. Development environments are lenient. The governance
rules themselves are stored on-chain and synchronized across the cluster. No
single agent, node, or operator can bypass the constitution.

### No more "works on my machine"

The same binary runs on **7 platforms**: Linux, macOS, Windows, browser (WASM),
WASI containers, ARM64 edge devices, and cloud VMs. A browser tab joins the
same mesh as a bare-metal server. Feature flags compile out what you don't need —
a minimal edge build has zero networking dependencies; a full cloud build includes
QUIC transport, Kademlia DHT, WASM sandbox, and container orchestration.

---

## What This Is

```
+-------------------------------------------------------------------+
|                         clawft                                     |
|  AI framework: LLM providers, channels, tools, skills, agents    |
+-------------------------------------------------------------------+
|                         WeftOS Kernel                               |
|  Processes, IPC, governance, ExoChain, mesh networking, apps      |
+-------------------------------------------------------------------+
|                    ECC Cognitive Substrate                          |
|  Causal DAG, cognitive tick, HNSW memory, impulse queue           |
+-------------------------------------------------------------------+
|                      Platform Layer                                |
|  Native | WASI | Browser | Edge | Cloud | Air-Gapped | Docker    |
+-------------------------------------------------------------------+
```

**clawft** is the AI framework layer: connecting to LLM providers (OpenAI,
Anthropic, Ollama, local models), managing conversations across channels
(Telegram, Slack, Discord, CLI, WebSocket), executing tools in WASM sandboxes,
and composing agent skills.

**WeftOS** is the kernel layer: process management with PID allocation and
capability-based RBAC, typed IPC with 7 target types, an append-only ExoChain
for cryptographic audit trails, three-branch constitutional governance, encrypted
mesh networking with post-quantum key exchange, and an application framework
for packaged agent deployments.

**ECC** (Ephemeral Causal Cognition) is the cognitive layer: a causal DAG that
tracks how ideas relate, an adaptive cognitive tick that processes events in
real-time, HNSW approximate nearest neighbor search for semantic memory,
BLAKE3-indexed cross-references linking every structure in the kernel, and an
impulse queue for ephemeral signals that decay rather than accumulate. ECC
operates in three modes — **Act** (real-time conversation), **Analyze**
(post-hoc corpus understanding), and **Generate** (goal-directed planning) —
using the same engine, the same data structures, and the same scoring system.

## Platform Support

| Platform | Binary | Transport | Mesh Role | Use Case |
|----------|--------|-----------|-----------|----------|
| **Linux** (x86_64, aarch64) | `weft` / `weave` | TCP, QUIC, WebSocket | Full node | Servers, workstations, CI/CD |
| **macOS** (x86_64, aarch64) | `weft` / `weave` | TCP, QUIC, WebSocket | Full node | Development, local inference |
| **Windows** (x86_64) | `weft.exe` / `weave.exe` | TCP, WebSocket | Full node | Enterprise workstations |
| **Raspberry Pi** (ARM64) | `weft` | TCP, mDNS | Edge node | IoT, local-first AI, offline |
| **Browser** (wasm32) | `clawft-wasm` | WebSocket | Light node | Web UI, mobile access |
| **WASI** (wasm32-wasi) | `clawft-wasi` | WebSocket | Sandboxed | Serverless, plugin isolation |
| **Cloud VM** | `weft` / `weave` | TCP, QUIC, WebSocket, Kademlia | Seed node | GPU inference, DHT bootstrap |
| **Docker** | `alpine:weft` | Configurable | Any | Sidecar orchestration, k8s |
| **Air-gapped** | `weft` | TCP (local) | Isolated cluster | Classified, medical, financial |

All platforms join the same mesh. A Raspberry Pi on your desk discovers a
MacBook over mDNS. The MacBook connects to a cloud GPU server via QUIC. A
browser tab on your phone joins over WebSocket. An air-gapped server in a
secure facility runs a self-contained cluster with local Ollama inference.
They all share the same governance constitution, chain state, and service
registry — or they run completely independently. Your choice.

### Deployment Patterns

**Personal AI assistant** — Single node on a laptop. Local LLM (Ollama).
No cloud, no API keys, no data leaving the machine. Full ExoChain audit trail.

**Team mesh** — 3-5 nodes on a LAN. mDNS auto-discovery. Shared knowledge
graph. Agents specialize: one does code review, one monitors CI, one handles
Slack. Governance prevents any agent from exceeding its role.

**Edge + cloud hybrid** — Raspberry Pis at branch offices handle local queries
with small models. Complex requests route to a central GPU server. Chain state
replicates incrementally. If the WAN goes down, edge nodes continue operating
autonomously.

**Air-gapped secure** — Military, medical, or financial deployment where no
data can leave the network. Local inference only. Post-quantum encryption on
all internal mesh links. ExoChain provides a cryptographic audit trail for
compliance. The governance engine enforces data classification policies.

**Browser-first** — Lightweight deployment where users access agents through
a web UI. The browser WASM binary connects to backend nodes via WebSocket.
Browser agents start with restricted capabilities; elevation requires governance
approval.

## Key Features

### clawft (AI Framework)

- **Multi-provider LLM** — OpenAI, Anthropic, Ollama, vLLM, llama.cpp, and any
  OpenAI-compatible API behind a single trait
- **Multi-channel messaging** — Telegram, Slack, Discord, CLI, WebSocket, HTTP
  with a unified PluginHost architecture
- **6-stage pipeline** — Classifier, Router, Assembler, Transport, Scorer,
  Learner for structured message processing
- **Tool system** — 27 built-in tools (filesystem, shell, memory, web, agent
  management) with WASM sandboxing
- **MCP integration** — Dual-mode Model Context Protocol: expose tools as server
  or connect to external MCP servers as client
- **Skills and agents** — Declarative SKILL.md format, multi-agent spawning,
  inter-agent IPC
- **Plugin system** — Git, Cargo, OAuth2, TreeSitter, browser, calendar,
  containers — all hot-reloadable

### WeftOS (Kernel)

- **Process management** — PID allocation, state machine (Running/Suspended/
  Stopping), resource tracking, agent supervisor with spawn/stop/restart
- **IPC** — Typed `KernelMessage` envelopes with 7 target types (Process, Topic,
  Broadcast, Service, ServiceMethod, RemoteNode, Kernel) and 6 payload variants
- **Capability-based security** — Per-agent RBAC with IpcScope (All/ParentOnly/
  Restricted/Topic/None), ToolPermissions, SandboxPolicy, ResourceLimits
- **ExoChain** — Append-only hash chain with SHAKE-256 linking, Ed25519 + ML-DSA-65
  dual signing, RVF segment persistence, witness chains, and lineage records
- **Three-branch governance** — Legislative/Executive/Judicial branches evaluate
  every privileged operation against 5-dimensional effect vectors (risk, fairness,
  privacy, novelty, security). Environment-scoped: dev is lenient, prod is strict.
- **WASM sandbox** — Wasmtime-based tool execution with fuel metering (CPU
  budget), memory limits (allocation bomb prevention), and complete host
  filesystem isolation
- **Container integration** — Docker/Podman sidecar orchestration with health
  check propagation to the kernel health system
- **Application framework** — `weftapp.toml` manifests, install/start/stop
  lifecycle, agent spawning with capability wiring from manifests

### Mesh Networking (K6)

- **Transport-agnostic** — TCP and WebSocket today, QUIC (quinn) ready.
  `MeshTransport` trait makes any byte stream a mesh link.
- **Noise Protocol encryption** — XX pattern for first contact, IK for known
  peers. All inter-node traffic encrypted. Ed25519 static keys.
- **Post-quantum protection** — Hybrid ML-KEM-768 + X25519 key exchange.
  Graceful degradation when one side lacks KEM support. Protects against
  store-now-decrypt-later quantum attacks.
- **Peer discovery** — Static seed peers, peer exchange during handshake, mDNS
  for LAN discovery (UDP multicast), Kademlia DHT for WAN discovery (XOR
  distance, k-buckets, governance-namespaced keys).
- **Cross-node IPC** — `MeshIpcEnvelope` transports `KernelMessage` across nodes
  transparently. Hop counting prevents routing loops. Bloom filter deduplication
  prevents double delivery.
- **Distributed process table** — CRDT-based last-writer-wins process
  advertisements. ConsistentHashRing for PID-to-node assignment.
- **Service discovery** — `ClusterServiceRegistry` with round-robin and
  affinity-based resolution. TTL-cached with negative cache. Circuit breaker
  prevents cascade failures.
- **Chain replication** — Incremental `tail_from()` sync, fork detection,
  bridge events anchoring remote chain heads, backpressure with checkpoint
  catch-up when >1000 events behind.
- **Tree synchronization** — Merkle root comparison, incremental diff transfer
  of only changed subtrees. Signed mutations verified against node Ed25519 keys.
- **SWIM heartbeat** — Alive/Suspect/Dead state machine with configurable
  timeouts. Direct ping + indirect probe via random witnesses.
- **Metadata consensus** — Raft-style log for service registry and process table.
  CRDT gossip for eventually-consistent state convergence.

### ECC (Ephemeral Causal Cognition)

The cognitive substrate that gives WeftOS agents the ability to reason about
causality, search semantic memory, and process ephemeral signals in real-time.

- **Causal DAG** — Typed, weighted edges tracking how ideas, events, and
  decisions relate. BFS traversal and path finding. Every edge links to the
  chain event that created it.
- **Cognitive tick** — An adaptive processing interval that fires regularly,
  processing accumulated causal edges, updating the HNSW index, and detecting
  drift. Auto-calibrated at boot time based on hardware capability.
- **HNSW search** — Approximate nearest neighbor search over high-dimensional
  embeddings. Thread-safe wrapper around `instant-distance`. Enables semantic
  queries like "find the 10 most similar past decisions."
- **Cross-references** — BLAKE3-based `UniversalNodeId` links structures across
  the kernel: chain events to tree nodes to causal edges to HNSW entries.
  Bidirectional traversal.
- **Impulse queue** — HLC-sorted ephemeral events with short TTL. For real-time
  cognitive coordination: "this agent just completed a task" signals that decay
  rather than persist.
- **Boot-time calibration** — Benchmarks compute time per tick, measures p50/p95
  latency, auto-adjusts tick interval for the hardware. Prevents cognitive
  overload on constrained devices.
- **Three operating modes** — The same engine runs in three modes:
  - **Act**: Real-time conversation processing within the cognitive tick
  - **Analyze**: Post-hoc understanding of existing corpora (transcripts, PRs,
    research papers)
  - **Generate**: Goal-directed content generation using causal planning
- **Cluster advertisement** — `NodeEccCapability` advertises each node's
  cognitive capacity (tick interval, compute headroom, HNSW vector count, causal
  edge count, spectral analysis support) so the mesh can route ECC queries to
  capable nodes.

## Quick Start

### Install

```sh
# Shell installer (Linux / macOS)
curl -fsSL https://github.com/WeftOS/weftos/releases/latest/download/clawft-cli-installer.sh | sh

# Or via Homebrew
brew install weave-logic-ai/tap/clawft-cli

# Or via cargo
cargo install weftos

# Or via Docker
docker pull ghcr.io/weftos/weftos:latest
```

Pre-built binaries for Linux (x86_64, ARM64), macOS (Intel, Apple Silicon), and Windows are on [GitHub Releases](https://github.com/WeftOS/weftos/releases). WASM module available as [`@weftos/core`](https://www.npmjs.com/package/@weftos/core) on npm. All framework crates on [crates.io](https://crates.io/crates/weftos).

### Configure

```sh
export OPENAI_API_KEY="sk-..."
```

### Run an agent

```sh
weaver kernel start                      # the agent runs through the kernel daemon
weft agent
weft agent -m "Summarize this project"
weft agent --local -m "Summarize this project"   # no daemon: run in-process
```

### Run WeftOS kernel

```sh
weave boot                # Start the kernel
weave ps                  # List running processes
weave service list        # List registered services
weave chain status        # ExoChain status
weave app install myapp   # Install an application
weave app list            # List installed apps
weave cluster peers       # Show mesh peers
weave ecc status          # ECC cognitive substrate status
```

## Architecture

clawft is organized as a Cargo workspace with 22 crates:

```
clawft-cli / clawft-weave     CLI binaries (weft / weave)
  |
clawft-kernel                  WeftOS kernel (K0-K6)
  |  |- boot, process, supervisor, capability
  |  |- ipc, a2a, topic, cron, health, service
  |  |- chain, tree_manager, gate, governance
  |  |- wasm_runner, container, app, agency
  |  |- mesh_*, mesh_tcp, mesh_ws (17 networking modules)
  |  |- causal, cognitive_tick, hnsw_service (ECC)
  |
clawft-core                    Agent engine, MessageBus, pipeline
clawft-llm                     LLM provider abstraction
clawft-tools                   Tool implementations
clawft-channels                Channel plugins
clawft-services                Background services
clawft-plugin-*                Plugin crates (git, cargo, oauth2, ...)
clawft-security                Security policies
clawft-wasm                    Browser / WASI WASM entrypoint
clawft-wasm-host               Native wasmtime plugin host (WEFT-398)
exo-resource-tree              Merkle resource tree with mutation log
```

### Feature Flags

| Feature | Crate | Description |
|---------|-------|-------------|
| `native` | `clawft-kernel` | Tokio runtime, native file I/O (default) |
| `exochain` | `clawft-kernel` | ExoChain hash chain + resource tree + gate backends |
| `cluster` | `clawft-kernel` | Multi-node clustering via ruvector-cluster/raft |
| `mesh` | `clawft-kernel` | Mesh networking (TCP, WebSocket, discovery, IPC) |
| `ecc` | `clawft-kernel` | ECC cognitive substrate (causal DAG, HNSW, impulse) |
| `wasm-sandbox` | `clawft-kernel` | Wasmtime WASM tool execution |
| `containers` | `clawft-kernel` | Docker/Podman container integration |
| `vector-memory` | `clawft-core` | IntelligentRouter, VectorStore, SessionIndexer |

Build configurations:

```sh
# Minimal (single-node, no networking)
cargo build --release

# Full kernel with mesh networking
cargo build --release --features "native,exochain,cluster,mesh,ecc"

# Everything
cargo build --release --features "native,exochain,cluster,mesh,ecc,wasm-sandbox,containers"

# Browser / WASI WASM — always use the unified script (handles
# --no-default-features --features browser, release-wasm profile, wasm-bindgen)
scripts/build.sh browser   # wasm32-unknown-unknown (browser tab)
scripts/build.sh wasi      # wasm32-wasip2 (edge / wasmtime hosts)
```

## Testing

5,040 tests across 22 crates. Zero regressions across feature combinations.

```sh
# All tests (full features)
cargo test -p clawft-kernel --features "native,exochain,cluster,mesh,wasm-sandbox"

# Production / merge gate (also what CI enforces via pr-gates.yml)
scripts/build.sh gate

# Build check
scripts/build.sh check

# Lint
scripts/build.sh clippy

# Optional: local K3–K6 phase rehearsal only (WEFT-464 — not wired into CI)
scripts/k6-gate.sh
```

## Documentation

The documentation site is built with [Fumadocs](https://fumadocs.vercel.app/)
and lives in `docs/src/`:

```sh
cd docs/src
npm install
npm run dev     # http://localhost:3000
```

32 pages across two sections:
- **clawft** — Getting started, architecture, CLI reference, configuration,
  plugins, providers, channels, tools, skills, browser mode, deployment, security
- **WeftOS** — Architecture, kernel phases (K0-K6), boot sequence, process table,
  IPC, capabilities, ExoChain, governance, WASM sandbox, containers, app framework,
  mesh networking, discovery, clustering, ECC, security, decisions, kernel guide

## Governance Model

WeftOS uses a three-branch constitutional governance model inspired by
separation of powers:

| Branch | Role | Example Rules |
|--------|------|---------------|
| **Legislative** | Define what agents can do | "Agents may not access /etc" |
| **Executive** | Approve runtime operations | "Auto-approve low-risk tool calls" |
| **Judicial** | Review and audit after the fact | "Flag operations with risk > 0.8" |

Every privileged operation is evaluated against a 5-dimensional **EffectVector**:

| Dimension | Measures |
|-----------|----------|
| Risk | Potential for harm or data loss |
| Fairness | Equitable resource distribution |
| Privacy | Data exposure and access scope |
| Novelty | Deviation from established patterns |
| Security | Attack surface and vulnerability |

Environments apply different thresholds:
- **Development**: Lenient (threshold 0.9) — rapid iteration
- **Staging**: Normal (threshold 0.6) — pre-production validation
- **Production**: Strict (threshold 0.3) — maximum safety

## Security

- **Capability-based access control** — Every agent has explicit capabilities
  (IPC scope, tool permissions, spawn rights, resource limits)
- **Dual-layer governance gate** — Routing-time check before message delivery +
  handler-time check before tool execution
- **Noise Protocol encryption** — All inter-node traffic encrypted with forward
  secrecy and mutual authentication
- **Post-quantum cryptography** — Hybrid ML-KEM-768 + X25519 key exchange,
  Ed25519 + ML-DSA-65 dual chain signing
- **WASM sandbox isolation** — Fuel metering, memory limits, no host filesystem
  access by default
- **Message size limits** — 16 MiB maximum at all deserialization boundaries
- **Rate limiting** — Configurable peer addition rate limiting
- **Browser sandboxing** — Browser agents start with `IpcScope::Restricted`,
  capability elevation requires governance gate approval
- **Tamper-evident audit trail** — ExoChain with SHAKE-256 hash linking,
  integrity verification, witness chains

## Building from Source

### Prerequisites

- Rust 1.93+ (edition 2024)
- Node.js 18+ (for documentation site)
- Docker (optional, for container features)

### Build

```sh
git clone https://github.com/WeftOS/weftos.git
cd clawft
scripts/build.sh native          # Release binary
scripts/build.sh native-debug    # Debug binary (fast iteration)
scripts/build.sh test            # Run tests
scripts/build.sh gate            # Full phase gate (11 checks)
scripts/build.sh all             # Everything (native + wasi + browser + ui)
```

### Browser Build

**W-BROWSER** runs the agent stack in a browser tab: compile to
`wasm32-unknown-unknown`, browser HTTP/LLM transport, the `www/` test harness,
and the agent pipeline wired in-tab (wasm-bindgen `init` / `send_message`).
This is distinct from the **WASI** path (`wasm32-wasip2` for wasmtime/edge).

```sh
# Prerequisites (once)
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli   # optional but needed for JS glue / serve

# Browser WASM (release-wasm + --no-default-features --features browser)
scripts/build.sh browser

# Serve the browser test harness (default port 8080)
scripts/build.sh serve

# Headless Chrome regression suite (wasm-pack + chromedriver)
scripts/build.sh test-browser

# WASI WASM for edge / server hosts (not the browser)
scripts/build.sh wasi
```

`scripts/build.sh browser` builds `clawft-wasm` and, when `wasm-bindgen` is
installed, writes JS glue to `crates/clawft-wasm/www/pkg/`. Prefer this over
raw `cargo build --target wasm32-unknown-unknown` so feature flags and the
bindgen step stay correct.

Full guides under **[docs/browser/](docs/browser/)**:

| Doc | Topic |
|-----|--------|
| [quickstart.md](docs/browser/quickstart.md) | Minimal HTML + WASM load |
| [building.md](docs/browser/building.md) | Toolchain, flags, outputs |
| [architecture.md](docs/browser/architecture.md) | Hybrid in-tab agent model |
| [deployment.md](docs/browser/deployment.md) | Shipping the browser bundle |
| [api-reference.md](docs/browser/api-reference.md) | wasm-bindgen surface |

Also see [ADR-083](docs/adr/adr-083-browser-wasm-support.md) (browser WASM) and
[ADR-044](docs/adr/adr-044-wasm-wasip1-target.md) (WASI target). For the React
dashboard browser-only mode (`?mode=wasm`), see [clawft-ui/README.md](clawft-ui/README.md).

## Contributing

1. Fork the repository and create a feature branch
2. Write tests for new functionality
3. Run `scripts/build.sh gate` before submitting
4. Follow the [kernel developer guide](docs/src/content/docs/weftos/kernel-guide.mdx)
5. Submit a pull request

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT License ([LICENSE-MIT](LICENSE-MIT) or
  <http://opensource.org/licenses/MIT>)

at your option.

## Attribution

clawft + WeftOS builds on ideas and patterns from:

- [claude-code](https://github.com/anthropics/claude-code) — Anthropic's agentic coding tool
- [claude-flow](https://github.com/ruvnet/claude-flow) — Multi-agent orchestration framework
- [ruvector](https://github.com/ruvnet/ruvector) — Rust vector operations and distributed consensus
- The WeftOS kernel architecture draws from microkernel OS design (L4, seL4),
  capability-based security (Capsicum), and distributed systems research
  (SWIM, Raft, CRDTs, Kademlia)
