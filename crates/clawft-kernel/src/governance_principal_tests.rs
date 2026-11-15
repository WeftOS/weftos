//! Principal selector (card gov-asker-selector, owner 2026-11-13): a rule may
//! name who it applies to. A rule without the selector applies to every
//! principal exactly as before.
//!
//! The rules here are built from JSON (the wire and chain form) and overlay
//! TOML, so the tests state the contract in the formats that are persisted.

use clawft_types::config::overlay::Limits;
use serde_json::json;

use crate::governance::{
    GovernanceDecision, GovernanceEngine, GovernanceRequest, GovernanceRule,
};
use crate::governance_overlay::{Overlay, merge};
use crate::governance_overlay_tests::{base_parent, engine_of, ov, parent_with};
use crate::parent_policy::verify_parent_policy;

/// A blocking rule on `action`, scoped to `principal` when given.
fn rule_json(id: &str, action: &str, principal: Option<&str>) -> GovernanceRule {
    let mut v = json!({
        "id": id,
        "description": "test rule",
        "branch": "Judicial",
        "severity": "Blocking",
        "force_on_match": true,
        "action_selector": action,
    });
    if let Some(p) = principal {
        v["principal_selector"] = json!(p);
    }
    serde_json::from_value(v).expect("rule parses")
}

/// Reads fields by their serialised name, so these tests state the stored
/// form and do not depend on a struct field existing.
mod erased {
    pub trait Ser {
        fn json(&self) -> serde_json::Value;
    }
    impl<T: serde::Serialize> Ser for T {
        fn json(&self) -> serde_json::Value {
            serde_json::to_value(self).unwrap()
        }
    }
}

fn ask(engine: &GovernanceEngine, agent: &str, action: &str) -> GovernanceDecision {
    engine.evaluate(&GovernanceRequest::new(agent, action)).decision
}

fn is_deny(d: &GovernanceDecision) -> bool {
    matches!(d, GovernanceDecision::Deny(_))
}

#[test]
fn a_rule_scoped_to_one_principal_denies_it_and_leaves_the_others_on_the_default() {
    let e = engine_of(vec![rule_json("no-shell-bot-a", "tool.shell_exec", Some("bot-a"))], 0.7, false);
    assert!(is_deny(&ask(&e, "bot-a", "tool.shell_exec")), "the named principal is denied");
    assert_eq!(ask(&e, "bot-b", "tool.shell_exec"), GovernanceDecision::Permit, "another principal is not");
    assert_eq!(ask(&e, "tapestry", "tool.shell_exec"), GovernanceDecision::Permit);
    assert_eq!(ask(&e, "bot-a2", "tool.shell_exec"), GovernanceDecision::Permit, "exact, not a prefix");
    assert_eq!(ask(&e, "bot-a", "tool.read_file"), GovernanceDecision::Permit, "other actions unaffected");
}

#[test]
fn a_trailing_star_scopes_a_family_of_principals() {
    let e = engine_of(vec![rule_json("no-shell-bots", "tool.shell_exec", Some("bot-*"))], 0.7, false);
    assert!(is_deny(&ask(&e, "bot-a", "tool.shell_exec")));
    assert!(is_deny(&ask(&e, "bot-7@node.1", "tool.shell_exec")));
    assert_eq!(ask(&e, "tapestry", "tool.shell_exec"), GovernanceDecision::Permit);
}

#[test]
fn an_unscoped_rule_still_applies_to_every_principal() {
    let e = engine_of(vec![rule_json("no-shell", "tool.shell_exec", None)], 0.7, false);
    for who in ["bot-a", "bot-b", "tapestry", "x"] {
        assert!(is_deny(&ask(&e, who, "tool.shell_exec")), "{who}");
    }
}

#[test]
fn the_existing_rule_set_gives_the_same_verdicts_for_every_principal() {
    // Constructor rules, an old-shape JSON rule and a selector-only rule:
    // none carries a principal, so no principal may change a verdict.
    let legacy: GovernanceRule = serde_json::from_str(
        r#"{"id":"old","description":"d","branch":"Legislative","severity":"Blocking","tool_selector":"net_*","force_on_match":true}"#,
    )
    .unwrap();
    let rules = vec![
        GovernanceRule::spawn_requires_approval(),
        GovernanceRule::browser_policy("BP-1", "browser"),
        GovernanceRule::binding_thread("bt"),
        legacy,
        rule_json("wl", "workload.*", None),
    ];
    let e = engine_of(rules, 0.7, true);
    for action in [
        "tool.agent_spawn",
        "tool.shell_exec",
        "tool.net_fetch",
        "workload.place",
        "browser.navigate",
        "soul.binding_thread_intact",
        "tool.read_file",
    ] {
        let want = ask(&e, "reference-principal", action);
        for who in ["tapestry", "bot-a", "mesh:7@n.1", "kernel"] {
            assert_eq!(ask(&e, who, action), want, "{who} on {action}");
        }
    }
}

#[test]
fn a_rule_without_the_selector_serialises_exactly_as_before() {
    let r = rule_json("plain", "tool.x", None);
    let v = serde_json::to_value(&r).unwrap();
    assert!(v.get("principal_selector").is_none(), "no new key on an unscoped rule: {v}");
    let s = rule_json("scoped", "tool.x", Some("bot-a"));
    assert_eq!(serde_json::to_value(&s).unwrap()["principal_selector"], "bot-a");
}

