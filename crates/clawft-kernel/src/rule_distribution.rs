//! Cluster-wide [`GovernanceRule`] distribution (WEFT-146).
//!
//! Replaces the assumption that every node holds an independent static
//! `Vec<GovernanceRule>` with a **versioned rule set** that can be
//! gossiped, merged after split-brain, and used for cross-node
//! escalation when local policy denies.
//!
//! # Wire shape
//!
//! Peers exchange [`RuleGossipEnvelope`] messages (JSON over mesh IPC or
//! any other transport). Merge is last-writer-wins on `(rule_id, version)`
//! with a deterministic tie-break on `updated_at_unix` then `origin_node`.
//!
//! # Escalation
//!
//! When local evaluation returns Deny, callers may consult
//! [`RuleDistribution::escalate`] which records an
//! [`EscalationRecord`] and — if a peer is known to hold a higher
//! version of a matching rule — returns [`EscalationOutcome::Forward`].

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::governance::{GovernanceDecision, GovernanceRule, RuleSeverity};

/// Versioned wrapper around a [`GovernanceRule`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionedRule {
    /// Monotonic per-rule version (origin node increments on edit).
    pub version: u64,
    /// Unix seconds of last mutation.
    pub updated_at_unix: u64,
    /// Node that last authored this version.
    pub origin_node: String,
    /// Rule body.
    pub rule: GovernanceRule,
}

impl VersionedRule {
    /// Construct version 1 of a rule.
    pub fn new(rule: GovernanceRule, origin_node: impl Into<String>, now_unix: u64) -> Self {
        Self {
            version: 1,
            updated_at_unix: now_unix,
            origin_node: origin_node.into(),
            rule,
        }
    }

    /// Bump version after a local edit.
    pub fn bump(&mut self, origin_node: impl Into<String>, now_unix: u64) {
        self.version = self.version.saturating_add(1);
        self.updated_at_unix = now_unix;
        self.origin_node = origin_node.into();
    }
}

/// Gossip payload carrying a full or partial rule set.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleGossipEnvelope {
    /// Sender node id.
    pub from_node: String,
    /// Rules being advertised / pushed.
    pub rules: Vec<VersionedRule>,
    /// Sender's aggregate set version (sum of rule versions) for cheap
    /// "do I need a full sync?" checks.
    pub set_version: u64,
}

/// Outcome of a cross-node escalation attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EscalationOutcome {
    /// No peer has a more recent matching rule — local deny stands.
    LocalStand,
    /// Forward the decision request to `peer` which holds a newer rule.
    Forward {
        /// Peer node id.
        peer: String,
        /// Rule id that motivated the forward.
        rule_id: String,
        /// Peer's rule version.
        peer_version: u64,
    },
}

/// Audit trail entry for an escalation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EscalationRecord {
    /// Local node.
    pub local_node: String,
    /// Action that was denied.
    pub action: String,
    /// Agent that requested it.
    pub agent_id: String,
    /// Outcome summary.
    pub outcome: String,
    /// Unix seconds.
    pub at_unix: u64,
}

/// In-memory cluster rule store with merge + escalation.
#[derive(Debug, Default)]
pub struct RuleDistribution {
    local_node: String,
    /// rule_id → versioned rule
    rules: HashMap<String, VersionedRule>,
    /// Best-known remote version per rule_id → (node, version)
    remote_hints: HashMap<String, (String, u64)>,
    escalations: Vec<EscalationRecord>,
}

impl RuleDistribution {
    /// Create an empty store for `local_node`.
    pub fn new(local_node: impl Into<String>) -> Self {
        Self {
            local_node: local_node.into(),
            rules: HashMap::new(),
            remote_hints: HashMap::new(),
            escalations: Vec::new(),
        }
    }

    /// Insert or replace a locally authored rule (starts at v1 or bumps).
    pub fn upsert_local(&mut self, rule: GovernanceRule, now_unix: u64) {
        let id = rule.id.clone();
        match self.rules.get_mut(&id) {
            Some(existing) => {
                existing.rule = rule;
                existing.bump(self.local_node.clone(), now_unix);
            }
            None => {
                self.rules
                    .insert(id, VersionedRule::new(rule, self.local_node.clone(), now_unix));
            }
        }
    }

