//! `governance.check`: ask the daemon's live governance engine whether an
//! action would be allowed, without doing it (Tapestry R9).
//!
//! | field | meaning |
//! |---|---|
//! | `agent_id` (alias `principal`) | who is acting; required, non-empty. The engine's rules match on it |
//! | `action` | the gate action string, e.g. `tool.shell_exec` |
//! | `tool` | tool name; when `action` is absent the action is `tool.<tool>`; also set as the `tool` context key for tool selectors |
//! | `context` | optional object; string values pass through, others are stringified (the gate's own rule) |
//! | `effect` | optional `{risk, fairness, privacy, novelty, security}`; a malformed one is refused, never defaulted |
//!
//! The reply is `{decision, reason, evaluated_rules, grant_applied}` with
//! `decision` one of `permit`, `permit_with_warning`, `escalate_to_human`,
//! `deny`. The request is built by the same code as `GateBackend::check`
//! (`GovernanceGate::build_request`) and evaluated by the same engine, so the
//! verdict is the one a real gate check would reach. Unlike `check` it writes
//! nothing to the chain.
//!
//! Fails closed: a missing gate, a gate with no engine and any malformed
//! parameter answer with an error and `ok: false`. The reply is never
//! `ok: true` with a `permit` unless the engine said so.
//!
//! Capability `Read`, local caller (see `capability.rs` and `scope_gate.rs`):
//! it changes no rule and no decision state, and it exposes only the verdict
//! for the asked action plus the names of the rules evaluated. The one side
//! effect is the engine's per-principal evaluation quota (WEFT-148), which
//! a check consumes exactly as a real gate check does; an exhausted quota
//! answers `deny`.

use clawft_kernel::gate::GateBackend;
use clawft_kernel::governance::GovernanceDecision;
use clawft_rpc::Response;
use serde_json::{Map, Value, json};

use crate::rpc_ext::{ExtCall, ExtFuture};

fn invalid(msg: impl Into<String>) -> Response {
    Response::error_with_kind("invalid_params", msg)
}

/// What a check asks, after validation.
struct Query {
    agent_id: String,
    action: String,
    context: Value,
}

fn parse(params: &Value) -> Result<Query, String> {
    let obj = params.as_object().ok_or("params must be an object")?;
    let agent_id = match (obj.get("agent_id"), obj.get("principal")) {
        (Some(Value::String(s)), _) | (None, Some(Value::String(s))) if !s.trim().is_empty() => {
            s.trim().to_owned()
        }
        _ => return Err("`agent_id` (or `principal`) must be a non-empty string".into()),
    };
    let str_field = |k: &str| -> Result<Option<String>, String> {
        match obj.get(k) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) if !s.trim().is_empty() => Ok(Some(s.trim().to_owned())),
            Some(_) => Err(format!("`{k}` must be a non-empty string")),
        }
    };
    let tool = str_field("tool")?;
    let action = match (str_field("action")?, &tool) {
        (Some(a), _) => a,
        (None, Some(t)) => format!("tool.{t}"),
        (None, None) => return Err("needs `action` or `tool`".into()),
    };
    let mut ctx: Map<String, Value> = match obj.get("context") {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(m)) => m.clone(),
        Some(_) => return Err("`context` must be an object".into()),
    };
    if let Some(t) = tool {
        ctx.insert("tool".into(), Value::String(t));
    }
    // `effect` rides inside the gate context, as `GateBackend::check` expects.
    match obj.get("effect") {
        None | Some(Value::Null) => {
            ctx.remove("effect");
        }
        Some(e) => {
            serde_json::from_value::<clawft_kernel::governance::EffectVector>(e.clone())
                .map_err(|err| format!("`effect`: {err}"))?;
            ctx.insert("effect".into(), e.clone());
        }
    }
    Ok(Query { agent_id, action, context: Value::Object(ctx) })
}

/// Evaluate `params` against `gate`. Pure of daemon state so tests can drive
/// it with a gate they built.
pub fn check(gate: Option<&dyn GateBackend>, params: &Value) -> Response {
    let q = match parse(params) {
        Ok(q) => q,
        Err(e) => return invalid(e),
    };
    let Some(gate) = gate else {
        return Response::error_with_kind(
            "no_governance_gate",
            "this kernel has no governance gate; cannot decide",
        );
    };
    let Some(v) = gate.check_detailed(&q.agent_id, &q.action, &q.context) else {
        return Response::error_with_kind(
            "no_governance_engine",
            "the kernel's gate has no governance engine; cannot decide",
        );
    };
    let (decision, reason) = match &v.decision {
        GovernanceDecision::Permit => ("permit", String::new()),
        GovernanceDecision::PermitWithWarning(w) => ("permit_with_warning", w.clone()),
        GovernanceDecision::EscalateToHuman(r) => ("escalate_to_human", r.clone()),
        GovernanceDecision::Deny(r) => ("deny", r.clone()),
        // `GovernanceDecision` may grow; an unknown verdict is not a permit.
        #[allow(unreachable_patterns)]
        other => ("deny", format!("unrecognised governance decision: {other}")),
    };
    Response::success(json!({
        "decision": decision,
        "reason": reason,
        "evaluated_rules": v.evaluated_rules,
        "grant_applied": v.grant_applied,
        "agent_id": q.agent_id,
        "action": q.action,
    }))
}

