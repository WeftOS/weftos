# WeftOS Kernel - Deferred Work Requirements

**Document Version**: 1.0
**Last Updated**: 2026-03-01 (section 6 corrected 2026-10-27; the other sections have not been re-verified against the code)
**Status**: Active Development

## Overview

This document tracks all intentionally deferred features from the WeftOS kernel sprint. Each item includes full implementation requirements, integration steps, and verification procedures. Items are prioritized by dependency chain and impact.

---

## 1. Wasmtime Integration (K3 - wasm_runner.rs)

### Status
**Stubbed** - Types and validation logic exist, execution is mocked.

### Priority
**HIGH** - Core functionality for tool execution sandbox.

### Code References
- `crates/clawft-kernel/src/k3_tools/wasm_runner.rs:299` - validate with wasmtime
- `crates/clawft-kernel/src/k3_tools/wasm_runner.rs:340` - compile module
- `crates/clawft-kernel/src/k3_tools/wasm_runner.rs:368` - run with fuel metering

### Dependencies
```toml
# Add to clawft-kernel/Cargo.toml
[dependencies]
wasmtime = { version = "27.0", optional = true }

[features]
wasm-sandbox = ["wasmtime"]
```

### Effort Estimate
**Medium** (4-8 hours)

### What's Needed

1. **Wasmtime Engine Setup**
   - Configure `Engine` with fuel metering enabled
   - Set memory limits and table limits
   - Enable WASI preview 1 support

2. **Module Compilation**
   - Implement `load_tool()` to compile WASM bytes to `Module`
   - Cache compiled modules by content hash
   - Validate module exports match tool manifest

3. **Execution Runtime**
   - Create `Store` with fuel limit from config
   - Instantiate module with WASI context
   - Handle memory limits and trap recovery
   - Extract return values from linear memory

### Integration Steps

```rust
// 1. Add wasmtime imports
#[cfg(feature = "wasm-sandbox")]
use wasmtime::*;
#[cfg(feature = "wasm-sandbox")]
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder};

// 2. Implement real load_tool()
#[cfg(feature = "wasm-sandbox")]
fn load_tool(&self, bytes: &[u8]) -> Result<(), WasmError> {
    let engine = Engine::new(&self.config)?;
    let module = Module::new(&engine, bytes)?;

    // Validate exports
    for export in module.exports() {
        if export.name() == "execute" {
            // Verify function signature
        }
    }

    Ok(())
}

// 3. Implement execute_tool()
#[cfg(feature = "wasm-sandbox")]
fn execute_tool(&self, tool_id: &str, input: &[u8]) -> Result<Vec<u8>, WasmError> {
    let engine = Engine::new(&self.config)?;
    let module = self.modules.get(tool_id)?;

    let wasi = WasiCtxBuilder::new()
        .inherit_stdio()
        .build();

    let mut store = Store::new(&engine, wasi);
    store.set_fuel(self.config.max_fuel)?;

    let instance = Instance::new(&mut store, &module, &[])?;
    let execute = instance.get_typed_func::<(u32, u32), u32>(&mut store, "execute")?;

    // Copy input to linear memory, call, extract output
    let result = execute.call(&mut store, (input_ptr, input_len))?;

    Ok(output_bytes)
}
```

### Verification

```bash
# 1. Build with feature
cargo build -p clawft-kernel --features wasm-sandbox

# 2. Run unit tests
cargo test -p clawft-kernel --features wasm-sandbox test_wasm_runner

# 3. Integration test with real WASM tool
cargo test -p clawft-kernel --features wasm-sandbox test_execute_hello_world_wasm

# 4. Verify fuel metering prevents infinite loops
cargo test -p clawft-kernel --features wasm-sandbox test_fuel_exhaustion
```

### SPARC Reference
- **K3 Specification**: `.planning/sparc/kernel/03-k3-tools.md`
- **WASM Tool Format**: Lines 200-250

---

## 2. Bollard/Docker Integration (K4 - container.rs)

### Status
**Stubbed** - State machine exists, Docker API calls are mocked.

### Priority
**HIGH** - Required for containerized tool execution.

### Code References
- `crates/clawft-kernel/src/k4_containers/container.rs:316` - bollard integration

### Dependencies
```toml
# Add to clawft-kernel/Cargo.toml
[dependencies]
bollard = { version = "0.17", optional = true }

[features]
containers = ["bollard"]
```

**System Requirements**:
- Docker daemon running locally
- User in `docker` group (Linux) or Docker Desktop (macOS/Windows)

### Effort Estimate
**Medium-Large** (8-12 hours)

### What's Needed

1. **Docker Client Setup**
   - Connect to local Docker daemon via Unix socket or TCP
   - Handle connection errors gracefully
   - Auto-detect Docker availability

2. **Container Lifecycle**
   - `start_container()`: Create and start container from image
   - `stop_container()`: Graceful shutdown with timeout, then force kill
   - `remove_container()`: Clean up stopped containers
   - `health_check()`: Poll container health status

3. **Resource Management**
   - Apply CPU/memory limits from ContainerConfig
   - Mount volumes for input/output
   - Network isolation
   - Log collection

### Integration Steps

