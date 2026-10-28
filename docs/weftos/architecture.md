# WeftOS Kernel Architecture

> **Status (2026-10-27).** Kernel crate map as of 0.7.0 (2026-04-28). The current two-layer picture, the mesh and placement subsystems and the Cogs repository boundary are in [../architecture/overview.md](../architecture/overview.md); this page does not cover them.

This document describes the WeftOS kernel layer as implemented in `crates/clawft-kernel/`.
The kernel sits between the CLI/API surface (`clawft-weave`, `clawft-cli`) and the core
agent runtime (`clawft-core`). It provides OS-like abstractions for agent orchestration:
process management, IPC, governance, cryptographic audit trails, and resource namespacing.

## Crate Map

```
clawft-cli          weft binary (user-facing CLI)
clawft-weave        weaver binary (operator CLI + daemon)
    |                   |
    v                   v
clawft-kernel       OS abstractions (this doc)
    |
    v
clawft-core         Agent loop, message bus, pipeline, sessions
    |
    v
clawft-platform     Platform traits (native / browser)
clawft-types        Shared domain types & config
clawft-llm          LLM provider abstraction
```

## Kernel Modules (25 source files)

| Module | Purpose | K-Phase |
|--------|---------|---------|
| `boot.rs` | Kernel lifecycle state machine (Booting → Running → Halted) | K0 |
| `console.rs` | Boot event log, phase tracking, structured output | K0 |
| `config.rs` | KernelConfig extension traits | K0 |
| `error.rs` | KernelError enum + KernelResult type alias | K0 |
| `process.rs` | PID allocation, ProcessTable, state machine | K1 |
| `supervisor.rs` | Agent spawn/stop/restart, SpawnRequest/SpawnResult; K2.1 adds SpawnBackend enum (Native/Wasm/Container/Tee/Remote) | K1 |
| `capability.rs` | RBAC capabilities, IpcScope, SandboxPolicy, ResourceLimits | K1 |
| `ipc.rs` | Typed message envelopes over MessageBus; K2.1 expands MessageTarget with Service routing | K2 |
| `a2a.rs` | Agent-to-agent routing with capability checks | K2 |
| `topic.rs` | Pub/sub TopicRouter with subscriptions | K2 |
| `agent_loop.rs` | Built-in kernel work loop (message handling, gate checks) | K2 |
| `cron.rs` | CronService for scheduled job execution | K2 |
| `health.rs` | HealthSystem with aggregated checks | K2 |
| `service.rs` | ServiceRegistry with named lifecycle management; K2.1 adds ServiceEntry, ServiceEndpoint, AuditLevel | K2 |
| `wasm_runner.rs` | WASM tool execution with fuel metering | K3 |
| `container.rs` | ContainerManager for Docker/Podman sidecars | K4 |
| `app.rs` | AppManager, manifest parsing, app lifecycle | K5 |
| `cluster.rs` | ClusterMembership, peer tracking, node state | K6 |
| `environment.rs` | Governance-scoped dev/staging/prod environments | K6 |
| `governance.rs` | Three-branch constitutional governance, EffectVector | K6 |
| `agency.rs` | Agent-first architecture, roles, manifests | K6 |
| `chain.rs` | ExoChain hash-linked event log (exochain feature) | Exo |
| `tree_manager.rs` | Resource tree + mutation log + chain facade (exochain) | Exo |
| `gate.rs` | GateBackend trait, CapabilityGate, GovernanceGate (exochain) | Exo |
| `lib.rs` | Crate root, re-exports | - |

## Feature Flags

| Feature | What It Enables | Key Dependencies |
|---------|----------------|-----------------|
| `native` (default) | Tokio runtime, native I/O | tokio, dirs |
| `exochain` | Chain, TreeManager, Gate, RVF persistence | rvf-crypto, rvf-types, rvf-wire, rvf-runtime, exo-resource-tree, ed25519-dalek, ciborium |
| `tilezero` | TileZeroGate (three-way gate with receipts) | cognitum-gate-tilezero (implies exochain) |
| `cluster` | ClusterService with raft consensus | ruvector-cluster, ruvector-raft, ruvector-replication |
| `wasm-sandbox` | Wasmtime-based tool runner | wasmtime |
| `containers` | Container lifecycle management | (system docker/podman) |

## Boot Sequence

