//! Agent directory: one view that joins declared, running and exposed agents
//! (design note `docs/grok-build-fork/agent-directory-design.md`).
//!
//! - declared: `agents/catalog.json` (generated from the `agents/<id>/`
//!   packages by `scripts/agents-catalog.mjs`),
//! - running: weave `agent.list`,
//! - exposed: weave `expose.list` rows of kind `harness` (ADR-120 section 3).
//!
//! The key is `weftos://agent/<id>`: no pid, no node, so it survives a daemon
//! restart. A source that cannot be read is named in `sources` with its
//! error; it is never reported as an empty list, and an entry whose status
//! depends on it says `unknown` instead of guessing.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

/// URI prefix of every directory key.
pub const URI_PREFIX: &str = "weftos://agent/";
/// Exposure id prefix for harness exposures that name an agent.
const HARNESS_PREFIX: &str = "harness.";
const MAX_ID_LEN: usize = 128;

/// One source's outcome: its rows, or why it could not be read.
pub type Source<T> = Result<T, String>;

/// Percent-encode everything outside `[A-Za-z0-9._-]`, so any agent id string
/// (running agents choose their own) becomes a safe URI segment.
pub fn encode_id(id: &str) -> String {
    let mut out = String::with_capacity(id.len());
    for b in id.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Directory key for an agent id.
pub fn uri_for(id: &str) -> String {
    format!("{URI_PREFIX}{}", encode_id(id))
}

/// Accept `weftos://agent/<id>` or a bare id; refuse anything else (empty,
/// over-long, another scheme or kind).
pub fn normalize_key(input: &str) -> Result<String, String> {
    let t = input.trim();
    if t.is_empty() {
        return Err("agent id is empty".into());
    }
    if t.len() > MAX_ID_LEN * 3 + URI_PREFIX.len() {
        return Err("agent id is too long".into());
    }
    if t.contains("://") {
        let rest = t
            .strip_prefix(URI_PREFIX)
            .ok_or_else(|| format!("not an agent URI (expected {URI_PREFIX}<id>): {t}"))?;
        if rest.is_empty() || rest.contains('/') {
            return Err(format!("malformed agent URI: {t}"));
        }
        return Ok(format!("{URI_PREFIX}{rest}"));
    }
    if t.contains('/') {
        return Err(format!("malformed agent id: {t}"));
    }
    Ok(uri_for(t))
}

/// The declared agents and where they were read from.
pub struct Declared {
    /// One row per agent package, or why they could not be read.
    pub rows: Source<Vec<Value>>,
    /// The teams (not agents), or why they could not be read.
    pub teams: Source<Vec<Value>>,
    /// `{origin: "disk"|"embedded", location, commit}`.
    pub origin: Value,
}

fn has_packages(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .map(|rd| rd.flatten().any(|e| e.path().join("weftos-package.yaml").is_file()))
        .unwrap_or(false)
}

/// Where the checkout's `agents/` package tree is: `$WEFTOS_AGENTS_DIR` (which
/// must be usable: a bad value is an error, never a silent fallback), else the
/// nearest `agents/` at or above `start` that holds a package. `Ok(None)`
/// means "no checkout here", and the caller uses the embedded tree.
pub fn locate_agents_dir(start: &Path, env_dir: Option<&str>) -> Source<Option<PathBuf>> {
    if let Some(d) = env_dir.filter(|d| !d.is_empty()) {
        let p = PathBuf::from(d);
        return if has_packages(&p) {
            Ok(Some(p))
        } else {
            Err(format!("WEFTOS_AGENTS_DIR {}: no agent packages (dirs with weftos-package.yaml)", p.display()))
        };
    }
    Ok(start
        .ancestors()
        .map(|d| d.join("agents"))
        .find(|a| has_packages(a)))
}

/// Declared agents from the package tree, through the one loader the repo has
/// (`weftos::init::source::AgentSource`): the checkout's `agents/` if there is
/// one, else the tree embedded in this binary.
pub fn load_declared(start: &Path, env_dir: Option<&str>) -> Declared {
    use weftos::init::source::AgentSource;
    let located = locate_agents_dir(start, env_dir);
    let (src, origin) = match located {
        Err(e) => return Declared { rows: Err(e.clone()), teams: Err(e), origin: json!({"origin": null}) },
        Ok(Some(dir)) => match AgentSource::from_dir(&dir) {
            Ok(s) => {
                let o = json!({"origin": "disk", "location": dir.display().to_string(), "commit": s.commit});
                (s, o)
            }
            Err(e) => {
                let e = format!("{}: {e}", dir.display());
                return Declared {
                    rows: Err(e.clone()),
                    teams: Err(e),
                    origin: json!({"origin": "disk", "location": dir.display().to_string()}),
                };
            }
        },
        Ok(None) => {
            let s = AgentSource::embedded();
            let o = json!({"origin": "embedded", "location": "built into this binary", "commit": s.commit});
            (s, o)
        }
    };
    let teams = teams_of(&src);
    let rows = rows_of(&src, teams.as_ref().ok());
    let mut origin = origin;
    origin["non_agent_packages"] = json!(non_agent_ids(&src));
    Declared { rows, teams, origin }
}

/// Every team in the tree, as declared in its team.yaml. Teams are not
/// agents: they are a separate section of the directory. Fields `weftos init`
/// ignores sit under `declared_not_enforced`.
pub fn teams_of(src: &weftos::init::source::AgentSource) -> Source<Vec<Value>> {
    use weftos::init::package::team_decl;
    let mut out = Vec::new();
    for t in src.team_ids() {
        let d = team_decl(src, &t)
            .map_err(|e| format!("team {t}: {e}"))?
            .ok_or_else(|| format!("team {t}: team.yaml vanished"))?;
        out.push(json!({
            "name": d.name,
            "schema": d.schema,
            "description": d.description,
            "lead": d.lead,
            "shared_skills": d.shared_skills,
            "members": d.members.iter().map(|m| json!({
                "agent": m.agent, "active": m.active, "pinned": m.pinned,
                "nickname": m.nickname, "position": m.position, "phase": m.phase,
            })).collect::<Vec<_>>(),
            "declared_not_enforced": d.declared_unenforced,
        }));
    }
    Ok(out)
}

/// Package ids of kind `skill` or `template` (not agents).
fn non_agent_ids(src: &weftos::init::source::AgentSource) -> Vec<String> {
    src.all_package_ids()
        .into_iter()
        .filter(|id| {
            let v: serde_yaml::Value = serde_yaml::from_slice(src.get(&format!("{id}/weftos-package.yaml")).unwrap_or_default())
                .unwrap_or(serde_yaml::Value::Null);
            matches!(v.get("kind").and_then(serde_yaml::Value::as_str), Some("skill" | "template"))
        })
        .collect()
}

/// One row per agent package: packages of kind `skill` or `template` are not
/// agents and are left out (`sources.declared.non_agent_packages` names them).
/// Inactive packages are kept, flagged by `state`. An empty tree is an error:
/// an embedded tree is empty only when the binary was built outside the repo.
pub fn rows_of(src: &weftos::init::source::AgentSource, teams: Option<&Vec<Value>>) -> Source<Vec<Value>> {
    use weftos::init::package::{package_skills, team_decl};
    let mut rows = Vec::new();
    let ids = src.all_package_ids();
    if ids.is_empty() {
        return Err(format!("{}: the package tree holds no agents", src.origin));
    }
    for id in ids {
        let path = format!("{id}/weftos-package.yaml");
        let m: serde_yaml::Value = serde_yaml::from_slice(src.get(&path).unwrap_or_default())
            .map_err(|e| format!("{} ({path}): not valid YAML: {e}", src.origin))?;
        let field = |k: &str| m.get(k).and_then(serde_yaml::Value::as_str).map(str::to_string);
        let kind = field("kind");
        if matches!(kind.as_deref(), Some("skill" | "template")) {
            continue;
        }
        let skills = package_skills(src, &id).map_err(|e| format!("{id}: {e}"))?;
        let mut memberships = Vec::new();
        for t in teams.into_iter().flatten() {
            let name = t["name"].as_str().unwrap_or_default();
            // Re-read the member's own declared data (init ignores it).
            let Some(d) = team_decl(src, name).ok().flatten() else { continue };
            for mem in d.members.iter().filter(|m| m.agent == id) {
                memberships.push(json!({
                    "team": name,
                    "lead": d.lead.as_deref() == Some(id.as_str()),
                    "active": mem.active,
                    "phase": mem.phase,
                    "position": mem.position,
                    "nickname": mem.nickname,
                    "pinned": mem.pinned,
                    "shared_skills": d.shared_skills,
                    "declared_not_enforced": mem.rest,
                }));
            }
        }
        rows.push(json!({
            "id": id,
            "version": field("version"),
            "kind": kind,
            "trust_tier": field("trust_tier"),
            "state": field("state").unwrap_or_else(|| "active".into()),
            "path": format!("agents/{id}"),
            "skills": {"own": skills.own, "requires": skills.requires},
            "teams": memberships,
        }));
    }
    if rows.is_empty() {
        return Err(format!("{}: the package tree holds no agents", src.origin));
    }
    Ok(rows)
}

#[derive(Default)]
struct Acc {
    name: Option<String>,
    declared: Option<Value>,
    running: Vec<Value>,
    exposed: Vec<Value>,
}

fn str_of<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str)
}

