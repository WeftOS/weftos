//! G0 re-verification probe (Tapestry plan Q16, 2026-11-07).
//!
//! G0 was a rule-order bug: a Warning-severity browser_policy match cleared a
//! block set by an earlier rule, so the same rules in a different order could
//! turn a Deny into a Permit. Fixed in `fe59c46ab`; the in-crate test
//! `rule_order_does_not_change_decision` covers one request over 720 orders.
//!
//! This probe widens the check: several rule sets that mix every rule kind and
//! severity the evaluator branches on (general Blocking/Warning/Advisory,
//! overlay approval and overlay deny, browser_policy at Blocking and Warning,
//! binding-thread at Blocking and Warning, force-on-match), several request
//! shapes (browser spawn, high and low effect, binding-thread mismatch), and
//! both `human_approval_required` settings. For every (set, config, request)
//! it evaluates every permutation of the rules and asserts the decision kind
//! never changes. It also reruns the original G0 pair in both orders.

use std::collections::{BTreeMap, BTreeSet};

use clawft_kernel::governance::{
    OVERLAY_APPROVAL_TAG, OVERLAY_DENY_TAG, binding_thread_default_rules,
    browser_policy_default_rules,
};
use clawft_kernel::{
    EffectVector, GovernanceBranch, GovernanceDecision, GovernanceEngine, GovernanceRequest,
    GovernanceRule, GovernanceRuleType, RuleSeverity,
};

fn general(id: &str, severity: RuleSeverity) -> GovernanceRule {
    GovernanceRule {
        id: id.into(),
        description: format!("probe rule {id}"),
        branch: GovernanceBranch::Judicial,
        severity,
        active: true,
        reference_url: None,
        sop_category: None,
        rule_type: GovernanceRuleType::General,
        action_selector: None,
        tool_selector: None,
        force_on_match: false,
        principal_selector: None,
    }
}

fn overlay(id: &str, tag: &str) -> GovernanceRule {
    let mut r = general(id, RuleSeverity::Blocking);
    r.sop_category = Some(tag.into());
    r.force_on_match = true;
    r
}

fn with_severity(mut r: GovernanceRule, s: RuleSeverity) -> GovernanceRule {
    r.severity = s;
    r
}

fn kind(d: &GovernanceDecision) -> &'static str {
    match d {
        GovernanceDecision::Permit => "Permit",
        GovernanceDecision::PermitWithWarning(_) => "PermitWithWarning",
        GovernanceDecision::EscalateToHuman(_) => "EscalateToHuman",
        GovernanceDecision::Deny(_) => "Deny",
        // The enum is #[non_exhaustive]; a new kind must show up, not hide.
        _ => "Other",
    }
}

fn permutations(items: &[GovernanceRule]) -> Vec<Vec<GovernanceRule>> {
    fn go(v: &mut Vec<GovernanceRule>, k: usize, out: &mut Vec<Vec<GovernanceRule>>) {
        if k == v.len() {
            out.push(v.clone());
            return;
        }
        for i in k..v.len() {
            v.swap(k, i);
            go(v, k + 1, out);
            v.swap(k, i);
        }
    }
    let mut v = items.to_vec();
    let mut out = Vec::new();
    go(&mut v, 0, &mut out);
    out
}

fn requests() -> Vec<(&'static str, GovernanceRequest)> {
    let high = EffectVector { risk: 0.95, security: 0.95, ..Default::default() };
    let low = EffectVector { risk: 0.05, ..Default::default() };

    let mut browser_spawn_low = GovernanceRequest::new("agent-1", "tool.agent_spawn").with_effect(low.clone());
    browser_spawn_low.context.insert("tool".into(), "agent_spawn".into());
    browser_spawn_low.context.insert("platform".into(), "browser".into());

    let mut browser_spawn_high = browser_spawn_low.clone();
    browser_spawn_high.effect = high.clone();

    let native_high = GovernanceRequest::new("agent-1", "tool.file_write").with_effect(high.clone());
    let native_low = GovernanceRequest::new("agent-1", "tool.file_read").with_effect(low);

    let mut binding = GovernanceRequest::new("agent-1", "soul.binding_thread_intact").with_effect(high);
    binding.context.insert("binding_thread_status".into(), "mismatch".into());

    vec![
        ("browser_spawn_low", browser_spawn_low),
        ("browser_spawn_high", browser_spawn_high),
        ("native_high", native_high),
        ("native_low", native_low),
        ("binding_mismatch", binding),
    ]
}

