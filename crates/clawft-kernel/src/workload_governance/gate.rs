//! [`WorkloadGate`]: the governance gate for `workload.*` actions.
//!
//! Evaluation order (every step's outcome is chained, ADR-022):
//!
//! 1. The action must be one of [`GOVERNED_ACTIONS`]; an unknown
//!    `workload.*` action is refused and chained as `workload.refuse`.
//! 2. The `workload` context object must parse and validate.
//! 3. The subject revocation list must be readable, a request that installs,
//!    places, loads, starts or migrates must name a package, signer key or
//!    artifact hash, and none of those may be revoked. Teardown (stop,
//!    unload, re-adoption) is never blocked by a revocation.
//! 4. Workloads carrying secrets need a `pinned` node (ADR-099 s8.3).
//! 5. A [`WorkloadPermitRule`] must match; otherwise the default deny holds.
//!    This is enforced in code as well as by the `WORKLOAD-DEFAULT-DENY`
//!    rule, so a rule set that lost that rule still fails closed.
//! 6. The governance engine evaluates the derived 5D effect vector with
//!    every rule except the default-deny category (effect ceiling and any
//!    other distributed rules still apply).

use std::sync::Arc;

use super::effect::{NodeTrustTier, WorkloadRequest};
use super::policy::{
    DEFAULT_DENY_CATEGORY, DEFAULT_DENY_RULE_ID, GOVERNED_ACTIONS, WORKLOAD_ACTION_PREFIX,
    WorkloadPermitRule, default_rules, is_governed_action,
};
use crate::chain::{self, ChainManager};
use crate::gate::{GateBackend, GateDecision};
use crate::governance::{
    GatePrincipal, GovernanceDecision, GovernanceEngine, GovernanceRequest, GovernanceRule,
    ProjectAttestation,
};
use crate::revocation::{RevocationKind, RevocationList};

/// Chain source used for workload governance events.
pub const CHAIN_SOURCE: &str = "workload";

/// Default effect-magnitude threshold for workload actions.
pub const DEFAULT_THRESHOLD: f64 = 0.8;

/// Start of the denial reason when the subject revocation list could not be
/// read. That is a fault of the node's files, not a verdict on the request, so
/// a caller that can retry later (a restart restoring placed instances)
/// treats it as transient, unlike any other governance denial.
pub const REVOCATION_UNREADABLE: &str = "revocation list unreadable (fail closed)";

/// Chain event kind for a governed action (`None` for unknown actions).
pub fn event_kind_for(action: &str) -> Option<&'static str> {
    Some(match action {
        "workload.install" => chain::EVENT_KIND_WORKLOAD_INSTALL,
        "workload.place" => chain::EVENT_KIND_WORKLOAD_PLACE,
        "workload.load" => chain::EVENT_KIND_WORKLOAD_LOAD,
        "workload.start" => chain::EVENT_KIND_WORKLOAD_START,
        "workload.stop" => chain::EVENT_KIND_WORKLOAD_STOP,
        "workload.unload" => chain::EVENT_KIND_WORKLOAD_UNLOAD,
        "workload.migrate" => chain::EVENT_KIND_WORKLOAD_MIGRATE,
        "workload.revoke" => chain::EVENT_KIND_WORKLOAD_REVOKE,
        "workload.node.bind" => chain::EVENT_KIND_WORKLOAD_NODE_BIND,
        "workload.node.unbind" => chain::EVENT_KIND_WORKLOAD_NODE_UNBIND,
        _ => return None,
    })
}

/// Governance gate for workload actions.
pub struct WorkloadGate {
    /// Engine with every rule except the default-deny category.
    permitted_engine: GovernanceEngine,
    /// Rule ids evaluated when no permit matched (for the audit record).
    default_rule_ids: Vec<String>,
    permits: Vec<WorkloadPermitRule>,
    chain: Option<Arc<ChainManager>>,
    revocations: Option<Arc<RevocationList>>,
    /// Set by [`WorkloadGate::exempt`]: why there is no revocation list.
    exempt_reason: Option<&'static str>,
    /// Project this gate decides for, overriding the kernel's instance
    /// attestation (tests; a kernel serves one project, set at boot).
    attestation: Option<ProjectAttestation>,
}

