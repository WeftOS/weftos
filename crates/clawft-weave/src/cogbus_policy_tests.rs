//! The daemon's cogbus policy (kernel rules, `cogbus-policy.json`), its
//! activity log and the report section (ADR-122 B3).

use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

use clawft_kernel::governance::{GovernanceBranch, GovernanceRule, GovernanceRuleType, RuleSeverity};
use clawft_types::cog_topics::{InputPolicy, TopicClass};
use serde_json::json;
use weftos_cogbus::{Broker, BrokerConfig, BusHooks, BusSink, ErrorCode, InputSpec, InstanceSpec, OutputSpec};

use super::*;
use crate::cogbus_activity::CogbusActivity;

const P1: &str = "01J9ZXW0PRJCTAAAAAAAAAAAAA";
const P2: &str = "01J9ZXW0PRJCTBBBBBBBBBBBBB";

fn req<'a>(action: BusAction, cog: &'a str, project: Option<&'a str>, topic: &'a str) -> BusRequest<'a> {
    BusRequest { action, instance_id: "i1", cog_id: cog, project_id: project, topic, schema: "telemetry.v1", topic_project: None }
}

fn write(dir: &Path, v: serde_json::Value) {
    let p = dir.join(POLICY_FILE);
    std::fs::write(&p, v.to_string()).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
}

fn rule(id: &str, action: &str, tool: Option<&str>) -> GovernanceRule {
    GovernanceRule {
        id: id.into(),
        description: "test".into(),
        branch: GovernanceBranch::Legislative,
        severity: RuleSeverity::Blocking,
        active: true,
        reference_url: None,
        sop_category: None,
        rule_type: GovernanceRuleType::General,
        action_selector: Some(action.into()),
        tool_selector: tool.map(String::from),
        force_on_match: true,
    }
}

#[test]
fn no_file_and_no_rules_allow_everything_and_grant_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let p = CogbusPolicy::new(tmp.path(), None);
    for a in [BusAction::Declare, BusAction::Publish, BusAction::Subscribe] {
        assert_eq!(p.decide(&req(a, "c", Some(P1), "telemetry/x")), Ok(()));
    }
    assert!(p.grants(&req(BusAction::Subscribe, "c", Some(P1), "telemetry/*")).is_empty());
}

#[test]
fn a_kernel_rule_denies_an_action_or_one_topic() {
    let tmp = tempfile::tempdir().unwrap();
    let rules = vec![rule("no-audio", "cogbus.subscribe", Some("audio.pcm/*"))];
    let p = CogbusPolicy::new(tmp.path(), Some((rules, 0.9, false)));
    let e = p.decide(&req(BusAction::Subscribe, "c", Some(P1), "audio.pcm/mic0")).unwrap_err();
    assert!(e.contains("no-audio"), "{e}");
    assert_eq!(p.decide(&req(BusAction::Subscribe, "c", Some(P1), "telemetry/x")), Ok(()));
    assert_eq!(p.decide(&req(BusAction::Publish, "c", Some(P1), "audio.pcm/mic0")), Ok(()));
    // An overlay-style deny on the action refuses all of it.
    let p = CogbusPolicy::new(tmp.path(), Some((vec![rule("no-pub", "cogbus.publish", None)], 0.9, false)));
    assert!(p.decide(&req(BusAction::Publish, "c", Some(P1), "telemetry/x")).is_err());
    assert_eq!(p.decide(&req(BusAction::Declare, "c", Some(P1), "telemetry")), Ok(()));
}

#[test]
fn file_denies_match_on_every_field_they_set() {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), json!({"deny": [
        {"cogs": ["bad"], "reason": "quarantined cog"},
        {"actions": ["cogbus.subscribe"], "topics": ["telemetry/secret"], "projects": [P2]},
    ]}));
    let p = CogbusPolicy::new(tmp.path(), None);
    let e = p.decide(&req(BusAction::Declare, "bad", Some(P1), "telemetry")).unwrap_err();
    assert!(e.contains("deny[0]: quarantined cog"), "{e}");
    assert!(p.decide(&req(BusAction::Subscribe, "ok", Some(P2), "telemetry/secret")).is_err());
    // The wildcard would read the denied source, so it is refused too.
    assert!(p.decide(&req(BusAction::Subscribe, "ok", Some(P2), "telemetry/*")).is_err());
    assert_eq!(p.decide(&req(BusAction::Subscribe, "ok", Some(P2), "telemetry/other")), Ok(()));
    assert_eq!(p.decide(&req(BusAction::Subscribe, "ok", Some(P1), "telemetry/secret")), Ok(()));
    assert_eq!(p.decide(&req(BusAction::Publish, "ok", Some(P2), "telemetry/secret")), Ok(()));
}

