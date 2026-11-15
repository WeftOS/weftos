# Brain · 04 — Bugs, Gaps & Current State

> Known defects and live working-tree state. Source-of-truth: `.planning/reviews/`,
> `.planning/sonobuoy/GAPS.md`, `docs/handoff.md`, `git status`. Compiled 2026-06-28.
> Updated 2026-11-14: item 7 (LeWM) rewritten to the current state.

## 0. Reconciliation update — 2026-06-28

The sections below were first compiled from the (stale) 2026-04-28 audit. After
landing the working tree (7 commits, tree now clean) the bug list was
reconciled against code:

- **BUG-7 (auth governance gate) — ALREADY FIXED** in `1b02f822` (WEFT-98/102),
  before this brain was written. `auth_service.rs` gates all five mutating ops
  (`register_credential`, `rotate_credential`, `request_token`, `revoke_token`,
  `register_hashed_credential`) on governance with a passing test. Section 1's
  BUG-7 entry is **historical** — no action.
- **The `.env`-shadows-`[kernel.llm]` trap (BUG-8) — ADDRESSED** in `a9ed9179`:
  `daemon.rs` now logs `"llm endpoint resolved"` with `url_source`/`model_source`
  and warns when env shadows config. Precedence unchanged (observability only).
- **Still open**: BUG-1 (leaf-display double-buffer), BUG-3 (daemon chain
  bridge), and ADR-057 (substrate read ACLs, unimplemented release-blocker).
- **Working tree is clean** — the §4 "uncommitted-work hazard" is resolved.

**Lesson encoded in memory**: audit-era bug lists go stale; verify against
current code before fixing (we nearly "re-fixed" already-correct auth gates).

## 0b. Reconciliation update — 2026-07-30 (infrastructure)

A session that touched no product code and instead fixed the machinery that
reports whether product code is correct. It was lying in three ways.

- **The AgentDB memory store was SPLIT.** `.swarm/memory.db` (188 rows,
  2026-07-03, no `provenance_type`) vs `.swarm/agentdb-memory.db` (live, has it
  per ADR-323). The store had been recreated under a new filename for the
  provenance migration and the old rows were never carried over — so
  `clawft-knowledge`, `ruv/brain` and every `weftos/*` namespace were
  **unretrievable by any agent**. REPAIRED: 182 active rows migrated via the MCP
  `memory_import` tool, namespaces preserved, content byte-verified 182/182
  sha256. Store now 232 entries, 100% embedding coverage. WEFT-669 (Done).
- **ROOT CAUSE of that split** (WEFT-684): `.mcp.json` is **tracked in git** and
  said `@claude-flow/cli@latest`, so the process owning the store resolved a
  different version per machine, per day. Nobody chose the schema migration;
  npx took it. Now pinned to `3.32.38`, with a machine-local override in
  `~/.claude.json` for the local grok build.
- **The CI hard gates had NEVER RUN** (WEFT-674). `pr-gates.yml` was
  pull_request-only; work lands on feature branches with no PR, and master is a
  month stale — `gh run list` showed zero runs across 211 commits. Fixed with
  push triggers + `workflow_dispatch`, and a concurrency-group bug that the new
  trigger would itself have introduced (`github.head_ref` is empty on push).
  Two further workflows had never run on ANY event (`benchmarks.yml`,
  `wasm-build.yml` targeted a `main` branch that does not exist), and
  `release-gate.yml` was malformed YAML that GitHub could not load.
- **First gate run exposed 9 failing jobs**, only 3 of them known. Newly
  visible: VSCode panel build, UI lint/type-check, Docs site build,
  weftos-design audit, Binary size check, Cargo audit. None have tickets yet.
- **Two tickets were false Dones.** WEFT-154 (Email) and WEFT-159 (Matrix) —
  both adapters still carry `// ! # WARNING: Planning stub -- does NOT transmit
  messages.`, with no imap/lettre and no matrix-sdk dependency. Reopened. A
  recurrence of the 0.7.0 audit's "channels 9/9 ≠ reality". Now bounded: all 12
  adapters audited, exactly these 2 are stubs.
- **Browser WASM: exactly 15 errors** — 10 non-Send futures (WEFT-663, original
  count *unchanged*), 4 cascading E0282, 1 E0433 `hermes` unresolved (WEFT-672,
  a separate newer break). An earlier "rotted 10→16" claim was a miscount of
  cargo's summary line.
- **ADR-031 drift** (WEFT-683): Accepted, declares RVF the production default
  for `KernelMessage`; `mesh_ipc.rs:47` unconditionally does `serde_json::to_vec`
  and no RVF path exists. The living-ADR rule says amend or build; leaving it
  Accepted-and-unimplemented is the forbidden outcome.
