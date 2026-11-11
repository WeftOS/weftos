//! `weftos init` against team.yaml files: each field is applied to a throwaway
//! project, or the run is refused. Throwaway trees only; nothing outside them.

use std::fs;
use std::path::{Path, PathBuf};

use crate::init::apply::apply;
use crate::init::plan::{AgentInitOptions, Plan, plan};
use crate::init::render::Host;
use crate::init::team::{ApplyRequest, apply_request, uninstall};
use crate::init::digest::Subject;

fn put(root: &Path, rel: &str, text: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, text).unwrap();
}

struct Lab {
    _tmp: tempfile::TempDir,
    agents: PathBuf,
    project: PathBuf,
}

/// Agents a (Read, Grep, Bash), b (Read), c (no tool list), all at version 1.2.0.
fn lab(team_yaml: &str) -> Lab {
    let tmp = tempfile::tempdir().unwrap();
    let agents = tmp.path().join("agents");
    let project = tmp.path().join("project");
    fs::create_dir_all(&project).unwrap();
    for (id, tools) in [("a", "tools: [Read, Grep, Bash]\n"), ("b", "tools: [Read]\n"), ("c", "")] {
        put(&agents, &format!("{id}/weftos-package.yaml"), &format!("name: {id}\nkind: lane\nversion: 1.2.0\n"));
        put(&agents, &format!("{id}/AGENT.md"), &format!("---\nname: {id}\ndescription: The {id} lane. Does things.\n{tools}---\n{id} body\n"));
    }
    put(&agents, "teams/t/team.yaml", team_yaml);
    Lab { _tmp: tmp, agents, project }
}

/// A schema 1 team named `t` with these members and extra top-level text.
fn team(members: &str, top: &str) -> String {
    format!("schema: 1\nname: t\n{top}members:\n{members}")
}

const AB: &str = "  - agent: a\n  - agent: b\n";

fn opts(l: &Lab, hosts: &[Host]) -> AgentInitOptions {
    AgentInitOptions {
        root: l.project.clone(),
        hosts: hosts.to_vec(),
        from: Some(l.agents.clone()),
        team: Some("t".into()),
        ..Default::default()
    }
}

const ALL: [Host; 3] = [Host::Claude, Host::Grok, Host::Codex];

fn refused(y: &str) -> String {
    let l = lab(y);
    plan(&opts(&l, &ALL)).expect_err("this team.yaml must be refused").to_string()
}

fn text(p: &Plan, path: &str) -> String {
    let i = p.items.iter().find(|i| i.path == path).unwrap_or_else(|| panic!("no {path} in the plan"));
    String::from_utf8(i.bytes.clone()).unwrap()
}

/// The three rendered files of agent `id`.
fn files(p: &Plan, id: &str) -> [String; 3] {
    [
        text(p, &format!(".claude/agents/{id}.md")),
        text(p, &format!(".grok/agents/{id}.md")),
        text(p, &format!(".codex/agents/{id}.toml")),
    ]
}

/// Member `a` with these extra fields: every host's file for `a` must hold `needle`.
fn charter_has(member_fields: &str, needle: &str) {
    let l = lab(&team(&format!("  - agent: a\n{member_fields}  - agent: b\n"), ""));
    let p = plan(&opts(&l, &ALL)).unwrap();
    for f in files(&p, "a") {
        assert!(f.contains(needle), "missing {needle:?} in:\n{f}");
    }
}

// ---- schema, name, version, description: checked, refused when wrong ----

#[test]
fn schema_other_than_1_is_refused() {
    assert!(refused("schema: 2\nname: t\nmembers:\n  - agent: a\n").contains("schema must be 1"));
}

#[test]
fn an_unknown_top_level_field_is_reported() {
    let e = refused(&team(AB, "bogus: 1\n"));
    assert!(e.contains("unknown field `bogus`"), "{e}");
}

#[test]
fn an_unknown_member_field_is_reported() {
    let e = refused(&team("  - agent: a\n    colour: red\n", ""));
    assert!(e.contains("unknown field `colour`"), "{e}");
}

#[test]
fn name_must_equal_the_team_directory() {
    assert!(refused("schema: 1\nname: other\nmembers:\n  - agent: a\n").contains("name must equal"));
}

#[test]
fn version_and_description_must_be_text() {
    assert!(refused(&team(AB, "version: 3\n")).contains("version must be"));
    assert!(refused(&team(AB, "description: [x]\n")).contains("description must be"));
}

// ---- members[].agent@version ----

