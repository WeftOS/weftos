//! Parsed WeftOS agent packages (`agents/<id>/`) and team selection.

use std::collections::BTreeSet;

use serde_yaml::Value;

use super::InitError;
use super::source::AgentSource;

/// One file inside a skill directory, path relative to the skill root.
pub type SkillFile = (String, Vec<u8>);

pub struct Skill {
    pub name: String,
    pub description: String,
    /// Includes `SKILL.md`, `references/`, `scripts/`, … verbatim.
    pub files: Vec<SkillFile>,
}

/// The spawnable persona from `AGENT.md`.
pub struct Persona {
    pub description: String,
    pub tools: Vec<String>,
    pub nickname: Option<String>,
    pub body: String,
}

pub struct ContextKey {
    pub key: String,
    pub description: String,
    pub required: bool,
}

pub struct Package {
    pub id: String,
    /// `kind:` from the manifest (`specialist`, `lane`, `skill`, `template`, ...).
    pub kind: String,
    /// `None` for skill-only packages (e.g. `lead-doctrine`).
    pub persona: Option<Persona>,
    pub skills: Vec<Skill>,
    pub context: Vec<ContextKey>,
}

/// Split `---\n<yaml>\n---\n<body>`. Returns `(Value::Null, text)` when absent.
pub fn split_frontmatter(text: &str) -> Result<(Value, String), String> {
    let t = text.strip_prefix('\u{feff}').unwrap_or(text);
    let Some(rest) = t
        .strip_prefix("---\n")
        .or_else(|| t.strip_prefix("---\r\n"))
    else {
        return Ok((Value::Null, t.to_string()));
    };
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end() == "---" {
            let yaml = &rest[..offset];
            let body = &rest[offset + line.len()..];
            let v = serde_yaml::from_str(yaml).map_err(|e| e.to_string())?;
            return Ok((v, body.to_string()));
        }
        offset += line.len();
    }
    Err("unterminated frontmatter".into())
}

fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(|s| s.trim().to_string())
}

/// `tools: [A, B]` or `tools: "A, B"`.
fn list_field(v: &Value, key: &str) -> Vec<String> {
    match v.get(key) {
        Some(Value::Sequence(s)) => s
            .iter()
            .filter_map(|x| x.as_str().map(str::to_string))
            .collect(),
        Some(Value::String(s)) => s
            .split(',')
            .map(|x| x.trim().to_string())
            .filter(|x| !x.is_empty())
            .collect(),
        _ => Vec::new(),
    }
}

fn utf8(bytes: &[u8], path: &str) -> Result<String, InitError> {
    String::from_utf8(bytes.to_vec()).map_err(|_| InitError::Other(format!("{path}: not UTF-8")))
}

fn load_skill(
    src: &AgentSource,
    dir: &str,
    fallback_name: &str,
    skip_top: &[&str],
) -> Result<Skill, InitError> {
    let skill_md = format!("{dir}/SKILL.md");
    let text = utf8(src.get(&skill_md).unwrap_or_default(), &skill_md)?;
    let (fm, _) =
        split_frontmatter(&text).map_err(|e| InitError::Other(format!("{skill_md}: {e}")))?;
    let files = src
        .under(dir)
        .filter(|(rel, _)| {
            !skip_top
                .iter()
                .any(|s| *rel == *s || rel.starts_with(&format!("{s}/")))
        })
        .map(|(rel, b)| (rel.to_string(), b.to_vec()))
        .collect();
    Ok(Skill {
        name: str_field(&fm, "name").unwrap_or_else(|| fallback_name.to_string()),
        description: str_field(&fm, "description").unwrap_or_default(),
        files,
    })
}