/// One node's agents and exposures. `node` is `None` for the local-only view
/// (rows then carry no node field).
pub struct NodeInput {
    pub node: Option<String>,
    pub running: Source<Vec<Value>>,
    pub exposed: Source<Value>,
}

impl NodeInput {
    /// Build from one block of the daemon's `agent.sources` answer. A block
    /// with `ok: false` makes both of its sources unreadable.
    pub fn from_block(b: &Value) -> Self {
        let node = b.get("node_id").and_then(Value::as_str).map(str::to_string);
        if b.get("ok").and_then(Value::as_bool) != Some(true) {
            let e = b.get("error").and_then(Value::as_str).unwrap_or("no answer").to_string();
            return Self { node, running: Err(e.clone()), exposed: Err(e) };
        }
        let running = match b.get("agents") {
            Some(Value::Array(a)) => Ok(a.clone()),
            _ => Err("answer has no agents array".to_string()),
        };
        let exposed = match b.get("exposures") {
            Some(d) if d.is_object() => Ok(d.clone()),
            _ => Err(b
                .get("exposures_error")
                .and_then(Value::as_str)
                .unwrap_or("answer has no exposures")
                .to_string()),
        };
        Self { node, running, exposed }
    }
}

fn node_label(n: &Option<String>) -> &str {
    n.as_deref().unwrap_or("local")
}

