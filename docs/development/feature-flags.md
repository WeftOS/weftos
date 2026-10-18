# Feature Flags (Cargo)

**WEFT-394.** Compile-time feature flags control native vs browser/WASM
capability, optional heavy deps (voice/ONNX, DiskANN, channels), and
kernel subsystems. This page is the developer reference for real
`[features]` entries in workspace `Cargo.toml` files. Prefer
`scripts/build.sh` over raw `cargo` for builds and tests.

> Source of truth is each crate’s `Cargo.toml` under `[features]`.
> If this doc and a crate disagree, trust the crate and update this file.

Related:

- Product/install oriented catalog: [`docs/weftos/feature-flags.md`](../weftos/feature-flags.md)
  (may lag; verify against `Cargo.toml`)
- Build entrypoints: `scripts/build.sh --help`
- Browser WASM size budgets: [`docs/architecture/wasm-bundle-size.md`](../architecture/wasm-bundle-size.md)

---

## Native vs browser at a glance

Most platform-facing crates use a **mutually exclusive pair**:

| Feature | Typical meaning |
|---------|-----------------|
| `native` (often **default**) | Tokio runtime, `dirs`, `reqwest` / native TLS, filesystem watchers, daemon-only seams |
| `browser` | `wasm-bindgen` / `web-sys` / `js-sys`, JS UUID/getrandom, Fetch-style HTTP, no shell exec / subagent spawn |

**Rule:** for `wasm32-unknown-unknown`, disable defaults and enable the
browser chain explicitly:

```bash
# Preferred
scripts/build.sh browser
# Extra (e.g. OPFS persistence — WEFT-13/14/392)
scripts/build.sh browser --features browser-opfs

# Equivalent cargo shape used by build.sh
cargo build --target wasm32-unknown-unknown -p clawft-wasm \
  --no-default-features --features browser
```

Native CLI (defaults already include `native` on library crates):

```bash
scripts/build.sh native
scripts/build.sh native --features voice-onnx   # common host config
scripts/build.sh install --features voice-onnx
```

### Propagation chain (browser)

When `clawft-wasm` enables `browser`, it pulls the same flag on
dependents (and those crates pull their own browser deps):

```
clawft-wasm/browser
  → clawft-core/browser
  → clawft-llm/browser
  → clawft-tools/browser
  → clawft-platform/browser
  → clawft-types/browser
```

`browser-opfs` additionally enables:

```
clawft-wasm/browser-opfs
  → clawft-platform/browser-opfs
  → clawft-tools/browser-opfs
```

Native defaults walk the opposite direction (`native` on types → platform →
core → tools → plugin → kernel).

### What browser builds intentionally omit

| Capability | Native feature / crate | Browser |
|------------|------------------------|---------|
| Shell / process exec | `clawft-tools/native-exec` | off |
| Subagent spawn tools | `clawft-tools/subagent` (implies `native`) | off |
| Real BPE tokenizer | `clawft-core/real-tokenizer` (implied by `native`) | char/word estimator |
| File watchers / dirs home | `notify`, `dirs` via `native` | OPFS or in-memory FS |
| DiskANN / sona / most kernel mesh | kernel features | not in browser WASM package |
| cpal / real mic | `voice-real-audio`, `live-audio` | not used |

Kernel WASM gate (WEFT-114): `scripts/build.sh check` also runs

```bash
cargo check -p clawft-kernel --target wasm32-unknown-unknown --no-default-features
```

so non-browser code cannot creep into the kernel default feature set.

### ECC exclusion on `wasm32-unknown-unknown` (WEFT-504)

The kernel `ecc` feature enables native-only deps (`blake3`,
`clawft-core/vector-memory`, `clawft-bvh`). It is **not** supported on
`wasm32-unknown-unknown`:

| Surface | Behavior |
|---------|----------|
| `scripts/build.sh browser` | Builds `clawft-wasm` with `--features browser` only — never enables `ecc` |
| `clawft-kernel` + `ecc` on wasm32-unknown-unknown | Hard `compile_error!` (WEFT-504) + native dep failures |
| `scripts/build.sh check` | After the WEFT-114 no-mesh check, asserts `--features ecc` **fails** on that target |