    /// Snapshot of active rules (for engine injection).
    pub fn active_rules(&self) -> Vec<GovernanceRule> {
        self.rules
            .values()
            .filter(|v| v.rule.active)
            .map(|v| v.rule.clone())
            .collect()
    }

    /// Aggregate set version (sum of per-rule versions).
    pub fn set_version(&self) -> u64 {
        self.rules.values().map(|v| v.version).sum()
    }

    /// Build a gossip envelope of the full local set.
    pub fn gossip_full(&self) -> RuleGossipEnvelope {
        RuleGossipEnvelope {
            from_node: self.local_node.clone(),
            rules: self.rules.values().cloned().collect(),
            set_version: self.set_version(),
        }
    }

    /// Merge a remote gossip envelope (LWW on version, then timestamp, then origin).
    ///
    /// Returns the number of rules accepted (inserted or upgraded).
    pub fn merge(&mut self, envelope: &RuleGossipEnvelope) -> usize {
        let mut accepted = 0usize;
        for remote in &envelope.rules {
            // Track remote hints for escalation even if we don't accept.
            self.remote_hints.insert(
                remote.rule.id.clone(),
                (envelope.from_node.clone(), remote.version),
            );

            match self.rules.get(&remote.rule.id) {
                None => {
                    self.rules
                        .insert(remote.rule.id.clone(), remote.clone());
                    accepted += 1;
                }
                Some(local) => {
                    if Self::remote_wins(local, remote) {
                        self.rules
                            .insert(remote.rule.id.clone(), remote.clone());
                        accepted += 1;
                    }
                }
            }
        }
        accepted
    }

    fn remote_wins(local: &VersionedRule, remote: &VersionedRule) -> bool {
        use std::cmp::Ordering;
        match remote.version.cmp(&local.version) {
            Ordering::Greater => true,
            Ordering::Less => false,
            Ordering::Equal => match remote.updated_at_unix.cmp(&local.updated_at_unix) {
                Ordering::Greater => true,
                Ordering::Less => false,
                Ordering::Equal => match remote.origin_node.cmp(&local.origin_node) {
                    Ordering::Greater => true,
                    Ordering::Less => false,
                    // Same version, time and origin but a different body (a
                    // restored node, a reused node id). Both sides must pick
                    // the same one or the cluster never converges (G3): the
                    // stricter body wins, then the canonical JSON text.
                    Ordering::Equal => Self::body_wins(&local.rule, &remote.rule),
                },
            },
        }
    }

    /// Total order on rule bodies for an exact version tie: `remote` wins when
    /// it is strictly greater. Stricter first (active, higher severity,
    /// `force_on_match`), then the serialized text, so the result does not
    /// depend on which side runs the merge.
    fn body_wins(local: &GovernanceRule, remote: &GovernanceRule) -> bool {
        let key = |r: &GovernanceRule| {
            (
                r.active,
                r.severity.clone(),
                r.force_on_match,
                serde_json::to_string(r).unwrap_or_default(),
            )
        };
        key(remote) > key(local)
    }