fn tag(mut row: Value, node: &Option<String>) -> Value {
    if let Some(n) = node {
        row["node"] = json!(n);
    }
    row
}

/// The agent key an exposure row belongs to: its explicit `agent` field
/// (ADR-120), else, for older exposures without one, `harness.<id>` of a
/// harness exposure. `Ok(None)`: not an agent exposure. `Err`: a malformed
/// `agent` field (reported in `warnings`, never joined by guesswork).
fn exposure_agent(r: &Value) -> Result<Option<(String, &'static str)>, String> {
    match r.get("agent") {
        Some(Value::String(a)) => return normalize_key(a).map(|k| Some((k, "field"))),
        Some(Value::Null) | None => {}
        Some(other) => return Err(format!("agent must be a string, got {other}")),
    }
    let id = str_of(r, "id").unwrap_or_default();
    if str_of(r, "kind") == Some("harness")
        && let Some(a) = id.strip_prefix(HARNESS_PREFIX).filter(|s| !s.is_empty())
    {
        return Ok(Some((uri_for(a), "harness-id-fallback")));
    }
    Ok(None)
}

/// Join declared agents with every node's running and exposed rows. Entries
/// are sorted by key.
pub fn join(declared: &Declared, nodes: &[NodeInput]) -> Value {
    let mut map: BTreeMap<String, Acc> = BTreeMap::new();
    let mut warnings: Vec<String> = Vec::new();

    if let Ok(rows) = &declared.rows {
        for r in rows {
            let Some(id) = str_of(r, "id") else { continue };
            let a = map.entry(uri_for(id)).or_default();
            a.name = Some(id.to_string());
            a.declared = Some(r.clone());
        }
    }
    for n in nodes {
        if let Ok(rows) = &n.running {
            for r in rows {
                let Some(id) = str_of(r, "agent_id") else { continue };
                let a = map.entry(uri_for(id)).or_default();
                a.name.get_or_insert_with(|| id.to_string());
                a.running.push(tag(
                    json!({"pid": r.get("pid"), "state": r.get("state"), "parent_pid": r.get("parent_pid")}),
                    &n.node,
                ));
            }
        }
        if let Ok(doc) = &n.exposed {
            let rows = doc.get("exposures").and_then(Value::as_array).cloned().unwrap_or_default();
            for r in rows {
                let (key, how) = match exposure_agent(&r) {
                    Ok(Some(k)) => k,
                    Ok(None) => continue,
                    Err(e) => {
                        warnings.push(format!(
                            "exposure {} on {}: {e}",
                            str_of(&r, "id").unwrap_or("?"),
                            node_label(&n.node)
                        ));
                        continue;
                    }
                };
                let a = map.entry(key.clone()).or_default();
                a.name.get_or_insert_with(|| key.trim_start_matches(URI_PREFIX).to_string());
                a.exposed.push(tag(
                    json!({
                        "exposure_id": r.get("id"),
                        "linked_by": how,
                        "state": r.get("state"),
                        "audience": r.get("audience"),
                        "announced": r.get("announced"),
                        "served": r.get("served"),
                        "refused": r.get("refused"),
                    }),
                    &n.node,
                ));
            }
        }
    }

    let unreadable: Vec<&Option<String>> = nodes.iter().filter(|n| n.running.is_err()).map(|n| &n.node).collect();
    let agents: Vec<Value> = map
        .into_iter()
        .map(|(key, a)| entry(key, a, declared.rows.is_ok(), &unreadable))
        .collect();

    let counted = |s: Result<usize, &String>| match s {
        Ok(n) => json!({"ok": true, "count": n}),
        Err(e) => json!({"ok": false, "error": e}),
    };
    let node_sources: Vec<Value> = nodes
        .iter()
        .map(|n| {
            let exposed_n = n.exposed.as_ref().map(|d| {
                d.get("exposures").and_then(Value::as_array).map_or(0, |a| {
                    a.iter().filter(|r| matches!(exposure_agent(r), Ok(Some(_)))).count()
                })
            });
            json!({
                "node": n.node,
                "ok": n.running.is_ok() && n.exposed.is_ok(),
                "running": counted(n.running.as_ref().map(Vec::len)),
                "exposed": counted(exposed_n),
            })
        })
        .collect();
    let mut declared_src = counted(declared.rows.as_ref().map(Vec::len));
    if let (Some(o), Some(d)) = (declared.origin.as_object(), declared_src.as_object_mut()) {
        for (k, v) in o {
            d.insert(k.clone(), v.clone());
        }
    }
    if let Some(d) = declared_src.as_object_mut() {
        d.insert("teams".into(), counted(declared.teams.as_ref().map(Vec::len)));
    }
    json!({
        "count": agents.len(),
        "agents": agents,
        // Teams are not agents; each agent's entry carries its own memberships.
        "teams": declared.teams.as_ref().ok().cloned().unwrap_or_default(),
        "warnings": warnings,
        "sources": { "declared": declared_src, "nodes": node_sources },
    })
}

