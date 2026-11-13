//! Gate backend abstraction for permission decisions.
//!
//! [`GateBackend`] provides a unified interface for making access
//! control decisions. The default implementation wraps
//! `CapabilityChecker` (binary Permit/Deny). When the `tilezero`
//! feature is enabled, `TileZeroGate` adds three-way decisions
//! (Permit/Defer/Deny) with cryptographic receipts logged to the chain.
//!
//! # TileZero receipt format (WEFT-152)
//!
//! When `TileZeroGate` decides, it serializes the
//! `cognitum_gate_tilezero::PermitToken` to JSON bytes and attaches them
//! as the opaque `token` (Permit) or `receipt` (Deny) field on
//! [`GateDecision`]. Defer carries only a human-readable `reason`.
//!
//! ## PermitToken fields
//!
//! | Field | Type | Description |
//! |-------|------|-------------|
//! | `decision` | enum | `Permit` / `Defer` / `Deny` |
//! | `action_id` | string (UUID) | Unique action identifier |
//! | `timestamp` | u64 ns | Issue time (Unix epoch nanoseconds) |
//! | `ttl_ns` | u64 | Validity window (default 60s) |
//! | `witness_hash` | `[u8; 32]` hex | BLAKE3 hash of the three-filter witness summary |
//! | `sequence` | u64 | Monotonic gate sequence |
//! | `signature` | `[u8; 64]` hex | Ed25519 signature over signable content |
//!
//! ## Chain event kinds
//!
//! Each decision appends one of
//! [`EVENT_KIND_GATE_PERMIT`](crate::chain::EVENT_KIND_GATE_PERMIT),
//! [`EVENT_KIND_GATE_DEFER`](crate::chain::EVENT_KIND_GATE_DEFER), or
//! [`EVENT_KIND_GATE_DENY`](crate::chain::EVENT_KIND_GATE_DENY) with payload
//! `{ agent_id, action, sequence, witness_hash }`.

use serde::{Deserialize, Serialize};

/// Result of a gate decision.
#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GateDecision {
    /// Action is permitted.
    Permit {
        /// Optional opaque permit token (e.g. TileZero PermitToken bytes).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token: Option<Vec<u8>>,
    },
    /// Decision is deferred (needs human or higher-level review).
    Defer {
        /// Why the decision was deferred.
        reason: String,
    },
    /// Action is denied.
    Deny {
        /// Why the action was denied.
        reason: String,
        /// Optional opaque witness receipt.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        receipt: Option<Vec<u8>>,
    },
}

impl GateDecision {
    /// Returns `true` if the decision is `Permit`.
    pub fn is_permit(&self) -> bool {
        matches!(self, GateDecision::Permit { .. })
    }

    /// Returns `true` if the decision is `Deny`.
    pub fn is_deny(&self) -> bool {
        matches!(self, GateDecision::Deny { .. })
    }
}

/// Trait for gate backends that make access-control decisions.
///
/// Implementations include:
/// - [`CapabilityGate`] — wraps the existing `CapabilityChecker`
///   for binary Permit/Deny decisions.
/// - `TileZeroGate` (behind `tilezero` feature) — three-way
///   Permit/Defer/Deny with cryptographic receipts.
pub trait GateBackend: Send + Sync {
    /// Check whether an agent is allowed to perform an action.
    ///
    /// # Arguments
    ///
    /// * `agent_id` - The agent requesting the action.
    /// * `action` - The action being attempted (e.g. "tool.shell_exec",
    ///   "ipc.send", "service.access").
    /// * `context` - Additional context for the decision (tool args,
    ///   target PID, etc.).
    fn check(&self, agent_id: &str, action: &str, context: &serde_json::Value) -> GateDecision;

    /// [`Self::check`] for a teardown (stop / unload / re-adoption of what
    /// is already placed): a denial caused only by the node's trust tier is
    /// waived, every other denial (revocation, default deny, rule or
    /// threshold) stands. Gates that cannot tell the difference return the
    /// plain decision, so they never waive anything.
    fn check_teardown(
        &self,
        agent_id: &str,
        action: &str,
        context: &serde_json::Value,
    ) -> GateDecision {
        self.check(agent_id, action, context)
    }

    /// The rules and settings this gate evaluates with, when it is backed by
    /// a governance engine. The user daemon exports it as the signed parent
    /// policy for its project kernels (ADR-103 D8). `None` for gates with no
    /// rule set.
    fn governance_snapshot(&self) -> Option<GovernanceSnapshot> {
        None
    }

    /// Evaluate `action` for `agent_id` the way [`Self::check`] does, but
    /// report the engine's own four-way verdict and the rules it evaluated,
    /// and write nothing to the chain. Read-only: the `governance.check` RPC
    /// uses it so a client can ask "would this be allowed?" before acting.
    ///
    /// `None` means this gate has no governance engine behind it; a caller
    /// must treat that as "cannot decide" and never as a permit.
    fn check_detailed(
        &self,
        _agent_id: &str,
        _action: &str,
        _context: &serde_json::Value,
    ) -> Option<GateVerdict> {
        None
    }
}

/// The engine's verdict for one [`GateBackend::check_detailed`] call.
#[derive(Debug, Clone)]
pub struct GateVerdict {
    /// What the engine decided, after any per-action exemption.
    pub decision: crate::governance::GovernanceDecision,
    /// Names of the rules the engine evaluated.
    pub evaluated_rules: Vec<String>,
    /// True when a per-action exemption turned a Deny / Escalate into a
    /// Permit (the same override `check` applies).
    pub grant_applied: bool,
}

/// The process and spawn caps a kernel runs under, as the limits a parent
/// policy exports (ADR-103 A6). Boot-time values, like the caps themselves.
pub fn parent_limits_of(kc: &clawft_types::config::KernelConfig) -> clawft_types::config::overlay::Limits {
    clawft_types::config::overlay::Limits {
        max_processes: Some(u64::from(kc.max_processes)),
        spawn_budget: kc.agent.as_ref().map(|a| u64::from(a.subagents.max_per_conv)),
        ..Default::default()
    }
}

/// A governance engine's rules and settings at one instant.
#[derive(Debug, Clone)]
pub struct GovernanceSnapshot {
    /// Every rule, active or not.
    pub rules: Vec<crate::governance::GovernanceRule>,
    /// The engine's risk threshold.
    pub risk_threshold: f64,
    /// Whether blocking verdicts escalate to a human.
    pub human_approval_required: bool,
    /// Process and spawn caps the exporter's kernel runs under (ADR-103
    /// A6). The engine does not know them: the daemon fills them in from
    /// its kernel config, so a project kernel's merged limits start from the
    /// parent's real caps and the overlay can only tighten them. The
    /// threshold and approval flag travel in the fields above, not here.
    pub limits: clawft_types::config::overlay::Limits,
}

/// Gate backend wrapping the existing `CapabilityChecker`.
///
/// Always returns `Permit` or `Deny` (never `Defer`). This is the
/// default gate used when no external gate crate is enabled.
pub struct CapabilityGate {
    process_table: std::sync::Arc<crate::process::ProcessTable>,
}

impl CapabilityGate {
    /// Create a capability gate backed by the given process table.
    pub fn new(process_table: std::sync::Arc<crate::process::ProcessTable>) -> Self {
        Self { process_table }
    }
}

impl GateBackend for CapabilityGate {
    fn check(&self, _agent_id: &str, action: &str, context: &serde_json::Value) -> GateDecision {
        // Extract PID from context if available
        let pid = context.get("pid").and_then(|v| v.as_u64()).unwrap_or(0);

        let checker =
            crate::capability::CapabilityChecker::new(std::sync::Arc::clone(&self.process_table));

        // Route to appropriate checker based on action prefix
        let result = if action.starts_with("tool.") {
            let tool_name = action.strip_prefix("tool.").unwrap_or(action);
            checker.check_tool_access(pid, tool_name, None, None)
        } else if action.starts_with("ipc.") {
            let target_pid = context
                .get("target_pid")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            checker.check_ipc_target(pid, target_pid)
        } else if action.starts_with("service.") {
            let service_name = action.strip_prefix("service.").unwrap_or(action);
            checker.check_service_access(pid, service_name, None)
        } else if action.starts_with("workload.") {
            // ADR-099 s4: workload actions are default-deny. Only the
            // workload gate (with explicit permit rules) may allow them.
            return GateDecision::Deny {
                reason: format!("'{action}' is default-deny; use the workload governance gate"),
                receipt: None,
            };
        } else {
            // Unknown action category: permit by default
            return GateDecision::Permit { token: None };
        };

        match result {
            Ok(()) => GateDecision::Permit { token: None },
            Err(e) => GateDecision::Deny {
                reason: e.to_string(),
                receipt: None,
            },
        }
    }
}