    /// After a local Deny, decide whether to escalate to a peer that holds
    /// a newer version of a rule matching `action`.
    pub fn escalate(
        &mut self,
        agent_id: &str,
        action: &str,
        local_decision: &GovernanceDecision,
        now_unix: u64,
    ) -> EscalationOutcome {
        let is_deny = matches!(local_decision, GovernanceDecision::Deny(_));
        if !is_deny {
            return EscalationOutcome::LocalStand;
        }

        // Prefer remote peers with strictly higher version for a rule
        // whose id/description mentions the action, or any Blocking rule.
        let mut best: Option<(String, String, u64)> = None;
        for (rule_id, (peer, ver)) in &self.remote_hints {
            let local_ver = self.rules.get(rule_id).map(|r| r.version).unwrap_or(0);
            if *ver <= local_ver {
                continue;
            }
            let relevant = self
                .rules
                .get(rule_id)
                .map(|r| {
                    r.rule.severity == RuleSeverity::Blocking
                        || r.rule.severity == RuleSeverity::Critical
                        || r.rule.id.contains(action)
                        || action.contains(&r.rule.id)
                })
                .unwrap_or(true);
            if !relevant {
                continue;
            }
            let replace = match &best {
                None => true,
                Some((_, _, bv)) => ver > bv,
            };
            if replace {
                best = Some((peer.clone(), rule_id.clone(), *ver));
            }
        }

        let outcome = match best {
            Some((peer, rule_id, peer_version)) => EscalationOutcome::Forward {
                peer,
                rule_id,
                peer_version,
            },
            None => EscalationOutcome::LocalStand,
        };

        self.escalations.push(EscalationRecord {
            local_node: self.local_node.clone(),
            action: action.into(),
            agent_id: agent_id.into(),
            outcome: format!("{outcome:?}"),
            at_unix: now_unix,
        });
        outcome
    }

    /// Escalation audit trail.
    pub fn escalation_log(&self) -> &[EscalationRecord] {
        &self.escalations
    }

    /// Number of rules currently held.
    pub fn len(&self) -> usize {
        self.rules.len()
    }

    /// True when no rules are held.
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::governance::{GovernanceBranch, GovernanceRule, RuleSeverity};

    fn rule(id: &str, severity: RuleSeverity) -> GovernanceRule {
        GovernanceRule {
            id: id.into(),
            description: format!("test {id}"),
            severity,
            branch: GovernanceBranch::Judicial,
            active: true,
            reference_url: None,
            sop_category: None,
            rule_type: Default::default(),
            action_selector: None,
            tool_selector: None,
            force_on_match: false,
        }
    }