impl Package {
    pub fn load(src: &AgentSource, id: &str) -> Result<Self, InitError> {
        let manifest_path = format!("{id}/weftos-package.yaml");
        let manifest_text = utf8(
            src.get(&manifest_path)
                .ok_or_else(|| InitError::Other(format!("unknown agent package '{id}'")))?,
            &manifest_path,
        )?;
        let manifest: Value = serde_yaml::from_str(&manifest_text)
            .map_err(|e| InitError::Other(format!("{manifest_path}: {e}")))?;

        let persona = match src.get(&format!("{id}/AGENT.md")) {
            Some(bytes) => {
                let text = utf8(bytes, "AGENT.md")?;
                let (fm, body) = split_frontmatter(&text)
                    .map_err(|e| InitError::Other(format!("{id}/AGENT.md: {e}")))?;
                Some(Persona {
                    description: str_field(&fm, "description").unwrap_or_default(),
                    tools: list_field(&fm, "tools"),
                    nickname: str_field(&fm, "nickname")
                        .or_else(|| str_field(&manifest, "nickname"))
                        .filter(|n| !n.starts_with('(')),
                    body: body.trim_start_matches('\n').to_string(),
                })
            }
            None => None,
        };

        let mut skills = Vec::new();
        let skills_root = format!("{id}/skills");
        let skill_dirs: BTreeSet<&str> = src
            .under(&skills_root)
            .filter_map(|(rel, _)| rel.strip_suffix("/SKILL.md"))
            .filter(|d| !d.contains('/'))
            .collect();
        for dir in skill_dirs {
            skills.push(load_skill(src, &format!("{id}/skills/{dir}"), dir, &[])?);
        }
        // Skill-only package: the package root itself is the skill.
        if persona.is_none() && src.get(&format!("{id}/SKILL.md")).is_some() {
            let skip = [
                "weftos-package.yaml",
                "skills",
                "evals",
                "CHANGELOG.md",
                "LICENSE",
            ];
            skills.push(load_skill(src, id, id, &skip)?);
        }

        let context = match manifest.get("required_project_context") {
            Some(Value::Sequence(items)) => items
                .iter()
                .filter_map(|i| {
                    Some(ContextKey {
                        key: str_field(i, "key")?,
                        description: str_field(i, "description").unwrap_or_default(),
                        required: i.get("required").and_then(Value::as_bool).unwrap_or(true),
                    })
                })
                .collect(),
            _ => Vec::new(),
        };

        Ok(Self {
            id: id.to_string(),
            kind: str_field(&manifest, "kind").unwrap_or_default(),
            persona,
            skills,
            context,
        })
    }
}

/// A team.yaml's members, split into active and inactive (`active: false`).
pub struct TeamMembers {
    pub active: Vec<String>,
    pub inactive: Vec<String>,
}

/// Ids named by a team.yaml (unvalidated), read tolerantly: `lead`, `members`,
/// `shared_skills` / `skills` may be strings or maps keyed `agent`/`id`/`package`/`skill`.
pub fn team_members(src: &AgentSource, team: &str) -> Result<Option<TeamMembers>, InitError> {
    let path = format!("teams/{team}/team.yaml");
    let Some(bytes) = src.get(&path) else {
        return Ok(None);
    };
    let v: Value =
        serde_yaml::from_slice(bytes).map_err(|e| InitError::Other(format!("{path}: {e}")))?;
    let mut members = TeamMembers {
        active: Vec::new(),
        inactive: Vec::new(),
    };
    let mut push = |item: &Value| {
        let id = match item {
            Value::String(s) => Some(s.clone()),
            other => ["agent", "id", "package", "skill"]
                .iter()
                .find_map(|k| other.get(*k).and_then(Value::as_str).map(str::to_string)),
        }
        // Team spec v1 members may pin a catalog version: `agent: steward@0.8.3`.
        .map(|id| id.split('@').next().unwrap_or_default().to_string());
        let active = item.get("active").and_then(Value::as_bool).unwrap_or(true);
        let list = if active {
            &mut members.active
        } else {
            &mut members.inactive
        };
        if let Some(id) = id
            && !list.contains(&id)
        {
            list.push(id);
        }
    };
    for key in ["lead", "members", "shared_skills", "skills"] {
        match v.get(key) {
            Some(Value::Sequence(items)) => items.iter().for_each(&mut push),
            Some(item) => push(item),
            None => {}
        }
    }
    Ok(Some(members))
}

/// One member slot of a team.yaml.
pub struct TeamSlot {
    pub agent: String,
    pub position: Option<u32>,
    pub active: bool,
}