```rust
// 1. Add bollard imports
#[cfg(feature = "containers")]
use bollard::Docker;
#[cfg(feature = "containers")]
use bollard::container::{Config, CreateContainerOptions, StartContainerOptions};
#[cfg(feature = "containers")]
use bollard::models::{HostConfig, ResourcesUlimits};

// 2. Add Docker client to ContainerManager
#[cfg(feature = "containers")]
pub struct ContainerManager {
    docker: Docker,
    containers: HashMap<String, ContainerHandle>,
}

// 3. Implement real start_container()
#[cfg(feature = "containers")]
async fn start_container(&mut self, id: &str, config: &ContainerConfig) -> Result<()> {
    let docker_config = Config {
        image: Some(config.image.clone()),
        host_config: Some(HostConfig {
            memory: Some(config.memory_limit as i64),
            nano_cpus: Some((config.cpu_limit * 1_000_000_000.0) as i64),
            ..Default::default()
        }),
        ..Default::default()
    };

    let create_options = CreateContainerOptions {
        name: id,
        platform: None,
    };

    let container = self.docker.create_container(Some(create_options), docker_config).await?;
    self.docker.start_container(&container.id, None::<StartContainerOptions<String>>).await?;

    Ok(())
}

// 4. Implement health_check()
#[cfg(feature = "containers")]
async fn health_check(&self, id: &str) -> Result<ContainerHealth> {
    let inspect = self.docker.inspect_container(id, None).await?;

    match inspect.state {
        Some(state) if state.running == Some(true) => Ok(ContainerHealth::Healthy),
        Some(state) if state.restarting == Some(true) => Ok(ContainerHealth::Unhealthy),
        _ => Ok(ContainerHealth::Stopped),
    }
}
```

### Verification

```bash
# 1. Ensure Docker is running
docker ps

# 2. Build with feature
cargo build -p clawft-kernel --features containers

# 3. Run unit tests (requires Docker)
cargo test -p clawft-kernel --features containers test_container_manager

# 4. Integration test with real container
cargo test -p clawft-kernel --features containers test_start_alpine_container

# 5. Verify resource limits are applied
cargo test -p clawft-kernel --features containers test_memory_limit_enforced
```

### SPARC Reference
- **K4 Specification**: `.planning/sparc/kernel/04-k4-containers.md`
- **Container Lifecycle**: Lines 150-200

---

## 3. TOML Manifest Parsing (K5 - app.rs)

### Status
**Partially Done** - Types defined, file loading not wired.

### Priority
**MEDIUM** - Nice-to-have for app deployment.

### Code References
- `crates/clawft-kernel/src/k5_apps/app.rs` - AppManifest struct
- Tests construct manifests programmatically instead of loading from files

### Dependencies
```toml
# Add to clawft-kernel/Cargo.toml
[dependencies]
toml = "0.8"
```

### Effort Estimate
**Small** (2-4 hours)

### What's Needed

1. **File Loading**
   - `AppManifest::from_file(path: &Path)` method
   - Error handling for missing/invalid files
   - Path resolution relative to app root

2. **Validation**
   - Ensure required fields are present
   - Validate version strings
   - Check tool references exist

3. **Example Manifests**
   - Create `examples/weftapp.toml` templates
   - Document all available fields

### Integration Steps

```rust
// 1. Add to app.rs
impl AppManifest {
    /// Load and parse a weftapp.toml manifest file
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, AppError> {
        let path = path.as_ref();
        let content = std::fs::read_to_string(path)
            .map_err(|e| AppError::ManifestNotFound(path.display().to_string(), e))?;

        let manifest: AppManifest = toml::from_str(&content)
            .map_err(|e| AppError::InvalidManifest(e.to_string()))?;

        manifest.validate()?;
        Ok(manifest)
    }

    fn validate(&self) -> Result<(), AppError> {
        if self.name.is_empty() {
            return Err(AppError::InvalidManifest("name required".into()));
        }
        // Additional validation...
        Ok(())
    }
}

// 2. Update AppManager::install_app()
pub async fn install_app(&mut self, path: &Path) -> Result<String, AppError> {
    let manifest_path = path.join("weftapp.toml");
    let manifest = AppManifest::from_file(manifest_path)?;
    // Rest of installation logic...
}
```

### Example weftapp.toml

```toml
# Create at examples/basic-app/weftapp.toml
name = "my-agent"
version = "1.0.0"
description = "Example WeftOS application"

[dependencies]
tools = ["http-client", "json-parser"]
apps = []

[resources]
memory_mb = 512
cpu_cores = 1.0

[[endpoints]]
name = "process"
description = "Process input data"
input_schema = "input.json"
output_schema = "output.json"
```

### Verification

```bash
# 1. Create test manifest
mkdir -p /tmp/test-app
cat > /tmp/test-app/weftapp.toml << 'EOF'
name = "test"
version = "1.0.0"
description = "Test app"
EOF

# 2. Run parsing test
cargo test -p clawft-kernel test_manifest_from_file

# 3. Verify validation catches errors
cargo test -p clawft-kernel test_manifest_validation_errors

# 4. Integration test with app installation
cargo test -p clawft-kernel test_install_app_from_directory
```

### SPARC Reference
- **K5 Specification**: `.planning/sparc/kernel/05-k5-apps.md`
- **Manifest Format**: Lines 100-150

---

## 4. Ruvector Crate Integration (All Phases)

### Status
**Not Started** - Crates not published or vendored.

### Priority
**LOW-MEDIUM** - Optional advanced features.

### Code References
- `crates/clawft-kernel/src/k0_boot/boot.rs:156`
- SPARC docs 01-06 mention ruvector integration points

### Dependencies

