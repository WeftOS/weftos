# Brain · 01 — Roadmap & Phases

> Planned work and phase structure. Source-of-truth: `.planning/ROADMAP.md`,
> `.planning/sparc/`, `.planning/improvements.md`, `.planning/sprint17.md`,
> `.planning/01/02/03-*.md`.

## 1. Phase taxonomy

The project uses **several overlapping naming schemes**. Knowing which is which
prevents confusion.

### K0–K8 — Kernel layer series (the OS build)

| Layer | Title | Status |
|---|---|---|
| K0 | Boot / Config / Daemon / Health | DONE |
| K1 | Process table / Supervisor / RBAC | DONE |
| K2 | A2A IPC (PID, Topic, Broadcast, Service routing) | DONE |
| K2.1 | Symposium impl (SpawnBackend, post-quantum signing, ServiceEntry) | DONE |
| K2b | Health monitor, watchdog, graceful shutdown, suspend/resume | DONE |
| K3 | WASM tool sandbox + 3-branch governance (22 rules) + ExoChain | DONE (1 item remaining) |
| K3c | ECC cognitive substrate (CausalGraph, HNSW, DEMOCRITUS, Weaver) | DONE |
| K4 | Container integration (Docker/Podman/Wasmtime) | 86.7% — 2 criteria left |
| K5 | Application framework (AppManifest, lifecycle) | 94.1% — 1 criterion left |
| K6 | Mesh networking (Kademlia/SWIM/Noise/mDNS/ML-KEM-768/CRDT) | DONE |
| K7 | Cognitive sync (cross-node) | PLANNED (named in ADR-026, not shipped) |
| K8 | GUI / human interface (egui native + WASM; Tauri scaffold) | IN PROGRESS |

K8 sub-phases: K8.1 scaffold, K8.2 MVP, K8.3 3D ECC viz, K8.4 dynamic
agent-generated apps, K8.5–K8.6 self-building.

### Sprint series (operational)

08a–08c (plans: self-healing / reliable-IPC / content-ops), 09a–09d (DONE: test
coverage, decision triage, Weaver runtime, integration polish), **Sprint 10**
(PLANNED — "runs, visible, sells": hardening + K8 MVP + AI Assessor client
pipeline), Sprint 11 (open-source launch + scale), Sprint 12 (self-building +
enterprise), **Sprint 17** (STARTED 2026-04-16 — security hardening + ontology
graph pipeline).

### clawft agent phases 1–5 (pre-kernel rewrite)

Phase 1 "Warp" (foundation + CLI + Telegram), Phase 2 "Weft" (Slack/Discord/
ruvector/services), Phase 3 "Finish" (WASM/CI/workspace/MCP), Phase 3G
(workspaces), Phase 4 (TieredRouter, 1,646 lines), Phase 5 "Improvements Sprint"
(CLOSED 2026-04-28, WEFT-24). All DONE.

### Multi-platform expansion (`.planning/sparc/00-master-plan.md`)

W-BROWSER (BW1–BW6), W-UI (S1.1–S3.7, 17 phases), W-VOICE (VP, VS1.1–VS3.3, 10
phases) — three parallel workstreams.

### Version cycles

Two distinct schemes:
- **WeftOS kernel**: 0.1.0 (kernel complete) → 0.2.0 (K8 GUI) → 0.3.0
  (marketplace) → 1.0.0 (stable API + audit).
- **clawft agent**: the 0.6.x/0.7.x/0.8.x/0.9.x/1.0.x track visible in releases
  and Plane tickets. WEFT-579..591 were pulled from 0.8.x into 0.7.0 blockers.

### WEFT-NNN tickets

Tracked in Plane (`weftos` workspace), spanning cycles 0.7.x–1.0.x. See
[`02-release-history-and-features.md`](02-release-history-and-features.md) for
the ticket→delivery map.

## 2. Planned features by phase (status where stated)

