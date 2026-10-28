# WeftOS Kernel Modules Reference

> **Status (2026-10-27).** Historical snapshot from 2026-03-25. It documents 25 source files; `crates/clawft-kernel/src/` now has 224 entries (method: `ls crates/clawft-kernel/src | wc -l`, files and directories), including `cog_ingest`, `exposure`, `licence`, `infer_proxy` and the governance overlay, none of which it describes. Treat it as the K0 to K5 module description only. Proposed: keep as a historical reference, or replace with a generated module list; no deletion without a ruling.

Per-module documentation for the 25 source files in `crates/clawft-kernel/src/`.
Each section covers: purpose, public API, integration points, and test coverage.

---

## K0: Foundation

### boot.rs (~600 lines)

**Purpose**: Kernel lifecycle state machine.

**Key Types**:
- `Kernel<P: Platform>` -- generic kernel struct parameterized by platform
- `KernelState` -- enum: `Booting`, `Running`, `ShuttingDown`, `Halted`

**Public API**:
```rust
Kernel::boot(config, kernel_config, platform) -> KernelResult<Self>
Kernel::shutdown(&self) -> KernelResult<()>
Kernel::state(&self) -> KernelState
Kernel::process_table(&self) -> &ProcessTable
Kernel::service_registry(&self) -> &ServiceRegistry
Kernel::ipc(&self) -> &KernelIpc
Kernel::health(&self) -> &HealthSystem
Kernel::a2a(&self) -> &A2ARouter
Kernel::cron(&self) -> &CronService
```

**Integration**: Initializes ChainManager + TreeManager when `exochain` feature enabled.
Loads RVF checkpoint on boot, saves on shutdown. Logs boot events to console.

### console.rs (~450 lines)

**Purpose**: Structured boot event logging.

**Key Types**:
- `BootEvent` -- timestamped event with phase + message
- `BootPhase` -- enum: `Init`, `Services`, `Network`, `Ready`
- `BootLog` -- append-only event buffer
- `KernelEventLog` -- aggregated event log
- `LogLevel` -- `Info`, `Warn`, `Error`, `Debug`

**Integration**: Boot events flow to `KernelEventLog`. Accessible via `weaver kernel logs`.

### config.rs (~60 lines)

**Purpose**: Extension traits for `KernelConfig` (defined in `clawft-types`).

**Key Types**:
- `KernelConfigExt` -- trait adding convenience methods

### error.rs (~90 lines)

**Purpose**: Kernel error types.

**Key Types**:
- `KernelError` -- enum covering boot, process, service, IPC, chain errors
- `KernelResult<T>` -- `Result<T, KernelError>`

---

## K1: Process & Supervision

### process.rs (~470 lines)

**Purpose**: PID-based agent tracking with state machine transitions.

**Key Types**:
- `Pid` -- newtype `u32` process identifier
- `ProcessTable` -- `DashMap<Pid, ProcessEntry>` with atomic PID counter
- `ProcessEntry` -- agent metadata: name, state, capabilities, timestamps
- `ProcessState` -- enum: `Starting`, `Running`, `Suspended`, `Stopping`, `Stopped`
- `ResourceUsage` -- messages_sent, tool_calls, cpu_time_ms

**Public API**:
```rust
ProcessTable::new(max: u32) -> Self
ProcessTable::spawn(name, capabilities) -> KernelResult<Pid>
ProcessTable::get(pid) -> Option<ProcessEntry>
ProcessTable::set_state(pid, state) -> KernelResult<()>
ProcessTable::remove(pid) -> KernelResult<ProcessEntry>
ProcessTable::list() -> Vec<(Pid, ProcessEntry)>
ProcessTable::count() -> usize
```

**Integration**: Supervisor wraps ProcessTable for lifecycle management. Chain logs
state transitions. Tree registers entries under `/kernel/agents/`.

### supervisor.rs (~800 lines)

**Purpose**: Agent lifecycle management.

**Key Types**:
- `AgentSupervisor` -- orchestrates spawn/stop/restart
- `SpawnRequest` -- agent name, type, capabilities, resources
- `SpawnResult` -- pid, inbox channel, status

**Public API**:
```rust
AgentSupervisor::spawn(request) -> KernelResult<SpawnResult>
AgentSupervisor::stop(pid) -> KernelResult<()>
AgentSupervisor::restart(pid) -> KernelResult<SpawnResult>
AgentSupervisor::inspect(pid) -> Option<ProcessEntry>
```