fn entry(key: String, a: Acc, declared_ok: bool, unreadable: &[&Option<String>]) -> Value {
    let mut from = Vec::new();
    if a.declared.is_some() {
        from.push("declared");
    }
    if !a.running.is_empty() {
        from.push("running");
    }
    if !a.exposed.is_empty() {
        from.push("exposed");
    }
    let inactive = a.declared.as_ref().and_then(|d| str_of(d, "state")) == Some("inactive");
    let on_node = |rows: &[Value], node: Option<&str>| rows.iter().any(|r| str_of(r, "node") == node);
    let dark: Vec<&str> = unreadable.iter().map(|n| node_label(n)).collect();

    let mut notes: Vec<String> = Vec::new();
    if declared_ok && a.declared.is_none() {
        notes.push("undeclared: not in the package tree".into());
    }
    if !a.running.is_empty() && inactive {
        notes.push("inactive_but_running: the package says inactive".into());
    }
    for e in a.exposed.iter().filter(|e| str_of(e, "state") == Some("on")) {
        let node = str_of(e, "node");
        let node_readable = !dark.contains(&node.unwrap_or("local"));
        if node_readable && !on_node(&a.running, node) {
            notes.push(format!(
                "exposed_not_running: {} is on at {} but no instance runs there",
                str_of(e, "exposure_id").unwrap_or("?"),
                node.unwrap_or("local")
            ));
        }
    }
    if !dark.is_empty() {
        notes.push(format!("running_unknown: could not read agents from {}", dark.join(", ")));
    }

    let status = if !a.running.is_empty() {
        "running"
    } else if !dark.is_empty() {
        "unknown"
    } else if !a.exposed.is_empty() {
        "stopped"
    } else if inactive {
        "inactive"
    } else {
        "declared"
    };

    json!({
        "id": key,
        "name": a.name,
        "status": status,
        "from": from,
        "declared": a.declared,
        "running": a.running,
        "exposed": a.exposed,
        "notes": notes,
    })
}