    #[test]
    fn split_merge_lww() {
        let mut a = RuleDistribution::new("node-a");
        let mut b = RuleDistribution::new("node-b");

        a.upsert_local(rule("exec-guard", RuleSeverity::Blocking), 100);
        b.upsert_local(rule("exec-guard", RuleSeverity::Warning), 100);
        // B edits again → higher version
        b.upsert_local(rule("exec-guard", RuleSeverity::Critical), 200);

        // Simulate split: each has local view; then merge B → A
        let gossip = b.gossip_full();
        let accepted = a.merge(&gossip);
        assert_eq!(accepted, 1);
        let active = a.active_rules();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].severity, RuleSeverity::Critical);
        assert_eq!(a.rules.get("exec-guard").unwrap().version, 2);
    }

    #[test]
    fn merge_ignores_stale() {
        let mut a = RuleDistribution::new("node-a");
        let mut b = RuleDistribution::new("node-b");
        a.upsert_local(rule("r1", RuleSeverity::Blocking), 100);
        a.upsert_local(rule("r1", RuleSeverity::Critical), 200); // v2
        b.upsert_local(rule("r1", RuleSeverity::Advisory), 50); // v1 stale
        let accepted = a.merge(&b.gossip_full());
        assert_eq!(accepted, 0);
        assert_eq!(a.rules.get("r1").unwrap().version, 2);
    }

    #[test]
    fn escalation_forwards_when_peer_newer() {
        let mut local = RuleDistribution::new("node-a");
        local.upsert_local(rule("cron-guard", RuleSeverity::Blocking), 100);
        // Inject remote hint as if we saw a gossip with higher version
        local.remote_hints.insert(
            "cron-guard".into(),
            ("node-b".into(), 5),
        );
        let outcome = local.escalate(
            "agent-1",
            "cron.add",
            &GovernanceDecision::Deny("blocked".into()),
            999,
        );
        match outcome {
            EscalationOutcome::Forward {
                peer,
                rule_id,
                peer_version,
            } => {
                assert_eq!(peer, "node-b");
                assert_eq!(rule_id, "cron-guard");
                assert_eq!(peer_version, 5);
            }
            other => panic!("expected Forward, got {other:?}"),
        }
        assert_eq!(local.escalation_log().len(), 1);
    }

    #[test]
    fn escalation_stands_on_permit() {
        let mut local = RuleDistribution::new("node-a");
        let outcome = local.escalate(
            "agent-1",
            "tool.read",
            &GovernanceDecision::Permit,
            1,
        );
        assert_eq!(outcome, EscalationOutcome::LocalStand);
    }

    #[test]
    fn serde_gossip_roundtrip() {
        let mut a = RuleDistribution::new("n1");
        a.upsert_local(rule("x", RuleSeverity::Warning), 1);
        let env = a.gossip_full();
        let json = serde_json::to_string(&env).unwrap();
        let back: RuleGossipEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(back.from_node, "n1");
        assert_eq!(back.rules.len(), 1);
    }

    // ---- G3: cross-node rule order and precedence (WO-036) ----

    use crate::governance::{GovernanceEngine, GovernanceRequest};

    fn versioned(id: &str, sev: RuleSeverity, version: u64, ts: u64, origin: &str) -> VersionedRule {
        VersionedRule {
            version,
            updated_at_unix: ts,
            origin_node: origin.into(),
            rule: rule(id, sev),
        }
    }

    fn env(from: &str, rules: Vec<VersionedRule>) -> RuleGossipEnvelope {
        RuleGossipEnvelope { from_node: from.into(), rules, set_version: 0 }
    }

    /// Order-independent fingerprint of a node's whole rule table.
    fn canon(d: &RuleDistribution) -> Vec<String> {
        let mut v: Vec<String> = d
            .rules
            .values()
            .map(|r| serde_json::to_string(r).unwrap())
            .collect();
        v.sort();
        v
    }

    fn perms<T: Clone>(xs: &[T]) -> Vec<Vec<T>> {
        if xs.len() <= 1 {
            return vec![xs.to_vec()];
        }
        let mut out = Vec::new();
        for i in 0..xs.len() {
            let mut rest = xs.to_vec();
            let head = rest.remove(i);
            for mut p in perms(&rest) {
                p.insert(0, head.clone());
                out.push(p);
            }
        }
        out
    }

    fn converge(order: &[RuleGossipEnvelope]) -> RuleDistribution {
        let mut d = RuleDistribution::new("observer");
        for e in order {
            d.merge(e);
        }
        d
    }

    #[test]
    fn exact_version_tie_converges_and_the_stricter_body_wins() {
        // Same version, time and origin, different bodies: a restored node or
        // a reused node id. Before the fix each side kept what it saw first.
        let e1 = env("p", vec![versioned("r", RuleSeverity::Warning, 1, 100, "node-x")]);
        let e2 = env("q", vec![versioned("r", RuleSeverity::Critical, 1, 100, "node-x")]);
        let ab = converge(&[e1.clone(), e2.clone()]);
        let ba = converge(&[e2, e1]);
        assert_eq!(canon(&ab), canon(&ba), "merge order changed the result");
        assert_eq!(ab.active_rules()[0].severity, RuleSeverity::Critical);
    }

    #[test]
    fn merge_converges_in_every_delivery_order() {
        let e1 = env(
            "n1",
            vec![
                versioned("a", RuleSeverity::Blocking, 2, 50, "n1"),
                versioned("b", RuleSeverity::Warning, 1, 10, "n1"),
                versioned("t", RuleSeverity::Advisory, 1, 100, "n1"),
            ],
        );
        let e2 = env(
            "n2",
            vec![
                versioned("a", RuleSeverity::Advisory, 2, 60, "n2"),
                versioned("b", RuleSeverity::Critical, 3, 5, "n2"),
                versioned("t", RuleSeverity::Blocking, 1, 100, "n1"),
            ],
        );
        let e3 = env(
            "n3",
            vec![
                versioned("a", RuleSeverity::Critical, 2, 60, "n3"),
                versioned("b", RuleSeverity::Blocking, 3, 5, "n3"),
                versioned("t", RuleSeverity::Critical, 1, 100, "n1"),
            ],
        );
        let all = [e1, e2, e3];
        let reference = canon(&converge(&all));
        for order in perms(&all) {
            let once = converge(&order);
            assert_eq!(canon(&once), reference);
            // Re-delivery is a no-op.
            let mut twice = converge(&order);
            for e in &order {
                assert_eq!(twice.merge(e), 0, "a second delivery must change nothing");
            }
            assert_eq!(canon(&twice), reference);
        }
    }

    fn kinds_for(rules: Vec<GovernanceRule>, human: bool) -> Vec<std::mem::Discriminant<GovernanceDecision>> {
        let mut e = GovernanceEngine::new(0.5, human)
            .with_eval_rate_limit(crate::rate_limit::RateLimitConfig::unlimited());
        for r in rules {
            e.add_rule(r);
        }
        let mut out = Vec::new();
        for action in ["tool.read", "tool.shell", "net.fetch"] {
            for risk in [0.0, 0.9] {
                let mut req = GovernanceRequest::new("agent", action);
                req.effect.risk = risk;
                out.push(std::mem::discriminant(&e.evaluate(&req).decision));
            }
        }
        out
    }

    #[test]
    fn converged_nodes_decide_alike_whatever_order_their_rules_load_in() {
        let mut forced = rule("shell-deny", RuleSeverity::Blocking);
        forced.action_selector = Some("tool.shell".into());
        forced.force_on_match = true;
        let e1 = env(
            "n1",
            vec![
                VersionedRule::new(rule("mag", RuleSeverity::Critical), "n1", 1),
                VersionedRule::new(rule("warn", RuleSeverity::Warning), "n1", 1),
            ],
        );
        let e2 = env("n2", vec![VersionedRule::new(forced, "n2", 2)]);
        for human in [false, true] {
            let mut reference = None;
            for order in perms(&[e1.clone(), e2.clone()]) {
                let d = converge(&order);
                let mut rules = d.active_rules();
                for _ in 0..2 {
                    let got = kinds_for(rules.clone(), human);
                    assert_eq!(*reference.get_or_insert_with(|| got.clone()), got);
                    rules.reverse();
                }
            }
        }
    }

    #[test]
    fn known_hazard_a_newer_edit_can_loosen_a_rule_everywhere() {
        // Characterization, not an endorsement: merge is last-writer-wins on
        // version, not tighten-only, and an envelope carries no signature
        // here. A higher version with `active = false` disables a blocking
        // rule on every node it reaches. Owner decision recorded in
        // docs/governance/r9-g3-rule-order-review.md (finding F2).
        let mut a = RuleDistribution::new("node-a");
        let mut forced = rule("guard", RuleSeverity::Critical);
        forced.force_on_match = true;
        a.upsert_local(forced.clone(), 1);
        let mut b = RuleDistribution::new("node-b");
        b.merge(&a.gossip_full());
        forced.active = false;
        b.upsert_local(forced, 2);
        assert_eq!(a.merge(&b.gossip_full()), 1);
        let mut e = GovernanceEngine::new(0.5, false)
            .with_eval_rate_limit(crate::rate_limit::RateLimitConfig::unlimited());
        for r in a.active_rules() {
            e.add_rule(r);
        }
        assert!(matches!(
            e.evaluate(&GovernanceRequest::new("agent", "tool.x")).decision,
            GovernanceDecision::Permit
        ));
    }

    #[test]
    fn known_gap_escalation_cannot_forward_after_a_real_merge() {
        // Characterization (finding F3): a merge accepts any higher version,
        // so no hint can ever stay above the local version; `escalate` only
        // forwards in the unit test that writes `remote_hints` directly.
        let mut local = RuleDistribution::new("node-a");
        local.upsert_local(rule("cron-guard", RuleSeverity::Blocking), 100);
        let mut peer = RuleDistribution::new("node-b");
        peer.merge(&local.gossip_full());
        for t in 0..5 {
            peer.upsert_local(rule("cron-guard", RuleSeverity::Blocking), 200 + t);
        }
        local.merge(&peer.gossip_full());
        let out = local.escalate("a", "cron.add", &GovernanceDecision::Deny("x".into()), 9);
        assert_eq!(out, EscalationOutcome::LocalStand);
    }
}