- **Plane reconciled**: 659 → 676+ items, label violations 69 → 0,
  `ws18-firmware` created, cycle `0.8.x` is the live gate (0.7.x is completed
  and hard-locked; it holds 129 Done + 8 Cancelled + **0 open** — nothing was
  ever stranded there, contrary to older notes).

Full resume context: `docs/handoff-tracker-ci-memory.md`.

## 0c. Research capture — 2026-09-11 (RMM / MEMORY.md dump)

Long-term memory inject is still the nanobot dump: `ContextBuilder` labels the
**entire** `MEMORY.md` as `# Relevant Memory:` every turn. WEFT-665 already
showed graft debris in that file is re-injected and hallucinated. Google Cloud
AI Research RMM (Tan et al., ACL 2025, arXiv:2503.08026) is the landing:
frozen retriever Top-K → SONA citation-rerank Top-M → session-end topic
merge/insert. Canonical note
`docs/research/rmm-reflective-memory-management.md`. Plane **WEFT-732** (retrospective) then **WEFT-733**
(prospective). Do not RL-update the embedder; do not
treat tweet 43/37/28% figures as paper results.

## 1. Known bugs / defects

| ID | Severity | Summary | Status |
|---|---|---|---|
| BUG-1 | HIGH | Leaf-display: gutter coords land in wire (Q24.8 x=12800=50px confirmed, drawn=9) but panel doesn't move. Prime suspect: `lgfx-bus-rgb-rs` v0.2.1 double-buffer swap presents wrong buffer. | Open — cheap fix (disable `double-buffer` feature) NOT yet run |
| BUG-2 | MEDIUM | Tofu sidebar icons | Believed fixed at HEAD (`912e9394`, Dingbats), not re-verified on dirty tree |
| BUG-3 | HIGH | ExoChain tracing→ChainManager bridge missing in daemon (`clawft-weave/main.rs`): 12 chain events from non-kernel crates hit stdout, never reach ExoChain | NOT fixed (RSS + chain.tail mapping fixed separately) |
| BUG-4 | LOW | clawft-rpc: 2 env-bound test flakes when a weaver daemon is already running | Pre-existing |
| BUG-5 | LOW | 2 `embedding_onnx` wordpiece tests flake under parallelism (1937/1939 pass) | Pre-existing |
| BUG-6 | LOW | DEMOCRITUS "Stuck { net_change: 0.0 }" spam on idle/empty causal graph — expected but undocumented | Hardened v0.6.19; doc bug |
| **BUG-7** | **CRITICAL** | `auth_service.rs` `rotate_credential` (L325) + `request_token` (L354) chain-log but **do not gate on governance approval** — only CRITICAL FAILs in the 21-item audit | **Open** since 2026-04-28, no fix on this branch |
| BUG-8 | MEDIUM | `.env` shadows `[kernel.llm]`: dotenvy loads LLM_SERVICE_URL/MODEL before weave.toml; stale values silently win; boot log doesn't say which layer won | Not fixed |
| BUG-9 | MEDIUM | EML `from_causal_graph` is O(n+m) (calls connected_components), not advertised O(1) | Open |

## 2. Audit & review findings

**Phase 3 review cycle (3d–3i, consensus.md)** — all 7 P0s resolved 2026-02-17
(ruvector Sprint-0 validation PASS with 4 plan corrections; pluggable ToolProvider
MCP architecture; 3-level discovery chain; WASM cdylib baseline 57.9 KB/24.3 KB
gzip; 3I re-baselined 12→8 P0s; 3D deferred to Phase 4). Open P1 items remained
in opt-level mismatch, YAML skill security, normalize_keys before merge, MCP
allowed_tools access control, SSE streaming.

**0.7.0 release-gate audit (2026-04-28, 17 workstreams)** — key CRITICAL/HIGH
findings, all open at audit time:
1. 7 of 11 channel adapters were stubs (email/google_chat/teams/whatsapp/signal/
   matrix/irc returned synthetic IDs) — *partially closed by WEFT-154..164 since*.
2. W-BROWSER pipeline not wired (browser_entry bypasses the 6-stage pipeline).
3. Multi-agent WS-07: 14/14 "done" but type-level scaffolding only; FlowDelegator
   never created; AgentRouter stored but not consulted; no recursive-delegation
   guard — *partially closed by WEFT-178/180/184 since*.
4. Voice WS-10: 5 P0 security controls unimplemented; no mic→TTS path — *closed by
   M5 / WEFT-555..557 since*.
5. Internal-dep drift: workspace 0.6.19 but path-deps pinned 0.6.6; stale
   `ghcr.io/clawft/clawft` paths.
6. KG-011/012 blocked on ruvllm-wasm 11-pattern HNSW cap; DiskANN backend is a
   HashMap linear-scan stub.
