//! Team apply/uninstall against temp project dirs and a synthetic agents tree.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use super::*;
use crate::init::digest::team_digest;

fn put(root: &Path, rel: &str, text: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, text).unwrap();
}

fn agent(dir: &Path, id: &str, ctx: &str) {
    put(dir, &format!("{id}/weftos-package.yaml"), &format!("name: {id}\nkind: lane\nversion: 1.2.0\n{ctx}"));
    put(dir, &format!("{id}/AGENT.md"), &format!("---\nname: {id}\ndescription: The {id} lane. Does things.\ntools: [Read]\n---\n{id} body\n"));
    put(dir, &format!("{id}/skills/{id}-skill/SKILL.md"), &format!("---\nname: {id}-skill\ndescription: {id} skill.\n---\nS\n"));
}

struct Lab {
    _tmp: tempfile::TempDir,
    agents: std::path::PathBuf,
    project: std::path::PathBuf,
}

fn lab() -> Lab {
    let tmp = tempfile::tempdir().unwrap();
    let agents = tmp.path().join("agents");
    let project = tmp.path().join("project");
    fs::create_dir_all(&project).unwrap();
    agent(&agents, "stew", "required_project_context:\n  - key: docs_root\n    description: docs\n  - key: board.cmd\n    description: board command\n");
    agent(&agents, "rev", "");
    agent(&agents, "scribe", "");
    agent(&agents, "later", "");
    put(&agents, "teams/core/team.yaml", "schema: 1\nname: core\nmembers:\n  - agent: stew\n    position: 1\n    active: true\n  - agent: rev\n    position: 2\n    active: true\n  - agent: later\n    position: 3\n    active: false\n");
    put(&agents, "teams/extra/team.yaml", "schema: 1\nname: extra\nmembers:\n  - agent: rev\n    position: 1\n  - agent: scribe\n    position: 2\n");
    Lab { _tmp: tmp, agents, project }
}

impl Lab {
    fn req(&self, team: &str, hosts: &[Host]) -> ApplyRequest {
        ApplyRequest {
            root: self.project.clone(),
            subject: Subject::Team,
            name: team.into(),
            hosts: hosts.to_vec(),
            from: Some(self.agents.clone()),
            pin: None,
            dry_run: false,
        }
    }

    fn from(&self) -> Option<std::path::PathBuf> {
        Some(self.agents.clone())
    }
}

