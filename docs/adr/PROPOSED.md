# Proposed ADRs — Undocumented Decisions Audit — **RESOLVED**

**Original audit**: 2026-04-02 (ADR Architect Agent), proposed ADR-024 … ADR-047.
**Resolved**: 2026-06-29 — **every proposal in this audit (024–047) has since been written as a real ADR** in this directory. There are no outstanding proposals from this audit. The numbering held: each proposed number maps 1:1 to its shipped file.

This file is retained only as the historical audit record + the mapping below. New ADRs are numbered from the highest existing file (currently **ADR-124**; ADR-123 is reserved without a file); do not reuse 024–047.

## Resolution map (proposal → shipped ADR file)

| # | Title (as shipped) | Status |
|---|---|---|
| 024 | Noise Protocol (snow) for inter-node encryption | Accepted |
| 025 | Ed25519 public key as node identity | Accepted |
| 026 | QUIC primary transport, WebSocket browser fallback | Accepted |
| 027 | Selective libp2p composition | Accepted |
| 028 | Mandatory dual signing (Ed25519 + ML-DSA-65) | Accepted |
| 029 | weftos-rvf-crypto fork strategy | Accepted |
| 030 | CBOR (ciborium) ExoChain payload codec | Accepted |
| 031 | rvf-wire zero-copy mesh wire format | Accepted |
| 032 | DashMap concurrency primitive | Accepted |
| 033 | Three-branch constitutional governance | Accepted |
| 034 | Five-dimensional effect algebra scoring | Accepted |
| 035 | Layered protocol architecture (ServiceApi) | Accepted |
| 036 | Hierarchical ToolRegistry | Accepted |
| 037 | Rust Edition 2024 / MSRV 1.93 | Accepted |
| 038 | Tauri 2.0 desktop shell | Accepted |
| 039 | SWIM mesh failure detection | Accepted |
| 040 | LWW-CRDT distributed process table | Accepted |
| 041 | ChainAnchor trait | Accepted |
| **042** | **Three operating modes (Act / Analyze / Generate)** | Accepted |
| 043 | BLAKE3 forward, SHAKE-256 present | Accepted |
| 044 | wasm32-wasip2 WASI build target | Accepted |
| **045** | **Tiered router with permission-based model selection** | Accepted |
| **046** | **Forest of trees architecture (polyglot tree ensemble)** | Accepted |
| **047** | **Self-calibrating cognitive tick** | Accepted |

## Foundational to the in-flight ECC graph-walk conversation work (ADR-062)

These shipped ADRs are the substrate the voice / conversation work builds on — folded into **ADR-062** (`adr-062-ecc-graph-walk-conversation.md`) as Depends-On / Relates-To:

- **ADR-042 (Three Operating Modes)** — a voice conversation is **Mode 1 "Act"**; the all-agents hive/swarm is **Mode 3 "Generate"**. Same forest, floor, tick, chain.
- **ADR-046 (Forest of Trees)** — the walk *is* a forest CrossRef traversal (Causal lineage ⇄ HNSW ⇄ ExoChain); graft/prune *are* the forest's graft/shake. ADR-062's central structural change = make the live conversation path traverse the forest, not HNSW in isolation.
- **ADR-047 (Self-Calibrating Cognitive Tick)** — the Talk-Mode loop hosts as a `SystemService` on this tick (50ms/10ms, `tick_budget_ratio` 0.3, adaptive).
- **ADR-045 (Tiered Router)** — the speed/power model tiers behind the Speculative (quick) / Committed (considered) node split.
- **ADR-056 (BVH-on-RVF 4D index)** — temporal/causal recall for the walk (v2 multi-index fusion).