Do **not** pass `--features ecc` into browser/WASI package builds.

---

## How to pass features

Always go through `scripts/build.sh` when possible:

```bash
scripts/build.sh native --features voice-onnx
scripts/build.sh native-debug --features voice
scripts/build.sh install --features voice-onnx
scripts/build.sh test --features email,matrix
scripts/build.sh browser --features browser-opfs
scripts/build.sh test-browser                        # FEATURES=browser (default)
FEATURES=browser-opfs scripts/build.sh test-browser  # also OPFS tests
scripts/build.sh gui-egui                            # clawft-gui-egui + native
scripts/build.sh bench clawft-kernel vector_backend_bench --features diskann
```

`--features` is forwarded as Cargo’s `--features <list>` (comma-separated).
`BUILD_FEATURES` is stamped into `--version` (WEFT-656) so
`scripts/build.sh install` can refuse a silent feature downgrade.

For package-scoped work:

```bash
cargo check -p clawft-channels --features signal
cargo test -p clawft-gui-egui --features live-smoke -- --ignored
cargo build -p weftos --features full
```

---

## Platform dual-stack crates

### `clawft-types`

| Feature | Default | Enables |
|---------|---------|---------|
| `native` | **yes** | `dirs` (home directory) |
| `browser` | no | `uuid/js`, `getrandom` for WASM |

### `clawft-platform`

| Feature | Default | Enables |
|---------|---------|---------|
| `native` | **yes** | tokio, reqwest, dirs, `clawft-types/native` |
| `browser` | no | wasm-bindgen stack, web-sys, js-sys, getrandom, `clawft-types/browser` |
| `browser-opfs` | no | Origin Private File System backend for `BrowserFileSystem` (implies `browser`; runtime falls back to memory if OPFS unavailable) |

### `clawft-llm`

| Feature | Default | Enables |
|---------|---------|---------|
| `native` | **yes** | reqwest + rustls-tls, tokio, `clawft-types/native` |
| `browser` | no | `clawft-types/browser`, `gloo-timers` (wall-clock sleep on wasm32; WEFT-390) |

### `clawft-core`

| Feature | Default | Enables |
|---------|---------|---------|
| `full` | **yes** | `rvf` + `signing` + `vector-memory` + `embedding-router` |
| `native` | **yes** | notify, dirs, tokio, service-llm, `real-tokenizer`, platform/llm/plugin/types `native` |
| `browser` | no | platform + llm `browser`, wasm-bindgen-futures, js-sys, futures-channel |
| `real-tokenizer` | via `native` | tiktoken-rs + tokenizers (BPE); browser omits → heuristic tokenizer |
| `vector-memory` | via `full` | rand, instant-distance |
| `rvf` | via `full` | rvf-runtime/types, reqwest, libc (macOS shim), clawft-cow-memory; implies `vector-memory` |
| `signing` | via `full` | ed25519-dalek, rand |
| `embedding-router` | via `full` | ruvector-diskann; implies `vector-memory` |
| `hybrid-rerank` | **opt-in** | ruvector-sona skill rerank (not in `full`) |

### `clawft-tools`

| Feature | Default | Enables |
|---------|---------|---------|
| `native-exec` | **yes** | native shell command execution |
| `native` | **yes** | tokio + platform/core/types `native` |
| `subagent` | **yes** | agent_spawn / task_* tools; implies `native` (daemon-only) |
| `browser` | no | platform + core `browser` |
| `browser-opfs` | no | platform/tools OPFS path (implies `browser`) |
| `canvas` | no | canvas marker |
| `vector-memory` | no | `clawft-core/vector-memory` |
| `delegate` | no | `clawft-services/delegate` |
| `voice` | no | live STT/TTS stack via channels + reqwest + symphonia decode |

### `clawft-plugin`

