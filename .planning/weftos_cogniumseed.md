# WeftOS Business Logic Implementation

## Document Info

| Field | Value |
|-------|-------|
| Version | 1.0 |
| Date | 2026-04-09 |
| Scope | WeftOS kernel integration with CognitiveSeed sensor fusion platform |

## 1. WeftOS Role in CognitiveSeed

WeftOS serves as the operating kernel for the Pi 5 compute node. It provides three capabilities that do not exist in the base CognitiveSeed/RuView stack:

1. **ECC (Ephemeral Causal Cognition)** — Persistent semantic memory with HNSW search, causal relationship tracking, and adaptive learning cycles
2. **ExoChain** — Tamper-evident, dual-signed (Ed25519 + ML-DSA-65 post-quantum) audit trail for every measurement, training run, and model deployment
3. **Constitutional Governance** — Three-branch rule system that enforces privacy, safety, and data handling policies on all system operations

## 2. Kernel Boot Sequence

### 2.1 Configuration

WeftOS kernel requires a `kernel` section in `~/.clawft/config.json`:

```json
{
  "kernel": {
    "max_processes": 64,
    "health_check_interval_secs": 30,
    "resource_tree": {
      "checkpoint_path": "/home/genesis/.clawft/chain.tree.json"
    },
    "checkpoint_interval": 100
  }
}
```

Without this section, the kernel boots in minimal mode (cron only). With it, the full stack initializes:

### 2.2 Boot Events (ExoChain Witnessed)

```
[INIT      ] WeftOS v0.1.0 booting...
[CONFIG    ] Max processes: 64, Health check: 30s
[SERVICES  ] Cron, Assessment, Container, Cluster registered
[SERVICES  ] Ed25519 signing key loaded
[SERVICES  ] Dual signing enabled (Ed25519 + ML-DSA-65)
[SERVICES  ] Local chain ready (signed=true)
[TREE      ] Resource tree restored from checkpoint
[ECC       ] ECC resource tree namespaces registered
[TREE      ] 36 built-in tools registered
[SERVICES  ] Governance genesis anchored (22 rules, threshold=0.7)
[ECC       ] ECC calibration complete (p50=15us, p95=34us, tick=50ms)
[ECC       ] ECC ready (hnsw=0, causal=0, tick=50ms)
[READY     ] Boot complete — 6 services active
```

Every boot event is appended to the ExoChain with SHAKE-256 hash linking.

### 2.3 Service Registry After Boot

| Service | Type | Function |
|---------|------|----------|
| cron | cron | Scheduled tasks (model checkpoints, compaction) |
| assessment | custom | Codebase coherence analysis |
| containers | custom | Docker/Podman integration (future) |
| cluster | core | Mesh peer management (ruvector) |
| ecc.hnsw | core | Vector similarity search for embeddings |
| ecc.cognitive_tick | core | Adaptive processing loop for learning |

## 3. ECC Integration with Sensor Fusion

### 3.1 HNSW Memory Store

Every sensor frame generates an embedding that is stored in the ECC HNSW index:

```
Embedding composition (per frame):
  CSI features:    256 floats (64 subcarriers × 4 nodes, normalized)
  Radar features:  18 floats (3 targets × (x, y, speed) × 2 (raw + fused))
  Temporal:        4 floats (time_of_day, day_of_week, seconds_since_wake, frame_count)
  
  Total embedding: 278-dimensional vector
  
  Metadata attached:
    target_count: int
    target_classes: [human, dog, cat, unknown]
    training_loss: float (if training frame)
    room_id: string
    model_version: int
```

**Use cases for HNSW search:**

| Query | What It Returns | Business Value |
|-------|----------------|---------------|
| "Find 10 most similar states to right now" | Historical frames with matching CSI/radar signatures | Anomaly detection — if no match, something new is happening |
| "Find frames where cat was in kitchen" | Filtered by metadata (class=cat, room=kitchen) | Activity pattern analysis |
| "Find frames with high training loss" | Frames where CSI model disagreed with radar | Identify challenging scenarios for model improvement |
| "Find frames from same time yesterday" | Temporal similarity query | Routine detection — is today's pattern normal? |

### 3.2 Causal DAG

The causal DAG tracks relationships between events:

```
Example causal chains:

  PIR_wake(node_A, 08:15) ──causes──> mesh_wake(all_nodes, 08:15)
       │
       └──causes──> CSI_detect(human, 08:15)
                        │
                        ├──causes──> radar_confirm(target_1, pos=(2.1, 3.4), 08:15)
                        │
                        └──causes──> vital_signs(hr=72, br=16, 08:15)
                                         │
                                         └──causes──> training_update(epoch=1547, loss=0.02)

  CSI_detect(cat, 14:30) ──causes──> zone_alert("cat in kitchen", 14:30)
       │
       └──correlates──> CSI_detect(cat, 14:25, kitchen)
                            │
                            └──correlates──> CSI_detect(cat, 13:50, kitchen)
                                                 │
                                                 └── PATTERN: "cat visits kitchen every ~40 min"
```

The causal DAG enables:
- Root cause analysis ("why did the alarm trigger?")
- Pattern discovery ("the cat always goes to the kitchen before the owner comes home")
- Drift diagnosis ("model accuracy dropped because furniture was moved — CSI environment changed")

### 3.3 Cognitive Tick

The adaptive processing loop runs at 50ms intervals (calibrated at boot):

```
Every tick:
  1. Check for new sensor embeddings in the queue
  2. Update HNSW index with new embeddings
  3. Process accumulated causal edges
  4. Check for cognitive drift:
     - Compare recent embeddings to historical distribution
     - If drift > threshold → flag for model retraining
  5. Update impulse queue (short-TTL coordination signals)
```

## 4. ExoChain Witnessing Strategy

### 4.1 What Gets Witnessed

| Event Type | Frequency | Chain Event Fields |
|------------|-----------|-------------------|
| **Kernel boot/shutdown** | On event | version, config, elapsed_ms |
| **Health check** | Every 30s | overall_health, service_count |
| **Training epoch** | Every 100 frames (~10s) | epoch, loss, sample_count, model_hash |
| **Model deployment** | On convergence | version, accuracy, training_hours, data_hash |
| **Anomaly detected** | On event | type, confidence, target_desc, frame_ref |
| **Target enter/exit** | On event | target_id, class, zone, timestamp |
| **Drift detected** | On event | metric, current_value, baseline, severity |
| **Configuration change** | On event | changed_field, old_value, new_value |
| **Node wake/sleep** | On event | node_id, trigger_source, battery_pct |

### 4.2 Chain Structure

```
Event N
  sequence:     1547
  chain_id:     0
  prev_hash:    "a3f2c8..."  (SHAKE-256 link to event N-1)
  hash:         "7b4d91..."  (SHAKE-256 of this event)
  source:       "fusion"
  kind:         "training.epoch"
  timestamp:    "2026-04-09T14:30:00Z"
  payload_hash: "d8e1f0..."  (SHA-256 of payload JSON)
  signature:    "..." (Ed25519)
  dual_sig:     "..." (ML-DSA-65, post-quantum)
  
  payload: {
    "epoch": 1547,
    "loss": 0.023,
    "samples": 150000,
    "model_hash": "sha256:a3f2c8d1...",
    "accuracy": {
      "position_mae_cm": 8.3,
      "class_accuracy": 0.94,
      "vital_signs_mae_bpm": 1.8
    },
    "radar_validation": {
      "frames_validated": 1000,
      "agreement_rate": 0.97
    }
  }
```

### 4.3 Attestation Document

Produced on request (API endpoint or scheduled):

```json
{
  "attestation": {
    "device_id": "cognitum-seed-xxxx",
    "timestamp": "2026-04-09T14:30:00Z",
    "chain_head": "7b4d91...",
    "chain_depth": 1547,
    "model": {
      "version": 47,
      "hash": "sha256:a3f2c8d1...",
      "trained_on": "12.3 hours of data",
      "accuracy": "94% classification, 8.3cm position MAE"
    },
    "store": {
      "epoch": 2341,
      "vector_count": 45000,
      "content_hash": "sha256:b5e3f7..."
    },
    "system": {
      "nodes_active": 4,
      "uptime_hours": 168,
      "targets_tracked": 3
    },
    "signatures": {
      "ed25519": "...",
      "ml_dsa_65": "..."
    }
  }
}
```

This document cryptographically proves: this device, running this model (trained on this data, achieving this accuracy), with this measurement history, signed with both classical and post-quantum algorithms. Useful for healthcare compliance, insurance, elder care liability, and pet care SLAs.

## 5. Governance Rules

### 5.1 Privacy Rules

| Rule | Effect Vector Dimension | Threshold | Action |
|------|------------------------|-----------|--------|
| Raw CSI data cannot leave the device | Privacy: 1.0 | Production: 0.3 | Block any network export of raw CSI frames |
| Radar target coordinates are anonymized in logs | Privacy: 0.8 | Production: 0.3 | Replace (x,y) with zone labels in exported data |
| Model training data is not exportable | Privacy: 0.9 | Production: 0.3 | Block model training data sync to mesh peers |
| Only aggregate statistics cross the mesh | Privacy: 0.6 | Production: 0.3 | Allow: counts, averages, zone labels. Block: raw positions, trajectories |

