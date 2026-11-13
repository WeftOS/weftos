//! Tests for the kernel-side governance overlay (ADR-103 D8, package E):
//! the merge table, the tighten-only property, the hashes, the engine
//! semantics of the generated rules, and bounded deterministic fuzzing of
//! the overlay and parent-policy files.

use chrono::{TimeZone, Utc};
use clawft_types::config::overlay::{
    Limits, OverlayError as MergeError,
};
use ed25519_dalek::SigningKey;

use crate::governance::{
    GovernanceBranch, GovernanceDecision, GovernanceEngine, GovernanceRequest,
    GovernanceRule, GovernanceRuleType, OVERLAY_APPROVAL_TAG, OVERLAY_DENY_TAG, RuleSeverity,
};
use crate::governance_overlay::{Overlay, OverlayError, merge};
use crate::parent_policy::{
    PARENT_POLICY_DOMAIN, ParentPolicy, ParentPolicyError, export_rules, verify_parent_policy,
};

pub(crate) fn user_key() -> SigningKey {
    SigningKey::from_bytes(&[7u8; 32])
}

pub(crate) fn rule(
    id: &str,
    severity: RuleSeverity,
    selector: Option<&str>,
    force: bool,
    active: bool,
) -> GovernanceRule {
    GovernanceRule {
        id: id.into(),
        description: format!("test rule {id}"),
        branch: GovernanceBranch::Legislative,
        severity,
        active,
        reference_url: None,
        sop_category: None,
        rule_type: GovernanceRuleType::General,
        action_selector: selector.map(str::to_owned),
        tool_selector: None,
        force_on_match: force,
        principal_selector: None,
    }
}

/// A signed parent policy over `rules` with threshold 0.8 and no human gate.
pub(crate) fn parent_with(rules: Vec<GovernanceRule>, limits: Limits, version: u64) -> ParentPolicy {
    export_rules(
        rules,
        0.8,
        false,
        &limits,
        &user_key(),
        version,
        Utc.with_ymd_and_hms(2026, 10, 1, 10, 0, 0).unwrap(),
    )
    .unwrap()
}

pub(crate) fn base_parent() -> ParentPolicy {
    parent_with(
        vec![
            rule("GOV-001", RuleSeverity::Blocking, None, false, true),
            rule("WL-DENY", RuleSeverity::Blocking, Some("workload.*"), true, true),
            rule("GOV-ADV", RuleSeverity::Advisory, None, false, true),
        ],
        Limits {
            max_processes: Some(64),
            spawn_budget: Some(8),
            ..Limits::default()
        },
        1,
    )
}

pub(crate) fn ov(toml: &str) -> Overlay {
    Overlay::from_toml(toml).unwrap_or_else(|e| panic!("overlay should parse: {e}"))
}

fn decide(engine: &GovernanceEngine, action: &str) -> GovernanceDecision {
    engine.evaluate(&GovernanceRequest::new("agent", action)).decision
}

pub(crate) fn engine_of(rules: Vec<GovernanceRule>, threshold: f64, human: bool) -> GovernanceEngine {
    let mut e = GovernanceEngine::new(threshold, human)
        .with_eval_rate_limit(crate::rate_limit::RateLimitConfig::unlimited());
    for r in rules {
        e.add_rule(r);
    }
    e
}

// ── the merge table ──────────────────────────────────────────────────────

#[test]
fn adding_a_deny_and_lowering_limits_is_accepted() {
    let o = ov(r#"
schema = 1
[[deny]]
id = "project.no-shell"
actions = ["tool.shell_exec", "workload.place*"]
reason = "this project never runs shell tools"
[[require_approval]]
actions = ["cron.add*"]
[limits]
risk_threshold = 0.5
max_processes = 32
spawn_budget = 4
human_approval_required = true
"#);
    let e = merge(&base_parent(), &o).unwrap();
    assert_eq!(e.limits.max_processes, Some(32));
    assert_eq!(e.limits.spawn_budget, Some(4));
    assert_eq!(e.limits.risk_threshold, Some(0.5));
    assert_eq!(e.limits.human_approval_required, Some(true));
    assert!(e.deny_actions.contains(&"tool.shell_exec".to_owned()));
    assert!(e.deny_actions.contains(&"workload.*".to_owned()), "parent deny kept");
    assert_eq!(e.require_approval_actions, vec!["cron.add*".to_owned()]);
    let ids: Vec<&str> = e.rules.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "GOV-001",
            "WL-DENY",
            "GOV-ADV",
            "project.no-shell[0]",
            "project.no-shell[1]",
            "overlay.approval[0]"
        ]
    );
    let d = &e.rules[3];
    assert_eq!(d.sop_category.as_deref(), Some(OVERLAY_DENY_TAG));
    assert!(d.force_on_match && d.active && d.severity == RuleSeverity::Blocking);
    assert_eq!(d.description, "this project never runs shell tools");
    assert_eq!(e.rules[5].sop_category.as_deref(), Some(OVERLAY_APPROVAL_TAG));
}

