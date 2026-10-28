# WeftOS K-Phase Status

> **Status (2026-10-27).** The phase table below was last corrected on 2026-04-28 (0.7.0). It does not record K6 beyond phase 1 or any 0.8.x work. For what shipped since, see CHANGELOG.md and the [architecture overview](../architecture/overview.md#the-weftos-layer-v084). The K, Sprint, Phase and version schemes overlap; do not read K-numbers as release numbers.

Implementation status for each kernel phase. As of 0.7.0
(2026-04-28), K0 through K5 are complete and K6 has shipped Phase 1
(types, traits, TCP/WS transport, 136 tests). The earlier "K3+
stubbed" wording was accurate at the K2 milestone; this doc was
allowed to drift through K3/K4/K5 and was corrected in WEFT-138 against
the `02-kernel-governance.md` audit and the `k0-k5-final-gap-analysis`
notes.

---

## Completed Phases

### K0: Kernel Foundation

**Status**: COMPLETE (45+ tests)

| Component | File | Tests | Notes |
|-----------|------|-------|-------|
| Kernel boot state machine | `boot.rs` | 14 | Booting → Running → Halted |
| Boot event logging | `console.rs` | 12 | BootEvent, BootPhase, KernelEventLog |
| Configuration extension | `config.rs` | 8 | KernelConfigExt trait |
| Error types | `error.rs` | 11 | KernelError enum + Display |

### K1: Process & Supervision

**Status**: COMPLETE (80+ tests)

| Component | File | Tests | Notes |
|-----------|------|-------|-------|
| Process table | `process.rs` | 22 | PID allocation, state machine |
| Agent supervisor | `supervisor.rs` | 35 | Spawn/stop/restart, resource limits |
| Capability model | `capability.rs` | 24 | RBAC, IpcScope, SandboxPolicy |

### K2: IPC & Communication

**Status**: COMPLETE (130+ tests)

| Component | File | Tests | Notes |
|-----------|------|-------|-------|
| Kernel IPC | `ipc.rs` | 18 | Message envelopes, targets, payloads |
| Agent-to-agent routing | `a2a.rs` | 28 | Per-agent inboxes, capability checks |
| Topic pub/sub | `topic.rs` | 14 | TopicRouter, subscriptions |
| Agent work loop | `agent_loop.rs` | 22 | Command processing, gate integration |
| Cron scheduler | `cron.rs` | 12 | Job registration, tick handling |
| Health system | `health.rs` | 10 | Aggregated checks |
| Service registry | `service.rs` | 16 | Named lifecycle management |

### K2b: Hardening

**Status**: COMPLETE (30+ tests)

| Component | Area | Tests | Notes |
|-----------|------|-------|-------|
| Chain-logged lifecycle | agent_loop.rs | 8 | ipc.recv, ipc.ack, agent.spawn |
| Signal-based stop/restart | daemon.rs | 4 | SIGTERM/SIGHUP handlers |
| CLI display improvements | commands/ | 6 | agent inspect, chain detail |
| DashMap deadlock fix | a2a.rs | 3 | Concurrent access safety |
| GovernanceGate | gate.rs | 7 | Governance → GateBackend adapter |
| Gate wiring in daemon | daemon.rs | 2 | GovernanceGate replaces None |

### K2.1: Symposium Implementation

**Status**: COMPLETE

**Scope**: Implement breaking changes and quick wins from the K2 Symposium
before K3 begins. See `docs/weftos/k2-symposium/08-symposium-results-report.md`
for full decision rationale.

Shipped:
- `SpawnBackend` enum on `SpawnRequest` (D3/C1) with `Native` implemented
  and `Wasm`/`Container`/`Tee`/`Remote` returning `BackendNotAvailable`.
- Post-quantum dual-signing scaffolding (D11/C6); see
  `adr-028-post-quantum-dual-signing.md` for the policy that K2.1
  enables.
- `MessageTarget::Service(name)` and `ServiceMethod` routing through
  `A2ARouter` (D1/D19).
- `ServiceEntry` as first-class registry concept, decoupled from PID.
- `AuditLevel` plumbing for chain-event severity.

Decisions addressed in this phase:

| Decision | Change | Description |
|----------|--------|-------------|
| D3 | C1 | `SpawnBackend` enum added to `SpawnRequest` |
| D11 | C6 | Post-quantum dual signing (Ed25519 + ML-DSA-65) |
| D14 | C8 | `SpawnBackend::Tee` variant (returns `BackendNotAvailable`) |
| D1 | -- | `ServiceEntry` as first-class registry concept |
| D19 | -- | Breaking IPC changes: `MessageTarget::Service(name)` routing |

Key deliverables:
- **SpawnBackend enum**: `Native`, `Wasm`, `Container`, `Tee`, `Remote` variants
  with only `Native` implemented; others return `BackendNotAvailable`
- **ServiceEntry struct**: Decoupled from PID -- references an owning agent,
  external endpoint, or container ID in the ServiceRegistry
- **MessageTarget expansion**: Add `Service(name)` variant for service routing
  through A2ARouter (D1, D19)
- **Post-quantum investigation**: Enable `DualKey` signing path in rvf-crypto
  for chain entries (Ed25519 + ML-DSA-65 dual signatures)

### ExoChain Subsystem

**Status**: COMPLETE (60+ tests)

| Component | File | Tests | Notes |
|-----------|------|-------|-------|
| Hash chain manager | `chain.rs` | 28 | SHAKE-256, Ed25519, witness chains |
| Resource tree facade | `tree_manager.rs` | 18 | Atomic tree+chain+mutation ops |
| Gate backends | `gate.rs` | 14 | CapabilityGate, GovernanceGate |

---

## K3+ Phases (Live Backends Wired)

### K3: WASM Sandbox

**Status**: COMPLETE (Wasmtime-backed runner, fuel metering, gates, chain
logging, contracts; tests pass with `wasm-sandbox` feature).

**File**: `wasm_runner.rs` (~530 lines)

What exists:
- `WasmToolRunner` struct with fuel metering, memory limits, timeout config
- `WasmTool` tool definition with module bytes
- `WasmSandboxConfig` with configurable limits
- `WasmValidation` module validation checks
- Full test suite for type API
- Wasmtime runtime integration (behind `wasm-sandbox` feature),
  upgraded from wasmtime 29 → 33; WASI no-preopens.
- Tool registry for WASM modules.
- Fuel accounting connected to resource limits.
- Chain logging for tool execution events.
- Gate check before WASM tool execution (dual-layer A2ARouter gate, C4).
- Tree registration under `/kernel/tools/`.
- Chain-anchored service contracts (C3) via `service.contract.register`.
- Shell→WASM pipeline (C5).

**Symposium additions** (K2 Symposium decisions):
- **C2**: `ServiceApi` trait -- internal API surface that protocol adapters
  (MCP, gRPC, Shell, HTTP) bind to. K3 implements local dispatch.
- **C4**: Dual-layer gate in A2ARouter -- routing-time governance check
  before inbox delivery, complementing handler-time GovernanceGate.
- **C3**: Chain-anchored service contracts -- immutable API schemas stored
  on the ExoChain as `service.contract.register` events.
- **C5**: WASM-compiled shell pipeline -- shell scripts compiled to WASM
  modules with chain-anchored provenance (container sandbox deferred to K4).
- **C9**: N-dimensional `EffectVector` -- refactor from fixed 5D to
  configurable named dimensions per environment.

**Key crates**: `ruvector-tiny-dancer-core` (semantic routing hints),
`cognitum-gate-kernel` (audit trail verification),
`ruvector-snapshot` (WASM state snapshots)

### K3c: ECC Cognitive Substrate

**Status**: COMPLETE (83 tests)

Adds the Ephemeral Causal Cognition (ECC) cognitive substrate behind the `ecc` feature flag.

| Component | File | Tests | Notes |
|-----------|------|-------|-------|
| Causal DAG | `causal.rs` | 22 | Typed/weighted edges, BFS traversal, path finding |
| Cognitive tick | `cognitive_tick.rs` | 20 | Adaptive interval, drift detection, SystemService |
| Cross-references | `crossref.rs` | 12 | UniversalNodeId (BLAKE3), bidirectional store |
| Calibration | `calibration.rs` | 10 | Boot-time benchmarking, p50/p95, auto tick interval |
| HNSW service | `hnsw_service.rs` | 11 | Thread-safe wrapper for clawft-core HnswStore |
| Impulse queue | `impulse.rs` | 8 | HLC-sorted ephemeral causal events |

Additional changes:
- `NodeEccCapability` in `cluster.rs` for cluster capability advertisement
- 7 `ecc.*` tools in `builtin_tool_catalog()` (`wasm_runner.rs`)
- `BootPhase::Ecc` + `ToolCategory::Ecc` in console/wasm_runner
- 6 resource tree namespaces under `/kernel/services/ecc/`
- `weaver ecc` CLI subcommands (status, calibrate, search, causal, crossrefs, tick)

**Source**: ECC Symposium (2026-03-22) — see `docs/weftos/ecc-symposium/`

### K4: Containers

**Status**: COMPLETE (config validation, lifecycle management, health
propagation, ChainAnchor scaffolding). Live Docker/Podman integration is
gated behind a real container daemon and is exercised manually rather
than in CI.

**File**: `container.rs` (~600 lines)

What exists:
- `ContainerManager` struct with lifecycle methods
- `ContainerConfig` (image, ports, volumes, env, restart policy)
- `ManagedContainer` state tracking
- `ContainerState` state machine
- Port mapping, volume mount, restart policy types
- `ChainAnchor` trait + `MockAnchor` (`chain.rs:2180-2212`).

Known limitations / deferred:
- Live Docker/Podman API client (bollard / shell exec) — interface in
  place, requires running container daemon for end-to-end smoke.
- Container health-check integration with `HealthSystem` is wired for
  the trait surface; live health propagation is exercised via manual
  smoke runs.
- Chain logging for container lifecycle events ships under K4; tree
  registration under `/kernel/services/{container_name}` and gate
  checks before container operations are wired through the standard
  K2/ExoChain plumbing.

**Symposium additions** (K2 Symposium decisions):
- **C7**: `ChainAnchor` trait for blockchain anchoring -- chain-agnostic
  with `anchor()`, `verify()`, and `status()` methods. First implementation
  is a local mock or OpenTimestamps to validate the interface shape.
- **C8**: `SpawnBackend::Tee { enclave: EnclaveConfig }` variant defined in
  K2.1 but returns `BackendNotAvailable` until TEE hardware is available (D14).
- **D18**: SONA reuptake spike -- pull forward from K5 into late K4 to
  validate accumulated training data and confirm K5 integration path.
- **D13**: SNARK prover research spike -- evaluate arkworks or halo2 for
  ZK proof integration into GovernanceGate and service invocations.

### K5: App Framework + Clustering

**Status**: COMPLETE (manifest parsing, install/start/stop/remove/list/inspect
lifecycle, namespaced agents/tools, partial-start rollback, lifecycle hooks).

**File**: `app.rs` (~980 lines)

> **Note**: Clustering was moved from K6 to K5 per D6/D21 -- distributed apps
> need clustering before the full network transport layer. K5 now combines the
> original application framework with multi-node distribution.

What exists:
- `AppManager` with install/start/stop/uninstall methods.
- `AppManifest` parsing from `weftapp.toml` (real file I/O).
- `InstalledApp` tracking with state machine.
- Agent, service, and tool spec types.
- Agent spawning from app manifests via `Supervisor`.
- Service registration from app manifests.
- Tool loading (native, WASM, API) from tool specs.
- Chain logging for app lifecycle events.
- Tree registration under `/apps/{name}`.

Known limitations / deferred:
- **Crypto-signed app bundles**: RVF-signed bundles of config +
  executables that live on-chain. Apps (weft, openclaw, claudecode,
  etc.) are intended to be verified against their chain-anchored
  signatures before installation; the verification path is in place
  for the trait, but production-grade bundle signing is post-0.7.
- **Clustering**: Multi-node service discovery and cross-node routing
  for distributed app deployment. The `cluster` feature ships with
  ServiceRegistry / A2ARouter remote-operation hooks; full
  partition-tolerance and split-brain handling are post-0.7 (tracked
  in the K6 audit).
- **SONA integration**: Self-optimizing agent framework against
  K3/K4 training data — the reuptake spike landed in late K4 (D18); the
  full K5 wire-through is post-0.7.
- **`docs/guides/kernel.md`**: operator-facing kernel guide is the K5
  documentation deliverable; landed in 0.7.0 (see WEFT-139).

### K6: Deep Networking + Replication

**Status**: PHASE 1 COMPLETE (types, traits, TCP/WS transport, 136 tests)

> **Note**: SPARC spec is required before K6 implementation begins (D22, C10).
> See `docs/weftos/sparc/k6-cluster-networking.md` (to be written). K6 is now
> purely deep networking and replication -- clustering moved to K5 per D6/D21.

**Files**: `cluster.rs` (~710 lines), `environment.rs` (~550 lines),
`governance.rs` (~400 lines), `agency.rs` (~530 lines)

What exists:
- `ClusterMembership` with peer tracking and node state
- `ClusterService` (behind `cluster` feature) with raft consensus types
- `EnvironmentManager` with governance-scoped environments
- `GovernanceEngine` with three-branch model (fully functional)
- `Agency` with roles, manifests, and interfaces

What's needed for K6 completion:
- Network transport layer (TCP/QUIC) for peer communication
- Raft consensus wired to ruvector-raft
- Cross-node chain replication
- Environment-scoped governance enforcement in daemon
- Agent manifest discovery and pairing protocol
- **TEE implementation**: `SpawnBackend::Tee` runtime when hardware is
  available for testing (D14). Trait surface defined in K2.1/C8.

---

## Implementation Priority for K3+

Ordering per D21 (K2 Symposium): K3 -> K4 -> K5 -> K6, with iteration.
K3-K6 are a development cycle, not a strict waterfall. Later phases may
drive changes that loop back to earlier phases. The symposium's breaking
changes (D19) minimize K0-K2 rework, but K3-K6 iteration is expected
and healthy.

| Priority | Phase | Effort | Key Dependency |
|----------|-------|--------|----------------|
| 1 | K3 WASM sandbox + ServiceApi | ~25h | wasmtime crate, C2/C3/C4/C5/C9 |
| 2 | K4 Containers + ChainAnchor | ~20h | Docker/Podman on host, C7 |
| 3 | K5 App framework + Clustering | ~35h | K3/K4 (WASM tools, containers) |
| 4 | K6 Deep Networking + Replication | ~40h | SPARC spec (C10), transport design |

### Integration Checklist for New K-Phase Work

- [x] Chain logging for all state changes
- [x] Tree registration in standard namespace
- [x] Gate check before privileged operations
- [x] Tests with chain verification
- [x] CLI commands in `clawft-weave`
- [x] Feature flag if external dependency
- [x] Documentation in `docs/weftos/`

---

## Running Tests

```bash
# All kernel tests (default features)
scripts/build.sh test

# Kernel only
cargo test -p clawft-kernel

# Kernel + exochain
cargo test -p clawft-kernel --features exochain

# Kernel + all features
cargo test -p clawft-kernel --features "exochain,cluster,wasm-sandbox,containers"

# Full phase gate (11 checks)
scripts/build.sh gate
```