### 5.2 Safety Rules

| Rule | Dimension | Threshold | Action |
|------|-----------|-----------|--------|
| Anomaly detection must alert within 30s | Risk: 0.7 | All envs | Trigger alert pipeline if unknown target type persists |
| Fall detection triggers immediate notification | Risk: 0.9 | All envs | Push notification + ExoChain witness |
| Model retraining requires minimum 1000 samples | Risk: 0.5 | Production | Prevent deploying undertrained models |
| Radar must validate CSI predictions periodically | Risk: 0.6 | Production | Radar cannot duty-cycle below 5% |

### 5.3 Operational Rules

| Rule | Dimension | Action |
|------|-----------|--------|
| Battery below 10% → reduce to presence-only mode | Security: 0.4 | Disable radar, CSI at 5 Hz, conserve power |
| Node offline > 5 minutes → redistribute tracking | Novelty: 0.3 | Remaining nodes increase coverage |
| ExoChain depth > 100K events → checkpoint and compact | Security: 0.2 | Automatic chain management |

## 6. Resource Tree Namespace

```
/
├── /apps
│   └── /cognitive-seed              # Main application
│       ├── /config                   # Runtime configuration
│       └── /models                   # Trained model versions
├── /kernel
│   ├── /agents
│   │   ├── /fusion-engine           # Sensor fusion process
│   │   ├── /trainer                 # Online learning process
│   │   └── /dashboard               # PWA server process
│   ├── /services
│   │   ├── /cron                    # Scheduled tasks
│   │   └── /ecc                     # Cognitive substrate
│   │       ├── /hnsw                # Vector memory
│   │       ├── /causal              # Causal DAG
│   │       ├── /tick                # Adaptive loop
│   │       └── /crossrefs           # Cross-structure links
│   └── /tools                       # 36 built-in tools
├── /network
│   └── /peers                       # Mesh peer nodes
│       ├── /node-a                  # Sensor node metadata
│       ├── /node-b
│       ├── /node-c
│       └── /node-d
└── /environments
    └── /production                  # Governance threshold: 0.3
```

## 7. API Surface (Combined)

### 7.1 Cognitum Agent (HTTPS :8443) — 33 endpoints

Existing API unchanged. Handles identity, vectors, witness, custody, sync, profiles, optimize, delivery, delta.

### 7.2 New Fusion Endpoints (to add to ws_server.py)

| Endpoint | Method | Purpose |
|----------|--------|---------|
| `/api/v1/targets` | GET | Current tracked targets with position, class, velocity |
| `/api/v1/targets/{id}/history` | GET | Position history for a specific target |
| `/api/v1/model/status` | GET | Training status, accuracy, epoch, phase |
| `/api/v1/model/retrain` | POST | Force model retraining from radar ground truth |
| `/api/v1/zones` | GET/POST | Define named zones (kitchen, bedroom, etc.) |
| `/api/v1/zones/{name}/events` | GET | Entry/exit events for a zone |
| `/api/v1/alerts` | GET | Active alerts (anomalies, falls, drift) |
| `/api/v1/nodes` | GET | Sensor node status (battery, signal, last seen) |

### 7.3 WeftOS Kernel (weaver) — CLI + IPC

Accessed via `weaver` CLI or kernel IPC socket:

| Command | Purpose |
|---------|---------|
| `weaver ecc search --query <embedding>` | Find similar historical states |
| `weaver ecc causal --node <event_id>` | Trace causal chain from an event |
| `weaver ecc tick` | Current cognitive tick statistics |
| `weaver chain local` | Recent chain events |
| `weaver chain verify` | Full chain integrity check |
| `weaver resource tree` | Merkle-hashed resource namespace |
| `weft assess` | Codebase coherence analysis |
| `weft agent -m "query"` | AI-powered analysis of system state |

## 8. Deployment Procedure

### 8.1 Pi 5 Setup

```bash
# 1. Flash Raspberry Pi OS 64-bit
#    User: genesis, Password: cognitum, Enable SSH, Configure WiFi

# 2. Install WeftOS
curl -fsSL https://github.com/WeftOS/weftos/releases/latest/download/clawft-cli-installer.sh | sh
curl -fsSL https://github.com/WeftOS/weftos/releases/latest/download/clawft-weave-installer.sh | sh

# 3. Initialize WeftOS
weft onboard
# Add kernel config section to ~/.clawft/config.json (see section 2.1)

# 4. Deploy CognitiveSeed
scp -r cognitiveseed/ genesis@<PI_IP>:/home/genesis/seed-deploy/
ssh genesis@<PI_IP> "bash /home/genesis/seed-deploy/SETUP.sh"

# 5. Deploy Cognitum Core
ssh genesis@<PI_IP> "sudo bash /home/genesis/seed-deploy/cognitum-core-setup.sh"

# 6. Start WeftOS kernel
ssh genesis@<PI_IP> "weaver kernel start"

# 7. Verify
ssh genesis@<PI_IP> "weaver kernel status && weaver chain verify"
```

