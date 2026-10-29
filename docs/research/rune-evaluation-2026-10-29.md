# Rune (ruvnet/rune) evaluation for WeftOS, 2026-10-29

**Verdict: watch, with one cheap pattern-only borrow. Do not adopt, and do not trial in the build path.**

Rune is a thin, unusually honest research prototype. Its own README says most of what is below. It does not touch any of WeftOS's build pain points, and the parts that are real are aimed at a different problem (kernel micro-optimisation by LLM proposal).

Read from a shallow clone of `ruvnet/rune` at `9242551` (2026-10-29). The ruvnet-brain worker was unavailable (initialisation timeout), so every claim below is `[primary: rune/<path>]`. Nothing was built or run, and no install scripts were executed.

## What it is

- **Size and age.** About 16.8k lines of Rust in 9 crates plus about 2.2k in `rune-study`, 211 `#[test]`s. Created 2026-10-26, v0.1.0 and v0.2.0 the same day, 8 commits, 2 authors (both the ruv account), 6 stars. Published on crates.io as `rune-toolchain` 0.2.0 (22 downloads).
- **Licence.** MIT, `LICENSE` copyright "2025 rUv". GitHub's API reports `mit` today, so the "null" in the brief is stale.
- **Shape.** A wrapper around Cargo, not a replacement. `crates/rune-toolchain-cargo/src/lib.rs` shells out to `cargo build --release --locked --offline --message-format=json-render-diagnostics --target-dir <dir>` and hashes one selected artifact. `docs/adr/001-system-boundary.md` frames it as "Cargo compatibility as an adapter" and puts a compiler fork and MIR passes out of scope.
- **Self-description.** The README "Status" section lists what is not done: no MetaHarness or AgenticOW integration, RuVector off by default, no aarch64 hardware, "Large real-world workspace with dependencies and build scripts: Not met" (`docs/VALIDATION.md`).

## Feature status

| Feature | Status | Evidence | Fit for WeftOS |
|---|---|---|---|
| Frozen, content-addressed build plan | Implemented | `rune-toolchain-core` (plans, tree digest, plan store); tests assert `build` never reads credentials | Low. Plan binds one source tree, lockfile and `rustc -vV`; any edit refuses to build until `--freeze` |
| Build (`cargo build --release --locked --offline`) | Implemented, fixture-scale | `rune-toolchain-cargo/src/lib.rs`; parity shown on fixtures and a Rune-workspace snapshot, one selected product | None. Hard-coded flags: no `-p`, `--features`, profile or `--workspace` handling. Selects one artifact, not a workspace |
| Independent verify (clean rebuild, digest compare) | Implemented | `rune verify`; README "Measured results" | Low. A second full build of a 15-35 min crate set doubles cost |
| RuVector memory | Partial | `rune-toolchain-memory`: recipes, fingerprint ranking, approval keys in-tree. `ruvector-core` HNSW is an optional feature, off by default (needs Rust 1.89) | Low. Memory holds optimisation recipes, not build or test history |
| Agent Booster edits | Implemented as a sidecar | `rune-toolchain-edit/src/sidecar.rs` runs `agent-booster@0.2.2` in a Node process; `scripts/booster-smoke.cjs` is a single fixture, `semantic_validation: 'not performed'` | None. We already have Edit and `hooks_codemod` |
| Bounded AI discovery (`rune improve`) | Implemented, narrow | `rune-toolchain/src/propose.rs`: OpenRouter via `ureq`, `data_collection: deny`, `zdr: true`, `allow_fallbacks: false`, budget reserved first; output is data, never applied | Low. Adds a network dependency and a second LLM provider to a cost-routing regime built around local and Claude tiers |
| Independent evaluator | Implemented, Linux-only | `rune-toolchain-eval`: hidden holdout, 30+ paired samples, CI lower bound, static gates for `unsafe`, new deps, `build.rs`, `std::net`. Isolation uses `timeout`, `prlimit`, `unshare`, so it fails closed without them | Pattern value only. Measures microkernel speedups, not workspace correctness. Does not run on macOS (our main dev host) |
| Authenticated promotion | Implemented | ADR 011; Ed25519 receipt plus separately supplied `--trusted-key` | Pattern value. Compare to our owner-run signing |
| Cross-target evidence | Partial | aarch64 under qemu, wasm32-wasip1 under Node WASI, both "emulated", performance claims refused; RVM "unvalidated" | Low. We use wasm32-unknown-unknown and wasip2, not wasip1. Our aarch64 evidence comes from real hardware or a VM |
| Cleanup and GC | Implemented | `rune-toolchain-gc`: ownership manifest, 7-day quarantine | Low. Our worktree and `target/` bloat is handled by agent-git-flow caps |
| "Learning" claim | Scoped, honestly reported | `study/RESULT.md`: 34-kernel control study `Insufficient`. `study-transfer-confirm/RESULT.md`: 53.5% vs 35.5% random over 288 fresh cases, preregistered, but 12 synthetic u16 sorting contexts, harness-supplied features | None. The authors state it "does not establish general compiler improvement" |