```
weaver kernel start
  1. Fork background daemon process
  2. Read KernelConfig from config file
  3. Kernel::boot() state machine: Booting → Running
     a. ProcessTable::new(max_processes)
     b. ServiceRegistry::new()
     c. KernelIpc::new(message_bus)
     d. HealthSystem::new(interval)
     e. A2ARouter::new()
     f. CronService::new() → register as system service
     g. ClusterMembership::new()
     h. [exochain] ChainManager::new() → load_from_rvf() if checkpoint exists
     i. [exochain] TreeManager::new(chain) → bootstrap standard namespaces
     j. [exochain] GovernanceGate::new(threshold, human_approval) → attach chain
     k. K2.1: SpawnBackend dispatch attached to supervisor; ServiceEntry metadata added to ServiceRegistry
  4. Bind Unix socket for JSON-RPC
  5. Start cron tick loop
  6. Listen for RPC requests
```

## Shutdown Sequence

```
weaver kernel stop
  1. Send "kernel.stop" RPC
  2. Daemon sets KernelState::ShuttingDown
  3. ServiceRegistry::stop_all()
  4. [exochain] ChainManager::save_to_rvf() → persist checkpoint
  5. Remove PID file + socket
  6. KernelState::Halted
```

## Concurrency Model

- **DashMap**: Lock-free concurrent maps for ProcessTable, ServiceRegistry, A2ARouter inboxes
- **tokio::sync::Mutex**: Protecting ChainManager, TreeManager internals
- **Arc**: Shared ownership across async tasks
- **CancellationToken**: Cooperative shutdown for agent loops and services
- **tokio::sync::mpsc**: Channels for IPC message delivery

## Directory Layout

```
crates/clawft-kernel/
  src/
    lib.rs              Crate root + re-exports
    boot.rs             Kernel<P> generic over Platform
    process.rs          ProcessTable (DashMap<Pid, ProcessEntry>)
    supervisor.rs       AgentSupervisor (spawn/stop/restart)
    ipc.rs              KernelIpc (MessageBus wrapper)
    a2a.rs              A2ARouter (per-agent inboxes)
    topic.rs            TopicRouter (pub/sub)
    capability.rs       AgentCapabilities + checker
    health.rs           HealthSystem + aggregation
    console.rs          BootEvent log
    service.rs          ServiceRegistry
    config.rs           KernelConfig extension
    error.rs            Error types
    cron.rs             CronService
    agent_loop.rs       kernel_agent_loop()
    chain.rs            ChainManager (exochain)
    tree_manager.rs     TreeManager (exochain)
    gate.rs             GateBackend + impls (exochain)
    governance.rs       GovernanceEngine + EffectVector
    environment.rs      EnvironmentManager
    cluster.rs          ClusterMembership
    container.rs        ContainerManager
    app.rs              AppManager
    agency.rs           Agency model
    wasm_runner.rs      WasmToolRunner
  Cargo.toml
```

## K2 Symposium Decisions

The K2 Symposium (2026-03-04) produced 22 design decisions and 10 approved changes
that shape K3+ development. Key architectural changes:

- **Layered Protocol Architecture** (D4): kernel IPC → ServiceApi → protocol adapters (MCP, gRPC, Shell, HTTP)
- **Defense-in-Depth Governance** (D7): dual gate checks at routing-time (A2ARouter) and handler-time (GovernanceGate)
- **Service Identity Model** (D1): services are separate from processes, managed by ServiceEntry
- **SpawnBackend** (D2/D3): Native/Wasm/Container/Tee/Remote execution backends
- **Breaking IPC Changes** (D19): MessageTarget restructured for service routing
- **K-Phase Reassignment** (D6/D21): clustering moves to K5, K6 = deep networking

Full report: `k2-symposium/08-symposium-results-report.md`

## Related Documentation

- [Kernel Modules Reference](./kernel-modules.md) -- per-module deep dive
- [Integration Patterns](./integration-patterns.md) -- RVF, chain, tree, governance
- [K-Phase Status](./k-phases.md) -- what's implemented vs pending
- [ADR-049: WeftOS Kernel](../adr/adr-049-weftos-kernel.md) -- design rationale (formerly `architecture/adr-028-weftos-kernel.md`; renumbered + relocated 2026-04-28 / WEFT-140 to deduplicate the ADR-028 number)
- [K2 Symposium Results](./k2-symposium/08-symposium-results-report.md) -- design decisions for K3+