#[test]
fn grants_are_per_reader_project_kind_and_cog() {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), json!({"grants": [
        {"to": P2, "from": P1, "topics": ["telemetry/board", "detections/*"]},
        {"to": P2, "from": P1, "topics": ["*"], "cogs": ["feeder"]},
    ]}));
    let p = CogbusPolicy::new(tmp.path(), None);
    let g = p.grants(&req(BusAction::Subscribe, "any", Some(P2), "telemetry/*"));
    assert_eq!(g, vec![Grant { project: P1.into(), source: Some("board".into()) }]);
    let g = p.grants(&req(BusAction::Subscribe, "feeder", Some(P2), "audio.pcm/*"));
    assert_eq!(g, vec![Grant { project: P1.into(), source: None }]);
    assert!(p.grants(&req(BusAction::Subscribe, "any", Some(P1), "telemetry/*")).is_empty());
    assert!(p.grants(&req(BusAction::Subscribe, "any", None, "telemetry/*")).is_empty());
}

#[test]
fn a_bad_or_loose_file_refuses_everything_and_a_fix_is_picked_up() {
    let tmp = tempfile::tempdir().unwrap();
    for bad in [
        json!({"grants": [{"to": P1, "from": P1, "topics": ["telemetry/*"]}]}),
        json!({"deny": [{"actions": ["cogbus.delete"]}]}),
        json!({"deny": [{"topics": ["Not A Topic"]}]}),
        json!({"grant": []}),
    ] {
        write(tmp.path(), bad.clone());
        let p = CogbusPolicy::new(tmp.path(), None);
        assert!(p.decide(&req(BusAction::Declare, "c", Some(P1), "telemetry")).is_err(), "{bad}");
    }
    write(tmp.path(), json!({}));
    let f = tmp.path().join(POLICY_FILE);
    std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o666)).unwrap();
    let p = CogbusPolicy::new(tmp.path(), None);
    let e = p.decide(&req(BusAction::Declare, "c", Some(P1), "telemetry")).unwrap_err();
    assert!(e.contains("world-writable"), "{e}");
    // A fixed file takes effect without a restart.
    std::fs::write(&f, r#"{"deny": [{"cogs": ["c"]}]}"#).unwrap();
    std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert!(p.decide(&req(BusAction::Declare, "c", Some(P1), "telemetry")).unwrap_err().contains("deny[0]"));
    assert_eq!(p.decide(&req(BusAction::Declare, "d", Some(P1), "telemetry")), Ok(()));
}

fn spec(id: &str, project: &str, writes: bool) -> InstanceSpec {
    let o = OutputSpec { topic: "telemetry".into(), schema: "telemetry.v1".into(), class: TopicClass::Reading };
    let i = InputSpec { topic: "telemetry".into(), schema: "telemetry.v1".into(), policy: InputPolicy::DropOldest };
    InstanceSpec {
        instance_id: id.into(),
        cog_id: id.into(),
        project_id: Some(project.into()),
        uid: None,
        outputs: if writes { vec![o] } else { vec![] },
        inputs: if writes { vec![] } else { vec![i] },
    }
}