impl WorkloadGate {
    /// Gate with the shipped [`default_rules`] and no permits (denies all),
    /// checking `revocations` on every request. The list is required: a gate
    /// built without one never denies a revoked package. A gate that
    /// genuinely has no subject list says why with [`Self::exempt`].
    pub fn new(
        risk_threshold: f64,
        human_approval: bool,
        revocations: Arc<RevocationList>,
    ) -> Self {
        Self::with_rules(risk_threshold, human_approval, default_rules(), revocations)
    }

    /// [`Self::new`] without a revocation list, for a gate whose workloads
    /// are not revoked through the subject list (the project supervisor:
    /// project certificates revoke through the project identity record) and
    /// for tests. `why` is kept and shown by [`Self::exempt_reason`]; the
    /// population test allows this only in a short list of files.
    pub fn exempt(risk_threshold: f64, human_approval: bool, why: &'static str) -> Self {
        let mut g = Self::build(risk_threshold, human_approval, default_rules(), None);
        g.exempt_reason = Some(why);
        g
    }

    /// Gate over a distributed rule set (e.g. `RuleDistribution::active_rules`)
    /// checking `revocations`.
    ///
    /// Only rules whose selectors can match `workload.*` actions matter; the
    /// rest are carried but never match.
    pub fn with_rules(
        risk_threshold: f64,
        human_approval: bool,
        rules: Vec<GovernanceRule>,
        revocations: Arc<RevocationList>,
    ) -> Self {
        Self::build(risk_threshold, human_approval, rules, Some(revocations))
    }