/// Look one entry up in a joined directory.
pub fn get(dir: &Value, key: &str) -> Result<Value, String> {
    let key = normalize_key(key)?;
    dir["agents"]
        .as_array()
        .and_then(|a| a.iter().find(|e| e["id"] == key.as_str()))
        .map(|e| {
            let mut e = e.clone();
            e["sources"] = dir["sources"].clone();
            e
        })
        .ok_or_else(|| format!("no agent {key} in the directory (declared, running or exposed)"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decl(rows: &[(&str, &str)]) -> Declared {
        Declared {
            rows: Ok(rows.iter().map(|(i, s)| json!({"id": i, "version": "1", "state": s})).collect()),
            teams: Ok(vec![]),
            origin: json!({"origin": "disk", "location": "t"}),
        }
    }
    fn run(rows: &[(u64, &str)]) -> Source<Vec<Value>> {
        Ok(rows.iter().map(|(p, i)| json!({"pid": p, "agent_id": i, "state": "running"})).collect())
    }
    /// (exposure id, kind, state, agent field)
    fn exp(rows: &[(&str, &str, &str, Option<&str>)]) -> Source<Value> {
        Ok(json!({"exposures": rows.iter().map(|(id, kind, st, ag)| {
            let mut v = json!({"id": id, "kind": kind, "state": st});
            if let Some(a) = ag { v["agent"] = json!(a); }
            v
        }).collect::<Vec<_>>()}))
    }
    fn local(running: Source<Vec<Value>>, exposed: Source<Value>) -> Vec<NodeInput> {
        vec![NodeInput { node: None, running, exposed }]
    }
    fn node(n: &str, running: Source<Vec<Value>>, exposed: Source<Value>) -> NodeInput {
        NodeInput { node: Some(n.into()), running, exposed }
    }
    fn find<'a>(d: &'a Value, id: &str) -> &'a Value {
        d["agents"].as_array().unwrap().iter().find(|e| e["id"] == uri_for(id).as_str()).unwrap()
    }

    #[test]
    fn same_entry_before_and_after_restart() {
        let c = decl(&[("developer", "active")]);
        let before = join(&c, &local(run(&[(10, "developer")]), exp(&[])));
        let after = join(&c, &local(run(&[(3, "developer")]), exp(&[])));
        assert_eq!(find(&before, "developer")["id"], find(&after, "developer")["id"]);
        assert_eq!(find(&before, "developer")["id"], "weftos://agent/developer");
        assert_eq!(find(&after, "developer")["running"][0]["pid"], 3);
    }

    #[test]
    fn declared_not_running() {
        let d = join(&decl(&[("a", "active"), ("b", "inactive")]), &local(run(&[]), exp(&[])));
        assert_eq!(find(&d, "a")["status"], "declared");
        assert_eq!(find(&d, "b")["status"], "inactive");
        assert_eq!(find(&d, "a")["from"], json!(["declared"]));
    }

    #[test]
    fn running_but_undeclared_is_flagged() {
        let d = join(&decl(&[]), &local(run(&[(1, "stray"), (2, "stray")]), exp(&[])));
        let e = find(&d, "stray");
        assert_eq!(e["status"], "running");
        assert_eq!(e["running"].as_array().unwrap().len(), 2);
        assert!(e["notes"][0].as_str().unwrap().starts_with("undeclared"));
    }

    #[test]
    fn exposure_joins_on_its_agent_field() {
        let rows = [("harness.team-run", "harness", "on", Some("weftos://agent/runner"))];
        let d = join(&decl(&[("runner", "active")]), &local(run(&[]), exp(&rows)));
        let e = find(&d, "runner");
        assert_eq!(e["status"], "stopped");
        assert_eq!(e["from"], json!(["declared", "exposed"]));
        assert_eq!(e["exposed"][0]["linked_by"], "field");
        assert!(e["notes"].to_string().contains("exposed_not_running"));
        assert!(d["agents"].as_array().unwrap().iter().all(|a| a["id"] != "weftos://agent/team-run"));
    }

    #[test]
    fn the_field_wins_over_the_harness_id() {
        let rows = [("harness.other", "harness", "on", Some("runner"))];
        let d = join(&decl(&[("runner", "active")]), &local(run(&[]), exp(&rows)));
        assert_eq!(d["count"], 1);
        assert_eq!(find(&d, "runner")["exposed"][0]["exposure_id"], "harness.other");
    }

    #[test]
    fn older_harness_exposures_fall_back_to_the_id_and_say_so() {
        let rows = [("harness.runner", "harness", "off", None)];
        let d = join(&decl(&[("runner", "active")]), &local(run(&[]), exp(&rows)));
        assert_eq!(find(&d, "runner")["exposed"][0]["linked_by"], "harness-id-fallback");
        assert_eq!(find(&d, "runner")["status"], "stopped");
    }

    #[test]
    fn nothing_else_links_an_exposure_to_an_agent() {
        let rows = [("infer.chat", "inference", "on", None), ("tts.x", "tts", "on", None), ("cog.runner.api", "cog-api", "on", None)];
        let d = join(&decl(&[("runner", "active")]), &local(run(&[]), exp(&rows)));
        assert_eq!(find(&d, "runner")["exposed"], json!([]));
        assert_eq!(d["count"], 1);
    }

    #[test]
    fn a_malformed_agent_field_is_a_warning_not_a_join() {
        let rows = [("harness.x", "harness", "on", Some("a/b"))];
        let d = join(&decl(&[]), &local(run(&[]), exp(&rows)));
        assert_eq!(d["count"], 0, "no fallback to harness.x when the field is present but bad");
        assert!(d["warnings"][0].as_str().unwrap().contains("harness.x"));
    }

    #[test]
    fn unreadable_local_source_is_named_and_status_is_unknown() {
        let d = join(&decl(&[("a", "active")]), &local(Err("daemon down".into()), exp(&[])));
        assert_eq!(d["sources"]["nodes"][0]["ok"], false);
        assert_eq!(d["sources"]["nodes"][0]["running"]["error"], "daemon down");
        assert_eq!(find(&d, "a")["status"], "unknown");
        let nod = Declared { rows: Err("no tree".into()), teams: Ok(vec![]), origin: json!({"origin": null}) };
        let d = join(&nod, &local(run(&[(1, "a")]), exp(&[])));
        assert_eq!(d["sources"]["declared"]["ok"], false);
        assert_eq!(find(&d, "a")["status"], "running");
        assert!(!find(&d, "a")["notes"].to_string().contains("undeclared"));
    }

    #[test]
    fn mesh_instances_carry_their_node_under_one_key() {
        let nodes = vec![
            node("n1", run(&[(5, "developer")]), exp(&[])),
            node("n2", run(&[(9, "developer")]), exp(&[("harness.d", "harness", "on", Some("developer"))])),
        ];
        let d = join(&decl(&[("developer", "active")]), &nodes);
        assert_eq!(d["count"], 1);
        let e = find(&d, "developer");
        assert_eq!(e["running"].as_array().unwrap().len(), 2);
        assert_eq!(e["running"][0]["node"], "n1");
        assert_eq!(e["running"][1]["node"], "n2");
        assert_eq!(e["exposed"][0]["node"], "n2");
        assert!(e["notes"].as_array().unwrap().is_empty(), "exposed at n2 where an instance runs");
    }

    #[test]
    fn exposure_on_a_node_without_an_instance_there_is_flagged() {
        let nodes = vec![
            node("n1", run(&[(5, "developer")]), exp(&[])),
            node("n2", run(&[]), exp(&[("harness.d", "harness", "on", Some("developer"))])),
        ];
        let d = join(&decl(&[("developer", "active")]), &nodes);
        assert!(find(&d, "developer")["notes"].to_string().contains("exposed_not_running"));
        assert_eq!(find(&d, "developer")["status"], "running");
    }

    #[test]
    fn an_unreachable_peer_is_named_and_does_not_fail_the_list() {
        let nodes = vec![
            node("n1", run(&[(5, "developer")]), exp(&[])),
            NodeInput::from_block(&json!({"node_id": "n2", "ok": false, "error": "timed out"})),
        ];
        let d = join(&decl(&[("developer", "active"), ("steward", "active")]), &nodes);
        assert_eq!(d["sources"]["nodes"][1]["node"], "n2");
        assert_eq!(d["sources"]["nodes"][1]["ok"], false);
        assert_eq!(d["sources"]["nodes"][1]["running"]["error"], "timed out");
        assert_eq!(find(&d, "developer")["status"], "running");
        // Not running anywhere we could see, but n2 is dark: unknown, not "declared".
        assert_eq!(find(&d, "steward")["status"], "unknown");
        assert!(find(&d, "steward")["notes"].to_string().contains("n2"));
    }

    #[test]
    fn from_block_reads_a_good_answer_and_refuses_a_bad_one() {
        let ok = NodeInput::from_block(&json!({"node_id": "n1", "ok": true, "agents": [], "exposures": {"exposures": []}}));
        assert!(ok.running.is_ok() && ok.exposed.is_ok());
        let no_reg = NodeInput::from_block(&json!({"node_id": "n1", "ok": true, "agents": [], "exposures": null, "exposures_error": "registry down"}));
        assert_eq!(no_reg.exposed.unwrap_err(), "registry down");
        let bad = NodeInput::from_block(&json!({"node_id": "n1", "ok": true}));
        assert!(bad.running.is_err());
    }

    #[test]
    fn odd_running_ids_are_encoded() {
        let d = join(&decl(&[]), &local(run(&[(1, "a b/c")]), exp(&[])));
        assert_eq!(d["agents"][0]["id"], "weftos://agent/a%20b%2Fc");
        assert!(get(&d, "a b/c").is_err(), "a bare id with a slash is malformed");
        assert!(get(&d, "weftos://agent/a%20b%2Fc").is_ok());
    }

    #[test]
    fn get_accepts_bare_and_uri_and_refuses_bad_input() {
        let d = join(&decl(&[("a", "active")]), &local(run(&[]), exp(&[])));
        assert_eq!(get(&d, "a").unwrap()["id"], "weftos://agent/a");
        assert_eq!(get(&d, "weftos://agent/a").unwrap()["id"], "weftos://agent/a");
        assert_eq!(get(&d, "a").unwrap()["sources"]["declared"]["ok"], true);
        for bad in ["", "  ", "http://x/a", "weftos://mesh.bot.x", "weftos://agent/", "weftos://agent/a/b"] {
            assert!(get(&d, bad).is_err(), "{bad:?} must be refused");
        }
        assert!(get(&d, "missing").unwrap_err().contains("no agent"));
    }

    fn write_pkg(root: &Path, id: &str, state: Option<&str>) {
        let d = root.join(id);
        std::fs::create_dir_all(&d).unwrap();
        let st = state.map(|s| format!("state: {s}\n")).unwrap_or_default();
        std::fs::write(d.join("weftos-package.yaml"), format!("name: {id}\nkind: lane\nversion: 1.2.3\n{st}")).unwrap();
    }

    #[test]
    fn declared_comes_from_the_package_tree_on_disk() {
        let t = tempfile::tempdir().unwrap();
        let agents = t.path().join("agents");
        write_pkg(&agents, "alpha", None);
        write_pkg(&agents, "beta", Some("inactive"));
        let deep = t.path().join("a/b");
        std::fs::create_dir_all(&deep).unwrap();
        let d = load_declared(&deep, None);
        let rows = d.rows.unwrap();
        assert_eq!(rows.len(), 2);
        let alpha = rows.iter().find(|r| r["id"] == "alpha").unwrap();
        assert_eq!(alpha["state"], "active");
        assert_eq!(alpha["kind"], "lane");
        assert_eq!(rows.iter().find(|r| r["id"] == "beta").unwrap()["state"], "inactive");
        assert_eq!(d.origin["origin"], "disk");
        assert!(d.origin["location"].as_str().unwrap().ends_with("agents"));
    }

    #[test]
    fn env_dir_is_used_and_a_bad_one_is_an_error_not_a_fallback() {
        let t = tempfile::tempdir().unwrap();
        write_pkg(t.path(), "gamma", None);
        let ok = load_declared(Path::new("/"), Some(t.path().to_str().unwrap()));
        assert_eq!(ok.rows.unwrap()[0]["id"], "gamma");
        let empty = tempfile::tempdir().unwrap();
        let bad = load_declared(t.path(), Some(empty.path().to_str().unwrap()));
        assert!(bad.rows.unwrap_err().contains("WEFTOS_AGENTS_DIR"));
        assert!(bad.origin["origin"].is_null(), "no silent switch to embedded");
    }

    #[test]
    fn outside_a_checkout_the_embedded_tree_is_used() {
        let t = tempfile::tempdir().unwrap();
        assert_eq!(locate_agents_dir(t.path(), None).unwrap(), None);
        let d = load_declared(t.path(), None);
        assert_eq!(d.origin["origin"], "embedded");
        let rows = d.rows.unwrap();
        assert!(rows.iter().any(|r| r["id"] == "developer"), "embedded tree is the repo's agents/");
    }

    #[test]
    fn broken_package_yaml_is_an_error() {
        let t = tempfile::tempdir().unwrap();
        let d = t.path().join("bad");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("weftos-package.yaml"), "name: [unclosed").unwrap();
        assert!(load_declared(Path::new("/"), Some(t.path().to_str().unwrap())).rows.unwrap_err().contains("not valid YAML"));
    }
}