| Feature | Default | Enables |
|---------|---------|---------|
| `native` | **yes** | tokio-util (cancellation) |
| `voice` | no | umbrella: vad + wake + stt + tts + tokio |
| `voice-stt` / `voice-tts` / `voice-vad` / `voice-wake` | no | individual markers |
| `voice-wake-rustpotter` | no | reserved fail-closed path (no rustpotter dep; WEFT-216) |

### `clawft-wasm`

Default features: **none** (must pass `browser` for web).

Browser / wasip2 **entrypoint only** (WEFT-398). Native wasmtime host code
lives in `clawft-wasm-host`.

| Feature | Enables |
|---------|---------|
| `browser` | clawft-core/llm/tools/platform + wasm-bindgen glue; propagates `*/browser` |
| `browser-opfs` | OPFS persistence for BrowserFileSystem + BrowserEnvironment (implies `browser`) |
| `wasm-plugins` | **compat re-export** of `clawft-wasm-host` modules (`sandbox`, `engine`, `audit`, `permission_store`). Prefer depending on `clawft-wasm-host` directly. |
| `alloc-talc` / `alloc-lol` | alternate WASM allocators |
| `alloc-tracing` | allocation tracing marker |

### `clawft-wasm-host`

Native **wasmtime** plugin host (sandbox, engine, audit, permission store).
No Cargo features required — the crate is host-only and is **not** part of
the browser `cdylib` bundle. Build/test with:

```bash
cargo test -p clawft-wasm-host
# Historical re-export path still works:
cargo test -p clawft-wasm --features wasm-plugins
```

### `clawft-gui-egui`

| Feature | Default | Enables |
|---------|---------|---------|
| `native` | **yes** | keeps native bins/tests out of the wasm panel build |
| `audio` | no | rodio → cpal boot sounds |
| `serde` | no | declared only to silence vendored egui demo `cfg_attr` (do not enable) |
| `live-smoke` | no | ignored live smoke against daemon + llama-server |

---

## CLI and top-level binaries

### `clawft-cli` (`weft` binary)

| Feature | Default | Enables |
|---------|---------|---------|
| `channels` | **yes** | `clawft-channels` |
| `services` | **yes** | `clawft-services` |
| `delegate` | **yes** | services + tools `delegate` |
| `api` | **yes** | `clawft-services/api` (axum HTTP API) |
| `vector-memory` | no | `clawft-core/vector-memory` |
| `voice` | no | tools/plugin voice + `clawft-voice-talk` + `live-audio` |
| `voice-onnx` | no | `voice` + talk-path ONNX inference (`ort`) |

### `weftos`

Delegates to `clawft-kernel` (and tokio/clap for native):

| Feature | Default | Enables |
|---------|---------|---------|
| `native` | **yes** | kernel native + tokio + clap + tracing-subscriber |
| `exochain` / `cluster` / `mesh` / `ecc` | no | kernel counterparts |
| `wasm-sandbox` / `containers` / `os-patterns` | no | kernel counterparts |
| `full` | no | all of the above including `native` |

### `clawft-weave` (`weaver` binary)

| Feature | Default | Enables |
|---------|---------|---------|
| `cluster` / `ecc` / `exochain` / `mesh` | **yes** | kernel feature passthrough |
| `rvf-rpc` | no | weftos-rvf-wire + rvf-types |

> **Removed (WEFT-497):** `agent-core-chat` was an empty default-on
> cutover toggle left after agent-core-v1 Phase D3. Spike path already
> deleted in D3 (`0dd28b49`); flag had no `#[cfg]` gates and was
> removed after soak on 0.8.x.

---

## Kernel (`clawft-kernel`)

Default: `native`, `exochain`, `cluster`, `tilezero`, `ecc`, `mesh`.