**External Crates** (not on crates.io):
```toml
# Option A: Git dependencies
[dependencies]
rvf-sona = { git = "https://github.com/ruvnet/ruvector", optional = true }
tiny-dancer = { git = "https://github.com/ruvnet/ruvector", optional = true }
prime-radiant = { git = "https://github.com/ruvnet/ruvector", optional = true }
rvf-wire = { git = "https://github.com/ruvnet/ruvector", optional = true }
rvf-kernel = { git = "https://github.com/ruvnet/ruvector", optional = true }

[features]
ruvector-cluster = ["rvf-sona", "tiny-dancer"]
ruvector-crypto = ["prime-radiant"]
ruvector-containers = ["rvf-kernel"]
ruvector-apps = ["rvf-wire"]
```

**Repositories**:
- https://github.com/ruvnet/ruvector
- https://github.com/ruvnet/agentic-flow
- https://github.com/ruvnet/ruflo

### Effort Estimate
**Large** (20-40 hours) - Depends on ruvector crate maturity.

### What's Needed

1. **Publish or Vendor Ruvector Crates**
   - Contact ruvector maintainers to publish to crates.io, OR
   - Add as git dependencies with pinned versions, OR
   - Vendor source into `vendor/ruvector/` with proper licensing

2. **Cluster Integration (rvf-sona, tiny-dancer)**
   - Distributed consensus for multi-node kernels
   - Shared memory across cluster
   - Leader election and failover

3. **Cryptographic Primitives (prime-radiant)**
   - Content addressing with BLAKE3
   - Zero-knowledge proofs for capability delegation
   - Encrypted storage backends

4. **Container Orchestration (rvf-kernel)**
   - Advanced scheduling policies
   - Auto-scaling based on load
   - Health monitoring and recovery

5. **Application Wire Protocol (rvf-wire)**
   - Binary serialization for app IPC
   - Schema evolution and versioning
   - Compression and streaming

### Integration Steps

```bash
# 1. Check if ruvector crates are published
cargo search rvf-sona
cargo search tiny-dancer
# If not found, proceed with git dependencies

# 2. Add git dependencies to clawft-kernel/Cargo.toml
# (see Dependencies section above)

# 3. Create feature-gated adapter modules
mkdir -p crates/clawft-kernel/src/adapters/ruvector/

# 4. Implement adapters per SPARC doc 07
# crates/clawft-kernel/src/adapters/ruvector/cluster.rs
# crates/clawft-kernel/src/adapters/ruvector/crypto.rs
# crates/clawft-kernel/src/adapters/ruvector/containers.rs
# crates/clawft-kernel/src/adapters/ruvector/wire.rs

# 5. Wire adapters into boot sequence
# crates/clawft-kernel/src/k0_boot/boot.rs
```

Example adapter:

```rust
// crates/clawft-kernel/src/adapters/ruvector/cluster.rs
#[cfg(feature = "ruvector-cluster")]
use rvf_sona::Cluster;

#[cfg(feature = "ruvector-cluster")]
pub struct RuvectorClusterAdapter {
    cluster: Cluster,
}

#[cfg(feature = "ruvector-cluster")]
impl RuvectorClusterAdapter {
    pub async fn join(&mut self, peers: Vec<String>) -> Result<()> {
        self.cluster.join(peers).await?;
        Ok(())
    }

    pub async fn consensus(&self, key: &str, value: Vec<u8>) -> Result<()> {
        self.cluster.propose(key, value).await?;
        Ok(())
    }
}
```

### Verification

```bash
# 1. Verify crates build
cargo build -p clawft-kernel --features ruvector-cluster
cargo build -p clawft-kernel --features ruvector-crypto

# 2. Run adapter tests
cargo test -p clawft-kernel --features ruvector-cluster test_cluster_adapter
cargo test -p clawft-kernel --features ruvector-crypto test_crypto_adapter

# 3. Integration test with real cluster
cargo test -p clawft-kernel --all-features test_multinode_consensus -- --ignored
```

### SPARC Reference
- **Ruvector Integration**: `.planning/sparc/kernel/07-ruvector-deep-integration.md`
- **K6 Networking**: `.planning/sparc/kernel/06-k6-network.md` (cluster membership)

---

## 5. ExoChain / exo-resource-tree (K0+, Doc 13)

### Status
**Not Started** - Requires external ExoChain crates.

### Priority
**LOW** - Advanced capability system, not required for MVP.

### Code References
- `crates/clawft-kernel/src/k0_boot/boot.rs:159`
- SPARC doc 13 (full ExoChain specification)

### Dependencies

**External Crates** (not on crates.io):
```toml
[dependencies]
exo-core = { git = "https://github.com/ruvnet/exochain", optional = true }
exo-identity = { git = "https://github.com/ruvnet/exochain", optional = true }
exo-consent = { git = "https://github.com/ruvnet/exochain", optional = true }
exo-dag = { git = "https://github.com/ruvnet/exochain", optional = true }

[features]
exochain = ["exo-core", "exo-identity", "exo-consent", "exo-dag"]
```

**Repository**:
- https://github.com/ruvnet/exochain

### Effort Estimate
**Very Large** (40-80 hours) - Complex system requiring new crate.

### What's Needed

1. **Create `crates/exo-resource-tree/` Crate**
   - New workspace member
   - Hierarchical resource tree data structure
   - Content-addressed node storage
   - Capability-based access control

