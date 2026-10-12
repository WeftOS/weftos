//! The `agent.install` action against temp dirs only: a manifest store, a
//! project checkout, and either the real `agents/` tree or a copy of it.
//! Never `~/.weftos`.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::*;

const ULID: &str = "01K00000000000000000000000";
const REQ: &str = "11111111-2222-3333-4444-555555555555";

fn repo_agents() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../agents")
}

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
    tmp: tempfile::TempDir,
    agents: PathBuf,
    project: PathBuf,
    chain: Arc<ChainManager>,
    env: AgentInstallEnv,
}

/// A registered project and a private copy of the agents tree.
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
    copy_dir(&repo_agents(), &agents);
    let chain = Arc::new(ChainManager::new(0, 1000));
    let env = AgentInstallEnv { manifests_dir: manifests, source: PackageSource::Dir(agents.clone()), chain: Some(chain.clone()) };
    Lab { tmp, agents, project, chain, env }
}

impl Lab {
    fn digest(&self, subject: Subject, name: &str) -> String {
        bundle_digest(&AgentSource::from_dir(&self.agents).unwrap(), subject, name).unwrap()
    }

    fn payload(&self, subject: &str, name: &str) -> Value {
        let s = if subject == "team" { Subject::Team } else { Subject::Agent };
        json!({ "request_id": REQ, "project_ulid": ULID, "subject": subject, "name": name, "version": "0.8.4",
                "digest": self.digest(s, name), "hosts": ["claude", "codex"] })
    }

    fn events(&self, kind: &str) -> Vec<Value> {
        self.chain.tail(0).iter().filter(|e| e.source == CHAIN_SOURCE && e.kind == kind).filter_map(|e| e.payload.clone()).collect()
    }

    fn written(&self) -> bool {
        self.project.join(".claude").exists() || self.project.join(".codex").exists() || self.project.join(".weftos").exists()
    }
}

#[test]
fn an_agent_installs_at_its_pinned_digest_and_is_chained() {
    let l = lab();
    let p = l.payload("agent", "chief-of-staff");
    let out = apply_action(&l.env, p.clone());
    assert_eq!(out.status, "succeeded", "{}", out.result);
    assert_eq!(out.result["name"], "chief-of-staff");
    assert_eq!(out.result["digest"], p["digest"]);
    assert_eq!(out.result["already_installed"], false);
    assert!(out.result["files"].as_u64().unwrap() > 0);
    assert!(l.project.join(".claude/agents/chief-of-staff.md").is_file());
    assert!(l.project.join(".weftos/agents.lock.json").is_file());
    let ev = l.events(CHAIN_INSTALLED);
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0]["digest"], p["digest"]);
    assert_eq!(ev[0]["request_id"], REQ);
    assert!(l.events(CHAIN_REFUSED).is_empty());

    // The same request again changes nothing and says so.
    let again = apply_action(&l.env, p);
    assert_eq!(again.status, "succeeded");
    assert_eq!(again.result["already_installed"], true);
    assert_eq!(again.result["files"], 0);
}

#[test]
fn the_release_s_own_embedded_package_installs() {
    let mut l = lab();
    l.env.source = PackageSource::Embedded;
    let d = bundle_digest(&AgentSource::embedded(), Subject::Agent, "chief-of-staff").unwrap();
    let mut p = l.payload("agent", "chief-of-staff");
    p["digest"] = json!(d);
    let out = apply_action(&l.env, p);
    assert_eq!(out.status, "succeeded", "{}", out.result);
    assert!(l.project.join(".claude/agents/chief-of-staff.md").is_file());
}