fn tree(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        for e in fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(root, &p, out);
            } else {
                out.insert(p.strip_prefix(root).unwrap().to_string_lossy().into(), fs::read(&p).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

const ALL: [Host; 3] = [Host::Claude, Host::Grok, Host::Codex];

#[test]
fn apply_writes_active_members_only_and_records_the_lock() {
    let l = lab();
    put(&l.project, "docs/keep.md", "x\n");
    let a = apply_request(&l.req("core", &ALL)).unwrap();
    assert!(!a.already && a.touched > 0);
    assert!(l.project.join(".claude/agents/stew.md").is_file());
    assert!(l.project.join(".claude/agents/rev.md").is_file());
    assert!(!l.project.join(".claude/agents/later.md").exists(), "inactive member applied");
    assert!(a.collisions.iter().all(|c| c.disposition == Disposition::Copied));

    let src = AgentSource::from_dir(&l.agents).unwrap();
    let lock = Lock::read(&l.project).unwrap().unwrap();
    assert_eq!(lock.team.as_deref(), Some("core"));
    let t = &lock.teams[0];
    assert_eq!(t.digest, team_digest(&src, "core").unwrap(), "team digest pinned");
    assert_eq!(a.digest, t.digest);
    let slots: Vec<(&str, bool)> = t.slots.iter().map(|s| (s.agent.as_str(), s.active)).collect();
    assert_eq!(slots, vec![("stew", true), ("rev", true), ("later", false)]);
    let stew = lock.agents.iter().find(|x| x.id == "stew").unwrap();
    assert_eq!(stew.version, "1.2.0");
    assert_eq!(stew.digest, crate::init::digest::package_digest(&src, "stew").unwrap());
    assert!(stew.files.iter().all(|f| f.sha256.len() == 64));
    assert!(lock.agents.iter().all(|x| x.id != "later"));
    assert_eq!(lock.context.as_ref().unwrap().path, ".agents/project-context.md");
}

#[test]
fn birth_report_names_bound_and_missing_keys() {
    let l = lab();
    put(&l.project, "docs/keep.md", "x\n");
    let a = apply_request(&l.req("core", &[Host::Claude])).unwrap();
    assert_eq!(a.birth.bound.get("docs_root").map(String::as_str), Some("docs"));
    assert_eq!(a.birth.missing, vec!["board.cmd".to_string()]);
    assert!(a.birth.hooks.is_empty());
    assert!(a.birth.slots.iter().any(|s| s.agent == "stew" && s.skills == vec!["stew-skill"]));
    let ctx = fs::read_to_string(l.project.join(".agents/project-context.md")).unwrap();
    assert!(ctx.contains("`docs_root`: docs (detected)") && ctx.contains("`board.cmd`: TODO"));
}

#[test]
fn reapply_is_idempotent() {
    let l = lab();
    apply_request(&l.req("core", &ALL)).unwrap();
    let before = tree(&l.project);
    let again = apply_request(&l.req("core", &ALL)).unwrap();
    assert!(again.already && again.touched == 0);
    assert!(again.collisions.iter().all(|c| c.disposition == Disposition::SkippedIdentical));
    assert_eq!(tree(&l.project), before);
}

#[test]
fn local_edit_and_unmanaged_files_are_refused_before_any_write() {
    let l = lab();
    // A file WeftOS never wrote sits where an agent would land.
    put(&l.project, ".claude/agents/rev.md", "mine\n");
    let e = apply_request(&l.req("core", &[Host::Claude])).unwrap_err();
    assert!(e.refused && e.message.contains("nothing was written"), "{e}");
    assert!(e.collisions.iter().any(|c| c.path == ".claude/agents/rev.md" && c.disposition == Disposition::SkippedDivergent));
    assert!(e.collisions.iter().any(|c| c.disposition == Disposition::Copied), "report still lists what would copy");
    assert!(!l.project.join(".claude/agents/stew.md").exists(), "partial write");
    assert!(!l.project.join(".weftos/agents.lock.json").exists());
    assert_eq!(fs::read_to_string(l.project.join(".claude/agents/rev.md")).unwrap(), "mine\n");

    // After a clean apply, a local edit is drift: re-apply refuses and keeps it.
    fs::remove_file(l.project.join(".claude/agents/rev.md")).unwrap();
    apply_request(&l.req("core", &[Host::Claude])).unwrap();
    put(&l.project, ".claude/agents/stew.md", "edited\n");
    let e = apply_request(&l.req("core", &[Host::Claude])).unwrap_err();
    assert!(e.refused && e.collisions.iter().any(|c| c.path == ".claude/agents/stew.md"));
    assert_eq!(fs::read_to_string(l.project.join(".claude/agents/stew.md")).unwrap(), "edited\n");
}

#[test]
fn another_packages_file_is_not_taken_over() {
    let l = lab();
    apply_request(&l.req("core", &[Host::Claude])).unwrap();
    // Re-point the lock so stew.md looks owned by rev; stew's render would collide.
    let mut lock = Lock::read(&l.project).unwrap().unwrap();
    for a in &mut lock.agents {
        if a.id == "rev" {
            let f = lock_file(&l.project, ".claude/agents/stew.md");
            a.files.push(f);
        }
    }
    lock.agents.iter_mut().find(|a| a.id == "stew").unwrap().files.retain(|f| f.path != ".claude/agents/stew.md");
    put(&l.project, ".weftos/agents.lock.json", &lock.to_json());
    put(&l.project, ".claude/agents/stew.md", "rev's now\n");
    let e = apply_request(&l.req("core", &[Host::Claude])).unwrap_err();
    assert!(e.refused && e.collisions.iter().any(|c| c.path == ".claude/agents/stew.md" && c.owner == "stew"));
}

fn lock_file(root: &Path, rel: &str) -> crate::init::lock::LockFile {
    crate::init::lock::LockFile {
        path: rel.into(),
        host: Host::Claude,
        sha256: sha256_hex(&fs::read(root.join(rel)).unwrap()),
        merge: Merge::File,
    }
}

#[test]
fn digest_pin_must_match() {
    let l = lab();
    let mut r = l.req("core", &ALL);
    r.pin = Some("0".repeat(64));
    let e = apply_request(&r).unwrap_err();
    assert!(e.refused && e.message.contains("digest mismatch"), "{e}");
    assert!(tree(&l.project).is_empty(), "wrote despite a bad pin");
    let src = AgentSource::from_dir(&l.agents).unwrap();
    r.pin = Some(team_digest(&src, "core").unwrap());
    apply_request(&r).unwrap();
    // Unknown team: refused, not an empty apply.
    let mut bad = l.req("nope", &ALL);
    bad.pin = None;
    assert!(apply_request(&bad).unwrap_err().refused);
}

#[test]
fn uninstall_restores_the_project_exactly() {
    let l = lab();
    put(&l.project, "README.md", "project\n");
    put(&l.project, "AGENTS.md", "# mine\n\nkeep this\n");
    put(&l.project, ".claude/settings.json", "{}\n");
    let before = tree(&l.project);
    apply_request(&l.req("core", &ALL)).unwrap();
    assert_ne!(tree(&l.project), before);
    let u = uninstall(&l.project, Some("core"), &l.from(), false).unwrap();
    assert_eq!(u.removed, vec!["rev".to_string(), "stew".to_string()]);
    assert!(u.kept.is_empty());
    assert_eq!(tree(&l.project), before, "uninstall left something behind or removed something else");
}

#[test]
fn uninstall_removes_a_file_created_agents_md_and_keeps_edits() {
    let l = lab();
    apply_request(&l.req("core", &ALL)).unwrap();
    assert!(l.project.join("AGENTS.md").is_file());
    put(&l.project, ".claude/agents/stew.md", "edited\n");
    put(&l.project, ".agents/project-context.md", "# filled in\nboard.cmd: x\n");
    let u = uninstall(&l.project, None, &l.from(), false).unwrap();
    assert_eq!(u.kept, vec![".claude/agents/stew.md".to_string()]);
    let left = tree(&l.project);
    let names: Vec<&str> = left.keys().map(String::as_str).collect();
    assert_eq!(names, vec![".agents/project-context.md", ".claude/agents/stew.md"], "only project facts and edits remain");
}

#[test]
fn uninstalling_one_team_keeps_members_another_team_needs() {
    let l = lab();
    apply_request(&l.req("core", &[Host::Claude])).unwrap();
    apply_request(&l.req("extra", &[Host::Claude])).unwrap();
    let lock = Lock::read(&l.project).unwrap().unwrap();
    assert_eq!(lock.teams.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), vec!["core", "extra"]);
    let u = uninstall(&l.project, Some("extra"), &l.from(), false).unwrap();
    assert_eq!(u.removed, vec!["scribe".to_string()], "rev is core's too");
    assert!(l.project.join(".claude/agents/rev.md").is_file());
    assert!(!l.project.join(".claude/agents/scribe.md").exists());
    let lock = Lock::read(&l.project).unwrap().unwrap();
    assert_eq!(lock.teams.len(), 1);
    assert_eq!(lock.team.as_deref(), Some("core"));
    // Unknown team and an empty project are refusals.
    assert!(uninstall(&l.project, Some("extra"), &l.from(), false).unwrap_err().refused);
    let empty = tempfile::tempdir().unwrap();
    assert!(uninstall(empty.path(), None, &l.from(), false).unwrap_err().refused);
}

#[test]
fn status_reports_drift_and_dry_run_writes_nothing() {
    let l = lab();
    let mut r = l.req("core", &[Host::Claude]);
    r.dry_run = true;
    let d = apply_request(&r).unwrap();
    assert_eq!(d.touched, 0);
    assert!(tree(&l.project).is_empty());
    apply_request(&l.req("core", &[Host::Claude])).unwrap();
    put(&l.project, ".claude/agents/rev.md", "edited\n");
    fs::remove_file(l.project.join(".claude/agents/stew.md")).unwrap();
    let s = status(&l.project, &l.from()).unwrap();
    assert_eq!(s.teams[0].source_digest.as_deref(), Some(s.teams[0].digest.as_str()));
    let rev = s.agents.iter().find(|a| a.id == "rev").unwrap();
    assert_eq!(rev.modified, vec![".claude/agents/rev.md".to_string()]);
    let stew = s.agents.iter().find(|a| a.id == "stew").unwrap();
    assert_eq!(stew.missing, vec![".claude/agents/stew.md".to_string()]);
    assert!(format_status(&s).contains("team core"));
}

#[test]
fn a_package_applies_alone() {
    let l = lab();
    let mut r = l.req("scribe", &[Host::Claude]);
    r.subject = Subject::Agent;
    apply_request(&r).unwrap();
    assert!(l.project.join(".claude/agents/scribe.md").is_file());
    assert!(!l.project.join(".claude/agents/stew.md").exists());
    let lock = Lock::read(&l.project).unwrap().unwrap();
    assert!(lock.teams.is_empty());
    let mut missing = l.req("ghost", &[Host::Claude]);
    missing.subject = Subject::Agent;
    assert!(apply_request(&missing).unwrap_err().refused);
}

#[test]
fn a_standalone_package_uninstalls_and_a_team_member_is_refused() {
    let l = lab();
    put(&l.project, "README.md", "project\n");
    let before = tree(&l.project);
    let mut r = l.req("scribe", &[Host::Claude]);
    r.subject = Subject::Agent;
    apply_request(&r).unwrap();
    let u = uninstall_package(&l.project, "scribe", &l.from(), false).unwrap();
    assert_eq!(u.removed, vec!["scribe".to_string()]);
    assert!(u.kept.is_empty());
    assert_eq!(tree(&l.project), before);
    // Not installed (any more), and the unknown name, are refusals.
    assert!(uninstall_package(&l.project, "scribe", &l.from(), false).unwrap_err().refused);

    apply_request(&l.req("core", &[Host::Claude])).unwrap();
    let e = uninstall_package(&l.project, "rev", &l.from(), false).unwrap_err();
    assert!(e.refused && e.message.contains("team 'core'"), "{}", e.message);
    assert!(l.project.join(".claude/agents/rev.md").is_file());
    assert!(uninstall_package(&l.project, "ghost", &l.from(), false).unwrap_err().refused);
}

#[test]
fn an_edited_standalone_package_is_kept_and_reported() {
    let l = lab();
    let mut r = l.req("scribe", &[Host::Claude]);
    r.subject = Subject::Agent;
    apply_request(&r).unwrap();
    put(&l.project, ".claude/agents/scribe.md", "edited by hand\n");
    let u = uninstall_package(&l.project, "scribe", &l.from(), true).unwrap();
    assert_eq!(u.kept, vec![".claude/agents/scribe.md".to_string()]);
    assert_eq!(std::fs::read_to_string(l.project.join(".claude/agents/scribe.md")).unwrap(), "edited by hand\n");
}
