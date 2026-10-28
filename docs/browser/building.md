# Building clawft-wasm for the Browser

This guide covers how to compile clawft to WebAssembly for use in a web browser.

## Prerequisites

### 1. Rust toolchain

Ensure you have a recent Rust toolchain (1.75+):

```bash
rustup update stable
```

### 2. wasm32-unknown-unknown target

Add the browser WASM compilation target:

```bash
rustup target add wasm32-unknown-unknown
```

### 3. wasm-pack

Install wasm-pack, which handles building, generating JS bindings, and
packaging:

```bash
cargo install wasm-pack
```

Alternatively, use the npm installer:

```bash
npm install -g wasm-pack
```

## Build Commands

### Using the unified build script

The simplest way to build the browser WASM target:

```bash
scripts/build.sh browser             # Build with release-wasm profile
scripts/build.sh browser --dry-run   # Preview the cargo command
```

This runs `cargo build --target wasm32-unknown-unknown -p clawft-wasm --no-default-features --features browser` with the `release-wasm` profile.

### Development build

```bash
cd crates/clawft-wasm
wasm-pack build --target web --no-default-features --features browser -- --no-default-features --features browser
```

The `--target web` flag produces ES module output suitable for `<script type="module">` imports. The `--no-default-features --features browser` arguments are passed both to wasm-pack (for wasm-bindgen) and to cargo (after `--`).

### Release build (optimized)

```bash
cd crates/clawft-wasm
wasm-pack build --target web --release --no-default-features --features browser -- --no-default-features --features browser
```

### Checking compilation without building

To verify the browser feature compiles without producing artifacts:

```bash
cargo check --target wasm32-unknown-unknown -p clawft-wasm --no-default-features --features browser
```

## Output Files

After a successful build, wasm-pack places output in `crates/clawft-wasm/pkg/`:

| File | Description |
|------|-------------|
| `clawft_wasm.js` | ES module with wasm-bindgen glue code. Exports `init()`, `send_message()`, `set_env()`, and a default initializer. |
| `clawft_wasm_bg.wasm` | The compiled WebAssembly binary. Loaded automatically by the JS glue. |
| `clawft_wasm_bg.wasm.d.ts` | TypeScript type declarations for the raw WASM imports. |
| `clawft_wasm.d.ts` | TypeScript type declarations for the public JS API. |
| `package.json` | npm package metadata for the generated module. |

## Regression suite + duration gate (WEFT-408 / P6.7)

```bash
# Headless Chrome suite (writes browser-test-duration.json)
scripts/build.sh test-browser

# ≤10% wall-clock regression vs scripts/bench/browser-test-duration-baseline.json
scripts/bench/check-browser-test-duration.sh browser-test-duration.json
```

CI (`.github/workflows/pr-gates.yml` job `browser-wasm-tests`) uploads the
duration JSON as an artefact, compares against the previous run when a
cache hit is available, and fails when the suite is more than 10% slower.
Re-seed the committed baseline after intentional suite growth:

```bash
BROWSER_DURATION_UPDATE_BASELINE=1 \
  scripts/bench/check-browser-test-duration.sh browser-test-duration.json
```

## Development Workflow

### 1. Build

```bash
cd crates/clawft-wasm
wasm-pack build --target web --no-default-features --features browser -- --no-default-features --features browser
```

### 2. Serve

The test harness is in `crates/clawft-wasm/www/`. Serve it with any static file server. For example:

```bash
# Using Python
python3 -m http.server 8080 --directory crates/clawft-wasm/www

# Using npx
npx serve crates/clawft-wasm/www

# Using cargo
cargo install miniserve
miniserve crates/clawft-wasm/www --port 8080
```

The server must serve files with correct MIME types. In particular, `.wasm` files must be served as `application/wasm`.

### 3. Open

| URL | Harness |
|-----|---------|
| `http://localhost:8080/` | Main-thread UI (`index.html`) |
| `http://localhost:8080/index-worker.html` | **Web Worker** UI (WEFT-400 — agent off main thread) |

Initialize with provider config and chat. Prefer the worker harness when
profiling UI jank or exercising long LLM calls. See
[`web-worker.md`](web-worker.md) for the message protocol and
`ClawftWorkerClient` API.

Protocol unit tests (no browser):

```bash
node --test crates/clawft-wasm/www/protocol.test.mjs
```

Performance baseline helpers + CI stub (WEFT-407 — load / init / first-msg /
memory; no Chrome required):

```bash
scripts/bench/browser-perf-baseline.sh
node --test crates/clawft-wasm/www/perf-baseline.test.mjs
```

See [performance.md](./performance.md) for targets and live harness capture.

### 4. Iterate

After making Rust changes, rebuild with wasm-pack and refresh the browser. The test harness loads WASM from `../pkg/` relative to the HTML file.

## Troubleshooting

**"wasm streaming compile failed"** -- The server is not sending the correct
`Content-Type: application/wasm` header for `.wasm` files. Use a server that
handles WASM MIME types (e.g., `npx serve`).

**"no global fetch available"** -- The WASM module is running outside of a
browser window or web worker context. Ensure you are loading it in a
standard browser environment.

**Large binary size** -- The release build with `wasm-opt` should produce a
binary under 300 KB uncompressed. If the binary is larger, check that
`--no-default-features` is passed so native-only dependencies are excluded.