The `study*` directories do contain real data: preregistrations with frozen hashes, per-evaluation reports and audit scripts. The weak result (`Insufficient`, `NoDifference`) is published unedited. This is the repo's best quality.

## Cargo compatibility

- **Wrapper, not replacement.** It cannot replace `scripts/build.sh`, which drives native/browser feature mutex handling (ADR-083), wasm32 and wasip2 builds, wasm-bindgen steps, container smoke tests and the 22-23 check `gate`.
- **Offline requirement.** `--offline` needs every dependency already in the local cargo cache or vendored. That works for a warmed cache; a cold worktree would fail until fetched separately.
- **Lockfile and publishing.** Rune neither writes `Cargo.lock` nor touches the registry. `scripts/publish-crates.sh` is unaffected. Rune requires a committed lockfile, which we have.
- **Platform.** Evaluation isolation depends on Linux `prlimit` and `unshare`. On macOS the evaluator refuses to run ("required isolation layers unavailable", `evaluator.rs:213`). Our CI and the PG host are Linux, our authoring machine is not.

## Security and supply chain

- **Network.** The only network call is `rune improve` to OpenRouter, opt-in, with a budget and a credential read from the environment. `build`, `verify` and `eval` do not call out. I found no telemetry. Cargo's own network access is disabled by `--offline`.
- **Auto-editing.** `improve` output is data and is not applied. `rune edit` applies only an explicit, hash-checked edit. Both claims match the code paths.
- **Isolation.** Self-described as "not a hardened sandbox; build scripts and proc-macros are untrusted code". WeftOS has many `build.rs` and proc-macro crates, so this limitation applies fully to us.
- **Node sidecar.** Agent Booster pulls an npm dependency (`package.json` pins 0.2.2). A Node supply-chain surface for a tool we do not need.
- **"Independently validated builds".** This means rebuilding in a clean directory and comparing digests, plus Ed25519-signed evaluator receipts verified against a key you supply. It attests that a build reproduces, not that it is correct. It does not fit our rule of owner-run release signing: Rune's flow expects a signing key on an evaluator runner (`rune eval --signing-key`), which agents must never hold. Reproducible-digest checking is useful, but we can get it with `cargo build --locked` twice.

## Maturity

- **Bus factor.** One author account; 3 days old. API surface is explicitly an "Architecture preview". Expect churn: 0.1.0 to 0.2.0 in one day with a promotion-trust redesign (ADR 011) landing after.
- **Relationship to other ruv repos.** Consumes `agent-booster` (agentic-flow), optionally `ruvector-core`; MetaHarness and AgenticOW are named but not integrated.
- **CI.** fmt, clippy `-D warnings`, tests, MSRV 1.88, `cargo audit` and `cargo deny` with a licence allowlist. Reasonable hygiene.

## Fit against WeftOS pain points

| Pain point | Does Rune help? |
|---|---|
| Per-worktree cold builds | No. Rune builds into its own target dir with plain cargo and adds no cache. sccache plus agent-git-flow already cover this |
| 15-35 min crate builds | No. It does not change rustc invocation, codegen or incrementality |
| Flaky tests under load | No. Rune has no test orchestration for a workspace; its evaluator runs fixture tests in isolation |
| Agents must use `build.sh` | Would conflict. Rune would be a second build entry point |
| Cheap models with mechanical verification | Pattern match only. Its hidden-holdout plus gate-before-build design is a good template for checking cheap-model output |

## Recommendation: watch, plus a pattern-only borrow

**Not adopt now.** It is not a build tool for a workspace like ours, it does not address our slowness, and it is three days old.

**Not a trial in a lane.** There is no lane where it plugs in without replacing `build.sh` behaviour it cannot reproduce (features, targets, mutex, wasm-bindgen).

**Borrow (cheap).** The candidate-gate pattern: statically reject cheap-model diffs that add `unsafe`, new dependencies, `build.rs` or proc-macros, `std::net` or `std::process` before compiling them. That is a ~50-line check in `scripts/` and fits the cheap-model routing doc. Source: `rune-toolchain-eval` static gates; read before reimplementing, no dependency needed.

**First step.** Re-check in about 8 weeks (or when an issue or release shows a real workspace build with features and build scripts). Meanwhile, implement the static diff gate for the Qwen lane.

