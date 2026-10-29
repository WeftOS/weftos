# ruv (ruvnet) Ecosystem Catalog

**Last Updated**: 2026-07-03 (reparse — focus on newly added libraries)
**Maintained by**: `ruv-researcher` agent. Charter directory — all ruv research lives here.

This catalog tracks ruvnet's GitHub org (~197 public repos) with emphasis on the
libraries relevant to WeftOS / clawft. The prior baseline lives in
`.planning/development_notes/ruv-ecosystem-analysis-20260414.md` (2026-04-14) and
`.planning/development_notes/ruvector-weftos-alignment.md` (2026-02-28). This
directory supersedes those for cataloging purposes; the dev-notes remain the
source of truth for the deep K0–K5 phase-by-phase alignment already written.

## Files

| File | Contents |
|------|----------|
| `crate-index.md` | Full repo/crate inventory, categorized, with WeftOS relevance flags |
| `brain/` | Integration of `stuinfla/ruvnet-brain` (third-party MIT knowledge base of the RuvNet ecosystem). `README.md` = trust + verification policy (5/5 spot-checks passed 2026-07-03); `coverage-map.md` = brain topics ↔ this catalog ↔ WeftOS areas; `distilled-notes.md` = verified extracts (rvm, RuLake, agentdb deeper than our survey). **Third-party — primary ruvnet/* source wins on conflict.** |
| `packages/agentbbs/overview.md` | Deep-dive: AgentBBS (federated agent+human BBS) |
| `packages/agenticow/overview.md` | Deep-dive: agenticow (copy-on-write vector memory branching) |
| `packages/new-libraries-2026-07.md` | Survey: metaharness, midstream, RuLake, agentdb, rvm, RuView, and other new repos |
| `docs/research/rune-evaluation-2026-10-29.md` (repo root) | Evaluation: ruvnet/rune (Cargo wrapper, Agent Booster sidecar, bounded AI discovery) — verdict: watch, pattern-only |
| `integration/midstream-integration-plan.md` | Plan: MidStream → WeftOS voice/ECC. Verdict: adopt `temporal-compare` (vendored) + reference the LTL checker; no-go on scheduler/strange-loop/QUIC (redundant with ImpulseQueue/CognitiveTick/Weaver or off the voice path) |
| `integration/agenticow-integration-plan.md` | Plan: agenticow COW memory branching → hermes-loop turn checkpointing + exochain DualStateBridge. **Decision: reimplement in Rust over `rvf_runtime::RvfStore` (already a workspace dep, Cargo.toml:208); skip the JS/napi/sidecar layer entirely** |

## Headline: what's new since the 2026-04-14 parse

Two libraries are the reason for this reparse, and both are already wired into
the claude-flow / ruflo MCP surface visible from this session:

- **agenticow** (`ruvnet/agenticow`, MIT, JS+native) — *"Git for Agent Memory."*
  Copy-on-write vector-memory branching over the RVF format. Branch = 162 bytes,
  ~0.5 ms, regardless of base size. Exposed as MCP tools
  `agenticow_checkpoint / branch / rollback / promote`.
- **AgentBBS** (`ruvnet/AgentBBS`, FSL source-available, Rust) — a federated,
  Ed25519-signed, content-addressed bulletin-board where humans and agents are
  both first-class. Exposed as MCP tools
  `federation_bbs_register / publish / watch / human_join`.

The team-lead's hypothesis is confirmed: both repos exist, both are integrated
into the claude-flow MCP server, and the tool names map 1:1 to their public APIs.

## New / significantly-updated repos since 2026-04-14

| Repo | Lang | Pushed | ★ | One-liner | WeftOS relevance |
|------|------|--------|---|-----------|-----------------|
| **agenticow** | JS+native | 07-03 | 38 | Git for agent memory — COW vector branching | **HIGH** — memory/checkpoint, actor state |
| **AgentBBS** | Rust | 07-01 | 18 | Federated agent+human BBS, signed messages | **HIGH** — A2A substrate, channels, chain |
| **metaharness** | TS | 07-03 | 360 | Factory that scaffolds branded agent harnesses | MED — harness/governance patterns |
| **midstream** | Rust | 06-28 | 128 | Real-time LLM token-stream analysis/steering | **HIGH** — voice/ECC mid-stream gating |
| **RuLake** | Rust | 06-08 | 11 | Cache-coherent memory fabric over any store | MED — LLM cache, receipt-pinned answers |
| **agentdb** (standalone) | TS | 06-22 | 72 | Vector memory that self-improves per use | MED — memory backend |
| **rvm** | Rust | 05-23 | 114 | The Virtual Machine for the agentic age | MED — WASM sandbox / K3 |
| **RuView** | Rust | 07-03 | huge | WiFi-signal → real-time spatial intelligence | LOW — sensor domain (sonobuoy adjacent) |
| **ruv-neural** | Rust | 06-29 | 20 | Closed-loop OS for gamma-entrainment research | LOW |
| **worldgraph** | Rust | 06-27 | 11 | Privacy-aware environmental digital twin (RF) | LOW — sensor/actor adjacent |
| **rvcsi** | Rust | 05-23 | 13 | Edge RF-sensing runtime (WiFi CSI) | LOW — sonobuoy adjacent |
| **rufield** | Rust | 06-14 | 17 | Camera-free multimodal field-sensing spec | LOW — sensor adjacent |
| **skygraph** | Rust | 06-14 | 30 | Realtime all-sky radar in the browser | LOW |
| **rupixel** | Rust | 06-26 | 32 | Pixel-native visual RAG on ruvector ANN | LOW — RAG |
| **rvFACE** | Rust | 07-02 | 11 | Face recognition SDK (Rust+WASM, Burn) | LOW |
| **helix** | Rust | 07-01 | 26 | Local-first anti-hallucination health record | LOW — coherence/anti-halluc pattern |
| **hackerone** | TS | 06-29 | 12 | HackerOne meta-harness (defender triage) | LOW — metaharness vertical |
| **CVE-bench** | JS | 06-27 | 16 | Reproduce-and-fix security benchmark | LOW — AgentBBS arena uses it |
| **retort** | — | 06-29 | 4 | Platform evolution / distillation engine | LOW — AgentBBS arena track |
| **rudevolution** | JS | 06-27 | 111 | Semantic decompiler | LOW |
| **open-claude-code** | JS | 07-02 | 432 | Nightly Claude Code CLI decompile/rebuild | LOW |
| **SonicChamber** | Rust | 06-22 | 2 | Acoustic digital-human workbench (USCT) | LOW — sonobuoy adjacent |
| **rvdna / ruqu / PhotonLayer** | Rust | 06-18 | — | Genomics / quantum / optical-AI (RVF ports) | LOW — RVF-substrate demos |

Full list and older repos are in `crate-index.md`.

## Key concepts newly relevant to WeftOS

| Concept | Where | WeftOS mapping |
|---------|-------|---------------|
| COW memory branching (fork/checkpoint/rollback/promote) | agenticow | Actor/agent state snapshots; hermes-loop trajectory branching; exochain-style lineage over memory |
| Federated signed message boards (Ed25519, content-addressed, anti-entropy) | AgentBBS `agentbbs-federation` | `clawft-substrate` A2A + `clawft-channels`; ed25519 actor identity (ADR-025/057) |
| DualStateBridge (code/ops branch ↔ memory branch) | agenticow + `@metaharness/jujutsu` | Chain op-log ↔ actor memory coupling |
| Mid-stream token gating/steering (DTW, LTL, attractor analysis) | midstream | Voice/ECC 50 ms CognitiveTick — gate/steer before token completion |
| Receipt-pinned cache coherence ("refuse to guess when data changed") | RuLake | LLM answer caching with witness/receipt semantics |
| Harness-factory + default-deny MCP policy + `mcp-scan` | metaharness | Governance/gate patterns; MCP tool auditing |
