//! Workload governance actions, the default-deny rule set and permit rules
//! (ADR-099 section 4, ADR-092 distribution).

use serde::{Deserialize, Serialize};

use super::effect::{NetworkPolicy, NodeTrustTier, PackageTrust, WorkloadEffect};
use crate::governance::{
    GovernanceBranch, GovernanceRule, GovernanceRuleType, RuleSeverity, selector_matches,
};

/// Prefix shared by every workload governance action.
pub const WORKLOAD_ACTION_PREFIX: &str = "workload.";

/// Install a package on a node.
pub const ACTION_INSTALL: &str = "workload.install";
/// Place an instance on a node.
pub const ACTION_PLACE: &str = "workload.place";
/// Load an instance into a runtime.
pub const ACTION_LOAD: &str = "workload.load";
/// Start a loaded instance.
pub const ACTION_START: &str = "workload.start";
/// Stop a running instance.
pub const ACTION_STOP: &str = "workload.stop";
/// Unload an instance.
pub const ACTION_UNLOAD: &str = "workload.unload";
/// Migrate an instance to another node.
pub const ACTION_MIGRATE: &str = "workload.migrate";
/// Revoke a package, signer key or artifact hash.
pub const ACTION_REVOKE: &str = "workload.revoke";
/// Bind a fleet identity to a mesh node.
pub const ACTION_NODE_BIND: &str = "workload.node.bind";

/// Every action the workload gate governs. Any other `workload.*` string is
/// refused as unknown.
pub const GOVERNED_ACTIONS: &[&str] = &[
    ACTION_INSTALL,
    ACTION_PLACE,
    ACTION_LOAD,
    ACTION_START,
    ACTION_STOP,
    ACTION_UNLOAD,
    ACTION_MIGRATE,
    ACTION_REVOKE,
    ACTION_NODE_BIND,
];

/// Rule id of the `workload.*` default-deny rule.
pub const DEFAULT_DENY_RULE_ID: &str = "WORKLOAD-DEFAULT-DENY";
/// Rule id of the effect-magnitude ceiling that still applies after a permit.
pub const EFFECT_CEILING_RULE_ID: &str = "WORKLOAD-EFFECT-CEILING";
/// SOP category marking rules that a matching permit rule lifts.
pub const DEFAULT_DENY_CATEGORY: &str = "workload.default_deny";

/// Whether `action` is one of [`GOVERNED_ACTIONS`].
pub fn is_governed_action(action: &str) -> bool {
    GOVERNED_ACTIONS.contains(&action)
}

/// The workload rules shipped in the governance rule set.
///
/// - [`DEFAULT_DENY_RULE_ID`]: blocks every `workload.*` action regardless of
///   effect magnitude (`force_on_match`). This closes the gap where
///   `CapabilityGate` and a plain `GovernanceGate` permit unknown actions.
/// - [`EFFECT_CEILING_RULE_ID`]: blocks a `workload.*` action whose effect
///   magnitude exceeds the engine threshold, even when a permit rule matched.
///
/// Both are ordinary [`GovernanceRule`]s, so they travel through
/// [`crate::rule_distribution::RuleDistribution`] like any other rule.
pub fn default_rules() -> Vec<GovernanceRule> {
    let rule = |id: &str, desc: &str, category: &str, force: bool| GovernanceRule {
        id: id.into(),
        description: desc.into(),
        branch: GovernanceBranch::Legislative,
        severity: RuleSeverity::Blocking,
        active: true,
        reference_url: None,
        sop_category: Some(category.into()),
        rule_type: GovernanceRuleType::General,
        action_selector: Some(format!("{WORKLOAD_ACTION_PREFIX}*")),
        tool_selector: None,
        force_on_match: force,
    };
    vec![
        rule(
            DEFAULT_DENY_RULE_ID,
            "workload.* actions are denied unless an explicit workload permit rule matches (ADR-099 s4)",
            DEFAULT_DENY_CATEGORY,
            true,
        ),
        rule(
            EFFECT_CEILING_RULE_ID,
            "workload.* actions above the effect threshold are denied even when permitted",
            "workload.effect_ceiling",
            false,
        ),
    ]
}

/// Seed the workload rules into a cluster rule store (ADR-092).
pub fn install_default_rules(
    dist: &mut crate::rule_distribution::RuleDistribution,
    now_unix: u64,
) {
    for rule in default_rules() {
        dist.upsert_local(rule, now_unix);
    }
}

fn default_min_trust() -> PackageTrust {
    PackageTrust::PinnedSigner
}
fn default_min_tier() -> NodeTrustTier {
    NodeTrustTier::Paired
}
fn default_max_network() -> NetworkPolicy {
    NetworkPolicy::Lan
}
fn default_max_cost() -> f64 {
    1.0
}

