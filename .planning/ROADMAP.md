# WeftOS Product Roadmap

**Last updated**: 2026-03-27
**Current state**: Kernel K0-K6 complete, Sprint 10 planned
**Branch**: feature/weftos-kernel-sprint → master

---

## Completed (K0-K6 + Sprints 08-09)

### Kernel Foundation — DONE
- K0: Boot sequence, config, daemon, health system
- K1: Process table, supervisor (spawn/stop/restart/watchdog), capability RBAC
- K2: A2A IPC (PID, Topic, Broadcast, Service routing), typed message envelopes
- K2.1: Symposium commitments (SpawnBackend, post-quantum dual signing, ServiceEntry)
- K2b: Health monitor, agent watchdog, graceful shutdown, suspend/resume

### WASM + Governance + Chain — DONE
- K3: WASM tool sandbox (30+ builtins, hierarchical ToolRegistry, SandboxConfig)
- Constitutional governance (3-branch, 22 rules, effect vectors, environment scoping)
- ExoChain (SHAKE-256, Ed25519 + ML-DSA-65 dual signing, checkpoints, witness receipts)

### ECC Cognitive Substrate — DONE
- K3c: CausalGraph (8 typed edges, BFS, path finding, spectral analysis, community detection, predictive change analysis), HNSW service, CognitiveTick, CrossRefStore, ImpulseQueue, ArtifactStore
- 5 embedding backends (Mock, ONNX with ort runtime, LLM API, SentenceTransformer, AST-aware)
- Weaver cognitive modeler (4,957 lines, 122 tests, 5 data sources, self-analysis, meta-Loom)

### Application + Mesh — DONE
- K4: Container integration (86.7% — 2 criteria remaining)
- K5: Application framework, AppManifest, lifecycle (94.1% — 1 criterion remaining)
- K6: Mesh networking (20 modules, 7,500+ lines) — Kademlia DHT, SWIM heartbeats, Noise Protocol, mDNS, post-quantum ML-KEM-768, distributed process table, CRDT gossip

### Sprint 08-09 Polish — DONE
- 08a-08c: SPARC plans written (self-healing, reliable IPC, content ops)
- 09a: Test coverage expansion
- 09b: Decision triage (24 decisions resolved, pending 23→9, blocked 1→0)
- 09c: Weaver runtime (41/41 TODO items, self-analysis of 29,448 nodes)
- 09d: Integration polish

### Metrics
- 175K+ lines of Rust across 22 crates
- 3,970+ tests (911 with ecc feature in kernel alone)
- 64 kernel modules, 54,794 kernel lines
- 115 commits analyzed, 179 modules mapped, 92 decision nodes tracked

---

## Sprint 10: Operational Hardening + K8 MVP + Client Pipeline (6 weeks)

**Goal**: "It runs, it's visible, it sells"

x-ref: `.planning/sparc/weftos/0.1/10-sprint-plan.md` for full detail

### Week 1-2: Kernel Hardening
| Track | Deliverable |
|-------|-------------|
| Self-Healing (08a) | RestartStrategy, process links, reconciliation, probes |
| Persistence | SQLite for ExoChain, causal graph + HNSW to disk |
| Observability (08b) | Dead letter queue, metrics, logging, timers |
| Tests | 20+ a2a.rs tests, 10+ boot.rs tests |
| Phase Closure | K3 (1 item), K4 (2 items), K5 (1 item) |
| DEMOCRITUS (ecc:D5) | Continuous cognitive loop: Sense→Embed→Search→Update→Commit |

### Week 2-3: Mesh + Tools + Assessor
| Track | Deliverable |
|-------|-------------|
| Mesh Runtime | MeshRuntime struct, A2A bridge (RemoteNode → MeshIpcEnvelope) |
| WASM Shell (D10) | Shell commands → WASM → sandbox → chain-log |
| Tool Catalog (D3) | 10 priority tools (fs.analyze, git.history, doc.parse, etc.) |
| Tool Signing (D9) | Ed25519 signing on ExoChain, verify at load |
| AI Assessor | Wire OpenRouter, seed 5 verticals, conversational intake |

### Week 3-4: K8 GUI + Assessor Deliverables + Web
| Track | Deliverable |
|-------|-------------|
| K8.1 Scaffold | Tauri 2.0 project, Rust↔TS bindings (rspc/ts-rs), WebSocket |
| K8.2 MVP | Dashboard (read-only) + Admin Forms (full round-trip CRUD) |
| K8 Gen Test | Weaver generates a TSX component, loads in Tauri shell |
| Assessor | PDF/PPTX deliverable generation, admin meeting flow |
| Web | weavelogic.ai audit, SEO, site structure revision |