7. JEPA/LeWM world-model, state on 2026-11-14:
   - The crates exist on master; the old "7 crates don't exist" claim is stale.
   - No ML weights ship; the candle skeleton returns Unavailable until weights load.
   - Both optimisers are residual-mean stubs, not neural training.
   - `SensorClass` lacks the real sensors (mmWave, UWB, ToF, Wi-Fi CSI, readings); being added.
   - No dataset archiver exists yet; being added.
   - No recorded dataset exists; W2 is blocked on hardware and a recording session.
8. Auth middleware exists but NOT wired into the Axum router (D-7).

**cargo audit (2026-04-28)** — 21 finding rows / 18 advisory IDs, all transitive:
Wasmtime 33.0.2 → 15 advisories incl. aarch64 Cranelift sandbox escape
(RUSTSEC-2026-0096) and Winch sandbox escape (-0095) [WEFT-551]; rustls-webpki
name-constraint bypass + CRL panic [WEFT-552]; unmaintained bincode/instant/paste/
rustls-pemfile/serial + unsound rand [WEFT-553]. All in `--ignore` list; gate stays
green; deferred to 0.8.x by design.

## 3. Gaps

**Sonobuoy GAPS.md** — all 5 research gaps (G1–G5) CLOSED by 2026-04-15
(sensor-position uncertainty → OWTT ranging ADR-078; Helmholtz-PINN → XPINN/PINO
ADR-079; FNO thermocline → ThermoFno ADR-080; multistatic SAS → VAE-prior ADR-079b
parked; federated learning → 5-layer codec ~210 B/buoy ADR-080b/090). G6 (Perch 2.0
weights) + G7 (Closure-SDK AGPL) tracking. No Rust crate scaffolded yet.

**Kernel phase completion** (k0-k5-final-gap-analysis, 2026-03-25): K4 86.7%
(2 criteria), K5 94.1% (1 criterion), K6 133 tests but K6.4/K6.5 sub-phases
(chain replay, tree Merkle diff, SWIM semantics, CRDT gossip) designed but not
exercised end-to-end; no two-node integration harness.

**3I 8 true P0s**: GAP-11 SSE streaming (hardest, trait change cascade risk),
GAP-03 web search wiring, GAP-14 JSON repair, GAP-18 retry/failover, GAP-12
`weft onboard`, GAP-15 memory tool search, GAP-17 tool-call parsing.

## 4. ⚠️ Current working-tree state — CRITICAL hazard

**Branch**: `feat/weftos-579-591-graduations`, ~27 commits ahead of master + HEAD
`7475ef99`. **Nothing committed across the entire 05-14→05-17 session chain.**

`git diff --stat HEAD` ≈ **588 files / +19,498 / −14,389**.

**Untracked new crates (zero git history)**: `lgfx-bus-rgb-rs` (production-proven
bus driver), `weftos-leaf-scene` (112 tests), `weftos-leaf-renderer` +
`weftos-leaf-sim` (74 tests), `weftos-leaf-canvas` (~1135 LoC), `weftos-scene-builder`
(19 tests), `weftos-leaf-touch-gt911` (6 tests), `clawft-edge-pad-idf`. Plus
`docs/design/vector-leaf-display.md` (~4800 words), `docs/adr/adr-056`, `adr-057`,
`docs/leaf-push-protocol.md`.

**Risk rating: CRITICAL.** Handoff says "one accidental `git checkout` from
disappearing." The whole vector-display subsystem (5+ crates, ~300 tests, 2 ADRs,
1 design doc) has no history. The `~/.claude/agents/esp32-s3-rgb-touch-display/`
learnings are outside the repo with no backup. Partial mitigation: the "Fallout
glitch" snapshot at `.planning/actors/inkpad-snapshots/2026-05-15-fallout-glitch/`.
**Recommended immediate action: commit a focused diff of the new crates + design
doc before touching anything else.**

## 5. Operational gotchas

- **Daemon binary swap**: `cp weaver ~/.cargo/bin/` while daemon runs → "Text file
  busy". Use atomic `mv` + restart. Running daemon keeps old inode; new features
  invisible until restart. Bit the user 3+ times in the 05-17 session.
- **`.env` shadows weave.toml** (BUG-8).
- **scripts/build.sh is mandatory** for build/test/check/lint/gate per CLAUDE.md —
  not raw cargo.

## 6. TODO / FIXME density

`git grep -iE "TODO|FIXME|HACK|XXX"` over `crates/**/*.rs` = **74 lines**; most are
in detection/test infrastructure. Substantive hotspots: `plugins_cmd.rs` (6, all
plugin scaffolding), `surface_host/compose.rs` (6, unwired Surface IRIs +
governance deferred to M1.6), `vector_quantization.rs` (2, KG-011/012 blocked on
ruvector-core PR #352), `service.rs` (2, cancellation token Phase D2). **Zero
substantive FIXME/HACK in production logic.** Deceptive: the voice workstream
encodes deferred work in module doc-comments, not inline markers (~40 stub modules,
0 grep hits).