/// An explicit allowance that lifts the `workload.*` default deny.
///
/// Every condition must hold. Defaults are the strict ones from ADR-099
/// section 8: pinned signer, paired node, LAN at most, no secrets, no
/// emulation, no accelerator.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkloadPermitRule {
    /// Unique id (recorded in the chain event when it matches).
    pub id: String,
    /// Why this permit exists.
    #[serde(default)]
    pub description: String,
    /// Action selectors (`workload.place`, or `workload.*`).
    pub actions: Vec<String>,
    /// Workload kinds allowed (`cog`, `inference`; `*` for any).
    pub kinds: Vec<String>,
    /// Least package trust allowed.
    #[serde(default = "default_min_trust")]
    pub min_package_trust: PackageTrust,
    /// Least node trust tier allowed.
    #[serde(default = "default_min_tier")]
    pub min_node_tier: NodeTrustTier,
    /// Most network exposure allowed.
    #[serde(default = "default_max_network")]
    pub max_network: NetworkPolicy,
    /// Whether secrets may be delivered.
    #[serde(default)]
    pub allow_secrets: bool,
    /// Whether emulated placement is allowed (operator opt-in).
    #[serde(default)]
    pub allow_emulated: bool,
    /// Accelerator id selectors allowed (`accel.gpu.*`). Empty: none.
    #[serde(default)]
    pub accelerators: Vec<String>,
    /// Highest resource cost allowed.
    #[serde(default = "default_max_cost")]
    pub max_resource_cost: f64,
    /// Verified project ids this permit applies to (ADR-103 A6). Empty: any.
    /// Matched against the gate's verified attestation, never against
    /// anything the request carries; with a non-empty list and no verified
    /// project the rule does not match.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub projects: Vec<String>,
    /// Principals (the gate's `agent_id`) this permit applies to. Empty:
    /// any. With a non-empty list a request whose principal is unknown, or
    /// not listed, does not match. The `project` kind uses it to limit its
    /// permit to [`SUPERVISOR_PRINCIPAL`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub principals: Vec<String>,
}

/// The principal the daemon's catalog verbs (`workload install` / `unload`)
/// decide as. The catalog verifies nothing, so those requests are `unsigned`;
/// a permit that accepts unsigned packages has to be limited to this
/// principal (`validate` refuses one that names none), so it cannot also
/// admit an unsigned placement.
pub const CATALOG_PRINCIPAL: &str = "catalog";

/// The internal principal the user-daemon project supervisor acts as. Never
/// accepted from a request: the supervisor builds its own gate and host with
/// this id, so an operator-facing placement path (whose gate has no such
/// permit) cannot start a `project` workload.
pub const SUPERVISOR_PRINCIPAL: &str = "weftos.project-supervisor";

/// The permit that lets the supervisor, and only the supervisor, run the
/// `project` kind on this node. It does not reach any other kind.
pub fn project_supervisor_permit() -> WorkloadPermitRule {
    let mut rule = WorkloadPermitRule::new(
        "PROJECT-SUPERVISOR-PERMIT",
        [ACTION_LOAD, ACTION_START, ACTION_STOP, ACTION_UNLOAD],
        ["project"],
    );
    rule.description = "the project supervisor may run per-project child kernels (ADR-103 A6)".into();
    rule.min_package_trust = PackageTrust::ProjectCert;
    rule.min_node_tier = NodeTrustTier::Paired;
    rule.max_network = NetworkPolicy::None;
    rule.principals = vec![SUPERVISOR_PRINCIPAL.into()];
    rule
}