2. **Core Concepts**
   - **Resource Node**: Kernel object mapped to CID
   - **Resource Path**: `/kernel/process/123`, `/kernel/ipc/channel/456`
   - **Capability Token**: Transferable access rights
   - **DAG Structure**: Merkle DAG for versioning and integrity

3. **Integration Points**
   - Boot sequence: Initialize resource tree
   - Process creation: Add process nodes
   - IPC: Capability delegation via resource references
   - Persistence: Store tree to exo-dag

### Integration Steps

```bash
# 1. Create new crate
mkdir -p crates/exo-resource-tree/src
cargo init --lib crates/exo-resource-tree

# 2. Add to workspace
# Edit Cargo.toml workspace.members

# 3. Implement resource tree
cat > crates/exo-resource-tree/src/lib.rs << 'EOF'
use exo_dag::{Cid, IpldDag};
use std::collections::HashMap;

pub struct ResourceTree {
    root: Cid,
    nodes: HashMap<String, ResourceNode>,
    dag: IpldDag,
}

pub struct ResourceNode {
    id: Cid,
    path: String,
    resource_type: ResourceType,
    capabilities: Vec<Capability>,
    children: Vec<Cid>,
}

pub enum ResourceType {
    Process,
    IpcChannel,
    Tool,
    Container,
    App,
}

pub struct Capability {
    id: Cid,
    holder: String, // DID
    rights: Rights,
}

impl ResourceTree {
    pub fn new() -> Self {
        // Initialize tree with root node
    }

    pub fn add_resource(&mut self, path: &str, rtype: ResourceType) -> Cid {
        // Add node to tree and DAG
    }

    pub fn grant_capability(&mut self, resource: &Cid, holder: &str, rights: Rights) -> Capability {
        // Create transferable capability token
    }

    pub fn verify_access(&self, resource: &Cid, holder: &str, required: Rights) -> bool {
        // Check if holder has required rights
    }
}
EOF

# 4. Wire into kernel boot
# Add to k0_boot/boot.rs initialization
```

### Verification

```bash
# 1. Build new crate
cargo build -p exo-resource-tree --features exochain

# 2. Unit tests for resource tree
cargo test -p exo-resource-tree

# 3. Integration with kernel
cargo test -p clawft-kernel --features exochain test_resource_tree_boot

# 4. Verify capability delegation
cargo test -p exo-resource-tree test_capability_transfer
```

### SPARC Reference
- **ExoChain Specification**: `.planning/sparc/kernel/13-exochain-resource-tree.md`
- **K0 Boot Integration**: `.planning/sparc/kernel/00-k0-boot.md` (lines 300-350)

---

## 6. Separate OS CLI (K0 - Done, shipped as `weaver`)

### Status
**Done.** The OS-side CLI is the `weaver` binary in `crates/clawft-weave`
(`[[bin]] name = "weaver"`). The agent-side CLI is `weft` in
`crates/clawft-cli`. No `weave` binary exists and `weft` has no `kernel`
subcommand: kernel, mesh, cluster, chain, workload and the rest of the OS
surface are `weaver` commands. See [the CLI reference](../reference/cli.md)
for the generated command list.

The original plan (a `weave` binary, with the `kernel` command kept on `weft` as a
deprecated alias) was not carried out as written. This document still
says `weave` in a few later places (sections 7 and 11); read those as
`weaver`.

---

## 7. Interactive Console REPL (K0 - console.rs)

### Status
**Partially Done** - Event types and formatting exist, REPL not implemented.

### Priority
**MEDIUM** - Nice-to-have for kernel debugging.

### Code References
- `crates/clawft-kernel/src/k0_boot/console.rs`
- Boot events and output are defined but no stdin loop

### Dependencies
```toml
# Add to clawft-kernel/Cargo.toml
[dependencies]
rustyline = "14.0"
```

### Effort Estimate
**Medium** (6-8 hours)

### What's Needed

1. **REPL Loop**
   - Readline with history
   - Command parsing and dispatch
   - Tab completion for commands
   - Ctrl+C handling

2. **Commands**
   - `status` - Show kernel status
   - `ps` - List processes
   - `kill <pid>` - Terminate process
   - `tools` - List loaded tools
   - `mem` - Memory stats
   - `help` - Show commands
   - `exit` - Shutdown kernel

3. **Integration with Kernel**
   - Console should have handle to running kernel
   - Commands send messages to kernel via channels
   - Console displays events from kernel event stream

### Integration Steps