#[test]
fn an_empty_overlay_changes_nothing_but_the_hash_inputs() {
    let p = base_parent();
    let e = merge(&p, &Overlay::empty()).unwrap();
    assert_eq!(e.rules.len(), p.rules.len());
    assert_eq!(e.limits, p.limits);
}

fn refused(toml: &str) -> OverlayError {
    merge(&base_parent(), &ov(toml)).expect_err("overlay must be refused")
}

#[test]
fn every_relaxation_is_refused_and_names_its_key() {
    let cases: &[(&str, &str, &str)] = &[
        ("raise max_processes", "[limits]\nmax_processes = 65\n", "limits.max_processes"),
        ("raise spawn_budget", "[limits]\nspawn_budget = 9\n", "limits.spawn_budget"),
        (
            "raise risk threshold",
            "[limits]\nrisk_threshold = 0.9\n",
            "limits.risk_threshold",
        ),
        (
            "permit",
            "permit = [\"tool.shell_exec\"]\n",
            "permit",
        ),
        ("deactivate", "deactivate = [\"GOV-001\"]\n", "deactivate"),
        (
            "shadow a parent id",
            "[[deny]]\nid = \"GOV-001\"\nactions = [\"a.b\"]\n",
            "deny[0].id",
        ),
        (
            "shadow by case",
            "[[deny]]\nid = \"gov-001\"\nactions = [\"a.b\"]\n",
            "deny[0].id",
        ),
        (
            "shadow by whitespace",
            "[[deny]]\nid = \" GOV-001 \"\nactions = [\"a.b\"]\n",
            "deny[0].id",
        ),
        (
            "malformed glob",
            "[[deny]]\nid = \"x\"\nactions = [\"a*b\"]\n",
            "deny[0].actions[0]",
        ),
        (
            "empty glob",
            "[[require_approval]]\nactions = [\"\"]\n",
            "require_approval[0].actions[0]",
        ),
        (
            "unknown top-level key",
            "frobnicate = 1\n",
            "frobnicate",
        ),
    ];
    for (name, toml, key) in cases {
        let e = refused(toml);
        assert_eq!(&e.key(), key, "{name}: {e}");
        assert!(e.boot_message().contains(key), "{name}");
    }
}

#[test]
fn a_parent_human_gate_cannot_be_switched_off() {
    let parent = export_rules(
        vec![],
        0.8,
        true,
        &Limits::default(),
        &user_key(),
        1,
        Utc::now(),
    )
    .unwrap();
    let e = merge(&parent, &ov("[limits]\nhuman_approval_required = false\n")).unwrap_err();
    assert_eq!(e.key(), "limits.human_approval_required");
    assert!(matches!(e, OverlayError::Overlay(MergeError::Relaxes { .. })));
}

#[test]
fn overlay_ids_cannot_collide_with_generated_ids() {
    // `a[0]` cannot be written as an overlay id, but a parent rule may own it.
    let mut p = base_parent();
    p.rules.push(rule("x[0]", RuleSeverity::Advisory, None, false, true));
    let e = merge(
        &p,
        &ov("[[deny]]\nid = \"x\"\nactions = [\"a.b\", \"c.d\"]\n"),
    )
    .unwrap_err();
    assert!(matches!(e, OverlayError::IdCollision { .. }), "{e}");
}

// ── glob semantics ───────────────────────────────────────────────────────

#[test]
fn trailing_star_is_a_prefix_match_and_nothing_else() {
    let e = merge(
        &base_parent(),
        &ov("[[deny]]\nid = \"d\"\nactions = [\"workload.place*\", \"tool.exact\"]\n"),
    )
    .unwrap();
    // Drop the parent's `workload.*` rule so only the overlay is under test.
    let rules: Vec<_> = e.rules.into_iter().filter(|r| r.id.starts_with("d[")).collect();
    let eng = engine_of(rules, 0.8, false);
    for hit in ["workload.place", "workload.placement.x", "workload.place.now", "tool.exact"] {
        assert!(matches!(decide(&eng, hit), GovernanceDecision::Deny(_)), "{hit}");
    }
    for miss in ["workload.plac", "workload.start", "tool.exact.x", "tool.exac", "xworkload.place"] {
        assert_eq!(decide(&eng, miss), GovernanceDecision::Permit, "{miss}");
    }
}