**K2.1 Additions (Symposium C1 + C8)**:
- `SpawnBackend` -- enum: `Native`, `Wasm`, `Container`, `Tee`, `Remote`
- `EnclaveConfig` -- TEE enclave configuration (placeholder until hardware available)
- `SpawnRequest.backend` -- `Option<SpawnBackend>` field (None = Native)

**Integration**: `[exochain]` logs `agent.spawn`, `agent.suspend`, `agent.resume` events.
Registers agents in tree under `/kernel/agents/{name}`. Enforces governance rules on spawn.

### capability.rs (~980 lines)

**Purpose**: RBAC permission model for agent processes.

**Key Types**:
- `AgentCapabilities` -- tools, ipc_scope, sandbox_policy, resource_limits
- `CapabilityChecker` -- validates actions against capabilities
- `ToolPermissions` -- allow/deny tool lists
- `IpcScope` -- `All`, `SameAgent`, `Named(Vec<String>)`
- `SandboxPolicy` -- `None`, `Strict`, `Permissive`
- `ResourceLimits` -- max_memory, max_cpu, max_messages
- `ResourceType` -- enum for limit categories

---

## K2: IPC & Communication

### ipc.rs (~470 lines)

**Purpose**: Typed message envelopes over the existing MessageBus.

**Key Types**:
- `KernelIpc` -- wraps `MessageBus` with envelope semantics
- `KernelMessage` -- from, to, payload, timestamp, id
- `MessageTarget` -- `Agent(Pid)`, `Service(String)`, `Broadcast`
- `MessagePayload` -- `Text(String)`, `Json(Value)`, `Binary(Vec<u8>)`
- `KernelSignal` -- `Shutdown`, `Suspend`, `Resume`, `Custom(String)`

**K2.1 Additions (Symposium D19)**:
- `MessageTarget::Service(String)` -- route to named service via ServiceRegistry
- `MessageTarget::ServiceMethod { service, method }` -- route to specific service method

**Integration**: Chain logs `ipc.send`, `ipc.recv`, `ipc.ack` events with message metadata.
Gate checks IPC scope before delivery.

### a2a.rs (~870 lines)

**Purpose**: Agent-to-agent message routing with capability-based access control.

**Key Types**:
- `A2ARouter` -- per-agent inbox registry with DashMap
- Methods: `register`, `unregister`, `send`, `send_checked`, `broadcast`

**Integration**: `send_checked` validates sender capabilities + IPC scope before delivery.
Chain logging optional via `send_checked` when chain is available.

### topic.rs (~360 lines)

**Purpose**: Pub/sub topic routing.

**Key Types**:
- `TopicRouter` -- topic → subscriber registry
- `Subscription` -- subscriber_id, filter, callback

**Public API**:
```rust
TopicRouter::subscribe(topic, subscriber_id) -> Subscription
TopicRouter::unsubscribe(subscription_id)
TopicRouter::publish(topic, message) -> usize  // returns delivery count
TopicRouter::list_topics() -> Vec<String>
```

### agent_loop.rs (~1100 lines)

**Purpose**: Built-in kernel work loop that processes messages for a kernel-managed agent.

**Signature**:
```rust
pub async fn kernel_agent_loop(
    pid: Pid,
    cancel: CancellationToken,
    inbox: mpsc::Receiver<KernelMessage>,
    a2a: Arc<A2ARouter>,
    cron: Arc<CronService>,
    process_table: Arc<ProcessTable>,
    #[cfg(feature = "exochain")] chain: Option<Arc<ChainManager>>,
    #[cfg(feature = "exochain")] gate: Option<Arc<dyn GateBackend>>,
)
```

**Built-in Commands**: `suspend`, `resume`, `exec`, `cron.add`, `cron.remove`

**Gate Check Flow**:
1. Extract command from message
2. Map to action string (e.g. `"tool.exec"`, `"service.cron.add"`)
3. Call `gate.check(agent_id, action, context)`
4. Permit → proceed; Deny → error reply; Defer → queue for review

**Chain Logging**: `ipc.recv`, `ipc.ack`, `agent.spawn`, `agent.suspend`, `agent.resume`

### cron.rs (~250 lines)

**Purpose**: Scheduled job execution.

**Key Types**:
- `CronService` -- job registry + tick handler

**Integration**: Implements `SystemService` trait. Jobs fire on tick, logged to chain
as `cron.fire` events. Gate checks `service.cron.add`/`service.cron.remove`.

### health.rs (~240 lines)

**Purpose**: Aggregated health checks.

**Key Types**:
- `HealthSystem` -- periodic health check runner
- `HealthStatus` -- `Healthy`, `Degraded`, `Unhealthy`
- `OverallHealth` -- aggregated status across all services

