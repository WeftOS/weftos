//! Tests for a project kernel's live governance (ADR-103 D8, package E):
//! fail-closed boot, limits, reload, parent updates, rule hashes on the chain.

use std::sync::Arc;

use chrono::{Duration, Utc};
use clawft_types::config::KernelConfig;
use clawft_types::config::overlay::Limits;
use clawft_types::project::cert::{CertRequest, ProjectCert};
use clawft_types::project::canon::hex_encode;
use clawft_types::runtime_paths::RuntimePaths;
use ed25519_dalek::SigningKey;
use serde_json::json;

use crate::chain::{ChainEvent, ChainManager};
use crate::gate::{GateBackend, GateDecision, GovernanceGate};
use crate::governance::RuleSeverity;
use crate::governance_overlay::OverlayError;
use crate::governance_overlay_tests::{base_parent, parent_with, rule, user_key};
use crate::overlay_runtime::{OverlayRuntime, child_paths, prepare};
use crate::overlay_trust::VERSION_PIN_FILE;
use crate::parent_policy::{ParentPolicy, ParentPolicyError};

const ID: &str = "01JB8Z3Q0V6X9KQ4M2N7T5R1WD";

pub(crate) struct Fixture {
    pub _t: tempfile::TempDir,
    pub paths: RuntimePaths,
}

pub(crate) fn project_key() -> SigningKey {
    SigningKey::from_bytes(&[3u8; 32])
}

pub(crate) fn write_parent(paths: &RuntimePaths, p: &ParentPolicy) {
    std::fs::write(paths.parent_policy(), serde_json::to_vec_pretty(p).unwrap()).unwrap();
}

pub(crate) fn fixture(parent: &ParentPolicy, overlay: Option<&str>) -> Fixture {
    let t = tempfile::tempdir().unwrap();
    let run = t.path().join("run").join(ID);
    let root = t.path().join("proj");
    std::fs::create_dir_all(&run).unwrap();
    std::fs::create_dir_all(root.join(".weftos")).unwrap();
    let paths = RuntimePaths::child_at(&run, ID, &root).unwrap();
    let cert = ProjectCert::sign(
        &user_key(),
        &CertRequest {
            project_id: ID.into(),
            project_pubkey: project_key().verifying_key().to_bytes(),
            serial: 1,
            issued_at: Utc::now() - Duration::minutes(1),
            expires_at: None,
        },
    );
    std::fs::write(paths.project_cert().unwrap(), serde_json::to_vec(&cert).unwrap()).unwrap();
    // 0600, as `load_or_create_project_key` writes it (boot reads it strictly).
    crate::parent_policy::write_atomic_0600(&paths.project_key().unwrap(), &project_key().to_bytes()).unwrap();
    write_parent(&paths, parent);
    if let Some(o) = overlay {
        std::fs::write(paths.overlay().unwrap(), o).unwrap();
    }
    Fixture { _t: t, paths }
}

pub(crate) struct Running {
    pub cm: Arc<ChainManager>,
    pub gate: Arc<dyn GateBackend>,
    pub rt: Arc<OverlayRuntime>,
}

pub(crate) fn start(f: &Fixture) -> Running {
    let prepared = prepare(&f.paths).unwrap();
    let cm = Arc::new(ChainManager::new(0, 1000));
    prepared.commit(&cm).unwrap();
    prepared.install_provider(&cm);
    let (threshold, human) = prepared.engine_params();
    let mut gate = GovernanceGate::new(threshold, human)
        .with_chain(Arc::clone(&cm))
        .with_eval_rate_limit(crate::rate_limit::RateLimitConfig::unlimited());
    for r in prepared.rules().iter().cloned() {
        gate = gate.add_rule(r);
    }
    let (gate, rt) = prepared.into_runtime(gate, Arc::clone(&cm));
    Running { cm, gate, rt }
}

fn deny_toml(action: &str) -> String {
    format!("[[deny]]\nid = \"d1\"\nactions = [\"{action}\"]\nreason = \"no\"\n")
}

fn check(g: &Arc<dyn GateBackend>, action: &str) -> GateDecision {
    g.check("agent-1", action, &json!({}))
}

fn is_deny(d: &GateDecision) -> bool {
    matches!(d, GateDecision::Deny { .. })
}

fn events(cm: &ChainManager) -> Vec<ChainEvent> {
    cm.tail(0)
}

fn prepare_err(f: &Fixture) -> OverlayError {
    prepare(&f.paths).err().expect("boot must be refused")
}

// ── boot refuses, fail closed ────────────────────────────────────────────

