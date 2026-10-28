# WASM Deployment

> **Public site:** [Fumadocs — WASM Deployment](../src/content/docs/weftos/guides/deployment-wasm.mdx)
> (WEFT-468 dual surface — see [README.md](./README.md)). ECC is excluded on
> `wasm32-unknown-unknown` (WEFT-504). The browser build (`scripts/build.sh browser`)
> is documented under [`docs/browser/`](../browser/).

clawft compiles to a WebAssembly module targeting `wasm32-wasip2`. The WASM
build provides a lightweight agent core (target: < 300 KB uncompressed,
< 120 KB gzipped) suitable for edge devices, IoT, and browser-based
environments.

## Current Status

The `wasm32-wasip2` module is functional but uses **platform stubs** for several subsystems:

| Subsystem       | Status                    | Notes                                      |
|-----------------|---------------------------|--------------------------------------------|
| Environment     | Working (in-memory)       | `HashMap`-backed, thread-safe via `Mutex`  |
| HTTP client     | Stub (returns errors)     | Awaiting `wasi:http/outgoing-handler`      |
| Filesystem      | Stub (returns errors)     | Awaiting `wasi:filesystem` stabilisation   |
| Process spawn   | Not available             | No equivalent in WASM environments         |
| Channels        | Not available             | Telegram, Slack, Discord require native I/O|
| Shell tools     | Not available             | `exec_shell`, `spawn` excluded             |

Available tools in WASM: `read_file`, `write_file`, `edit_file`,
`list_directory`, `memory_read`, `memory_write`, `web_fetch`, `web_search`.

Excluded tools: `exec_shell`, `spawn`, `message`.

## Prerequisites

- **Rust 1.95+** with the `wasm32-wasip2` target:

  ```bash
  rustup target add wasm32-wasip2
  ```

