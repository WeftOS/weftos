# Capability crosscut (auto)

Generated: 2026-10-13T18:02:42.284Z

**String:** `SEE → WIRE → BUILD → UPSTREAM`

## Counts

| Mode | N |
|------|---|
| SEE | 19 |
| WIRE | 0 |
| BUILD | 0 |
| UPSTREAM | 2 |

## Full table

| Node | Domain | Mode | Next | Brain score |
|------|--------|------|------|-------------|
| `ruflo` | orchestrator | **SEE** | Present on both sides; ensure dual-host/MCP path uses it | 17.615 |
| `agentdb` | memory | **SEE** | Present on both sides; ensure dual-host/MCP path uses it | 8.242 |
| `agenticow` | memory | **SEE** | Present on both sides; ensure dual-host/MCP path uses it | 6.154 |
| `metaharness-read` | harness | **SEE** | Present on both sides; ensure dual-host/MCP path uses it | 15.162 |
| `metaharness-flywheel` | harness | **SEE** | Present on both sides; ensure dual-host/MCP path uses it | 15.432 |
| `metaharness-darwin` | harness | **SEE** | Dry loop present (darwin-loop.mjs); full @metaharness/darwin evolve is optional S3 with --confirm | 11.602 |
| `metaharness-hosts` | host | **SEE** | Grok host reference present — agents can SEE pathfinder; UPSTREAM host-grok package still open (S1 publish) | 16.309 |
| `metaharness-router` | routing | **UPSTREAM** | Optional consume @metaharness/router + savings (S2) | 15.427 |
| `ruvector` | substrate | **SEE** | Present on both sides; ensure dual-host/MCP path uses it | 9.615 |
| `cognitum-gate-tilezero` | governance | **SEE** | Dep + agent task present — C3 full cargo CI smoke still optional maturity | 20.645 |
| `cognitum-maas` | cloud | **SEE** | Present on both sides; ensure dual-host/MCP path uses it | 18.141 |
| `cognitum-seed` | edge | **SEE** | Present on both sides; ensure dual-host/MCP path uses it | 12.768 |
| `worldgraph` | twin | **SEE** | Present on both sides; ensure dual-host/MCP path uses it | 15.323 |
| `quic-mesh` | network | **SEE** | Present on both sides; ensure dual-host/MCP path uses it | 6.388 |
| `ruvllm-sona` | learning | **UPSTREAM** | Optional micro-loop (ADR-234); not required for WeftOS product — document only (no BUILD until product need) | 24.323 |
| `weftos-kernel-ecc` | weftos | **SEE** | Index/doctor already ok; keep brain index fresh so agents find it | 11.774 |
| `weftos-exochain` | weftos | **SEE** | Index/doctor already ok; keep brain index fresh so agents find it | 10.213 |
| `weftos-bvh-spatial` | weftos | **SEE** | Index/doctor already ok; keep brain index fresh so agents find it | 15.056 |
| `weftos-lewm` | weftos | **SEE** | Index/doctor already ok; keep brain index fresh so agents find it | 12.111 |
| `weftos-voice` | weftos | **SEE** | Index/doctor already ok; keep brain index fresh so agents find it | 17.108 |
| `weftos-splat-edge` | weftos | **SEE** | Index/doctor already ok; keep brain index fresh so agents find it | 16.517 |

## Suggested Darwin gen-1

```json
{
  "lever": "prefer_intervention",
  "targetMode": "WIRE",
  "focus": "agentdb",
  "note": "Mutate harness docs/tasks only — one WIRE node per generation"
}
```

Machine JSON: `.metaharness/brain/crosscut-latest.json`
