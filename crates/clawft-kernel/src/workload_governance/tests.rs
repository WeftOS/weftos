//! Acceptance tests for workload governance (ADR-099 s4, s7).

use std::sync::Arc;

use serde_json::{Value, json};

use super::*;
use crate::chain::{ChainEvent, ChainManager};
use crate::gate::{GateBackend, GovernanceGate};
use crate::revocation::{RevocationKind, RevocationList};
use crate::rule_distribution::RuleDistribution;

const KEY: &str = "abababababababababababababababababababababababababababababababab";
const HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// A well-behaved cog request: pinned signer, paired node, LAN, native.
fn cog_ctx() -> Value {
    json!({"workload": {
        "kind": "cog",
        "package_trust": "pinned_signer",
        "node_tier": "paired",
        "network": "lan",
        "secrets": false,
        "emulated": false,
        "resource_cost": 0.2,
        "package_id": "cog.fall-detect",
        "signer_keys": [KEY],
        "artifact_hashes": [HASH],
    }})
}

fn with(mut ctx: Value, key: &str, v: Value) -> Value {
    ctx["workload"][key] = v;
    ctx
}

/// A list that is never written (nothing in these tests revokes through it).
fn unused_list() -> Arc<RevocationList> {
    Arc::new(RevocationList::new(std::path::PathBuf::from("unused/revoked_hosts.json")))
}

fn chain() -> Arc<ChainManager> {
    Arc::new(ChainManager::new(0, 1000))
}

fn workload_events(cm: &ChainManager) -> Vec<ChainEvent> {
    cm.tail(0).into_iter().filter(|e| e.source == "workload").collect()
}

fn last_payload(cm: &ChainManager) -> Value {
    workload_events(cm).last().and_then(|e| e.payload.clone()).expect("workload event")
}

fn cog_place_permit() -> WorkloadPermitRule {
    WorkloadPermitRule::new("permit-cog-place", ["workload.place"], ["cog"])
}

#[test]
fn default_rules_deny_and_chain_every_governed_action() {
    let cm = chain();
    let gate = WorkloadGate::exempt(0.8, false, "test").with_chain(cm.clone());
    for action in GOVERNED_ACTIONS {
        let d = gate.check("agent-1", action, &cog_ctx());
        assert!(d.is_deny(), "{action} should be denied by default");
        let ev = workload_events(&cm).pop().unwrap();
        assert_eq!(ev.kind, *action, "chain kind mirrors the action");
        let p = ev.payload.unwrap();
        assert_eq!(p["decision"], "deny");
        assert_eq!(p["action"], *action);
        assert!(p["reason"].as_str().unwrap().contains("default deny"));
        assert_eq!(p["evaluated_rules"], json!([DEFAULT_DENY_RULE_ID]));
        assert_eq!(p["workload"]["kind"], "cog");
    }
    assert_eq!(workload_events(&cm).len(), GOVERNED_ACTIONS.len());
}

#[test]
fn unknown_workload_action_is_refused_and_chained() {
    let cm = chain();
    let gate = WorkloadGate::exempt(0.8, false, "test")
        .with_chain(cm.clone())
        .with_permit(WorkloadPermitRule::new("all", ["workload.*"], ["*"]))
        .unwrap();
    assert!(gate.check("a", "workload.teleport", &cog_ctx()).is_deny());
    let ev = workload_events(&cm).pop().unwrap();
    assert_eq!(ev.kind, "workload.refuse");
    assert_eq!(ev.payload.unwrap()["action"], "workload.teleport");
}

#[test]
fn non_workload_action_is_denied_and_not_chained() {
    let cm = chain();
    let gate = WorkloadGate::exempt(0.8, false, "test").with_chain(cm.clone());
    assert!(gate.check("a", "tool.exec", &cog_ctx()).is_deny());
    assert!(workload_events(&cm).is_empty());
}

