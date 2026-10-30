# WeftOS Feature Gate Reference

> **Status (2026-10-27).** Canonical for `clawft-kernel` features, but incomplete. Of the 22 entries under `[features]` in `crates/clawft-kernel/Cargo.toml`, the table below does not list `diskann`, `http-api`, `mesh-rvf`, `quantum-braket`, `quantum-pasqal`, `quic`, `rabitq-bench`, `sensor`, `test-support`, `treesitter` and `workload-runtime`; it lists `bluetooth`, `network`, `rfkill`, `voice`, `voice-wake` and `voice-wake-rustpotter`, which are not kernel features (method: feature names parsed from the Cargo.toml with tomllib against the first column of this table). The descriptions of the features it does list have not been re-checked. Rewrite proposed; see the Doc queue.

All feature flags are defined in `crates/clawft-kernel/Cargo.toml`. The default
set is `native`, `exochain`, `cluster`, `tilezero`, `ecc`, `mesh`, `quic` and
`workload-runtime` (`default` under `[features]`). `weaver` (`clawft-weave`)
enables `cluster`, `ecc`, `exochain`, `mesh` and `placement` by default.

## Feature Table

| Feature | What it enables | Key dependencies | Use when |
|---------|----------------|------------------|----------|
| `native` | Tokio async runtime, full kernel boot | tokio, tokio-util, futures | Always -- this is the default |
| `ecc` | Cognitive substrate: causal graph, HNSW vector memory, tick loop | blake3, clawft-core/vector-memory | AI/cognitive features, semantic search |
| `exochain` | Cryptographic audit trail, resource tree, governance events | rvf-crypto, rvf-wire, rvf-types, rvf-runtime, exo-resource-tree, ed25519-dalek, ciborium | Audit logging, compliance, governance |
| `mesh` | P2P WebSocket networking between nodes | ed25519-dalek, tokio-tungstenite, futures-util | Multi-node clusters |
| `os-patterns` | Self-healing, dead-letter queue, metrics, structured logging | (implies `exochain`) | Production deployments |
| `onnx-embeddings` | Real ONNX model inference for embeddings | ort, ndarray | Quality semantic search (replaces mock provider) |
| `wasm-sandbox` | Wasmtime-based tool execution sandbox | wasmtime, wasmtime-wasi | Running untrusted WASM tools safely |
| `cluster` | Distributed coordination: Raft consensus, sharding, replication | ruvector-cluster, ruvector-raft, ruvector-replication, parking_lot | Multi-node consensus and data replication |
| `tilezero` | TileZero governance gate | cognitum-gate-tilezero (implies `exochain`) | Advanced governance policies |
| `containers` | Container runtime integration markers | (none extra) | Docker/Podman orchestration hooks |
| `quic` | QUIC (quinn) mesh transport; heavy rustls/ring tree, on by default (opt out with `--no-default-features --features mesh`) | (implies `mesh`) quinn, rcgen, rustls | Multi-node clusters over QUIC |
| `mesh-rvf` | Experimental RVF `KernelMessage` IPC encoding (`MeshIpcEncoding::Rvf`); encode/decode return `UnsupportedEncoding` for now, default stays JSON | (implies `mesh`) | Experimenting with the RVF IPC path; not for production |
| `workload-runtime` | `WorkloadRuntime` adapters: native process, container engines, Cognitum Seed API | (implies `native`, `ecc`, `exochain`) reqwest, rustls, serde_yaml | Placing workloads on nodes; on by default |
| `quantum-pasqal` | Experimental Pasqal quantum backend; interface only, stubs return `NotImplemented` | (implies `ecc`) reqwest | Quantum backend development |
| `quantum-braket` | Experimental Amazon Braket quantum backend; interface only, stubs return `NotImplemented` | (implies `ecc`) reqwest | Quantum backend development |
| `diskann` | DiskANN vector backend | (implies `ecc`) ruvector-diskann | Large vector indexes beyond RAM |
| `rabitq-bench` | RaBitQ runner in `benches/vector_backend_bench.rs` (bench only) | ruvector-rabitq | Running the vector backend benchmark |
| `treesitter` | Tree-sitter code analysis plugin (Rust, TypeScript) | clawft-plugin-treesitter | Source code analysis |
| `sensor` | Sensor feature marker | (none extra) | Sensor integrations |
| `http-api` | HTTP API feature marker | (none extra) | HTTP API surface |
| `test-support` | Test-only hooks for other crates' integration tests (debug builds only) | (none extra) | Integration tests in other crates |

## Dependency Graph

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

Features that imply `exochain` automatically pull in its dependencies. You do
not need to list `exochain` explicitly when enabling `tilezero` or
`os-patterns`.

## Recommended Combinations

### Development (fast builds)

```bash
scripts/build.sh native
# Only the default `native` feature. Fastest compile times.
```