- **Kernel K0–K6** — DONE: boot/daemon/health, process supervisor + RBAC, A2A
  IPC + post-quantum dual signing, WASM sandbox (30+ builtins), 3-branch
  governance (22 rules, effect vectors), ExoChain (SHAKE-256 + Ed25519 +
  ML-DSA-65), ECC substrate (CausalGraph 8 edges, HNSW, DEMOCRITUS, Weaver 4,957
  lines/122 tests, 5 embedding backends), K6 mesh.
- **Sprint 10** — PLANNED: self-healing (<1s restart), SQLite persistence for
  ExoChain/causal/HNSW, dead-letter queue + metrics, MeshRuntime + 2-node demo,
  WASM shell, 10-tool catalog + Ed25519 tool signing, K8.1/K8.2 Tauri dashboard,
  AI Assessor (OpenRouter, 5 verticals → PDF), weavelogic.ai + Fumadocs.
- **Sprint 11** — PLANNED: 5 GUI views + 3D ECC viz, multi-hop mesh + WAN DHT,
  open-source launch (Apache 2.0 + commercial), 37 assessor verticals.
- **Sprint 12** — PLANNED: self-building (Weaver proposes UI via governance),
  app marketplace, platform packaging, enterprise multi-tenant + SSO + compliance
  (SOC 2 / EU AI Act / HIPAA).
- **Sprint 17** — IN PROGRESS: prompt-injection defense, RPC auth, plugin supply
  chain (Ed25519 manifest signing), ontology graph pipeline (VOWL/OWL/RDF),
  namespace isolation + log redaction.
- **LeWM world model (ADRs 048–058)**: landed in the 0.8.4 cycle (updated
  2026-11-14): sensor-primary latent world model under the ECC causal DAG. Crates
  `weftos-worldmodel-core`, `weftos-worldmodel-impls`, `weftos-worldmodel` and
  `clawft-worldmodel-service` exist, with APIs, planners, SIGReg monitor, rollback gate,
  two training surfaces and RVF hot-swap. Stubs only for weights: the candle path is a
  skeleton that returns Unavailable. Training hold lifted for the sensor world model on
  2026-10-07; W1 to W7 phases are in `docs/plans/world-model-data-and-training.md`.
  Residual is weights, the data path, and a real dataset. Decoupling invariant is ADR-090.
- **Post-1.0 horizon** — DEFERRED: blockchain anchoring, ZK proofs, full PKI/CA,
  WASM snapshots, crates.io publication, FedRAMP/CMMC, Mentra glasses.

## 3. Roadmap timeline

`2026-Q1` kernel K0–K2 + clawft Phases 1–3 (working `weft` binary, 4.9 MB) →
`2026-02-24` Browser/UI/Voice master plan → `2026-02-28` kernel orchestrator,
K3–K6 plans → `2026-03` M1 egui spike → `2026-03-27` ROADMAP written, K0–K6 +
Sprints 08–09 DONE, Sprint 10 issued → `2026-04` M1.5 substrate/surface/Admin
app, 0.6.19 released → `2026-04-16` Sprint 17 security starts → `2026-04-28`
Phase 5 closed (WEFT-24), 0.7.0 release-gate audit (17 workstreams), WEFT-579..591
→ 0.7.0 blockers. Near-term targets: first paid assessments ($2.5–7.5K), first
retainer ($10–15K/mo), Fumadocs site, K8 dashboard with real data. Year-1
cumulative revenue target $400K–$1.5M; 1.0 after 2 stable minors + security audit.

## 4. Product vision

WeftOS is a dual-thesis product. **Agent layer**: a sub-10 MB Rust binary
(`weft`) for ARM/IoT — multi-channel messaging, permission-aware tiered LLM
routing, a 6-stage self-learning pipeline. The flagship business application is
the **AI Assessor**: a consulting product that audits client AI readiness and
produces governed PDF reports, funding the roadmap via retainers. **Kernel
layer**: a cognitive OS substrate positioning WeftOS as the tool organizations
use to understand, document (live knowledge graph), and plan the automation of
their own systems — infrastructure for the enterprise AI-governance market. The
flagship technology demo is Weaver's self-analysis of 29,448 of its own nodes.