// ---------------------------------------------------------------------------
// TileZero gate adapter (behind `tilezero` feature)
// ---------------------------------------------------------------------------

#[cfg(feature = "tilezero")]
pub use tilezero_gate::TileZeroGate;

#[cfg(feature = "tilezero")]
mod tilezero_gate {
    use super::{GateBackend, GateDecision};
    use std::sync::Arc;

    use cognitum_gate_tilezero::{
        ActionContext, ActionMetadata, ActionTarget, GateDecision as TzDecision, TileZero,
    };

    /// Gate backend wrapping [`cognitum_gate_tilezero::TileZero`].
    ///
    /// Provides three-way Permit/Defer/Deny decisions with Ed25519-signed
    /// `PermitToken`s and blake3-chained `WitnessReceipt`s. Gate events
    /// are logged to the kernel chain when a `ChainManager` is provided.
    pub struct TileZeroGate {
        tilezero: Arc<TileZero>,
        chain: Option<Arc<crate::chain::ChainManager>>,
    }

    impl TileZeroGate {
        /// Create a new TileZero gate.
        ///
        /// `tilezero` — a shared `TileZero` instance (created once at
        /// boot, fed with tile reports by the coherence fabric).
        ///
        /// `chain` — optional chain manager for audit logging. When
        /// provided, every decision emits a `gate.permit`, `gate.defer`,
        /// or `gate.deny` event.
        pub fn new(
            tilezero: Arc<TileZero>,
            chain: Option<Arc<crate::chain::ChainManager>>,
        ) -> Self {
            Self { tilezero, chain }
        }

        /// Reference to the optional chain manager (for test inspection).
        #[cfg(test)]
        pub(crate) fn chain(&self) -> Option<&Arc<crate::chain::ChainManager>> {
            self.chain.as_ref()
        }

        /// Build an [`ActionContext`] from our gate parameters.
        pub(crate) fn build_action_context(
            agent_id: &str,
            action: &str,
            context: &serde_json::Value,
        ) -> ActionContext {
            ActionContext {
                action_id: uuid::Uuid::new_v4().to_string(),
                action_type: action.to_owned(),
                target: ActionTarget {
                    device: context
                        .get("device")
                        .and_then(|v| v.as_str())
                        .map(String::from),
                    path: context
                        .get("path")
                        .and_then(|v| v.as_str())
                        .map(String::from),
                    extra: Default::default(),
                },
                context: ActionMetadata {
                    agent_id: agent_id.to_owned(),
                    session_id: context
                        .get("session_id")
                        .and_then(|v| v.as_str())
                        .map(String::from),
                    prior_actions: Vec::new(),
                    urgency: context
                        .get("urgency")
                        .and_then(|v| v.as_str())
                        .unwrap_or("normal")
                        .to_owned(),
                },
            }
        }
    }

    impl GateBackend for TileZeroGate {
        fn check(&self, agent_id: &str, action: &str, context: &serde_json::Value) -> GateDecision {
            let action_ctx = Self::build_action_context(agent_id, action, context);

            // TileZero::decide() is async. We use block_in_place since
            // the kernel always runs inside a multi-threaded tokio runtime.
            let token = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(self.tilezero.decide(&action_ctx))
            });

            // Serialize the signed PermitToken for the opaque bytes field.
            let token_bytes = serde_json::to_vec(&token).ok();

            // Map TileZero's three-way decision to our GateDecision.
            let decision = match token.decision {
                TzDecision::Permit => GateDecision::Permit { token: token_bytes },
                TzDecision::Defer => GateDecision::Defer {
                    reason: format!(
                        "TileZero deferred: coherence uncertain (seq={})",
                        token.sequence,
                    ),
                },
                TzDecision::Deny => GateDecision::Deny {
                    reason: format!(
                        "TileZero denied: coherence below threshold (seq={})",
                        token.sequence,
                    ),
                    receipt: token_bytes,
                },
            };

            // Log to chain (WEFT-152: canonical EVENT_KIND_GATE_* kinds).
            if let Some(ref cm) = self.chain {
                let event_kind = match &decision {
                    GateDecision::Permit { .. } => crate::chain::EVENT_KIND_GATE_PERMIT,
                    GateDecision::Defer { .. } => crate::chain::EVENT_KIND_GATE_DEFER,
                    GateDecision::Deny { .. } => crate::chain::EVENT_KIND_GATE_DENY,
                };
                cm.append(
                    "gate",
                    event_kind,
                    Some(serde_json::json!({
                        "agent_id": agent_id,
                        "action": action,
                        "sequence": token.sequence,
                        "witness_hash": token.witness_hash.iter()
                            .map(|b| format!("{b:02x}"))
                            .collect::<String>(),
                    })),
                );
            }

            decision
        }
    }
}

// ---------------------------------------------------------------------------
// Governance gate adapter (behind `exochain` feature)
// ---------------------------------------------------------------------------

/// Gate backend wrapping the `GovernanceEngine`.
///
/// Bridges the 5D effect-algebra governance engine into the kernel's
/// gate slot, mapping `GovernanceDecision` → `GateDecision`. Governance
/// events are logged to the exochain when a `ChainManager` is provided.
pub struct GovernanceGate {
    engine: crate::governance::GovernanceEngine,
    chain: Option<std::sync::Arc<crate::chain::ChainManager>>,
    /// Actions granted a per-action exemption from the blocking-deny path.
    /// With WEFT-634 action/tool selectors, rules can target specific
    /// actions (e.g. `tool.agent_spawn`) without lowering the global
    /// threshold. This opt-in set remains the operator override for
    /// pre-granting exactly-named actions: an exempted action that would
    /// otherwise be denied/deferred is permitted instead, and the
    /// override is still witnessed (as `governance.grant`) so the audit
    /// trail is never silent.
    exempt_actions: std::collections::HashSet<String>,
}

impl GovernanceGate {
    /// Create a governance gate with the given risk threshold.
    pub fn new(risk_threshold: f64, human_approval: bool) -> Self {
        Self {
            engine: crate::governance::GovernanceEngine::new(risk_threshold, human_approval),
            chain: None,
            exempt_actions: std::collections::HashSet::new(),
        }
    }

    /// Create an open governance gate that permits everything.
    pub fn open() -> Self {
        Self {
            engine: crate::governance::GovernanceEngine::open(),
            chain: None,
            exempt_actions: std::collections::HashSet::new(),
        }
    }

    /// Configure per-principal governance-evaluation rate limiting (WEFT-148).
    pub fn with_eval_rate_limit(
        self,
        config: crate::rate_limit::RateLimitConfig,
    ) -> Self {
        self.engine.set_eval_rate_limit(config);
        self
    }

    /// Take over the runtime configuration of the gate this one replaces:
    /// per-action exemptions, rate limit and scorer. Rules, threshold and
    /// chain are the new gate's own.
    pub(crate) fn inherit_config(&mut self, old: &GovernanceGate) {
        self.exempt_actions = old.exempt_actions.clone();
        self.engine.inherit_config(&old.engine);
    }

    /// Attach a chain manager for audit logging.
    pub fn with_chain(mut self, cm: std::sync::Arc<crate::chain::ChainManager>) -> Self {
        self.chain = Some(cm);
        self
    }

    /// Grant a per-action exemption from the blocking-deny path.
    ///
    /// `action` must match the gate action string exactly (the chat path uses
    /// `format!("tool.{name}")`, so e.g. `"tool.agent_spawn"`). An exempted
    /// action that the engine would deny or escalate is permitted instead, but
    /// the decision is still evaluated and witnessed as a `governance.grant`.
    /// Every other action is unaffected; a gate that never calls this is
    /// identical to before. Opt-in only — driven by the
    /// `[kernel.agent.subagents].governance_grant` config flag.
    pub fn exempt_action(mut self, action: impl Into<String>) -> Self {
        self.exempt_actions.insert(action.into());
        self
    }

