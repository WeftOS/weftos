//! What a team apply created: project-context detection and the birth report
//! (bound adapters, missing keys, member slots, skills, hooks).

use std::collections::BTreeMap;
use std::path::Path;

use serde::Serialize;

use super::package::Package;
use super::plan::{Action, Plan};

/// Project facts read from the checkout: only paths and commands whose
/// evidence is a file in the project. Never reads outside `root`.
pub fn detect(root: &Path) -> BTreeMap<String, String> {
    let mut d = BTreeMap::new();
    let mut put = |k: &str, v: String| {
        d.insert(k.to_string(), v);
    };
    if root.join("docs").is_dir() {
        put("docs_root", "docs".into());
    }
    if root.join("docs/adr").is_dir() {
        put("decision_record_index_path", "docs/adr".into());
    }
    if root.join("docs/handoff.md").is_file() {
        put("handoff_record_path", "docs/handoff.md".into());
    }
    let mut workflows: Vec<String> = std::fs::read_dir(root.join(".github/workflows"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.ends_with(".yml") || n.ends_with(".yaml"))
        .collect();
    workflows.sort();
    if let Some(w) = workflows.first() {
        put("gate_workflow_path", format!(".github/workflows/{w}"));
    }
    if root.join("Cargo.toml").is_file() {
        put("unit_test_command", "cargo test".into());
    } else if root.join("package.json").is_file() {
        put("unit_test_command", "npm test".into());
    }
    d
}

#[derive(Clone, Debug, Serialize)]
pub struct SlotBirth {
    pub agent: String,
    pub skills: Vec<String>,
    pub files: Vec<String>,
}

/// The report `team apply` prints and `agent.install` can return.
#[derive(Clone, Debug, Serialize)]
pub struct Birth {
    pub team: Option<String>,
    pub hosts: Vec<String>,
    /// Paths this apply wrote (created or updated), sorted.
    pub created: Vec<String>,
    pub slots: Vec<SlotBirth>,
    /// Hooks the apply installed. The renderer installs none; stated, not omitted.
    pub hooks: Vec<String>,
    /// Required keys with a detected value in the project context.
    pub bound: BTreeMap<String, String>,
    /// Keys still open in the project context (required first).
    pub missing: Vec<String>,
}

pub fn birth(plan: &Plan, packages: &[Package], hosts: &[&str], detected: &BTreeMap<String, String>) -> Birth {
    let mut created: Vec<String> = plan
        .items
        .iter()
        .filter(|i| matches!(i.action, Action::Create | Action::Seed | Action::Update | Action::Overwrite))
        .map(|i| i.path.clone())
        .collect();
    created.sort();
    let slots = packages
        .iter()
        .map(|p| SlotBirth {
            agent: p.id.clone(),
            skills: p.skills.iter().map(|s| s.name.clone()).collect(),
            files: plan
                .items
                .iter()
                .filter(|i| i.owner == p.id && i.action != Action::Delete)
                .map(|i| i.path.clone())
                .collect(),
        })
        .collect();
    let mut bound = BTreeMap::new();
    let mut missing = Vec::new();
    for k in packages.iter().flat_map(|p| p.context.iter()) {
        match detected.get(&k.key) {
            Some(v) => {
                bound.insert(k.key.clone(), v.clone());
            }
            None => {
                let label = if k.required { k.key.clone() } else { format!("{} (optional)", k.key) };
                if !missing.contains(&label) {
                    missing.push(label);
                }
            }
        }
    }
    missing.sort_by_key(|m| m.ends_with("(optional)"));
    Birth {
        team: plan.team.clone(),
        hosts: hosts.iter().map(|h| h.to_string()).collect(),
        created,
        slots,
        hooks: Vec::new(),
        bound,
        missing,
    }
}

pub fn format_birth(b: &Birth) -> String {
    let mut s = format!(
        "Birth report: team {} on {}\n  created/updated: {} file(s)\n",
        b.team.as_deref().unwrap_or("-"),
        b.hosts.join(", "),
        b.created.len()
    );
    for p in &b.created {
        s.push_str(&format!("    {p}\n"));
    }
    s.push_str("  slots:\n");
    for sl in &b.slots {
        let skills = if sl.skills.is_empty() { "-".to_string() } else { sl.skills.join(", ") };
        s.push_str(&format!("    {:<18} skills: {skills}\n", sl.agent));
    }
    s.push_str(&format!(
        "  hooks: {}\n",
        if b.hooks.is_empty() { "none installed".to_string() } else { b.hooks.join(", ") }
    ));
    s.push_str("  bound:\n");
    for (k, v) in &b.bound {
        s.push_str(&format!("    {k} = {v}\n"));
    }
    s.push_str(&format!(
        "  open questions (fill in .agents/project-context.md): {}\n",
        if b.missing.is_empty() { "none".into() } else { b.missing.join(", ") }
    ));
    s
}