### 8.2 Sensor Node Setup

```bash
# 1. Flash RuView firmware to each ESP32-S3
#    Configure: WiFi SSID, Pi IP address, Node ID (A/B/C/D)

# 2. Wire LD2450 (UART), AM312 (GPIO4), antenna (U.FL), battery (TP4056)

# 3. Mount on walls (one per wall, ~1.5m height, facing into room)

# 4. Power on — PIR triggers wake, ESP-NOW wakes mesh, CSI+radar streaming begins

# 5. Verify on Pi:
#    journalctl -u pulse-ws -f  (should show packets from 4 nodes)
#    curl http://localhost:8888  (dashboard should load)
```

## 9. Future Capabilities (Enabled by WeftOS)

| Capability | WeftOS Feature Used | Timeline |
|------------|-------------------|----------|
| Multi-room mesh (multiple Pi nodes) | Cluster + chain replication | Phase 2 |
| AI assistant queries ("where's the cat?") | MCP tools + ECC search | Phase 2 |
| Federated learning across households | Mesh + governance (privacy rules) | Phase 3 |
| Insurance/compliance attestation | ExoChain + custody API | Phase 2 |
| Voice control ("arm the system") | WeftOS voice STT/TTS | Phase 3 |
| Anomaly alerting (SMS/push) | WeftOS channels (Telegram, Slack) | Phase 2 |
| WASM browser dashboard | WeftOS WASM light node | Phase 2 |


# WeftOS + Cognitum Integration

## Overview

WeftOS is a secure distributed AI operating system built in Rust. Cognitum is a custody-first identity appliance with a vector store and witness chain. The two systems share design philosophy around cryptographic provenance and can complement each other.

## WeftOS Architecture (Summary)

```
+-----------------------------------------------+
| clawft (AI Framework)                         |
| LLM providers, channels, tools, skills, agents|
+-----------------------------------------------+
| WeftOS Kernel (K0-K6)                         |
| Processes, IPC, governance, ExoChain,         |
| mesh networking, WASM sandbox                 |
+-----------------------------------------------+
| ECC (Ephemeral Causal Cognition)              |
| Causal DAG, HNSW memory, impulse queue        |
+-----------------------------------------------+
| Platform (native/WASM/browser/Docker/edge)    |
+-----------------------------------------------+
```

### Key WeftOS Components

| Component | Purpose | Cognitum Equivalent |
|-----------|---------|-------------------|
| ExoChain | Append-only hash chain with dual signing (Ed25519 + ML-DSA-65) | Witness chain (SHA-256, single signing) |
| Vector memory | HNSW approximate nearest neighbor search | Vector store (brute-force cosine search) |
| Process table | PID allocation, state machine, supervisor | None (single-process agent) |
| IPC | Typed kernel messages, 7 target types | None (HTTP request/response) |
| Mesh networking | Noise Protocol encrypted P2P (TCP/QUIC/WebSocket) | None (client-server only) |
| Constitutional governance | 3-branch system, 5D effect vectors | None |
| WASM sandbox | Fuel-metered tool execution | None |
| Capability RBAC | Per-agent permissions with scopes | None |

### Key Cognitum Components

| Component | Purpose | WeftOS Equivalent |
|-----------|---------|------------------|
| Device identity | Ed25519 keypair + UUID | ExoChain node identity |
| Witness chain | Provenance audit trail | ExoChain (superset) |
| Vector store | Epoch-versioned vector storage + sync | ECC HNSW memory (superset) |
| Pairing | Time-windowed host enrollment | Capability-based mesh join |
| Custody attestation | Signed integrity proofs | ExoChain verification |
| SSE delta stream | Real-time change events | IPC topic subscriptions |
| Profiles | Per-user vector namespaces | Agent process contexts |

## Integration Strategy

### Option A: Cognitum as a WeftOS Edge Node

Run Cognitum's Pi as a WeftOS edge node in a mesh. The cognitum-agent becomes a WeftOS process.

