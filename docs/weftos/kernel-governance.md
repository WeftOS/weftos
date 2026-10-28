# WeftOS Kernel Governance

> **Status (2026-10-27).** Reference written against the 0.7.x governance code (last edited 2026-07-31). Governance rules are now also distributed across the cluster (ADR-092) and there is no governance CLI: the `governance.*` surface is RPC only (see [../reference/rpc.md](../reference/rpc.md#governance)).

Complete reference for the constitutional governance system that controls
all agent behavior in WeftOS.

---

## Overview

WeftOS implements a **three-branch constitutional governance model** inspired by
separation-of-powers principles. Every action an agent takes -- tool execution,
IPC messaging, service access, process spawning -- passes through a governance
evaluation pipeline before it is allowed to proceed.

The governance system has three key properties:

1. **Immutability.** Governance rules are anchored to the ExoChain as
   `governance.genesis` and `governance.rule` events. Once written, they
   cannot be modified. The only mechanism for change is a
   `governance.root.supersede` event that creates a new constitutional root.

2. **Dual-layer enforcement.** Every message passes through two independent
   gate checks: one at routing time (in the A2A router) and one at handler
   time (in the agent loop). Both must pass for execution to proceed.

3. **Effect-algebra scoring.** Actions are scored along 5 dimensions
   (risk, fairness, privacy, novelty, security) using an L2-norm magnitude
   calculation. The magnitude is compared against a configurable threshold
   to determine whether the action is permitted, warned, escalated, or denied.

---

## Architecture

### Three-Branch Constitutional Model

```
                    +-----------------+
                    |   Constitution  |
                    | (Chain Genesis) |
                    +--------+--------+
                             |
            +----------------+----------------+
            |                |                |
   +--------v--------+  +---v-----------+  +-v--------------+
   |   LEGISLATIVE   |  |   EXECUTIVE   |  |    JUDICIAL    |
   |                  |  |               |  |                |
   | SOPs, manifests, |  | Agent exec    |  | CGR validation |
   | genesis rules,   |  | policies,     |  | engine, bias   |
   | data protection  |  | deployment,   |  | checks, audit  |
   |                  |  | lifecycle     |  | compliance     |
   +------------------+  +---------------+  +----------------+
```

**Separation guarantee:** No branch can modify another branch's constraints.
A Judicial rule cannot be changed by an Executive policy, and vice versa.
This is enforced by the `GovernanceBranch` enum attached to every rule --
the branch is immutable once a rule is anchored to the chain.

### Chain-Anchored Rules

At kernel boot (in `boot.rs`), the governance system writes two kinds of
chain events:

1. **`governance.genesis`** -- A single event containing the full
   constitutional ruleset, version, risk threshold, and genesis sequence
   number. This is the **governance root**. Its payload:

   ```json
   {
     "version": "2.0.0",
     "risk_threshold": 0.7,
     "human_approval_required": false,
     "rules": [ ... ],
     "rule_count": 22,
     "genesis_seq": 0
   }
   ```

2. **`governance.rule`** -- One event per rule, anchored individually for
   granular verification. Each payload contains:

   ```json
   {
     "rule_id": "GOV-001",
     "branch": "judicial",
     "severity": "blocking",
     "genesis_seq": 0
   }
   ```

**Immutability.** Once these events are on the chain, they cannot be edited
or deleted. The chain is append-only. The only way to change the governance
constitution is a `governance.root.supersede` event (see
[Adding Rules at Runtime](#adding-rules-at-runtime-k6)).

**Cluster implications.** If a node receives a `governance.root.supersede`
event it does not recognize (e.g., from a different genesis lineage), that
node should reject the new root and halt synchronization. This prevents
split-brain governance where different nodes enforce different constitutions.

### Dual-Layer Enforcement

Every A2A message passes through two independent gate checks:

```
  Agent A                                                    Agent B
    |                                                          |
    |  send(msg)                                               |
    |  ------>  +------------------+                           |
    |           | A2ARouter        |                           |
    |           |                  |                           |
    |           | 1. ROUTING GATE  |--- Deny --> error to A    |
    |           |    (gate.check)  |                           |
    |           |                  |--- Defer/Permit -------+  |
    |           +------------------+                        |  |
    |                                                       v  |
    |                                            +-------------+--+
    |                                            | agent_loop (B)  |
    |                                            |                 |
    |                                            | 2. HANDLER GATE |
    |                                            |    (gate.check) |
    |                                            |                 |
    |                                            | Deny -> error   |
    |                                            | Defer -> defer  |
    |                                            | Permit -> exec  |
    |                                            +-----------------+
```

**Layer 1: Routing gate** (`a2a.rs`, `A2ARouter::send`).
When a message is sent via the A2A router, the gate extracts the action
string from the message payload:

- `MessagePayload::ToolCall { name, .. }` produces `"tool.{name}"`
- `MessagePayload::Signal(_)` produces `"ipc.signal"`
- All other payloads produce `"ipc.message"`

A `Deny` decision blocks the message before it reaches any inbox.
A `Defer` decision still delivers the message -- the handler decides.
A `Permit` decision delivers the message normally.

**Layer 2: Handler gate** (`agent_loop.rs`, `agent_loop`).
When the agent loop processes an incoming message, it performs a second gate
check for protected commands:

| Command | Gate Action |
|---------|-------------|
| `exec` | `"tool.exec"` |
| `cron.add` | `"service.cron.add"` |
| `cron.remove` | `"service.cron.remove"` |

For `exec` commands, the handler enriches the gate context with the tool
name and its `EffectVector` from the tool registry (if available):

```json
{
  "pid": 42,
  "tool": "fs.write_file",
  "effect": {
    "risk": 0.4,
    "security": 0.2,
    "privacy": 0.1
  }
}
```

The handler gate treats all three decisions as final:
- **Deny** sends an error reply with `{"error": "<reason>", "denied": true}`.
- **Defer** sends a deferral reply with `{"deferred": true, "reason": "<reason>"}`.
- **Permit** continues to normal tool execution.

### Effect Vector Scoring

The `EffectVector` is a 5-dimensional vector that quantifies the impact of
an action:

```rust
pub struct EffectVector {
    pub risk: f64,       // Probability of negative outcome (0.0 - 1.0)
    pub fairness: f64,   // Impact on equitable treatment (0.0 - 1.0)
    pub privacy: f64,    // Impact on data privacy (0.0 - 1.0)
    pub novelty: f64,    // How unprecedented the action is (0.0 - 1.0)
    pub security: f64,   // Impact on system security (0.0 - 1.0)
}
```

**Magnitude calculation (L2 norm):**

```
magnitude = sqrt(risk^2 + fairness^2 + privacy^2 + novelty^2 + security^2)
```

The maximum possible magnitude is `sqrt(5) = 2.236` (all dimensions at 1.0).
In practice, most actions score well below 1.0.

**Threshold comparison.** The engine compares the magnitude against the
configured `risk_threshold`. If `magnitude > risk_threshold`, the threshold
is considered exceeded, which triggers blocking/warning behavior depending
on the severity of active rules.

**Tool catalog integration.** Each tool in the `ToolRegistry` has a
`BuiltinToolSpec` that includes an `EffectVector`:

```rust
pub struct BuiltinToolSpec {
    pub name: String,          // e.g. "fs.read_file"
    pub description: String,
    pub category: ToolCategory,
    pub gate_action: String,   // e.g. "tool.fs.read_file"
    pub effect: EffectVector,  // Governance scoring vector
    pub native: bool,
}
```

When the agent loop processes an `exec` command, it looks up the tool in the
registry and passes its `EffectVector` to the gate context. This means
governance scoring is tool-aware: a `fs.read_file` tool might have
`{risk: 0.1, privacy: 0.2}` while a `shell_exec` tool might have
`{risk: 0.8, security: 0.7}`.

**Helper methods on EffectVector:**

| Method | Description |
|--------|-------------|
| `magnitude()` | L2 norm of all 5 dimensions |
| `any_exceeds(threshold)` | True if any single dimension exceeds the threshold |
| `max_dimension()` | Returns the highest individual dimension value |

---

## Default Genesis Rules

The kernel boots with 22 genesis rules organized across all three branches.

### Core Constitutional Rules (GOV-001 through GOV-007)

| ID | Description | Branch | Severity | SOP Category |
|----|-------------|--------|----------|--------------|
| GOV-001 | High-risk operations require elevated review | Judicial | Blocking | -- |
| GOV-002 | Security-sensitive actions must not exceed security threshold | Judicial | Blocking | -- |
| GOV-003 | Privacy-impacting operations flagged for review | Legislative | Warning | -- |
| GOV-004 | Novel/unprecedented actions require advisory logging | Executive | Advisory | -- |
| GOV-005 | Filesystem write operations scored for risk | Legislative | Warning | -- |
| GOV-006 | Agent spawn operations require governance clearance | Executive | Blocking | -- |
| GOV-007 | IPC messages between agents logged for audit trail | Judicial | Advisory | -- |

### AI-SDLC SOP Rules: Legislative (SOP-L001 through SOP-L006)

| ID | Description | Severity | SOP Category | SOP Reference |
|----|-------------|----------|--------------|---------------|
| SOP-L001 | AI-IRB approval required before high-impact deployments | Blocking | governance | [SOP-1300-01](https://github.com/AISDLC/AI-SDLC-SOPs/blob/main/sops/SOP-1300-01-AI_IRB_Approval.md) |
| SOP-L002 | Version control and branching policies must be enforced | Warning | governance | [SOP-1003-01](https://github.com/AISDLC/AI-SDLC-SOPs/blob/main/sops/SOP-1003-01-AI_Version_Control.md) |
| SOP-L003 | Requirements must include AI-IRB ethical review | Warning | engineering | [SOP-1040-01](https://github.com/AISDLC/AI-SDLC-SOPs/blob/main/sops/SOP-1040-01-AI_Requirements.md) |
| SOP-L004 | Release planning must follow structured lifecycle gates | Advisory | lifecycle | [SOP-1005-01](https://github.com/AISDLC/AI-SDLC-SOPs/blob/main/sops/SOP-1005-01-AI_Release_Planning.md) |
| SOP-L005 | Data protection and PII handling must comply with policy | Blocking | ethics | [SOP-1303-01](https://github.com/AISDLC/AI-SDLC-SOPs/blob/main/sops/SOP-1303-01-AI_Data_Protection.md) |
| SOP-L006 | Risk register must be maintained and reviewed | Warning | governance | [SOP-1062-01](https://github.com/AISDLC/AI-SDLC-SOPs/blob/main/sops/SOP-1062-01-AI_Risk_Register.md) |

### AI-SDLC SOP Rules: Executive (SOP-E001 through SOP-E005)

| ID | Description | Severity | SOP Category | SOP Reference |
|----|-------------|----------|--------------|---------------|
| SOP-E001 | Secure coding standards must be followed | Warning | engineering | [SOP-1200-01](https://github.com/AISDLC/AI-SDLC-SOPs/blob/main/sops/SOP-1200-01-AI_Secure_Coding.md) |
| SOP-E002 | Deployment requires governance clearance checkpoint | Blocking | lifecycle | [SOP-1220-01](https://github.com/AISDLC/AI-SDLC-SOPs/blob/main/sops/SOP-1220-01-AI_Deployment_Clearance.md) |
| SOP-E003 | Incident response procedures must be documented and followed | Warning | security | [SOP-1008-01](https://github.com/AISDLC/AI-SDLC-SOPs/blob/main/sops/SOP-1008-01-AI_Incident_Response.md) |
| SOP-E004 | Decommissioning must follow structured teardown procedure | Advisory | lifecycle | [SOP-1011-01](https://github.com/AISDLC/AI-SDLC-SOPs/blob/main/sops/SOP-1011-01-AI_Decommissioning.md) |
| SOP-E005 | Third-party AI procurement requires screening | Advisory | governance | [SOP-1004-01](https://github.com/AISDLC/AI-SDLC-SOPs/blob/main/sops/SOP-1004-01-AI_Procurement_Screening.md) |

### AI-SDLC SOP Rules: Judicial (SOP-J001 through SOP-J004)

| ID | Description | Severity | SOP Category | SOP Reference |
|----|-------------|----------|--------------|---------------|
| SOP-J001 | Bias and fairness assessments required for model outputs | Blocking | ethics | [SOP-1301-01](https://github.com/AISDLC/AI-SDLC-SOPs/blob/main/sops/SOP-1301-01-AI_Bias_Fairness.md) |
| SOP-J002 | Explainability documentation required for decision systems | Warning | ethics | [SOP-1302-01](https://github.com/AISDLC/AI-SDLC-SOPs/blob/main/sops/SOP-1302-01-AI_Explainability.md) |
| SOP-J003 | Model drift detection and monitoring must be active | Warning | lifecycle | [SOP-1009-01](https://github.com/AISDLC/AI-SDLC-SOPs/blob/main/sops/SOP-1009-01-AI_Drift_Detection.md) |
| SOP-J004 | Quality records must be maintained for audit compliance | Advisory | quality | [SOP-2002-01](https://github.com/AISDLC/AI-SDLC-SOPs/blob/main/sops/SOP-2002-01-AI_Quality_Records.md) |

### Rule Distribution Summary

| Branch | Count | Blocking | Warning | Advisory |
|--------|-------|----------|---------|----------|
| Legislative | 8 | 3 (GOV-003 is Warning) | 4 | 1 |
| Executive | 7 | 2 | 2 | 3 |
| Judicial | 7 | 3 | 2 | 2 |
| **Total** | **22** | **8** | **8** | **6** |

---

## How Enforcement Works

### Step-by-step: What happens when an agent calls a tool

1. Agent A sends a `ToolCall` message via `A2ARouter::send()`.

2. **Routing gate** (`a2a.rs`): The router extracts the action string
   (e.g., `"tool.shell_exec"`), builds a context JSON with the target and
   layer info, and calls `gate.check(agent_id, action, context)`.

3. The `GovernanceGate::check()` implementation:
   - Extracts the `EffectVector` from `context["effect"]` (defaults to zero).
   - Extracts string context fields into a `HashMap`.
   - Constructs a `GovernanceRequest` and calls `GovernanceEngine::evaluate()`.

4. If the decision is `Deny`, the message is blocked and the sender receives
   a `KernelError::CapabilityDenied` error. Processing stops here.

5. If the decision is `Defer`, the message is still delivered to the target
   inbox. The handler-time gate makes the final call.

6. If the decision is `Permit`, the message is delivered to the target inbox.

7. **Handler gate** (`agent_loop.rs`): When Agent B's loop processes the
   incoming message, it performs a second gate check for protected commands
   (`exec`, `cron.add`, `cron.remove`). For `exec`, it enriches the
   context with the tool name and its `EffectVector` from the tool registry.

8. If both gates permit, the tool executes normally.

9. The governance decision is logged to the ExoChain as a witness event
   (`governance.permit`, `governance.warn`, `governance.defer`, or
   `governance.deny`).

### Decision Logic

The `GovernanceEngine::evaluate()` algorithm:

```
1. Compute magnitude = effect.magnitude()  (L2 norm)
2. Set threshold_exceeded = (magnitude > risk_threshold)
3. For each active rule:
   a. If rule.severity is Blocking or Critical:
      - If threshold_exceeded: mark has_blocking = true, record reason
   b. If rule.severity is Warning:
      - If threshold_exceeded: mark has_warning = true
   c. If rule.severity is Advisory:
      - No action (logged only)
4. Decision:
   a. If has_blocking AND human_approval_required:
      -> EscalateToHuman(reason)
   b. If has_blocking AND NOT human_approval_required:
      -> Deny(reason)
   c. If threshold_exceeded AND has_warning (but no blocking):
      -> PermitWithWarning(message)
   d. Otherwise:
      -> Permit
```

Key implication: **severity alone does not block.** A `Blocking` rule only
triggers denial when `threshold_exceeded` is true. If the effect magnitude
is below the threshold, even `Critical` rules pass. This design means
low-impact actions are never blocked, regardless of how many rules exist.

### Gate Decisions

The `GateDecision` enum maps governance decisions to gate-layer outcomes:

| GovernanceDecision | GateDecision | Routing Layer | Handler Layer |
|---|---|---|---|
| `Permit` | `Permit { token: None }` | Deliver | Execute |
| `PermitWithWarning(w)` | `Permit { token: None }` | Deliver | Execute |
| `EscalateToHuman(r)` | `Defer { reason: r }` | Deliver | Send deferral reply |
| `Deny(r)` | `Deny { reason: r, receipt: None }` | Block | Send error reply |

**Permit tokens.** The `GateDecision::Permit` variant carries an optional
`token: Option<Vec<u8>>` field. For the governance gate, this is always
`None`. When the TileZero feature gate is enabled, the `TileZeroGate`
provides Ed25519-signed `PermitToken`s in this field.

**Deny receipts.** Similarly, `GateDecision::Deny` carries an optional
`receipt: Option<Vec<u8>>` for cryptographic witness receipts (TileZero
feature only).

### Chain Logging of Decisions

When a `ChainManager` is attached to the `GovernanceGate`, every decision
emits a chain event:

| Event Kind | When |
|---|---|
| `governance.permit` | Action permitted (no warnings) |
| `governance.warn` | Action permitted with warning |
| `governance.defer` | Action escalated to human |
| `governance.deny` | Action denied |

Each event payload includes:

```json
{
  "agent_id": "agent-42",
  "action": "tool.shell_exec",
  "effect": {
    "risk": 0.8,
    "fairness": 0.0,
    "privacy": 0.1,
    "novelty": 0.0,
    "security": 0.6
  },
  "threshold_exceeded": true,
  "evaluated_rules": ["GOV-001", "GOV-002", "SOP-J001", "..."]
}
```

### Session lifecycle chain events (WEFT-85 / MW-7)

Conversation sessions are substrate state. Their create / append / destroy
markers flow through the same `chain_event!` → pending buffer → daemon
`ChainManager` path as governance decisions, so a long-running session is
auditable for the full hot loop (not only create→destroy).

| Event Kind | Source | When | Payload |
|---|---|---|---|
| `session.create` | `session` | `SessionManager::get_or_create` creates a new session (cache + disk miss) | `key` |
| `session.append` | `session` | Every successful `SessionManager::append_turn` | `key`, `role`, `turn_count` |
| `session.destroy` | `session` | `SessionManager::delete_session` | `key` |

**Sample-rate cap.** High-volume sessions can flood the chain. Configure
via `SessionManager::with_append_event_every_n(n)`:

- `n = 1` (default): emit one `session.append` per turn (100 turns → 100 events).
- `n = 10`: emit one event per 10 turns (100 turns → 10 events).

Message **content is not** included in the append payload (PII / volume).
Audit completeness for “activity happened” uses `key` + `role` + `turn_count`.

Constants: `EVENT_KIND_SESSION_CREATE` / `EVENT_KIND_SESSION_APPEND` /
`EVENT_KIND_SESSION_DESTROY` in both `clawft_core::chain_event` and
`clawft_kernel::chain` (shared vocabulary).

---

## Customizing Governance

### Creating Custom Rules

```rust
use clawft_kernel::governance::{GovernanceBranch, GovernanceRule, RuleSeverity};

let rule = GovernanceRule {
    id: "CUSTOM-001".into(),
    description: "Production database writes require team-lead approval".into(),
    branch: GovernanceBranch::Executive,
    severity: RuleSeverity::Blocking,
    active: true,
    reference_url: Some("https://internal.example.com/sops/db-writes.md".into()),
    sop_category: Some("data-safety".into()),
};
```

**Field reference:**

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `id` | `String` | Yes | Unique identifier. Convention: `PREFIX-NNN` |
| `description` | `String` | Yes | Human-readable explanation |
| `branch` | `GovernanceBranch` | Yes | `Legislative`, `Executive`, or `Judicial` |
| `severity` | `RuleSeverity` | Yes | `Advisory`, `Warning`, `Blocking`, or `Critical` |
| `active` | `bool` | No (default: true) | Whether the rule is currently evaluated |
| `reference_url` | `Option<String>` | No | URL to full SOP documentation |
| `sop_category` | `Option<String>` | No | Category tag for filtering |

**Severity ordering:** `Advisory < Warning < Blocking < Critical`

### Creating a Custom Governance Body

Implement the `GateBackend` trait to create a completely custom gate:

```rust
use clawft_kernel::gate::{GateBackend, GateDecision};

/// A custom gate that denies all shell execution in production.
pub struct ProductionShellGuard {
    environment: String,
}

impl ProductionShellGuard {
    pub fn new(environment: &str) -> Self {
        Self {
            environment: environment.to_owned(),
        }
    }
}

impl GateBackend for ProductionShellGuard {
    fn check(
        &self,
        agent_id: &str,
        action: &str,
        context: &serde_json::Value,
    ) -> GateDecision {
        // Block all shell tools in production
        if self.environment == "production"
            && (action == "tool.shell_exec"
                || action == "tool.bash"
                || action == "tool.run_command")
        {
            return GateDecision::Deny {
                reason: format!(
                    "shell execution by agent '{agent_id}' is forbidden in production"
                ),
                receipt: None,
            };
        }

        // Defer high-risk actions to human review
        if let Some(risk) = context
            .get("effect")
            .and_then(|e| e.get("risk"))
            .and_then(|r| r.as_f64())
        {
            if risk > 0.9 {
                return GateDecision::Defer {
                    reason: format!("risk score {risk:.2} requires human review"),
                };
            }
        }

        GateDecision::Permit { token: None }
    }
}
```

**Trait requirements:** `GateBackend` requires `Send + Sync`, so your
implementation must be safe to call from multiple threads.

**Wiring into boot.** To use a custom gate, modify the governance gate
construction in `boot.rs`:

```rust
// Replace the default GovernanceGate with your custom gate
let custom_gate: Arc<dyn GateBackend> = Arc::new(
    ProductionShellGuard::new("production")
);
a2a_router.set_gate(custom_gate);
```

Or compose gates by creating a gate that delegates to multiple backends:

```rust
pub struct CompositeGate {
    gates: Vec<Arc<dyn GateBackend>>,
}

impl GateBackend for CompositeGate {
    fn check(
        &self,
        agent_id: &str,
        action: &str,
        context: &serde_json::Value,
    ) -> GateDecision {
        for gate in &self.gates {
            let decision = gate.check(agent_id, action, context);
            if decision.is_deny() {
                return decision;
            }
        }
        GateDecision::Permit { token: None }
    }
}
```

### Modifying the Risk Threshold

The `GovernanceEngine` is constructed with a `risk_threshold` parameter:

```rust
// Open governance -- nothing is blocked
let engine = GovernanceEngine::new(1.0, false);

// Default production setting
let engine = GovernanceEngine::new(0.7, false);

// Strict governance with human escalation
let engine = GovernanceEngine::new(0.3, true);
```

**How threshold affects decisions:**

| Threshold | Effect Magnitude 0.5 | Effect Magnitude 0.8 | Effect Magnitude 1.2 |
|-----------|---------------------|---------------------|---------------------|
| 1.0 (open) | Permit | Permit | Deny (if blocking rules exist) |
| 0.7 (default) | Permit | Deny | Deny |
| 0.3 (strict) | Deny | Deny | Deny |

When `human_approval_required` is `true`, all "Deny" outcomes become
"EscalateToHuman" instead, which maps to `GateDecision::Defer` at the gate
layer.

### Adding Rules at Runtime (K6+)

> **Note:** Runtime rule addition is planned for the K6 kernel phase. The
> current implementation (K2-K4) only supports genesis-time rule loading.
> This section describes the intended design.

Runtime rule changes require a `governance.root.supersede` chain event:

```json
{
  "kind": "governance.root.supersede",
  "source": "governance",
  "payload": {
    "original_genesis_seq": 0,
    "new_version": "2.1.0",
    "new_risk_threshold": 0.5,
    "added_rules": [
      {
        "id": "CUSTOM-001",
        "description": "New rule added at runtime",
        "branch": "Executive",
        "severity": "Blocking"
      }
    ],
    "removed_rule_ids": [],
    "reason": "Security policy update per incident IR-2025-042"
  }
}
```

**Implications for cluster nodes:**

- Nodes that accept the supersede event update their in-memory
  `GovernanceEngine` and continue operating under the new constitution.
- Nodes that reject the supersede event (e.g., because it weakens security
  below their configured minimum) must halt synchronization and alert their
  operator.
- The original genesis sequence is always referenced to maintain the chain
  of custody.

---

## Environment-Specific Governance

### Development (threshold = 1.0)

```rust
let gate = GovernanceGate::new(1.0, false); // or GovernanceGate::open()
```

- All actions permitted regardless of effect magnitude.
- Rules are still evaluated and logged (for debugging), but never block.
- `GovernanceEngine::open()` is a convenience constructor for this mode.
- Chain events are still emitted as `governance.permit` for audit trail.

### Staging (threshold = 0.7)

```rust
let gate = GovernanceGate::new(0.7, false);
```

- This is the default configuration set in `boot.rs`.
- Blocking rules enforce denial when effect magnitude exceeds 0.7.
- Warning rules emit `governance.warn` events.
- Most typical agent operations (file reads, low-risk IPC) pass freely.
- High-risk operations (shell exec, deployment, network access) may be blocked.

### Production (threshold = 0.3, human_approval = true)

```rust
let gate = GovernanceGate::new(0.3, true);
```

- Very strict governance. Even moderate-risk actions exceed the threshold.
- When blocking rules trigger, the decision is `EscalateToHuman` rather
  than `Deny`, giving human operators the chance to approve.
- The handler-time gate sends a deferral reply to the requesting agent
  with `{"deferred": true, "reason": "..."}`.
- Currently, there is no built-in human approval UI -- the deferral reply
  is the mechanism for external systems to implement approval workflows.

---

## Capability Model

The capability model operates at a lower level than governance. While
governance evaluates the *effect* of an action, capabilities determine
whether an agent has the *structural permission* to attempt the action
at all.

### Agent Capabilities

```rust
pub struct AgentCapabilities {
    pub can_spawn: bool,          // Can spawn child processes (default: true)
    pub can_ipc: bool,            // Can send/receive IPC messages (default: true)
    pub can_exec_tools: bool,     // Can execute tools (default: true)
    pub can_network: bool,        // Can make network requests (default: false)
    pub ipc_scope: IpcScope,      // IPC routing restriction (default: All)
    pub resource_limits: ResourceLimits,  // Resource budgets
}
```

### IPC Scope

The `IpcScope` enum controls which message targets an agent can communicate with:

| Variant | Direct PID Messaging | Topic Pub/Sub | Description |
|---------|---------------------|---------------|-------------|
| `All` | Yes (any PID) | Yes (any topic) | Unrestricted IPC |
| `ParentOnly` | No (caller checks parent) | Yes | Only talk to parent process |
| `Restricted(Vec<u64>)` | Yes (listed PIDs only) | Public topics only (`public` / `public.*`) | Explicit PID allowlist; browser default (S7 / WEFT-108) |
| `Topic(Vec<String>)` | No | Yes (listed topics only) | Topic-based IPC only |
| `None` | No | No | All IPC disabled |

Browser-platform agents start with `AgentCapabilities::browser_default()`
(`IpcScope::Restricted([])`). Governance also installs `browser_policy`
rules (`GovernanceRuleType::BrowserPolicy`, genesis BP-001..003) that
deny non-public topic subscribe/publish, spawn, and network for
`platform=browser` independently of effect magnitude. See
[`docs/guides/browser.md`](../guides/browser.md).

### Resource Limits

```rust
pub struct ResourceLimits {
    pub max_memory_bytes: u64,    // Default: 256 MiB
    pub max_cpu_time_ms: u64,     // Default: 300,000 (5 minutes)
    pub max_tool_calls: u64,      // Default: 1,000
    pub max_messages: u64,        // Default: 5,000
}
```

### Sandbox Policy

```rust
pub struct SandboxPolicy {
    pub allow_shell: bool,        // Can execute shell commands (default: false)
    pub allow_network: bool,      // Can make network requests (default: false)
    pub allowed_paths: Vec<String>,   // Filesystem paths allowed
    pub denied_paths: Vec<String>,    // Filesystem paths denied
}
```

### Tool Permissions

```rust
pub struct ToolPermissions {
    pub allow: Vec<String>,           // Tool allowlist (empty = all allowed)
    pub deny: Vec<String>,            // Tool denylist (overrides allow)
    pub service_access: Vec<String>,  // Named services (e.g., "memory", "cron")
}
```

Deny overrides allow. If a tool appears in both lists, it is denied.

### Capability Checking

The `CapabilityChecker` validates structural permissions:

```rust
pub struct CapabilityChecker {
    process_table: Arc<ProcessTable>,
}
```

**Methods:**

| Method | Action Prefix | What It Checks |
|--------|---------------|----------------|
| `check_tool_access(pid, tool_name, perms, sandbox)` | `tool.*` | `can_exec_tools`, deny/allow lists, shell sandbox |
| `check_ipc_target(from_pid, to_pid)` | `ipc.*` | `can_ipc`, IPC scope for target PID |
| `check_ipc_topic(pid, topic)` | `ipc.*` | `can_ipc`, topic scope |
| `check_service_access(pid, service_name, perms)` | `service.*` | Service access list |
| `check_resource_limit(pid, resource)` | -- | Memory, CPU, tool calls, messages |

### How Capabilities Interact with Governance

The `CapabilityGate` wraps `CapabilityChecker` into a `GateBackend`:

```
  Incoming action
       |
       v
  CapabilityGate.check()
       |
       +-- action starts with "tool." --> check_tool_access()
       +-- action starts with "ipc."  --> check_ipc_target()
       +-- action starts with "service." --> check_service_access()
       +-- unknown prefix            --> Permit (default open)
       |
       v
  Ok(()) --> GateDecision::Permit
  Err(e) --> GateDecision::Deny { reason: e.to_string() }
```

The `GovernanceGate` operates at a higher level -- it evaluates effect
vectors against rules and thresholds. Both gates can be composed (via the
routing-time and handler-time slots) for defense-in-depth.

---

## SOP Integration

### AI-SDLC SOPs

The 15 SOP rules (SOP-L001 through SOP-J004) map to the
[AI-SDLC SOP framework](https://github.com/AISDLC/AI-SDLC-SOPs). Each
rule carries a `reference_url` that agents can fetch for the full procedure,
and a `sop_category` for filtering.

**SOP Categories in the genesis ruleset:**

| Category | Count | Description |
|----------|-------|-------------|
| `governance` | 4 | IRB approval, version control, risk register, procurement |
| `ethics` | 3 | Data protection, bias/fairness, explainability |
| `engineering` | 2 | Requirements review, secure coding |
| `lifecycle` | 4 | Release planning, deployment, decommissioning, drift detection |
| `security` | 1 | Incident response |
| `quality` | 1 | Quality records for audit |

**Filtering rules by category:**

```rust
use clawft_kernel::governance::GovernanceRule;

let all_rules: Vec<GovernanceRule> = /* from engine or genesis */;
let ethics_rules = GovernanceRule::filter_by_category(&all_rules, "ethics");
// Returns: [SOP-L005, SOP-J001, SOP-J002]
```

### Adding Your Own SOPs

Create rules with custom `reference_url` and `sop_category` values:

```rust
let custom_sop = GovernanceRule {
    id: "MYORG-SEC-001".into(),
    description: "All API endpoints must use mTLS in production".into(),
    branch: GovernanceBranch::Executive,
    severity: RuleSeverity::Blocking,
    active: true,
    reference_url: Some("https://wiki.myorg.com/sops/mtls-requirement".into()),
    sop_category: Some("network-security".into()),
};

// Add to the governance gate
let gate = GovernanceGate::new(0.7, false)
    .add_rule(custom_sop);

// Later, filter your custom category
let net_sec_rules = GovernanceRule::filter_by_category(&rules, "network-security");
```

Category values are freeform strings. There is no fixed taxonomy -- use
whatever categories make sense for your organization.

---

## Chain Witnessing

### Post-Quantum Transport Protection

Mesh connections use hybrid Noise + ML-KEM-768 key exchange (K6.4b).
This ensures that even if classical X25519 DH is broken by quantum
computers, the ML-KEM-768 shared secret protects all mesh traffic.
Governance decisions transmitted over the mesh are protected by this
hybrid encryption. The KEM capability is negotiated during the Noise
handshake -- nodes advertise `kem_supported` in their handshake payload.

### What Gets Logged

All governance-related chain events use `"governance"` as the source:

| Event Kind | When | Payload |
|---|---|---|
| `governance.genesis` | Boot time, once | Version, threshold, all rules, genesis_seq |
| `governance.rule` | Boot time, per rule | rule_id, branch, severity, genesis_seq |
| `governance.permit` | Action permitted | agent_id, action, effect, evaluated_rules |
| `governance.warn` | Permitted with warning | + `warning` field |
| `governance.defer` | Escalated to human | + `reason` field |
| `governance.deny` | Action denied | + `reason` field |

When the TileZero feature is enabled, additional gate events are logged:

| Event Kind | When | Payload |
|---|---|---|
| `gate.permit` | TileZero permits | agent_id, action, sequence, witness_hash |
| `gate.defer` | TileZero defers | agent_id, action, sequence, witness_hash |
| `gate.deny` | TileZero denies | agent_id, action, sequence, witness_hash |

### Verifying Governance Integrity

To verify the governance genesis exists on the chain:

```rust
let gate: &GovernanceGate = /* from kernel */;

// Returns the genesis sequence number if found
match gate.verify_governance_genesis() {
    Some(seq) => println!("Governance genesis at sequence {seq}"),
    None => println!("No governance genesis found (open governance?)"),
}
```

To audit governance decisions via chain query:

```rust
let chain: &ChainManager = /* from kernel */;
let all_events = chain.tail(0); // all events from beginning

// Find all deny events
let denials: Vec<_> = all_events.iter()
    .filter(|e| e.kind == "governance.deny")
    .collect();

for event in &denials {
    if let Some(payload) = &event.payload {
        println!(
            "DENIED: agent={} action={} reason={}",
            payload["agent_id"],
            payload["action"],
            payload["reason"],
        );
    }
}

// Verify governance genesis
let genesis_events: Vec<_> = all_events.iter()
    .filter(|e| e.kind == "governance.genesis")
    .collect();

assert!(!genesis_events.is_empty(), "governance genesis must be on chain");
let genesis = genesis_events[0].payload.as_ref().unwrap();
assert_eq!(genesis["rule_count"].as_u64().unwrap(), 22);
```

---

## RVF Governance Bridge

When the `exochain` feature is enabled, WeftOS governance maps bidirectionally
to the RVF (RuVector Framework) witness governance model:

| WeftOS Concept | RVF Equivalent |
|---|---|
| `GovernanceDecision::Permit` | `PolicyCheck::Allowed` |
| `GovernanceDecision::PermitWithWarning` | `PolicyCheck::Confirmed` |
| `GovernanceDecision::EscalateToHuman` | `PolicyCheck::Confirmed` |
| `GovernanceDecision::Deny` | `PolicyCheck::Denied` |

Engine mode mapping:

| Condition | RVF GovernanceMode |
|---|---|
| `risk_threshold >= 1.0` | `Autonomous` |
| `human_approval_required` | `Approved` |
| Otherwise | `Restricted` |

```rust
// Get the RVF governance mode
let mode = engine.to_rvf_mode(); // GovernanceMode::Restricted

// Build a full RVF GovernancePolicy
let policy = engine.to_rvf_policy(); // GovernancePolicy::restricted()

// Map a governance result to RVF task outcome
let outcome = result.to_rvf_task_outcome(); // TaskOutcome::Solved / Failed / Skipped
```

---

## API Reference

### Types

**`governance.rs`:**

| Type | Description |
|---|---|
| `GovernanceRule` | A single governance rule with id, description, branch, severity, active flag, SOP fields, `rule_type` |
| `GovernanceRuleType` | `General` (default), `BrowserPolicy` (S7 / WEFT-108) |
| `GovernanceBranch` | `Legislative`, `Executive`, `Judicial` |
| `RuleSeverity` | `Advisory`, `Warning`, `Blocking`, `Critical` (ordered) |
| `EffectVector` | 5D vector: risk, fairness, privacy, novelty, security (all `f64`) |
| `GovernanceDecision` | `Permit`, `PermitWithWarning(String)`, `EscalateToHuman(String)`, `Deny(String)` |
| `GovernanceRequest` | agent_id, action, effect (`EffectVector`), context (`HashMap<String, String>`) |
| `GovernanceResult` | decision, evaluated_rules, effect, threshold_exceeded |
| `GovernanceEngine` | Stateful engine holding rules, risk_threshold, human_approval_required |

**`gate.rs`:**

| Type | Description |
|---|---|
| `GateDecision` | `Permit { token }`, `Defer { reason }`, `Deny { reason, receipt }` |
| `GateBackend` (trait) | `fn check(&self, agent_id, action, context) -> GateDecision` |
| `CapabilityGate` | Gate wrapping `CapabilityChecker` (binary Permit/Deny) |
| `GovernanceGate` | Gate wrapping `GovernanceEngine` (5D effect algebra) |
| `TileZeroGate` | Gate wrapping TileZero (three-way with crypto receipts, `tilezero` feature) |

**`capability.rs`:**

| Type | Description |
|---|---|
| `AgentCapabilities` | Per-agent permission flags and resource limits |
| `IpcScope` | `All`, `ParentOnly`, `Restricted(Vec<u64>)`, `Topic(Vec<String>)`, `None` |
| `ResourceLimits` | max_memory_bytes, max_cpu_time_ms, max_tool_calls, max_messages |
| `SandboxPolicy` | allow_shell, allow_network, allowed_paths, denied_paths |
| `ToolPermissions` | allow list, deny list, service_access list |
| `ResourceType` | `Memory(u64)`, `CpuTime(u64)`, `ConcurrentTools(u32)`, `Messages(u64)` |
| `CapabilityChecker` | Stateful checker backed by `ProcessTable` |

### GovernanceEngine Methods

| Method | Signature | Description |
|---|---|---|
| `new` | `(risk_threshold: f64, human_approval: bool) -> Self` | Create with given threshold |
| `open` | `() -> Self` | Create open engine (threshold = 1.0, no human approval) |
| `add_rule` | `(&mut self, rule: GovernanceRule)` | Add a governance rule |
| `active_rules` | `(&self) -> Vec<&GovernanceRule>` | All rules where `active == true` |
| `rules_by_branch` | `(&self, branch: &GovernanceBranch) -> Vec<&GovernanceRule>` | Filter by branch |
| `evaluate` | `(&self, request: &GovernanceRequest) -> GovernanceResult` | Run evaluation pipeline |
| `risk_threshold` | `(&self) -> f64` | Current threshold |
| `rule_count` | `(&self) -> usize` | Total rules (active + inactive) |

### GovernanceGate Methods

| Method | Signature | Description |
|---|---|---|
| `new` | `(risk_threshold: f64, human_approval: bool) -> Self` | Create with threshold |
| `open` | `() -> Self` | Create open gate |
| `with_chain` | `(self, cm: Arc<ChainManager>) -> Self` | Attach chain for audit logging |
| `add_rule` | `(self, rule: GovernanceRule) -> Self` | Add rule (builder pattern) |
| `engine` | `(&self) -> &GovernanceEngine` | Access inner engine |
| `verify_governance_genesis` | `(&self) -> Option<u64>` | Check genesis exists on chain |

### GateBackend Trait

```rust
pub trait GateBackend: Send + Sync {
    fn check(
        &self,
        agent_id: &str,
        action: &str,
        context: &serde_json::Value,
    ) -> GateDecision;
}
```

---

## FAQ

**Q: What happens if no governance gate is configured?**
A: The kernel operates in open governance mode. All actions are permitted.
This occurs when the `exochain` feature is not enabled, or when no chain
manager is available at boot. The kernel logs: "Governance: no chain
available, using open governance".

**Q: Can I disable a specific genesis rule without modifying boot.rs?**
A: Currently no. All 22 genesis rules are hardcoded in `boot.rs`. In K6+,
the `governance.root.supersede` mechanism will allow runtime deactivation.
As a workaround, you can set `risk_threshold = 1.0` to make all rules
effectively advisory.

**Q: Do Advisory rules ever block anything?**
A: No. Advisory rules are always evaluated (they appear in
`evaluated_rules`) but never contribute to blocking or warning decisions.
They exist purely for audit trail purposes.

**Q: What is the difference between GovernanceGate and CapabilityGate?**
A: `CapabilityGate` checks structural permissions -- can this agent run
tools at all? Can it send IPC to this PID? It gives binary Permit/Deny.
`GovernanceGate` checks effect-based policy -- is this action too risky
given the current threshold and rules? It supports the full Permit/Warn/
Escalate/Deny spectrum. Both can be active simultaneously through the
dual-layer enforcement model.

**Q: How do I test governance rules locally?**
A: Create a `GovernanceEngine` directly in a test:

```rust
let mut engine = GovernanceEngine::new(0.5, false);
engine.add_rule(your_rule);

let request = GovernanceRequest {
    agent_id: "test-agent".into(),
    action: "tool.deploy".into(),
    effect: EffectVector { risk: 0.8, ..Default::default() },
    context: Default::default(),
};

let result = engine.evaluate(&request);
assert!(matches!(result.decision, GovernanceDecision::Deny(_)));
```

**Q: Can I run both TileZero and Governance gates simultaneously?**
A: Not directly in the same gate slot. The A2A router accepts a single
`Arc<dyn GateBackend>`. To compose both, create a `CompositeGate` (see
[Creating a Custom Governance Body](#creating-a-custom-governance-body))
that delegates to both backends.

**Q: What is the `governance` feature gate vs `exochain`?**
A: The `governance` and `exochain` features control compilation of the
governance engine. Without them, `GovernanceEngine::evaluate()` returns
`GovernanceDecision::Permit` unconditionally. All types (`GovernanceRule`,
`EffectVector`, etc.) compile unconditionally for use as data structures.

**Q: How does the RVF bridge affect governance decisions?**
A: It does not affect decisions. The RVF bridge is a one-way mapping:
WeftOS governance decisions are translated to RVF equivalents for
recording in witness bundles. The RVF side is read-only from governance's
perspective.