### service.rs (~330 lines)

**Purpose**: Named service lifecycle management.

**Key Types**:
- `ServiceRegistry` -- DashMap of named services
- `SystemService` -- trait: `start()`, `stop()`, `health_check()`
- `ServiceType` -- `Core`, `Plugin`, `App`

**Public API**:
```rust
ServiceRegistry::register(name, service) -> KernelResult<()>
ServiceRegistry::unregister(name) -> KernelResult<()>
ServiceRegistry::get(name) -> Option<Arc<dyn SystemService>>
ServiceRegistry::start_all() -> Result<()>
ServiceRegistry::stop_all() -> Result<()>
ServiceRegistry::snapshot() -> Vec<(String, Arc<dyn SystemService>)>
```

**K2.1 Additions (Symposium D1 + D9)**:
- `ServiceEntry` -- service metadata: name, owner_pid, endpoint, audit_level
- `ServiceEndpoint` -- enum: `AgentInbox(Pid)`, `External { url }`, `Container { id }`
- `AuditLevel` -- enum: `Full` (witness every call, D9), `GateOnly` (governance only)

---

## K3-K6: Extended Subsystems

### wasm_runner.rs (~530 lines) [K3]

**Purpose**: WASM tool execution with sandboxing.

**Key Types**:
- `WasmToolRunner` -- executes WASM modules with fuel metering + memory limits
- `WasmTool` -- tool definition with WASM module bytes
- `WasmToolResult` -- execution result
- `WasmSandboxConfig` -- fuel_limit, memory_limit, timeout
- `WasmValidation` -- module validation checks

### container.rs (~600 lines) [K4]

**Purpose**: Sidecar container lifecycle.

**Key Types**:
- `ContainerManager` -- start/stop/health for Docker/Podman containers
- `ContainerConfig` -- image, ports, volumes, env, restart policy
- `ManagedContainer` -- running container state
- `ContainerState` -- `Created`, `Running`, `Stopped`, `Failed`
- `PortMapping`, `VolumeMount`, `RestartPolicy`

### app.rs (~980 lines) [K5]

**Purpose**: Application manifest parsing and lifecycle.

**Key Types**:
- `AppManager` -- install/start/stop/uninstall applications
- `AppManifest` -- parsed `weftapp.toml` with metadata, agents, services, tools
- `InstalledApp` -- installed application state
- `AppState` -- `Installed`, `Starting`, `Running`, `Stopping`, `Stopped`, `Failed`
- `AgentSpec`, `ServiceSpec`, `ToolSpec` -- manifest component definitions
- `AppCapabilities`, `AppHooks`, `ToolSource`

### cluster.rs (~710 lines) [K6]

**Purpose**: Multi-node cluster membership.

**Key Types**:
- `ClusterMembership` -- peer tracking, node state
- `PeerNode` -- remote node info: id, address, state, platform
- `NodeState` -- `Joining`, `Active`, `Leaving`, `Failed`
- `NodeId` -- unique node identifier
- `NodePlatform` -- `Native`, `Browser`, `Wasm`
- `ClusterConfig` -- cluster settings
- `ClusterService` -- (behind `cluster` feature) full raft consensus

### environment.rs (~550 lines) [K6]

**Purpose**: Governance-scoped environments.

**Key Types**:
- `EnvironmentManager` -- manage dev/staging/prod environments
- `Environment` -- environment definition with governance scope
- `EnvironmentClass` -- `Development`, `Staging`, `Production`
- `GovernanceScope` -- governance rules per environment
- `GovernanceBranches` -- branch-specific configs
- `AuditLevel`, `LearningMode`

### governance.rs (~400 lines) [K6]

**Purpose**: Three-branch constitutional governance with 5D effect algebra.

**Key Types**:
- `GovernanceEngine` -- rule evaluation engine
- `EffectVector` -- 5D scoring: risk, fairness, privacy, novelty, security
- `GovernanceDecision` -- `Permit`, `PermitWithWarning`, `EscalateToHuman`, `Deny`
- `GovernanceRequest` -- agent_id, action, effect, context
- `GovernanceResult` -- decision + evaluated rules
- `GovernanceRule` -- id, description, branch, severity, active
- `GovernanceBranch` -- `Legislative`, `Executive`, `Judicial`
- `RuleSeverity` -- `Informational`, `Warning`, `Blocking`

**Three-Branch Model**:
1. **Legislative** -- SOPs, rules, manifests (define boundaries)
2. **Executive** -- Agents (act within boundaries)
3. **Judicial** -- Governance engine via gate (validate every action)