### Week 4-5: Integration + External Analysis
| Track | Deliverable |
|-------|-------------|
| Mesh Hardening | mDNS + Kademlia wired, 2-node LAN demo |
| Assessor Dogfood | End-to-end assessment on test client, Weaver → CMVG |
| External Analysis | Weaver on Mentra, ClawStage, 1 public project |
| Deployment | Dockerfile, docker-compose, install guide, feature gate docs |
| Web Deploy | weavelogic.ai live, Fumadocs deployed, 2+ blog posts |

### Week 6: Gate Validation + Buffer
| Track | Deliverable |
|-------|-------------|
| Gate | 200+ new tests, clippy clean, all exit criteria checked |
| Polish | Bug fixes, client feedback integration, buffer |

### Sprint 10 Exit Criteria
- [ ] Crashed agent auto-restarts within 1 second
- [ ] Kernel state survives clean restart
- [ ] DLQ captures failed messages, metrics visible
- [ ] DEMOCRITUS loop runs continuously, graph grows
- [ ] Two nodes discover + exchange message on LAN
- [ ] WASM shell command executes in sandbox (D10)
- [ ] 10 new tools functional (D3), tool signing works (D9)
- [ ] K8: Tauri app renders dashboard + admin forms with real kernel data
- [ ] K8: Weaver-generated TSX component loads and functions
- [ ] Assessor: conversational intake → scoring → PDF report
- [ ] Assessor: 5 industry verticals seeded
- [ ] weavelogic.ai revised with services + WeftOS + CTA
- [ ] Fumadocs site deployed
- [ ] 200+ new tests

---

## Sprint 11: Full GUI + Open Source Launch + Client Scale (6 weeks)

**Goal**: "Ship the interface, launch the community, scale the pipeline"

### K8 GUI Completion
- K8.2: All 5 dashboard views (Process Explorer, Chain Viewer, Knowledge Graph, Governance Console)
- K8.3: 3D ECC visualization (React Three Fiber + Three.js)
- K8.4: Dynamic agent-generated app loading (runtime TS bundle injection)
- Knowledge Graph interactive view with community coloring, spectral overlay, temporal playback
- Mobile-responsive layout, dark/light theme

### Mesh Production
- Multi-hop message routing
- Kademlia DHT for WAN discovery
- Chain state replication (incremental, fork recovery)
- Service advertisement + remote service resolution working
- 3+ node cluster sustained operation

### Open Source Launch
- Public GitHub repo with exceptional README
- "Show HN" launch with Weaver self-analysis story
- r/rust, r/programming, r/artificial launch posts
- Contributor guide, "good first issue" labels, CONTRIBUTING.md
- License finalized (Apache 2.0 core, commercial enterprise tier)
- GitHub Actions CI/CD

### AI Assessor Scale
- Full 37 industry verticals populated
- Cloud infrastructure scanner agents (AWS/GCP/Azure)
- Live meeting intelligence (real-time agent in calls)
- Cross-assessment anonymized pattern matching
- Assessment Knowledge Agent (semantic search + causal tracing)

### Web & Marketing
- Blog series: origin story (6 posts)
- Conference CFP submissions (RustConf, KubeCon, RSA)
- Podcast pitch round (10 podcasts)
- Newsletter launch ("The Cognitive OS Weekly")
- LinkedIn content cadence (3-4/week)
- Partnership outreach (Drata, Vanta, LangChain, Balena)

### Sprint 11 Exit Criteria
- [ ] K8 GUI: 5 views + 3D graph + agent-generated apps
- [ ] Mesh: 3-node cluster running sustained
- [ ] GitHub: 500+ stars in first month
- [ ] Assessor: 37 verticals, cloud scanner, meeting intelligence
- [ ] 3+ client assessments completed
- [ ] weavelogic.ai ranking for target keywords

---

## Sprint 12: Self-Building Platform + Enterprise (6 weeks)

**Goal**: "The OS improves itself, enterprise clients deploy"

### K8 Self-Building (K8.5-K8.6)
- Weaver proposes UI improvements through governance
- Agent-built distributable apps (calculator, sensor dashboard, custom shells)
- App marketplace / builder interface
- Multi-window support, system tray
- Platform-specific packaging (macOS .app, Windows .exe, Linux AppImage)

### Enterprise Features
- Multi-tenant governance isolation
- SSO/SAML integration (extend OAuth2 plugin)
- Compliance reporting (SOC 2 / EU AI Act / HIPAA mappings)
- Policy template library
- SLA-backed support tier