#[cfg(test)]
mod team_tests {
    use super::*;
    use std::borrow::Cow;
    use weftos::init::source::AgentSource;

    fn src_of(files: &[(&str, &str)]) -> AgentSource {
        AgentSource {
            files: files.iter().map(|(p, c)| (p.to_string(), Cow::Owned(c.as_bytes().to_vec()))).collect(),
            origin: "test".into(),
            commit: "t".into(),
        }
    }

    fn tree() -> AgentSource {
        src_of(&[
            ("dev/weftos-package.yaml", "name: dev\nkind: lane\nrequires:\n  - skill: build-lane\n"),
            ("dev/skills/build-lane/SKILL.md", "x"),
            ("doc/weftos-package.yaml", "name: doc\nkind: lane\nstate: inactive\n"),
            ("doctrine/weftos-package.yaml", "name: doctrine\nkind: skill\n"),
            ("doctrine/SKILL.md", "x"),
            (
                "teams/core/team.yaml",
                "schema: 1\nname: core\nlead: null\nshared_skills: [doctrine]\nmembers:\n  - agent: dev\n    position: 1\n    model_tier: mid\n    authority: supervised\n  - agent: doc\n    position: 2\n    active: false\n    phase: 2\nedges:\n  - {from: dev, to: doc, kind: consults}\nwrite_authority: {code: dev}\n",
            ),
            ("teams/other/team.yaml", "schema: 1\nname: other\nmembers:\n  - agent: dev\n"),
        ])
    }