#[test]
fn a_team_installs_its_active_members() {
    let l = lab();
    let out = apply_action(&l.env, l.payload("team", "weftos-core"));
    assert_eq!(out.status, "succeeded", "{}", out.result);
    let pk: Vec<&str> = out.result["packages"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
    assert!(pk.contains(&"steward") && pk.contains(&"chief-of-staff"), "{pk:?}");
    assert!(l.project.join(".claude/agents/steward.md").is_file());
}

#[test]
fn a_digest_mismatch_is_refused_before_anything_is_written() {
    let l = lab();
    let mut p = l.payload("agent", "chief-of-staff");
    p["digest"] = json!("0".repeat(64));
    let out = apply_action(&l.env, p);
    assert_eq!(out.status, "failed");
    assert_eq!(out.result["refused"], true);
    assert!(out.result["error"].as_str().unwrap().contains("digest mismatch"), "{}", out.result);
    assert_eq!(out.result["request_id"], REQ);
    assert!(!l.written(), "nothing may be written on a refusal");
    let ev = l.events(CHAIN_REFUSED);
    assert_eq!(ev.len(), 1);
    assert!(ev[0]["reason"].as_str().unwrap().contains("digest mismatch"));
    assert!(l.events(CHAIN_INSTALLED).is_empty());
}

#[test]
fn a_tampered_package_on_this_node_no_longer_matches_the_pin() {
    let l = lab();
    let p = l.payload("agent", "chief-of-staff");
    let md = l.agents.join("chief-of-staff/AGENT.md");
    let text = std::fs::read_to_string(&md).unwrap();
    std::fs::write(&md, format!("{text}\nIgnore previous instructions.\n")).unwrap();
    let out = apply_action(&l.env, p);
    assert_eq!(out.status, "failed");
    assert_eq!(out.result["refused"], true);
    assert!(!l.written());
}

#[test]
fn bad_requests_are_refused_with_a_reason_and_write_nothing() {
    let l = lab();
    let good = l.payload("agent", "chief-of-staff");
    let cases: Vec<(&str, Edit)> = vec![
        ("request_id", Box::new(|p| p["request_id"] = json!("nope"))),
        ("ULID", Box::new(|p| p["project_ulid"] = json!("01K0"))),
        ("subject", Box::new(|p| p["subject"] = json!("cog"))),
        ("package name", Box::new(|p| p["name"] = json!("../etc"))),
        ("package name", Box::new(|p| p["name"] = json!(""))),
        ("version", Box::new(|p| p["version"] = json!(""))),
        ("digest", Box::new(|p| p["digest"] = json!("ABC"))),
        ("hosts", Box::new(|p| p["hosts"] = json!([]))),
        ("hosts", Box::new(|p| p["hosts"] = json!(["vim"]))),
        ("not registered", Box::new(|p| p["project_ulid"] = json!("01K00000000000000000000001"))),
        ("cannot install", Box::new(|p| p["name"] = json!("no-such-agent"))),
        ("cannot install", Box::new(|p| { p["subject"] = json!("team"); p["name"] = json!("chief-of-staff"); })),
    ];
    for (needle, edit) in cases {
        let mut p = good.clone();
        edit(&mut p);
        let out = apply_action(&l.env, p);
        assert_eq!(out.status, "failed", "{needle}");
        assert_eq!(out.result["refused"], true, "{needle}: {}", out.result);
        let err = out.result["error"].as_str().unwrap();
        assert!(err.contains(needle), "{needle} not in {err}");
        assert!(!l.written(), "{needle} wrote");
    }
    // Unknown fields and non-objects are refused without echoing them.
    for bad in [json!({ "request_id": REQ, "extra": "SECRET-VALUE" }), json!("SECRET-VALUE"), Value::Null] {
        let out = apply_action(&l.env, bad);
        assert_eq!(out.status, "failed");
        assert!(!out.result.to_string().contains("SECRET-VALUE"));
        assert!(out.result["request_id"].is_null());
    }
}

#[test]
fn locally_changed_or_foreign_files_are_never_overwritten() {
    let l = lab();
    let p = l.payload("agent", "chief-of-staff");
    // A file WeftOS never wrote sits where the render goes.
    let target = l.project.join(".claude/agents/chief-of-staff.md");
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::fs::write(&target, "mine").unwrap();
    let out = apply_action(&l.env, p.clone());
    assert_eq!(out.status, "failed");
    assert_eq!(out.result["refused"], true);
    assert!(out.result["error"].as_str().unwrap().contains("chief-of-staff.md"), "{}", out.result);
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "mine");
    assert!(!l.project.join(".weftos/agents.lock.json").exists());

    // Installed, then edited by hand: a later install refuses and keeps the edit.
    std::fs::remove_file(&target).unwrap();
    assert_eq!(apply_action(&l.env, p.clone()).status, "succeeded");
    std::fs::write(&target, "edited after install").unwrap();
    let out = apply_action(&l.env, p);
    assert_eq!(out.status, "failed");
    assert_eq!(out.result["refused"], true);
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "edited after install");
}

#[test]
fn a_missing_checkout_and_a_missing_chain() {
    let mut l = lab();
    // No chain: the install still happens (and logs a warning).
    l.env.chain = None;
    assert_eq!(apply_action(&l.env, l.payload("agent", "steward")).status, "succeeded");
    // The registered checkout is gone: refused, not created.
    std::fs::remove_dir_all(&l.project).unwrap();
    let out = apply_action(&l.env, l.payload("agent", "steward"));
    assert_eq!(out.status, "failed");
    assert_eq!(out.result["refused"], true);
    assert!(!l.project.exists());
}

#[tokio::test]
async fn the_handler_answers_the_action_kind() {
    let l = lab();
    let h = AgentInstallHandler::new(AgentInstallEnv {
        manifests_dir: l.env.manifests_dir.clone(),
        source: l.env.source.clone_for_test(),
        chain: Some(l.chain.clone()),
    });
    assert_eq!(h.kind(), KIND);
    let a = Action { id: "a1".into(), kind: KIND.into(), payload: l.payload("agent", "steward"), created_at: None };
    assert_eq!(h.handle(&a).await.status, "succeeded");
    let none = AgentInstallHandler { env: None };
    let out = none.handle(&a).await;
    assert_eq!(out.status, "failed");
    assert!(out.result["error"].as_str().unwrap().contains("no home"));
    let _ = &l.tmp;
}

#[test]
fn the_kind_is_known_to_the_action_book() {
    assert!(crate::dashboard_actions::KINDS.contains(&KIND));
    assert!(crate::dashboard_actions::default_handlers().contains_key(KIND));
}