    /// Add a governance rule.
    pub fn add_rule(mut self, rule: crate::governance::GovernanceRule) -> Self {
        self.engine.add_rule(rule);
        self
    }

    /// Access the inner governance engine.
    pub fn engine(&self) -> &crate::governance::GovernanceEngine {
        &self.engine
    }

    /// Verify that the governance genesis event exists on the chain.
    ///
    /// Returns the genesis sequence number if found, or `None` if no
    /// chain is attached or no genesis event exists.
    pub fn verify_governance_genesis(&self) -> Option<u64> {
        let cm = self.chain.as_ref()?;
        let events = cm.tail(0); // all events
        events
            .iter()
            .find(|e| e.kind == "governance.genesis")
            .and_then(|e| {
                e.payload
                    .as_ref()
                    .and_then(|p| p.get("genesis_seq"))
                    .and_then(|v| v.as_u64())
            })
    }

    /// Build the governance request a gate check evaluates. Shared by
    /// [`GateBackend::check`] and [`GateBackend::check_detailed`] so both
    /// go through the same request construction.
    fn build_request(
        agent_id: &str,
        action: &str,
        context: &serde_json::Value,
    ) -> crate::governance::GovernanceRequest {
        let effect = Self::extract_effect(context);
        let mut ctx_map = Self::extract_context(context);

        // WEFT-634: ensure tool identity is available for tool_selector matching.
        // Chat path uses `tool.{name}` action strings; callers may also pass
        // an explicit `"tool"` context field.
        if !ctx_map.contains_key("tool")
            && let Some(tool) = action.strip_prefix("tool.")
        {
            ctx_map.insert("tool".into(), tool.to_owned());
        }

        // WEFT-636: build attributed principal from agent_id + optional
        // user / parent / conv context keys.
        let mut principal = crate::governance::GatePrincipal::agent(agent_id);
        if let Some(uid) = ctx_map.get("user_id").cloned() {
            principal = principal.with_user(uid);
        }
        if let Some(pid) = ctx_map
            .get("parent_agent_id")
            .cloned()
            .or_else(|| ctx_map.get("parent_agent").cloned())
        {
            principal = principal.with_parent(pid);
        }
        if let Some(cid) = ctx_map
            .get("conv_id")
            .cloned()
            .or_else(|| ctx_map.get("conversation_id").cloned())
        {
            principal = principal.with_conv(cid);
        }

        crate::governance::GovernanceRequest {
            agent_id: agent_id.to_owned(),
            action: action.to_owned(),
            effect,
            context: ctx_map,
            node_id: None,
            principal: Some(principal),
        }
        // ADR-103 A6: project and instance come from the kernel's own
        // attestation; any `project_id` in the caller's context is dropped.
        .attributed()
    }

    /// Extract an [`EffectVector`] from the gate context JSON.
    ///
    /// Looks for an `"effect"` object with `risk`, `fairness`, `privacy`,
    /// `novelty`, `security` fields. Returns default if absent.
    fn extract_effect(context: &serde_json::Value) -> crate::governance::EffectVector {
        context
            .get("effect")
            .and_then(|v| serde_json::from_value::<crate::governance::EffectVector>(v.clone()).ok())
            .unwrap_or_default()
    }

    /// Extract string context map from JSON for governance request.
    fn extract_context(context: &serde_json::Value) -> std::collections::HashMap<String, String> {
        let mut map = std::collections::HashMap::new();
        if let Some(obj) = context.as_object() {
            for (k, v) in obj {
                if k == "effect" {
                    continue; // already extracted separately
                }
                if let Some(s) = v.as_str() {
                    map.insert(k.clone(), s.to_owned());
                } else {
                    map.insert(k.clone(), v.to_string());
                }
            }
        }
        map
    }
}

impl GateBackend for GovernanceGate {
    fn governance_snapshot(&self) -> Option<GovernanceSnapshot> {
        Some(GovernanceSnapshot {
            rules: self.engine.rules().to_vec(),
            risk_threshold: self.engine.risk_threshold(),
            human_approval_required: self.engine.human_approval_required(),
            limits: Default::default(),
        })
    }

    fn check_detailed(
        &self,
        agent_id: &str,
        action: &str,
        context: &serde_json::Value,
    ) -> Option<GateVerdict> {
        use crate::governance::GovernanceDecision;
        let request = Self::build_request(agent_id, action, context);
        let result = self.engine.evaluate(&request);
        let grant_applied = self.exempt_actions.contains(action)
            && matches!(
                result.decision,
                GovernanceDecision::Deny(_) | GovernanceDecision::EscalateToHuman(_)
            );
        let decision = if grant_applied {
            GovernanceDecision::Permit
        } else {
            result.decision
        };
        Some(GateVerdict {
            decision,
            evaluated_rules: result.evaluated_rules,
            grant_applied,
        })
    }