    /// Why this gate does not check the subject revocation list, if it does not.
    pub fn exempt_reason(&self) -> Option<&'static str> {
        self.exempt_reason
    }

    fn build(
        risk_threshold: f64,
        human_approval: bool,
        rules: Vec<GovernanceRule>,
        revocations: Option<Arc<RevocationList>>,
    ) -> Self {
        let mut permitted_engine = GovernanceEngine::new(risk_threshold, human_approval);
        let mut default_rule_ids = Vec::new();
        for rule in rules {
            if rule.sop_category.as_deref() == Some(DEFAULT_DENY_CATEGORY) {
                if rule.active {
                    default_rule_ids.push(rule.id.clone());
                }
            } else {
                permitted_engine.add_rule(rule);
            }
        }
        if default_rule_ids.is_empty() {
            default_rule_ids.push(format!("{DEFAULT_DENY_RULE_ID} (built-in)"));
        }
        Self {
            permitted_engine,
            default_rule_ids,
            permits: Vec::new(),
            chain: None,
            revocations,
            exempt_reason: None,
            attestation: None,
        }
    }

    /// Add a permit rule after validating it.
    pub fn with_permit(mut self, rule: WorkloadPermitRule) -> Result<Self, String> {
        rule.validate()?;
        if self.permits.iter().any(|p| p.id == rule.id) {
            return Err(format!("duplicate permit rule id '{}'", rule.id));
        }
        self.permits.push(rule);
        Ok(self)
    }

    /// Attach a chain manager for audit events.
    pub fn with_chain(mut self, cm: Arc<ChainManager>) -> Self {
        self.chain = Some(cm);
        self
    }

    /// Replace the revocation list checked on every request (an exempt gate
    /// becomes one that checks it).
    pub fn with_revocations(mut self, list: Arc<RevocationList>) -> Self {
        self.revocations = Some(list);
        self.exempt_reason = None;
        self
    }

    /// Decide for the verified project `att` instead of the kernel's
    /// instance attestation. Permit rules with `projects` match on it.
    pub fn with_attestation(mut self, att: ProjectAttestation) -> Self {
        self.attestation = Some(att);
        self
    }

    /// The verified project this gate decides for: its own attestation, else
    /// the kernel's (never anything from the request context).
    fn project(&self) -> Option<(ProjectAttestation, Option<&'static str>)> {
        match (
            &self.attestation,
            crate::governance_project::instance_project(),
        ) {
            (Some(a), inst) => Some((a.clone(), inst.map(|(_, i)| i))),
            (None, Some((a, i))) => Some((a.clone(), Some(i))),
            (None, None) => None,
        }
    }

    /// Permit rules in evaluation order.
    pub fn permits(&self) -> &[WorkloadPermitRule] {
        &self.permits
    }

    /// The denial for an action that is not a teardown action.
    pub(crate) fn not_teardown(action: &str) -> Option<GateDecision> {
        (!TEARDOWN_ACTIONS.contains(&action)).then(|| {
            Self::deny(format!(
                "'{action}' is not a teardown action ({})",
                TEARDOWN_ACTIONS.join(", ")
            ))
        })
    }

    fn deny(reason: impl Into<String>) -> GateDecision {
        GateDecision::Deny {
            reason: reason.into(),
            receipt: None,
        }
    }

    fn record(&self, kind: &str, payload: serde_json::Value) {
        if let Some(cm) = &self.chain {
            cm.append(CHAIN_SOURCE, kind, Some(payload));
        }
    }

    fn decide(&self, agent_id: &str, action: &str, context: &serde_json::Value) -> Outcome {
        self.decide_with(agent_id, action, context, false)
    }

    /// [`Self::decide`]; with `skip_revocation` the revocation list is not
    /// consulted (teardown only: see [`GateBackend::check_teardown`]).
    fn decide_with(
        &self,
        agent_id: &str,
        action: &str,
        context: &serde_json::Value,
        skip_revocation: bool,
    ) -> Outcome {
        let mut out = Outcome::new();
        let req = match WorkloadRequest::from_context(context) {
            Ok(r) => r,
            Err(e) => {
                out.decision = Self::deny(format!("invalid workload context: {e}"));
                return out;
            }
        };
        out.workload = serde_json::to_value(&req.effect).ok();
        let vector = req.effect.to_effect_vector();
        out.effect = serde_json::to_value(&vector).ok();

        if let Some(list) = self.revocations.as_ref().filter(|_| !skip_revocation) {
            if let Some(err) = list.subjects_error() {
                out.decision = Self::deny(format!("{REVOCATION_UNREADABLE}: {err}"));
                return out;
            }
            // A request that names nothing cannot be checked, so it cannot be
            // trusted: without this a caller (or a builder that lost the
            // refs) could omit all three and carry only a `package_trust`
            // claim past the list. Teardown actions are exempt: a stop or
            // unload names the placed instance, not a package.
            if NEEDS_REFS.contains(&action)
                && req.refs.package_id.is_none()
                && req.refs.signer_keys.is_empty()
                && req.refs.artifact_hashes.is_empty()
            {
                out.decision = Self::deny(format!(
                    "'{action}' names no package, signer key or artifact hash, so it cannot be \
                     checked against the revocation list (fail closed)"
                ));
                return out;
            }
            let hit = list.first_revoked(
                req.refs.package_id.as_deref(),
                &req.refs.signer_keys,
                &req.refs.artifact_hashes,
            );
            if let Some(s) = hit {
                out.decision =
                    Self::deny(format!("{} '{}' is revoked: {}", s.kind, s.id, s.reason));
                out.revoked = serde_json::to_value(&s).ok();
                return out;
            }
        }

        if req.effect.secrets && req.effect.node_tier < NodeTrustTier::Pinned {
            out.decision =
                Self::deny("workloads carrying secrets require a pinned node (ADR-099 s8.3)");
            return out;
        }

        let project = self.project();
        let project_id = project.as_ref().map(|(a, _)| a.project_id());
        let Some(permit) = self
            .permits
            .iter()
            .find(|p| p.matches_for(action, &req.effect, project_id, Some(agent_id)))
        else {
            out.evaluated_rules = self.default_rule_ids.clone();
            let why: Vec<String> = self
                .permits
                .iter()
                .filter(|p| p.actions.iter().any(|s| crate::governance::selector_matches(s, action)) && p.kinds.iter().any(|k| k == "*" || k == &req.effect.kind))
                .map(|p| {
                    let m = p.mismatches(action, &req.effect);
                    let m = if m.is_empty() { "project or principal scope does not match".to_owned() } else { m.join("; ") };
                    format!("permit '{}': {m}", p.id)
                })
                .collect();
            let detail = if why.is_empty() { String::new() } else { format!(" [{}]", why.join(" | ")) };
            out.decision = Self::deny(format!(
                "default deny: no workload permit rule matches '{action}' for kind '{}' (rule {}){detail}",
                req.effect.kind,
                self.default_rule_ids.join(", ")
            ));
            return out;
        };
        out.permit_rule = Some(permit.id.clone());

        let request = GovernanceRequest {
            agent_id: agent_id.to_owned(),
            action: action.to_owned(),
            effect: vector,
            context: req.effect.context_map(),
            node_id: None,
            principal: Some(GatePrincipal::agent(agent_id)),
        }
        // ADR-103 A6: the project comes from the verified attestation, so
        // permit rules and the audit principal can match on it.
        .attributed_with(
            project.as_ref().map(|(a, _)| a),
            project.as_ref().and_then(|(_, i)| *i),
        );
        let result = self.permitted_engine.evaluate(&request);
        out.evaluated_rules = result.evaluated_rules;
        out.threshold_exceeded = result.threshold_exceeded;
        out.decision = match result.decision {
            GovernanceDecision::Permit | GovernanceDecision::PermitWithWarning(_) => {
                GateDecision::Permit { token: None }
            }
            GovernanceDecision::EscalateToHuman(reason) => GateDecision::Defer { reason },
            GovernanceDecision::Deny(reason) => Self::deny(reason),
        };
        out
    }
}