#[test]
fn permit_rule_allows_and_chains() {
    let cm = chain();
    let gate = WorkloadGate::exempt(0.8, false, "test")
        .with_chain(cm.clone())
        .with_permit(cog_place_permit())
        .unwrap();
    assert!(gate.check("agent-1", "workload.place", &cog_ctx()).is_permit());
    let ev = workload_events(&cm).pop().unwrap();
    assert_eq!(ev.kind, "workload.place");
    let p = ev.payload.unwrap();
    assert_eq!(p["decision"], "permit");
    assert_eq!(p["permit_rule"], "permit-cog-place");
    assert_eq!(p["threshold_exceeded"], false);
    assert!(p["effect"]["risk"].as_f64().unwrap() > 0.0);
    assert!(!p["evaluated_rules"].as_array().unwrap().iter().any(|r| r == DEFAULT_DENY_RULE_ID));

    // The permit is scoped: another action and another kind stay denied.
    assert!(gate.check("agent-1", "workload.start", &cog_ctx()).is_deny());
    let inference = with(cog_ctx(), "kind", json!("inference"));
    assert!(gate.check("agent-1", "workload.place", &inference).is_deny());
    assert_eq!(last_payload(&cm)["decision"], "deny");
}

#[test]
fn permit_rule_conditions_are_all_enforced() {
    let gate = WorkloadGate::exempt(0.8, false, "test").with_permit(cog_place_permit()).unwrap();
    let cases = [
        ("package_trust", json!("signed_unpinned")),
        ("node_tier", json!("discovered")),
        ("network", json!("egress")),
        ("emulated", json!(true)),
        ("accelerator", json!("accel.gpu.metal")),
    ];
    for (field, value) in cases {
        let ctx = with(cog_ctx(), field, value.clone());
        assert!(gate.check("a", "workload.place", &ctx).is_deny(), "{field}={value}");
    }
    let mut cost_cap = cog_place_permit();
    cost_cap.max_resource_cost = 0.1;
    let capped = WorkloadGate::exempt(0.8, false, "test").with_permit(cost_cap).unwrap();
    assert!(capped.check("a", "workload.place", &cog_ctx()).is_deny());

    // Opt-ins lift the matching condition only.
    let mut opt = cog_place_permit();
    opt.allow_emulated = true;
    opt.accelerators = vec!["accel.gpu.*".into()];
    let gate = WorkloadGate::exempt(0.8, false, "test").with_permit(opt).unwrap();
    let emulated = with(cog_ctx(), "emulated", json!(true));
    assert!(gate.check("a", "workload.place", &emulated).is_permit());
    let metal = with(cog_ctx(), "accelerator", json!("accel.gpu.metal"));
    assert!(gate.check("a", "workload.place", &metal).is_permit());
    let npu = with(cog_ctx(), "accelerator", json!("accel.npu.ane"));
    assert!(gate.check("a", "workload.place", &npu).is_deny());
}

/// A permit rule that allows everything still cannot lift the ceiling.
fn permissive() -> WorkloadPermitRule {
    let mut p = WorkloadPermitRule::new("permissive", ["workload.*"], ["*"]);
    p.min_package_trust = PackageTrust::Unsigned;
    p.principals = vec!["a".into()];
    p.min_node_tier = NodeTrustTier::Discovered;
    p.max_network = NetworkPolicy::Egress;
    p
}

fn risky_ctx() -> Value {
    let ctx = with(cog_ctx(), "package_trust", json!("unsigned"));
    let ctx = with(ctx, "node_tier", json!("discovered"));
    with(ctx, "network", json!("egress"))
}

#[test]
fn effect_ceiling_denies_high_effect_even_with_permit() {
    let cm = chain();
    let gate = WorkloadGate::exempt(0.8, false, "test")
        .with_chain(cm.clone())
        .with_permit(permissive())
        .unwrap();
    assert!(gate.check("a", "workload.install", &cog_ctx()).is_permit());
    // Caller-supplied `effect` is ignored; the vector is derived.
    let mut ctx = risky_ctx();
    ctx["effect"] = json!({"risk": 0.0, "security": 0.0});
    assert!(gate.check("a", "workload.install", &ctx).is_deny());
    let p = last_payload(&cm);
    assert_eq!(p["threshold_exceeded"], true);
    assert_eq!(p["permit_rule"], "permissive");
    assert!(p["evaluated_rules"].as_array().unwrap().iter().any(|r| r == EFFECT_CEILING_RULE_ID));
}

#[test]
fn human_approval_turns_ceiling_deny_into_defer() {
    let cm = chain();
    let gate = WorkloadGate::exempt(0.8, true, "test")
        .with_chain(cm.clone())
        .with_permit(permissive())
        .unwrap();
    let d = gate.check("a", "workload.install", &risky_ctx());
    assert!(matches!(d, crate::gate::GateDecision::Defer { .. }));
    assert_eq!(last_payload(&cm)["decision"], "defer");
}