/// The `members:` slots of a team.yaml in position order (pinned
/// `agent: id@ver` resolves to the id). `None` when the team does not exist.
pub fn team_slots(src: &AgentSource, team: &str) -> Result<Option<Vec<TeamSlot>>, InitError> {
    let path = format!("teams/{team}/team.yaml");
    let Some(bytes) = src.get(&path) else {
        return Ok(None);
    };
    let v: Value =
        serde_yaml::from_slice(bytes).map_err(|e| InitError::Other(format!("{path}: {e}")))?;
    let mut slots = Vec::new();
    if let Some(Value::Sequence(items)) = v.get("members") {
        for item in items {
            let id = ["agent", "id", "package"]
                .iter()
                .find_map(|k| item.get(*k).and_then(Value::as_str))
                .or_else(|| item.as_str())
                .map(|s| s.split('@').next().unwrap_or_default().to_string());
            let Some(agent) = id else { continue };
            slots.push(TeamSlot {
                agent,
                position: item.get("position").and_then(Value::as_u64).map(|p| p as u32),
                active: item.get("active").and_then(Value::as_bool).unwrap_or(true),
            });
        }
    }
    slots.sort_by_key(|s| s.position.unwrap_or(u32::MAX));
    Ok(Some(slots))
}

/// Manifest `version` of package `id` (None when the manifest has none).
pub fn package_version(src: &AgentSource, id: &str) -> Option<String> {
    let text = std::str::from_utf8(src.get(&format!("{id}/weftos-package.yaml"))?).ok()?;
    let v: Value = serde_yaml::from_str(text).ok()?;
    str_field(&v, "version")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_splits_yaml_and_body() {
        let (fm, body) =
            split_frontmatter("---\nname: x\ntools: [Read, Bash]\n---\n\n# Body\n").unwrap();
        assert_eq!(str_field(&fm, "name").as_deref(), Some("x"));
        assert_eq!(list_field(&fm, "tools"), vec!["Read", "Bash"]);
        assert_eq!(body, "\n# Body\n");
    }

    #[test]
    fn frontmatter_absent_or_unterminated() {
        let (fm, body) = split_frontmatter("# just text\n").unwrap();
        assert!(fm.is_null());
        assert_eq!(body, "# just text\n");
        assert!(split_frontmatter("---\nname: x\n").is_err());
    }

    #[test]
    fn tools_accepts_comma_string() {
        let v: Value = serde_yaml::from_str("tools: Read, Grep").unwrap();
        assert_eq!(list_field(&v, "tools"), vec!["Read", "Grep"]);
    }

    fn source_with_team(yaml: &str) -> AgentSource {
        let mut files = std::collections::BTreeMap::new();
        files.insert(
            "teams/t/team.yaml".to_string(),
            std::borrow::Cow::Owned(yaml.as_bytes().to_vec()),
        );
        AgentSource {
            files,
            origin: "test".into(),
            commit: "test".into(),
        }
    }

    #[test]
    fn team_v1_pinned_members_resolve_to_package_ids() {
        let src = source_with_team(
            "schema: 1\nlead: null\nshared_skills:\n  - lead-doctrine\nmembers:\n  - agent: steward@0.8.3\n    active: true\n  - agent: measurer@0.8.3\n    active: false\n    phase: 2\nedges:\n  - from: steward\n    to: mo\n    kind: consults\n",
        );
        let m = team_members(&src, "t").unwrap().unwrap();
        assert_eq!(m.active, vec!["steward", "lead-doctrine"]);
        assert_eq!(m.inactive, vec!["measurer"]);
    }

    #[test]
    fn embedded_weftos_core_is_a_v1_team_with_the_same_members() {
        let m = team_members(&AgentSource::embedded(), "weftos-core")
            .unwrap()
            .unwrap();
        for id in ["steward", "doc-gardener", "liber", "mo", "developer", "reviewer", "tester"] {
            assert!(m.active.iter().any(|a| a == id), "{id} not active");
        }
        assert_eq!(m.inactive, vec!["documenter", "measurer"]);
        assert!(m.active.iter().all(|a| !a.contains('@')));
    }
}