#[test]
fn a_pin_must_equal_the_catalog_version() {
    let e = refused(&team("  - agent: a@9.9.9\n", ""));
    assert!(e.contains("pins @9.9.9") && e.contains("1.2.0"), "{e}");
    let l = lab(&team("  - agent: a@1.2.0\n", ""));
    plan(&opts(&l, &ALL)).expect("a matching pin is accepted");
}

// ---- active, phase, position ----

#[test]
fn active_must_be_a_boolean_and_inactive_members_name_their_phase() {
    assert!(refused(&team("  - agent: a\n    active: \"false\"\n", "")).contains("true or false"));
    let l = lab(&team("  - agent: a\n  - agent: c\n    active: false\n    phase: 2\n", ""));
    let p = plan(&opts(&l, &ALL)).unwrap();
    assert!(!p.selected.contains(&"c".to_string()));
    assert!(p.warnings.iter().any(|w| w.contains("c (phase 2)")), "{:?}", p.warnings);
}

#[test]
fn phase_and_position_must_be_whole_numbers() {
    assert!(refused(&team("  - agent: a\n    phase: soon\n", "")).contains("whole number"));
    assert!(refused(&team("  - agent: a\n    position: first\n", "")).contains("whole number"));
}

#[test]
fn position_orders_the_lock_slots() {
    let l = lab(&team("  - agent: b\n    position: 2\n  - agent: a\n    position: 1\n", ""));
    let p = plan(&opts(&l, &[Host::Claude])).unwrap();
    let slots: Vec<String> = slots_of(&p).iter().map(|(a, _)| a.clone()).collect();
    assert_eq!(slots, vec!["a", "b"]);
}

/// `(agent, active)` of the first team's lock slots.
fn slots_of(p: &Plan) -> Vec<(String, bool)> {
    let v: serde_json::Value = serde_json::from_str(&p.lock_json).unwrap();
    v["teams"][0]["slots"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| (s["agent"].as_str().unwrap().to_string(), s["active"].as_bool().unwrap()))
        .collect()
}

// ---- charter fields, on every host ----

#[test]
fn nickname_is_applied() {
    charter_has("    nickname: Aye\n", "Known on this team as Aye.");
}

#[test]
fn one_job_is_applied() {
    charter_has("    one_job: Keep the lights on.\n", "One job: Keep the lights on.");
}

#[test]
fn authority_is_applied_and_checked() {
    charter_has("    authority: escalated\n", "Authority: escalated.");
    assert!(refused(&team("  - agent: a\n    authority: boss\n", "")).contains("authority `boss`"));
}

#[test]
fn owner_is_applied() {
    charter_has("    owner: docs-owner\n", "Owner role: docs-owner.");
}

#[test]
fn tiers_are_applied_and_checked() {
    charter_has("    tiers: [internal, restricted]\n", "tiers: internal, restricted.");
    assert!(refused(&team("  - agent: a\n    tiers: [secret]\n", "")).contains("tiers `secret`"));
}

#[test]
fn must_not_is_applied() {
    charter_has("    must_not: [touch production, merge]\n", "- touch production");
    charter_has("    must_not: [touch production, merge]\n", "- merge");
}

// ---- tools and model_tier ----

#[test]
fn tools_narrow_the_claude_tool_list_and_never_widen_it() {
    let l = lab(&team("  - agent: a\n    tools: [Read]\n", ""));
    let p = plan(&opts(&l, &[Host::Claude])).unwrap();
    let f = text(&p, ".claude/agents/a.md");
    assert!(f.contains("tools: Read\n") && !f.contains("Grep"), "{f}");
    let e = refused(&team("  - agent: b\n    tools: [Read, Write]\n", ""));
    assert!(e.contains("widen the package") && e.contains("Write"), "{e}");
}

#[test]
fn tools_cannot_be_enforced_on_grok_or_codex_so_the_run_is_refused() {
    let l = lab(&team("  - agent: a\n    tools: [Read]\n", ""));
    for h in [Host::Grok, Host::Codex] {
        let e = plan(&opts(&l, &[h])).unwrap_err().to_string();
        assert!(e.contains("only the claude host can enforce"), "{e}");
    }
}

