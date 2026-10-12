//! What a fetched project says about how it runs (`weaver project add`).
//!
//! Read-only and bounded: the checkout's `process-compose.yaml`,
//! `compose/ports.yaml` and `package.json` scripts are looked at, and the
//! *names* of the environment variables it needs are listed from
//! `.env.example` style files and `${VAR}` references in the process-compose
//! file. Values are never read into a result. The one place a `.env` file is
//! opened is [`dotenv_names`], which keeps the names and drops each value as it
//! goes, to tell whether a variable the project needs is already provided.
//! Nothing here guesses: what is not found is reported as not found.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::router_routes::{PC_HTTP_USE, PortClaim, ProjectRoutes, Route, parse_ports_yaml};
use crate::router_sources::PORTS_FILE;

/// Largest file read here.
const MAX_FILE: u64 = 256 * 1024;
/// Most variable names listed.
pub const MAX_ENV_NAMES: usize = 64;
const PC_FILES: [&str; 3] = ["process-compose.yaml", "process-compose.yml", "compose/process-compose.yaml"];
const ENV_EXAMPLES: [&str; 5] = [".env.example", ".env.sample", ".env.template", ".env.local.example", "app/.env.example"];

/// How the project runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RunKind {
    /// A process-compose file (relative to the root).
    ProcessCompose { file: PathBuf },
    /// No process-compose file; `package.json` has these scripts.
    PackageScripts { scripts: Vec<String> },
    /// Nothing recognisable.
    Unknown,
}

/// One environment variable the project asks for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EnvVar {
    pub name: String,
    /// Where it was found (relative path).
    pub file: String,
    /// Whether this process's environment has it (the value is never read out).
    pub set_here: bool,
}

/// Everything found in one checkout.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Detected {
    pub run: RunKind,
    /// `process-compose-http` claim from `compose/ports.yaml`.
    pub pc_http: Option<u16>,
    pub claims: Vec<PortClaim>,
    /// Routes the project declares itself.
    pub repo_routes: Vec<Route>,
    pub env_vars: Vec<EnvVar>,
    /// `project:` from `compose/ports.yaml`, when it names one.
    pub ports_project: Option<String>,
    /// What was looked for and not found, or refused.
    pub notes: Vec<String>,
}

fn read_small(path: &Path) -> Option<String> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_FILE {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

fn valid_env_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 64 && !n.starts_with(|c: char| c.is_ascii_digit()) && n.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// Variable names assigned in a `.env.example` style text (`NAME=`, `export NAME=`).
/// Only the part before `=` is looked at.
pub fn env_names_of_example(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| {
            let l = l.strip_prefix("export ").unwrap_or(l);
            let name = l.split_once('=')?.0.trim();
            valid_env_name(name).then(|| name.to_owned())
        })
        .collect()
}

/// `${NAME}` references in a process-compose file (`${NAME:-default}` has a default and is skipped).
pub fn env_names_of_compose(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find("${") {
        rest = &rest[i + 2..];
        let Some(end) = rest.find('}') else { break };
        let name = &rest[..end];
        if valid_env_name(name) {
            out.push(name.to_owned());
        }
    }
    out
}

/// Names assigned in the project's own `.env` (values are dropped as read).
pub fn dotenv_names(root: &Path) -> Vec<String> {
    read_small(&root.join(".env")).map(|t| env_names_of_example(&t)).unwrap_or_default()
}

fn package_scripts(root: &Path) -> Vec<String> {
    let Some(text) = read_small(&root.join("package.json")) else { return Vec::new() };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else { return Vec::new() };
    let mut names: Vec<String> = v.get("scripts").and_then(|s| s.as_object()).map(|o| o.keys().take(32).cloned().collect()).unwrap_or_default();
    names.sort();
    names
}

/// Look at the checkout at `root`. `is_set` says whether a variable is set in
/// the environment (injected so tests do not read the real one).
pub fn detect(root: &Path, is_set: &dyn Fn(&str) -> bool) -> Detected {
    let mut notes = Vec::new();
    let compose = PC_FILES.iter().find(|f| read_small(&root.join(f)).is_some());
    let ports_text = read_small(&root.join(PORTS_FILE));
    let ports: Option<ProjectRoutes> = match &ports_text {
        Some(text) => Some(parse_ports_yaml(text, "project", root)),
        None => {
            notes.push(format!("no {PORTS_FILE}: the project declares no ports or routes"));
            None
        }
    };
    let scripts = package_scripts(root);
    let run = match (compose, scripts.is_empty()) {
        (Some(f), _) => RunKind::ProcessCompose { file: PathBuf::from(f) },
        (None, false) => {
            notes.push("no process-compose file; package.json scripts found, but `project up` only runs process-compose".into());
            RunKind::PackageScripts { scripts }
        }
        (None, true) => {
            notes.push("no process-compose file and no package.json scripts: nothing to start".into());
            RunKind::Unknown
        }
    };
    let (pc_http, claims, repo_routes, ports_project) = match &ports {
        Some(p) => {
            for r in &p.refused {
                notes.push(format!("{PORTS_FILE}: {}", r.reason));
            }
            let named = ports_text.as_deref().is_some_and(|t| t.lines().any(|l| l.starts_with("project:")));
            (p.info.pc_http, p.info.claims.clone(), p.routes.clone(), named.then(|| p.info.slug.clone()))
        }
        None => (None, Vec::new(), Vec::new(), None),
    };
    if matches!(run, RunKind::ProcessCompose { .. }) && pc_http.is_none() {
        notes.push(format!("{PORTS_FILE} has no `{PC_HTTP_USE}` claim: `project up` cannot tell which port process-compose will serve"));
    }
    let mut env_vars: Vec<EnvVar> = Vec::new();
    let mut add = |names: Vec<String>, file: &str| {
        for name in names {
            if env_vars.len() < MAX_ENV_NAMES && !env_vars.iter().any(|e| e.name == name) {
                let set_here = is_set(&name);
                env_vars.push(EnvVar { name, file: file.to_owned(), set_here });
            }
        }
    };
    for f in ENV_EXAMPLES {
        if let Some(t) = read_small(&root.join(f)) {
            add(env_names_of_example(&t), f);
        }
    }
    if let Some(f) = compose
        && let Some(t) = read_small(&root.join(f))
    {
        add(env_names_of_compose(&t), f);
    }
    Detected { run, pc_http, claims, repo_routes, env_vars, ports_project, notes }
}

#[cfg(test)]
#[path = "project_add_detect_tests.rs"]
mod tests;