#[test]
fn secrets_require_pinned_node_even_when_permitted() {
    let mut p = cog_place_permit();
    p.allow_secrets = true;
    let gate = WorkloadGate::exempt(0.8, false, "test").with_permit(p).unwrap();
    let secret = with(cog_ctx(), "secrets", json!(true));
    assert!(gate.check("a", "workload.place", &secret).is_deny());
    let pinned = with(secret, "node_tier", json!("pinned"));
    assert!(gate.check("a", "workload.place", &pinned).is_permit());
}

#[test]
fn invalid_context_is_denied_and_chained() {
    let cm = chain();
    let gate = WorkloadGate::exempt(0.8, false, "test")
        .with_chain(cm.clone())
        .with_permit(permissive())
        .unwrap();
    let bad = [
        json!({}),
        json!({"workload": "cog"}),
        with(cog_ctx(), "kind", json!("Cog!")),
        with(cog_ctx(), "resource_cost", json!(2.0)),
        with(cog_ctx(), "package_trust", json!("trusted")),
        with(cog_ctx(), "accelerator", json!("GPU")),
        with(cog_ctx(), "signer_keys", json!(["not-hex"])),
    ];
    for ctx in &bad {
        assert!(gate.check("a", "workload.place", ctx).is_deny(), "{ctx}");
        let p = last_payload(&cm);
        assert_eq!(p["decision"], "deny");
        assert!(p["reason"].as_str().unwrap().contains("invalid workload context"), "{p}");
    }
    assert_eq!(workload_events(&cm).len(), bad.len());
}

#[test]
fn revoked_package_signer_or_artifact_is_denied_and_chained() {
    let dir = tempfile::tempdir().unwrap();
    let list = Arc::new(RevocationList::new(dir.path().join("revoked_hosts.json")));
    let cm = chain();
    let gate = WorkloadGate::exempt(0.8, false, "test")
        .with_chain(cm.clone())
        .with_revocations(list.clone())
        .with_permit(cog_place_permit())
        .unwrap();
    assert!(gate.check("a", "workload.place", &cog_ctx()).is_permit());

    for (kind, id) in [
        (RevocationKind::Package, "cog.fall-detect"),
        (RevocationKind::SignerKey, KEY),
        (RevocationKind::ArtifactHash, HASH),
    ] {
        assert!(revoke_and_record(&list, Some(&cm), kind, id, "test", "operator").unwrap());
        let rev = workload_events(&cm).pop().unwrap();
        assert_eq!(rev.kind, "workload.revoke");
        assert_eq!(rev.payload.as_ref().unwrap()["subject_id"], id);

        assert!(gate.check("a", "workload.place", &cog_ctx()).is_deny(), "{kind}");
        let p = last_payload(&cm);
        assert_eq!(p["revoked"]["kind"], json!(kind));
        list.unrevoke_subject(kind, id).unwrap();
    }
    // A repeat revocation is not re-chained.
    let before = workload_events(&cm).len();
    revoke_and_record(&list, Some(&cm), RevocationKind::Package, "x.y", "r", "op").unwrap();
    assert!(!revoke_and_record(&list, Some(&cm), RevocationKind::Package, "x.y", "r", "op").unwrap());
    assert_eq!(workload_events(&cm).len(), before + 1);
}

#[test]
fn unreadable_revocation_list_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(crate::revocation::SUBJECTS_FILE_NAME), "[{").unwrap();
    let list = Arc::new(RevocationList::load(dir.path().join("revoked_hosts.json")));
    let gate = WorkloadGate::exempt(0.8, false, "test")
        .with_revocations(list)
        .with_permit(cog_place_permit())
        .unwrap();
    assert!(gate.check("a", "workload.place", &cog_ctx()).is_deny());
}

#[test]
fn default_deny_rule_blocks_workload_in_plain_governance_gate() {
    let mut g = GovernanceGate::new(0.8, false);
    for r in default_rules() {
        g = g.add_rule(r);
    }
    let zero = json!({"effect": {"risk": 0.0}});
    assert!(g.check("a", "workload.place", &zero).is_deny());
    assert!(g.check("a", "tool.read_file", &zero).is_permit(), "scoped to workload.*");
}