#[test]
fn an_unscoped_overlay_and_its_effective_rules_hash_as_before() {
    // Pinned from the code before the selector existed (xpc run at the
    // tests-first commit): adding the field must not move any stored hash.
    let o = ov(r#"
schema = 1
[[deny]]
id = "project.no-shell"
actions = ["tool.shell_exec"]
[[require_approval]]
actions = ["cron.add*"]
"#);
    let eff = merge(&base_parent(), &o).unwrap();
    let hex = |b: &[u8; 32]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
    assert_eq!(
        (hex(&o.hash), hex(&eff.effective_hash)),
        ("778f08919a9d04f45b8d03d371526f1261ad27c3a971700474000f286d0aefef".to_owned(), "3b6d167e08f317ac96bb7f3f0700e2219958c6f2cab12f39c5f73011349ace94".to_owned())
    );
}

// ── overlay schema ───────────────────────────────────────────────────────

const SCOPED: &str = r#"
schema = 1
[[deny]]
id = "project.bot-no-shell"
actions = ["tool.shell_exec"]
principal = "bot-a"
reason = "bot-a never runs shell"
[[require_approval]]
id = "project.bots-ask"
actions = ["tool.write"]
principal = "bot-*"
"#;

#[test]
fn the_overlay_round_trips_the_selector_and_the_merge_scopes_the_rules() {
    let o = ov(SCOPED);
    let field = |v: &dyn erased::Ser, k: &str| v.json()[k].as_str().map(str::to_owned);
    assert_eq!(field(&o.file.deny[0], "principal").as_deref(), Some("bot-a"));
    assert_eq!(field(&o.file.require_approval[0], "principal").as_deref(), Some("bot-*"));
    let back: clawft_types::config::overlay::OverlayFile =
        serde_json::from_str(&serde_json::to_string(&o.file).unwrap()).unwrap();
    assert_eq!(back, o.file, "serialise then parse keeps the selector");

    let eff = merge(&base_parent(), &o).unwrap();
    let scoped: Vec<(String, Option<String>)> = eff
        .rules
        .iter()
        .filter(|r| r.id.starts_with("project."))
        .map(|r| (r.id.clone(), field(r, "principal_selector")))
        .collect();
    assert_eq!(
        scoped,
        [
            ("project.bot-no-shell".to_owned(), Some("bot-a".to_owned())),
            ("project.bots-ask".to_owned(), Some("bot-*".to_owned()))
        ]
    );

    // The merged rules decide per principal.
    let e = engine_of(eff.rules.clone(), 0.8, false);
    assert!(is_deny(&ask(&e, "bot-a", "tool.shell_exec")));
    assert_eq!(ask(&e, "tapestry", "tool.shell_exec"), GovernanceDecision::Permit);
    assert!(matches!(ask(&e, "bot-z", "tool.write"), GovernanceDecision::EscalateToHuman(_)));
    assert_eq!(ask(&e, "tapestry", "tool.write"), GovernanceDecision::Permit);
}

#[test]
fn the_selector_is_part_of_the_overlay_and_effective_hashes() {
    let a = ov(SCOPED);
    let b = ov(&SCOPED.replace("principal = \"bot-a\"", "principal = \"bot-b\""));
    assert_ne!(a.hash, b.hash);
    let ea = merge(&base_parent(), &a).unwrap();
    let eb = merge(&base_parent(), &b).unwrap();
    assert_ne!(ea.effective_hash, eb.effective_hash);
}

fn refused(toml: &str) -> String {
    let o = Overlay::from_toml(toml).map_err(|e| e.to_string());
    match o {
        Err(e) => e,
        Ok(o) => match merge(&base_parent(), &o) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("overlay should be refused: {toml}"),
        },
    }
}

#[test]
fn an_invalid_selector_is_refused_at_load_with_a_clear_error() {
    let too_long = "x".repeat(129);
    for bad in ["", "   ", "bot a", "a*b", "**", "bot/a", "bot-a\n", too_long.as_str()] {
        let toml = format!(
            "schema = 1\n[[deny]]\nid = \"d\"\nactions = [\"tool.x\"]\nprincipal = {}\n",
            serde_json::to_string(bad).unwrap()
        );
        let msg = refused(&toml);
        assert!(msg.contains("malformed principal selector"), "{bad:?}: {msg}");
        assert!(msg.contains("deny[0].principal"), "names the key: {msg}");
    }
    let msg = refused("schema = 1\n[[require_approval]]\nactions = [\"tool.x\"]\nprincipal = \"a*b\"\n");
    assert!(msg.contains("malformed principal selector") && msg.contains("require_approval[0].principal"), "{msg}");
}

#[test]
fn a_parent_policy_rule_with_an_invalid_selector_does_not_verify() {
    use ed25519_dalek::SigningKey;
    let key = SigningKey::from_bytes(&[7u8; 32]);
    let pubkey = key.verifying_key().to_bytes();
    let ok = parent_with(vec![rule_json("p", "tool.x", Some("bot-a"))], Limits::default(), 1);
    verify_parent_policy(&ok, &pubkey).expect("a valid selector verifies");
    let bad = parent_with(vec![rule_json("p", "tool.x", Some("a*b"))], Limits::default(), 1);
    let err = verify_parent_policy(&bad, &pubkey).expect_err("invalid selector refused");
    assert!(err.to_string().contains("principal selector"), "{err}");
}

#[test]
fn an_unreadable_selector_on_a_live_rule_applies_to_everyone_never_to_no_one() {
    // A rule that reached the engine with a selector it cannot interpret
    // (wire, chain) must not silently stop denying.
    let e = engine_of(vec![rule_json("odd", "tool.x", Some("a*b"))], 0.7, false);
    assert!(is_deny(&ask(&e, "anyone", "tool.x")));
}
