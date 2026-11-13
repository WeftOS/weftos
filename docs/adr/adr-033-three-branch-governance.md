# ADR-033: Three-Branch Constitutional Governance Model

**Date**: 2026-04-03
**Status**: Accepted
**Updated**: 2026-11-13. Rules may carry an optional `principal_selector` (exact id or trailing `*`) matched against the request's `agent_id`; absent means every principal. See ADR-103 A18.
**Deciders**: K2 Symposium (Governance Design), K3 Symposium (Gate Actions), kernel-governance.md specification

## Context

WeftOS agents execute tools, send IPC messages, spawn processes, and access services -- all of which must be governed by a policy system that prevents misuse. Traditional RBAC (role-based access control) models are insufficient because they cannot express multi-dimensional risk scoring, branch-separated authority, or chain-anchored immutability. The system needed a governance model that prevents any single authority from unilaterally modifying constraints, ensures all rules are tamper-evident, and evaluates actions using a scoring algebra rather than binary allow/deny.

The governance architecture was rendered across K2 (initial design), K3 (gate action integration), and the kernel-governance.md specification, which codifies the three-branch model, 22 genesis rules, dual-layer enforcement, and effect-algebra scoring.

## Decision

Governance uses a three-branch constitutional model implemented in `crates/clawft-kernel/src/governance.rs`:

1. **Legislative** (`GovernanceBranch::Legislative`): SOPs, manifests, genesis rules, data protection policies. Defines the boundaries within which agents operate. Includes rules GOV-003, GOV-005 and SOP-L001 through SOP-L006.

2. **Executive** (`GovernanceBranch::Executive`): Agent execution policies, deployment lifecycle, spawn clearance. Controls how agents act within defined boundaries. Includes rules GOV-004, GOV-006 and SOP-E001 through SOP-E005.

3. **Judicial** (`GovernanceBranch::Judicial`): CGR (Causal Graph Reasoning) validation engine, bias checks, audit compliance. Validates every action against constitutional constraints. Includes rules GOV-001, GOV-002, GOV-007 and SOP-J001 through SOP-J004.

**Separation guarantee**: No branch can modify another branch's constraints. The `GovernanceBranch` enum is attached to every `GovernanceRule` and is immutable once the rule is anchored to the ExoChain. This is enforced structurally -- the branch field is set at rule creation and cannot be mutated.

**Chain-anchored immutability**: At kernel boot (`boot.rs`), the governance system writes two kinds of chain events:
- `governance.genesis` -- a single event containing the full 22-rule constitutional ruleset, version (`"2.0.0"`), risk threshold (`0.7`), and genesis sequence number.
- `governance.rule` -- one event per rule, anchored individually for granular verification. Each carries `rule_id`, `branch`, `severity`, and `genesis_seq`.

Once written, these events cannot be edited or deleted. The only mechanism for change is `governance.root.supersede`, which creates a new constitutional root (a new genesis lineage).

**Dual-layer enforcement**: Every A2A message passes through two independent gate checks:
- Layer 1 (Routing gate, `a2a.rs`): Extracts action string from `MessagePayload` (`"tool.{name}"`, `"ipc.signal"`, `"ipc.message"`), checks against governance, can Deny/Defer/Permit.
- Layer 2 (Handler gate, `agent_loop.rs`): Second gate check for protected commands (`exec`, `cron.add`, `cron.remove`), enriched with tool name and `EffectVector` from the tool registry.

Both gates must permit for execution to proceed.

**Cluster implications**: The `governance.genesis` hash serves as the cluster identity (per K5 D4). A node that receives a `governance.root.supersede` from a different genesis lineage rejects it and halts synchronization, preventing split-brain governance.

## Consequences

### Positive
- Branch separation prevents privilege escalation -- an Executive policy cannot weaken a Judicial constraint
- Chain-anchored immutability provides tamper-evident audit of all governance rules; every rule change is cryptographically witnessed
- Dual-layer enforcement provides defense-in-depth -- a routing gate bypass does not grant execution permission
- The 22-rule genesis set covers AI-SDLC SOPs (bias, fairness, data protection, incident response, drift detection) aligned with external standards
- Governance genesis hash as cluster ID (K5 D4) ensures all nodes in a cluster enforce the same constitution

### Negative
- Governance bugs cannot be patched without a `governance.root.supersede` event, which creates a new constitutional root -- this is by design but operationally expensive
- The immutability guarantee means every genesis rule must be carefully reviewed before first boot; mistakes are permanent within that genesis lineage
- Dual-layer gate checking adds latency to every message delivery path (two governance evaluations per message)

### Neutral
- The rule distribution is balanced: Legislative 8, Executive 7, Judicial 7 (22 total), with 8 Blocking, 8 Warning, 6 Advisory severity levels
- The `GovernanceEngine::evaluate()` returns `GovernanceDecision::Permit` when the `governance` feature gate is disabled, enabling open governance for development builds