#[test]
fn default_rules_distribute_to_peers_per_adr_092() {
    let mut origin = RuleDistribution::new("node-a");
    install_default_rules(&mut origin, 100);
    let mut peer = RuleDistribution::new("node-b");
    assert_eq!(peer.merge(&origin.gossip_full()), 2);

    let rules = peer.active_rules();
    assert!(rules.iter().any(|r| r.id == DEFAULT_DENY_RULE_ID));
    let mut g = GovernanceGate::new(0.8, false);
    for r in rules.clone() {
        g = g.add_rule(r);
    }
    assert!(g.check("a", "workload.install", &json!({})).is_deny());
    let wg = WorkloadGate::with_rules(0.8, false, rules, unused_list());
    assert!(wg.check("a", "workload.install", &cog_ctx()).is_deny());
}

#[test]
fn gate_fails_closed_without_default_deny_rule() {
    let gate = WorkloadGate::with_rules(0.8, false, Vec::new(), unused_list());
    assert!(gate.check("a", "workload.place", &cog_ctx()).is_deny());
    let gate = gate.with_permit(cog_place_permit()).unwrap();
    assert!(gate.check("a", "workload.place", &cog_ctx()).is_permit());
}

#[test]
fn permit_rules_are_validated() {
    let bad = [
        WorkloadPermitRule::new("", ["workload.place"], ["cog"]),
        WorkloadPermitRule::new("r", Vec::<String>::new(), ["cog"]),
        WorkloadPermitRule::new("r", ["workload.place"], Vec::<String>::new()),
        WorkloadPermitRule::new("r", ["*"], ["cog"]),
        WorkloadPermitRule::new("r", ["tool.*"], ["cog"]),
        WorkloadPermitRule::new("r", ["workload.teleport"], ["cog"]),
    ];
    for rule in bad {
        assert!(WorkloadGate::exempt(0.8, false, "test").with_permit(rule.clone()).is_err(), "{rule:?}");
    }
    let dup = WorkloadGate::exempt(0.8, false, "test")
        .with_permit(cog_place_permit())
        .unwrap()
        .with_permit(cog_place_permit());
    assert!(dup.is_err());
}

#[test]
fn scripted_permit_and_deny_events_survive_chain_save_and_load() {
    let dir = tempfile::tempdir().unwrap();
    let cm = chain();
    let gate = WorkloadGate::exempt(0.8, false, "test")
        .with_chain(cm.clone())
        .with_permit(cog_place_permit())
        .unwrap();
    gate.check("operator", "workload.place", &cog_ctx());
    gate.check("operator", "workload.start", &cog_ctx());
    let path = dir.path().join("chain.json");
    cm.save_to_file(&path).unwrap();

    let loaded = ChainManager::load_from_file(&path, 1000).unwrap();
    let got: Vec<(String, Value)> = workload_events(&loaded)
        .into_iter()
        .map(|e| (e.kind, e.payload.unwrap()["decision"].clone()))
        .collect();
    assert_eq!(
        got,
        vec![
            ("workload.place".to_owned(), json!("permit")),
            ("workload.start".to_owned(), json!("deny")),
        ]
    );
}

// ── ADR-103 A6 (Phase 2 package I): permits match the verified project ──

mod project_permits {
    use super::*;
    use crate::governance::{AttestSource, ProjectAttestation};

    const P1: &str = "01JB8Z3Q0V6X9KQ4M2N7T5R1WD";
    const P2: &str = "01JB8Z3Q0V6X9KQ4M2N7T5R1WE";

    fn gate_for(project: Option<&str>) -> WorkloadGate {
        let mut permit = cog_place_permit();
        permit.projects = vec![P1.into()];
        let g = WorkloadGate::exempt(0.8, false, "test").with_permit(permit).unwrap();
        match project {
            Some(p) => g.with_attestation(ProjectAttestation::from_verified(p, AttestSource::BoundKernel)),
            None => g,
        }
    }

    #[test]
    fn a_project_scoped_permit_matches_only_the_verified_project() {
        assert!(!gate_for(Some(P1)).check("a", "workload.place", &cog_ctx()).is_deny());
        assert!(gate_for(Some(P2)).check("a", "workload.place", &cog_ctx()).is_deny());
        assert!(gate_for(None).check("a", "workload.place", &cog_ctx()).is_deny());
    }