#[test]
fn model_tier_sets_the_claude_model_and_is_reported_elsewhere() {
    let l = lab(&team("  - agent: a\n    model_tier: small\n  - agent: b\n    model_tier: top\n  - agent: c\n    model_tier: mid\n", ""));
    let p = plan(&opts(&l, &ALL)).unwrap();
    assert!(text(&p, ".claude/agents/a.md").contains("model: haiku\n"));
    assert!(text(&p, ".claude/agents/b.md").contains("model: opus\n"));
    assert!(text(&p, ".claude/agents/c.md").contains("model: sonnet\n"));
    for h in ["codex", "grok"] {
        let w = p.warnings.iter().find(|w| w.contains(&format!("not applied on {h}"))).expect("a warning");
        assert!(w.contains("a, b, c"), "{w}");
    }
    assert!(refused(&team("  - agent: a\n    model_tier: huge\n", "")).contains("model_tier `huge`"));
}

#[test]
fn without_model_tier_claude_keeps_inherit() {
    let l = lab(&team(AB, ""));
    let p = plan(&opts(&l, &[Host::Claude])).unwrap();
    assert!(text(&p, ".claude/agents/a.md").contains("model: inherit\n"));
}

// ---- edges, write_authority, team_rules, hosts ----

#[test]
fn edges_are_applied_to_both_ends_and_checked() {
    let l = lab(&team(AB, "edges:\n  - {from: a, to: b, kind: delegates_to}\n"));
    let p = plan(&opts(&l, &ALL)).unwrap();
    for f in files(&p, "a") {
        assert!(f.contains("- you delegates_to b"), "{f}");
    }
    for f in files(&p, "b") {
        assert!(f.contains("- a delegates_to you"), "{f}");
    }
    assert!(refused(&team(AB, "edges:\n  - {from: a, to: zed, kind: consults}\n")).contains("not a member"));
    assert!(refused(&team(AB, "edges:\n  - {from: a, to: b, kind: loves}\n")).contains("kind `loves`"));
    assert!(refused(&team(AB, "edges:\n  - {from: a, to: a, kind: consults}\n")).contains("itself"));
}

#[test]
fn write_authority_is_applied_and_checked() {
    let l = lab(&team(AB, "write_authority:\n  board: a\n  docs: b\n"));
    let p = plan(&opts(&l, &ALL)).unwrap();
    for f in files(&p, "a") {
        assert!(f.contains("- board: a (you)") && f.contains("- docs: b\n"), "{f}");
    }
    assert!(refused(&team(AB, "write_authority:\n  board: zed\n")).contains("holder `zed`"));
    assert!(refused(&team(AB, "write_authority:\n  board: [a, b]\n")).contains("exactly one member"));
}

#[test]
fn team_rules_are_applied_and_checked() {
    let l = lab(&team(AB, "team_rules:\n  - name: stand-down\n    statement: Stop when blocked.\n"));
    let p = plan(&opts(&l, &ALL)).unwrap();
    for f in files(&p, "b") {
        assert!(f.contains("- stand-down: Stop when blocked."), "{f}");
    }
    assert!(refused(&team(AB, "team_rules:\n  - name: x\n    statement: \"one\\ntwo\"\n")).contains("one line"));
    let dup = "team_rules:\n  - {name: x, statement: s}\n  - {name: x, statement: t}\n";
    assert!(refused(&team(AB, dup)).contains("declared twice"));
    assert!(refused(&team(AB, "team_rules:\n  - {name: x, statement: s, extra: 1}\n")).contains("unknown field `extra`"));
}

#[test]
fn host_tool_maps_are_applied_per_host_and_checked() {
    let hosts = "hosts:\n  claude:\n    tool_map: {Task: Task}\n  codex:\n    tool_map: {Task: spawn_agent}\n";
    let l = lab(&team(AB, hosts));
    let p = plan(&opts(&l, &ALL)).unwrap();
    assert!(text(&p, ".codex/agents/a.toml").contains("Task is `spawn_agent`"));
    assert!(text(&p, ".claude/agents/a.md").contains("Tool names on claude:\n- Task is `Task`"));
    assert!(!text(&p, ".grok/agents/a.md").contains("Tool names on"));
    assert!(refused(&team(AB, "hosts:\n  zed:\n    tool_map: {Task: t}\n")).contains("unknown field `zed`"));
    assert!(refused(&team(AB, "hosts:\n  codex: {}\n")).contains("needs a tool_map"));
}

// ---- presets ----

const PRESETS: &str = "presets:\n  full:\n    active: {}\n  lean:\n    active: {b: false, c: true}\n  split:\n    hosts: {b: codex}\nhosts:\n  claude:\n    tool_map: {Task: Task}\n  codex:\n    tool_map: {Task: spawn_agent}\n";
const ABC: &str = "  - agent: a\n  - agent: b\n  - agent: c\n    active: false\n    phase: 2\n";

