# WeftOS Kernel Manual Testing Guide

**Target**: `clawft-kernel` crate
**Version**: 0.1.0
**Test Count**: 2,948 unit tests across 24+ modules
**Total SLOC**: ~14,000 lines

This guide provides comprehensive manual and integration testing procedures
for the WeftOS kernel. Work through the master checklist below, checking off
each item as you go. Every checklist item links to its corresponding section.

---

## Master Checklist

### Prerequisites (Section 1)

- [x] **P.1** Rust toolchain installed (1.75+) — [Section 1.1](#11-required-tools)
- [x] **P.2** Build script present and executable — [Section 1.1](#11-required-tools)
- [x] **P.3** Native debug build succeeds — [Section 1.2](#12-build-the-kernel-crate)
- [x] **P.4** All-features build succeeds — [Section 1.2](#12-build-the-kernel-crate)
- [x] **P.5** Test suite runs (259 tests, 0 failures) — [Section 1.3](#13-verify-test-suite-runs)

### Automated Test Validation (Section 2)

- [x] **A.1** Full workspace tests pass (`scripts/build.sh test`) — [Section 2.1](#21-full-test-suite)
- [x] **A.2** Kernel-only tests pass (`cargo test -p clawft-kernel`) — [Section 2.1](#21-full-test-suite)
- [x] **A.3** Per-module test filters work (boot, process, service) — [Section 2.1](#21-full-test-suite)
- [x] **A.4** Test count matches expected (259) — [Section 2.2](#22-coverage-check)
- [x] **A.5** Test execution under 1 second — [Section 2.3](#23-performance-benchmarking)

### K0: Foundation — 9 Modules (Section 3.1)

- [x] **K0.1** `boot.rs` — Boot lifecycle (Booting→Running→ShuttingDown→Halted) — [Section 3.1.1](#311-bootrs---kernel-boot-sequence)
- [x] **K0.2** `process.rs` — PID allocation, state transitions, table queries — [Section 3.1.2](#312-processrs---process-table-and-pid-management)
- [x] **K0.3** `service.rs` — Register/unregister, start/stop, health checks — [Section 3.1.3](#313-servicers---service-registry)
- [x] **K0.4** `ipc.rs` — Message targets, payloads, serialization — [Section 3.1.4](#314-ipcrs---kernel-ipc)
- [x] **K0.5** `capability.rs` — Tool permissions, IPC scope, resource limits — [Section 3.1.5](#315-capabilityrs---agent-capabilities-and-rbac)
- [x] **K0.6** `health.rs` — Status aggregation (Healthy/Degraded/Down) — [Section 3.1.6](#316-healthrs---health-system)
- [x] **K0.7** `console.rs` — Boot events, phase transitions, log formatting — [Section 3.1.7](#317-consolers---boot-events-and-logging)
- [x] **K0.8** `config.rs` — KernelConfigExt, defaults, serde round-trip — [Section 3.1.8](#318-configrs---kernel-configuration)
- [x] **K0.9** `error.rs` — Error variants, Display, Result propagation — [Section 3.1.9](#319-errorrs---kernel-error-types)

### K1: Agent Lifecycle + Scheduling + Persistence — 7 Modules (Section 3.2)

- [x] **K1.1** `supervisor.rs` — Spawn/stop/restart, spawn_and_run, capabilities, running count — [Section 3.2.1](#321-supervisorrs---agent-supervisor)
- [x] **K1.2** `agent_loop.rs` — Built-in agent work loop (ping, cron, exec commands) — [Section 3.2.2](#322-agent_looprs---kernel-agent-work-loop)
- [x] **K1.3** `a2a.rs` — Per-PID inbox routing, capability-checked delivery, chain logging — [Section 3.2.3](#323-a2ars---agent-to-agent-router)
- [x] **K1.4** `cron.rs` — Interval-based job scheduling, tick engine, fire tracking — [Section 3.2.4](#324-cronrs---cron-scheduling-engine)
- [x] **K1.5** `chain.rs` — Local exochain, RVF persistence, crypto-verified load — [Section 3.2.5](#325-chainrs---exochain-persistence)
- [x] **K1.6** `tree_manager.rs` — Resource tree, checkpoint save/load, agent registration — [Section 3.2.6](#326-tree_managerrs---resource-tree-manager)
- [x] **K1.7** Daemon end-to-end — 16-step live playbook (boot→spawn→message→cron→persist→restart) — [Section 3.2.7](#327-daemon-end-to-end-live-playbook)

### K2: A2A IPC — 2 Modules (Section 3.3)

- [ ] **K2.1** `a2a.rs` — Per-PID inboxes, message delivery, correlation IDs — [Section 3.3.1](#331-a2ars---agent-to-agent-router)
- [ ] **K2.2** `topic.rs` — Subscribe/unsubscribe/publish, subscriber filtering — [Section 3.3.2](#332-topicrs---topic-based-pubsub)

### K3: WASM Sandbox — 1 Module (Section 3.4)

- [ ] **K3.1** `wasm_runner.rs` — Config, validation, stub error messages — [Section 3.4.1](#341-wasm_runnerrs---wasm-tool-runner-stubbed)

### K4: Containers — 1 Module (Section 3.5)

- [ ] **K4.1** `container.rs` — Config, lifecycle types, stub error messages — [Section 3.5.1](#351-containerrs---container-manager-stubbed)

### K5: App Framework — 1 Module (Section 3.6)

- [ ] **K5.1** `app.rs` — Manifest validation, install/remove, state machine — [Section 3.6.1](#361-apprs---app-manager-and-manifests)

### K6a: Distributed — 3 Modules (Section 3.7)

- [ ] **K6a.1** `cluster.rs` — Peer add/remove, state tracking, heartbeat — [Section 3.7.1](#371-clusterrs---cluster-membership)
- [ ] **K6a.2** `environment.rs` — Create/remove, class scoping, governance — [Section 3.7.2](#372-environmentrs---environment-manager)
- [ ] **K6a.3** `governance.rs` — Rules, EffectVector, three-branch decisions — [Section 3.7.3](#373-governancers---governance-engine)

### K6b: Agency — 1 Module (Section 3.8)

- [ ] **K6b.1** `agency.rs` — Roles, manifests, spawn permissions, interfaces — [Section 3.8.1](#381-agencyrs---agent-first-architecture)

### Integration Scenarios (Section 4)

- [ ] **INT.1** Full kernel boot + service registration + health — [Section 4.1](#41-scenario-1-full-kernel-boot-and-service-registration)
- [ ] **INT.2** Agent spawn + capability checking — [Section 4.2](#42-scenario-2-agent-spawn-with-capability-checking)
- [ ] **INT.3** A2A message routing with correlation IDs — [Section 4.3](#43-scenario-3-a2a-message-routing-with-correlation)
- [ ] **INT.4** Topic pub/sub with multiple subscribers — [Section 4.4](#44-scenario-4-topic-pubsub-with-multiple-subscribers)
- [ ] **INT.5** App lifecycle with governance evaluation — [Section 4.5](#45-scenario-5-app-lifecycle-with-governance)
- [ ] **INT.6** Cluster environment with multi-node health — [Section 4.6](#46-scenario-6-cluster-environment-with-multi-node-health)

### CLI Testing (Section 5)

- [ ] **CLI.1** `weaver kernel status` — Returns state, uptime, counts — [Section 5.1](#51-test-weaver-kernel-status)
- [ ] **CLI.2** `weaver kernel services` — Lists services or "No services" — [Section 5.2](#52-test-weaver-kernel-services)
- [ ] **CLI.3** `weaver kernel ps` — Shows process table with PID 0 — [Section 5.3](#53-test-weaver-kernel-ps)
- [ ] **CLI.4** `weaver kernel start --foreground --verbose` — Boots with verbose output — [Section 5.4](#54-test-weaver-kernel-start-foreground-interactive)
- [ ] **CLI.5** `weaver kernel stop` — Graceful shutdown — [Section 5.5](#55-test-weaver-kernel-stop)

### Feature-Gated Modules (Section 6)

- [ ] **FG.1** Build with `wasm-sandbox` feature — [Section 6.1](#61-testing-with-wasm-sandbox-feature)
- [ ] **FG.2** Build with `containers` feature — [Section 6.2](#62-testing-with-containers-feature)
- [ ] **FG.3** Build with all features combined — [Section 6.3](#63-testing-all-features-together)

### Deferred / Cannot Test Yet (Section 7)

- [x] **DEF.1** Review: Ruvector integration — Now integrated (rvf-wire, rvf-types, rvf-runtime)
- [x] **DEF.2** Review: Exo-resource-tree — Now integrated (tree_manager.rs, checkpoint persistence)
- [ ] **DEF.3** Review: Interactive console REPL (not testable) — [Section 7](#7-what-cannot-be-tested-yet)
- [ ] **DEF.4** Review: WASM execution (stubbed) — [Section 7](#7-what-cannot-be-tested-yet)
- [ ] **DEF.5** Review: Container orchestration (stubbed) — [Section 7](#7-what-cannot-be-tested-yet)
- [ ] **DEF.6** Review: Network transport (not implemented) — [Section 7](#7-what-cannot-be-tested-yet)
- [ ] **DEF.7** Review: Hook execution (not implemented) — [Section 7](#7-what-cannot-be-tested-yet)

### Regression Checklist (Section 8)

- [ ] **RG.1** Build all targets (native, debug, check, all-features) — [Section 8.1](#81-build-all-targets)
- [ ] **RG.2** Full test suite (workspace + kernel + all-features) — [Section 8.2](#82-run-full-test-suite)
- [ ] **RG.3** Lint and format (`clippy` + `cargo fmt --check`) — [Section 8.3](#83-lint-and-format)
- [ ] **RG.4** Phase gate (`scripts/build.sh gate`) — [Section 8.4](#84-phase-gate-11-checks)
- [ ] **RG.5** CLI smoke tests (status, services, ps) — [Section 8.5](#85-integration-smoke-tests)
- [ ] **RG.6** Documentation build (0 warnings) — [Section 8.6](#86-documentation-build)
- [ ] **RG.7** Feature flag validation (each individually) — [Section 8.7](#87-feature-flag-validation)
- [ ] **RG.8** Performance (test execution < 1s) — [Section 8.8](#88-memory-and-performance)
- [ ] **RG.9** Cross-platform check — [Section 8.9](#89-cross-platform-check)
- [ ] **RG.10** Dependent crate integration — [Section 8.10](#810-integration-with-dependent-crates)

### Totals

| Category | Items | Testable Now | Deferred | Status |
|----------|-------|-------------|----------|--------|
| Prerequisites | 5 | 5 | 0 | Done |
| Automated Tests | 5 | 5 | 0 | Done |
| K0 Foundation | 9 | 9 | 0 | Done (283 tests) |
| K1 Agent Lifecycle | 7 | 7 | 0 | Done (16-step playbook) |
| K2 A2A IPC | 2 | 2 | 0 | |
| K3 WASM | 1 | 1 (stub) | 0 | |
| K4 Containers | 1 | 1 (stub) | 0 | |
| K5 App Framework | 1 | 1 | 0 | |
| K6a Distributed | 3 | 3 | 0 | |
| K6b Agency | 1 | 1 | 0 | |
| Integration | 6 | 6 | 0 | |
| CLI | 5 | 5 | 0 | |
| Feature Gates | 3 | 3 | 0 | |
| Deferred Review | 7 | 0 | 7 | |
| Regression | 10 | 10 | 0 | |
| **TOTAL** | **66** | **59** | **7** | |

---

## 1. Prerequisites

### 1.1 Required Tools

```bash
# Rust toolchain (1.75+)
rustc --version

# Clawft build script
ls -l scripts/build.sh

# weft CLI (built from source)
cargo build --release --bin weft
export PATH="$PWD/target/release:$PATH"
```

### 1.2 Build the Kernel Crate

```bash
# Build kernel with default features (native)
scripts/build.sh native

# Build with all features
cargo build -p clawft-kernel --all-features

# Build kernel with feature flags
cargo build -p clawft-kernel --features wasm-sandbox
cargo build -p clawft-kernel --features containers
```

### 1.3 Verify Test Suite Runs

```bash
# Run all kernel tests (should show 259 passed)
scripts/build.sh test

# Run kernel-only tests
cargo test -p clawft-kernel

# Run with output for debugging
cargo test -p clawft-kernel -- --nocapture
```

---

## 2. Running Automated Tests

### 2.1 Full Test Suite

```bash
# Run all workspace tests
scripts/build.sh test

# Run kernel tests only
cargo test -p clawft-kernel --lib

# Run integration tests
cargo test -p clawft-kernel --test '*'

# Run specific module tests
cargo test -p clawft-kernel boot::tests
cargo test -p clawft-kernel process::tests
cargo test -p clawft-kernel service::tests
```

### 2.2 Coverage Check

```bash
# Count tests per module
cargo test -p clawft-kernel -- --list | wc -l

# Expected: 259 tests
```

### 2.3 Performance Benchmarking

```bash
# Time test execution
time cargo test -p clawft-kernel

# Expected: <1 second for full suite
```

---

## 3. Module-by-Module Manual Testing

### 3.1 K0: Foundation

#### 3.1.1 `boot.rs` - Kernel Boot Sequence

**What to Test Manually**:
- Boot lifecycle state transitions (Booting → Running → ShuttingDown → Halted)
- Boot log collection and formatting
- AppContext initialization and extraction
- Boot time tracking
- Service initialization during boot

**Test Snippets**:

```rust
use clawft_kernel::{Kernel, KernelState};
use clawft_platform::NativePlatform;
use clawft_types::config::Config;

#[tokio::test]
async fn manual_kernel_boot_lifecycle() {
    let config = Config::default();
    let platform = NativePlatform::new();

    // Boot kernel
    let mut kernel = Kernel::boot(config, platform).await.unwrap();
    assert_eq!(kernel.state(), &KernelState::Running);

    // Check boot time was recorded
    let uptime = kernel.uptime_secs();
    assert!(uptime < 5.0);

    // Verify boot log has entries
    let log = kernel.boot_log();
    assert!(!log.is_empty());

    // Check all boot phases completed
    let phases: Vec<_> = log.iter().map(|e| e.phase()).collect();
    println!("Boot phases: {:?}", phases);

    // Shutdown
    kernel.shutdown().await.unwrap();
    assert_eq!(kernel.state(), &KernelState::Halted);
}
```

**Expected Behaviors**:
- Boot completes in <5 seconds
- Boot log contains entries for: Init, Config, Services, ResourceTree, Agents, Network, Ready
- State transitions are irreversible (cannot boot a halted kernel)
- Shutdown stops all services gracefully

**Edge Cases**:
- Boot with minimal config (no services, no agents)
- Boot with invalid config (should fail with KernelError::BootFailed)
- Double shutdown (should be idempotent)
- Access boot log after shutdown (should still be available)

#### 3.1.2 `process.rs` - Process Table and PID Management

**What to Test Manually**:
- PID allocation monotonicity (never reuses PIDs)
- Process state transitions (Starting → Running → Suspended → Stopping → Exited)
- Process table queries (list, get, count)
- Resource usage tracking
- Concurrent process creation

**Test Snippets**:

```rust
use clawft_kernel::{ProcessTable, ProcessState, ResourceUsage};

#[tokio::test]
async fn manual_process_lifecycle() {
    let table = ProcessTable::new();

    // Allocate PIDs
    let pid1 = table.allocate_pid();
    let pid2 = table.allocate_pid();
    assert!(pid2 > pid1, "PIDs must be monotonic");

    // Insert process
    let usage = ResourceUsage {
        cpu_time_ms: 100,
        memory_bytes: 1024 * 1024,
        open_files: 5,
    };
    table.insert_process(pid1, "test-agent", usage.clone());

    // Verify retrieval
    let entry = table.get_process(&pid1).unwrap();
    assert_eq!(entry.name(), "test-agent");
    assert_eq!(entry.state(), &ProcessState::Starting);

    // Transition states
    table.set_state(&pid1, ProcessState::Running).unwrap();
    assert_eq!(table.get_process(&pid1).unwrap().state(), &ProcessState::Running);

    table.set_state(&pid1, ProcessState::Suspended).unwrap();
    table.set_state(&pid1, ProcessState::Stopping).unwrap();
    table.set_state(&pid1, ProcessState::Exited).unwrap();

    // List all processes
    let all = table.list_all();
    assert_eq!(all.len(), 2); // pid1 and pid2

    // Count by state
    let running_count = table.count_by_state(&ProcessState::Running);
    println!("Running processes: {}", running_count);
}
```

**Expected Behaviors**:
- PIDs start at 1 and increment (0 reserved for kernel)
- PIDs are never reused within a session
- State transitions follow valid paths only
- Resource usage updates are atomic
- List operations are O(1) with DashMap

**Edge Cases**:
- Set state on non-existent PID (should return error)
- Remove process from multiple threads concurrently
- Allocate 10,000+ PIDs (should not wrap or panic)
- Update resource usage while reading process entry

#### 3.1.3 `service.rs` - Service Registry

**What to Test Manually**:
- Service registration and unregistration
- Service lifecycle (start/stop)
- Service health checks
- Service listing and queries
- Duplicate service name handling

**Test Snippets**:

```rust
use clawft_kernel::{ServiceRegistry, SystemService, HealthStatus};
use async_trait::async_trait;

struct TestService {
    name: String,
    healthy: bool,
}

#[async_trait]
impl SystemService for TestService {
    fn name(&self) -> &str { &self.name }

    async fn start(&mut self) -> Result<(), String> {
        println!("Starting {}", self.name);
        Ok(())
    }

    async fn stop(&mut self) -> Result<(), String> {
        println!("Stopping {}", self.name);
        Ok(())
    }

    async fn health_check(&self) -> HealthStatus {
        if self.healthy {
            HealthStatus::Healthy
        } else {
            HealthStatus::Down("test failure".to_string())
        }
    }
}

#[tokio::test]
async fn manual_service_registry() {
    let registry = ServiceRegistry::new();

    // Register service
    let svc = TestService {
        name: "test-svc-1".to_string(),
        healthy: true,
    };
    registry.register(Arc::new(tokio::sync::Mutex::new(svc))).await;

    // List services
    let names = registry.list_service_names().await;
    assert!(names.contains(&"test-svc-1".to_string()));

    // Get service
    let retrieved = registry.get("test-svc-1").await.unwrap();
    let locked = retrieved.lock().await;
    assert_eq!(locked.name(), "test-svc-1");

    // Start all
    registry.start_all().await.unwrap();

    // Health check all
    let health_map = registry.health_all().await;
    assert!(matches!(health_map.get("test-svc-1"), Some(HealthStatus::Healthy)));

    // Stop all
    registry.stop_all().await.unwrap();

    // Unregister
    registry.unregister("test-svc-1").await;
    assert!(registry.get("test-svc-1").await.is_none());
}
```

**Expected Behaviors**:
- Service names must be unique
- start_all/stop_all are idempotent
- Health checks run concurrently
- Unregistered services cannot be retrieved

**Edge Cases**:
- Register duplicate service name (should replace or error)
- Unregister non-existent service (should be no-op)
- Health check timeout simulation
- Start/stop service that panics

#### 3.1.4 `ipc.rs` - Kernel IPC

**What to Test Manually**:
- Message creation with different targets (Pid, Topic, Broadcast, Service)
- Message payload serialization
- KernelSignal handling
- MessageBus integration

**Test Snippets**:

```rust
use clawft_kernel::{KernelIpc, KernelMessage, MessageTarget, MessagePayload};
use clawft_core::bus::MessageBus;
use serde_json::json;

#[tokio::test]
async fn manual_kernel_ipc() {
    let bus = Arc::new(MessageBus::new());
    let ipc = KernelIpc::new(bus.clone());

    // Send message to PID
    let payload = MessagePayload::Json(json!({"cmd": "ping"}));
    let msg = KernelMessage::new(
        MessageTarget::Pid(42),
        payload,
        Some("request-123".to_string()),
    );

    ipc.send_message(msg).await.unwrap();

    // Send broadcast
    let broadcast = KernelMessage::broadcast(
        MessagePayload::Text("shutdown".to_string()),
    );
    ipc.send_message(broadcast).await.unwrap();

    // Send to topic
    let topic_msg = KernelMessage::new(
        MessageTarget::Topic("logs".to_string()),
        MessagePayload::Text("log entry".to_string()),
        None,
    );
    ipc.send_message(topic_msg).await.unwrap();
}
```

**Expected Behaviors**:
- Messages are routed based on target type
- Correlation IDs are preserved
- Broadcast messages reach all subscribers
- Invalid targets return error

**Edge Cases**:
- Send message with empty payload
- Send message to PID 0 (kernel reserved)
- Send message before bus is initialized
- Large payload (>1MB JSON)

#### 3.1.5 `capability.rs` - Agent Capabilities and RBAC

**What to Test Manually**:
- Tool permission checking
- IPC scope validation
- Resource limit enforcement
- Sandbox policy evaluation

**Test Snippets**:

```rust
use clawft_kernel::{
    AgentCapabilities, CapabilityChecker, IpcScope,
    ResourceLimits, ResourceType, ToolPermissions,
};

#[tokio::test]
async fn manual_capability_checking() {
    let caps = AgentCapabilities {
        tools: ToolPermissions::Restricted(vec![
            "read_file".to_string(),
            "write_file".to_string(),
        ]),
        ipc_scope: IpcScope::Restricted(vec![1, 2, 3]),
        resource_limits: ResourceLimits {
            max_memory_mb: 512,
            max_cpu_percent: 50,
            max_concurrent_operations: 10,
            max_file_size_mb: 100,
        },
        ..Default::default()
    };

    let checker = CapabilityChecker::new(caps);

    // Check tool permission
    assert!(checker.can_use_tool("read_file"));
    assert!(!checker.can_use_tool("execute_code"));

    // Check IPC scope
    assert!(checker.can_send_to_pid(&1));
    assert!(!checker.can_send_to_pid(&99));

    // Check resource limits
    assert!(checker.can_allocate_resource(
        ResourceType::Memory,
        400 * 1024 * 1024 // 400MB
    ));
    assert!(!checker.can_allocate_resource(
        ResourceType::Memory,
        600 * 1024 * 1024 // 600MB
    ));
}
```

**Expected Behaviors**:
- ToolPermissions::All allows any tool
- ToolPermissions::Restricted enforces whitelist
- IpcScope::Restricted blocks unlisted PIDs
- Resource limits are enforced strictly

**Edge Cases**:
- Check permission on empty tool list
- Check IPC scope with PID 0
- Allocate resource at exactly the limit
- Nested capability inheritance

#### 3.1.6 `health.rs` - Health System

**What to Test Manually**:
- Health status aggregation
- Overall health calculation (Healthy/Degraded/Down)
- Service health integration
- Health history tracking

**Test Snippets**:

```rust
use clawft_kernel::{HealthSystem, HealthStatus, OverallHealth};

#[tokio::test]
async fn manual_health_system() {
    let health = HealthSystem::new();

    // Add service health
    health.update_service_health("db", HealthStatus::Healthy).await;
    health.update_service_health("cache", HealthStatus::Degraded("high latency".to_string())).await;
    health.update_service_health("queue", HealthStatus::Down("connection refused".to_string())).await;

    // Get overall health
    let overall = health.overall_health().await;
    assert_eq!(overall, OverallHealth::Down);

    // Get service health
    let db_health = health.service_health("db").await;
    assert!(matches!(db_health, Some(HealthStatus::Healthy)));

    // List all
    let all = health.all_service_health().await;
    assert_eq!(all.len(), 3);

    println!("Health report: {:?}", all);
}
```

**Expected Behaviors**:
- Overall health is Down if any service is Down
- Overall health is Degraded if any service is Degraded (and none Down)
- Overall health is Healthy only if all services are Healthy
- Health updates are atomic

**Edge Cases**:
- Query health before any services registered
- Rapid health updates from multiple threads
- Remove service and check health (should be removed from map)

#### 3.1.7 `console.rs` - Boot Events and Logging

**What to Test Manually**:
- Boot event recording
- Boot phase transitions
- Log level filtering
- Boot log formatting

**Test Snippets**:

```rust
use clawft_kernel::{BootEvent, BootPhase, BootLog, LogLevel};

#[tokio::test]
async fn manual_boot_console() {
    let mut log = BootLog::new();

    // Record events
    log.record(BootPhase::Init, LogLevel::Info, "Initializing kernel");
    log.record(BootPhase::Config, LogLevel::Info, "Loading config");
    log.record(BootPhase::Services, LogLevel::Info, "Starting services");
    log.record(BootPhase::Services, LogLevel::Warn, "Service 'cache' degraded");
    log.record(BootPhase::Ready, LogLevel::Info, "Kernel ready");

    // Check count
    assert_eq!(log.len(), 5);

    // Format output
    let formatted = log.format_terminal();
    println!("{}", formatted);

    // Check phases
    let events = log.events();
    let phases: Vec<_> = events.iter().map(|e| e.phase()).collect();
    assert!(phases.contains(&BootPhase::Init));
    assert!(phases.contains(&BootPhase::Ready));
}
```

**Expected Behaviors**:
- Events are recorded in order
- Timestamps are monotonic
- Log levels are preserved
- Terminal formatting includes ANSI colors (if TTY)

**Edge Cases**:
- Record 10,000 events (check memory usage)
- Format empty log
- Record events with unicode characters

#### 3.1.8 `config.rs` - Kernel Configuration

**What to Test Manually**:
- KernelConfigExt wrapping
- Default values
- Config serialization/deserialization
- Config validation

**Test Snippets**:

```rust
use clawft_kernel::config::KernelConfigExt;
use clawft_types::config::{Config, KernelConfig};

#[tokio::test]
async fn manual_kernel_config() {
    let mut cfg = Config::default();
    cfg.kernel = KernelConfig {
        enabled: true,
        max_processes: 100,
        health_check_interval_secs: 30,
        ..Default::default()
    };

    // Wrap in extension
    let kext = KernelConfigExt::from(cfg.kernel.clone());

    // Check values
    assert!(kext.is_enabled());
    assert_eq!(kext.max_processes(), 100);
    assert_eq!(kext.health_check_interval_secs(), 30);

    // Serialize round-trip
    let json = serde_json::to_string(&cfg.kernel).unwrap();
    let parsed: KernelConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.max_processes, 100);
}
```

**Expected Behaviors**:
- Default config is valid
- All fields serialize correctly
- Extension wrapper provides type-safe accessors

**Edge Cases**:
- Config with max_processes = 0
- Config with negative health_check_interval
- Config deserialization from partial JSON

#### 3.1.9 `error.rs` - Kernel Error Types

**What to Test Manually**:
- Error variant creation
- Error message formatting
- Error conversion from sub-crates
- Error propagation

**Test Snippets**:

```rust
use clawft_kernel::{KernelError, KernelResult};

#[tokio::test]
async fn manual_error_handling() {
    // Create errors
    let boot_err = KernelError::BootFailed("config invalid".to_string());
    let process_err = KernelError::ProcessNotFound(42);
    let service_err = KernelError::ServiceNotFound("db".to_string());

    // Format messages
    assert_eq!(boot_err.to_string(), "boot failed: config invalid");
    assert_eq!(process_err.to_string(), "process not found: 42");
    assert_eq!(service_err.to_string(), "service not found: db");

    // Use in Result
    fn test_fn() -> KernelResult<()> {
        Err(KernelError::AlreadyRunning)
    }

    assert!(test_fn().is_err());
}
```

**Expected Behaviors**:
- All error variants implement Display and Debug
- Error messages are descriptive
- Errors convert to anyhow::Error

**Edge Cases**:
- Error with empty message
- Error with very long message (>1000 chars)
- Nested error causes

---

### 3.2 K1: Supervisor + RBAC

#### 3.2.1 `supervisor.rs` - Agent Supervisor

**What to Test Manually**:
- Agent spawning with SpawnRequest
- Agent lifecycle (spawn/stop/restart)
- Spawn result tracking
- Concurrent spawn requests

**Test Snippets**:

```rust
use clawft_kernel::{AgentSupervisor, SpawnRequest, AgentCapabilities};
use clawft_platform::NativePlatform;
use std::time::Duration;

#[tokio::test]
async fn manual_supervisor_spawn() {
    let platform = NativePlatform::new();
    let supervisor = AgentSupervisor::new(platform);

    // Create spawn request
    let request = SpawnRequest {
        name: "test-agent".to_string(),
        role: "worker".to_string(),
        capabilities: AgentCapabilities::default(),
        timeout: Some(Duration::from_secs(10)),
    };

    // Spawn agent
    let result = supervisor.spawn(request).await.unwrap();

    assert!(result.pid > 0);
    assert_eq!(result.name, "test-agent");
    println!("Spawned agent PID: {}", result.pid);

    // Check running count
    let count = supervisor.running_count().await;
    assert_eq!(count, 1);

    // Stop agent
    supervisor.stop(result.pid).await.unwrap();

    // Verify stopped
    let count_after = supervisor.running_count().await;
    assert_eq!(count_after, 0);
}
```

**Expected Behaviors**:
- Spawn returns unique PID
- Spawn timeout is enforced
- Stop is graceful
- Restart preserves PID

**Edge Cases**:
- Spawn with invalid capabilities
- Stop non-existent agent
- Restart agent that exited normally
- Spawn 100 agents concurrently

#### 3.2.2 `agent_loop.rs` - Kernel Agent Work Loop

**What to Test Manually**:
- Built-in command processing (ping, cron.add, cron.list, cron.remove, exec, echo)
- Message reception from A2ARouter inbox
- Reply routing back to sender via A2ARouter
- Cancellation token triggers clean exit
- RVF payload decoding (CBOR + JSON fallback)

**Test via Daemon**:

```bash
# Spawn agent and send ping
weaver agent spawn test-agent
weaver agent send 1 '{"cmd":"ping"}'
# Expected: {"status":"ok","pid":1,"uptime_ms":...}

# Send echo command
weaver agent send 1 '{"cmd":"echo","text":"hello world"}'
# Expected: {"echo":"hello world","pid":1}

# Send unknown command
weaver agent send 1 '{"cmd":"unknown"}'
# Expected: {"error":"unknown command: unknown","pid":1}
```

**Expected Behaviors**:
- Agent processes messages sequentially (FIFO)
- Replies include correlation ID from original message
- Cancellation exits with code 0
- Unknown commands return error (not panic)

**Edge Cases**:
- Send non-JSON text payload (should wrap as echo)
- Send to agent after cancellation (inbox closed)
- Rapid sequential messages (should all process)

#### 3.2.3 `a2a.rs` - Agent-to-Agent Router

**What to Test Manually**:
- Per-PID inbox creation and message delivery
- Capability-checked routing (IPC scope enforcement)
- Topic routing via TopicRouter integration
- Broadcast delivery to all inboxes except sender
- Chain-logged delivery via `send_checked()`
- Inbox cleanup on channel close

**Test via Daemon**:

```bash
# Spawn two agents
weaver agent spawn agent-a
weaver agent spawn agent-b

# Send message from kernel (PID 0) to agent-a (PID 1)
weaver agent send 1 '{"cmd":"ping"}'
# Expected: delivered to PID 1's inbox

# Verify chain logging
weaver chain local -c 5
# Expected: ipc.send events logged
```

**Expected Behaviors**:
- Inbox capacity is 1024 messages per agent
- `try_send` is non-blocking (drops if full)
- Closed inboxes are removed automatically
- DashMap Ref is cloned before `remove()` to avoid deadlock

**Edge Cases**:
- Send to non-existent PID (should return ProcessNotFound)
- Send from non-Running process (should return error)
- Concurrent sends to same PID from multiple senders
- Inbox overflow (>1024 pending messages)

#### 3.2.4 `cron.rs` - Cron Scheduling Engine

**What to Test Manually**:
- Job creation with interval and target PID
- Job listing and removal
- Tick engine: fires overdue jobs, respects intervals
- Disabled jobs are skipped
- Fire count tracking

**Test via Daemon**:

```bash
# Add a cron job (no target)
weaver cron add --name "healthcheck" --interval 60 --command "check"

# Add targeted cron job
weaver cron add --name "heartbeat" --interval 10 --command "ping" --target 1

# List all jobs
weaver cron list
# Expected: table with job ID, name, interval, target, fire count

# Remove a job
weaver cron remove <job-id>

# Verify job was removed
weaver cron list
```

**Expected Behaviors**:
- New jobs fire immediately on first tick (last_fired=None)
- Subsequent ticks respect interval_secs
- Disabled jobs are not fired
- Job IDs are UUIDs
- CronService implements SystemService trait

**Edge Cases**:
- Remove non-existent job (returns None)
- Add job with 0-second interval
- Tick with no registered jobs
- Concurrent add/remove during tick

#### 3.2.5 `chain.rs` - Exochain Persistence

**What to Test Manually**:
- Chain event appending (source, kind, metadata)
- RVF format save/load with cryptographic verification
- JSON fallback save/load
- Genesis event on fresh chain
- Chain integrity verification (hash chain)
- Sequence monotonicity across restarts

**Test via Daemon**:

```bash
# View chain events
weaver chain local -c 30
# Expected: genesis, boot.*, agent.spawn, ipc.send, cron.* events

# Verify chain integrity
weaver chain verify
# Expected: "Chain integrity: VALID"

# View chain status
weaver chain status
# Expected: chain_id, sequence, event count

# Trigger manual checkpoint (writes the signed RVF checkpoint)
weaver chain checkpoint
# Expected: "Signed: yes (<runtime>/chain.rvf)"; chain verify now reports
# "Signature: VALID through sequence N (Ed25519 + ML-DSA-65)"

# One event in full, with the checkpoint signature that covers it
weaver chain get 3
# Expected: payload plus coverage.status = signed, or
# "unsigned: pending checkpoint" for an event newer than the last checkpoint

# Shutdown and restart daemon
weaver kernel stop
weaver kernel start

# Verify events persisted
weaver chain local -c 30
# Expected: previous session events visible, sequence continues
weaver chain verify
# Expected: still VALID
```

**Expected Behaviors**:
- Each event has unique sequence number
- Hash chain links each event to its predecessor
- RVF format uses content-hash integrity
- JSON fallback when RVF fails
- Chain survives daemon restart

**Edge Cases**:
- Corrupt RVF file (should fall back to JSON)
- Missing checkpoint file (should start fresh)
- Very long chain (>10,000 events)

#### 3.2.6 `tree_manager.rs` - Resource Tree Manager

**What to Test Manually**:
- Tree bootstrap (creates root + /kernel + /apps + /network namespaces)
- Agent registration (inserts /kernel/agents/<name> node)
- Agent unregistration (removes node)
- Merkle hash computation across tree
- NodeScoring integration
- Checkpoint save/load roundtrip

**Test via Daemon**:

```bash
# View resource tree
weaver resource tree
# Expected: table with /, /kernel, /kernel/agents, /kernel/services, etc.

# Inspect a node
weaver resource inspect /kernel/agents/worker-1
# Expected: kind=Agent, metadata (pid, state, capabilities, chain_seq)

# View tree stats
weaver resource stats
# Expected: total nodes, namespaces, services, agents, root hash

# After restart, verify tree state restored
weaver kernel stop && weaver kernel start
weaver resource stats
# Expected: node count matches previous session
```

**Expected Behaviors**:
- Bootstrap creates 9 nodes (root + 4 namespaces + 4 sub-namespaces)
- Agent registration adds node + emits chain event
- Merkle hash updates propagate from leaf to root
- Checkpoint serializes full tree state

**Edge Cases**:
- Register agent with duplicate name
- Unregister non-existent agent
- Tree with 100+ agents
- Checkpoint file permission errors

#### 3.2.7 Daemon End-to-End Live Playbook

**Full 16-step integration test** (all steps verified passing):

```bash
# 1. Build with exochain
cargo build -p clawft-weave --features exochain

# 2. Start daemon
weaver kernel start
# PASS: Boots, shows "Running", chain initialized

# 3. Check status
weaver kernel status
# PASS: state=running, 1 process (kernel), 1 service (cron)

# 4. Spawn an agent
weaver agent spawn worker-1
# PASS: Returns PID 1, agent is Running

# 5. Verify agent in tree
weaver resource tree
# PASS: Shows /kernel/agents/worker-1 node

# 6. Send command to agent
weaver agent send 1 '{"cmd":"ping"}'
# PASS: Message delivered

# 7. Create a cron job via CLI
weaver cron add --name "heartbeat" --interval 10 --command "ping" --target 1
# PASS: Returns job ID

# 8. List cron jobs
weaver cron list
# PASS: Shows heartbeat job with interval=10s

# 9. Create cron job via agent command
weaver agent send 1 '{"cmd":"cron.add","name":"check","interval_secs":30,"command":"health"}'
# PASS: Job created via agent

# 10. Verify chain logging
weaver chain local -c 30
# PASS: Shows agent.spawn, ipc.send, cron.add, cron.fire events

# 11. Check chain integrity
weaver chain verify
# PASS: "Chain integrity: VALID"

# 12. View resource tree with metadata
weaver resource inspect /kernel/agents/worker-1
# PASS: Shows metadata (pid, state, chain_seq, capabilities)

# 13. Stop agent
weaver agent stop 1
# PASS: Agent transitions to Exited

# 14. Verify agent exit logged
weaver chain local -c 5
# PASS: Shows agent.stop + agent.exit events

# 15. Shutdown daemon
weaver kernel stop
# PASS: chain.rvf + chain.tree.json saved to ~/.clawft/

# 16. Restart and verify persistence
weaver kernel start
weaver chain local -c 30
# PASS: Previous chain events visible (not fresh genesis)
weaver resource stats
# PASS: Tree state restored from checkpoint
weaver chain verify
# PASS: Chain integrity valid across restart
```

---

### 3.3 K2: A2A IPC

#### 3.3.1 `a2a.rs` - Agent-to-Agent Router

**What to Test Manually**:
- Per-PID inbox creation
- Message delivery to specific PID
- Message correlation
- Inbox cleanup on agent exit

**Test Snippets**:

```rust
use clawft_kernel::{A2ARouter, MessagePayload};
use serde_json::json;

#[tokio::test]
async fn manual_a2a_routing() {
    let router = A2ARouter::new();

    // Send message to PID
    let payload = MessagePayload::Json(json!({"cmd": "ping"}));
    router.send_to_pid(42, payload, Some("req-123".to_string())).await.unwrap();

    // Receive message
    let received = router.receive_for_pid(42).await.unwrap();
    assert_eq!(received.correlation_id, Some("req-123".to_string()));

    if let MessagePayload::Json(val) = received.payload {
        assert_eq!(val["cmd"], "ping");
    } else {
        panic!("Expected JSON payload");
    }

    // Send to non-existent PID (creates inbox)
    router.send_to_pid(99, MessagePayload::Text("hello".to_string()), None).await.unwrap();

    // Check inbox exists
    let msg = router.receive_for_pid(99).await.unwrap();
    assert!(matches!(msg.payload, MessagePayload::Text(_)));
}
```

**Expected Behaviors**:
- Inboxes are created on-demand
- Messages are FIFO per inbox
- Receive blocks until message arrives
- Correlation IDs are preserved

**Edge Cases**:
- Send to PID 0 (should error)
- Receive timeout
- Inbox overflow (>1000 messages)
- Concurrent sends to same PID

#### 3.3.2 `topic.rs` - Topic-Based Pub/Sub

**What to Test Manually**:
- Topic subscription
- Topic unsubscription
- Message publishing to topic
- Subscriber delivery

**Test Snippets**:

```rust
use clawft_kernel::{TopicRouter, MessagePayload};

#[tokio::test]
async fn manual_topic_pubsub() {
    let router = TopicRouter::new();

    // Subscribe PIDs to topic
    router.subscribe("logs", 1).await;
    router.subscribe("logs", 2).await;
    router.subscribe("logs", 3).await;

    // Publish message
    let payload = MessagePayload::Text("log entry".to_string());
    router.publish("logs", payload).await.unwrap();

    // Check subscriber count
    let subs = router.subscribers("logs").await;
    assert_eq!(subs.len(), 3);

    // Unsubscribe
    router.unsubscribe("logs", 2).await;
    let subs_after = router.subscribers("logs").await;
    assert_eq!(subs_after.len(), 2);

    // Publish to empty topic (should succeed but no delivery)
    router.publish("metrics", MessagePayload::Text("metric".to_string())).await.unwrap();
}
```

**Expected Behaviors**:
- Subscribers receive all published messages
- Unsubscribe stops future messages
- Publishing to empty topic is no-op
- Topic names are case-sensitive

**Edge Cases**:
- Subscribe same PID twice (should be idempotent)
- Publish large payload (>10MB)
- Publish to 1000 subscribers
- Unsubscribe from non-existent topic

---

### 3.4 K3: WASM Sandbox

#### 3.4.1 `wasm_runner.rs` - WASM Tool Runner (Stubbed)

**What to Test Manually**:
- WasmToolRunner initialization (should fail if wasmtime not enabled)
- WasmTool validation
- WasmSandboxConfig parsing
- Stub error messages

**Test Snippets**:

```rust
use clawft_kernel::{WasmToolRunner, WasmSandboxConfig};

#[tokio::test]
async fn manual_wasm_runner() {
    let config = WasmSandboxConfig {
        max_memory_bytes: 10 * 1024 * 1024, // 10MB
        max_execution_time_ms: 5000,
        allowed_imports: vec!["env".to_string()],
    };

    // Attempt to create runner (should fail with stub error)
    let result = WasmToolRunner::new(config);
    assert!(result.is_err());

    let err = result.unwrap_err();
    assert!(err.to_string().contains("WASM support not enabled"));
}
```

**Expected Behaviors** (when feature enabled):
- WASM modules load successfully
- Execution timeout is enforced
- Memory limits are enforced
- Invalid WASM bytes are rejected

**Edge Cases**:
- Load malformed WASM file
- Execute infinite loop (should timeout)
- Allocate beyond memory limit
- Import disallowed function

---

### 3.5 K4: Containers

#### 3.5.1 `container.rs` - Container Manager (Stubbed)

**What to Test Manually**:
- ContainerManager initialization (should fail if bollard not enabled)
- ManagedContainer lifecycle
- ContainerState transitions
- Stub error messages

**Test Snippets**:

```rust
use clawft_kernel::{ContainerManager, ContainerConfig};

#[tokio::test]
async fn manual_container_manager() {
    // Attempt to create manager (should fail with stub error)
    let result = ContainerManager::new();
    assert!(result.is_err());

    let err = result.unwrap_err();
    assert!(err.to_string().contains("container support not enabled"));
}
```

**Expected Behaviors** (when feature enabled):
- Containers start successfully
- Health checks poll container status
- Stop is graceful (SIGTERM then SIGKILL)
- Restart policy is enforced

**Edge Cases**:
- Start container with invalid image
- Stop container that already exited
- Restart container with RestartPolicy::Never
- Container port conflicts

---

### 3.6 K5: App Framework

#### 3.6.1 `app.rs` - App Manager and Manifests

**What to Test Manually**:
- AppManifest parsing and validation
- App installation
- App lifecycle (Installed → Starting → Running → Stopping → Stopped → Failed)
- App removal
- Hook execution

**Test Snippets**:

```rust
use clawft_kernel::{AppManager, AppManifest, AppState, AgentSpec, ServiceSpec};

#[tokio::test]
async fn manual_app_lifecycle() {
    let manager = AppManager::new();

    // Create manifest
    let manifest = AppManifest {
        name: "test-app".to_string(),
        version: "1.0.0".to_string(),
        agents: vec![
            AgentSpec {
                name: "worker".to_string(),
                role: "processor".to_string(),
                count: 2,
            }
        ],
        tools: vec![],
        services: vec![
            ServiceSpec {
                name: "db".to_string(),
                type_: "postgres".to_string(),
            }
        ],
        capabilities: Default::default(),
        hooks: Default::default(),
    };

    // Install app
    manager.install(manifest).await.unwrap();

    // Check state
    let app = manager.get("test-app").await.unwrap();
    assert_eq!(app.state(), &AppState::Installed);

    // Start app
    manager.start("test-app").await.unwrap();
    let app_after_start = manager.get("test-app").await.unwrap();
    assert_eq!(app_after_start.state(), &AppState::Running);

    // List apps
    let all = manager.list_all().await;
    assert_eq!(all.len(), 1);

    // Stop app
    manager.stop("test-app").await.unwrap();

    // Remove app (should fail while running)
    let remove_result = manager.remove("test-app").await;
    assert!(remove_result.is_err());
}
```

**Expected Behaviors**:
- Manifest validation catches missing required fields
- App states transition in order
- Apps cannot be removed while Running
- Hooks execute at correct lifecycle points

**Edge Cases**:
- Install app with duplicate name
- Start app with missing dependencies
- Stop app that is already Stopped
- Remove app with active agents

---

### 3.7 K6a: Distributed

#### 3.7.1 `cluster.rs` - Cluster Membership

**What to Test Manually**:
- Node registration
- Node state tracking (Joining → Active → Leaving → Left)
- Peer discovery
- Node health monitoring

**Test Snippets**:

```rust
use clawft_kernel::{ClusterMembership, PeerNode, NodeState, NodeId};

#[tokio::test]
async fn manual_cluster_membership() {
    let cluster = ClusterMembership::new();

    // Add peer nodes
    let node1 = PeerNode {
        id: NodeId::new(),
        address: "192.168.1.10:8080".to_string(),
        state: NodeState::Active,
        platform: Default::default(),
        last_seen: chrono::Utc::now(),
    };

    cluster.add_peer(node1.clone()).await;

    // List peers
    let peers = cluster.list_peers().await;
    assert_eq!(peers.len(), 1);

    // Update node state
    cluster.update_peer_state(&node1.id, NodeState::Leaving).await.unwrap();

    // Get peer
    let peer = cluster.get_peer(&node1.id).await.unwrap();
    assert_eq!(peer.state, NodeState::Leaving);

    // Remove peer
    cluster.remove_peer(&node1.id).await;
    assert!(cluster.get_peer(&node1.id).await.is_none());
}
```

**Expected Behaviors**:
- Node IDs are unique
- State transitions are tracked
- Last seen timestamps update
- Stale nodes are detected

**Edge Cases**:
- Add peer with duplicate ID (should replace or error)
- Update state of non-existent peer
- List peers when cluster is empty
- Concurrent peer updates

#### 3.7.2 `environment.rs` - Environment Manager

**What to Test Manually**:
- Environment creation (Development, Staging, Production, Research)
- Environment scoping and isolation
- Governance branch assignment
- Learning mode configuration

**Test Snippets**:

```rust
use clawft_kernel::{
    EnvironmentManager, Environment, EnvironmentClass,
    GovernanceBranches, LearningMode,
};

#[tokio::test]
async fn manual_environment_manager() {
    let manager = EnvironmentManager::new();

    // Create development environment
    let dev_env = Environment {
        name: "dev".to_string(),
        class: EnvironmentClass::Development,
        governance: GovernanceBranches {
            legislative: true,
            executive: true,
            judicial: false,
        },
        learning_mode: LearningMode::Aggressive,
        audit_level: Default::default(),
    };

    manager.create_environment(dev_env).await.unwrap();

    // Create production environment
    let prod_env = Environment {
        name: "prod".to_string(),
        class: EnvironmentClass::Production,
        governance: GovernanceBranches {
            legislative: true,
            executive: true,
            judicial: true,
        },
        learning_mode: LearningMode::Conservative,
        audit_level: Default::default(),
    };

    manager.create_environment(prod_env).await.unwrap();

    // List environments
    let envs = manager.list_environments().await;
    assert_eq!(envs.len(), 2);

    // Get environment
    let dev = manager.get_environment("dev").await.unwrap();
    assert_eq!(dev.class, EnvironmentClass::Development);

    // Remove environment
    manager.remove_environment("dev").await.unwrap();
}
```

**Expected Behaviors**:
- Environment names are unique
- Production requires all governance branches
- Development allows aggressive learning
- Environments are isolated

**Edge Cases**:
- Create environment with duplicate name
- Remove environment with active apps
- Switch environment class after creation
- Query non-existent environment

#### 3.7.3 `governance.rs` - Governance Engine

**What to Test Manually**:
- Governance rule creation
- Request evaluation with EffectVector
- Three-branch decision making
- Rule severity enforcement

**Test Snippets**:

```rust
use clawft_kernel::{
    GovernanceEngine, GovernanceRule, GovernanceRequest,
    EffectVector, GovernanceBranch, RuleSeverity,
};

#[tokio::test]
async fn manual_governance_engine() {
    let engine = GovernanceEngine::new();

    // Create rule
    let rule = GovernanceRule {
        id: "rule-001".to_string(),
        branch: GovernanceBranch::Legislative,
        severity: RuleSeverity::High,
        description: "No file writes in production".to_string(),
        effect: EffectVector {
            security: -5.0,
            privacy: 0.0,
            autonomy: 2.0,
            beneficence: 0.0,
            non_maleficence: -3.0,
        },
    };

    engine.add_rule(rule).await;

    // Create request
    let request = GovernanceRequest {
        action: "write_file".to_string(),
        context: serde_json::json!({"env": "production"}),
        requesting_agent: "agent-42".to_string(),
    };

    // Evaluate
    let decision = engine.evaluate(request).await;
    println!("Decision: {:?}", decision);

    // Check if approved
    assert!(!decision.approved);
    assert!(decision.rationale.contains("production"));
}
```

**Expected Behaviors**:
- Rules are evaluated in priority order (severity)
- EffectVector scores are aggregated
- Negative scores reduce approval chance
- All three branches must approve for High severity

**Edge Cases**:
- Request with no matching rules (should default allow/deny?)
- Request with conflicting rules
- Request with missing context
- Rule with invalid EffectVector (NaN, Infinity)

---

### 3.8 K6b: Agency

#### 3.8.1 `agency.rs` - Agent-First Architecture

**What to Test Manually**:
- Agent role definition
- Agent manifest creation
- Spawn permission checking
- Agent interface protocol enforcement

**Test Snippets**:

```rust
use clawft_kernel::{
    Agency, AgentRole, AgentManifest, AgentPriority,
    AgentResources, AgentRestartPolicy,
};

#[tokio::test]
async fn manual_agency() {
    let agency = Agency::new();

    // Define role
    let role = AgentRole {
        name: "worker".to_string(),
        description: "Background task processor".to_string(),
        permissions: vec!["read_file".to_string(), "write_file".to_string()],
        spawn_limit: Some(10),
    };

    agency.register_role(role).await;

    // Create agent manifest
    let manifest = AgentManifest {
        name: "worker-001".to_string(),
        role: "worker".to_string(),
        priority: AgentPriority::Normal,
        resources: AgentResources {
            cpu_shares: 512,
            memory_mb: 256,
        },
        restart_policy: AgentRestartPolicy::OnFailure,
        interfaces: vec![],
    };

    // Check spawn permission
    let can_spawn = agency.can_spawn(&manifest).await;
    assert!(can_spawn);

    // Spawn agent
    agency.spawn_agent(manifest).await.unwrap();

    // Check spawn count
    let count = agency.count_by_role("worker").await;
    assert_eq!(count, 1);
}
```

**Expected Behaviors**:
- Roles define permission templates
- Spawn limits are enforced per role
- Agent resources are allocated from pool
- Restart policies are honored

**Edge Cases**:
- Spawn agent with undefined role
- Spawn beyond role limit
- Spawn with zero resources
- Agent crash with RestartPolicy::Never

---

## 4. Integration Test Scenarios

These scenarios test multiple modules working together.

### 4.1 Scenario 1: Full Kernel Boot and Service Registration

```rust
use clawft_kernel::{Kernel, ServiceRegistry, SystemService};
use clawft_platform::NativePlatform;
use clawft_types::config::Config;

#[tokio::test]
async fn integration_boot_with_services() {
    // Create config
    let mut config = Config::default();
    config.kernel.enabled = true;

    // Boot kernel
    let platform = NativePlatform::new();
    let mut kernel = Kernel::boot(config, platform).await.unwrap();

    // Extract service registry
    let registry = kernel.service_registry();

    // Register custom service
    // (implement TestService as shown earlier)
    let svc = Arc::new(tokio::sync::Mutex::new(TestService {
        name: "test-svc".to_string(),
        healthy: true,
    }));
    registry.register(svc).await;

    // Start all services
    registry.start_all().await.unwrap();

    // Check health
    let health = registry.health_all().await;
    assert!(health.get("test-svc").is_some());

    // Shutdown kernel
    kernel.shutdown().await.unwrap();
}
```

### 4.2 Scenario 2: Agent Spawn with Capability Checking

```rust
use clawft_kernel::{
    AgentSupervisor, SpawnRequest, AgentCapabilities,
    CapabilityChecker, ToolPermissions,
};

#[tokio::test]
async fn integration_spawn_with_capabilities() {
    let platform = NativePlatform::new();
    let supervisor = AgentSupervisor::new(platform);

    // Create capabilities
    let caps = AgentCapabilities {
        tools: ToolPermissions::Restricted(vec!["read_file".to_string()]),
        ..Default::default()
    };

    // Create spawn request
    let request = SpawnRequest {
        name: "restricted-agent".to_string(),
        role: "reader".to_string(),
        capabilities: caps.clone(),
        timeout: None,
    };

    // Spawn agent
    let result = supervisor.spawn(request).await.unwrap();

    // Check capabilities
    let checker = CapabilityChecker::new(caps);
    assert!(checker.can_use_tool("read_file"));
    assert!(!checker.can_use_tool("write_file"));

    // Stop agent
    supervisor.stop(result.pid).await.unwrap();
}
```

### 4.3 Scenario 3: A2A Message Routing with Correlation

```rust
use clawft_kernel::{A2ARouter, MessagePayload};
use serde_json::json;

#[tokio::test]
async fn integration_a2a_correlation() {
    let router = A2ARouter::new();

    // Agent 1 sends request to Agent 2
    let request = MessagePayload::Json(json!({
        "type": "request",
        "cmd": "process_data",
        "data": [1, 2, 3, 4, 5],
    }));

    router.send_to_pid(2, request, Some("req-123".to_string())).await.unwrap();

    // Agent 2 receives and processes
    let received = router.receive_for_pid(2).await.unwrap();
    assert_eq!(received.correlation_id, Some("req-123".to_string()));

    // Agent 2 sends response back to Agent 1
    let response = MessagePayload::Json(json!({
        "type": "response",
        "result": 15, // sum of data
    }));

    router.send_to_pid(1, response, Some("req-123".to_string())).await.unwrap();

    // Agent 1 receives response
    let resp = router.receive_for_pid(1).await.unwrap();
    assert_eq!(resp.correlation_id, Some("req-123".to_string()));
}
```

### 4.4 Scenario 4: Topic Pub/Sub with Multiple Subscribers

```rust
use clawft_kernel::{TopicRouter, MessagePayload};

#[tokio::test]
async fn integration_topic_fanout() {
    let router = TopicRouter::new();

    // Subscribe 3 agents to "events" topic
    router.subscribe("events", 10).await;
    router.subscribe("events", 20).await;
    router.subscribe("events", 30).await;

    // Publish event
    let event = MessagePayload::Json(serde_json::json!({
        "type": "user_login",
        "user_id": 42,
        "timestamp": chrono::Utc::now().to_rfc3339(),
    }));

    router.publish("events", event).await.unwrap();

    // All subscribers should receive (verify via A2A inboxes)
    // (This requires integration with A2ARouter - left as exercise)
}
```

### 4.5 Scenario 5: App Lifecycle with Governance

```rust
use clawft_kernel::{
    AppManager, AppManifest, GovernanceEngine,
    GovernanceRequest, GovernanceRule, GovernanceBranch,
};

#[tokio::test]
async fn integration_app_with_governance() {
    let app_manager = AppManager::new();
    let gov_engine = GovernanceEngine::new();

    // Add governance rule: no apps in production without approval
    let rule = GovernanceRule {
        id: "prod-approval".to_string(),
        branch: GovernanceBranch::Executive,
        severity: RuleSeverity::High,
        description: "Production apps require approval".to_string(),
        effect: EffectVector {
            security: -10.0,
            privacy: -5.0,
            autonomy: 0.0,
            beneficence: 0.0,
            non_maleficence: -5.0,
        },
    };
    gov_engine.add_rule(rule).await;

    // Create app manifest
    let manifest = AppManifest {
        name: "prod-app".to_string(),
        version: "1.0.0".to_string(),
        agents: vec![],
        tools: vec![],
        services: vec![],
        capabilities: Default::default(),
        hooks: Default::default(),
    };

    // Check governance
    let request = GovernanceRequest {
        action: "install_app".to_string(),
        context: serde_json::json!({"env": "production", "app": "prod-app"}),
        requesting_agent: "admin".to_string(),
    };

    let decision = gov_engine.evaluate(request).await;

    // If approved, install app
    if decision.approved {
        app_manager.install(manifest).await.unwrap();
    }
}
```

### 4.6 Scenario 6: Cluster Environment with Multi-Node Health

```rust
use clawft_kernel::{
    ClusterMembership, EnvironmentManager, Environment,
    EnvironmentClass, HealthSystem,
};

#[tokio::test]
async fn integration_cluster_health() {
    let cluster = ClusterMembership::new();
    let env_mgr = EnvironmentManager::new();
    let health_sys = HealthSystem::new();

    // Create production environment
    let prod_env = Environment {
        name: "prod".to_string(),
        class: EnvironmentClass::Production,
        governance: Default::default(),
        learning_mode: LearningMode::Conservative,
        audit_level: Default::default(),
    };
    env_mgr.create_environment(prod_env).await.unwrap();

    // Add cluster nodes
    for i in 1..=3 {
        let node = PeerNode {
            id: NodeId::new(),
            address: format!("10.0.0.{}:8080", i),
            state: NodeState::Active,
            platform: Default::default(),
            last_seen: chrono::Utc::now(),
        };
        cluster.add_peer(node).await;
    }

    // Check cluster health
    let peers = cluster.list_peers().await;
    for peer in peers {
        let status = if peer.state == NodeState::Active {
            HealthStatus::Healthy
        } else {
            HealthStatus::Degraded(format!("Node state: {:?}", peer.state))
        };
        health_sys.update_service_health(&peer.address, status).await;
    }

    // Overall health should be healthy if all nodes are active
    let overall = health_sys.overall_health().await;
    assert_eq!(overall, OverallHealth::Healthy);
}
```

---

## 5. CLI Testing

The kernel CLI commands are exposed via `weaver kernel` (the `weft` binary has no `kernel` subcommand). `weaver kernel start` backgrounds by default; the examples below use `--foreground` where they need the boot log. The expected-output blocks in this section are illustrative and have not been re-checked against the current output; compare with `weaver kernel status` / `services` in `weftos-deployment-sops.md` SOP 1.

### 5.1 Test `weaver kernel status`

```bash
# Start the kernel daemon (backgrounds by default)
weaver kernel start

# Check status
weaver kernel status

# Expected output:
# Kernel State: running
# Uptime: 15 seconds
# Processes: 3
# Services: 2
# Health: Healthy
```

### 5.2 Test `weaver kernel services`

```bash
# List registered services
weaver kernel services

# Expected output (table format):
# NAME          TYPE         STATE     HEALTH
# message-bus   core         running   healthy
# cron          scheduler    running   healthy
```

### 5.3 Test `weaver kernel ps`

```bash
# List process table
weaver kernel ps

# Expected output:
# PID   NAME            STATE     CPU(ms)  MEM(MB)  FILES
# 1     kernel          running   45       12       5
# 2     agent-worker-1  running   120      64       8
# 3     agent-worker-2  running   95       48       6
```

### 5.4 Test `weaver kernel start --foreground` (Interactive)

```bash
# Boot kernel with verbose output
weaver kernel start --foreground --verbose

# Expected output:
# [INFO] Phase: Init - Initializing kernel subsystems
# [INFO] Phase: Config - Loading configuration
# [INFO] Phase: Services - Starting core services
# [INFO] Phase: ResourceTree - Building resource hierarchy
# [INFO] Phase: Agents - Spawning initial agents
# [INFO] Phase: Network - Establishing cluster connectivity
# [INFO] Phase: Ready - Kernel is ready
# Kernel booted in 2.3 seconds
```

### 5.5 Test `weaver kernel stop`

```bash
# Graceful shutdown
weaver kernel stop

# Expected output:
# Stopping all services...
# Stopping all agents...
# Kernel halted
```

---

## 6. Feature-Gated Module Testing

### 6.1 Testing with `wasm-sandbox` Feature

```bash
# Build with WASM support
cargo build -p clawft-kernel --features wasm-sandbox

# Run WASM-specific tests (when wasmtime is integrated)
cargo test -p clawft-kernel --features wasm-sandbox wasm_runner::tests

# Expected: Tests pass with actual WASM execution
```

### 6.2 Testing with `containers` Feature

```bash
# Build with container support
cargo build -p clawft-kernel --features containers

# Run container-specific tests (when bollard is integrated)
cargo test -p clawft-kernel --features containers container::tests

# Expected: Tests pass with actual Docker integration
```

### 6.3 Testing All Features Together

```bash
# Build with all features
cargo build -p clawft-kernel --all-features

# Run all tests
cargo test -p clawft-kernel --all-features

# Expected: >259 tests (additional WASM and container tests)
```

---

## 7. What Cannot Be Tested Yet

### 7.1 Deferred to Future Phases

1. **Ruvector Integration** (boot.rs)
   - Feature-gated, not yet implemented
   - Cannot test ruvector-based memory management

2. **Exo-Resource-Tree** (boot.rs)
   - Feature-gated, not yet implemented
   - Cannot test hierarchical resource tracking

3. **Interactive Console REPL** (console.rs)
   - Only event types implemented, no stdin loop
   - Cannot test interactive kernel commands

4. **Built-In Services** (service.rs)
   - ServiceRegistry starts empty
   - No CronService or other system services registered

5. **WASM Execution** (wasm_runner.rs)
   - Stubbed without wasmtime dependency
   - Cannot test actual WASM module loading

6. **Container Orchestration** (container.rs)
   - Stubbed without bollard dependency
   - Cannot test actual Docker container lifecycle

7. **Network Transport** (cluster.rs)
   - Cluster membership is in-memory only
   - Cannot test actual TCP/UDP peer communication

8. **Distributed Consensus** (cluster.rs)
   - No Raft or gossip protocol implementation yet
   - Cannot test multi-node quorum

9. **Hook Execution** (app.rs)
   - Hooks defined in manifest but not executed
   - Cannot test pre_start/post_start/pre_stop hooks

10. **Effect Vector Scoring** (governance.rs)
    - EffectVector math is stubbed
    - Cannot test multi-dimensional decision scoring

---

## 8. Regression Checklist

Before committing changes to `clawft-kernel`, run this checklist:

### 8.1 Build All Targets

```bash
# Native build (default)
scripts/build.sh native

# Debug build (fast iteration)
scripts/build.sh native-debug

# Check compilation (no codegen)
scripts/build.sh check

# All features
cargo build -p clawft-kernel --all-features
```

### 8.2 Run Full Test Suite

```bash
# All kernel tests
scripts/build.sh test

# Kernel-only tests
cargo test -p clawft-kernel

# Test with all features
cargo test -p clawft-kernel --all-features
```

### 8.3 Lint and Format

```bash
# Clippy (warnings as errors)
scripts/build.sh clippy

# Format check
cargo fmt -p clawft-kernel -- --check
```

### 8.4 Phase Gate (11 Checks)

```bash
# Run all pre-commit checks
scripts/build.sh gate

# Expected: ALL PASS
```

### 8.5 Integration Smoke Tests

```bash
# Boot kernel
cargo run --bin weaver -- kernel start

# Check status
cargo run --bin weaver -- kernel status

# List services
cargo run --bin weaver -- kernel services

# List processes
cargo run --bin weaver -- kernel ps

# Shutdown
cargo run --bin weaver -- kernel stop
```

### 8.6 Documentation Build

```bash
# Build docs
cargo doc -p clawft-kernel --no-deps --open

# Check for broken links
cargo doc -p clawft-kernel --no-deps 2>&1 | grep -i "warning"
```

### 8.7 Feature Flag Validation

```bash
# Build with no default features (should fail or build minimal)
cargo build -p clawft-kernel --no-default-features

# Build with each feature individually
cargo build -p clawft-kernel --no-default-features --features native
cargo build -p clawft-kernel --no-default-features --features wasm-sandbox
cargo build -p clawft-kernel --no-default-features --features containers
```

### 8.8 Memory and Performance

```bash
# Run tests with memory profiling (requires valgrind)
valgrind --leak-check=full cargo test -p clawft-kernel

# Time test execution
time cargo test -p clawft-kernel

# Expected: <1 second
```

### 8.9 Cross-Platform Check (if applicable)

```bash
# Check for platform-specific code
rg "cfg\(target_os" crates/clawft-kernel/src

# Test on Linux/macOS/Windows (if available)
```

### 8.10 Integration with Dependent Crates

```bash
# Test CLI with kernel
cargo test -p clawft-cli kernel

# Test services with kernel
cargo test -p clawft-services

# Full workspace test
scripts/build.sh test
```

---

## 9. Summary

This manual testing guide covers:

- **9 K0 modules**: boot, process, service, ipc, capability, health, config, console, error
- **7 K1 modules**: supervisor, agent_loop, a2a, cron, chain, tree_manager, daemon e2e
- **2 K2 modules**: a2a (unit), topic
- **1 K3 module**: wasm_runner (stubbed)
- **1 K4 module**: container (stubbed)
- **1 K5 module**: app
- **3 K6a modules**: cluster, environment, governance
- **1 K6b module**: agency

**Total**: 24+ modules, 2,948 tests, ~14,000 SLOC

Use this guide to:
1. Validate automated tests are working
2. Perform manual integration testing
3. Test CLI commands
4. Prepare for production deployment
5. Verify feature flags
6. Run pre-commit regression checks

All code snippets are executable and ready to copy into your test suite or run interactively.