```rust
// 1. Add to console.rs
use rustyline::error::ReadlineError;
use rustyline::{DefaultEditor, Result as RlResult};

pub struct KernelConsole {
    editor: DefaultEditor,
    kernel_tx: mpsc::Sender<ConsoleCommand>,
    event_rx: mpsc::Receiver<BootEvent>,
}

impl KernelConsole {
    pub fn new(kernel_tx: mpsc::Sender<ConsoleCommand>) -> Self {
        let editor = DefaultEditor::new().expect("Failed to create readline editor");
        // Setup channels...
        Self { editor, kernel_tx, event_rx }
    }

    pub async fn run_repl(&mut self) -> Result<()> {
        println!("WeftOS Kernel Console v{}", env!("CARGO_PKG_VERSION"));
        println!("Type 'help' for commands, 'exit' to quit.\n");

        loop {
            match self.editor.readline("weft> ") {
                Ok(line) => {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }

                    self.editor.add_history_entry(line)?;

                    match self.parse_command(line) {
                        Some(cmd) => {
                            if matches!(cmd, ConsoleCommand::Exit) {
                                break;
                            }
                            self.kernel_tx.send(cmd).await?;
                        }
                        None => {
                            eprintln!("Unknown command: {}. Type 'help'.", line);
                        }
                    }
                }
                Err(ReadlineError::Interrupted) => {
                    println!("^C (Ctrl+D to exit)");
                }
                Err(ReadlineError::Eof) => {
                    break;
                }
                Err(err) => {
                    eprintln!("Readline error: {}", err);
                    break;
                }
            }

            // Poll for kernel events
            while let Ok(event) = self.event_rx.try_recv() {
                self.display_event(&event);
            }
        }

        Ok(())
    }

    fn parse_command(&self, line: &str) -> Option<ConsoleCommand> {
        let parts: Vec<&str> = line.split_whitespace().collect();
        match parts.get(0)? {
            "status" => Some(ConsoleCommand::Status),
            "ps" => Some(ConsoleCommand::ListProcesses),
            "kill" => Some(ConsoleCommand::KillProcess(parts.get(1)?.parse().ok()?)),
            "tools" => Some(ConsoleCommand::ListTools),
            "mem" => Some(ConsoleCommand::MemoryStats),
            "help" => Some(ConsoleCommand::Help),
            "exit" => Some(ConsoleCommand::Exit),
            _ => None,
        }
    }
}

// 2. Add console command type
pub enum ConsoleCommand {
    Status,
    ListProcesses,
    KillProcess(u32),
    ListTools,
    MemoryStats,
    Help,
    Exit,
}

// 3. Wire into CLI
// crates/clawft-cli/src/commands/weave_commands.rs
pub async fn run_console() -> Result<()> {
    let (kernel_tx, kernel_rx) = mpsc::channel(100);

    // Boot kernel in background
    let kernel_handle = tokio::spawn(async move {
        let mut kernel = WeftKernel::new().await?;
        kernel.run_with_console(kernel_rx).await
    });

    // Run console REPL
    let mut console = KernelConsole::new(kernel_tx);
    console.run_repl().await?;

    kernel_handle.await??;
    Ok(())
}
```

### Verification

```bash
# 1. Build with rustyline
cargo build -p clawft-kernel

# 2. Run console
weave console
# Should see: "WeftOS Kernel Console v0.1.0"
# Should see: "weft> " prompt

# 3. Test commands
weft> status
weft> ps
weft> tools
weft> help
weft> exit

# 4. Test readline features
# - Arrow keys for history
# - Ctrl+C handling
# - Ctrl+D to exit

# 5. Integration test
cargo test -p clawft-kernel test_console_repl -- --ignored
```

### SPARC Reference
- **K0 Console**: `.planning/sparc/kernel/00-k0-boot.md` (lines 250-300)

---

## 8. Cross-Node IPC / Networking (K6, Doc 12)

### Status
**Stubbed** - Cluster membership types exist, transport not implemented.

### Priority
**LOW** - Advanced multi-node feature.

### Code References
- `crates/clawft-kernel/src/k6_network/cluster.rs`

### Dependencies
```toml
# Add to clawft-kernel/Cargo.toml
[dependencies]
libp2p = { version = "0.54", optional = true }
tokio-tungstenite = { version = "0.24", optional = true }

[features]
networking = ["libp2p", "tokio-tungstenite"]
```

### Effort Estimate
**Very Large** (40-60 hours)

### What's Needed

1. **Peer Discovery**
   - mDNS for local network discovery
   - Bootstrap nodes for public internet
   - DID-based addressing

2. **Transport Layers**
   - libp2p for native nodes
   - WebSocket for browser nodes
   - NAT traversal (STUN/TURN)

3. **Message Routing**
   - Process-to-process IPC across nodes
   - Capability token verification
   - Encryption and authentication

4. **Consistency**
   - Distributed consensus (Raft or Paxos)
   - Event ordering
   - Conflict resolution

### Integration Steps

```bash
# 1. Create networking module
mkdir -p crates/clawft-kernel/src/k6_network/transport/

# 2. Implement libp2p transport
# crates/clawft-kernel/src/k6_network/transport/libp2p.rs

# 3. Implement WebSocket transport
# crates/clawft-kernel/src/k6_network/transport/websocket.rs

# 4. Implement peer discovery
# crates/clawft-kernel/src/k6_network/discovery.rs

# 5. Wire into K2 IPC
# Update k2_ipc/channel.rs to support remote channels
```

Example transport:

```rust
// crates/clawft-kernel/src/k6_network/transport/libp2p.rs
#[cfg(feature = "networking")]
use libp2p::{Swarm, PeerId, Multiaddr};

#[cfg(feature = "networking")]
pub struct LibP2PTransport {
    swarm: Swarm<Behaviour>,
    local_peer_id: PeerId,
}

#[cfg(feature = "networking")]
impl LibP2PTransport {
    pub async fn new() -> Result<Self> {
        let local_key = identity::Keypair::generate_ed25519();
        let local_peer_id = PeerId::from(local_key.public());

        let transport = libp2p::tcp::tokio::Transport::default();
        let behaviour = Behaviour::new(local_peer_id);

        let swarm = Swarm::new(transport, behaviour, local_peer_id);

        Ok(Self { swarm, local_peer_id })
    }

    pub async fn dial(&mut self, peer: &Multiaddr) -> Result<()> {
        self.swarm.dial(peer.clone())?;
        Ok(())
    }

    pub async fn send_message(&mut self, peer: PeerId, msg: Vec<u8>) -> Result<()> {
        // Send message via libp2p
        Ok(())
    }
}
```