#[test]
fn the_broker_with_the_daemon_hooks_grants_denies_and_logs() {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), json!({
        "grants": [{"to": P2, "from": P1, "topics": ["telemetry/board"]}],
        "deny": [{"actions": ["cogbus.subscribe"], "cogs": ["nosy"]}],
    }));
    let act = Arc::new(CogbusActivity::memory());
    let hooks = BusHooks { policy: Arc::new(CogbusPolicy::new(tmp.path(), None)), sink: act.clone(), ..BusHooks::default() };
    let b = Broker::new(BrokerConfig::new("n")).with_hooks(hooks);
    let tok = |n: u8| format!("{n:02x}").repeat(32);
    b.register(spec("w", P1, true), &tok(1)).unwrap();
    b.register(spec("r", P2, false), &tok(2)).unwrap();
    b.register(spec("nosy", P2, false), &tok(3)).unwrap();
    let w = b.authenticate(&tok(1), None).unwrap();
    let mut r = b.authenticate(&tok(2), None).unwrap();
    let mut nosy = b.authenticate(&tok(3), None).unwrap();
    b.publish(&w, "telemetry", "board", "telemetry.v1", 1, json!({})).unwrap();
    b.subscribe(&mut r, "telemetry/board", "telemetry.v1").unwrap();
    let e = b.subscribe(&mut nosy, "telemetry/board", "telemetry.v1").unwrap_err();
    assert_eq!(e.code, ErrorCode::Denied);
    b.record_refusal(&e, "nosy");

    let rows = act.log().query(None, 10);
    let got: Vec<(String, String, String)> =
        rows.iter().map(|r| (r.exposure.clone(), r.caller_node.clone(), r.outcome.clone())).collect();
    assert_eq!(got[0], ("cogbus.subscribe:telemetry/board".into(), "r".into(), "ok".into()));
    assert_eq!(got[1].0, "cogbus.subscribe:telemetry/board");
    assert_eq!(got[1].1, "nosy");
    assert!(got[1].2.starts_with("refused:denied cogbus.subscribe denied"), "{}", got[1].2);
    assert_eq!(rows[1].caller_user.as_deref(), Some(P2));
    act.log().verify().unwrap();
}

#[test]
fn the_activity_log_persists_chained_in_the_runtime_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let a = CogbusActivity::open(tmp.path());
    a.record(&weftos_cogbus::BusActivity {
        ts_ms: clawft_kernel::exposure::registry::now_ms(),
        action: "cogbus.stop".into(),
        instance: "i1".into(),
        project: None,
        topic: "-".into(),
        outcome: weftos_cogbus::Outcome::Stopped { refusals: 20 },
    });
    drop(a);
    let again = CogbusActivity::open(tmp.path());
    let rows = again.log().query(None, 5);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].outcome, "stopped:20 refusals");
    again.log().verify().unwrap();
}

#[test]
fn the_report_section_is_bounded_and_busiest_first() {
    let b = Broker::new(BrokerConfig::new("n"));
    let tok = |n: u8| format!("{n:02x}").repeat(32);
    b.register(spec("w", P1, true), &tok(1)).unwrap();
    let w = b.authenticate(&tok(1), None).unwrap();
    for i in 0..100 {
        for _ in 0..(i % 7 + 1) {
            b.publish(&w, "telemetry", &format!("s{i:03}"), "telemetry.v1", 1, json!({})).unwrap();
        }
    }
    let v = crate::cogbus_serve::report_of(&b, clawft_kernel::exposure::registry::now_ms());
    assert_eq!(v["total"], 100);
    let items = v["items"].as_array().unwrap();
    assert_eq!(items.len(), crate::cogbus_serve::REPORT_TOPICS);
    assert_eq!(items[0]["msgs"], 7);
    let mut keys: Vec<&String> = items[0].as_object().unwrap().keys().collect();
    keys.sort();
    assert_eq!(keys, ["class", "drops", "msgs", "project", "schema", "subs", "topic", "writer"]);
    assert_eq!(v["refusals_1h"], 0);
    assert!(v.to_string().len() < 16 * 1024, "{} bytes", v.to_string().len());

    // The report budget trims topic rows and keeps the total.
    let mut report = json!({"host": "h", "mesh_identity": "id", "topics": v, "exposures": {"items": []}});
    let fits = crate::dashboard_budget::fit(&mut report, 2 * 1024);
    assert!(fits, "{} bytes", report.to_string().len());
    assert_eq!(report["topics_truncated"], true);
    assert_eq!(report["topics"]["total"], 100);
}
