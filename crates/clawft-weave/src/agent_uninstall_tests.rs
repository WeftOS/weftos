//! The `agent.uninstall` action against temp dirs only (never `~/.weftos`):
//! installs go through the `agent.install` action, then come back out.

use std::path::{Path, PathBuf};

use clawft_kernel::chain::ChainManager;
use serde_json::{Value, json};

use super::*;
use crate::agent_install;
use weftos::init::digest::bundle_digest;
use weftos::init::source::AgentSource;

const ULID: &str = "01K00000000000000000000000";
const REQ: &str = "11111111-2222-3333-4444-555555555555";

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap().flatten() {
        let (src, dst) = (e.path(), to.join(e.file_name()));
        if src.is_dir() {
            copy_dir(&src, &dst);
        } else {
            std::fs::copy(&src, &dst).unwrap();
        }
    }
}

type Edit = Box<dyn Fn(&mut Value)>;

struct Lab {
    _tmp: tempfile::TempDir,
    agents: PathBuf,
    project: PathBuf,
    chain: Arc<ChainManager>,
    env: AgentInstallEnv,
}

fn lab() -> Lab {
    let tmp = tempfile::tempdir().unwrap();
    let manifests = tmp.path().join("projects");
    std::fs::create_dir_all(&manifests).unwrap();
    let project = tmp.path().join("app");
    std::fs::create_dir_all(&project).unwrap();
    let toml = format!(
        "schema = 1\nid = \"{ULID}\"\nname = \"app\"\nroot = {:?}\nstate = \"active\"\n\
         created = \"2026-10-07T02:48:45Z\"\nlast_seen = \"2026-10-07T02:48:46Z\"\nproject_toml = \"present\"\n",
        project.display().to_string()
    );
    std::fs::write(manifests.join(format!("{ULID}.toml")), toml).unwrap();
    let agents = tmp.path().join("agents");
    copy_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../agents"), &agents);
    let chain = Arc::new(ChainManager::new(0, 1000));
    let env = AgentInstallEnv { manifests_dir: manifests, source: PackageSource::Dir(agents.clone()), chain: Some(chain.clone()) };
    Lab { _tmp: tmp, agents, project, chain, env }
}

impl Lab {
    fn install(&self, subject: &str, name: &str) {
        let s = if subject == "team" { Subject::Team } else { Subject::Agent };
        let digest = bundle_digest(&AgentSource::from_dir(&self.agents).unwrap(), s, name).unwrap();
        let out = agent_install::apply_action(
            &self.env,
            json!({ "request_id": REQ, "project_ulid": ULID, "subject": subject, "name": name, "version": "0.8.4",
                    "digest": digest, "hosts": ["claude"] }),
        );
        assert_eq!(out.status, "succeeded", "{}", out.result);
    }

    fn payload(&self, subject: &str, name: &str) -> Value {
        json!({ "request_id": REQ, "project_ulid": ULID, "subject": subject, "name": name })
    }

    fn events(&self, kind: &str) -> Vec<Value> {
        self.chain.tail(0).iter().filter(|e| e.source == CHAIN_SOURCE && e.kind == kind).filter_map(|e| e.payload.clone()).collect()
    }
}

#[test]
fn an_installed_agent_uninstalls_and_is_chained() {
    let l = lab();
    l.install("agent", "chief-of-staff");
    let md = l.project.join(".claude/agents/chief-of-staff.md");
    assert!(md.is_file());
    let out = apply_action(&l.env, l.payload("agent", "chief-of-staff"));
    assert_eq!(out.status, "succeeded", "{}", out.result);
    assert_eq!(out.result["request_id"], REQ);
    assert_eq!(out.result["removed"], json!(["chief-of-staff"]));
    assert!(out.result["files"].as_u64().unwrap() > 0);
    assert!(!md.exists());
    let ev = l.events(CHAIN_UNINSTALLED);
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0]["request_id"], REQ);
    assert!(l.events(CHAIN_REFUSED).is_empty());
    // Nothing is left to uninstall: a refusal, not a silent success.
    let again = apply_action(&l.env, l.payload("agent", "chief-of-staff"));
    assert_eq!(again.status, "failed");
    assert_eq!(again.result["refused"], true);
}