**EffectVector Scoring**:
```
magnitude = sqrt(risk^2 + fairness^2 + privacy^2 + novelty^2 + security^2)
```
If `magnitude > threshold` and a `Blocking` rule matches → `Deny`.

**K3 Refactor (Symposium C9)**: `EffectVector` will become N-dimensional with named
dimensions. Current 5D (risk, fairness, privacy, novelty, security) remains the default.
Edge devices can use compact 3D. Environments configure their dimension schema.

### agency.rs (~530 lines) [K6]

**Purpose**: Agent-first architecture model.

**Key Types**:
- `Agency` -- agent registry with roles and manifests
- `AgentManifest` -- declarative agent definition
- `AgentRole` -- `Worker`, `Coordinator`, `Supervisor`, `System`
- `AgentPriority`, `AgentResources`, `AgentRestartPolicy`
- `AgentHealth`, `AgentInterface`, `InterfaceProtocol`, `ResponseMode`

---

## ExoChain Subsystem (feature-gated)

### chain.rs (~1100 lines)

**Purpose**: Append-only hash-linked event log with cryptographic integrity.

**Key Types**:
- `ChainManager` -- event log manager
- `ChainEvent` -- sequence, source, kind, payload, timestamp, hash, prev_hash
- `ChainStatus` -- length, last_hash, genesis timestamp
- `ChainVerifyResult` -- valid, event_count, errors
- `ChainCheckpoint` -- serializable checkpoint for persistence

**Hashing**: SHAKE-256 (256-bit output) via `rvf_crypto::hash::shake256_256`

**Public API**:
```rust
ChainManager::new(chain_id, max_events) -> Self
ChainManager::append(source, kind, payload) -> ChainEvent
ChainManager::verify_integrity() -> ChainVerifyResult
ChainManager::tail(n) -> Vec<ChainEvent>
ChainManager::len() -> usize
ChainManager::status() -> ChainStatus
ChainManager::save_to_rvf(path) -> Result<()>
ChainManager::load_from_rvf(path) -> Result<Self>
ChainManager::record_lineage(parent, child, derivation) -> LineageRecord
ChainManager::verify_lineage(records) -> bool
ChainManager::verify_witness() -> Result<bool>
```

**RVF Integration**:
- `save_to_rvf()`: Writes chain events as RVF segments (ExoChainHeader + CBOR payload),
  appends Ed25519 signature footer, creates witness chain entries
- `load_from_rvf()`: Reads segments, validates checksums, restores witness chain
- `record_lineage()`: DNA-style provenance for agent spawn and resource derivation
- `verify_witness()`: Creates and verifies witness chain from entries

### tree_manager.rs (~400 lines)

**Purpose**: Unified facade over ResourceTree + MutationLog + ChainManager.

**Key Types**:
- `TreeManager` -- wraps tree, mutation log, and chain
- `TreeStats` -- node count, depth, mutation count

**Public API**:
```rust
TreeManager::new(chain) -> Self
TreeManager::bootstrap() -> Result<()>  // creates /kernel, /apps, /network, /environments
TreeManager::insert(path, metadata) -> Result<()>
TreeManager::remove(path) -> Result<()>
TreeManager::update_meta(path, key, value) -> Result<()>
TreeManager::register_service(name) -> Result<()>
TreeManager::stats() -> TreeStats
```

**Atomic Operations**: Every tree mutation creates:
1. `MutationEvent` in the mutation log
2. `ChainEvent` on the chain (`tree.bootstrap`, `tree.insert`, `tree.remove`)
3. Stores `chain_seq` metadata on the tree node for two-way traceability

**Standard Namespaces** (created on bootstrap):
- `/kernel` -- kernel metadata
- `/kernel/services` -- registered services
- `/kernel/agents` -- spawned agents
- `/apps` -- installed applications
- `/network/peers` -- cluster peers
- `/environments` -- governance-scoped environments

### gate.rs (~850 lines)

**Purpose**: Access control abstraction for the agent loop.

**Key Types**:
- `GateBackend` -- trait: `check(agent_id, action, context) -> GateDecision`
- `GateDecision` -- `Permit { token }`, `Defer { reason }`, `Deny { reason, receipt }`
- `CapabilityGate` -- binary permit/deny from capability checker
- `GovernanceGate` -- bridges GovernanceEngine into GateBackend
- `TileZeroGate` -- (tilezero feature) three-way with cryptographic receipts

