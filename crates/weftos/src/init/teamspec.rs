//! Strict reading of a `team.yaml` for `weftos init` and `weftos team apply`
//! (ADR-112, `agents/teams/SCHEMA.md`), and the part of it that lands in the
//! rendered agent files.
//!
//! A team with `schema: 1` is read strictly: an undeclared field, a field of
//! the wrong type or an unknown value is an error, never skipped. What each
//! field does here:
//!
//! - applied to the rendered agent: `tools` (Claude `tools:`, narrowing only),
//!   `model_tier` (Claude `model:`), and a "Team charter" section holding
//!   `nickname`, `one_job`, `authority`, `owner`, `tiers`, `must_not`, the
//!   member's `edges`, `write_authority`, `team_rules` and the host's
//!   `tool_map`;
//! - applied to the selection: `active`, `phase`, `lead`, `shared_skills`,
//!   `presets` (`--preset`), `position` (lock slot order), `agent@version`
//!   (the pin must equal the catalog version);
//! - checked and refused when wrong: `schema`, `name`, `version`, `description`.
//!
//! A legacy team (no `schema`) keeps the old tolerant read; the fields init
//! does not apply are listed in a warning.

use std::collections::{BTreeMap, BTreeSet};

use serde_yaml::Value;

use super::InitError;
use super::package::{Package, package_version};
use super::render::Host;
use super::source::AgentSource;