// ── engine semantics of the generated rules ──────────────────────────────

#[test]
fn an_overlay_deny_is_a_deny_even_when_the_engine_escalates_to_a_human() {
    let e = merge(
        &base_parent(),
        &ov("[[deny]]\nid = \"d\"\nactions = [\"tool.shell_exec\"]\n[[require_approval]]\nactions = [\"cron.add\"]\n"),
    )
    .unwrap();
    for human in [false, true] {
        let eng = engine_of(e.rules.clone(), 0.8, human);
        assert!(
            matches!(decide(&eng, "tool.shell_exec"), GovernanceDecision::Deny(_)),
            "human={human}"
        );
        // Approval rules escalate whether or not the engine flag is on.
        assert!(
            matches!(decide(&eng, "cron.add"), GovernanceDecision::EscalateToHuman(_)),
            "human={human}"
        );
        assert_eq!(decide(&eng, "tool.read_file"), GovernanceDecision::Permit);
    }
}

#[test]
fn a_deny_beats_an_approval_on_the_same_action() {
    let e = merge(
        &base_parent(),
        &ov("[[deny]]\nid = \"d\"\nactions = [\"a.b\"]\n[[require_approval]]\nactions = [\"a.b\"]\n"),
    )
    .unwrap();
    let eng = engine_of(e.rules, 0.8, false);
    assert!(matches!(decide(&eng, "a.b"), GovernanceDecision::Deny(_)));
}

// ── hashes ───────────────────────────────────────────────────────────────

#[test]
fn overlay_hash_ignores_comments_and_whitespace_but_not_content() {
    let a = ov("schema = 1\n[[deny]]\nid = \"d\"\nactions = [\"b.x\", \"a.x\"]\n");
    let b = ov("# note\nschema=1\n\n[[deny]]\n  id=\"d\"\n  actions=[\"a.x\",\"b.x\"]\n");
    assert_eq!(a.hash, b.hash);
    let c = ov("schema = 1\n[[deny]]\nid = \"d\"\nactions = [\"a.x\"]\n");
    assert_ne!(a.hash, c.hash);
    assert_ne!(a.hash, Overlay::empty().hash);
}

#[test]
fn the_effective_hash_covers_active_severity_selector_and_limits() {
    let o = Overlay::empty();
    let base = merge(&base_parent(), &o).unwrap().effective_hash;
    let tweak = |f: &dyn Fn(&mut ParentPolicy)| {
        let mut p = base_parent();
        f(&mut p);
        merge(&p, &o).unwrap().effective_hash
    };
    let flipped_active = tweak(&|p| p.rules[0].active = false);
    let weaker = tweak(&|p| p.rules[0].severity = RuleSeverity::Warning);
    let selector = tweak(&|p| p.rules[0].action_selector = Some("x.*".into()));
    let force = tweak(&|p| p.rules[0].force_on_match = true);
    let limit = tweak(&|p| p.limits.max_processes = Some(63));
    let all = [base, flipped_active, weaker, selector, force, limit];
    for (i, a) in all.iter().enumerate() {
        for b in &all[i + 1..] {
            assert_ne!(a, b);
        }
    }
}

#[test]
fn rule_order_does_not_change_the_hashes_and_the_file_hash_is_recomputed() {
    let p = base_parent();
    let mut q = p.clone();
    q.rules.reverse();
    // The stored rule_hash is ignored by the merge: lie in it.
    q.rule_hash = "00".repeat(32);
    let (a, b) = (merge(&p, &Overlay::empty()).unwrap(), merge(&q, &Overlay::empty()).unwrap());
    assert_eq!(a.parent_hash, b.parent_hash);
    assert_eq!(a.effective_hash, b.effective_hash);
}

#[test]
fn effective_hash_depends_on_the_overlay() {
    let a = merge(&base_parent(), &Overlay::empty()).unwrap();
    let b = merge(
        &base_parent(),
        &ov("[[deny]]\nid = \"d\"\nactions = [\"a.b\"]\n"),
    )
    .unwrap();
    assert_eq!(a.parent_hash, b.parent_hash);
    assert_ne!(a.overlay_hash, b.overlay_hash);
    assert_ne!(a.effective_hash, b.effective_hash);
}