fn rule_sets() -> Vec<(&'static str, Vec<GovernanceRule>)> {
    let bp = browser_policy_default_rules();
    let bt = binding_thread_default_rules();
    let warn_bp = with_severity(
        GovernanceRule::browser_policy("BP-002W", "warning-severity spawn rule"),
        RuleSeverity::Warning,
    );
    vec![
        (
            "mixed_blocking",
            vec![
                GovernanceRule::spawn_requires_approval(),
                warn_bp.clone(),
                bp[0].clone(),
                bt[0].clone(),
                general("GEN-BLOCK", RuleSeverity::Blocking),
                general("GEN-WARN", RuleSeverity::Warning),
                general("GEN-ADV", RuleSeverity::Advisory),
                overlay("OVL-APPROVE", OVERLAY_APPROVAL_TAG),
            ],
        ),
        (
            "soft_only",
            vec![
                warn_bp.clone(),
                with_severity(bp[0].clone(), RuleSeverity::Warning),
                with_severity(bt[0].clone(), RuleSeverity::Warning),
                general("GEN-WARN", RuleSeverity::Warning),
                general("GEN-ADV", RuleSeverity::Advisory),
                overlay("OVL-APPROVE", OVERLAY_APPROVAL_TAG),
                GovernanceRule::spawn_requires_approval(),
            ],
        ),
        (
            // No blocking or approval rule at all: reaches Permit and
            // PermitWithWarning, the outcomes G0 could wrongly produce.
            "warnings_only",
            vec![
                with_severity(
                    GovernanceRule::browser_policy("BP-002W", "warning-severity spawn rule"),
                    RuleSeverity::Warning,
                ),
                with_severity(bp[0].clone(), RuleSeverity::Warning),
                with_severity(bt[0].clone(), RuleSeverity::Warning),
                general("GEN-WARN", RuleSeverity::Warning),
                general("GEN-WARN-2", RuleSeverity::Warning),
                general("GEN-ADV", RuleSeverity::Advisory),
                with_severity(general("GEN-BLOCK-OFF", RuleSeverity::Blocking), RuleSeverity::Advisory),
            ],
        ),
        (
            "overlay_deny_vs_approval",
            vec![
                overlay("OVL-APPROVE", OVERLAY_APPROVAL_TAG),
                overlay("OVL-DENY", OVERLAY_DENY_TAG),
                warn_bp,
                with_severity(bt[0].clone(), RuleSeverity::Warning),
                general("GEN-WARN", RuleSeverity::Warning),
                general("GEN-ADV", RuleSeverity::Advisory),
                general("GEN-BLOCK", RuleSeverity::Blocking),
            ],
        ),
    ]
}

#[test]
fn original_g0_pair_denies_in_both_orders() {
    let warn_bp = with_severity(
        GovernanceRule::browser_policy("BP-002", "Browser nodes cannot spawn child agents without capability elevation"),
        RuleSeverity::Warning,
    );
    let mut req = GovernanceRequest::new("agent-1", "tool.agent_spawn");
    req.context.insert("tool".into(), "agent_spawn".into());
    req.context.insert("platform".into(), "browser".into());
    for order in [
        vec![GovernanceRule::spawn_requires_approval(), warn_bp.clone()],
        vec![warn_bp.clone(), GovernanceRule::spawn_requires_approval()],
    ] {
        let mut e = GovernanceEngine::new(0.7, false);
        for r in order {
            e.add_rule(r);
        }
        let d = e.evaluate(&req).decision;
        assert!(matches!(d, GovernanceDecision::Deny(_)), "expected Deny, got {d:?}");
    }
}

#[test]
fn decision_kind_never_depends_on_rule_order() {
    let mut summary: BTreeMap<String, BTreeSet<&'static str>> = BTreeMap::new();
    let mut evaluations = 0usize;
    for (set_name, rules) in rule_sets() {
        let perms = permutations(&rules);
        for human in [false, true] {
            for (req_name, req) in requests() {
                let mut kinds = BTreeSet::new();
                for perm in &perms {
                    // A fresh engine per order: the per-principal evaluation
                    // rate limit must not bleed between permutations.
                    let mut e = GovernanceEngine::new(0.7, human);
                    for r in perm {
                        e.add_rule(r.clone());
                    }
                    kinds.insert(kind(&e.evaluate(&req).decision));
                    evaluations += 1;
                }
                assert_eq!(
                    kinds.len(),
                    1,
                    "{set_name} human_approval={human} {req_name}: order changed the decision: {kinds:?}"
                );
                summary.insert(format!("{set_name} human={human} {req_name}"), kinds);
            }
        }
    }
    for (k, v) in &summary {
        println!("G0-REVERIFY {k}: {v:?}");
    }
    let all: BTreeSet<_> = summary.values().flatten().copied().collect();
    println!("G0-REVERIFY evaluations={evaluations} distinct_kinds={all:?}");
    // The probe is only meaningful if it reaches more than one outcome.
    assert!(all.len() >= 3, "probe too narrow: only reached {all:?}");
}