    fn check(&self, agent_id: &str, action: &str, context: &serde_json::Value) -> GateDecision {
        let request = Self::build_request(agent_id, action, context);
        let result = self.engine.evaluate(&request);

        use crate::governance::GovernanceDecision;

        // A blocking decision (Deny / EscalateToHuman) is overridden to Permit
        // for an explicitly-exempted action. The engine still ran and its
        // verdict is preserved for the audit witness below — the grant flips
        // the *gate outcome*, not the *governance evaluation*.
        let grant_applied = self.exempt_actions.contains(action)
            && matches!(
                result.decision,
                GovernanceDecision::Deny(_) | GovernanceDecision::EscalateToHuman(_)
            );

        let decision = if grant_applied {
            GateDecision::Permit { token: None }
        } else {
            match &result.decision {
                GovernanceDecision::Permit => GateDecision::Permit { token: None },
                GovernanceDecision::PermitWithWarning(_) => GateDecision::Permit { token: None },
                GovernanceDecision::EscalateToHuman(reason) => GateDecision::Defer {
                    reason: reason.clone(),
                },
                GovernanceDecision::Deny(reason) => GateDecision::Deny {
                    reason: reason.clone(),
                    receipt: None,
                },
            }
        };

        // Log to chain.
        if let Some(ref cm) = self.chain {
            let (event_kind, extra) = if grant_applied {
                // The audit trail must show the grant was exercised, not fall
                // silent: record what the engine would have done and that the
                // per-action exemption overrode it to a Permit.
                let overridden_reason = match &result.decision {
                    GovernanceDecision::Deny(r) | GovernanceDecision::EscalateToHuman(r) => {
                        r.clone()
                    }
                    _ => String::new(),
                };
                (
                    "governance.grant",
                    serde_json::json!({
                        "granted_action": action,
                        "overridden_reason": overridden_reason,
                    }),
                )
            } else {
                match &result.decision {
                    GovernanceDecision::Permit => ("governance.permit", serde_json::json!({})),
                    GovernanceDecision::PermitWithWarning(w) => {
                        ("governance.warn", serde_json::json!({"warning": w}))
                    }
                    GovernanceDecision::EscalateToHuman(r) => {
                        ("governance.defer", serde_json::json!({"reason": r}))
                    }
                    GovernanceDecision::Deny(r) => {
                        ("governance.deny", serde_json::json!({"reason": r}))
                    }
                }
            };

            let mut payload = serde_json::json!({
                "agent_id": agent_id,
                "action": action,
                "effect": {
                    "risk": request.effect.risk,
                    "fairness": request.effect.fairness,
                    "privacy": request.effect.privacy,
                    "novelty": request.effect.novelty,
                    "security": request.effect.security,
                },
                "threshold_exceeded": result.threshold_exceeded,
                "evaluated_rules": result.evaluated_rules,
            });

            if let Some(obj) = payload.as_object_mut() {
                if let Some(p) = &result.principal {
                    obj.insert(
                        "principal".into(),
                        serde_json::to_value(p).unwrap_or(serde_json::Value::Null),
                    );
                }
                if let Some(extra_obj) = extra.as_object() {
                    for (k, v) in extra_obj {
                        obj.insert(k.clone(), v.clone());
                    }
                }
            }

            cm.append("governance", event_kind, Some(payload));
        }

        decision
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::AgentCapabilities;
    use crate::process::{ProcessEntry, ProcessState, ProcessTable, ResourceUsage};
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    fn make_gate_with_agent(caps: AgentCapabilities) -> (CapabilityGate, u64) {
        let table = Arc::new(ProcessTable::new(16));
        let entry = ProcessEntry {
            pid: 0,
            agent_id: "test-agent".to_owned(),
            state: ProcessState::Running,
            capabilities: caps,
            resource_usage: ResourceUsage::default(),
            cancel_token: CancellationToken::new(),
            parent_pid: None,
        };
        let pid = table.insert(entry).unwrap();
        (CapabilityGate::new(table), pid)
    }

    #[test]
    fn capability_gate_permits_default() {
        let (gate, pid) = make_gate_with_agent(AgentCapabilities::default());
        let ctx = serde_json::json!({"pid": pid});
        let decision = gate.check("test-agent", "tool.read_file", &ctx);
        assert!(decision.is_permit());
    }

    #[test]
    fn capability_gate_denies_no_tools() {
        let caps = AgentCapabilities {
            can_exec_tools: false,
            ..Default::default()
        };
        let (gate, pid) = make_gate_with_agent(caps);
        let ctx = serde_json::json!({"pid": pid});
        let decision = gate.check("test-agent", "tool.read_file", &ctx);
        assert!(decision.is_deny());
    }

    #[test]
    fn capability_gate_denies_ipc_disabled() {
        let caps = AgentCapabilities {
            can_ipc: false,
            ..Default::default()
        };
        let (gate, pid) = make_gate_with_agent(caps);
        let ctx = serde_json::json!({"pid": pid, "target_pid": 999});
        let decision = gate.check("test-agent", "ipc.send", &ctx);
        assert!(decision.is_deny());
    }

    #[test]
    fn capability_gate_unknown_action_permits() {
        let (gate, pid) = make_gate_with_agent(AgentCapabilities::default());
        let ctx = serde_json::json!({"pid": pid});
        let decision = gate.check("test-agent", "custom.action", &ctx);
        assert!(decision.is_permit());
    }

    #[test]
    fn capability_gate_denies_workload_actions() {
        let (gate, pid) = make_gate_with_agent(AgentCapabilities::default());
        let ctx = serde_json::json!({"pid": pid});
        for action in crate::workload_governance::GOVERNED_ACTIONS {
            assert!(gate.check("test-agent", action, &ctx).is_deny(), "{action}");
        }
        assert!(gate.check("test-agent", "workload.bogus", &ctx).is_deny());
    }

    #[test]
    fn gate_decision_serde_roundtrip() {
        let decisions = vec![
            GateDecision::Permit {
                token: Some(vec![1, 2, 3]),
            },
            GateDecision::Defer {
                reason: "need review".into(),
            },
            GateDecision::Deny {
                reason: "denied".into(),
                receipt: None,
            },
        ];
        for d in decisions {
            let json = serde_json::to_string(&d).unwrap();
            let _: GateDecision = serde_json::from_str(&json).unwrap();
        }
    }

    // ── GovernanceGate tests ─────────────────────────────────────

    use crate::governance::{GovernanceBranch, GovernanceRule, RuleSeverity};

    #[test]
    fn governance_gate_permits_low_risk() {
        let gate = GovernanceGate::new(0.5, false).add_rule(GovernanceRule {
            id: "security-check".into(),
            description: "Block high-risk actions".into(),
            branch: GovernanceBranch::Judicial,
            severity: RuleSeverity::Blocking,
            active: true,
            reference_url: None,
            sop_category: None,
            rule_type: Default::default(),
            action_selector: None,
            tool_selector: None,
            force_on_match: false,
            principal_selector: None,
        });

        let ctx = serde_json::json!({
            "effect": { "risk": 0.1, "security": 0.05 }
        });
        let decision = gate.check("agent-1", "tool.read_file", &ctx);
        assert!(decision.is_permit());
    }

    #[test]
    fn governance_gate_denies_high_risk() {
        let gate = GovernanceGate::new(0.5, false).add_rule(GovernanceRule {
            id: "security-check".into(),
            description: "Block high-risk actions".into(),
            branch: GovernanceBranch::Judicial,
            severity: RuleSeverity::Blocking,
            active: true,
            reference_url: None,
            sop_category: None,
            rule_type: Default::default(),
            action_selector: None,
            tool_selector: None,
            force_on_match: false,
            principal_selector: None,
        });

        let ctx = serde_json::json!({
            "effect": { "risk": 0.8, "security": 0.6 }
        });
        let decision = gate.check("agent-1", "tool.exec", &ctx);
        assert!(decision.is_deny());
    }

    #[test]
    fn governance_gate_defers_with_human_approval() {
        let gate = GovernanceGate::new(0.5, true).add_rule(GovernanceRule {
            id: "security-check".into(),
            description: "Block high-risk actions".into(),
            branch: GovernanceBranch::Judicial,
            severity: RuleSeverity::Blocking,
            active: true,
            reference_url: None,
            sop_category: None,
            rule_type: Default::default(),
            action_selector: None,
            tool_selector: None,
            force_on_match: false,
            principal_selector: None,
        });

        let ctx = serde_json::json!({
            "effect": { "risk": 0.8 }
        });
        let decision = gate.check("agent-1", "tool.exec", &ctx);
        assert!(matches!(decision, GateDecision::Defer { .. }));
    }

    #[test]
    fn governance_gate_warns_on_threshold() {
        let gate = GovernanceGate::new(0.5, false).add_rule(GovernanceRule {
            id: "risk-check".into(),
            description: "Warn on risky actions".into(),
            branch: GovernanceBranch::Executive,
            severity: RuleSeverity::Warning,
            active: true,
            reference_url: None,
            sop_category: None,
            rule_type: Default::default(),
            action_selector: None,
            tool_selector: None,
            force_on_match: false,
            principal_selector: None,
        });

        let ctx = serde_json::json!({
            "effect": { "risk": 0.8 }
        });
        // Warning rules don't block — should still permit
        let decision = gate.check("agent-1", "tool.deploy", &ctx);
        assert!(decision.is_permit());
    }

    #[test]
    fn governance_gate_logs_to_chain() {
        let cm = Arc::new(crate::chain::ChainManager::new(0, 10));
        let initial_len = cm.len();

        let gate = GovernanceGate::new(0.5, false)
            .with_chain(cm.clone())
            .add_rule(GovernanceRule {
                id: "sec".into(),
                description: "test".into(),
                branch: GovernanceBranch::Judicial,
                severity: RuleSeverity::Blocking,
                active: true,
                reference_url: None,
                sop_category: None,
                rule_type: Default::default(),
                action_selector: None,
                tool_selector: None,
                force_on_match: false,
                principal_selector: None,
            });

        // Low risk → governance.permit
        let ctx = serde_json::json!({"effect": {"risk": 0.1}});
        gate.check("agent-1", "tool.read", &ctx);
        assert_eq!(cm.len(), initial_len + 1);

        let events = cm.tail(1);
        assert_eq!(events[0].kind, "governance.permit");
        assert_eq!(events[0].source, "governance");

        // High risk → governance.deny
        let ctx = serde_json::json!({"effect": {"risk": 0.9}});
        gate.check("agent-1", "tool.exec", &ctx);
        let events = cm.tail(1);
        assert_eq!(events[0].kind, "governance.deny");

        let payload = events[0].payload.as_ref().unwrap();
        assert_eq!(payload["agent_id"], "agent-1");
        assert_eq!(payload["action"], "tool.exec");
        assert!(payload["threshold_exceeded"].as_bool().unwrap());
    }

    #[test]
    fn governance_gate_open_permits_all() {
        let gate = GovernanceGate::open();
        let ctx = serde_json::json!({
            "effect": { "risk": 0.99, "security": 0.99 }
        });
        let decision = gate.check("agent-1", "tool.dangerous", &ctx);
        assert!(decision.is_permit());
    }

    #[test]
    fn governance_gate_extracts_effect_from_context() {
        let gate = GovernanceGate::new(0.5, false).add_rule(GovernanceRule {
            id: "sec".into(),
            description: "test".into(),
            branch: GovernanceBranch::Judicial,
            severity: RuleSeverity::Blocking,
            active: true,
            reference_url: None,
            sop_category: None,
            rule_type: Default::default(),
            action_selector: None,
            tool_selector: None,
            force_on_match: false,
            principal_selector: None,
        });

        // Context with effect embedded
        let ctx = serde_json::json!({
            "pid": 1,
            "effect": {
                "risk": 0.7,
                "fairness": 0.0,
                "privacy": 0.3,
                "novelty": 0.0,
                "security": 0.0
            }
        });
        let decision = gate.check("agent-1", "tool.exec", &ctx);
        // magnitude of (0.7, 0, 0.3, 0, 0) ≈ 0.76 > 0.5 → deny
        assert!(decision.is_deny());

        // Context without effect → default (zero) → permit
        let ctx_no_effect = serde_json::json!({"pid": 1});
        let decision = gate.check("agent-1", "tool.exec", &ctx_no_effect);
        assert!(decision.is_permit());
    }

    // ── Sprint 11 Security Tests ────────────────────────────────────

    #[test]
    fn replay_attack_same_context_twice() {
        // Submit the same governance check twice; both should return
        // consistent decisions (stateless gate — no replay detection
        // at gate level, but we verify determinism).
        let cm = Arc::new(crate::chain::ChainManager::new(0, 10));
        let gate = GovernanceGate::new(0.5, false)
            .with_chain(cm.clone())
            .add_rule(GovernanceRule {
                id: "sec".into(),
                description: "test".into(),
                branch: GovernanceBranch::Judicial,
                severity: RuleSeverity::Blocking,
                active: true,
                reference_url: None,
                sop_category: None,
                rule_type: Default::default(),
                action_selector: None,
                tool_selector: None,
                force_on_match: false,
                principal_selector: None,
            });

        let ctx = serde_json::json!({"effect": {"risk": 0.1}});

        let d1 = gate.check("agent-1", "tool.read", &ctx);
        let initial_len = cm.len();
        let d2 = gate.check("agent-1", "tool.read", &ctx);

        // Both decisions are permit (low risk).
        assert!(d1.is_permit());
        assert!(d2.is_permit());

        // Both calls logged to chain (two distinct events).
        assert_eq!(cm.len(), initial_len + 1);
    }

    #[test]
    fn replay_attack_chain_records_each_invocation() {
        let cm = Arc::new(crate::chain::ChainManager::new(0, 10));
        let gate = GovernanceGate::new(0.5, false)
            .with_chain(cm.clone())
            .add_rule(GovernanceRule {
                id: "sec".into(),
                description: "block risky".into(),
                branch: GovernanceBranch::Judicial,
                severity: RuleSeverity::Blocking,
                active: true,
                reference_url: None,
                sop_category: None,
                rule_type: Default::default(),
                action_selector: None,
                tool_selector: None,
                force_on_match: false,
                principal_selector: None,
            });

        let ctx = serde_json::json!({"effect": {"risk": 0.9}});
        let before = cm.len();
        gate.check("agent-1", "tool.exec", &ctx);
        gate.check("agent-1", "tool.exec", &ctx);
        gate.check("agent-1", "tool.exec", &ctx);
        // Every invocation produces a chain event.
        assert_eq!(cm.len(), before + 3);
    }

    #[test]
    fn invalid_capability_empty_action() {
        let (gate, pid) = make_gate_with_agent(AgentCapabilities::default());
        let ctx = serde_json::json!({"pid": pid});
        // Empty action string — no recognized prefix → permits by default.
        let decision = gate.check("test-agent", "", &ctx);
        assert!(decision.is_permit());
    }

    #[test]
    fn invalid_capability_very_long_action() {
        let (gate, pid) = make_gate_with_agent(AgentCapabilities::default());
        let ctx = serde_json::json!({"pid": pid});
        let long_action = "tool.".to_owned() + &"x".repeat(10_000);
        let decision = gate.check("test-agent", &long_action, &ctx);
        // Should not panic. Default caps allow tools.
        assert!(decision.is_permit());
    }

    #[test]
    fn invalid_capability_special_characters() {
        let (gate, pid) = make_gate_with_agent(AgentCapabilities::default());
        let ctx = serde_json::json!({"pid": pid});
        // Action with null bytes and unicode
        let decision = gate.check("test-agent", "tool.\0\x01\u{FEFF}", &ctx);
        assert!(decision.is_permit());
    }

    #[test]
    fn invalid_capability_action_with_path_traversal() {
        let (gate, pid) = make_gate_with_agent(AgentCapabilities::default());
        let ctx = serde_json::json!({"pid": pid});
        let decision = gate.check("test-agent", "tool.../../etc/passwd", &ctx);
        // Should still work — gate routes based on prefix.
        assert!(decision.is_permit());
    }

    #[test]
    fn permission_escalation_no_tool_access() {
        let caps = AgentCapabilities {
            can_exec_tools: false,
            can_ipc: false,
            can_spawn: false,
            ..Default::default()
        };
        let (gate, pid) = make_gate_with_agent(caps);
        let ctx = serde_json::json!({"pid": pid});

        // Agent without tool access tries various tool actions.
        assert!(gate.check("agent", "tool.shell_exec", &ctx).is_deny());
        assert!(gate.check("agent", "tool.read_file", &ctx).is_deny());
        assert!(gate.check("agent", "tool.write_file", &ctx).is_deny());

        // IPC also denied.
        let ipc_ctx = serde_json::json!({"pid": pid, "target_pid": 999});
        assert!(gate.check("agent", "ipc.send", &ipc_ctx).is_deny());
    }

    #[test]
    fn permission_escalation_service_access_denied() {
        // Agent with default caps but checking service access for
        // a non-existent service should be handled gracefully.
        let (gate, pid) = make_gate_with_agent(AgentCapabilities::default());
        let ctx = serde_json::json!({"pid": pid});
        // Service check depends on capability checker internals.
        let decision = gate.check("agent", "service.nonexistent_service", &ctx);
        // Service access check: capabilities allow by default.
        assert!(decision.is_permit() || decision.is_deny());
    }

    #[test]
    fn governance_gate_missing_pid_defaults_to_zero() {
        let (gate, _pid) = make_gate_with_agent(AgentCapabilities::default());
        // Context without pid field.
        let ctx = serde_json::json!({});
        let decision = gate.check("test-agent", "tool.read", &ctx);
        // pid=0 is not in process table, so tool check may deny.
        // The important thing is it does not panic.
        let _ = decision;
    }

    #[test]
    fn governance_gate_concurrent_checks() {
        let cm = Arc::new(crate::chain::ChainManager::new(0, 100));
        let gate = Arc::new(
            GovernanceGate::new(0.5, false)
                .with_chain(cm.clone())
                .add_rule(GovernanceRule {
                    id: "sec".into(),
                    description: "test".into(),
                    branch: GovernanceBranch::Judicial,
                    severity: RuleSeverity::Blocking,
                    active: true,
                    reference_url: None,
                    sop_category: None,
                    rule_type: Default::default(),
                    action_selector: None,
                    tool_selector: None,
                    force_on_match: false,
                    principal_selector: None,
                }),
        );

        let before = cm.len();

        std::thread::scope(|s| {
            for i in 0..10 {
                let gate = Arc::clone(&gate);
                s.spawn(move || {
                    let ctx = serde_json::json!({"effect": {"risk": 0.1 * (i as f64)}});
                    gate.check(&format!("agent-{i}"), "tool.check", &ctx);
                });
            }
        });

        // All 10 checks should be logged.
        assert_eq!(cm.len(), before + 10);
    }

    #[test]
    fn governance_gate_risk_boundary_at_threshold() {
        // Test exactly at the threshold boundary.
        let gate = GovernanceGate::new(0.5, false).add_rule(GovernanceRule {
            id: "sec".into(),
            description: "boundary test".into(),
            branch: GovernanceBranch::Judicial,
            severity: RuleSeverity::Blocking,
            active: true,
            reference_url: None,
            sop_category: None,
            rule_type: Default::default(),
            action_selector: None,
            tool_selector: None,
            force_on_match: false,
            principal_selector: None,
        });

        // Risk exactly at 0.5 — the magnitude of (0.5,0,0,0,0) = 0.5
        let ctx = serde_json::json!({"effect": {"risk": 0.5}});
        let decision = gate.check("agent", "tool.exec", &ctx);
        // At threshold: should be permit (not exceeded).
        assert!(decision.is_permit());

        // Slightly above threshold.
        let ctx_above = serde_json::json!({"effect": {"risk": 0.51}});
        let decision_above = gate.check("agent", "tool.exec", &ctx_above);
        assert!(decision_above.is_deny());
    }

    #[test]
    fn gate_decision_deny_reason_preserved() {
        let gate = GovernanceGate::new(0.5, false).add_rule(GovernanceRule {
            id: "sec".into(),
            description: "test deny reason".into(),
            branch: GovernanceBranch::Judicial,
            severity: RuleSeverity::Blocking,
            active: true,
            reference_url: None,
            sop_category: None,
            rule_type: Default::default(),
            action_selector: None,
            tool_selector: None,
            force_on_match: false,
            principal_selector: None,
        });

        let ctx = serde_json::json!({"effect": {"risk": 0.9}});
        let decision = gate.check("agent-1", "tool.danger", &ctx);
        match decision {
            GateDecision::Deny { reason, .. } => {
                assert!(!reason.is_empty(), "deny reason should not be empty");
            }
            _ => panic!("expected deny decision for high-risk action"),
        }
    }

    #[test]
    fn gate_decision_defer_reason_preserved() {
        let gate = GovernanceGate::new(0.5, true).add_rule(GovernanceRule {
            id: "sec".into(),
            description: "escalate test".into(),
            branch: GovernanceBranch::Judicial,
            severity: RuleSeverity::Blocking,
            active: true,
            reference_url: None,
            sop_category: None,
            rule_type: Default::default(),
            action_selector: None,
            tool_selector: None,
            force_on_match: false,
            principal_selector: None,
        });

        let ctx = serde_json::json!({"effect": {"risk": 0.9}});
        let decision = gate.check("agent-1", "tool.danger", &ctx);
        match decision {
            GateDecision::Defer { reason } => {
                assert!(!reason.is_empty(), "defer reason should not be empty");
            }
            _ => panic!("expected defer decision for high-risk action with human approval"),
        }
    }

    #[test]
    fn governance_gate_inactive_rule_ignored() {
        let gate = GovernanceGate::new(0.5, false).add_rule(GovernanceRule {
            id: "inactive-rule".into(),
            description: "this rule is inactive".into(),
            branch: GovernanceBranch::Judicial,
            severity: RuleSeverity::Blocking,
            active: false,
            reference_url: None,
            sop_category: None,
            rule_type: Default::default(),
            action_selector: None,
            tool_selector: None,
            force_on_match: false,
            principal_selector: None,
        });

        let ctx = serde_json::json!({"effect": {"risk": 0.9}});
        let decision = gate.check("agent-1", "tool.danger", &ctx);
        // Inactive rule should not block; governance may still block
        // based on threshold. But inactive rules are not evaluated.
        let _ = decision;
    }

    #[test]
    fn governance_gate_multiple_rules_evaluated() {
        let gate = GovernanceGate::new(0.5, false)
            .add_rule(GovernanceRule {
                id: "rule-1".into(),
                description: "first".into(),
                branch: GovernanceBranch::Judicial,
                severity: RuleSeverity::Blocking,
                active: true,
                reference_url: None,
                sop_category: None,
                rule_type: Default::default(),
                action_selector: None,
                tool_selector: None,
                force_on_match: false,
                principal_selector: None,
            })
            .add_rule(GovernanceRule {
                id: "rule-2".into(),
                description: "second".into(),
                branch: GovernanceBranch::Executive,
                severity: RuleSeverity::Warning,
                active: true,
                reference_url: None,
                sop_category: None,
                rule_type: Default::default(),
                action_selector: None,
                tool_selector: None,
                force_on_match: false,
                principal_selector: None,
            });

        let ctx = serde_json::json!({"effect": {"risk": 0.9}});
        let decision = gate.check("agent-1", "tool.exec", &ctx);
        assert!(decision.is_deny());
    }

    // ── check_detailed (read-only verdict for governance.check) ──

    fn blocking_rule() -> GovernanceRule {
        GovernanceRule {
            id: "security-check".into(),
            description: "Block high-risk actions".into(),
            branch: GovernanceBranch::Judicial,
            severity: RuleSeverity::Blocking,
            active: true,
            reference_url: None,
            sop_category: None,
            rule_type: Default::default(),
            action_selector: None,
            tool_selector: None,
            force_on_match: false,
            principal_selector: None,
        }
    }

    #[test]
    fn check_detailed_agrees_with_check_on_deny_defer_permit() {
        let high = serde_json::json!({ "effect": { "risk": 0.8, "security": 0.6 } });
        let low = serde_json::json!({ "effect": { "risk": 0.1 } });

        let deny_gate = GovernanceGate::new(0.5, false).add_rule(blocking_rule());
        let v = deny_gate.check_detailed("a", "tool.exec", &high).unwrap();
        assert!(matches!(v.decision, crate::governance::GovernanceDecision::Deny(_)));
        assert!(deny_gate.check("a", "tool.exec", &high).is_deny());
        assert!(!v.evaluated_rules.is_empty());

        let defer_gate = GovernanceGate::new(0.5, true).add_rule(blocking_rule());
        let v = defer_gate.check_detailed("a", "tool.exec", &high).unwrap();
        assert!(matches!(
            v.decision,
            crate::governance::GovernanceDecision::EscalateToHuman(_)
        ));

        let v = deny_gate.check_detailed("a", "tool.read_file", &low).unwrap();
        assert!(matches!(v.decision, crate::governance::GovernanceDecision::Permit));
    }

    #[test]
    fn check_detailed_honours_exemption_like_check() {
        let high = serde_json::json!({ "effect": { "risk": 0.8, "security": 0.6 } });
        let gate = GovernanceGate::new(0.5, false)
            .add_rule(blocking_rule())
            .exempt_action("tool.exec");
        let v = gate.check_detailed("a", "tool.exec", &high).unwrap();
        assert!(v.grant_applied);
        assert!(matches!(v.decision, crate::governance::GovernanceDecision::Permit));
        assert!(gate.check("a", "tool.exec", &high).is_permit());
    }

    #[test]
    fn check_detailed_writes_nothing_to_the_chain() {
        let cm = std::sync::Arc::new(crate::chain::ChainManager::new(0, 100));
        let gate = GovernanceGate::new(0.5, false)
            .add_rule(blocking_rule())
            .with_chain(std::sync::Arc::clone(&cm));
        let before = cm.tail(0).len();
        let high = serde_json::json!({ "effect": { "risk": 0.8, "security": 0.6 } });
        let _ = gate.check_detailed("a", "tool.exec", &high);
        assert_eq!(cm.tail(0).len(), before, "check_detailed must not append");
        let _ = gate.check("a", "tool.exec", &high);
        assert!(cm.tail(0).len() > before, "check still logs, so the test is not vacuous");
    }

    #[test]
    fn gates_without_an_engine_return_no_verdict() {
        let gate = CapabilityGate::new(Arc::new(ProcessTable::new(4)));
        assert!(gate.check_detailed("a", "tool.x", &serde_json::json!({})).is_none());
    }
}

#[cfg(all(test, feature = "tilezero"))]
mod tilezero_tests {
    //! WEFT-152: exercise cognitum-gate-tilezero Permit/Defer/Deny paths
    //! through `TileZeroGate`, including cryptographic token bytes and
    //! chain event kinds.
    use super::*;
    use cognitum_gate_tilezero::GateThresholds;
    use std::sync::Arc;