/// Actions that carry a package and so must name it (see `decide`).
const NEEDS_REFS: &[&str] = &[
    "workload.install",
    "workload.place",
    "workload.load",
    "workload.start",
    "workload.migrate",
];

/// Actions [`GateBackend::check_teardown`] accepts: stop, unload, and
/// re-adoption of a placed instance (gated as `workload.load`).
const TEARDOWN_ACTIONS: &[&str] = &["workload.stop", "workload.unload", "workload.load"];

/// Intermediate result, turned into the chain payload.
struct Outcome {
    decision: GateDecision,
    workload: Option<serde_json::Value>,
    effect: Option<serde_json::Value>,
    permit_rule: Option<String>,
    evaluated_rules: Vec<String>,
    threshold_exceeded: bool,
    revoked: Option<serde_json::Value>,
}

impl Outcome {
    fn new() -> Self {
        Self {
            decision: GateDecision::Deny {
                reason: "undecided".into(),
                receipt: None,
            },
            workload: None,
            effect: None,
            permit_rule: None,
            evaluated_rules: Vec::new(),
            threshold_exceeded: false,
            revoked: None,
        }
    }
}

fn decision_parts(d: &GateDecision) -> (&'static str, Option<&str>) {
    match d {
        GateDecision::Permit { .. } => ("permit", None),
        GateDecision::Defer { reason } => ("defer", Some(reason)),
        GateDecision::Deny { reason, .. } => ("deny", Some(reason)),
    }
}

impl GateBackend for WorkloadGate {
    fn check(&self, agent_id: &str, action: &str, context: &serde_json::Value) -> GateDecision {
        if !action.starts_with(WORKLOAD_ACTION_PREFIX) {
            // Not ours: refuse rather than silently permit. Not chained,
            // because the workload audit trail covers workload.* only.
            return Self::deny(format!(
                "WorkloadGate only governs workload.* actions, got '{action}'"
            ));
        }
        if !is_governed_action(action) {
            let reason = format!(
                "unknown workload action '{action}'; governed actions: {}",
                GOVERNED_ACTIONS.join(", ")
            );
            self.record(
                chain::EVENT_KIND_WORKLOAD_REFUSE,
                serde_json::json!({
                    "decision": "deny",
                    "agent_id": agent_id,
                    "action": action,
                    "reason": reason,
                }),
            );
            return Self::deny(reason);
        }

        let out = self.decide(agent_id, action, context);
        let (decision, reason) = decision_parts(&out.decision);
        let kind = event_kind_for(action).unwrap_or(chain::EVENT_KIND_WORKLOAD_REFUSE);
        self.record(
            kind,
            serde_json::json!({
                "decision": decision,
                "reason": reason,
                "agent_id": agent_id,
                "action": action,
                "workload": out.workload,
                "effect": out.effect,
                "permit_rule": out.permit_rule,
                "evaluated_rules": out.evaluated_rules,
                "threshold_exceeded": out.threshold_exceeded,
                "revoked": out.revoked,
            }),
        );
        out.decision
    }