    #[test]
    fn an_entry_carries_its_package_skills_and_team_memberships() {
        let s = tree();
        let teams = teams_of(&s).unwrap();
        let rows = rows_of(&s, Some(&teams)).unwrap();
        let dev = rows.iter().find(|r| r["id"] == "dev").unwrap();
        assert_eq!(dev["skills"]["own"], json!(["build-lane"]));
        assert_eq!(dev["skills"]["requires"], json!(["build-lane"]));
        let ms = dev["teams"].as_array().unwrap();
        assert_eq!(ms.len(), 2);
        let core = ms.iter().find(|m| m["team"] == "core").unwrap();
        assert_eq!(core["active"], true);
        assert_eq!(core["lead"], false);
        assert_eq!(core["shared_skills"], json!(["doctrine"]));
        assert_eq!(core["declared_not_enforced"]["model_tier"], "mid");
        assert_eq!(core["declared_not_enforced"]["authority"], "supervised");
        let doc = rows.iter().find(|r| r["id"] == "doc").unwrap();
        assert_eq!(doc["state"], "inactive");
        assert_eq!(doc["teams"][0]["active"], false);
        assert_eq!(doc["teams"][0]["phase"], 2);
    }

    #[test]
    fn skill_packages_are_not_agents_and_teams_are_a_separate_section() {
        let s = tree();
        let teams = teams_of(&s).unwrap();
        let rows = rows_of(&s, Some(&teams)).unwrap();
        assert!(rows.iter().all(|r| r["id"] != "doctrine"));
        assert_eq!(non_agent_ids(&s), vec!["doctrine"]);
        let d = join(
            &Declared { rows: Ok(rows), teams: Ok(teams), origin: json!({"origin": "disk"}) },
            &[NodeInput { node: None, running: Ok(vec![]), exposed: Ok(json!({"exposures": []})) }],
        );
        assert_eq!(d["teams"].as_array().unwrap().len(), 2);
        assert!(d["agents"].as_array().unwrap().iter().all(|a| a["id"] != "weftos://agent/core"));
        assert_eq!(d["sources"]["declared"]["teams"]["count"], 2);
        let core = d["teams"].as_array().unwrap().iter().find(|t| t["name"] == "core").unwrap();
        assert_eq!(core["declared_not_enforced"]["edges"][0]["kind"], "consults");
        assert_eq!(core["declared_not_enforced"]["write_authority"]["code"], "dev");
        assert_eq!(core["members"][1]["agent"], "doc");
    }