    #[test]
    fn a_project_named_in_the_request_context_is_not_a_source() {
        let mut ctx = cog_ctx();
        ctx["project_id"] = json!(P1);
        ctx["workload"]["project_id"] = json!(P1);
        assert!(gate_for(Some(P2)).check("a", "workload.place", &ctx).is_deny());
        assert!(gate_for(None).check("a", "workload.place", &ctx).is_deny());
    }

    #[test]
    fn permit_rules_without_projects_serialise_as_before() {
        let v = serde_json::to_value(cog_place_permit()).unwrap();
        assert!(v.get("projects").is_none());
        let mut bad = cog_place_permit();
        bad.projects = vec![String::new()];
        assert!(bad.validate().is_err());
    }
}

fn stop_permit() -> WorkloadPermitRule {
    let mut p = WorkloadPermitRule::new("permit-cog-stop", ["workload.stop"], ["cog"]);
    p.min_node_tier = NodeTrustTier::Paired;
    p
}

#[test]
fn teardown_waives_a_node_tier_denial_and_chains_the_waiver() {
    let cm = chain();
    let gate = WorkloadGate::exempt(0.8, false, "test")
        .with_chain(cm.clone())
        .with_permit(stop_permit())
        .unwrap();
    let demoted = with(cog_ctx(), "node_tier", json!("discovered"));
    assert!(gate.check("a", "workload.stop", &demoted).is_deny());
    assert!(
        gate.check_teardown("a", "workload.stop", &demoted)
            .is_permit()
    );
    assert_eq!(last_payload(&cm)["teardown_node_tier_waived"], true);
}

#[test]
fn teardown_waives_a_revocation_but_never_a_default_deny() {
    let dir = tempfile::tempdir().unwrap();
    let list = Arc::new(RevocationList::new(dir.path().join("revoked_hosts.json")));
    list.revoke_subject(RevocationKind::Package, "cog.fall-detect", "bad")
        .unwrap();
    let cm = chain();
    let gate = WorkloadGate::exempt(0.8, false, "test")
        .with_chain(cm.clone())
        .with_permit(stop_permit())
        .unwrap()
        .with_revocations(list);
    let demoted = with(cog_ctx(), "node_tier", json!("discovered"));
    let paired = cog_ctx();
    // The gate still denies a plain check on a revoked package ...
    assert!(gate.check("a", "workload.stop", &paired).is_deny());
    // ... but taking it down is allowed, on a paired node and a demoted one:
    // a revoked package must stay stoppable. The waiver is chained.
    assert!(gate.check_teardown("a", "workload.stop", &paired).is_permit());
    let p = last_payload(&cm);
    assert_eq!(p["teardown_revocation_waived"]["id"], "cog.fall-detect");
    assert_eq!(p["teardown_node_tier_waived"], false);
    assert!(gate.check_teardown("a", "workload.stop", &demoted).is_permit());
    let p = last_payload(&cm);
    assert_eq!(p["teardown_revocation_waived"]["id"], "cog.fall-detect");
    assert_eq!(p["teardown_node_tier_waived"], true);
    // No permit for the action at all: a default deny stays a deny.
    let bare = WorkloadGate::exempt(0.8, false, "test");
    assert!(
        bare.check_teardown("a", "workload.stop", &demoted)
            .is_deny()
    );
    // A permit for another kind of workload does not cover this one either.
    let other = WorkloadGate::exempt(0.8, false, "test")
        .with_permit(WorkloadPermitRule::new(
            "other",
            ["workload.stop"],
            ["inference"],
        ))
        .unwrap();
    assert!(
        other
            .check_teardown("a", "workload.stop", &demoted)
            .is_deny()
    );
}