fn with_preset(l: &Lab, preset: &str, hosts: &[Host]) -> AgentInitOptions {
    AgentInitOptions { preset: Some(preset.into()), ..opts(l, hosts) }
}

#[test]
fn a_preset_overrides_active_and_the_lock_records_it() {
    let l = lab(&team(ABC, PRESETS));
    let none = plan(&opts(&l, &[Host::Claude])).unwrap();
    assert_eq!(none.selected, vec!["a", "b"]);
    let p = plan(&with_preset(&l, "lean", &[Host::Claude])).unwrap();
    assert_eq!(p.selected, vec!["a", "c"]);
    assert_eq!(slots_of(&p), vec![("a".to_string(), true), ("b".to_string(), false), ("c".to_string(), true)]);
}

#[test]
fn a_preset_puts_a_member_on_one_host() {
    let l = lab(&team(ABC, PRESETS));
    let p = plan(&with_preset(&l, "split", &[Host::Claude, Host::Codex])).unwrap();
    assert!(p.items.iter().any(|i| i.path == ".codex/agents/b.toml"));
    assert!(!p.items.iter().any(|i| i.path == ".claude/agents/b.md"));
    assert!(p.items.iter().any(|i| i.path == ".claude/agents/a.md"));
    let e = plan(&with_preset(&l, "split", &[Host::Claude])).unwrap_err().to_string();
    assert!(e.contains("which this run does not render"), "{e}");
}

#[test]
fn an_unknown_preset_or_a_preset_without_a_team_is_refused() {
    let l = lab(&team(ABC, PRESETS));
    let e = plan(&with_preset(&l, "nope", &ALL)).unwrap_err().to_string();
    assert!(e.contains("no preset `nope`") && e.contains("lean"), "{e}");
    let o = AgentInitOptions { agents: vec!["a".into()], ..with_preset(&l, "lean", &ALL) };
    assert!(plan(&o).unwrap_err().to_string().contains("--preset lean needs a team"));
    assert!(refused(&team(AB, "presets:\n  p:\n    colour: x\n")).contains("unknown field `colour`"));
    assert!(refused(&team(AB, "presets:\n  p:\n    active: {zed: true}\n")).contains("unknown member `zed`"));
    assert!(refused(&team(AB, "presets:\n  p:\n    hosts: {a: codex}\n")).contains("not a declared host"));
}

fn apply_with(l: &Lab, preset: Option<&str>, clear: bool) -> Result<super::super::team::Applied, super::super::team::TeamError> {
    apply_request(&ApplyRequest {
        root: l.project.clone(),
        subject: Subject::Team,
        name: "t".into(),
        hosts: vec![Host::Claude],
        from: Some(l.agents.clone()),
        pin: None,
        dry_run: false,
        preset: preset.map(str::to_string),
        clear_preset: clear,
    })
}

fn has(l: &Lab, id: &str) -> bool {
    l.project.join(format!(".claude/agents/{id}.md")).exists()
}

fn recorded(l: &Lab) -> Option<String> {
    let st = crate::init::team::status(&l.project, &Some(l.agents.clone())).unwrap();
    st.teams[0].preset.clone()
}

#[test]
fn a_re_apply_reuses_the_recorded_preset() {
    let l = lab(&team(ABC, PRESETS));
    apply_with(&l, Some("lean"), false).unwrap();
    assert_eq!(recorded(&l).as_deref(), Some("lean"));
    assert!(has(&l, "a") && has(&l, "c") && !has(&l, "b"));
    let again = apply_with(&l, None, false).unwrap();
    assert!(again.already, "re-applying with no flag must change nothing");
    assert!(!has(&l, "b"), "the preset's deactivated member must stay out");
}

#[test]
fn an_explicit_preset_overrides_the_recorded_one() {
    let l = lab(&team(ABC, PRESETS));
    apply_with(&l, Some("lean"), false).unwrap();
    apply_with(&l, Some("full"), false).unwrap();
    assert_eq!(recorded(&l).as_deref(), Some("full"));
    assert!(has(&l, "b"));
}

#[test]
fn no_preset_clears_the_recorded_one() {
    let l = lab(&team(ABC, PRESETS));
    apply_with(&l, Some("lean"), false).unwrap();
    apply_with(&l, None, true).unwrap();
    assert_eq!(recorded(&l), None);
    assert!(has(&l, "b"));
    apply_with(&l, None, false).unwrap();
    assert_eq!(recorded(&l), None, "once cleared, a plain re-apply stays cleared");
}