### Single-node Production

```bash
scripts/build.sh native --features ecc,exochain,os-patterns
```

Gives you the cognitive substrate, cryptographic audit trail, self-healing,
and structured metrics -- everything needed for a single production node.

### Multi-node Cluster

```bash
scripts/build.sh native --features ecc,exochain,os-patterns,mesh,cluster
```

Adds P2P mesh networking and Raft-based distributed consensus on top of the
single-node stack.

### Full Features

```bash
scripts/build.sh native --features ecc,exochain,os-patterns,mesh,cluster,onnx-embeddings,wasm-sandbox
```

Everything enabled. Requires the ONNX model to be downloaded
(`scripts/download-model.sh`) and a Wasmtime-compatible host.

## Checking Active Features

At runtime, `weave status` reports which feature gates were compiled in.
During development, `scripts/build.sh check` validates that the selected
feature combination compiles cleanly.

## Adding a New Feature Gate

1. Add the feature and its optional deps to `crates/clawft-kernel/Cargo.toml`.
2. Guard code with `#[cfg(feature = "your-feature")]`.
3. Add a row to the table above.
4. Run `scripts/build.sh gate` to verify all combinations still compile.

## Platform constraints (host OS, not Cargo features)

Some host-local substrate adapters are gated by **target OS**, not by
Cargo features — Cargo features cannot express "only on Linux."

### Linux-only sysfs adapters (WEFT-420)

| Adapter | Crate path | Probe source |
|---------|------------|--------------|
| `network` | `clawft-substrate::network` | `/sys/class/net`, `/sys/class/power_supply` |
| `bluetooth` | `clawft-substrate::bluetooth` | `/sys/class/bluetooth`, `/sys/class/rfkill` |
| `rfkill` | `clawft-substrate::rfkill` | `/sys/class/rfkill` (same ABI; related) |

**Compile gate:** modules are native-only (`cfg(not(target_arch = "wasm32"))`).
**Runtime gate:** `clawft_substrate::sysfs::linux_sysfs_native()`
(`cfg!(target_os = "linux")`).

| Host | Behaviour |
|------|-----------|
| Linux | Real sysfs sampling. Missing hardware → plain `absent` / `present: false` (no platform `cause`). |
| macOS / Windows / other | Adapter still **opens** so tray/Explorer keep a uniform surface. Emits placeholders with `platform: "unsupported"` and `cause: "sysfs-unavailable"`, and writes `substrate/meta/adapter/<id>/health` with `event: "error"` and that cause in `reason`. |
| wasm32 | Modules not compiled; GUI uses Snapshot fallback. |

macOS (CoreWLAN / IOBluetooth) and Windows (WinRT) ports are
**unscheduled**. Do not treat non-Linux `absent` as "no radio" without
checking `cause == "sysfs-unavailable"` or the adapter-health reason.

Shared helpers live in `crates/clawft-substrate/src/sysfs.rs`.

## Related crate features (not kernel)

### `clawft-plugin` voice / wake (WEFT-212, WEFT-671, WEFT-216)

| Feature | Crate | What it enables | Notes |
|---------|-------|-----------------|-------|
| `voice` | `clawft-plugin` | Umbrella: `voice-vad` + `voice-wake` + `voice-stt` + `voice-tts` + tokio | Legacy scaffold + transitional wake (WEFT-671). Product Talk Mode is **not** this path. |
| `voice-wake` | `clawft-plugin` | `WakeWordDetector` / `WakeDaemon` API | **Stub backend** only. `process_frame` always returns `false`. |
| `voice-wake-rustpotter` | `clawft-plugin` | Fail-closed `try_with_rustpotter` reserve | **Does not link rustpotter.** crates.io `rustpotter` 3.0.2 → `candle-core` 0.2.2 fails to compile on this workspace (rand dual + half/bf16). Returns `PluginError::NotImplemented` with `RUSTPOTTER_BLOCKED_REASON`. |

**Concrete alternative (documented, not wired):** OpenWakeWord-style keyword spotting via ONNX through `clawft-voice-onnx` (`ort`), once a trained model and capture loop exist. Do **not** add `dep:rustpotter` until upstream builds on current deps or a compatible pin is proven.

Full decision + revisit triggers: `docs/plans/wave-0k-WEFT-216-result.md`.

## Gaussian splat pipeline (clawft-splatd)

External **runtime** tools (not Cargo features). Crates always compile.

| Tool | Install (macOS) | Stage |
|------|-----------------|-------|
| ffmpeg / ffprobe | `brew install ffmpeg` | probe, frames |
| colmap | `brew install colmap` | sfm |
| brush-cli | Brush 0.3.0 release binary on PATH | train |
| splat-transform | `npm i -g @playcanvas/splat-transform` | compress (optional) |

See `docs/weftos/splat-harness.md` and `config/splatd.toml`.