#[test]
fn boot_prepares_and_applies_limits_only_downward() {
    let f = fixture(
        &base_parent(),
        Some("[limits]\nmax_processes = 32\nspawn_budget = 3\n"),
    );
    let p = prepare(&f.paths).unwrap();
    let mut kc = KernelConfig {
        max_processes: 128,
        ..KernelConfig::default()
    };
    p.apply_limits(&mut kc);
    assert_eq!(kc.max_processes, 32);
    let sub = &kc.agent.as_ref().unwrap().subagents;
    assert_eq!(sub.max_per_conv, 3);
    assert!(sub.enabled);

    // A config already below the limits is left alone.
    let mut small = KernelConfig {
        max_processes: 8,
        ..KernelConfig::default()
    };
    small.agent = Some(Default::default());
    small.agent.as_mut().unwrap().subagents.max_per_conv = 1;
    p.apply_limits(&mut small);
    assert_eq!(small.max_processes, 8);
    assert_eq!(small.agent.unwrap().subagents.max_per_conv, 1);
}

#[test]
fn a_zero_spawn_budget_disables_agent_spawning() {
    let f = fixture(&base_parent(), Some("[limits]\nspawn_budget = 0\n"));
    let mut kc = KernelConfig::default();
    prepare(&f.paths).unwrap().apply_limits(&mut kc);
    let sub = kc.agent.unwrap().subagents;
    assert_eq!((sub.max_per_conv, sub.enabled), (0, false));
}

#[test]
fn a_broken_overlay_refuses_boot_and_names_the_key() {
    let cases: &[(&str, &str, &str)] = &[
        ("permit", "permit = [\"x.y\"]\n", "permit"),
        ("deactivate", "deactivate = [\"GOV-001\"]\n", "deactivate"),
        ("raised limit", "[limits]\nmax_processes = 999\n", "limits.max_processes"),
        ("shadow", "[[deny]]\nid = \"gov-001\"\nactions = [\"a.b\"]\n", "deny[0].id"),
        ("not toml", "[[deny\nid =", "overlay.toml"),
        ("wrong schema", "schema = 2\n", "schema"),
    ];
    for (name, toml, key) in cases {
        let f = fixture(&base_parent(), Some(toml));
        let e = prepare_err(&f);
        assert_eq!(&e.key(), key, "{name}: {e}");
        assert!(e.boot_message().contains("fewer rules than its parent"), "{name}");
    }
}

#[test]
fn an_unreadable_overlay_is_an_error_not_an_empty_overlay() {
    let f = fixture(&base_parent(), None);
    // A directory where the file belongs.
    std::fs::create_dir(f.paths.overlay().unwrap()).unwrap();
    assert!(matches!(prepare_err(&f), OverlayError::Io { .. }));
}

#[test]
fn a_missing_overlay_is_an_empty_one() {
    let f = fixture(&base_parent(), None);
    assert!(prepare(&f.paths).is_ok());
}

#[test]
fn a_tampered_parent_policy_refuses_boot() {
    let f = fixture(&base_parent(), None);
    let text = std::fs::read_to_string(f.paths.parent_policy()).unwrap();
    // Switch the blocking rule off in the file.
    let off = text.replacen("\"active\": true", "\"active\": false", 1);
    assert_ne!(off, text);
    std::fs::write(f.paths.parent_policy(), off).unwrap();
    let e = prepare_err(&f);
    assert!(matches!(e, OverlayError::Parent(ParentPolicyError::RuleHash)), "{e}");

    // A missing parent policy is also a refusal.
    std::fs::remove_file(f.paths.parent_policy()).unwrap();
    assert!(matches!(prepare_err(&f), OverlayError::Parent(ParentPolicyError::File(_))));

    // A policy signed by some other key is refused.
    let other = SigningKey::from_bytes(&[9u8; 32]);
    let forged = crate::parent_policy::export_rules(
        vec![],
        0.9,
        false,
        &Limits::default(),
        &other,
        1,
        Utc::now(),
    )
    .unwrap();
    write_parent(&f.paths, &forged);
    assert!(matches!(prepare_err(&f), OverlayError::Parent(ParentPolicyError::UserKey)));
}

#[test]
fn only_a_child_root_can_run_the_project_profile() {
    let t = tempfile::tempdir().unwrap();
    let e = child_paths(&RuntimePaths::at(t.path())).unwrap_err();
    assert_eq!(e, OverlayError::NotAChild);
    assert_eq!(e.key(), "kernel.profile");
}