/// `governance.check` (user daemon or project kernel; `Read`).
pub fn handle(call: ExtCall) -> ExtFuture {
    Box::pin(async move {
        let gate = call.ctx.kernel.read().await.governance_gate().cloned();
        check(gate.as_deref(), &call.params)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use clawft_kernel::gate::{CapabilityGate, GovernanceGate};
    use clawft_kernel::governance::{GovernanceBranch, GovernanceRule, RuleSeverity};

    fn rule(id: &str, tool: Option<&str>, severity: RuleSeverity) -> GovernanceRule {
        GovernanceRule {
            id: id.into(),
            description: format!("test rule {id}"),
            branch: GovernanceBranch::Judicial,
            severity,
            active: true,
            reference_url: None,
            sop_category: None,
            rule_type: Default::default(),
            action_selector: None,
            tool_selector: tool.map(String::from),
            force_on_match: false,
        }
    }

    fn deny_gate() -> GovernanceGate {
        GovernanceGate::new(0.5, false).add_rule(rule("block-shell", None, RuleSeverity::Blocking))
    }

    fn high() -> Value {
        json!({"risk": 0.8, "security": 0.6})
    }

    fn result(r: &Response) -> &Value {
        assert!(r.ok, "expected ok, got {:?}", r.error);
        r.result.as_ref().unwrap()
    }

    #[test]
    fn deny_rule_returns_deny_with_reason_and_rules() {
        let g = deny_gate();
        let r = check(
            Some(&g),
            &json!({"agent_id": "tapestry", "tool": "shell_exec", "effect": high()}),
        );
        let v = result(&r);
        assert_eq!(v["decision"], "deny");
        assert!(!v["reason"].as_str().unwrap().is_empty());
        assert_eq!(v["action"], "tool.shell_exec");
        assert!(v["evaluated_rules"].as_array().unwrap().iter().any(|x| x == "block-shell"));
    }

    #[test]
    fn tool_selector_deny_needs_no_effect_and_spares_other_tools() {
        let mut r = rule("no-shell", Some("shell_exec"), RuleSeverity::Blocking);
        r.force_on_match = true;
        let g = GovernanceGate::new(0.7, false).add_rule(r);
        let denied = check(Some(&g), &json!({"agent_id": "tapestry", "tool": "shell_exec"}));
        assert_eq!(result(&denied)["decision"], "deny");
        let allowed = check(Some(&g), &json!({"agent_id": "tapestry", "tool": "read_file"}));
        assert_eq!(result(&allowed)["decision"], "permit");
    }

    #[test]
    fn low_effect_permits() {
        let g = deny_gate();
        let r = check(
            Some(&g),
            &json!({"agent_id": "tapestry", "action": "tool.read_file", "effect": {"risk": 0.1}}),
        );
        assert_eq!(result(&r)["decision"], "permit");
    }

    #[test]
    fn human_approval_engine_escalates() {
        let g = GovernanceGate::new(0.5, true).add_rule(rule("r", None, RuleSeverity::Blocking));
        let r = check(Some(&g), &json!({"principal": "tapestry", "action": "tool.x", "effect": high()}));
        assert_eq!(result(&r)["decision"], "escalate_to_human");
    }

    #[test]
    fn matches_the_real_gate_check() {
        let g = deny_gate();
        let ctx = json!({"effect": high(), "tool": "shell_exec"});
        assert!(g.check("tapestry", "tool.shell_exec", &ctx).is_deny());
        let r = check(Some(&g), &json!({"agent_id": "tapestry", "tool": "shell_exec", "effect": high()}));
        assert_eq!(result(&r)["decision"], "deny");
    }

    #[test]
    fn missing_gate_and_engineless_gate_refuse() {
        let p = json!({"agent_id": "a", "action": "tool.x"});
        let r = check(None, &p);
        assert!(!r.ok);
        assert_eq!(r.error_kind.as_deref(), Some("no_governance_gate"));
        assert!(r.result.is_none());

        let cap = CapabilityGate::new(std::sync::Arc::new(clawft_kernel::process::ProcessTable::new(4)));
        let r = check(Some(&cap), &p);
        assert!(!r.ok);
        assert_eq!(r.error_kind.as_deref(), Some("no_governance_engine"));
        assert!(r.result.is_none());
    }

    #[test]
    fn malformed_params_refuse_never_permit() {
        let g = GovernanceGate::open();
        for bad in [
            json!("nope"),
            json!({}),
            json!({"action": "tool.x"}),
            json!({"agent_id": "  ", "action": "tool.x"}),
            json!({"agent_id": 7, "action": "tool.x"}),
            json!({"agent_id": "a"}),
            json!({"agent_id": "a", "action": ""}),
            json!({"agent_id": "a", "action": "tool.x", "context": "str"}),
            json!({"agent_id": "a", "action": "tool.x", "effect": "high"}),
            json!({"agent_id": "a", "action": "tool.x", "effect": {"risk": "very"}}),
        ] {
            let r = check(Some(&g), &bad);
            assert!(!r.ok, "must refuse {bad}");
            assert_eq!(r.error_kind.as_deref(), Some("invalid_params"), "{bad}");
            assert!(r.result.is_none());
        }
    }

    #[test]
    fn check_does_not_chain_log() {
        let cm = std::sync::Arc::new(clawft_kernel::chain::ChainManager::new(0, 100));
        let g = deny_gate().with_chain(std::sync::Arc::clone(&cm));
        let before = cm.tail(0).len();
        let _ = check(Some(&g), &json!({"agent_id": "a", "action": "tool.x", "effect": high()}));
        assert_eq!(cm.tail(0).len(), before);
    }

    #[test]
    fn routed_as_read_and_allowed_outside_a_project() {
        use crate::capability::{Capability, required_capability};
        assert_eq!(required_capability("governance.check"), Capability::Read);
        assert!(crate::scope_gate::READ_ONLY_ALLOW.contains(&"governance.check"));
    }
}