    fn check_teardown(
        &self,
        agent_id: &str,
        action: &str,
        context: &serde_json::Value,
    ) -> GateDecision {
        // Only taking something down (or re-attaching what is already
        // placed, gated as a load) may use this path; anything else could
        // be used to dodge the node-tier rules.
        if let Some(d) = Self::not_teardown(action) {
            debug_assert!(false, "check_teardown called with {action}");
            return d;
        }
        let first = self.check(agent_id, action, context);
        let GateDecision::Deny { reason, .. } = &first else {
            return first;
        };
        // Would the same request pass on a node of the highest tier, with the
        // revocation list set aside? Then the denial was the node tier, a
        // revocation, or both, and none of those blocks taking something
        // down: stopping a revoked package is the point of revoking it.
        // Default deny, rule and threshold denials do not depend on either
        // and stay denials.
        let mut raised = context.clone();
        let Some(w) = raised.get_mut("workload").and_then(|w| w.as_object_mut()) else {
            return first;
        };
        w.insert("node_tier".into(), serde_json::json!(NodeTrustTier::Pinned));
        if !self.decide_with(agent_id, action, &raised, true).decision.is_permit() {
            return first;
        }
        let tier_waived = !self.decide_with(agent_id, action, context, true).decision.is_permit();
        let revoked = self.decide(agent_id, action, context).revoked;
        let kind = event_kind_for(action).unwrap_or(chain::EVENT_KIND_WORKLOAD_REFUSE);
        self.record(
            kind,
            serde_json::json!({
                "decision": "permit", "agent_id": agent_id, "action": action,
                "teardown_node_tier_waived": tier_waived,
                "teardown_revocation_waived": revoked,
                "waived_denial": reason,
                "node_tier": context.pointer("/workload/node_tier"),
            }),
        );
        GateDecision::Permit { token: None }
    }
}

/// Chain every revocation and un-revocation `list` takes from now on, from
/// any caller (the operator verb, a mesh notice, kernel code), as
/// `workload.revoke` / `workload.unrevoke` events from source
/// [`CHAIN_SOURCE`]. First call wins; returns whether this one did.
pub fn chain_revocations(list: &RevocationList, chain: Arc<ChainManager>) -> bool {
    list.set_audit(Arc::new(move |kind, payload| {
        chain.append(CHAIN_SOURCE, kind, Some(payload));
    }))
}

/// Revoke a subject and chain a `workload.revoke` event when newly added.
///
/// Returns `Ok(true)` if newly revoked, `Ok(false)` if it already was. The
/// event is chained even when persisting the list fails (the entry holds in
/// memory, the event says `persisted: false`, and the error is returned).
/// With `chain` `None` the list's own audit sink ([`chain_revocations`])
/// records it, if one is installed.
pub fn revoke_and_record(
    list: &RevocationList,
    chain: Option<&ChainManager>,
    kind: RevocationKind,
    id: &str,
    reason: &str,
    revoked_by: &str,
) -> Result<bool, crate::revocation::RevocationError> {
    match chain {
        Some(cm) => list.revoke_audited(kind, id, reason, revoked_by, Some(&|k, p| {
            cm.append(CHAIN_SOURCE, k, Some(p));
        })),
        None => list.revoke_subject_by(kind, id, reason, revoked_by),
    }
}

/// Lift a revocation and chain a `workload.unrevoke` event when one was
/// removed. Same chain rules as [`revoke_and_record`].
pub fn unrevoke_and_record(
    list: &RevocationList,
    chain: Option<&ChainManager>,
    kind: RevocationKind,
    id: &str,
    unrevoked_by: &str,
) -> Result<bool, crate::revocation::RevocationError> {
    match chain {
        Some(cm) => list.unrevoke_audited(kind, id, unrevoked_by, Some(&|k, p| {
            cm.append(CHAIN_SOURCE, k, Some(p));
        })),
        None => list.unrevoke_subject_by(kind, id, unrevoked_by),
    }
}