#[test]
fn a_certificate_that_does_not_match_the_project_refuses_boot() {
    let f = fixture(&base_parent(), None);
    // The key beside the certificate is not the certified one.
    std::fs::write(f.paths.project_key().unwrap(), [4u8; 32]).unwrap();
    assert!(matches!(prepare_err(&f), OverlayError::Cert(_)));
    std::fs::write(f.paths.project_key().unwrap(), project_key().to_bytes()).unwrap();
    prepare(&f.paths).unwrap();

    // No certificate at all.
    std::fs::remove_file(f.paths.project_cert().unwrap()).unwrap();
    assert!(matches!(prepare_err(&f), OverlayError::Cert(_)));
}

// ── rule hash on the chain ───────────────────────────────────────────────

#[test]
fn every_event_after_boot_carries_the_effective_hash_and_it_changes_on_update() {
    let f = fixture(&base_parent(), Some(&deny_toml("tool.shell_exec")));
    let r = start(&f);
    let h1 = r.rt.rule_hash().unwrap();
    assert_eq!(hex_encode(&h1), r.rt.applied().effective_hash);

    r.cm.append("test", "after.boot", None);
    check(&r.gate, "tool.read_file");
    let evs = events(&r.cm);
    assert!(evs.len() > 3);
    for e in evs.iter().filter(|e| e.sequence > 0) {
        assert_eq!(e.rule_hash, Some(h1), "event {} {}", e.sequence, e.kind);
    }
    assert!(evs.iter().any(|e| e.kind == "governance.overlay.applied"));
    let boot_event = evs.iter().find(|e| e.kind == "governance.overlay.applied").unwrap();
    assert_eq!(boot_event.payload.as_ref().unwrap()["source"], "boot");

    // Push a changed parent policy (one more parent rule).
    let mut rules = base_parent().rules;
    rules.push(rule("GOV-NEW", RuleSeverity::Blocking, Some("net.*"), true, true));
    let v2 = parent_with(rules, base_parent().limits, 2);
    let applied = r.rt.apply_parent_update(v2).unwrap();
    let h2 = r.rt.rule_hash().unwrap();
    assert_ne!(h1, h2);
    assert_eq!(applied.effective_hash, hex_encode(&h2));
    assert_eq!(applied.parent_version, 2);

    r.cm.append("test", "after.update", None);
    let evs = events(&r.cm);
    let upd = evs
        .iter()
        .rfind(|e| e.kind == "governance.overlay.applied")
        .unwrap();
    assert_eq!(upd.rule_hash, Some(h2), "the applied event is the first under the new rules");
    let p = upd.payload.as_ref().unwrap();
    assert_eq!(p["source"], "parent.update");
    for k in ["parent_hash", "overlay_hash", "effective_hash"] {
        assert_eq!(p[k].as_str().unwrap().len(), 64, "{k}");
    }
    assert_eq!(evs.last().unwrap().rule_hash, Some(h2));
    // Earlier events keep the hash they were written under.
    assert_eq!(evs.iter().find(|e| e.kind == "after.boot").unwrap().rule_hash, Some(h1));
    // The new parent rule is live.
    assert!(is_deny(&check(&r.gate, "net.fetch")));
}

#[test]
fn a_denied_action_is_denied_and_chained_with_the_hash() {
    let f = fixture(&base_parent(), Some(&deny_toml("tool.shell_exec")));
    let r = start(&f);
    let h = r.rt.rule_hash().unwrap();
    let d = check(&r.gate, "tool.shell_exec");
    let GateDecision::Deny { reason, .. } = d else {
        panic!("expected Deny, got {d:?}");
    };
    assert!(reason.contains("d1"), "{reason}");
    assert!(!is_deny(&check(&r.gate, "tool.read_file")));
    let evs = events(&r.cm);
    let deny = evs.iter().rfind(|e| e.kind == "governance.deny").expect("deny is chained");
    assert_eq!(deny.rule_hash, Some(h));
    assert!(evs.iter().any(|e| e.kind == "governance.permit" && e.rule_hash == Some(h)));
}

// ── check_detailed through the overlay gate ──────────────────────────────

fn detailed(g: &Arc<dyn GateBackend>, action: &str) -> crate::gate::GateVerdict {
    g.check_detailed("agent-1", action, &json!({}))
        .expect("an overlay gate always has an engine behind it")
}

