# WeftOS Configuration Reference

> **Status (2026-10-27).** Hand-written, last edited 2026-03-27, and limited to the kernel section of `config.json`. The generated references are authoritative where they differ: [../reference/config.md](../reference/config.md) and [../reference/config-sections.md](../reference/config-sections.md) (kernel, mesh, router, project, cog.toml). Proposed: reduce this page to a stub that links to them.

WeftOS reads its configuration from a JSON file (typically `config.json` at the
project root or `~/.clawft/config.json` globally). The root `Config` struct
contains sections for agents, providers, gateway, tools, voice, and the kernel
subsystem.

This document focuses on the **kernel** section and related subsystems that
control WeftOS behavior.

## Kernel Configuration

The `kernel` section controls the WeftOS kernel subsystem.

```json
{
  "kernel": {
    "enabled": false,
    "max_processes": 64,
    "health_check_interval_secs": 30
  }
}
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `false` | Activate the kernel on startup. When false, kernel subsystems only run when invoked via `weave kernel` commands. |
| `max_processes` | u32 | `64` | Maximum concurrent entries in the process table. Raise for workloads that spawn many parallel agents. |
| `health_check_interval_secs` | u64 | `30` | Seconds between periodic health checks. Lower values detect failures faster but add overhead. |

## Chain Configuration

Nested under `kernel.chain`. Controls the cryptographic audit trail (requires
the `exochain` feature gate).

```json
{
  "kernel": {
    "chain": {
      "enabled": true,
      "checkpoint_interval": 1000,
      "chain_id": 0,
      "checkpoint_path": ".clawft/chain.json"
    }
  }
}
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `true` | Enable the local chain. |
| `checkpoint_interval` | u64 | `1000` | Events between automatic checkpoints. Lower values give more durable state at the cost of more I/O. |
| `chain_id` | u32 | `0` | Chain identifier. `0` means the local node chain. |
| `checkpoint_path` | string? | `~/.clawft/chain.json` | File path for chain persistence. Omit to use the default location. |

## Resource Tree Configuration

Nested under `kernel.resource_tree`. Manages the hierarchical resource model
(requires the `exochain` feature gate).

```json
{
  "kernel": {
    "resource_tree": {
      "enabled": true,
      "checkpoint_path": ".clawft/resources.json"
    }
  }
}
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `true` | Enable the resource tree. |
| `checkpoint_path` | string? | (in-memory) | Path to checkpoint file. Omit for in-memory only. |

## Cluster Configuration

Nested under `kernel.cluster`. Controls distributed coordination (requires the
`cluster` and/or `mesh` feature gates).

```json
{
  "kernel": {
    "cluster": {
      "replication_factor": 3,
      "shard_count": 64,
      "heartbeat_interval_secs": 5,
      "node_timeout_secs": 30,
      "enable_consensus": true,
      "min_quorum_size": 2,
      "seed_nodes": ["192.168.1.100:8080"],
      "node_name": "node-alpha"
    }
  }
}
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `replication_factor` | usize | `3` | Replica copies per shard. |
| `shard_count` | u32 | `64` | Total shards in the cluster. |
| `heartbeat_interval_secs` | u64 | `5` | Seconds between heartbeat checks. |
| `node_timeout_secs` | u64 | `30` | Seconds before marking a node offline. |
| `enable_consensus` | bool | `true` | Enable DAG-based consensus. |
| `min_quorum_size` | usize | `2` | Minimum nodes required for quorum. |
| `seed_nodes` | string[] | `[]` | Addresses of coordinator nodes for discovery. |
| `node_name` | string? | (none) | Human-readable display name for this node. |

## Gateway Configuration

Top-level `gateway` section. Controls the HTTP/WebSocket server.

```json
{
  "gateway": {
    "host": "0.0.0.0",
    "port": 18790,
    "api_port": 18789,
    "api_enabled": false,
    "cors_origins": ["http://localhost:5173"]
  }
}
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `host` | string | `"0.0.0.0"` | Bind address. |
| `port` | u16 | `18790` | Main gateway listen port. |
| `api_port` | u16 | `18789` | REST API port (separate from gateway). |
| `api_enabled` | bool | `false` | Whether the REST/WS API is active. |
| `cors_origins` | string[] | `["http://localhost:5173"]` | Allowed CORS origins for the API. |

## ECC Tick Calibration

The ECC tick loop auto-calibrates at boot based on system performance. You can
influence calibration via `CalibrationConfig`:

| Parameter | Default | Description |
|-----------|---------|-------------|
| `tick_interval_ms` | `50` | Minimum tick interval in milliseconds. The calibrator may increase this if the system is slow. |

Calibration runs during `weave boot` and logs the chosen tick interval. You
typically do not need to change this.

## Embedding Backend

The ECC cognitive substrate uses pluggable embedding backends selected at
compile time:

| Backend | Feature gate | Description |
|---------|-------------|-------------|
| Mock | (default) | Deterministic hash-based vectors. Fast, no external model needed. |
| ONNX | `onnx-embeddings` | Real neural embeddings via `all-MiniLM-L6-v2`. Requires model download. |

The model path defaults to `.weftos/models/all-MiniLM-L6-v2.onnx`. Run
`scripts/download-model.sh` to fetch it.

## Configuration Precedence

```
compiled defaults  <  ~/.clawft/config.json (global)  <  project config.json  <  env vars
```

Both `snake_case` and `camelCase` field names are accepted in JSON (e.g.,
`max_processes` and `maxProcesses` are equivalent).

## Minimal Working Configuration

For most development, an empty config is sufficient -- all fields have sensible
defaults:

```json
{}
```

For single-node production:

```json
{
  "kernel": {
    "enabled": true,
    "chain": { "enabled": true },
    "resource_tree": { "enabled": true }
  },
  "gateway": {
    "api_enabled": true
  }
}
```