const TOP_KEYS: &[&str] = &[
    "schema", "name", "version", "description", "lead", "shared_skills", "skills", "members", "edges",
    "write_authority", "team_rules", "presets", "hosts",
];
const MEMBER_KEYS: &[&str] = &[
    "agent", "nickname", "position", "one_job", "model_tier", "tools", "authority", "owner", "must_not",
    "tiers", "active", "phase",
];
const LEGACY_TOP: &[&str] = &["schema", "name", "version", "description", "lead", "shared_skills", "skills", "members"];
const LEGACY_MEMBER: &[&str] = &["agent", "id", "package", "skill", "nickname", "position", "active", "phase"];
const AUTHORITIES: &[&str] = &["autonomous", "supervised", "escalated"];
const TIERS: &[&str] = &["public", "internal", "restricted"];
const EDGE_KINDS: &[&str] = &["consumes", "delegates_to", "consults"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelTier {
    Top,
    Mid,
    Small,
}

impl ModelTier {
    /// The Claude Code `model:` alias for the tier.
    pub fn claude_alias(self) -> &'static str {
        match self {
            ModelTier::Top => "opus",
            ModelTier::Mid => "sonnet",
            ModelTier::Small => "haiku",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Member {
    pub id: String,
    pub active: bool,
    pub phase: Option<u64>,
    pub nickname: Option<String>,
    pub one_job: Option<String>,
    pub model_tier: Option<ModelTier>,
    pub tools: Option<Vec<String>>,
    pub authority: Option<String>,
    pub owner: Option<String>,
    pub must_not: Vec<String>,
    pub tiers: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct Edge {
    pub from: String,
    pub to: String,
    pub kind: String,
}

#[derive(Clone, Debug, Default)]
pub struct Preset {
    pub active: BTreeMap<String, bool>,
    /// Member to the one host it is rendered for.
    pub hosts: BTreeMap<String, Host>,
}

#[derive(Clone, Debug)]
pub struct TeamSpec {
    pub name: String,
    /// No `schema`: only the old fields are read; `ignored` lists the rest.
    pub legacy: bool,
    pub ignored: Vec<String>,
    pub members: Vec<Member>,
    pub edges: Vec<Edge>,
    pub write_authority: BTreeMap<String, String>,
    pub rules: Vec<(String, String)>,
    pub presets: BTreeMap<String, Preset>,
    pub tool_maps: BTreeMap<Host, BTreeMap<String, String>>,
}

fn bad(path: &str, msg: impl AsRef<str>) -> InitError {
    InitError::Other(format!("{path}: {}", msg.as_ref()))
}

fn host_of(s: &str) -> Option<Host> {
    match s {
        "claude" => Some(Host::Claude),
        "grok" => Some(Host::Grok),
        "codex" => Some(Host::Codex),
        _ => None,
    }
}

fn keys(v: &Value) -> Vec<String> {
    match v {
        Value::Mapping(m) => m.keys().filter_map(|k| k.as_str().map(str::to_string)).collect(),
        _ => Vec::new(),
    }
}

/// Every key of mapping `v` must be in `allowed`.
fn only_keys(path: &str, what: &str, v: &Value, allowed: &[&str]) -> Result<(), InitError> {
    let Value::Mapping(m) = v else {
        return Err(bad(path, format!("{what} must be a mapping")));
    };
    for k in m.keys() {
        match k.as_str() {
            Some(s) if allowed.contains(&s) => {}
            Some(s) => return Err(bad(path, format!("unknown field `{s}` in {what} (declared: {})", allowed.join(", ")))),
            None => return Err(bad(path, format!("{what} has a non-string key"))),
        }
    }
    Ok(())
}

fn text(path: &str, what: &str, v: &Value) -> Result<String, InitError> {
    match v.as_str() {
        Some(s) if !s.trim().is_empty() => Ok(s.trim().to_string()),
        _ => Err(bad(path, format!("{what} must be a non-empty string"))),
    }
}

fn strings(path: &str, what: &str, v: &Value) -> Result<Vec<String>, InitError> {
    let Value::Sequence(items) = v else {
        return Err(bad(path, format!("{what} must be a list of strings")));
    };
    items.iter().map(|i| text(path, what, i)).collect()
}

/// Read `teams/<team>/team.yaml`. `None` when the team does not exist.
pub fn read(src: &AgentSource, team: &str) -> Result<Option<TeamSpec>, InitError> {
    let path = format!("teams/{team}/team.yaml");
    let Some(bytes) = src.get(&path) else { return Ok(None) };
    let v: Value = serde_yaml::from_slice(bytes).map_err(|e| bad(&path, e.to_string()))?;
    if !matches!(v, Value::Mapping(_)) {
        return Err(bad(&path, "a team file must be a mapping"));
    }
    if v.get("schema").is_none() {
        return Ok(Some(legacy(team, &v)));
    }
    if v.get("schema").and_then(Value::as_u64) != Some(1) {
        return Err(bad(&path, "schema must be 1 (the only team spec this weftos reads)"));
    }
    only_keys(&path, "the team", &v, TOP_KEYS)?;
    if let Some(n) = v.get("name")
        && text(&path, "name", n)? != team
    {
        return Err(bad(&path, format!("name must equal the team directory `{team}`")));
    }
    for k in ["version", "description"] {
        if let Some(x) = v.get(k) {
            text(&path, k, x)?;
        }
    }
    match v.get("lead") {
        None | Some(Value::Null) => {}
        Some(x) => {
            text(&path, "lead", x)?;
        }
    }
    for k in ["shared_skills", "skills"] {
        if let Some(x) = v.get(k) {
            strings(&path, k, x)?;
        }
    }

    let mut members: Vec<Member> = Vec::new();
    match v.get("members") {
        None => {}
        Some(Value::Sequence(items)) => {
            for item in items {
                members.push(member(src, &path, item)?);
            }
        }
        Some(_) => return Err(bad(&path, "members must be a list")),
    }
    let mut seen = BTreeSet::new();
    for m in &members {
        if !seen.insert(m.id.as_str()) {
            return Err(bad(&path, format!("member `{}` is listed twice", m.id)));
        }
    }
    let known = |id: &str| members.iter().any(|m| m.id == id);

    let mut edges = Vec::new();
    match v.get("edges") {
        None => {}
        Some(Value::Sequence(items)) => {
            for (i, e) in items.iter().enumerate() {
                only_keys(&path, &format!("edge #{i}"), e, &["from", "to", "kind"])?;
                let get = |k: &str| text(&path, &format!("edge #{i} {k}"), e.get(k).unwrap_or(&Value::Null));
                let (from, to, kind) = (get("from")?, get("to")?, get("kind")?);
                if !EDGE_KINDS.contains(&kind.as_str()) {
                    return Err(bad(&path, format!("edge #{i} kind `{kind}` is not one of {}", EDGE_KINDS.join(", "))));
                }
                for end in [&from, &to] {
                    if !known(end) {
                        return Err(bad(&path, format!("edge #{i} names `{end}`, which is not a member")));
                    }
                }
                if from == to {
                    return Err(bad(&path, format!("edge #{i} points `{from}` at itself")));
                }
                edges.push(Edge { from, to, kind });
            }
        }
        Some(_) => return Err(bad(&path, "edges must be a list")),
    }

    let mut write_authority = BTreeMap::new();
    match v.get("write_authority") {
        None => {}
        Some(Value::Mapping(m)) => {
            for (k, h) in m {
                let res = k.as_str().ok_or_else(|| bad(&path, "write_authority has a non-string resource"))?;
                let holder = text(&path, &format!("write_authority.{res} (exactly one member)"), h)?;
                if !known(&holder) {
                    return Err(bad(&path, format!("write_authority.{res} holder `{holder}` is not a member")));
                }
                write_authority.insert(res.to_string(), holder);
            }
        }
        Some(_) => return Err(bad(&path, "write_authority must map a resource to one member")),
    }

    let mut rules: Vec<(String, String)> = Vec::new();
    match v.get("team_rules") {
        None => {}
        Some(Value::Sequence(items)) => {
            for r in items {
                only_keys(&path, "a team_rules entry", r, &["name", "statement"])?;
                let name = text(&path, "team_rules name", r.get("name").unwrap_or(&Value::Null))?;
                let st = text(&path, &format!("team_rules `{name}` statement"), r.get("statement").unwrap_or(&Value::Null))?;
                if st.contains('\n') {
                    return Err(bad(&path, format!("team_rules `{name}` statement must be one line")));
                }
                if rules.iter().any(|(n, _)| *n == name) {
                    return Err(bad(&path, format!("team_rules `{name}` is declared twice")));
                }
                rules.push((name, st));
            }
        }
        Some(_) => return Err(bad(&path, "team_rules must be a list")),
    }

    let mut tool_maps = BTreeMap::new();
    match v.get("hosts") {
        None => {}
        Some(h @ Value::Mapping(m)) => {
            only_keys(&path, "hosts", h, &["claude", "codex", "grok"])?;
            for (k, body) in m {
                let name = k.as_str().unwrap_or_default();
                only_keys(&path, &format!("hosts.{name}"), body, &["tool_map"])?;
                let Some(Value::Mapping(tm)) = body.get("tool_map") else {
                    return Err(bad(&path, format!("hosts.{name} needs a tool_map mapping")));
                };
                let mut map = BTreeMap::new();
                for (tk, tv) in tm {
                    let tk = tk.as_str().ok_or_else(|| bad(&path, format!("hosts.{name}.tool_map has a non-string key")))?;
                    map.insert(tk.to_string(), text(&path, &format!("hosts.{name}.tool_map.{tk}"), tv)?);
                }
                tool_maps.insert(host_of(name).expect("checked by only_keys"), map);
            }
        }
        Some(_) => return Err(bad(&path, "hosts must be a mapping")),
    }

    let mut presets = BTreeMap::new();
    match v.get("presets") {
        None => {}
        Some(Value::Mapping(m)) => {
            for (k, body) in m {
                let name = k.as_str().ok_or_else(|| bad(&path, "a preset name must be a string"))?;
                let here = format!("preset `{name}`");
                if !matches!(body, Value::Mapping(_)) {
                    return Err(bad(&path, format!("{here} must be a mapping")));
                }
                only_keys(&path, &here, body, &["active", "hosts"])?;
                let mut p = Preset::default();
                if let Some(a) = body.get("active") {
                    let Value::Mapping(am) = a else { return Err(bad(&path, format!("{here} active must be a mapping"))) };
                    for (mk, mv) in am {
                        let mid = mk.as_str().unwrap_or_default();
                        if !known(mid) {
                            return Err(bad(&path, format!("{here} names unknown member `{mid}`")));
                        }
                        let on = mv.as_bool().ok_or_else(|| bad(&path, format!("{here} active.{mid} must be true or false")))?;
                        p.active.insert(mid.to_string(), on);
                    }
                }
                if let Some(h) = body.get("hosts") {
                    let Value::Mapping(hm) = h else { return Err(bad(&path, format!("{here} hosts must be a mapping"))) };
                    for (mk, mv) in hm {
                        let mid = mk.as_str().unwrap_or_default();
                        if !known(mid) {
                            return Err(bad(&path, format!("{here} names unknown member `{mid}`")));
                        }
                        let hname = text(&path, &format!("{here} hosts.{mid}"), mv)?;
                        let host = host_of(&hname)
                            .filter(|h| tool_maps.contains_key(h))
                            .ok_or_else(|| bad(&path, format!("{here} puts `{mid}` on `{hname}`, which is not a declared host")))?;
                        p.hosts.insert(mid.to_string(), host);
                    }
                }
                presets.insert(name.to_string(), p);
            }
        }
        Some(_) => return Err(bad(&path, "presets must be a mapping")),
    }

    Ok(Some(TeamSpec {
        name: team.to_string(),
        legacy: false,
        ignored: Vec::new(),
        members,
        edges,
        write_authority,
        rules,
        presets,
        tool_maps,
    }))
}

fn legacy(team: &str, v: &Value) -> TeamSpec {
    let mut ignored: Vec<String> =
        keys(v).into_iter().filter(|k| !LEGACY_TOP.contains(&k.as_str())).map(|k| k.to_string()).collect();
    if let Some(Value::Sequence(items)) = v.get("members") {
        for item in items {
            let who = item.get("agent").or_else(|| item.get("id")).and_then(Value::as_str).unwrap_or("?");
            for k in keys(item).into_iter().filter(|k| !LEGACY_MEMBER.contains(&k.as_str())) {
                ignored.push(format!("members[{who}].{k}"));
            }
        }
    }
    TeamSpec {
        name: team.to_string(),
        legacy: true,
        ignored,
        members: Vec::new(),
        edges: Vec::new(),
        write_authority: BTreeMap::new(),
        rules: Vec::new(),
        presets: BTreeMap::new(),
        tool_maps: BTreeMap::new(),
    }
}

fn member(src: &AgentSource, path: &str, item: &Value) -> Result<Member, InitError> {
    only_keys(path, "a member", item, MEMBER_KEYS)?;
    let full = text(path, "a member's `agent`", item.get("agent").unwrap_or(&Value::Null))?;
    let (id, pin) = match full.split_once('@') {
        Some((a, ver)) => (a.to_string(), Some(ver.to_string())),
        None => (full.clone(), None),
    };
    if id.is_empty() {
        return Err(bad(path, format!("member `{full}` has no package id")));
    }
    if let Some(pin) = pin {
        let catalog = package_version(src, &id).unwrap_or_else(|| crate::VERSION.to_string());
        if pin != catalog {
            return Err(bad(path, format!("member `{id}` pins @{pin} but the catalog version is {catalog}")));
        }
    }
    let ctx = |k: &str| format!("member `{id}` {k}");
    let opt_text = |k: &str| item.get(k).map(|x| text(path, &ctx(k), x)).transpose();
    let one_of = |k: &str, set: &[&str]| -> Result<Option<String>, InitError> {
        match opt_text(k)? {
            Some(s) if set.contains(&s.as_str()) => Ok(Some(s)),
            Some(s) => Err(bad(path, format!("{} `{s}` is not one of {}", ctx(k), set.join(", ")))),
            None => Ok(None),
        }
    };
    let model_tier = match one_of("model_tier", &["top", "mid", "small"])?.as_deref() {
        Some("top") => Some(ModelTier::Top),
        Some("mid") => Some(ModelTier::Mid),
        Some("small") => Some(ModelTier::Small),
        _ => None,
    };
    let list = |k: &str| item.get(k).map(|x| strings(path, &ctx(k), x)).transpose();
    let tiers = list("tiers")?.unwrap_or_default();
    if let Some(t) = tiers.iter().find(|t| !TIERS.contains(&t.as_str())) {
        return Err(bad(path, format!("{} `{t}` is not one of {}", ctx("tiers"), TIERS.join(", "))));
    }
    let uint = |k: &str| match item.get(k) {
        None => Ok(None),
        Some(x) => x.as_u64().map(Some).ok_or_else(|| bad(path, format!("{} must be a whole number", ctx(k)))),
    };
    uint("position")?;
    Ok(Member {
        id: id.clone(),
        active: match item.get("active") {
            None => true,
            Some(x) => x.as_bool().ok_or_else(|| bad(path, format!("{} must be true or false", ctx("active"))))?,
        },
        phase: uint("phase")?,
        nickname: opt_text("nickname")?,
        one_job: opt_text("one_job")?,
        model_tier,
        tools: list("tools")?,
        authority: one_of("authority", AUTHORITIES)?,
        owner: opt_text("owner")?,
        must_not: list("must_not")?.unwrap_or_default(),
        tiers,
    })
}

impl TeamSpec {
    pub fn member(&self, id: &str) -> Option<&Member> {
        self.members.iter().find(|m| m.id == id)
    }

    pub fn preset(&self, name: &str) -> Result<&Preset, InitError> {
        self.presets.get(name).ok_or_else(|| {
            let declared: Vec<&str> = self.presets.keys().map(String::as_str).collect();
            InitError::Other(format!(
                "team {} has no preset `{name}` (declared: {})",
                self.name,
                if declared.is_empty() { "none".into() } else { declared.join(", ") }
            ))
        })
    }

    /// The warning for a legacy team: what init reads and what it leaves.
    pub fn legacy_warning(&self) -> Option<String> {
        (self.legacy && !self.ignored.is_empty()).then(|| {
            format!(
                "team {}: no `schema: 1`, so these fields are not applied: {}",
                self.name,
                self.ignored.join(", ")
            )
        })
    }
}

/// The package with the teams' declarations applied for `host`, and whether a
/// `model_tier` was declared that this host cannot apply. Two teams that
/// disagree on `tools` or `model_tier` for one package are refused.
pub fn shape(specs: &[TeamSpec], mut pkg: Package, host: Host) -> Result<(Package, bool), InitError> {
    let Some(persona) = pkg.persona.as_mut() else { return Ok((pkg, false)) };
    let (mut tools, mut tier): (Option<(String, Vec<String>)>, Option<(String, ModelTier)>) = (None, None);
    let mut charter = String::new();
    for spec in specs.iter().filter(|s| !s.legacy) {
        let Some(m) = spec.member(&pkg.id) else { continue };
        if let Some(t) = &m.tools {
            let wider: Vec<&String> =
                t.iter().filter(|x| !persona.tools.is_empty() && !persona.tools.contains(x)).collect();
            if !wider.is_empty() {
                return Err(InitError::Other(format!(
                    "team {}: member `{}` tools widen the package (not in its AGENT.md tools): {}",
                    spec.name,
                    pkg.id,
                    wider.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
                )));
            }
            if host != Host::Claude {
                return Err(InitError::Other(format!(
                    "team {}: member `{}` narrows `tools`, which only the claude host can enforce; \
                     the {} agent file has no tool list. Remove `tools` or render claude only",
                    spec.name,
                    pkg.id,
                    host.as_str()
                )));
            }
            match &tools {
                Some((other, o)) if o != t => {
                    return Err(InitError::Other(format!(
                        "teams {other} and {} give `{}` different tools",
                        spec.name, pkg.id
                    )));
                }
                _ => tools = Some((spec.name.clone(), t.clone())),
            }
        }
        if let Some(t) = m.model_tier {
            match &tier {
                Some((other, o)) if *o != t => {
                    return Err(InitError::Other(format!(
                        "teams {other} and {} give `{}` different model tiers",
                        spec.name, pkg.id
                    )));
                }
                _ => tier = Some((spec.name.clone(), t)),
            }
        }
        charter.push_str(&charter_for(spec, m, host));
    }
    if let Some((_, t)) = tools {
        persona.tools = t;
    }
    let mut unapplied = false;
    if let Some((_, t)) = tier {
        if host == Host::Claude {
            persona.model = Some(t.claude_alias().to_string());
        } else {
            unapplied = true;
        }
    }
    if !charter.is_empty() {
        persona.body = format!("{}\n{charter}", persona.body.trim_end());
        persona.body.push('\n');
    }
    Ok((pkg, unapplied))
}

fn charter_for(spec: &TeamSpec, m: &Member, host: Host) -> String {
    let mut s = format!("\n## Team charter: {}\n\n", spec.name);
    if let Some(n) = &m.nickname {
        s.push_str(&format!("Known on this team as {n}.\n"));
    }
    if let Some(j) = &m.one_job {
        s.push_str(&format!("One job: {j}\n"));
    }
    if let Some(a) = &m.authority {
        s.push_str(&format!("Authority: {a}.\n"));
    }
    if let Some(o) = &m.owner {
        s.push_str(&format!("Owner role: {o}.\n"));
    }
    if !m.tiers.is_empty() {
        s.push_str(&format!("May read confidentiality tiers: {}.\n", m.tiers.join(", ")));
    }
    if !m.must_not.is_empty() {
        s.push_str("\nYou must not:\n");
        for x in &m.must_not {
            s.push_str(&format!("- {x}\n"));
        }
    }
    let (out, inc): (Vec<&Edge>, Vec<&Edge>) =
        spec.edges.iter().filter(|e| e.from == m.id || e.to == m.id).partition(|e| e.from == m.id);
    if !out.is_empty() || !inc.is_empty() {
        s.push_str("\nWorking relationships:\n");
        for e in out {
            s.push_str(&format!("- you {} {}\n", e.kind, e.to));
        }
        for e in inc {
            s.push_str(&format!("- {} {} you\n", e.from, e.kind));
        }
    }
    if !spec.write_authority.is_empty() {
        s.push_str("\nWrite authority (one holder per resource):\n");
        for (res, holder) in &spec.write_authority {
            let me = if *holder == m.id { " (you)" } else { "" };
            s.push_str(&format!("- {res}: {holder}{me}\n"));
        }
    }
    if !spec.rules.is_empty() {
        s.push_str("\nTeam rules:\n");
        for (name, st) in &spec.rules {
            s.push_str(&format!("- {name}: {st}\n"));
        }
    }
    if let Some(tm) = spec.tool_maps.get(&host) {
        s.push_str(&format!("\nTool names on {}:\n", host.as_str()));
        for (k, v) in tm {
            s.push_str(&format!("- {k} is `{v}`\n"));
        }
    }
    s
}

#[cfg(test)]
#[path = "teamspec_tests.rs"]
mod tests;