#[test]
fn check_teardown_refuses_any_action_that_is_not_a_teardown() {
    // The guard itself: start, place and install are not teardown actions.
    for a in ["workload.start", "workload.place", "workload.install", "workload.migrate"] {
        assert!(WorkloadGate::not_teardown(a).unwrap().is_deny(), "{a}");
    }
    for a in ["workload.stop", "workload.unload", "workload.load"] {
        assert!(WorkloadGate::not_teardown(a).is_none(), "{a}");
    }
    // Through the trait method: denied in release builds; in debug builds
    // the debug_assert fires first.
    let r = std::panic::catch_unwind(|| {
        let g = WorkloadGate::exempt(0.8, false, "test")
            .with_permit(WorkloadPermitRule::new("all", ["workload.*"], ["cog"]))
            .unwrap();
        g.check_teardown("a", "workload.start", &cog_ctx())
    });
    match r {
        Ok(d) => assert!(d.is_deny()),
        // Only a debug build may panic here (a debug_assert in the check path).
        Err(_) => {
            if !cfg!(debug_assertions) {
                panic!("a release build must deny, not panic");
            }
        }
    }
}

// ── card 08f1bcff: revocation cannot be bypassed and is always chained ──

fn place_gate(list: &Arc<RevocationList>, cm: &Arc<ChainManager>) -> WorkloadGate {
    WorkloadGate::exempt(0.8, false, "test")
        .with_chain(cm.clone())
        .with_revocations(list.clone())
        .with_permit(WorkloadPermitRule::new(
            "all-cog",
            ["workload.*"],
            ["cog"],
        ))
        .unwrap()
}

fn without_refs(mut ctx: Value) -> Value {
    for k in ["package_id", "signer_keys", "artifact_hashes"] {
        ctx["workload"].as_object_mut().unwrap().remove(k);
    }
    ctx
}

/// (a) A caller that omits the package, signer and artifact refs and claims
/// `pinned_signer` must not get past the revocation list.
#[test]
fn omitted_refs_cannot_bypass_revocation() {
    let dir = tempfile::tempdir().unwrap();
    let list = Arc::new(RevocationList::new(dir.path().join("revoked_hosts.json")));
    let cm = chain();
    let gate = place_gate(&list, &cm);
    list.revoke_subject(RevocationKind::Package, "cog.fall-detect", "bad").unwrap();
    list.revoke_subject(RevocationKind::SignerKey, KEY, "leaked").unwrap();

    // With refs the revoked package is denied, as before.
    assert!(gate.check("a", "workload.place", &cog_ctx()).is_deny());
    // Without any, the same request used to carry only its trust claim past
    // the list and be permitted. It is denied now, for every action that
    // carries a package, and the denial is chained with its reason.
    for action in ["workload.install", "workload.place", "workload.load", "workload.start"] {
        assert!(
            gate.check("a", action, &without_refs(cog_ctx())).is_deny(),
            "{action} with omitted refs must be denied"
        );
        let p = last_payload(&cm);
        assert_eq!(p["decision"], "deny");
        assert!(p["reason"].as_str().unwrap().contains("names no package"), "{p}");
    }
    // Empty lists and a null package id are the same as omitting them.
    let mut empty = cog_ctx();
    empty["workload"]["package_id"] = Value::Null;
    empty["workload"]["signer_keys"] = json!([]);
    empty["workload"]["artifact_hashes"] = json!([]);
    assert!(gate.check("a", "workload.place", &empty).is_deny());

    // The check is not a blanket deny: a request that names an unrevoked
    // package is permitted, and a teardown (which names the instance, not a
    // package) is not held to it.
    let mut other = cog_ctx();
    other["workload"]["package_id"] = json!("cog.other");
    other["workload"]["signer_keys"] = json!([]);
    other["workload"]["artifact_hashes"] = json!([HASH]);
    assert!(gate.check("a", "workload.place", &other).is_permit());
    assert!(
        gate.check_teardown("a", "workload.stop", &without_refs(cog_ctx()))
            .is_permit()
    );
}