#[test]
fn check_detailed_reports_an_overlay_deny_with_its_rule_id() {
    use crate::governance::GovernanceDecision;
    let f = fixture(&base_parent(), Some(&deny_toml("tool.shell_exec")));
    let r = start(&f);
    let v = detailed(&r.gate, "tool.shell_exec");
    let GovernanceDecision::Deny(reason) = &v.decision else {
        panic!("expected Deny, got {:?}", v.decision);
    };
    assert!(reason.contains("d1"), "{reason}");
    assert!(v.evaluated_rules.iter().any(|id| id.contains("d1")), "{:?}", v.evaluated_rules);
    // It agrees with `check`, and writes nothing to the chain.
    assert!(is_deny(&check(&r.gate, "tool.shell_exec")));
    let before = events(&r.cm).len();
    let _ = detailed(&r.gate, "tool.shell_exec");
    assert_eq!(events(&r.cm).len(), before);
    assert!(matches!(detailed(&r.gate, "tool.read_file").decision, GovernanceDecision::Permit | GovernanceDecision::PermitWithWarning(_)));
}

#[test]
fn check_detailed_reports_an_overlay_approval_rule_as_escalate() {
    use crate::governance::GovernanceDecision;
    let f = fixture(
        &base_parent(),
        Some("[[require_approval]]\nactions = [\"cron.add\"]\n"),
    );
    let r = start(&f);
    let v = detailed(&r.gate, "cron.add");
    assert!(matches!(v.decision, GovernanceDecision::EscalateToHuman(_)), "{:?}", v.decision);
    assert!(matches!(check(&r.gate, "cron.add"), GateDecision::Defer { .. }));
}

#[test]
fn check_detailed_keeps_a_base_deny_through_the_overlay() {
    use crate::governance::GovernanceDecision;
    // The overlay denies something else; the parent's `workload.*` deny stands.
    let f = fixture(&base_parent(), Some(&deny_toml("tool.shell_exec")));
    let r = start(&f);
    let v = detailed(&r.gate, "workload.start");
    assert!(matches!(v.decision, GovernanceDecision::Deny(_)), "{:?}", v.decision);
    assert!(v.evaluated_rules.iter().any(|id| id == "WL-DENY"), "{:?}", v.evaluated_rules);
    assert!(is_deny(&check(&r.gate, "workload.start")));
}

// ── reload and update ────────────────────────────────────────────────────

#[test]
fn an_overlay_edited_on_disk_does_nothing_until_reload() {
    let f = fixture(&base_parent(), None);
    let r = start(&f);
    let h0 = r.rt.rule_hash().unwrap();
    assert!(!is_deny(&check(&r.gate, "tool.shell_exec")));

    std::fs::write(f.paths.overlay().unwrap(), deny_toml("tool.shell_exec")).unwrap();
    // Inert: same rules, same hash, still permitted, no event.
    assert!(!is_deny(&check(&r.gate, "tool.shell_exec")));
    assert_eq!(r.rt.rule_hash().unwrap(), h0);
    let n = events(&r.cm).iter().filter(|e| e.kind.starts_with("governance.overlay")).count();
    assert_eq!(n, 1, "only the boot event");

    let a = r.rt.reload().unwrap();
    assert_eq!(a.parent_version, 1);
    assert_ne!(r.rt.rule_hash().unwrap(), h0);
    assert!(is_deny(&check(&r.gate, "tool.shell_exec")));
    let evs = events(&r.cm);
    let last = evs.iter().rfind(|e| e.kind == "governance.overlay.applied").unwrap();
    assert_eq!(last.payload.as_ref().unwrap()["source"], "reload");
}

#[test]
fn a_bad_edit_is_rejected_on_reload_and_the_running_rules_stay() {
    let f = fixture(&base_parent(), Some(&deny_toml("tool.shell_exec")));
    let r = start(&f);
    let h = r.rt.rule_hash().unwrap();

    std::fs::write(f.paths.overlay().unwrap(), "permit = [\"tool.shell_exec\"]\n").unwrap();
    let e = r.rt.reload().unwrap_err();
    assert_eq!(e.key(), "permit");
    assert_eq!(r.rt.rule_hash().unwrap(), h);
    assert!(is_deny(&check(&r.gate, "tool.shell_exec")), "still denied");
    let evs = events(&r.cm);
    let rej = evs.iter().rfind(|e| e.kind == "governance.overlay.rejected").unwrap();
    let p = rej.payload.as_ref().unwrap();
    assert_eq!((p["source"].as_str(), p["key"].as_str()), (Some("reload"), Some("permit")));

    // Deleting the file is refused while a non-empty overlay is in force;
    // an explicit empty file is the deliberate way to clear it.
    std::fs::remove_file(f.paths.overlay().unwrap()).unwrap();
    assert_eq!(r.rt.reload().unwrap_err(), OverlayError::OverlayMissing);
    assert!(is_deny(&check(&r.gate, "tool.shell_exec")));
    std::fs::write(f.paths.overlay().unwrap(), "").unwrap();
    r.rt.reload().unwrap();
    assert!(!is_deny(&check(&r.gate, "tool.shell_exec")));
}

