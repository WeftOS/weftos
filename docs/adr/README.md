# Architecture Decision Records

This directory contains Architecture Decision Records (ADRs) for the WeftOS + clawft project, following the MADR 3.0 format. ADR-001 to ADR-023 came out of the Sprint 11 Symposium (2026-03-27) and its supporting analysis; later records were written as decisions were taken. The index below lists every `adr-*.md` file in this directory (123 files, ADR-001 to ADR-124).

ADR-123 has no file. The number is reserved by a card on the dashboard board; do not reuse it. Each Status column cell is copied from the ADR's own header, so the ADR is authoritative if the two ever differ.

## ADR Index

| ADR | Title | Status | Category | Source |
|-----|-------|--------|----------|--------|
| [ADR-001](adr-001-lockstep-semver.md) | Workspace-level lockstep semver versioning | Accepted | Release | Track 3 |
| [ADR-002](adr-002-cargo-dist.md) | cargo-dist for release artifact generation (manual version/changelog; no release-plz/git-cliff — WEFT-471) | Accepted | Release | Track 3 |
| [ADR-003](adr-003-codemirror.md) | CodeMirror 6 over Monaco for code editor block | Accepted | GUI | Track 4 |
| [ADR-004](adr-004-no-dockview.md) | No dockview -- CSS Grid + custom Lego engine | Accepted | GUI | Track 4 |
| [ADR-005](adr-005-xterm-js.md) | xterm.js for WeftOS console | Superseded by egui shell | GUI | Track 4 |
| [ADR-006](adr-006-custom-block-renderer.md) | Custom block renderer (json-render pattern) | Accepted | GUI | Track 4 |
| [ADR-007](adr-007-zustand-tauri-events.md) | Zustand + Tauri events for state management | Superseded by egui shell | GUI | Track 4 |
| [ADR-008](adr-008-weftos-cloud-side.md) | WeftOS cloud-side for Mentra (not on-device) | Accepted | Integration | Track 6 |
| [ADR-009](adr-009-sparse-lanczos.md) | Sparse Lanczos for spectral analysis | Accepted | Performance | Track 7 |
| [ADR-010](adr-010-keep-tokio.md) | Keep Tokio (do not adopt Asupersync) | Accepted | Architecture | Track 9 |
| [ADR-011](adr-011-no-frankensearch.md) | Do not add FrankenSearch (raw HNSW sufficient) | Accepted | Performance | Track 9 |
| [ADR-012](adr-012-inline-sha3.md) | Inline sha3 / blake3 fallback for rvf-crypto | Accepted | Release | Tracks 3, 8 |
| [ADR-013](adr-013-json-block-descriptor.md) | JSON block descriptor architecture | Superseded by egui shell | GUI | Track 4, Design Notes |
| [ADR-014](adr-014-fumadocs.md) | Fumadocs as single documentation source of truth | Accepted | Documentation | Track 5, Unification Plan |
| [ADR-015](adr-015-three-property-web.md) | Three-property web architecture | Accepted | Documentation | Web Presence Strategy |
| [ADR-016](adr-016-multi-target-theming.md) | Multi-target theming system | Accepted | GUI | Track 4, Theming Spec |
| [ADR-017](adr-017-gepa-prompt-evolution.md) | GEPA prompt evolution for pipeline/learner.rs | Accepted | Architecture | Hermes Analysis |
| [ADR-018](adr-018-hermes-llm-provider.md) | Hermes models as clawft-llm provider | Accepted | Integration | Hermes Analysis |
| [ADR-019](adr-019-registry-trait.md) | Registry trait in clawft-types | Accepted | Architecture | Track 1 |
| [ADR-020](adr-020-chainloggable.md) | ChainLoggable trait for audit gap closure | Accepted | Architecture | Tracks 1, 2, 4, 8 |
| [ADR-021](adr-021-cli-kernel-compliance.md) | CLI commands must route through kernel daemon | Accepted | Architecture | Sprint 14 |
| [ADR-022](adr-022-exochain-mandatory-audit.md) | All state-changing operations must log to ExoChain | Accepted | Architecture | Sprint 14 |
| [ADR-023](adr-023-assessment-as-kernel-service.md) | Assessment as a kernel service | Accepted | Architecture | Sprint 16 |
| [ADR-024](adr-024-noise-protocol-encryption.md) | Noise protocol for mesh encryption | Accepted | Security | K6 |
| [ADR-025](adr-025-ed25519-node-identity.md) | Ed25519 node identity | Accepted | Security | K6 |
| [ADR-026](adr-026-quic-primary-transport.md) | QUIC as primary transport | Accepted | Architecture | K6 |
| [ADR-027](adr-027-selective-libp2p.md) | Selective libp2p adoption | Accepted | Architecture | K6 |
| [ADR-028](adr-028-post-quantum-dual-signing.md) | Mandatory dual signing (Ed25519 + ML-DSA-65) | Accepted | Security | K2 / K5 Symposium |
| [ADR-029](adr-029-rvf-crypto-fork-strategy.md) | weftos-rvf-crypto fork strategy | Accepted | Release | K2 Symposium |
| [ADR-030](adr-030-cbor-exochain-codec.md) | CBOR exochain codec | Accepted | Architecture | K6 |
| [ADR-031](adr-031-rvf-wire-mesh-format.md) | RVF wire mesh format (JSON shipped; RVF deferred WEFT-683) | Accepted | Architecture | K6 |
| [ADR-032](adr-032-dashmap-concurrency.md) | DashMap for concurrent registry | Accepted | Performance | K2 |
| [ADR-033](adr-033-three-branch-governance.md) | Three-branch governance model | Accepted | Architecture | Governance |
| [ADR-034](adr-034-effect-algebra-scoring.md) | Effect-algebra scoring | Accepted | Architecture | Governance |
| [ADR-035](adr-035-serviceapi-layered-protocol.md) | ServiceApi layered protocol | Accepted | Architecture | K3 |
| [ADR-036](adr-036-hierarchical-tool-registry.md) | Hierarchical tool registry | Accepted | Architecture | K3 |
| [ADR-037](adr-037-rust-edition-2024-msrv.md) | Rust edition 2024 / MSRV policy | Accepted | Release | Sprint 14 |
| [ADR-038](adr-038-tauri-desktop-shell.md) | Tauri for desktop shell | Superseded by egui shell (2026-04-28) | GUI | GUI Track |
| [ADR-039](adr-039-swim-failure-detection.md) | SWIM failure detection | Accepted | Architecture | K6 |
| [ADR-040](adr-040-lww-crdt-process-table.md) | LWW-CRDT for process table | Accepted | Architecture | K6 |
| [ADR-041](adr-041-chainanchor-trait.md) | ChainAnchor trait | Accepted | Architecture | K4 |
| [ADR-042](adr-042-three-operating-modes.md) | Three operating modes | Accepted | Architecture | Sprint 16 |
| [ADR-043](adr-043-blake3-shake256-migration.md) | BLAKE3 / SHAKE-256 migration | Accepted | Security | Sprint 16 |
| [ADR-044](adr-044-wasm-wasip1-target.md) | WASM wasip1 target (alias for wasip2) | Accepted | Release | Sprint 14 |
| [ADR-045](adr-045-tiered-router-permissions.md) | Tiered router permissions | Accepted | Architecture | Sprint 16 |
| [ADR-046](adr-046-forest-of-trees-architecture.md) | Forest-of-trees architecture | Accepted | Architecture | Sprint 16 |
| [ADR-047](adr-047-self-calibrating-tick.md) | Self-calibrating cognitive tick | Accepted | Architecture | DEMOCRITUS |
| [ADR-048](adr-048-kernel-phase-responsibilities.md) | Kernel phase (K-level) responsibilities | Accepted | Architecture | Sprint 14 (formerly ADR-020 — renumbered 2026-04-28 / WEFT-140) |
| [ADR-049](adr-049-weftos-kernel.md) | WeftOS kernel architecture overview | Accepted | Architecture | K0 (formerly `architecture/adr-028-weftos-kernel.md` — renumbered + relocated 2026-04-28 / WEFT-140) |
| [ADR-050](adr-050-escalation-security-final-review.md) | Escalation security model — CONS-003 final review | Accepted | Security | Sprint 14 consolidation (CONS-003) |
| [ADR-051](adr-051-clawft-plugin-crates-fate.md) | Fate of the eight orphaned `clawft-plugin-*` crates | Accepted | Architecture | Plugin crate audit |
| [ADR-052](adr-052-toolregistry-split.md) | `clawft-core::ToolRegistry` vs `clawft-kernel::ToolRegistry` — two registries, two concerns | Accepted | Architecture | Tool registry audit |
| [ADR-053](adr-053-voice-stt-canonical-path.md) | Voice STT canonical path — substrate-side whisper | Accepted | Architecture | 0.7.0 release-gate audit (WEFT-205) |
| [ADR-054](adr-054-claude-flow-integration.md) | claude-flow integration — user-installed, not first-party | Accepted | Integration | 0.7.0 release-gate audit (WEFT-488) |
| [ADR-055](adr-055-backend-adapter-contract.md) | BackendAdapter contract for the agent dashboard | Accepted | GUI | 0.7.0 release-gate audit (WEFT-319) |
| [ADR-056](adr-056-bvh-spatial-index.md) | BVH-on-RVF spatial-temporal index over ECC | Accepted | Architecture | scorch_and_awe concept paper (2026-05-03) |
| [ADR-057](adr-057-substrate-read-acl.md) | Substrate per-path read ACLs (MUST-HAVE for 0.8.x) | Accepted | Security | Watch-as-Actor decision 2026-05-12 |
| [ADR-058](adr-058-per-conversation-context-memory-tier.md) | Per-conversation context memory tier (session RVF + HNSW) | Accepted | Architecture | Long-context agent-loop thread 2026-06-28; RMM follow-on `docs/research/rmm-reflective-memory-management.md` |
| [ADR-059](adr-059-qwen3-embedding-provider.md) | Qwen3-Embedding-0.6B as the clawft-kernel embedding provider (ort/ONNX) | Accepted | Architecture | Long-context agent-loop thread 2026-06-28 |
| [ADR-060](adr-060-local-hermes-serving-kv.md) | Local Hermes serving + KV management for the agent loop | Accepted | Architecture | Long-context agent-loop thread 2026-06-28 |
| [ADR-061](adr-061-conversational-voice-agent-loop.md) | Conversational voice agent loop — full-duplex, dual-layer TTS | Accepted | Architecture | Voice-pipeline thread 2026-06-26..28 (`~/llm` voicelab) |
| [ADR-062](adr-062-ecc-graph-walk-conversation.md) | ECC graph-walk conversation — responses as nodes built by walking the causal graph | Proposed | Architecture | ECC graph-walk design swarm 2026-06-29 |
| [ADR-063](adr-063-substrate-signed-envelope-a2a.md) | Signed, content-addressed envelope for substrate A2A messaging | Proposed | Architecture / Security | Substrate A2A design |
| [ADR-064](adr-064-substrate-anti-entropy-reconciliation.md) | Fail-closed anti-entropy reconciliation for cross-node substrate sync | Proposed | Architecture | Substrate sync design |
| [ADR-065](adr-065-channel-bridge-subkey-identity-loop-guard.md) | Per-source Ed25519 subkey identity and loop-guard for channel bridge mirroring | Proposed | Security | Channel bridge design |
| [ADR-066](adr-066-capability-tokens-human-join-governance.md) | Capability tokens and human-join flow for board/channel access, gated by governance | Proposed | Security | Board/channel access design |
| [ADR-067](adr-067-conversation-graph-view.md) | Conversation Graph View — timeline-scrubbed causal graph with classification-as-node-property in the egui GUI | Proposed | GUI | Conversation graph design |
| [ADR-068](adr-068-full-duplex-voice-edge-agent.md) | Full-duplex conversation floor + dumb streaming VoiceEdge | Proposed | Architecture | Voice-pipeline thread |
| [ADR-069](adr-069-atom-primary-index-panopticon.md) | Atom Primary Index & Locator Resolver (the Panopticon) | Proposed | Architecture | Atom index design |
| [ADR-070](adr-070-mcp-registry-ownership.md) | MCP server registry ownership — CLI durable config vs daemon runtime | Accepted | Architecture | WEFT-494 (ws15 MCP audit open question) |
| [ADR-071](adr-071-wasm-panel-auth.md) | WASM panel auth — per-panel token / capability model for webview proxy | Accepted | Security | WEFT-495 (ws15 MCP audit open question) |
| [ADR-072](adr-072-webview-substrate-publish-gate.md) | Webview vs daemon substrate write boundary (`substrate.publish` denylist) | Accepted | Security | WEFT-496 (ws15 MCP audit open question) |
| [ADR-073](adr-073-agent-workspace-cnvs-principles.md) | Agent Workspace interaction principles (CNVS-informed visible agents + WindowIntent) | Accepted | GUI | cnvs.dev / MaxBlade demos 2026-07-30 |
| [ADR-074](adr-074-interim-xai-grok-voice.md) | Interim primary voice — xAI Grok Voice; local remains offline/fallback | Accepted | Architecture | Owner: local sub-par for realtime; xAI until replacement |
| [ADR-075](adr-075-grok-weftos-mcp-client-bridge.md) | Grok Build (and peer CLIs) as WeftOS MCP clients; L1–L3 bridge ladder | Accepted | Integration | Owner: Grok drives WeftOS via `weft mcp-server` |
| [ADR-076](adr-076-mcp-tool-surface-capability-catalog.md) | MCP tool surface principles + unified capability catalog (profiles, façade) | Accepted | Integration | Audit: engine strong, outbound catalog weak |
| [ADR-077](adr-077-android-splat-capture-edge-node.md) | Android native splat capture as WeftOS edge node (phone → Mac/cloud) | Accepted (plan) | Integration | Native > web; Kernel AndroidPlatform |
| [ADR-078](adr-078-splat-feeds-world-model.md) | Splat pipeline feeds structured world model (objects/volumes → BVH) | Accepted | Architecture | Appearance SOG + structure leaves |
| [ADR-079](adr-079-urth-digital-twin.md) | **Urth** — multi-scale sparse-first planetary twin (Snow Crash north star) | Accepted (vision) | Architecture | LOD + open feeds + local densify; not “Earth” product branding |
| [ADR-080](adr-080-pending-skill-review-timing.md) | Pending-skill review timing — CLI + non-blocking start notice | Accepted | Architecture | WEFT-74 |
| [ADR-081](adr-081-no-imessage-applescript-bridge.md) | No first-party iMessage AppleScript channel (formal drop) | Accepted | Integration | WEFT-175 |
| [ADR-082](adr-082-graphify-port.md) | Graphify Rust port — `clawft-graphify` knowledge-graph crate | Accepted | Architecture | WEFT-371 (planned as ADR-049; 049 taken by kernel overview / WEFT-140) |
| [ADR-083](adr-083-browser-wasm-support.md) | Browser WASM Support | Accepted | Architecture | W-BROWSER (retroactive, 2026-07-31) |
| [ADR-084](adr-084-dependency-graph-retrieval-graphify.md) | Dependency-graph retrieval in Graphify (SGKR) | Proposed (Candidate) | Architecture | WEFT-372 phase2 survey (planned as ADR-050; 050 taken) |
| [ADR-085](adr-085-entity-dedup-hnsw-prefilter.md) | Entity deduplication via HNSW pre-filter (CodaRAG) | Proposed (Candidate) | Architecture | WEFT-372 phase2 survey (planned as ADR-051; 051 taken) |
| [ADR-086](adr-086-codebook-cold-start-entities.md) | Codebook cold-start for emerging entities (TransFIR) | Proposed (Candidate) | Architecture | WEFT-372 phase2 survey (planned as ADR-052; 052 taken) |
| [ADR-087](adr-087-spatiotemporal-dual-branch-sensors.md) | Spatio-temporal dual-branch for sensor systems (K-STEMIT) | Proposed (Candidate) | Architecture | WEFT-372 phase2 survey (planned as ADR-053; 053 taken by voice STT) |
| [ADR-088](adr-088-bvh-leaf-vector-ref.md) | Optional VectorRef on BVH spatial leaf payloads (before WEFT-709) | Accepted | Architecture | Design note `bvh_schema_updates.md`; amends ADR-056 |
| [ADR-089](adr-089-exochain-dag-merge-split-brain.md) | ExoChain DAG merge strategy + split-brain (not leader consensus) | Accepted | Architecture | WEFT-109 (K5 Q1/Q5) |
| [ADR-090](adr-090-lewm-ecc-decoupling-invariant.md) | LeWM ↔ ECC decoupling invariant (runtime-checkable rules) | Accepted (2026-07-31) | Architecture | LeWM / ECC design |
| [ADR-091](adr-091-lightrag-dual-level.md) | LightRAG dual-level keyword retrieval in graphify | Accepted | Architecture | Graphify retrieval |
| [ADR-092](adr-092-governance-rule-distribution.md) | Cluster-wide GovernanceRule distribution | Accepted (foundation) | Architecture / Security | Governance |
| [ADR-093](adr-093-bvh-hnsw-phase-f-join.md) | BVH × HNSW Phase F dual-index join (spatial-first / feature-first) | Accepted | Architecture | WEFT-723; builds on ADR-088 |
| [ADR-094](adr-094-spawn-user-level-permission.md) | Spawn-at-user-level permission story (principals + Defer grant) | Accepted (foundation) | Architecture / Security | WEFT-635; relates WEFT-633/634/636 |
| [ADR-095](adr-095-batch-graph-analytics-plane.md) | Batch graph analytics plane (disk-spill join-agg) — research hold for sensor scale | Draft (Proposed) | Architecture / Performance | Sinchenko DataFusion graphs; DiskANN companion research |
| [ADR-096](adr-096-metaharness-foundation.md) | MetaHarness as foundational agent/fusion evolution layer (flywheel; optional runtime) | Draft (Proposed) | Architecture / Integration | rUv MetaHarness + Grok/Ruflo; Graph View churn |
| [ADR-097](adr-097-metaharness-data-governance.md) | Universal MetaHarness governance over all WeftOS data surfaces | Draft (Proposed) | Architecture / Security / Integration | WEFT-728; fs/DB/sensors/mesh/substrate |
| [ADR-098](adr-098-environment-process-compose.md) | Per-project process-compose; environment pane planned only | Draft (Proposed) | Architecture / Integration | Triple loop; no raw yaml glob |
| [ADR-099](adr-099-governed-workload-placement.md) | Governed workload placement across the mesh (open capability vocabulary, accelerators, runtime adapters) | Accepted | Architecture / Mesh | Cogs + local inference; docs/research/mesh-placement |
| [ADR-100](adr-100-cog-workload-kind.md) | The cog workload kind (package, host contract, ingest bridge, Seed strategy) | Accepted | Architecture / Integration | Cognitum cogs; ADR-099. Cog-project decisions (COG-NNN) live in the private cogs repo |
| [ADR-101](adr-101-inference-workload-kind.md) | Inference workload kind; migrate local model hosting onto placement | Accepted | Architecture / Integration | ~/llm serving; ADR-060 / ADR-099 |
| [ADR-102](adr-102-gateway-health-and-api-playground.md) | Gateway health detail, daemon-issued tokens, and the API playground | Partially implemented (D1, D3, D5 and `/mcp` done; `/playground` page not done) | Architecture / Security | Owner decisions D1–D5, 2026-09-29 |
| [ADR-103](adr-103-weave-topology-roles-and-instances.md) | The Weave topology: roles, project kernels, instances and environments | Accepted (2026-09-30) | Architecture / Mesh | Owner decisions D1–D14, 2026-09-30 |
| [ADR-104](adr-104-sensor-guides-ship-with-sensor-cogs.md) | Sensor guides ship with sensor cogs | Accepted (2026-10-01) | Platform / Cogs | Owner, 2026-10-01; successor COG-104 in WeftOS/weftos-cogs |
| [ADR-105](adr-105-cog-sources.md) | Cog sources: per-project multi-repo cog catalog (WeftOS, Cognitum with licence, private signed repo), namespaced resolution, package trust fixes | Accepted | Architecture / Integration | `docs/cogs/cog-sources.md`; ADR-099 / ADR-100 / COG-008; successor [COG-105](https://github.com/WeftOS/weftos-cogs/blob/v0.1.4/docs/decisions/COG-105-cog-sources.md) |
| [ADR-106](adr-106-seed-licence-proxy.md) | Cognitum Seed as licence proxy for its WeftOS mesh: per-mesh checkout grants, operator hash approval, mesh-scoped redistribution | Proposed (review complete) | Architecture / Integration | ADR-099 / ADR-100 / ADR-103 / ADR-105; Phase 1a awaiting go; successor [COG-106](https://github.com/WeftOS/weftos-cogs/blob/v0.1.4/docs/decisions/COG-106-host-licence.md) |
| [ADR-107](adr-107-catalog-hardware-software-link.md) | Hardware catalog links modules to cogs (`Module::cogs`, firmware, docs); sensor detail panel in the console | Accepted | Platform / Console | ADR-104 / ADR-105 / COG-009; successor [COG-107](https://github.com/WeftOS/weftos-cogs/blob/v0.1.4/docs/decisions/COG-107-catalog-link.md) |
| [ADR-108](adr-108-project-workspaces-across-machines.md) | Install projects locally: one identity, many installations; workspace discovery and node actions over the heartbeat; `weft project init --adopt` | Accepted | Architecture / Projects | ADR-103 / ADR-099; P1, P2 in v0.8.3, `--adopt` on 0.8.4 |
| [ADR-109](adr-109-agent-environment-profiles-and-local-doctor.md) | Agent environment profiles (credential-free) and a local doctor that reports to the dashboard | Accepted | Platform / Agents | ADR-108; profile store `weftos-weavelogic` |
| [ADR-110](adr-110-agent-transcript-collection.md) | Agent transcript collection for shared memory and training: redact before transport, mesh-only, per-project access | Accepted | Platform / Agents | ADR-108 / ADR-109 |
| [ADR-111](adr-111-sensor-evidence-and-readings-contracts.md) | Sensor evidence and readings contracts: `spatial.evidence.v1` (with acoustic range, radio delay, clock correction) and the SenML readings profile and vocabulary | Accepted | Contracts / Sensors | `contracts/sensors/`; consumed by RuView ADR-384 and the sensor cogs |
| [ADR-112](adr-112-agent-teams-define-apply-run.md) | Agent teams: team spec v1 (roster, edges, write-authority table, presets, hosts), project-scoped apply with lock and birth report, dashboard apply over the mesh, and a WeftOS team runtime | Accepted | Platform / Agents | Extends the Agent Directory (AD-1..23); ADR-108 / ADR-109 / ADR-110 |
| [ADR-113](adr-113-pg-forge-ci-and-package-mirrors.md) | PG forge: Forgejo with Actions, per-project runners, Kellnr / Verdaccio / image-proxy mirrors, pipelines rendered from control-plane templates | Proposed | Platform / Infra | ADR-108 / ADR-112; Terraform environment interface |
| [ADR-114](adr-114-weftos-uri-naming.md) | `weftos://` names: internal mesh addressing (authority = mesh) for business concepts (companies, projects, goals, tickets, installations, members), mesh resources (nodes, hosts, services) and artifacts (cogs, teams, agents, memory, sensors); a name never grants access | Proposed | Platform / Naming | ADR-108 / ADR-112; prior art rvm ADR-157 (`ruv://`) |
| [ADR-115](adr-115-rvm-workload-kind.md) | `rvm` workload kind: rvm's verify-before-load and honest isolation claim (rvm-rvf/host/launch, pinned, feature-gated) over WeftOS's Wasmtime engine and OS confinement; rvm bare metal watch-only | Proposed | Platform / Workloads | ADR-114; evidence docs/research/rvm-spike-2026-10.md |
| [ADR-116](adr-116-local-tailnet-router.md) | Tailnet router on every dev machine: Tailscale Serve :443 → user-daemon router; projects by path from `compose/ports.yaml` routes; conflict refusal; `/_weftos/` index over each project's process-compose | Proposed | Platform / Dev environment | Builds on ADR-098 (amends §2), ADR-108 |
| [ADR-117](adr-117-display-endpoints-objects-and-bindings.md) | Display endpoints: any app targets a leaf display; objects first (scene ops, object-level input), live bindings to mesh data, the leaf as a sensor, display grants and arbitration with a firmware system strip; pixels only as image/video media objects; egui via a scene adapter; first app the command center | Proposed | Platform / Leaves | Builds on vector-leaf-display.md, ADR-103, ADR-108, ADR-114, ADR-111 |
| [ADR-118](adr-118-chief-of-staff-console.md) | Chief-of-staff console: green vector screen (visual panel with avatar and later page mode, status, scrollable chat log), voice only with a hardware push button, a real chief-of-staff agent using WeftOS models, skills and the owner's team | Proposed | Apps / Leaves / Agents | ADR-117, ADR-112, voice stack |
| [ADR-119](adr-119-field-slate.md) | Field slate: portable WeftOS node in an aluminium storage clipboard: CrowPanel 7" + e-ink + calculator keypad + fingerprint (layout A), brain in the cavity, inputs as sensor events, plugin rail with printed sleeves as cogs, LoRa, parametric OpenSCAD parts | Proposed | Hardware / Leaves | ADR-117, ADR-118, ADR-100/104, ADR-108 |
| [ADR-120](adr-120-mesh-resources-and-exposures.md) | Mesh resources and exposures: library/toolchain facts, one registry of exposed APIs and harnesses (generalising ADR-101 expose_to_mesh), announced or refused, local off switch always wins, chained activity log, dashboard Mesh Network panels | Proposed | Mesh / Placement | ADR-099, ADR-101, ADR-108, ADR-114, ADR-022 |
| [ADR-121](adr-121-ai-org-versions-and-instances.md) | AI org: definition -> installation -> instance; flat org above per-person chiefs of staff; request/approve/install; versions, version trees and dependencies (local file + CoW ruvector index); nodes report instances on the chain and enforce digests; explain affordance | Accepted | Agents / Org | ADR-112, ADR-114, ADR-120, ADR-022 |
| [ADR-122](adr-122-cog-data-plane-broker.md) | Cog data plane: `cogbus` broker in the node daemon; topics `<kind>/<source_id>`; readings over a Unix socket, streams in shared-memory rings, blobs by BLAKE3 handle; per-instance tokens; governed publish/subscribe; reader policies; raw data local by default; mesh topics as ADR-120 exposures; ingest bridge becomes a consumer | Accepted | Cogs / Data plane | ADR-099, ADR-100, ADR-114, ADR-117, ADR-120, ADR-031 |
| [ADR-124](adr-124-node-clock-and-reading-time.md) | Node clock status on `mesh.clock.v1.status`; SenML capture time; host owns chrony | Proposed | Architecture / Sensors | ADR-025 / ADR-100 / ADR-106 |


## Categories

| Category | ADRs | Description |
|----------|------|-------------|
| **Release** | 001, 002, 012, 029, 037, 044 | Versioning, distribution, and build decisions |
| **GUI** | 003, 004, 005, 006, 007, 013, 016, 038, 055, 067, 073 | UI/UX technology and architecture decisions |
| **Architecture** | 010, 017, 019, 020, 021, 022, 023, 026, 027, 030, 031, 033, 034, 035, 036, 039, 040, 041, 042, 045, 046, 047, 048, 049, 051, 052, 053, 056, 061, 062, 063, 064, 068, 069, 070, 074, 078, 079, 080, 082, 084, 085, 086, 087, 088, 089, 090, 091, 093, 095, 099, 101, 102, 103, 106, 120, 121, 122, 124 | Core system design decisions |
| **Security** | 024, 025, 028, 043, 050, 057, 065, 066, 071, 072, 092, 097 | Cryptography, identity, and chain-integrity decisions |
| **Performance** | 009, 011, 032, 095 | Algorithmic and optimization decisions |
| **Integration** | 008, 018, 054, 075, 076, 077, 081, 096, 097, 098 | External system integration decisions |
| **Documentation** | 014, 015 | Documentation and web presence decisions |
| **Platform and later** | 100, 104, 105, 107–119 | Cogs, projects, agents, leaves, dev environment (see each row's Category) |

## Decision Sources

All decisions were produced during or immediately after the Sprint 11 Symposium (2026-03-27):

- **Track 1**: Code Pattern Extraction (ADR-019, ADR-020)
- **Track 3**: Release Engineering (ADR-001, ADR-002, ADR-012)
- **Track 4**: UI/UX Design Summit (ADR-003 through ADR-007, ADR-013, ADR-016)
- **Track 5**: Changelog and Documentation (ADR-014)
- **Track 6**: Mentra Integration (ADR-008)
- **Track 7**: Algorithmic Optimization (ADR-009)
- **Track 9**: Optimization Plan (ADR-010, ADR-011)
- **Hermes Integration Analysis**: ADR-017, ADR-018
- **Web Presence Strategy**: ADR-015
- **Theming System Spec**: ADR-016
- **Fumadocs Unification Plan**: ADR-014

## Adding New ADRs

1. Create a new file: `adr-NNN-short-title.md`
2. Use the template format (Context / Decision / Consequences)
3. Add the entry to the index table above
4. Set status to `Proposed` until reviewed and accepted