- A WASI-compatible runtime. Supported options:
  - [Wasmtime](https://wasmtime.dev/) (recommended, full WASI preview 2)
  - [WAMR](https://github.com/bytecodealliance/wasm-micro-runtime) (minimal footprint, IoT)

## Building

Using the unified build script:

```bash
scripts/build.sh wasi      # WASI target (wasm32-wasip2, release-wasm profile)
scripts/build.sh browser   # Browser target (wasm32-unknown-unknown)
```

Or build directly with cargo:

```bash
cargo build -p clawft-wasm --target wasm32-wasip2 --release
```

The output module is at:

```
target/wasm32-wasip2/release/clawft_wasm.wasm
```

### Size Optimization

The release profile is pre-configured for size (`opt-level = "z"`, LTO,
strip, single codegen unit, `panic = "abort"`). To further reduce size:

```bash
# Install wasm-opt (part of binaryen)
apt install binaryen   # or: brew install binaryen

# Optimize the module
wasm-opt -Oz -o clawft_wasm.opt.wasm target/wasm32-wasip2/release/clawft_wasm.wasm
```

## Running with Wasmtime

```bash
wasmtime run target/wasm32-wasip2/release/clawft_wasm.wasm
```

To grant filesystem access (for config loading, once filesystem stubs are
replaced):

```bash
wasmtime run \
  --dir ~/.clawft::/root/.clawft \
  target/wasm32-wasip2/release/clawft_wasm.wasm
```

Pass environment variables:

```bash
wasmtime run \
  --env OPENAI_API_KEY="sk-..." \
  target/wasm32-wasip2/release/clawft_wasm.wasm
```

## Running with WAMR

```bash
iwasm target/wasm32-wasip2/release/clawft_wasm.wasm
```

WAMR uses less memory than Wasmtime and is suited for resource-constrained
devices. See the [WAMR documentation](https://github.com/bytecodealliance/wasm-micro-runtime)
for embedding in C/C++ applications.

## Platform Limitations

The WASM build excludes components that require native OS features:

1. **No shell execution** -- `exec_shell` and `spawn` tools are not registered.
2. **No messaging channels** -- Telegram, Slack, and Discord require long-lived
   TCP connections and are excluded.
3. **HTTP returns errors** -- The `WasiHttpClient` returns an error for all
   requests until `wasi:http/outgoing-handler` is implemented.
4. **Filesystem returns errors** -- The `WasiFileSystem` returns
   `ErrorKind::Unsupported` for all operations until WASI filesystem APIs are
   wired.
5. **No home directory** -- `home_dir()` returns `None` in WASM environments.
6. **Environment is in-memory** -- Variables set via `set_var` are stored in a
   `HashMap` and do not persist across module restarts.

## Size Budget

| Component    | Target     |
|--------------|------------|
| Uncompressed | < 300 KB   |
| Gzipped      | < 120 KB   |

The `release-wasm` Cargo profile inherits from `release` and uses `opt-level = "z"`
for aggressive size optimisation.

---

## WASM Plugin Host (C2)

The WASM plugin host allows third-party plugins to run as isolated WASM
modules inside the clawft process. Plugins are sandboxed with configurable
resource limits and have access to a controlled set of host functions.

Source: `crates/clawft-wasm-host/src/engine.rs`, `crates/clawft-wasm-host/src/sandbox.rs`. `clawft-wasm` re-exports the host behind the `wasm-plugins` feature.

### Feature Gate

The plugin host is gated behind the `wasm-plugins` feature flag:

```bash
# Build with WASM plugin support
cargo build -p clawft-wasm --features wasm-plugins

# Build without (default -- smaller binary)
cargo build -p clawft-wasm
```

### wasmtime Integration

The plugin host uses [wasmtime](https://wasmtime.dev/) (the workspace pins 48.x in `Cargo.toml`) with the WIT
(WebAssembly Interface Types) component model. Each plugin gets its own
`wasmtime::Store` with isolated memory and an independent fuel budget.

The engine is configured with:

- `consume_fuel(true)` -- enables fuel metering for CPU limiting
- `epoch_interruption(true)` -- enables wall-clock timeout via epoch ticks

### Resource Limits

| Resource | Default | Hard Maximum | Description |
|----------|---------|-------------|-------------|
| Fuel budget | 1,000,000,000 (~1s CPU) | 10,000,000,000 (~10s) | Fuel units consumed per WASM instruction |
| Memory | 16 MB | 256 MB | Per-plugin linear memory limit |
| Table elements | 10,000 | 100,000 | WASM table size limit |
| Execution timeout | 30s | 300s | Wall-clock timeout via epoch interruption |

Plugin manifests can request custom resource limits. Values are clamped to
the hard maximums at load time.

### Fuel Metering

Each WASM instruction consumes fuel from the plugin's budget. When fuel is
exhausted, the WASM execution traps with an error. The fuel budget resets
on each invocation (a fresh `Store` is created per call).

Configure the fuel budget per plugin:

```toml
[plugins.my-plugin.resources]
max_fuel = 500_000_000  # ~0.5s CPU
```

### Wall-Clock Timeout

Even with generous fuel budgets, a background thread enforces a wall-clock
timeout via wasmtime epoch interruption. When the timeout fires:

1. The background thread calls `engine.increment_epoch()`
2. Any running WASM code traps at the next epoch check point
3. The execution returns an error

This prevents plugins from consuming excessive real time (e.g., a tight
loop that consumes fuel slowly but runs for minutes).

### Memory Limits

Memory limits are enforced via `wasmtime::StoreLimits`:

```rust
StoreLimitsBuilder::new()
    .memory_size(config.max_memory_mb * 1024 * 1024)
    .table_elements(config.max_table_elements)
    .instances(10)
    .tables(10)
    .memories(2)
    .build()
```

A WASM `memory.grow` instruction that would exceed the limit returns `-1`
(or traps, depending on the module's behavior).

### Host Functions

Plugins interact with the host through 5 WIT-defined host functions:

| Function | Signature | Description |
|----------|-----------|-------------|
| `http-request` | `(method, url, headers, body) -> result<string, string>` | Make an HTTP request (validated against sandbox) |
| `read-file` | `(path) -> result<string, string>` | Read a file (validated against filesystem allowlist) |
| `write-file` | `(path, content) -> result<_, string>` | Write a file (validated, max 4 MB) |
| `get-env` | `(name) -> option<string>` | Read an environment variable (allowlist + deny list) |
| `log` | `(level, message)` | Emit a log message (rate limited, max 4 KB) |

All host function calls pass through the `PluginSandbox` validation layer
and are recorded in the per-plugin `AuditLog`.

### Sandbox Security

Each plugin runs with a `PluginSandbox` that enforces:

- **Network allowlist**: Only permitted hosts (exact or wildcard `*.example.com`)
- **SSRF protection**: Private/reserved IPs (127.0.0.0/8, 10.0.0.0/8,
  169.254.0.0/16, etc.) are always blocked
- **Filesystem containment**: File access only within declared paths;
  symlink traversal detected and blocked
- **Environment variable deny list**: `PATH`, `HOME`, `ANTHROPIC_API_KEY`,
  `OPENAI_API_KEY`, etc. are never accessible regardless of allowlist
- **Rate limiting**: Per-plugin HTTP and log rate counters
- **Scheme blocking**: `file://`, `data://`, `ftp://` schemes are blocked

### Binary Size Enforcement

WASM plugin modules must meet size constraints at install/load time:

| Metric | Limit |
|--------|-------|
| Uncompressed | < 300 KB |
| Gzipped | < 120 KB |
| Plugin directory | < 10 MB |

Modules exceeding these limits are rejected at load time with a clear error.

### Audit Logging

Every host function call produces an audit entry recording:

- Function name (e.g., `http-request`, `read-file`)
- Parameters summary
- Whether the call was permitted or denied
- Error message (if denied)
- Duration in milliseconds

The audit log is per-plugin and can be queried for compliance and debugging.

### Multi-Plugin Isolation

Each plugin gets independent:

- `wasmtime::Store` (no shared memory between plugins)
- `PluginSandbox` (separate permission sets)
- `AuditLog` (separate audit trails)
- Rate counters (one plugin's rate limit does not affect others)

---

## Future Roadmap

- **WASI HTTP preview2**: Replace the HTTP stub with real outbound requests
  via `wasi:http/outgoing-handler`, enabling LLM API calls from WASM.
- **WASI filesystem**: Replace the filesystem stub with `wasi:filesystem/types`
  and `wasi:filesystem/preopens` for config and session persistence.
- **Component model**: Package as a WASI component for composable deployment.