// ── parent policy: signature, tamper, domain ─────────────────────────────

#[test]
fn a_signed_policy_verifies_and_every_tamper_is_refused() {
    let pk = user_key().verifying_key().to_bytes();
    let good = base_parent();
    verify_parent_policy(&good, &pk).unwrap();

    let mut t = good.clone();
    t.rules[0].active = false;
    assert_eq!(verify_parent_policy(&t, &pk), Err(ParentPolicyError::RuleHash));

    // Fix up the hash too: only the signature stands in the way now.
    let mut t = good.clone();
    t.rules[0].active = false;
    t.rule_hash = hex(&crate::parent_policy::parent_rules_hash(&t.rules, &t.limits));
    assert_eq!(verify_parent_policy(&t, &pk), Err(ParentPolicyError::BadSignature));

    let mut t = good.clone();
    t.version += 1;
    assert_eq!(verify_parent_policy(&t, &pk), Err(ParentPolicyError::BadSignature));

    let mut t = good.clone();
    t.limits.max_processes = Some(1000);
    assert!(verify_parent_policy(&t, &pk).is_err());

    let mut t = good.clone();
    t.sig = "00".repeat(64);
    assert_eq!(verify_parent_policy(&t, &pk), Err(ParentPolicyError::BadSignature));
    t.sig = "xyz".into();
    assert_eq!(verify_parent_policy(&t, &pk), Err(ParentPolicyError::SigFormat));

    // A different user key is refused by id before its signature is looked at.
    let other = SigningKey::from_bytes(&[9u8; 32]).verifying_key().to_bytes();
    assert_eq!(verify_parent_policy(&good, &other), Err(ParentPolicyError::UserKey));
}

fn hex(b: &[u8; 32]) -> String {
    clawft_types::project::canon::hex_encode(b)
}

#[test]
fn a_signature_made_for_another_statement_type_does_not_verify() {
    use ed25519_dalek::Signer;
    let pk = user_key().verifying_key().to_bytes();
    let mut p = base_parent();
    // Same body, signed under the certificate's domain tag instead.
    let body = p.signed_bytes();
    let body = &body[PARENT_POLICY_DOMAIN.len()..];
    let mut other = clawft_types::project::cert::CERT_DOMAIN.as_bytes().to_vec();
    other.extend_from_slice(body);
    p.sig = clawft_types::project::canon::hex_encode(&user_key().sign(&other).to_bytes());
    assert_eq!(verify_parent_policy(&p, &pk), Err(ParentPolicyError::BadSignature));
}

#[test]
fn export_refuses_an_out_of_range_threshold() {
    for bad in [f64::NAN, -0.1, 1.5] {
        let r = export_rules(vec![], bad, false, &Limits::default(), &user_key(), 1, Utc::now());
        assert!(matches!(r, Err(ParentPolicyError::Limit("risk_threshold"))), "{bad}");
    }
}


#[test]
fn an_approval_overlapping_a_parent_deny_is_refused() {
    // WL-DENY denies `workload.*`; approvals under it would soften the deny.
    for glob in ["workload.start*", "workload.*", "workload.start", "work*"] {
        let e = refused(&format!("[[require_approval]]\nactions = [\"{glob}\"]\n"));
        assert!(matches!(e, OverlayError::ApprovalOverlapsDeny { .. }), "{glob}: {e}");
        assert_eq!(e.key(), "require_approval[0].actions[0]");
    }
    // A disjoint glob is fine.
    assert!(merge(&base_parent(), &ov("[[require_approval]]\nactions = [\"cron.add\"]\n")).is_ok());
}

