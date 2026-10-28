# Browser Performance Profiling Baseline (WEFT-407)

Captures **load**, **init**, **first-message**, and **memory** metrics for the
browser WASM path (`clawft-wasm` + `www/` harness). Targets come from SPARC
BW6; the measurement surface is shared by the HTML harness and a CI-friendly
script.

| Piece | Path |
|-------|------|
| Metric helpers + schema | [`crates/clawft-wasm/www/perf-baseline.js`](../../crates/clawft-wasm/www/perf-baseline.js) |
| Harness wiring | [`crates/clawft-wasm/www/main.js`](../../crates/clawft-wasm/www/main.js) (`window.__clawftPerf`) |
| Node unit tests | [`crates/clawft-wasm/www/perf-baseline.test.mjs`](../../crates/clawft-wasm/www/perf-baseline.test.mjs) |
| Bench script | [`scripts/bench/browser-perf-baseline.sh`](../../scripts/bench/browser-perf-baseline.sh) |
| Checked-in targets | [`scripts/bench/browser-baseline.json`](../../scripts/bench/browser-baseline.json) |
| Live JSON output | `target/wasm-bench/browser-perf.json` (gitignored) |

Schema id: **`weftos.browser-perf.v1`**.

## Targets (BW6)

| Metric | Target | How measured |
|--------|--------|--------------|
| WASM module load | **&lt; 500 ms** | `performance.now()` around `import("./pkg/clawft_wasm.js")` + wasm-bindgen default init |
| `init(config)` | **&lt; 200 ms** | `performance.now()` around exported `init(config_json)` |
| First message | **&lt; 3 s** | First `send_message` after init (**includes LLM RTT**) |
| Subsequent message | **&lt; 2 s** | Later `send_message` samples (avg) |
| Memory (WASM heap) | **&lt; 32 MB** | `WebAssembly.Memory.buffer.byteLength`; Chrome `performance.memory` when present |

Related (not part of this baseline): OPFS read/write latency (&lt; 20 ms / &lt; 50 ms)
stays with OPFS feature tests; bundle size is gated by
[`scripts/bench/check-bundle-size.sh`](../../scripts/bench/check-bundle-size.sh)
and [architecture/wasm-bundle-size.md](../architecture/wasm-bundle-size.md).

## CI-friendly path (default)

No Chrome, no network LLM, and no built `pkg/` required:

```bash
# Unit tests + stub report (schema-valid, samples null → checks skipped)
scripts/bench/browser-perf-baseline.sh

# Helpers only
scripts/bench/browser-perf-baseline.sh --test-only

# Force stub JSON only
scripts/bench/browser-perf-baseline.sh --stub --json-out /tmp/browser-perf.json
```

Stub mode writes a report with `"mode": "stub"` and null samples. Target
checks for null values are **skipped** (not failed), so CI stays green while
still locking the schema and evaluation logic.

```bash
node --test crates/clawft-wasm/www/perf-baseline.test.mjs
```

## Live capture (local harness)

```bash
scripts/build.sh browser
scripts/build.sh serve          # default :8080
# Open http://localhost:8080
# Click Initialize, send at least one message
```

The harness:

1. Records load / init / message latencies into structured samples.
2. Captures WASM buffer size (and Chrome JS heap when available).
3. Publishes a full report on **`window.__clawftPerf`**.
4. Shows a one-line strip (`#perf-panel`) under the header.

Dump the report:

```js
// DevTools console
copy(JSON.stringify(window.__clawftPerf, null, 2))
```

Save to `target/wasm-bench/browser-perf.json`, then:

```bash
scripts/bench/browser-perf-baseline.sh --check target/wasm-bench/browser-perf.json
```

A complete live report uses `"mode": "live"` after the first successful
message; partial runs (load/init only) use `"mode": "partial"`.

## Report shape (v1)

```json
{
  "schema": "weftos.browser-perf.v1",
  "benchmark": "browser_perf_baseline",
  "mode": "stub",
  "timestamp": "2026-07-31T00:00:00.000Z",
  "targets": {
    "load_ms": 500,
    "init_ms": 200,
    "first_msg_ms": 3000,
    "subsequent_msg_ms": 2000,
    "memory_wasm_heap_mb": 32
  },
  "samples": {
    "load_ms": null,
    "init_ms": null,
    "first_msg_ms": null,
    "subsequent_msg_ms": [],
    "memory": {
      "js_heap_used_mb": null,
      "js_heap_total_mb": null,
      "wasm_buffer_bytes": null,
      "wasm_buffer_mb": null
    }
  },
  "checks": [
    { "name": "load_ms", "value": null, "target": 500, "status": "skip" }
  ],
  "summary": { "pass": 0, "fail": 0, "skip": 5, "ok": true }
}
```

## Interpreting first-message latency

`first_msg_ms` **includes** provider network time. A slow model or cold
OpenRouter route can exceed 3 s even when local WASM work is fast. When
comparing hosts or PRs:

- Prefer a fixed free model and the same region.
- For pure local cost, use `load_ms` + `init_ms` + WASM heap only.
- Worker harness ([web-worker.md](./web-worker.md)) isolates agent work off
  the main thread; main-thread `first-msg` still includes LLM RTT.

## Updating the checked-in baseline

[`scripts/bench/browser-baseline.json`](../../scripts/bench/browser-baseline.json)
holds **targets** and optional numeric baseline slots (currently `null`).
When a representative live run is recorded:

1. Paste samples into `baseline.*` fields.
2. Note date, browser, machine class, and model in a short comment in that
   file or in [benchmarks/results.md](../benchmarks/results.md).
3. Do **not** tighten targets without an ADR / BW6 amendment.

## Related

- [quickstart.md](./quickstart.md) — harness usage
- [building.md](./building.md) — `scripts/build.sh browser` / `serve`
- [web-worker.md](./web-worker.md) — off-main-thread agent (WEFT-400)
- [architecture/wasm-bundle-size.md](../architecture/wasm-bundle-size.md) — size gate
- [benchmarks/results.md](../benchmarks/results.md) — native / WASI numbers
- SPARC BW6: `.planning/sparc/browser/06-phase-BW6-integration.md` (Performance Profiling)