### Verification

```bash
# 1. Build with networking
cargo build -p clawft-kernel --features networking

# 2. Run local peer discovery test
cargo test -p clawft-kernel --features networking test_mdns_discovery

# 3. Run two-node IPC test
cargo test -p clawft-kernel --features networking test_cross_node_ipc -- --ignored

# 4. Browser-to-native WebSocket test
cargo test -p clawft-kernel --features networking test_browser_websocket -- --ignored
```

### SPARC Reference
- **K6 Networking**: `.planning/sparc/kernel/06-k6-network.md`
- **Cross-Node IPC**: `.planning/sparc/kernel/12-distributed-patterns.md`

---

## 9. Cryptographic Filesystem (K6, Doc 08)

### Status
**Not Started**

### Priority
**LOW** - Advanced storage feature.

### Code References
- SPARC doc 08 describes content-addressed storage

### Dependencies
```toml
[dependencies]
blake3 = "1.5"
exo-dag = { git = "https://github.com/ruvnet/exochain", optional = true }

[features]
crypto-fs = ["blake3", "exo-dag"]
```

### Effort Estimate
**Large** (30-50 hours)

### What's Needed

1. **Content Addressing**
   - BLAKE3 hashing for all content
   - CID-based file references
   - Deduplication

2. **Merkle DAG Storage**
   - exo-dag Merkle Mountain Range
   - Chunking for large files
   - Incremental updates

3. **Storage Backends**
   - Local filesystem backend
   - IPFS backend (optional)
   - S3-compatible backend (optional)

4. **Versioning**
   - Immutable snapshots
   - Time-travel queries
   - Garbage collection

### Integration Steps

```bash
# 1. Create crypto-fs module
mkdir -p crates/clawft-kernel/src/k6_network/crypto_fs/

# 2. Implement content store
# crates/clawft-kernel/src/k6_network/crypto_fs/store.rs

# 3. Implement chunking
# crates/clawft-kernel/src/k6_network/crypto_fs/chunker.rs

# 4. Create backends
# crates/clawft-kernel/src/k6_network/crypto_fs/backends/local.rs
# crates/clawft-kernel/src/k6_network/crypto_fs/backends/ipfs.rs
```

### Verification

```bash
cargo build -p clawft-kernel --features crypto-fs
cargo test -p clawft-kernel --features crypto-fs test_content_addressing
cargo test -p clawft-kernel --features crypto-fs test_merkle_dag_versioning
```

### SPARC Reference
- **Crypto Filesystem**: `.planning/sparc/kernel/08-crypto-filesystem.md`

---

## 10. Local Inference / GGUF Runtime (Doc 11)

### Status
**Not Started**

### Priority
**LOW** - Optional inference capability.

### Code References
- SPARC doc 11 describes 4-tier model routing

### Dependencies
```toml
[dependencies]
llama-cpp-sys = { version = "0.1", optional = true }
# OR
ruvllm = { git = "https://github.com/ruvnet/ruvector", optional = true }

[features]
local-inference = ["llama-cpp-sys"]
```

### Effort Estimate
**Large** (30-50 hours)

### What's Needed

1. **GGUF Model Loading**
   - llama.cpp bindings or ruvllm crate
   - Model file format detection
   - Quantization support (Q4_K_M, Q8_0, etc.)

2. **4-Tier Routing**
   - Tier 1: Agent Booster (WASM transforms) - <1ms
   - Tier 2: Local Haiku-equivalent - ~500ms
   - Tier 3: Local Sonnet-equivalent - 2-5s
   - Tier 4: Cloud API fallback

3. **Resource Management**
   - GPU offloading (CUDA/Metal)
   - Memory limits
   - Batching and queueing

### Integration Steps

```bash
# 1. Create inference module
mkdir -p crates/clawft-kernel/src/k3_tools/inference/

# 2. Implement GGUF loader
# crates/clawft-kernel/src/k3_tools/inference/gguf.rs

# 3. Implement routing logic
# crates/clawft-kernel/src/k3_tools/inference/router.rs

# 4. Wire into tool execution
# Update k3_tools to check for local model before calling API
```

### Verification

```bash
# Download test model
wget https://huggingface.co/.../model.gguf -O /tmp/test-model.gguf

cargo build -p clawft-kernel --features local-inference
cargo test -p clawft-kernel --features local-inference test_gguf_loader
cargo test -p clawft-kernel --features local-inference test_4tier_routing
```

### SPARC Reference
- **Local Inference**: `.planning/sparc/kernel/11-local-inference.md`

---

## 11. Dockerfile.alpine (K4)

### Status
**Not Started**

### Priority
**MEDIUM** - Useful for deployment.

### Code References
- SPARC K4 spec mentions containerized deployment

### Dependencies
- Docker
- Alpine Linux base image

### Effort Estimate
**Small** (2-4 hours)

### What's Needed

1. **Multi-Stage Dockerfile**
   - Build stage with Rust toolchain
   - Runtime stage with minimal Alpine
   - Static linking or musl target

2. **Image Optimization**
   - Small final image (<50MB)
   - Non-root user
   - Health check endpoint

3. **Docker Compose**
   - Example orchestration
   - Volume mounts for config
   - Logging configuration

### Integration Steps