| Feature | Notes |
|---------|--------|
| `native` | tokio stack + blake3 + core/platform/types `native` |
| `exochain` | exo-resource-tree, rvf crypto/wire/types/runtime, ed25519, ciborium |
| `tilezero` | cognitum-gate-tilezero; **implies `exochain`** |
| `cluster` | ruvector-cluster/raft/replication, parking_lot |
| `mesh` | WebSocket mesh (tokio-tungstenite, snow, ed25519) |
| `mesh-rvf` | experimental RVF KernelMessage encoding surface (implies `mesh`; encode path not fully landed) |
| `ecc` | cognitive substrate; pulls `clawft-core/vector-memory` + blake3 |
| `quantum-pasqal` / `quantum-braket` | experimental stubs; imply `ecc` + reqwest |
| `diskann` | real DiskANN backend; implies `ecc`. **Without it, hybrid/diskann vector config degrades to brute-force stub** (kernel warns; `vector.strict` errors) |
| `onnx-embeddings` | ort + ndarray + tokenizers + half |
| `wasm-sandbox` | wasmtime + wasi |
| `os-patterns` | implies `exochain` |
| `treesitter` | `clawft-plugin-treesitter` |
| `sensor` / `containers` / `http-api` | markers (http-api used by `clawft-services/api`) |

Dependency sketch:

```
tilezero ──► exochain
os-patterns ──► exochain
diskann ──► ecc
quantum-* ──► ecc
mesh-rvf ──► mesh
```

---

## Channels, services, graphify

### `clawft-channels`

Default: `native` → `clawft-plugin/native`.

| Feature | Deps / notes |
|---------|----------------|
| `email` | imap, lettre, mailparse, native-tls |
| `whatsapp` | axum, hex |
| `signal` | ensures `clawft-plugin/native` |
| `matrix` | marker |
| `google-chat` | base64 |
| `irc` | tokio-rustls, webpki-roots, rustls-pki-types, base64 |
| `teams` | axum, jsonwebtoken |
| `voice` | rustfft; **fake audio by default** (CI-friendly) |
| `voice-real-audio` | voice + cpal |
| `real-audio-test` | voice-real-audio + device tests |
| `voice-xai` | xAI Realtime WS + tool bridge (implies `voice`) |
| `diarization-sherpa` | reserved (implies `voice`; not staged) |
| `discord-voice` | Discord VC bridge scaffold (implies `voice`) |

### `clawft-services`

Default: **none**.

| Feature | Enables |
|---------|---------|
| `delegate` | regex routing |
| `rvf` / `test-utils` / `clawhub` | markers |
| `api` | axum + tower-http + core/platform/kernel; enables `clawft-kernel/http-api` |

### `clawft-graphify`

Default: `code-domain`, `rdf-ingest`.

| Feature | Notes |
|---------|--------|
| `ast-extract` | tree-sitter |
| `lang-*` / `lang-all` | per-language grammars (python, js, ts, rust, go, java, c, cpp, ruby, csharp) |
| `semantic-extract` / `vision-extract` / `forensic-domain` | markers |
| `html-export` / `neo4j-export` / `mcp` | export / MCP server |
| `kernel-bridge` | clawft-kernel + async-trait |
| `http-client` | real reqwest (production weaver; tests use stubs) |
| `rdf-ingest` | oxrdfio + oxrdf (default on) |
| `full` | all extractors, langs, export, kernel-bridge, http-client, rdf, mcp |

### `clawft-plugin-treesitter`

Default empty. Grammars: `rust`, `typescript`, `python`, `javascript`.

---

## Voice stack (native-heavy)

Default voice crates keep **weights and device I/O off** so workspace
builds stay hermetic.

| Crate | Features |
|-------|----------|
| `clawft-voice-onnx` | `onnx` → `ort` |
| `clawft-voice-tts` | `onnx` → `ort` (SNAC + Kokoro) |
| `clawft-voice-aec` | `bridge` (cpal stdio), `webrtc-aec` (libwebrtc APM), `device` (cpal + channels voice seams) |
| `clawft-voice-talk` | `webrtc-aec`, `onnx` (tts+onnx), `live-audio` → aec `device` |