    #[test]
    fn a_broken_team_file_is_reported_not_dropped() {
        let s = src_of(&[("a/weftos-package.yaml", "name: a\nkind: lane\n"), ("teams/t/team.yaml", "members: [")]);
        assert!(teams_of(&s).unwrap_err().contains("team t"));
        // The agent rows still read; the teams source says why it is missing.
        assert!(rows_of(&s, None).is_ok());
    }

    #[test]
    fn the_real_tree_has_weftos_core_with_chief_of_staff_and_inactive_members() {
        let s = AgentSource::embedded();
        let teams = teams_of(&s).unwrap();
        let core = teams.iter().find(|t| t["name"] == "weftos-core").unwrap();
        assert_eq!(core["schema"], 1);
        assert!(core["shared_skills"].as_array().unwrap().iter().any(|x| x == "lead-doctrine"));
        let rows = rows_of(&s, Some(&teams)).unwrap();
        assert!(rows.iter().all(|r| r["id"] != "lead-doctrine"));
        let cos = rows.iter().find(|r| r["id"] == "chief-of-staff").unwrap();
        assert_eq!(cos["teams"][0]["team"], "weftos-core");
        let documenter = rows.iter().find(|r| r["id"] == "documenter").unwrap();
        assert_eq!(documenter["teams"][0]["active"], false);
    }
}