```
[WeftOS Cloud/Desktop Node]
         |
    mesh (Noise Protocol)
         |
[WeftOS Edge Node — Raspberry Pi]
    |-- cognitum-agent (as WeftOS process, PID assigned)
    |-- ExoChain replaces witness chain
    |-- ECC memory replaces vector store
    |-- Mesh IPC replaces HTTP API
```

**How to get there:**

1. Install WeftOS on the Pi (`weft` binary runs on ARM64 Linux)
2. Boot the kernel: `weave boot`
3. Register cognitum-agent as a WeftOS application
4. The agent's witness chain migrates to ExoChain (gains post-quantum signing, dual-layer verification)
5. The vector store migrates to ECC HNSW memory (gains approximate search, semantic queries, cross-session persistence)
6. Pairing becomes mesh peer joining with capability grants
7. SSE delta stream becomes IPC topic subscriptions

**What Cognitum gains:**
- Post-quantum cryptography (ML-KEM-768, ML-DSA-65)
- Distributed mesh coordination (no single point of failure)
- Constitutional governance gates on all operations
- WASM sandboxed tool execution
- Persistent semantic memory with HNSW search
- Causal DAG tracking relationships between data

**What WeftOS gains:**
- A physical custody device (hardware root of trust on Pi)
- Offline-first identity that survives network loss
- A concrete edge deployment pattern

### Option B: Bridge Pattern (Incremental)

Keep both systems running independently. Build a bridge that syncs data between them.

```
[cognitum-agent :8443] <--bridge--> [WeftOS clawft]
  witness chain   ----sync--->  ExoChain events
  vector store    ----sync--->  ECC memory
  device identity ----map---->  node identity
  SSE stream      ----pipe--->  IPC topics
```

**Implementation:**

A Python or Rust bridge process that:
1. Subscribes to Cognitum's SSE delta stream (`GET /api/v1/delta/stream`)
2. On each `ingest` event, pushes vectors into WeftOS ECC memory
3. On each `witness` event, records a corresponding ExoChain entry
4. Exposes Cognitum's identity as a WeftOS node credential
5. Routes WeftOS IPC messages to Cognitum HTTP endpoints

This is the lowest-risk path — no changes to either system, just a translation layer.

### Option C: Replace Cognitum Agent with WeftOS Agent

The most complete integration. Replace `cognitum-agent` entirely with a WeftOS agent that exposes the same API surface but uses WeftOS internals.

The cognitum-agent API (33 endpoints) becomes a WeftOS skill/tool set:
- `/store/*` → ECC memory operations
- `/witness/*` → ExoChain queries
- `/custody/*` → ExoChain verification + governance
- `/identity` → WeftOS node identity
- `/pair/*` → Mesh peer management
- `/delta/stream` → IPC topic subscription

## WeftOS Installation on Cognitum Pi

WeftOS ships ARM64 Linux binaries. On the Pi 5:

```bash
# Install clawft CLI (agent framework)
curl -fsSL https://github.com/WeftOS/weftos/releases/latest/download/clawft-cli-installer.sh | sh

# Install weaver (kernel daemon)
curl -fsSL https://github.com/WeftOS/weftos/releases/latest/download/clawft-weave-installer.sh | sh

# Verify
weft --version    # clawft CLI
weaver --version  # WeftOS kernel

# Initialize workspace
cd /home/genesis
weft onboard

# Start kernel daemon
weaver kernel start

# Check status
weaver kernel status

# Run assessment on cognitum codebase
weft assess run --dir /home/genesis/seed-deploy
```

## Shared Design Principles

Both systems were built around the same core ideas:

1. **Cryptographic provenance** — Every mutation is hash-chained and verifiable
2. **Offline-first** — Full functionality without cloud connectivity
3. **Epoch-based sync** — Incremental replication with conflict detection
4. **Self-contained identity** — Device/node generates its own keypair
5. **Minimal dependencies** — Cognitum uses stdlib only; WeftOS uses Rust with no runtime deps

This alignment makes integration natural rather than forced.

## Current State

### Installed in this project
- WeftOS CLI (`weft` v0.5.1) — installed at `~/.cargo/bin/weft`
- Workspace initialized at `.weftos/` with assessment config
- Initial assessment completed: 35 files, 10531 lines, 100% coherence

### Assessment Results
```
Files scanned:      35
Lines of code:      10531
Coherence score:    100.0%
Findings:           1 (medium: pulse-data.js at 1177 lines, consider splitting)
```

### Next Steps

1. Run `weft assess` after each change to track coherence
2. Use `weft assess link` to cross-reference with the parent PowerPlatePulse repo
3. Evaluate Option B (bridge pattern) as the first integration step
4. Test WeftOS on Pi 5 ARM64 to verify edge deployment viability