CLI path: `--features voice` assembles the graph with cpal live-audio;
`--features voice-onnx` adds real STT/TTS/speaker inference. Without
`onnx`, the graph constructs and degrades gracefully.

Channels path: `--features voice` is trait-injected with fake sources;
add `voice-real-audio` for cpal.

---

## Leaf / embedded / misc

| Crate | Features |
|-------|----------|
| `weftos-leaf-types` | `std` → serde/ciborium std |
| `weftos-leaf-scene` | `std` → serde/ciborium std |
| `weftos-leaf-renderer` | `std` → scene/std |
| `weftos-leaf-display` | `std` → types/std (host `SimSurface`) |
| `weftos-leaf-sim` | `window` → SDL2 via embedded-graphics-simulator |
| `lgfx-bus-rgb-rs` | `double-buffer` (**default**) — dual PSRAM framebuffers |
| `eml-core` | `experimental-attention` |
| `clawft-treecalc` | `serde` |

Archive plugins under `crates/archive/*` declare empty `default = []`
only and are not part of the active product surface.

---

## Recommended combinations

### Fast native dev

```bash
scripts/build.sh native-debug
# library crates: default native; CLI: channels, services, delegate, api
```

### Host “working voice” install

```bash
scripts/build.sh install --features voice-onnx
```

### Kernel-ish single node (via weftos package)

```bash
cargo build -p weftos --features native,ecc,exochain,os-patterns
# or
cargo build -p weftos --features full
```

Note: `scripts/build.sh native` builds **`weft` + `weaver`** (cli + weave),
not the `weftos` package. Pass features that those package graphs understand
(e.g. `voice-onnx` on cli; weave defaults already enable cluster/ecc/exochain/mesh).

### Browser agent WASM

```bash
scripts/build.sh browser
scripts/build.sh browser --features browser-opfs
scripts/build.sh test-browser
scripts/build.sh bundle-size
```

### Channels adapters in CI/local tests

```bash
scripts/build.sh test --features email,irc,teams,voice
```

---

## K-phase → kernel features

| Phase | Features |
|-------|----------|
| K0–K2 boot / supervisor / IPC | `native` |
| K3 WASM sandbox | `native`, `wasm-sandbox` |
| K3c ECC | `native`, `ecc` |
| K4 containers | `native`, `containers`, `exochain` |
| K5 apps / audit | `native`, `exochain` |
| K6 mesh | `native`, `mesh`, `cluster` |
| OS patterns | `native`, `exochain`, `os-patterns` |
| TileZero gate | `native`, `tilezero` (⇒ exochain) |

---

## Adding or changing a feature

1. Edit the owning crate’s `Cargo.toml` `[features]` and optional deps.
2. Gate code with `#[cfg(feature = "…")]` (or `cfg_attr`).
3. Propagate through dual-stack crates when the flag is platform-shaped
   (`native` / `browser` / `browser-opfs`).
4. Wire `scripts/build.sh` only if a new build profile or default
   combination is needed.
5. Update **this file** and, if user-facing, `docs/weftos/feature-flags.md`.
6. Run:

```bash
scripts/build.sh check
scripts/build.sh test   # or scoped package tests
# browser path when touching wasm dual-stack:
scripts/build.sh browser
scripts/build.sh check  # includes kernel wasm32 --no-default-features
```

---

## Crates without feature flags

Representative active crates with no `[features]` section (or only empty
defaults in archive): `clawft-rpc`, `clawft-security`, `clawft-surface`,
`clawft-substrate`, `clawft-canon`, `clawft-app`, `clawft-cow-memory`,
`exo-resource-tree`, service crates (`clawft-service-llm`,
`clawft-service-whisper`, …), and most leaf hardware crates without a
`std` gate.

---

## See also

- [`docs/development/contributing.md`](contributing.md)
- [`docs/browser/building.md`](../browser/building.md)
- [`docs/architecture/wasm-bundle-size.md`](../architecture/wasm-bundle-size.md)
- `scripts/build.sh --help`