/// (b) A failed write of the list must not lose the audit event.
#[test]
fn the_revocation_event_survives_a_persistence_failure() {
    let dir = tempfile::tempdir().unwrap();
    // A file where the list's directory should be: every save fails.
    std::fs::write(dir.path().join("blocker"), "x").unwrap();
    let bad = dir.path().join("blocker").join("revoked_hosts.json");
    let list = Arc::new(RevocationList::new(bad));
    let cm = chain();
    let gate = place_gate(&list, &cm);
    assert!(gate.check("a", "workload.place", &cog_ctx()).is_permit());

    let err = revoke_and_record(&list, Some(&cm), RevocationKind::Package, "cog.fall-detect", "bad", "operator")
        .unwrap_err();
    assert!(matches!(err, crate::revocation::RevocationError::Persist(_)), "{err}");
    let ev: Vec<_> = workload_events(&cm).into_iter().filter(|e| e.kind == "workload.revoke").collect();
    assert_eq!(ev.len(), 1, "the event was chained although the write failed");
    let p = ev[0].payload.as_ref().unwrap();
    assert_eq!(p["subject_id"], "cog.fall-detect");
    assert_eq!(p["revoked_by"], "operator");
    assert_eq!(p["persisted"], false);
    assert!(p["persist_error"].is_string());
    // And the revocation is in force (fail closed), not dropped with the write.
    assert!(gate.check("a", "workload.place", &cog_ctx()).is_deny());
    // Asking again does not chain it a second time.
    assert!(!revoke_and_record(&list, Some(&cm), RevocationKind::Package, "cog.fall-detect", "bad", "operator").unwrap());
    assert_eq!(workload_events(&cm).iter().filter(|e| e.kind == "workload.revoke").count(), 1);

    // The same holds for a caller that never passed a chain (a mesh notice,
    // kernel code): the list's own sink records it.
    let dir2 = tempfile::tempdir().unwrap();
    std::fs::write(dir2.path().join("blocker"), "x").unwrap();
    let list2 = RevocationList::new(dir2.path().join("blocker").join("r.json"));
    let cm2 = chain();
    assert!(chain_revocations(&list2, cm2.clone()));
    assert!(list2.revoke_subject(RevocationKind::SignerKey, KEY, "leaked").is_err());
    let ev = workload_events(&cm2);
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0].kind, "workload.revoke");
    assert_eq!(ev[0].payload.as_ref().unwrap()["persisted"], false);
}

/// (c) Lifting a revocation is chained, whoever does it.
#[test]
fn unrevoke_is_chained() {
    let dir = tempfile::tempdir().unwrap();
    let list = Arc::new(RevocationList::new(dir.path().join("revoked_hosts.json")));
    let cm = chain();
    let gate = place_gate(&list, &cm);
    revoke_and_record(&list, Some(&cm), RevocationKind::Package, "cog.fall-detect", "bad", "operator").unwrap();
    assert!(gate.check("a", "workload.place", &cog_ctx()).is_deny());

    assert!(unrevoke_and_record(&list, Some(&cm), RevocationKind::Package, "cog.fall-detect", "operator").unwrap());
    let ev = workload_events(&cm).pop().unwrap();
    assert_eq!(ev.kind, "workload.unrevoke");
    assert_eq!(ev.kind, crate::chain::EVENT_KIND_WORKLOAD_UNREVOKE);
    let p = ev.payload.unwrap();
    assert_eq!(p["subject_kind"], json!(RevocationKind::Package));
    assert_eq!(p["subject_id"], "cog.fall-detect");
    assert_eq!(p["unrevoked_by"], "operator");
    assert_eq!(p["persisted"], true);
    assert!(gate.check("a", "workload.place", &cog_ctx()).is_permit());

    // Nothing to lift: nothing chained.
    let before = workload_events(&cm).len();
    assert!(!unrevoke_and_record(&list, Some(&cm), RevocationKind::Package, "cog.fall-detect", "operator").unwrap());
    assert_eq!(workload_events(&cm).len(), before);

    // A caller with no chain of its own (the list's sink records it).
    let sink_cm = chain();
    assert!(chain_revocations(&list, sink_cm.clone()));
    list.revoke_subject(RevocationKind::ArtifactHash, HASH, "bad").unwrap();
    assert!(list.unrevoke_subject_by(RevocationKind::ArtifactHash, HASH, "mesh:abcd").unwrap());
    let kinds: Vec<_> = workload_events(&sink_cm).into_iter().map(|e| e.kind).collect();
    assert_eq!(kinds, ["workload.revoke", "workload.unrevoke"]);
    // The chain constants and the list's audit kinds are one vocabulary.
    assert_eq!(crate::revocation::AUDIT_REVOKE_KIND, crate::chain::EVENT_KIND_WORKLOAD_REVOKE);
    assert_eq!(crate::revocation::AUDIT_UNREVOKE_KIND, crate::chain::EVENT_KIND_WORKLOAD_UNREVOKE);
}

// ── review round: generation, hex ids, unsigned permits need a principal ──