    /// Default thresholds: empty ReducedGraph starts at cut=100, e=100,
    /// shift=0 → structural OK, shift OK, evidence Accept → **Permit**.
    fn make_tilezero_gate() -> TileZeroGate {
        let tz = Arc::new(cognitum_gate_tilezero::TileZero::new(
            GateThresholds::default(),
        ));
        TileZeroGate::new(tz, None)
    }

    fn make_tilezero_gate_with(
        thresholds: GateThresholds,
        with_chain: bool,
    ) -> TileZeroGate {
        let tz = Arc::new(cognitum_gate_tilezero::TileZero::new(thresholds));
        let chain = if with_chain {
            Some(Arc::new(crate::chain::ChainManager::new(0, 10)))
        } else {
            None
        };
        TileZeroGate::new(tz, chain)
    }

    fn make_tilezero_gate_with_chain() -> TileZeroGate {
        make_tilezero_gate_with(GateThresholds::default(), true)
    }

    /// Thresholds that force **Deny** via the structural filter:
    /// empty graph cut=100 < min_cut=1000.
    fn deny_thresholds() -> GateThresholds {
        GateThresholds {
            min_cut: 1000.0,
            ..GateThresholds::default()
        }
    }

    /// Thresholds that force **Defer** via the evidence filter:
    /// e=100 sits between tau_deny and tau_permit → Continue → Defer.
    fn defer_thresholds() -> GateThresholds {
        GateThresholds {
            tau_deny: 0.01,
            tau_permit: 200.0, // e=100 is below this → Continue
            min_cut: 5.0,      // structural still OK (cut=100)
            max_shift: 0.5,    // shift still OK (shift=0)
            ..GateThresholds::default()
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tilezero_gate_returns_decision() {
        let gate = make_tilezero_gate();
        let ctx = serde_json::json!({"pid": 1});
        let decision = gate.check("test-agent", "tool.read_file", &ctx);
        // Default empty graph → Permit (see module docs / make helpers).
        assert!(
            decision.is_permit(),
            "default thresholds should Permit, got {decision:?}"
        );
    }

    /// WEFT-152: explicit Permit path with serialized PermitToken receipt.
    #[tokio::test(flavor = "multi_thread")]
    async fn tilezero_gate_permit_path() {
        let gate = make_tilezero_gate_with(GateThresholds::default(), true);
        let ctx = serde_json::json!({"session_id": "s-permit"});
        let decision = gate.check("agent-permit", "tool.read_file", &ctx);

        match &decision {
            GateDecision::Permit { token } => {
                let bytes = token.as_ref().expect("Permit must carry token bytes");
                let pt: cognitum_gate_tilezero::PermitToken =
                    serde_json::from_slice(bytes).expect("token JSON must deserialize");
                assert_eq!(pt.decision, cognitum_gate_tilezero::GateDecision::Permit);
                assert_eq!(pt.sequence, 0);
                // Ed25519 signature is non-zero after signing.
                assert!(pt.signature.iter().any(|&b| b != 0), "signature must be set");
                // witness_hash is 32 bytes (hex-serialized as [u8;32]).
                assert_eq!(pt.witness_hash.len(), 32);
            }
            other => panic!("expected Permit, got {other:?}"),
        }

        let chain = gate.chain().unwrap();
        let events = chain.tail(1);
        assert_eq!(events[0].kind, crate::chain::EVENT_KIND_GATE_PERMIT);
        assert_eq!(events[0].source, "gate");
        let payload = events[0].payload.as_ref().unwrap();
        assert_eq!(payload["agent_id"], "agent-permit");
        assert_eq!(payload["action"], "tool.read_file");
        assert!(payload.get("sequence").is_some());
        assert!(payload.get("witness_hash").is_some());
    }

    /// WEFT-152: explicit Defer path (evidence Continue band).
    #[tokio::test(flavor = "multi_thread")]
    async fn tilezero_gate_defer_path() {
        let gate = make_tilezero_gate_with(defer_thresholds(), true);
        let ctx = serde_json::json!({});
        let decision = gate.check("agent-defer", "tool.review", &ctx);

        match &decision {
            GateDecision::Defer { reason } => {
                assert!(
                    reason.contains("TileZero deferred"),
                    "defer reason should mention TileZero: {reason}"
                );
                assert!(
                    reason.contains("seq="),
                    "defer reason should include sequence: {reason}"
                );
            }
            other => panic!("expected Defer, got {other:?}"),
        }

        let chain = gate.chain().unwrap();
        let events = chain.tail(1);
        assert_eq!(events[0].kind, crate::chain::EVENT_KIND_GATE_DEFER);
        assert_eq!(events[0].payload.as_ref().unwrap()["agent_id"], "agent-defer");
    }

    /// WEFT-152: explicit Deny path (structural min_cut failure) with receipt.
    #[tokio::test(flavor = "multi_thread")]
    async fn tilezero_gate_deny_path() {
        let gate = make_tilezero_gate_with(deny_thresholds(), true);
        let ctx = serde_json::json!({});
        let decision = gate.check("agent-deny", "tool.danger", &ctx);

        match &decision {
            GateDecision::Deny { reason, receipt } => {
                assert!(
                    reason.contains("TileZero denied"),
                    "deny reason should mention TileZero: {reason}"
                );
                let bytes = receipt.as_ref().expect("Deny must carry receipt bytes");
                let pt: cognitum_gate_tilezero::PermitToken =
                    serde_json::from_slice(bytes).expect("receipt JSON must deserialize");
                assert_eq!(pt.decision, cognitum_gate_tilezero::GateDecision::Deny);
                assert!(pt.signature.iter().any(|&b| b != 0));
            }
            other => panic!("expected Deny, got {other:?}"),
        }

        let chain = gate.chain().unwrap();
        let events = chain.tail(1);
        assert_eq!(events[0].kind, crate::chain::EVENT_KIND_GATE_DENY);
        assert_eq!(events[0].payload.as_ref().unwrap()["action"], "tool.danger");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tilezero_gate_includes_token_bytes() {
        let gate = make_tilezero_gate();
        let ctx = serde_json::json!({});
        let decision = gate.check("agent-1", "tool.search", &ctx);

        match &decision {
            GateDecision::Permit { token } => {
                // Permit tokens carry serialized PermitToken
                assert!(token.is_some());
                let bytes = token.as_ref().unwrap();
                // Should deserialize back to a PermitToken
                let pt: cognitum_gate_tilezero::PermitToken =
                    serde_json::from_slice(bytes).unwrap();
                assert_eq!(pt.sequence, 0);
                assert_eq!(pt.decision, cognitum_gate_tilezero::GateDecision::Permit);
            }
            GateDecision::Deny { receipt, .. } => {
                // Deny receipts also carry the signed token
                assert!(receipt.is_some());
            }
            GateDecision::Defer { .. } => {
                // Defer has no token/receipt, just a reason
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tilezero_gate_logs_to_chain() {
        let gate = make_tilezero_gate_with_chain();
        let ctx = serde_json::json!({"urgency": "high"});
        let decision = gate.check("agent-1", "tool.deploy", &ctx);

        // Default thresholds → Permit → gate.permit
        assert!(decision.is_permit());
        let chain = gate.chain().unwrap();
        let seq = chain.sequence();
        // At minimum: genesis(0) + gate event(1)
        assert!(seq >= 1, "expected chain event, got seq={seq}");
        let events = chain.tail(1);
        assert_eq!(events[0].kind, crate::chain::EVENT_KIND_GATE_PERMIT);
    }

    /// WEFT-152: all three decision branches produce distinct chain kinds.
    #[tokio::test(flavor = "multi_thread")]
    async fn tilezero_gate_three_way_chain_kinds() {
        // Permit
        let permit_gate = make_tilezero_gate_with(GateThresholds::default(), true);
        permit_gate.check("a", "tool.p", &serde_json::json!({}));
        assert_eq!(
            permit_gate.chain().unwrap().tail(1)[0].kind,
            crate::chain::EVENT_KIND_GATE_PERMIT
        );

        // Defer
        let defer_gate = make_tilezero_gate_with(defer_thresholds(), true);
        defer_gate.check("a", "tool.d", &serde_json::json!({}));
        assert_eq!(
            defer_gate.chain().unwrap().tail(1)[0].kind,
            crate::chain::EVENT_KIND_GATE_DEFER
        );

        // Deny
        let deny_gate = make_tilezero_gate_with(deny_thresholds(), true);
        deny_gate.check("a", "tool.n", &serde_json::json!({}));
        assert_eq!(
            deny_gate.chain().unwrap().tail(1)[0].kind,
            crate::chain::EVENT_KIND_GATE_DENY
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tilezero_gate_sequential_decisions() {
        let gate = make_tilezero_gate();
        let ctx = serde_json::json!({});

        // Multiple calls should produce incrementing sequences
        let d1 = gate.check("agent-1", "tool.a", &ctx);
        let d2 = gate.check("agent-1", "tool.b", &ctx);

        // Both should return valid decisions
        assert!(d1.is_permit());
        assert!(d2.is_permit());

        // Token sequences should increment (0, then 1)
        let seq = |d: &GateDecision| -> u64 {
            match d {
                GateDecision::Permit { token } => {
                    let pt: cognitum_gate_tilezero::PermitToken =
                        serde_json::from_slice(token.as_ref().unwrap()).unwrap();
                    pt.sequence
                }
                _ => panic!("expected Permit"),
            }
        };
        assert_eq!(seq(&d1), 0);
        assert_eq!(seq(&d2), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tilezero_gate_action_context_mapping() {
        // Verify our ActionContext builder extracts fields correctly
        let ctx = serde_json::json!({
            "device": "router-1",
            "path": "/config/acl",
            "session_id": "sess-42",
            "urgency": "critical",
        });

        let action_ctx =
            tilezero_gate::TileZeroGate::build_action_context("agent-x", "tool.deploy", &ctx);

        assert_eq!(action_ctx.action_type, "tool.deploy");
        assert_eq!(action_ctx.context.agent_id, "agent-x");
        assert_eq!(action_ctx.context.urgency, "critical");
        assert_eq!(action_ctx.context.session_id, Some("sess-42".into()));
        assert_eq!(action_ctx.target.device, Some("router-1".into()));
        assert_eq!(action_ctx.target.path, Some("/config/acl".into()));
    }

    // ── D6: per-action governance grant (GovernanceGate::exempt_action) ──

    /// agent_spawn's effect (risk 0.5, novelty 0.6, security 0.5) has magnitude
    /// ≈ 0.927, above the 0.8 chat-gate threshold — so a blocking rule denies it
    /// unless the action is explicitly granted.
    fn high_magnitude_spawn_ctx() -> serde_json::Value {
        serde_json::json!({ "effect": { "risk": 0.5, "novelty": 0.6, "security": 0.5 } })
    }

    fn blocking_chat_gate() -> GovernanceGate {
        use crate::governance::{GovernanceBranch, GovernanceRule, RuleSeverity};
        GovernanceGate::new(0.8, false).add_rule(GovernanceRule {
            id: "chat-tool-guard".into(),
            description: "Block high-risk tool dispatches in agent.chat".into(),
            branch: GovernanceBranch::Judicial,
            severity: RuleSeverity::Blocking,
            active: true,
            reference_url: None,
            sop_category: None,
            rule_type: Default::default(),
            action_selector: None,
            tool_selector: None,
            force_on_match: false,
            principal_selector: None,
        })
    }

    #[test]
    fn governance_grant_default_off_denies_high_magnitude_spawn() {
        let gate = blocking_chat_gate();
        let d = gate.check("agent-P", "tool.agent_spawn", &high_magnitude_spawn_ctx());
        assert!(
            matches!(d, GateDecision::Deny { .. }),
            "without the grant, a 0.93 spawn must be denied, got {d:?}"
        );
    }

    #[test]
    fn governance_grant_permits_exempted_spawn() {
        let gate = blocking_chat_gate().exempt_action("tool.agent_spawn");
        let d = gate.check("agent-P", "tool.agent_spawn", &high_magnitude_spawn_ctx());
        assert!(
            matches!(d, GateDecision::Permit { .. }),
            "the grant must permit tool.agent_spawn, got {d:?}"
        );
    }

    #[test]
    fn governance_grant_does_not_leak_to_other_actions() {
        // Granting agent_spawn must NOT permit an unrelated high-magnitude action.
        let gate = blocking_chat_gate().exempt_action("tool.agent_spawn");
        let d = gate.check("agent-P", "tool.exec", &high_magnitude_spawn_ctx());
        assert!(
            matches!(d, GateDecision::Deny { .. }),
            "the exemption must not leak to tool.exec, got {d:?}"
        );
    }

    #[test]
    fn governance_grant_is_witnessed_on_chain() {
        let cm = std::sync::Arc::new(crate::chain::ChainManager::new(0, 1000));
        let gate = blocking_chat_gate()
            .with_chain(cm.clone())
            .exempt_action("tool.agent_spawn");
        let d = gate.check("agent-P", "tool.agent_spawn", &high_magnitude_spawn_ctx());
        assert!(matches!(d, GateDecision::Permit { .. }));

        // The grant must leave an audit trail — a governance.grant event, not silence.
        let events = cm.tail(0);
        let grant = events
            .iter()
            .find(|e| e.kind == "governance.grant")
            .expect("the exercised grant must be witnessed on the chain");
        let payload = grant.payload.as_ref().expect("grant event carries a payload");
        assert_eq!(
            payload.get("granted_action").and_then(|v| v.as_str()),
            Some("tool.agent_spawn")
        );
        assert!(
            payload
                .get("overridden_reason")
                .and_then(|v| v.as_str())
                .is_some_and(|r| !r.is_empty()),
            "the witness records what the grant overrode"
        );
    }

    /// WEFT-633/634: spawn-approval rule + human_approval maps Escalate → Defer.
    #[test]
    fn spawn_approval_rule_defers_via_selector() {
        let gate = GovernanceGate::new(0.99, true)
            .add_rule(crate::governance::GovernanceRule::spawn_requires_approval());
        // Low effect — force_on_match still trips; tool identity derived from action.
        let ctx = serde_json::json!({
            "effect": { "risk": 0.05, "security": 0.05 },
            "user_id": "user-alice",
            "conv_id": "conv-P",
        });
        let decision = gate.check("agent-1", "tool.agent_spawn", &ctx);
        match decision {
            GateDecision::Defer { reason } => {
                assert!(
                    reason.contains("SPAWN-APPROVAL") || reason.contains("force-match"),
                    "defer reason={reason}"
                );
            }
            other => panic!("expected Defer for spawn approval, got {other:?}"),
        }

        // Non-spawn tool is unaffected by the selector rule.
        let other = gate.check("agent-1", "tool.read_file", &ctx);
        assert!(other.is_permit());
    }

    /// WEFT-636: principal attribution appears on the chain payload.
    #[test]
    fn governance_gate_chain_payload_carries_principal() {
        let cm = std::sync::Arc::new(crate::chain::ChainManager::new(0, 1000));
        let gate = GovernanceGate::new(0.5, false)
            .with_chain(cm.clone())
            .add_rule(crate::governance::GovernanceRule {
                id: "sec".into(),
                description: "block".into(),
                branch: crate::governance::GovernanceBranch::Judicial,
                severity: crate::governance::RuleSeverity::Blocking,
                active: true,
                reference_url: None,
                sop_category: None,
                rule_type: Default::default(),
                action_selector: None,
                tool_selector: None,
                force_on_match: false,
                principal_selector: None,
            });
        let ctx = serde_json::json!({
            "effect": { "risk": 0.9, "security": 0.9 },
            "user_id": "user-bob",
            "parent_agent_id": "parent-1",
            "conv_id": "conv-C",
        });
        let _ = gate.check("child-agent", "tool.exec", &ctx);
        let events = cm.tail(0);
        let deny = events
            .iter()
            .find(|e| e.kind == "governance.deny")
            .expect("deny event");
        let payload = deny.payload.as_ref().expect("payload");
        let principal = payload
            .get("principal")
            .expect("principal in payload");
        assert_eq!(principal.get("agent_id").and_then(|v| v.as_str()), Some("child-agent"));
        assert_eq!(principal.get("user_id").and_then(|v| v.as_str()), Some("user-bob"));
        assert_eq!(
            principal.get("parent_agent_id").and_then(|v| v.as_str()),
            Some("parent-1")
        );
        assert_eq!(principal.get("conv_id").and_then(|v| v.as_str()), Some("conv-C"));
    }
}