impl WorkloadPermitRule {
    /// Strict permit for `actions` on workloads of `kinds`.
    pub fn new(
        id: impl Into<String>,
        actions: impl IntoIterator<Item = impl Into<String>>,
        kinds: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            id: id.into(),
            description: String::new(),
            actions: actions.into_iter().map(Into::into).collect(),
            kinds: kinds.into_iter().map(Into::into).collect(),
            min_package_trust: default_min_trust(),
            min_node_tier: default_min_tier(),
            max_network: default_max_network(),
            allow_secrets: false,
            allow_emulated: false,
            accelerators: Vec::new(),
            max_resource_cost: default_max_cost(),
            projects: Vec::new(),
            principals: Vec::new(),
        }
    }

    /// Reject rules that are empty, malformed, or reach outside `workload.*`.
    pub fn validate(&self) -> Result<(), String> {
        if self.id.trim().is_empty() {
            return Err("permit rule id must not be empty".into());
        }
        if self.actions.is_empty() || self.kinds.is_empty() {
            return Err(format!("permit rule '{}' needs at least one action and one kind", self.id));
        }
        for a in &self.actions {
            let known = is_governed_action(a)
                || a == "workload.*"
                || (a.ends_with('*') && a.starts_with(WORKLOAD_ACTION_PREFIX));
            if !known {
                return Err(format!(
                    "permit rule '{}': action selector '{a}' is not a governed workload action",
                    self.id
                ));
            }
        }
        if self.kinds.iter().any(|k| k.is_empty()) || self.accelerators.iter().any(|a| a.is_empty()) {
            return Err(format!("permit rule '{}': empty kind or accelerator selector", self.id));
        }
        if self.min_package_trust == PackageTrust::Unsigned && self.principals.is_empty() {
            return Err(format!(
                "permit rule '{}' accepts unsigned packages, so it must name the principals it \
                 applies to (e.g. [\"{CATALOG_PRINCIPAL}\"] for the catalog verbs)",
                self.id
            ));
        }
        if self.principals.iter().any(|p| p.is_empty()) {
            return Err(format!("permit rule '{}': empty principal selector", self.id));
        }
        if self.projects.iter().any(|p| p.is_empty()) {
            return Err(format!("permit rule '{}': empty project selector", self.id));
        }
        if !self.max_resource_cost.is_finite() || !(0.0..=1.0).contains(&self.max_resource_cost) {
            return Err(format!("permit rule '{}': max_resource_cost must be in [0, 1]", self.id));
        }
        Ok(())
    }

    /// Why this rule does not permit `action` with `effect`, one entry per
    /// failing condition. Empty when only the project or principal scope (which
    /// the caller knows) keeps it from matching, or when it does match.
    pub fn mismatches(&self, action: &str, effect: &WorkloadEffect) -> Vec<String> {
        let mut out = Vec::new();
        if !self.actions.iter().any(|s| selector_matches(s, action)) {
            out.push(format!("action '{action}' is not listed"));
        }
        if !self.kinds.iter().any(|k| k == "*" || k == &effect.kind) {
            out.push(format!("kind '{}' is not listed", effect.kind));
        }
        if effect.package_trust < self.min_package_trust {
            out.push(format!("package trust {:?} is below min_package_trust {:?}", effect.package_trust, self.min_package_trust));
        }
        if effect.node_tier < self.min_node_tier {
            out.push(format!("node tier {:?} is below min_node_tier {:?}", effect.node_tier, self.min_node_tier));
        }
        if effect.network > self.max_network {
            out.push(format!("network {:?} exceeds max_network {:?}", effect.network, self.max_network));
        }
        if effect.secrets && !self.allow_secrets {
            out.push("secrets are not allowed (allow_secrets)".into());
        }
        if effect.emulated && !self.allow_emulated {
            out.push("emulated placement is not allowed (allow_emulated)".into());
        }
        if effect.resource_cost > self.max_resource_cost {
            out.push(format!("resource cost {} exceeds max_resource_cost {}", effect.resource_cost, self.max_resource_cost));
        }
        out
    }

    /// Advisory problems with a rule that is valid but can never match what it
    /// is for. A native cog runs with the host's network (ADR-100 s4), so the
    /// placement path classes it `egress`; a `cog` permit capped below that
    /// refuses every native cog.
    pub fn warnings(&self) -> Vec<String> {
        let mut out = Vec::new();
        let places = self.actions.iter().any(|a| selector_matches(a, "workload.place"));
        if places && self.kinds.iter().any(|k| k == "cog") && self.max_network < NetworkPolicy::Egress {
            out.push(format!(
                "permit rule '{}' allows kind 'cog' with max_network {:?}, but a native cog is classed \
                 network 'egress' (it has the host's network): it will refuse every native cog; set \
                 max_network to \"egress\"",
                self.id, self.max_network
            ));
        }
        out
    }

    /// Whether this rule permits `action` with `effect`.
    pub fn matches(&self, action: &str, effect: &WorkloadEffect) -> bool {
        self.matches_in(action, effect, None)
    }

    /// [`matches`](Self::matches) for a request attributed to `project`
    /// (the verified project id, or `None` outside any project).
    pub fn matches_in(&self, action: &str, effect: &WorkloadEffect, project: Option<&str>) -> bool {
        self.matches_for(action, effect, project, None)
    }

    /// [`matches_in`](Self::matches_in) for a request made by `principal`
    /// (the gate's agent id; `None` when unknown, which never satisfies a
    /// rule that lists principals).
    pub fn matches_for(
        &self,
        action: &str,
        effect: &WorkloadEffect,
        project: Option<&str>,
        principal: Option<&str>,
    ) -> bool {
        let principal_ok =
            self.principals.is_empty() || principal.is_some_and(|p| self.principals.iter().any(|x| x == p));
        let project_ok = self.projects.is_empty() || project.is_some_and(|p| self.projects.iter().any(|x| x == p));
        let action_ok = self.actions.iter().any(|s| selector_matches(s, action));
        let kind_ok = self.kinds.iter().any(|k| k == "*" || k == &effect.kind);
        let accel_ok = match &effect.accelerator {
            None => true,
            Some(acc) => self.accelerators.iter().any(|s| selector_matches(s, acc)),
        };
        principal_ok
            && project_ok
            && action_ok
            && kind_ok
            && accel_ok
            && effect.package_trust >= self.min_package_trust
            && effect.node_tier >= self.min_node_tier
            && effect.network <= self.max_network
            && (self.allow_secrets || !effect.secrets)
            && (self.allow_emulated || !effect.emulated)
            && effect.resource_cost <= self.max_resource_cost
    }
}