### Hardening
- Remaining 15 WASM tools (D3 complete)
- WASM-compiled shell production (D10 complete)
- Named pipes, trace IDs (08b remainder)
- Artifact store with BLAKE3 dedup (08c K3-G1)
- Full WeaverEngine validation against 08c K3c criteria

### Sprint 12 Exit Criteria
- [ ] Agent-generated app loads and runs in production K8 shell
- [ ] Multi-tenant: two clients isolated on same infrastructure
- [ ] Compliance report generates for SOC 2 mapping
- [ ] All 08a/08b/08c exit criteria validated (117/117)

---

## In-Flight Research: LeWM World Model (ADRs 048–058)

**Status (updated 2026-11-14)**: crates landed (WEFT-519..534 and 543 Done): `weftos-worldmodel-core`, `weftos-worldmodel-impls`, `weftos-worldmodel`, `clawft-worldmodel-service`, plus ADR-090 invariant predicates, ADR-111 and `contracts/sensors/`. APIs, stubs, planners, SIGReg monitor, rollback gate, two training surfaces, RVF hot-swap and the W4 eval harness exist. **Residual is weights, the data path (W1 archiver, W2 recording), and a real dataset.** Existing WeftOS installs are unaffected.
**Living plan**: `docs/plans/world-model-data-and-training.md` (W1 to W7 phases)
**State report (2026-11-14)**: `/Volumes/ai-models/research/weftos-agent-notes/reports/lewm-worldmodel-state-20261114.md`
**Tracking**: `.planning/symposiums/lewm-worldmodel/diagram.md` · `.planning/lewm-worldmodel-rs-page/`
**Public page**: `https://weftos.weavelogic.ai/lewm-worldmodel-rs`

A sensor-primary latent world model layered *under* the ECC causal DAG. The sensor pipeline is the primary, self-sufficient plane; a WorldModelService is an additive, optional consumer. Decoupling-invariant by design (ADR-090): a node running WeftOS today will keep working identically whether LeWM is present, absent, or removed later.

### What exists now (shipped to master)
- Architecture diagram + infographic (`.planning/symposiums/lewm-worldmodel/diagram.md`)
- Public narrative page `/lewm-worldmodel-rs` (scroll pop-up book, 12 beats)
- Design notes `.planning/lewm-worldmodel-rs-page/{DESIGN-A,DESIGN-B,PLAN}.md`

### What is real vs stub
Plan around the stubs. The first group runs in-process today. The second group holds the residual work.

| Piece | Reality in code (2026-11-14) |
|-------|------------------------------|
| `weftos-sensor-pipeline` and `weftos-sensor-pipeline-wire` (ADR-056) | Exist. The wire crate frames observations as CBOR with Ed25519 signing. Schema stays provisional until 1.0-freeze. |
| Core traits and constants | Real. `LATENT_DIM = 192`, `Encoder`, `Predictor`, `LatentPlanner`, `LatticeApi`, `SigRegMonitor` (0.85 over 30 s), four-condition `RollbackGate`. |
| Planners and SIGReg monitor | Real. CEM, MPPI-warm and gradient planners; Welford SIGReg monitor with auto-rollback. |
| RVF hot-swap and rollback | Real. `SensorModelRegistry` commits at a tick and rolls back on gate failure. |
| W4 eval harness | Real on synthetic fixtures only. No recorded data exists yet. |
| `SensorClass` | Partial. Six classes exist (Generic, RgbD, Imu, Proprio, Lidar, Audio). mmWave, UWB, ToF, Wi-Fi CSI and SenML readings have no class yet. |
| `pred_φ` | Stub. `LinearPredPhi` adds the action code to the first 32 latent dims with no learned parameters. |
| Encoder | Stub. `HashEncoder` is the only non-null encoder. |
| Training surfaces and optimisers | Stub. Both pipelines train a residual mean, not a neural model. Checkpoints hold a 768-byte residual vector. |
| `candle` feature | Skeleton. `VitTinyConfig` and the ViT and AdaLN shells exist. They return `Unavailable` because no weights load. |
| Dataset archiver | Missing. No code exists yet. |
| Recorded dataset (W2) | Blocked on hardware and a recording session. |
| Service topologies | Role metadata only. HotStandby and P2P run on the same local loop, with no Raft and no mesh wiring. |
| DEMOCRITUS 1 kHz servo loop (ADR-047) | Research, not in tree. Self-calibrated heartbeat; runs *local* to node, not over mesh. |

### Reserved identifiers
| Identifier | Status |
|------------|--------|
| Mesh topics `mesh.sensor.v1.{encoded,consensus,control}` (ADR-053) | Reserved. Do not claim these topic prefixes in other features. |
| StructureTag::LatentWorldModel (ADR-048) | Reserved. Do not re-use this tag in CausalNode metadata. |