```dockerfile
# Create crates/clawft-kernel/Dockerfile.alpine
FROM rust:1.83-alpine AS builder

RUN apk add --no-cache musl-dev openssl-dev

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
COPY scripts/build.sh scripts/

# Build static binary
RUN scripts/build.sh native --release --target x86_64-unknown-linux-musl

FROM alpine:3.20
RUN apk add --no-cache ca-certificates

RUN addgroup -g 1000 weft && adduser -D -u 1000 -G weft weft

COPY --from=builder /build/target/x86_64-unknown-linux-musl/release/weft /usr/local/bin/
COPY --from=builder /build/target/x86_64-unknown-linux-musl/release/weave /usr/local/bin/

USER weft
WORKDIR /home/weft

HEALTHCHECK --interval=30s --timeout=3s --start-period=5s \
  CMD weaver kernel status || exit 1

ENTRYPOINT ["/usr/local/bin/weave"]
CMD ["kernel", "boot"]
```

```yaml
# Create crates/clawft-kernel/docker-compose.yml
services:
  weft-kernel:
    build:
      context: ../..
      dockerfile: crates/clawft-kernel/Dockerfile.alpine
    container_name: weft-kernel
    volumes:
      - ./config:/home/weft/.config/weft
      - kernel-data:/home/weft/.local/share/weft
    ports:
      - "8080:8080"
    environment:
      - RUST_LOG=info
    restart: unless-stopped

volumes:
  kernel-data:
```

### Verification

```bash
# 1. Build image
cd crates/clawft-kernel
docker build -f Dockerfile.alpine -t weft-kernel:latest ../..

# 2. Check image size
docker images weft-kernel:latest
# Should be <50MB

# 3. Run container
docker run --rm weft-kernel:latest --version

# 4. Run with docker-compose
docker-compose up -d
docker-compose ps
docker-compose logs -f

# 5. Health check
docker inspect weft-kernel | jq '.[0].State.Health'
```

### SPARC Reference
- **K4 Containers**: `.planning/sparc/kernel/04-k4-containers.md` (lines 300-350)

---

## 12. Kernel Developer Guide (K5)

### Status
**Not Started**

### Priority
**MEDIUM** - Important for onboarding.

### Code References
- SPARC K5 spec calls for comprehensive guide

### Dependencies
None - documentation only.

### Effort Estimate
**Medium** (8-12 hours)

### What's Needed

1. **Architecture Overview**
   - K0-K6 layer diagram
   - Boot sequence flow
   - Component interaction

2. **Extending the Kernel**
   - Adding new K-layers
   - Writing custom services
   - Plugin system

3. **Writing Apps**
   - weftapp.toml format
   - App lifecycle hooks
   - Best practices

4. **Advanced Topics**
   - Multi-node deployment
   - Performance tuning
   - Security considerations

### Integration Steps

```bash
# 1. Create guide structure
cat > docs/guides/kernel.md << 'EOF'
# WeftOS Kernel Developer Guide

## Table of Contents
1. Architecture Overview
2. Boot Sequence
3. K-Layer Deep Dive
4. Extending the Kernel
5. Writing Applications
6. Multi-Node Deployment
7. Performance Optimization
8. Security Best Practices
9. Troubleshooting

## 1. Architecture Overview

WeftOS kernel is organized into 7 layered subsystems (K0-K6):

```
K0: Boot & Console    - System initialization, event logging
K1: Process Manager   - Process lifecycle, scheduling
K2: IPC Channels      - Inter-process communication
K3: Tools Registry    - Tool execution (native, WASM, API)
K4: Containers        - Docker container orchestration
K5: App Manager       - Application deployment and lifecycle
K6: Network & Cluster - Multi-node coordination
```

[... continue with detailed sections ...]
EOF

# 2. Add diagrams
# Create docs/guides/kernel-diagrams/ with Mermaid diagrams

# 3. Add code examples
mkdir -p docs/guides/kernel-examples/
# Add example apps, tools, extensions
```

### Verification

```bash
# 1. Render markdown locally
mdbook build docs/

# 2. Check links
markdown-link-check docs/guides/kernel.md

# 3. Validate code examples compile
cd docs/guides/kernel-examples/custom-tool/
cargo build

# 4. Review for completeness
# All K-layers documented
# All extension points explained
# All APIs documented
```

### SPARC Reference
- **K5 Apps**: `.planning/sparc/kernel/05-k5-apps.md` (guide requirements)
- **All K-layers**: `.planning/sparc/kernel/*.md`

---

## Priority Matrix

| Priority | Items | Effort | Dependencies |
|----------|-------|--------|--------------|
| **HIGH** | 1, 2 | 12-20h | None |
| **MEDIUM** | 3, 6, 7, 11, 12 | 22-38h | Items 1,2 |
| **LOW** | 4, 5, 8, 9, 10 | 130-240h | External crates |

## Recommended Implementation Order

### Phase 1: Core Execution (Items 1, 2, 3)
Complete WASM and container execution to enable real tool/app running.

**Effort**: ~20 hours
**Deliverable**: Functional K3 tools and K4 containers

### Phase 2: UX & Documentation (Items 6, 7, 12)
Improve developer experience with split CLIs, console, and guide.

**Effort**: ~20 hours
**Deliverable**: `weave` binary, REPL console, kernel guide

### Phase 3: Deployment (Item 11)
Containerized deployment for production use.

**Effort**: ~4 hours
**Deliverable**: Dockerfile.alpine and docker-compose.yml

### Phase 4: Advanced Features (Items 4, 5, 8, 9, 10)
Optional features requiring external dependencies. Tackle as needed.