**Revisit criteria.** Adopt for a trial only if all hold:
1. It builds our workspace with `-p`, features and wasm targets and produces artifacts identical to `scripts/build.sh native`.
2. Evaluator isolation works on macOS or runs only in Linux CI.
3. At least two release lines with a stable API and a second maintainer.
4. A published result on a real workload, not a synthetic sorting study.

**Strongest non-ruv alternative.** The boring stack we already run: `cargo build --locked`, sccache, `cargo-deny`, `cargo-audit`, `cargo-vet` or `cargo-semver-checks`, and criterion for benchmark gating. They solve build speed and supply chain with mature tooling.

## Spike testability (commit `9242551`)

Install and run, as documented in `README.md`:

```sh
# Prerequisite: Rust 1.88+ (we pin 1.95). Linux is needed for isolation (prlimit, unshare).
cargo install rune-toolchain --locked        # or: cargo build --release from a clone; binary is `rune`
cd <workspace>                                # needs Cargo.toml and a committed Cargo.lock; deps already in the cargo cache
rune build --freeze                           # first approval of a plan
rune build ; rune verify ; rune explain ; rune rollback
rune -C <workspace> ...                       # -C selects the workspace
```

The CLI has no `check`, `test`, `doctor` or `affected` command. It exposes `build`, `verify`, `improve`, `edit`, `eval`, `recipes`, `promote`, `target`, `package`, `verify-artifact`, `cleanup`, `explain` and `rollback`. `build` writes state under `.rune/` inside the workspace.

| Task | Testable? | Status and notes |
|---|---|---|
| T1 install | Yes | Implemented. No `doctor`. Optional `npm install` of `agent-booster@0.2.2` (Node sidecar) for T7 |
| T2 cold check | No | No check mode. Only `cargo build --release --locked --offline` of one selected product. A comparable test is a release build, not `build.sh check` |
| T3 warm rebuild | Partial | `rune build` refuses after any source edit (`stale plan`) until `--freeze`, so measure freeze+build vs `build.sh`. Needs its own target dir; no sccache hook, so set `RUSTC_WRAPPER` in the environment and see whether it passes through |
| T4 crate tests | No | No test command. The evaluator runs only contract fixtures, not workspace tests |
| T5 wasm | No for our targets | Only `wasm32-wasip1` (Node WASI, emulated) and aarch64 (qemu). No `--features`/`--no-default-features`, no wasm32-unknown-unknown or wasip2 flow. Cannot respect the ADR-083 mutex because it passes no feature flags. Expect a failed or wrong build; record as the finding |
| T6 affected | No | Documented-only/absent: no selection logic |
| T7 Agent Booster | Yes, narrowly | Implemented: `rune edit --file F --sha256 H --find ... --booster [--node-modules DIR] [--language rust]`. One file per call, hash-checked, exact-find. Local; needs Node and the npm package. Drive it from a loop for ~20 files. No semantic validation (`booster-smoke.cjs` says so). Baseline: `hooks_codemod` or sed |
| T8 RuVector memory | No (cannot observe effect on builds) | Memory exists for recipes only (`rune recipes add/rank/validate/snapshot`). The ruvector HNSW ranking is behind an off-by-default feature needing Rust 1.89. Running T3 twice teaches it nothing. Mark n/a and put it in feedback |
| T9 bounded discovery | Stub-level for compile fixes | `rune improve` sends one budgeted prompt for a kernel optimisation proposal, not compile-error repair. It needs `OPENROUTER_API_KEY` and sends source to openrouter.ai. Per the plan's rule, record and stop. `RUNE_OPENROUTER_URL` can point it at a local mock to inspect the request body without a key leaving the machine |
| T10 validated build | Yes, partially | Implemented: `rune verify` (clean rebuild, digest compare), `rune package --signing-key FILE` and `rune verify-artifact --trusted-key FILE`. Use a throwaway test key generated outside the repo; Rune never generates keys. This proves reproducibility and provenance, not correctness, and does not replace owner-run release signing |

Anything that needs a key or leaves the machine: `rune improve` only (`OPENROUTER_API_KEY`, openrouter.ai, sends source text). `build`, `verify`, `edit` (exact replace or local booster), `eval`, `target`, `package`, `cleanup` are local. Cargo itself stays offline. The booster path runs Node with the pinned npm package; read `scripts/` and `package-lock.json` first and install with `npm ci --ignore-scripts`.

Net: of T1-T10, T1, T3 (partial), T7 and T10 are runnable; T2, T4, T5, T6 are structurally unsupported and become findings; T8 and T9 should be n/a or run only against a local mock.