#[test]
fn an_approval_never_softens_another_blocking_rule() {
    // An untagged forced deny and a tagged approval on the same action.
    let mut deny = rule("P-DENY", RuleSeverity::Blocking, Some("a.b"), true, true);
    deny.sop_category = None;
    let mut appr = rule("O-APPR", RuleSeverity::Blocking, Some("a.b"), true, true);
    appr.sop_category = Some(OVERLAY_APPROVAL_TAG.into());
    let eng = engine_of(vec![deny, appr.clone()], 0.8, false);
    assert!(matches!(decide(&eng, "a.b"), GovernanceDecision::Deny(_)));

    // A magnitude rule that fires also keeps its Deny.
    let mag = rule("P-MAG", RuleSeverity::Blocking, None, false, true);
    let eng = engine_of(vec![mag, appr.clone()], 0.5, false);
    let req = GovernanceRequest::new("agent", "a.b").with_effect(crate::governance::EffectVector {
        risk: 0.9,
        security: 0.9,
        ..Default::default()
    });
    assert!(matches!(eng.evaluate(&req).decision, GovernanceDecision::Deny(_)));

    // The approval alone still escalates.
    let eng = engine_of(vec![appr], 0.8, false);
    assert!(matches!(decide(&eng, "a.b"), GovernanceDecision::EscalateToHuman(_)));
}

#[test]
fn a_zero_process_cap_is_refused_everywhere() {
    let e = refused("[limits]\nmax_processes = 0\n");
    assert_eq!(e.key(), "limits.max_processes");
    let p = export_rules(
        vec![],
        0.8,
        false,
        &Limits {
            max_processes: Some(0),
            ..Limits::default()
        },
        &user_key(),
        1,
        Utc::now(),
    );
    assert!(matches!(p, Err(ParentPolicyError::Limit("max_processes"))));
}

#[test]
fn a_parent_without_a_threshold_still_caps_the_overlay_at_the_engine_default() {
    let mut p = base_parent();
    p.limits.risk_threshold = None;
    let e = merge(&p, &ov("[limits]\nrisk_threshold = 0.9\n")).unwrap_err();
    assert_eq!(e.key(), "limits.risk_threshold");
    let ok = merge(&p, &ov("[limits]\nrisk_threshold = 0.5\n")).unwrap();
    assert_eq!(ok.limits.risk_threshold, Some(0.5));
    // No overlay value: the effective ceiling is the default, spelled out.
    let none = merge(&p, &Overlay::empty()).unwrap();
    assert_eq!(none.limits.risk_threshold, Some(crate::overlay_runtime::DEFAULT_RISK_THRESHOLD));
}

#[test]
fn an_overlay_raised_human_flag_never_softens_a_parent_deny() {
    // Review M1: `[limits] human_approval_required = true` alone.
    let raise = ov("[limits]\nhuman_approval_required = true\n");
    let e = merge(&base_parent(), &raise).unwrap();
    assert_eq!(e.limits.human_approval_required, Some(true));
    let eng = engine_of(e.rules.clone(), 0.8, true);
    // The parent's force-matched deny stays a Deny under the flag...
    for action in ["workload.place", "workload.start"] {
        assert!(matches!(decide(&eng, action), GovernanceDecision::Deny(_)), "{action}");
    }
    // ...and a magnitude-blocked action is denied, not offered for approval.
    let hot = GovernanceRequest::new("agent", "tool.anything").with_effect(crate::governance::EffectVector {
        risk: 1.0,
        security: 1.0,
        ..Default::default()
    });
    assert!(matches!(eng.evaluate(&hot).decision, GovernanceDecision::Deny(_)));
    // The same rules without the re-tag WOULD have escalated (the hole).
    let untagged = engine_of(base_parent().rules, 0.8, true);
    assert!(matches!(decide(&untagged, "workload.place"), GovernanceDecision::EscalateToHuman(_)));
    // Non-denied actions still pass; the overlay's own approval rules still ask.
    assert_eq!(decide(&eng, "tool.read_file"), GovernanceDecision::Permit);
    let e = merge(&base_parent(), &ov("[[require_approval]]\nactions = [\"cron.add\"]\n[limits]\nhuman_approval_required = true\n")).unwrap();
    let eng = engine_of(e.rules, 0.8, true);
    assert!(matches!(decide(&eng, "cron.add"), GovernanceDecision::EscalateToHuman(_)));
    assert!(matches!(decide(&eng, "workload.place"), GovernanceDecision::Deny(_)));
}

#[test]
fn a_parent_that_asks_for_human_approval_keeps_its_own_semantics() {
    // The flag came from the parent: its rules are not re-tagged, so its
    // blocking verdicts escalate exactly as on the user daemon.
    let p = parent_with(
        base_parent().rules,
        Limits { human_approval_required: Some(true), ..Limits::default() },
        1,
    );
    let e = merge(&p, &Overlay::empty()).unwrap();
    assert!(e.rules.iter().all(|r| r.sop_category.as_deref() != Some(OVERLAY_DENY_TAG)));
}