#[test]
fn a_team_uninstalls() {
    let l = lab();
    l.install("team", "weftos-core");
    assert!(l.project.join(".claude/agents/steward.md").is_file());
    let out = apply_action(&l.env, l.payload("team", "weftos-core"));
    assert_eq!(out.status, "succeeded", "{}", out.result);
    assert!(out.result["removed"].as_array().unwrap().iter().any(|v| v == "steward"));
    assert!(!l.project.join(".claude/agents/steward.md").exists());
}

#[test]
fn edited_files_refuse_the_request_and_are_named() {
    let l = lab();
    l.install("agent", "chief-of-staff");
    let md = l.project.join(".claude/agents/chief-of-staff.md");
    std::fs::write(&md, "my own words\n").unwrap();
    let out = apply_action(&l.env, l.payload("agent", "chief-of-staff"));
    assert_eq!(out.status, "failed");
    assert_eq!(out.result["refused"], true);
    assert_eq!(out.result["request_id"], REQ);
    assert_eq!(out.result["edited"], json!([".claude/agents/chief-of-staff.md"]));
    assert_eq!(std::fs::read_to_string(&md).unwrap(), "my own words\n", "an edited file is never touched");
    assert!(l.project.join(".weftos/agents.lock.json").is_file(), "nothing may be written on a refusal");
    let ev = l.events(CHAIN_REFUSED);
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0]["edited"], json!([".claude/agents/chief-of-staff.md"]));
    assert!(l.events(CHAIN_UNINSTALLED).is_empty());
}

#[test]
fn a_team_member_cannot_be_uninstalled_alone() {
    let l = lab();
    l.install("team", "weftos-core");
    let out = apply_action(&l.env, l.payload("agent", "steward"));
    assert_eq!(out.status, "failed");
    assert_eq!(out.result["refused"], true);
    assert!(out.result["error"].as_str().unwrap().contains("uninstall that team"), "{}", out.result);
    assert!(l.project.join(".claude/agents/steward.md").is_file());
}

#[test]
fn bad_requests_are_refused_with_a_reason() {
    let l = lab();
    l.install("agent", "chief-of-staff");
    let good = l.payload("agent", "chief-of-staff");
    let cases: Vec<(&str, Edit)> = vec![
        ("request_id", Box::new(|p| p["request_id"] = json!("nope"))),
        ("ULID", Box::new(|p| p["project_ulid"] = json!("01K0"))),
        ("subject", Box::new(|p| p["subject"] = json!("cog"))),
        ("package name", Box::new(|p| p["name"] = json!("../etc"))),
        ("not registered", Box::new(|p| p["project_ulid"] = json!("01K00000000000000000000001"))),
        ("not installed", Box::new(|p| p["name"] = json!("no-such-agent"))),
        ("not applied", Box::new(|p| { p["subject"] = json!("team"); p["name"] = json!("nope"); })),
    ];
    for (needle, edit) in cases {
        let mut p = good.clone();
        edit(&mut p);
        let out = apply_action(&l.env, p);
        assert_eq!(out.status, "failed", "{needle}");
        assert_eq!(out.result["refused"], true, "{needle}: {}", out.result);
        assert!(out.result["error"].as_str().unwrap().contains(needle), "{needle}: {}", out.result);
    }
    assert!(l.project.join(".claude/agents/chief-of-staff.md").is_file(), "refusals wrote");
    for bad in [json!({ "request_id": REQ, "extra": "SECRET-VALUE" }), json!("SECRET-VALUE"), Value::Null] {
        let out = apply_action(&l.env, bad);
        assert_eq!(out.status, "failed");
        assert!(!out.result.to_string().contains("SECRET-VALUE"));
    }
}

#[test]
fn the_action_is_registered() {
    assert!(crate::dashboard_actions::KINDS.contains(&KIND));
    assert!(crate::dashboard_actions::default_handlers().get(KIND).is_some_and(|h| h.kind() == KIND));
}