**Effort**: ~200 hours (staggered)
**Deliverable**: Ruvector integration, ExoChain, networking, crypto-fs, inference

---

## Testing Requirements

All deferred items MUST pass these checks before merge:

1. **Unit Tests**: `cargo test -p clawft-kernel`
2. **Feature Gates**: `cargo build --features <feature>` succeeds
3. **No Regressions**: `scripts/build.sh gate` passes
4. **Documentation**: Updated in relevant docs/guides/*.md
5. **Examples**: Working code examples provided

---

## Getting Help

- **SPARC Docs**: `.planning/sparc/kernel/*.md` - Full specifications
- **Architecture**: `docs/guides/architecture.md` - System design
- **Issues**: GitHub issues for questions/bugs
- **Community**: Discord/Slack for real-time help

---

## Windows transport — named-pipe RPC (0.8.x)

### Status
**Landed (WEFT-11 + WEFT-559)** — `DaemonClient` + named-pipe server
helpers in `clawft-rpc`, weave daemon accept loop under
`cfg(windows)`, and `kernel start|stop|restart` gated for Windows.
`x86_64-pc-windows-msvc` is re-enabled in cargo-dist; CI Windows
roundtrip remains a residual verification step.

### Priority
**Medium** — required to ship a usable Windows binary. Linux (glibc +
musl) and macOS (x86_64 + arm64) remain the primary deployment
surface until Windows artefacts are soak-tested in release CI.

### What shipped

| Piece | Location | Status |
|-------|----------|--------|
| Logical → pipe name mapping | `protocol::pipe_name_for_path` / `default_pipe_name` | Done (WEFT-11) |
| `DaemonClient` over `NamedPipeClient` | `crates/clawft-rpc/src/client.rs` (`cfg(windows)`) | Done (WEFT-11) |
| Server bind helpers | `crates/clawft-rpc/src/named_pipe.rs` (`create_listener`, `create_listener_next`) | Done (WEFT-11) |
| Unit tests | `pipe_name_*` (all platforms); roundtrip under `cfg(windows)` | Done |
| Daemon accept loop | `crates/clawft-weave/src/daemon.rs` (`cfg(windows)`) | Done (WEFT-559) |
| Transport-agnostic `handle_connection` | `daemon.rs` (`AsyncRead + AsyncWrite`) | Done (WEFT-559) |
| `kernel start/stop/restart` on Windows | `kernel_cmd.rs` (RPC shutdown + taskkill) | Done (WEFT-559) |
| cargo-dist Windows target | `Cargo.toml` `[workspace.metadata.dist]` | Re-enabled |
| CI Windows matrix job | `.github/workflows/` | Residual (document-only) |

### Code references
- `crates/clawft-rpc/src/client.rs` — Unix UDS + Windows named-pipe
  `DaemonClient`; other platforms return `None` with a clear error on
  `call`.
- `crates/clawft-rpc/src/named_pipe.rs` — server listener helpers.
- `crates/clawft-rpc/src/protocol.rs` — `pipe_name_for_path` (stable
  hash of the logical `socket_path` under `\\.\pipe\clawft-kernel-…`).
- `crates/clawft-weave/src/daemon.rs` — Unix `UnixListener` or Windows
  `create_listener` / `create_listener_next` accept loop; connection
  handlers are stream-oriented (`tokio::io::split`).
- `crates/clawft-weave/src/commands/kernel_cmd.rs` — Windows stop uses
  `kernel.shutdown` RPC then `taskkill`; restart is stop+start.
- `Cargo.toml` `[workspace.metadata.dist]` — `x86_64-pc-windows-msvc`
  listed.

### cargo-dist Windows re-enable steps (operator)

1. Confirm `[workspace.metadata.dist].targets` includes
   `"x86_64-pc-windows-msvc"` (done with WEFT-559).
2. On a Windows host (or `windows-latest` CI):
   ```powershell
   cargo test -p clawft-rpc --target x86_64-pc-windows-msvc
   cargo build -p clawft-weave --release --target x86_64-pc-windows-msvc
   .\target\x86_64-pc-windows-msvc\release\weaver.exe kernel start --foreground
   # other shell:
   .\weaver.exe kernel status
   ```
3. Tag a release so cargo-dist produces the zip + powershell installer.
4. Optional residual: add a `windows-latest` job that runs the
   `cfg(windows)` named-pipe roundtrip test.

### Verification

- [x] `scripts/build.sh check` / `cargo test -p clawft-rpc` on Unix
      (pipe-name tests + connect-none tests).
- [x] Daemon accept loop + `kernel start|stop|restart` wired under
      `cfg(windows)` (WEFT-559).
- [x] `x86_64-pc-windows-msvc` re-enabled in cargo-dist targets.
- [ ] `cargo test -p clawft-rpc --target x86_64-pc-windows-msvc` on a
      Windows host (named-pipe roundtrip).
- [ ] `weaver kernel start --foreground` on Windows accepts named-pipe
      connections from `weaver kernel status`.
- [ ] cargo-dist zip/powershell artefact installs and runs end-to-end.

### Tracking

- Plane: **WEFT-11** (client + helpers), **WEFT-483** (0.7.0 deferral,
  closed by stub), **WEFT-559** (daemon accept loop + dist re-enable).

---

**Last Updated**: 2026-07-31
**Maintainer**: Project Owner
**Status**: Living document - update as items are completed