#[test]
fn a_pushed_policy_needs_a_valid_user_signature() {
    let f = fixture(&base_parent(), None);
    let r = start(&f);
    let h = r.rt.rule_hash().unwrap();

    let relaxed = parent_with(vec![], Limits::default(), 5);
    let mut forged = relaxed.clone();
    forged.sig = "00".repeat(64);
    let e = r.rt.apply_parent_update(forged).unwrap_err();
    assert_eq!(e.key(), "parent-policy.sig");

    let other = SigningKey::from_bytes(&[9u8; 32]);
    let theirs = crate::parent_policy::export_rules(
        vec![],
        0.9,
        false,
        &Limits::default(),
        &other,
        6,
        Utc::now(),
    )
    .unwrap();
    assert_eq!(
        r.rt.apply_parent_update(theirs).unwrap_err().key(),
        "parent-policy.user_key_id"
    );
    assert_eq!(r.rt.rule_hash().unwrap(), h, "nothing was applied");
    let rejected = events(&r.cm)
        .iter()
        .filter(|e| e.kind == "governance.overlay.rejected")
        .count();
    assert_eq!(rejected, 2);
}

#[test]
fn an_older_signed_policy_is_refused_and_survives_a_restart() {
    let f = fixture(&parent_with(base_parent().rules, base_parent().limits, 5), None);
    let r = start(&f);
    let v7 = parent_with(vec![], Limits::default(), 7);
    r.rt.apply_parent_update(v7).unwrap();
    let pin = f.paths.state_dir().unwrap().join(VERSION_PIN_FILE);
    assert_eq!(std::fs::read_to_string(&pin).unwrap().trim(), "7");

    let old = parent_with(base_parent().rules, base_parent().limits, 6);
    let e = r.rt.apply_parent_update(old.clone()).unwrap_err();
    assert!(matches!(e, OverlayError::Parent(ParentPolicyError::Rollback { have: 6, pinned: 7 })));

    // Put the old (validly signed, stricter or looser) file back and restart.
    write_parent(&f.paths, &old);
    let e = prepare_err(&f);
    assert_eq!(e.key(), "parent-policy.version");
}

#[test]
fn an_older_file_on_disk_is_ignored_by_reload() {
    let f = fixture(&parent_with(base_parent().rules, base_parent().limits, 5), None);
    let r = start(&f);
    r.rt.apply_parent_update(parent_with(vec![], Limits::default(), 7)).unwrap();
    let h = r.rt.rule_hash().unwrap();
    // Someone swaps the file for the older, valid, stricter policy.
    write_parent(&f.paths, &parent_with(base_parent().rules, base_parent().limits, 5));
    let a = r.rt.reload().unwrap();
    assert_eq!(a.parent_version, 7, "the newer accepted policy stays in force");
    assert_eq!(r.rt.rule_hash().unwrap(), h);
}

#[test]
fn reload_picks_up_a_newer_parent_file() {
    let f = fixture(&base_parent(), None);
    let r = start(&f);
    let h = r.rt.rule_hash().unwrap();
    let mut rules = base_parent().rules;
    rules.push(rule("GOV-NEW", RuleSeverity::Blocking, Some("net.*"), true, true));
    write_parent(&f.paths, &parent_with(rules, base_parent().limits, 2));
    let a = r.rt.reload().unwrap();
    assert_eq!(a.parent_version, 2);
    assert_ne!(r.rt.rule_hash().unwrap(), h);
    assert!(is_deny(&check(&r.gate, "net.fetch")));
}

#[test]
fn lowering_a_boot_cap_by_push_reports_restart_required() {
    let f = fixture(&base_parent(), None);
    let r = start(&f);
    assert!(!r.rt.applied().restart_required);
    let tighter = parent_with(
        base_parent().rules,
        Limits {
            max_processes: Some(8),
            spawn_budget: Some(8),
            ..Limits::default()
        },
        2,
    );
    let a = r.rt.apply_parent_update(tighter).unwrap();
    assert!(a.restart_required);
}

#[test]
fn the_gate_exports_what_it_runs_for_the_next_parent_policy() {
    let f = fixture(&base_parent(), Some(&deny_toml("tool.shell_exec")));
    let r = start(&f);
    let snap = r.gate.governance_snapshot().unwrap();
    assert!(snap.rules.iter().any(|x| x.id == "d1"));
    assert_eq!(snap.rules.len(), r.rt.applied().rule_count);
}