### Stability contract for existing installs (v0.6.x and forward)
- **Nothing in the LeWM research branch gates an existing release.** LeWM lands behind a feature flag when it lands.
- **Mesh, ECC, ExoChain, governance, and kernel APIs are unchanged** by this work. The decoupling-invariant (ADR-090) is non-negotiable.
- **Reserved identifiers above will not be assigned to anything else.** Other teams building on mesh/causal-graph surfaces can safely ignore them.
- **Sensor-pipeline wire format is considered provisional** until a `weftos-sensor-pipeline` crate ships with a version. Any integration before then must pin to a specific commit.

### Trigger to promote out of research
- W2 exit (plan §5): dataset card filed with hours per phase, held-out split by day, proof-tag counts and checksums, plus the ADR-384 step 4 validation record
- W5 flat model beats the matched baseline on held-out days, passes the rollback gate, and runs at 10 Hz on the Mac
- Decoupling-invariant test suite green (install-with-LeWM ≡ install-without-LeWM observability)
- ADRs 048–058 moved from research to accepted

---

## Post-1.0 Horizon

| Item | Description | Trigger |
|------|-------------|---------|
| **Blockchain anchoring (D12)** | ChainAnchor trait, simplest binding first | Enterprise demand for external audit chain |
| **Zero-knowledge proofs (D13)** | ZK for rollups, privacy-preserving governance, verifiable computation | Regulated industry requirement |
| **Trajectory learning (D17)** | Governance learns from decision trajectories | Sufficient decision volume (1000+ decisions logged) |
| **Full PKI/CA chain (D9)** | Central authority + CA chain for tool signing | Multi-operator deployment |
| **WASM snapshots (k3:D13)** | Long-running service state snapshots | K5+ long-running service adoption |
| **crates.io publication** | `weftos` crate on crates.io | Post open source launch stabilization |
| **Air-gapped deployment** | Full offline with local inference, FedRAMP/CMMC | Defense/IC contract |
| **Mentra integration** | WeftOS on ARM Cortex-A53 glasses | Hardware partnership |

---

## Revenue Timeline (from GTM Synthesis)

x-ref: `docs/business_plan/06-analysis-research/gtm-strategy-synthesis-2026-03.md`

| Milestone | When | Revenue |
|-----------|------|---------|
| First assessment sold | Sprint 10 Week 2-3 | $2,500-$7,500 |
| First retainer signed | Sprint 10 Week 4-6 | $10-15K/month |
| 3 active retainers | Sprint 11 Week 2-4 | $30-45K/month |
| Open source launch | Sprint 11 Week 3-4 | Community building |
| First enterprise governance pilot | Sprint 12 | $75-150K |
| **Cumulative Y1** | **Month 12** | **$400K-$1.5M** |

---

## Architecture Layers

```
K8:  GUI / Human Interface (Tauri 2.0 + React/TS + Three.js)
     ├── Dashboard (system health, metrics)
     ├── Admin Forms (CRUD against kernel APIs)
     ├── Knowledge Graph (3D causal visualization)
     ├── Governance Console (live decisions)
     ├── App Marketplace (agent-built distributable apps)
     └── Onboarding Flow (conversational discovery)

K6:  Mesh Networking (Noise, ML-KEM-768, Kademlia, SWIM, mDNS)
K5:  Application Framework (AppManifest, lifecycle, weftapp.toml)
K4:  Container Integration (Docker/Podman, Wasmtime)
K3c: ECC Cognitive Substrate (DEMOCRITUS loop, HNSW, spectral analysis)
K3:  WASM Tool Sandbox / Constitutional Governance / ExoChain
K2:  Agent-to-Agent IPC (A2A Router, topics, service resolution)
K1:  Process Management / Supervisor (restart strategies, links, monitors)
K0:  Boot / Config / Daemon
```

## Key Metrics to Track

| Metric | Current | Sprint 10 Target | Sprint 11 Target |
|--------|---------|------------------|------------------|
| Tests | 3,970 | 4,200+ | 4,500+ |
| Kernel lines | 54,794 | 60,000+ | 65,000+ |
| Decision debt | 9 pending, 0 blocked | 0 pending | 0 pending |
| Phase completion | 210/327 (64%) | 275/327 (84%) | 327/327 (100%) |
| GitHub stars | 0 (private) | 0 (still private) | 500+ (public) |
| Assessor verticals | 5 partial | 5 complete | 37 complete |
| Paying clients | 0 | 2-3 assessments | 3-4 retainers |
| Monthly revenue | $0 | $12K-$30K | $30K-$60K |