#[test]
fn the_list_generation_moves_on_every_change_and_only_then() {
    let dir = tempfile::tempdir().unwrap();
    let list = RevocationList::new(dir.path().join("revoked_hosts.json"));
    let g0 = list.generation();
    assert!(list.revoke_subject(RevocationKind::Package, "cog.a", "r").unwrap());
    let g1 = list.generation();
    assert!(g1 > g0);
    assert!(!list.revoke_subject(RevocationKind::Package, "cog.a", "r").unwrap());
    assert_eq!(list.generation(), g1, "a repeat changes nothing");
    assert!(list.unrevoke_subject(RevocationKind::Package, "cog.a").unwrap());
    assert!(list.generation() > g1);
}

#[test]
fn a_hex_shaped_package_id_is_case_insensitive_like_the_other_hex_ids() {
    let dir = tempfile::tempdir().unwrap();
    let list = RevocationList::new(dir.path().join("revoked_hosts.json"));
    let upper = "AB".repeat(32);
    list.revoke_subject(RevocationKind::Package, &upper, "r").unwrap();
    assert!(list.is_subject_revoked(RevocationKind::Package, &upper.to_lowercase()));
    assert!(list.is_subject_revoked(RevocationKind::Package, &upper));
    assert_eq!(list.list_subjects(None)[0].id, upper.to_lowercase());
    // Names that are not hex are untouched (case still matters there).
    list.revoke_subject(RevocationKind::Package, "Cog.A", "r").unwrap();
    assert!(!list.is_subject_revoked(RevocationKind::Package, "cog.a"));
}

#[test]
fn a_permit_that_accepts_unsigned_packages_must_name_its_principals() {
    let mut p = WorkloadPermitRule::new("catalog", ["workload.install"], ["cog"]);
    p.min_package_trust = PackageTrust::Unsigned;
    assert!(p.validate().unwrap_err().contains("principals"));
    assert!(WorkloadGate::exempt(0.8, false, "test").with_permit(p.clone()).is_err());
    p.principals = vec![CATALOG_PRINCIPAL.into()];
    assert!(p.validate().is_ok());
    // The catalog principal's permit does not admit another principal.
    let mut ctx = cog_ctx();
    ctx["workload"]["package_trust"] = json!("unsigned");
    let gate = WorkloadGate::exempt(2.0, false, "test").with_permit(p).unwrap();
    assert!(gate.check(CATALOG_PRINCIPAL, "workload.install", &ctx).is_permit());
    assert!(gate.check("kernel", "workload.install", &ctx).is_deny());
    // A signed floor needs no principal.
    assert!(WorkloadPermitRule::new("x", ["workload.place"], ["cog"]).validate().is_ok());
}

#[test]
fn a_gate_is_built_with_its_list_or_says_why_not() {
    let list = unused_list();
    assert!(WorkloadGate::new(0.8, false, list.clone()).exempt_reason().is_none());
    assert!(WorkloadGate::with_rules(0.8, false, Vec::new(), list).exempt_reason().is_none());
    assert_eq!(WorkloadGate::exempt(0.8, false, "why").exempt_reason(), Some("why"));
}

/// A permit written with defaults (max_network lan) never matches a native cog,
/// which the placement path classes `egress`. The refusal must say which
/// condition failed, and the rule must warn.
#[test]
fn default_permit_refuses_a_native_cog_and_says_why() {
    let permit = WorkloadPermitRule::new("COG-DEFAULTS", ["workload.place"], ["cog"]);
    let warnings = permit.warnings();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("egress"), "{warnings:?}");
    let gate = WorkloadGate::exempt(0.8, false, "test").with_permit(permit).unwrap();
    let native = with(cog_ctx(), "network", json!("egress"));
    match gate.check("a", "workload.place", &native) {
        crate::gate::GateDecision::Deny { reason, .. } => {
            assert!(reason.contains("COG-DEFAULTS"), "{reason}");
            assert!(reason.contains("network Egress exceeds max_network Lan"), "{reason}");
        }
        other => panic!("expected deny, got {other:?}"),
    }
    let mut ok = WorkloadPermitRule::new("COG-EGRESS", ["workload.place"], ["cog"]);
    ok.max_network = crate::workload_governance::NetworkPolicy::Egress;
    assert!(ok.warnings().is_empty());
}
