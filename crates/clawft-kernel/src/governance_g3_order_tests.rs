//! G3 (WO-036): rule order and precedence across merged layers and nodes.
//!
//! Pins the intended order of the strictness lattice (deny beats escalate
//! beats permit) for the parent + overlay merge, whatever the position of a
//! rule in the engine, and shows which inputs besides the rules change a
//! verdict. Review: `docs/governance/r9-g3-rule-order-review.md`.

use std::mem::discriminant;

use clawft_types::config::overlay::Limits;

use crate::gate::{GateBackend, GovernanceGate};
use crate::governance::{
    GovernanceDecision, GovernanceEngine, GovernanceRequest, GovernanceRule, RuleSeverity,
};
use crate::governance_overlay::merge;
use crate::governance_overlay_tests::{engine_of, ov, parent_with, rule};

#[derive(Debug, PartialEq, Clone, Copy)]
enum Kind {
    Permit,
    Warn,
    Escalate,
    Deny,
}

fn kind(d: &GovernanceDecision) -> Kind {
    match d {
        GovernanceDecision::Permit => Kind::Permit,
        GovernanceDecision::PermitWithWarning(_) => Kind::Warn,
        GovernanceDecision::EscalateToHuman(_) => Kind::Escalate,
        GovernanceDecision::Deny(_) => Kind::Deny,
    }
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

fn decide(e: &GovernanceEngine, action: &str, tool: Option<&str>, risk: f64) -> Kind {
    let mut req = GovernanceRequest::new("agent", action);
    req.effect.risk = risk;
    if let Some(t) = tool {
        req.context.insert("tool".into(), t.into());
    }
    kind(&e.evaluate(&req).decision)
}

/// Every order of `rules` must give `expected` for each case.
fn assert_position_independent(
    rules: &[GovernanceRule],
    threshold: f64,
    human: bool,
    cases: &[(&str, Option<&str>, f64, Kind)],
) {
    for order in perms(rules) {
        let e = engine_of(order.clone(), threshold, human);
        for (action, tool, risk, want) in cases {
            let got = decide(&e, action, *tool, *risk);
            assert_eq!(
                got,
                *want,
                "{action} (human={human}) in order {:?}",
                order.iter().map(|r| r.id.as_str()).collect::<Vec<_>>()
            );
        }
    }
}

fn base() -> Vec<GovernanceRule> {
    vec![
        rule("GOV-001", RuleSeverity::Blocking, None, false, true),
        rule("WL-DENY", RuleSeverity::Blocking, Some("workload.*"), true, true),
        rule("GOV-ADV", RuleSeverity::Advisory, None, false, true),
    ]
}

#[test]
fn deny_beats_escalate_beats_permit_in_every_position() {
    let parent = parent_with(base(), Limits::default(), 1);
    let o = ov(r#"
schema = 1
[[deny]]
id = "no-shell"
actions = ["tool.shell*"]
[[require_approval]]
id = "ask-cron"
actions = ["cron.add*"]
"#);
    let eff = merge(&parent, &o).unwrap();
    let human = eff.human_approval(false);
    let t = eff.risk_threshold(0.8);
    let cases = [
        // parent deny, not escalated while the flag is off
        ("workload.place", None, 0.0, Kind::Deny),
        // overlay deny
        ("tool.shell_exec", None, 0.0, Kind::Deny),
        // overlay approval alone escalates
        ("cron.add", None, 0.0, Kind::Escalate),
        // nothing matches at zero effect: permit
        ("tool.read", None, 0.0, Kind::Permit),
        // magnitude path of the parent's un-forced blocking rule
        ("tool.read", None, 0.95, Kind::Deny),
    ];
    assert_position_independent(&eff.rules, t, human, &cases);
}

#[test]
fn an_overlay_that_sets_human_approval_never_softens_a_parent_deny() {
    let parent = parent_with(base(), Limits::default(), 1);
    let o = ov(r#"
schema = 1
[[deny]]
id = "no-shell"
actions = ["tool.shell*"]
[[require_approval]]
id = "ask-cron"
actions = ["cron.add*"]
[limits]
human_approval_required = true
"#);
    let eff = merge(&parent, &o).unwrap();
    assert!(eff.human_approval(false));
    let cases = [
        ("workload.place", None, 0.0, Kind::Deny),
        ("tool.shell_exec", None, 0.0, Kind::Deny),
        ("cron.add", None, 0.0, Kind::Escalate),
        ("tool.read", None, 0.95, Kind::Deny),
    ];
    assert_position_independent(&eff.rules, eff.risk_threshold(0.8), true, &cases);
}

#[test]
fn an_approval_overlapping_a_deny_the_merge_cannot_see_is_still_a_deny() {
    // The merge's overlap check only reads parent rules with an action
    // selector and no tool selector. A parent deny that also names a tool is
    // outside that view, so the overlay approval is accepted; the engine must
    // still answer Deny in every order (approval never outranks a block).
    let mut p = rule("P-CRON", RuleSeverity::Blocking, Some("cron.add*"), true, true);
    p.tool_selector = Some("cron".into());
    let parent = parent_with(vec![p], Limits::default(), 1);
    let o = ov(r#"
schema = 1
[[require_approval]]
id = "ask-cron"
actions = ["cron.add*"]
"#);
    let eff = merge(&parent, &o).expect("approval over a tool-scoped parent deny is accepted");
    let cases = [
        ("cron.add", Some("cron"), 0.0, Kind::Deny),
        // Without the tool in context the parent rule does not apply; only
        // the approval does.
        ("cron.add", None, 0.0, Kind::Escalate),
    ];
    assert_position_independent(&eff.rules, 0.8, false, &cases);
}

#[test]
fn nodes_with_the_same_parent_and_overlay_agree_whatever_the_rule_listing_order() {
    let o = ov(r#"
schema = 1
[[deny]]
id = "no-shell"
actions = ["tool.shell*"]
"#);
    let mut hashes = std::collections::BTreeSet::new();
    for order in perms(&base()) {
        let eff = merge(&parent_with(order, Limits::default(), 1), &o).unwrap();
        hashes.insert(eff.effective_hash);
    }
    assert_eq!(hashes.len(), 1, "effective_hash depends on listing order");
}

#[test]
fn rule_sets_are_not_the_whole_verdict_limits_are_hashed_too() {
    // Same rules, different engine parameters: different verdicts. The merge
    // hash covers limits, so such nodes never look equal.
    let parent = parent_with(base(), Limits::default(), 1);
    let loose = merge(&parent, &ov("schema = 1\n")).unwrap();
    let tight = merge(&parent, &ov("schema = 1\n[limits]\nrisk_threshold = 0.1\n")).unwrap();
    assert_ne!(loose.effective_hash, tight.effective_hash);
    let a = engine_of(loose.rules.clone(), loose.risk_threshold(0.8), false);
    let b = engine_of(tight.rules.clone(), tight.risk_threshold(0.8), false);
    assert_eq!(decide(&a, "tool.read", None, 0.5), Kind::Permit);
    assert_eq!(decide(&b, "tool.read", None, 0.5), Kind::Deny);
    // The human-approval flag turns the same blocking match into Escalate.
    let r = vec![rule("B", RuleSeverity::Blocking, Some("x.*"), true, true)];
    assert_eq!(decide(&engine_of(r.clone(), 0.8, false), "x.y", None, 0.0), Kind::Deny);
    assert_eq!(decide(&engine_of(r, 0.8, true), "x.y", None, 0.0), Kind::Escalate);
}

#[test]
fn known_hazard_an_operator_exemption_overrides_an_overlay_hard_deny() {
    // Characterization (finding F1, owner decision): `exempt_action` turns any
    // Deny or Escalate into Permit for an exactly-named action, including the
    // overlay's hard deny that the engine never escalates. Today that is the
    // documented operator grant; this test fails if the precedence changes so
    // the change is a conscious one.
    let parent = parent_with(vec![], Limits::default(), 1);
    let o = ov(r#"
schema = 1
[[deny]]
id = "no-spawn"
actions = ["tool.agent_spawn"]
"#);
    let eff = merge(&parent, &o).unwrap();
    let mut open = GovernanceGate::new(0.8, false)
        .with_eval_rate_limit(crate::rate_limit::RateLimitConfig::unlimited());
    let mut granted = GovernanceGate::new(0.8, false)
        .with_eval_rate_limit(crate::rate_limit::RateLimitConfig::unlimited())
        .exempt_action("tool.agent_spawn");
    for r in eff.rules.iter().cloned() {
        open = open.add_rule(r.clone());
        granted = granted.add_rule(r);
    }
    let ctx = serde_json::json!({});
    assert_ne!(
        discriminant(&open.check_detailed("a", "tool.agent_spawn", &ctx).unwrap().decision),
        discriminant(&GovernanceDecision::Permit),
        "without the grant the overlay deny holds"
    );
    let v = granted.check_detailed("a", "tool.agent_spawn", &ctx).unwrap();
    assert!(v.grant_applied && matches!(v.decision, GovernanceDecision::Permit));
}