**GovernanceGate Flow**:
```
agent sends command
  → gate.check(agent_id, action, context)
    → extract EffectVector from context JSON
    → GovernanceEngine.evaluate(request)
    → map GovernanceDecision → GateDecision
    → log to chain (governance.permit/warn/defer/deny)
  → agent_loop proceeds or blocks
```

**Chain Events from GovernanceGate**:

| Event Kind | Fields |
|------------|--------|
| `governance.permit` | agent_id, action, effect, rules |
| `governance.warn` | agent_id, action, effect, rules, warning |
| `governance.defer` | agent_id, action, effect, rules, reason |
| `governance.deny` | agent_id, action, effect, rules, reason |

**K3 Extension (Symposium D7)**: A2ARouter will also receive a `GateBackend` for
routing-time gate checks. This creates two enforcement points: routing-time in
A2ARouter (prevents unauthorized messages) and handler-time in agent_loop
(prevents unauthorized command execution).

---

## K3c: ECC Cognitive Substrate (feature: `ecc`)

### causal.rs (~700 lines, 22 tests)

**Purpose**: CausalGraph DAG with typed, weighted directed edges.

**Key Types**: `CausalGraph`, `CausalEdge`, `CausalEdgeType`, `CausalNode`

**Public API**: `add_node`, `get_node`, `remove_node`, `link`, `unlink`, `get_forward_edges`, `get_reverse_edges`, `traverse_forward`, `traverse_reverse`, `find_path`

### cognitive_tick.rs (~550 lines, 20 tests)

**Purpose**: Configurable cognitive loop with adaptive interval and drift detection.

**Key Types**: `CognitiveTick`, `CognitiveTickConfig`, `CognitiveTickStats`

**Public API**: `new`, `with_interval`, `stats`, `record_tick`, `is_running`, `set_running`

**Implements**: `SystemService` (name: `ecc.cognitive_tick`)

### crossref.rs (~425 lines, 12 tests)

**Purpose**: Universal Node IDs (BLAKE3) and cross-structure references.

**Key Types**: `UniversalNodeId`, `StructureTag`, `CrossRefType`, `CrossRef`, `CrossRefStore`

**Public API**: `UniversalNodeId::new` (BLAKE3 hash), `CrossRefStore::insert`, `get_forward`, `get_reverse`, `get_all`, `by_type`

### calibration.rs (~410 lines, 10 tests)

**Purpose**: Boot-time ECC benchmarking — measures HNSW insert/search + causal edge creation + BLAKE3 hashing.

**Key Types**: `EccCalibration`, `EccCalibrationConfig`

**Public API**: `run_calibration(hnsw, causal, config) -> EccCalibration`

### hnsw_service.rs (~280 lines, 11 tests)

**Purpose**: Thread-safe kernel wrapper around clawft-core's HnswStore.

**Key Types**: `HnswService`, `HnswServiceConfig`, `HnswSearchResult`

**Public API**: `insert`, `search`, `len`, `clear`, `insert_count`, `search_count`

**Implements**: `SystemService` (name: `ecc.hnsw`)

### impulse.rs (~320 lines, 8 tests)

**Purpose**: HLC-sorted ephemeral causal event queue for inter-structure communication.

**Key Types**: `ImpulseQueue`, `Impulse`, `ImpulseType`

**Public API**: `emit`, `drain_ready`, `pending_count`, `clear`

---

## K6 Planned: Mesh Networking

### mesh_noise.rs (K6.1, planned)

**Purpose**: Noise Protocol wrapper for encrypted mesh connections.

**Planned Types**: `NoiseChannel`, `NoiseHandshakeConfig`

**Post-Quantum Upgrade (K6.4b)**: After the Noise XX handshake completes,
an ML-KEM-768 key encapsulation upgrade runs inside the encrypted channel.
The final session key combines both X25519 and ML-KEM-768 shared secrets
via HKDF, protecting against store-now-decrypt-later quantum attacks.
Negotiated via `kem_supported: bool` in the `WeftHandshake` payload.
Graceful degradation when unsupported. Reuses
`ruvector-dag/src/qudag/crypto/ml_kem.rs` (`MlKem768::encapsulate/decapsulate`).

---

## Test Coverage

Total: 562 kernel tests (479 baseline + 83 ECC).

Each module has a `#[cfg(test)] mod tests` block. Run with:
```bash
scripts/build.sh test                                      # full workspace
cargo test -p clawft-kernel --features exochain             # kernel + exochain (479 tests)
cargo test -p clawft-kernel --features exochain,ecc         # kernel + exochain + ecc (562 tests)
```