#[test]
fn preset_and_no_preset_together_are_refused() {
    let l = lab(&team(ABC, PRESETS));
    let o = AgentInitOptions { clear_preset: true, ..with_preset(&l, "lean", &ALL) };
    assert!(plan(&o).unwrap_err().to_string().contains("--preset and --no-preset"));
}

// ---- legacy teams, apply and uninstall ----

#[test]
fn a_legacy_team_keeps_working_and_names_the_fields_it_ignores() {
    let l = lab("name: t\nmodel_hint: x\nmembers:\n  - agent: a\n    authority: boss\n");
    let p = plan(&opts(&l, &ALL)).unwrap();
    let w = p.warnings.iter().find(|w| w.contains("no `schema: 1`")).expect("a warning");
    assert!(w.contains("model_hint") && w.contains("members[a].authority"), "{w}");
    assert!(!text(&p, ".claude/agents/a.md").contains("Team charter"));
}

#[test]
fn the_charter_survives_a_second_apply_and_a_round_trip_uninstall() {
    let l = lab(&team(AB, "write_authority:\n  board: a\n"));
    let o = AgentInitOptions { apply: true, ..opts(&l, &ALL) };
    let p = plan(&o).unwrap();
    apply(&p).unwrap();
    assert!(plan(&o).unwrap().is_noop(), "the second apply must write nothing");
    let u = uninstall(&l.project, Some("t"), &Some(l.agents.clone()), false).unwrap();
    assert_eq!(u.removed, vec!["a", "b"]);
    assert!(!l.project.join(".claude/agents/a.md").exists());
}

#[test]
fn removing_one_team_re_renders_an_agent_another_team_still_uses() {
    let l = lab(&team(AB, "team_rules:\n  - {name: r, statement: First team rule.}\n"));
    put(&l.agents, "teams/u/team.yaml", "schema: 1\nname: u\nteam_rules:\n  - {name: r2, statement: Second team rule.}\nmembers:\n  - agent: a\n");
    let req = |t: &str| ApplyRequest {
        root: l.project.clone(),
        subject: Subject::Team,
        name: t.into(),
        hosts: vec![Host::Claude],
        from: Some(l.agents.clone()),
        pin: None,
        dry_run: false,
        preset: None,
        clear_preset: false,
    };
    apply_request(&req("t")).unwrap();
    apply_request(&req("u")).unwrap();
    let both = fs::read_to_string(l.project.join(".claude/agents/a.md")).unwrap();
    assert!(both.contains("First team rule.") && both.contains("Second team rule."), "{both}");
    uninstall(&l.project, Some("t"), &Some(l.agents.clone()), false).expect("a shared agent must not block the uninstall");
    let left = fs::read_to_string(l.project.join(".claude/agents/a.md")).unwrap();
    assert!(!left.contains("First team rule.") && left.contains("Second team rule."), "{left}");
    assert!(!l.project.join(".claude/agents/b.md").exists());
}

#[test]
fn two_teams_that_disagree_on_a_model_tier_are_refused() {
    let l = lab(&team("  - agent: a\n    model_tier: top\n", ""));
    put(&l.agents, "teams/u/team.yaml", "schema: 1\nname: u\nmembers:\n  - agent: a\n    model_tier: small\n");
    let req = |t: &str| ApplyRequest {
        root: l.project.clone(),
        subject: Subject::Team,
        name: t.into(),
        hosts: vec![Host::Claude],
        from: Some(l.agents.clone()),
        pin: None,
        dry_run: false,
        preset: None,
        clear_preset: false,
    };
    apply_request(&req("t")).unwrap();
    let e = apply_request(&req("u")).unwrap_err();
    assert!(e.message.contains("different model tiers") || e.message.contains("planning"), "{}", e.message);
}

#[test]
fn the_embedded_weftos_core_team_applies_on_every_host() {
    let tmp = tempfile::tempdir().unwrap();
    let o = AgentInitOptions { root: tmp.path().to_path_buf(), hosts: ALL.to_vec(), ..Default::default() };
    let p = plan(&o).expect("the team that ships must pass its own strict read");
    let steward = text(&p, ".claude/agents/steward.md");
    assert!(steward.contains("## Team charter: weftos-core"), "{steward}");
    assert!(steward.contains("- board: steward (you)") && steward.contains("model: opus\n"));
    assert!(text(&p, ".codex/agents/steward.toml").contains("Task is `spawn_agent`"));
    assert!(p.warnings.iter().any(|w| w.contains("not applied on codex")));
}
