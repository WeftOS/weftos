//! The part of "add a project to this machine" that both front doors share:
//! the `install` dashboard action and `weaver project add` fetch with their own
//! fetchers, then call [`register`] and [`finish`] here, so "Add locally" and
//! the CLI behave identically from the moment the files are on disk.
//!
//! - [`register`] runs the sibling `weft project init` (`--adopt <ULID>` when the
//!   project has an identity, plain `--name` for a git-only checkout) and returns
//!   the registered ULID.
//! - [`finish`] looks at the checkout ([`crate::project_add_detect`]), decides and
//!   writes the router route ([`crate::project_add_route`]), writes the receipt
//!   ([`crate::project_add_receipt`]) and returns the [`AddReport`]: what ran
//!   where, in words a person can act on. Nothing in `finish` fails the add: a
//!   route that cannot be written is a note, not a rollback.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;

use crate::project_add_detect::{Detected, EnvVar, RunKind, detect};
use crate::project_add_receipt::{Receipt, receipts_dir};
use crate::project_add_route::{RoutePlan, decl_of, plan_route};
use crate::project_git::redact;
use crate::project_install::FetchedRepo;
use crate::project_install_handlers::InstallEnv;
use crate::router_overlay;
use crate::router_sources::{ManifestSource, load};

const REGISTER_TIMEOUT: Duration = Duration::from_secs(60);

/// How the checkout was registered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RegisteredAs {
    /// `weft project init --adopt <ULID>`: a working copy of a project whose primary is elsewhere.
    Workspace,
    /// `weft project init --name`: a project of its own, with a fresh identity.
    Project,
}

/// Register `root` and return its ULID. `ulid` is the project's identity when
/// it has one (adopt), `None` for a git-only checkout.
pub async fn register(env: &InstallEnv, weft: &Path, root: &Path, ulid: Option<&str>, name: Option<&str>, siblings: &[&Path]) -> Result<(String, RegisteredAs), String> {
    let mut cmd = tokio::process::Command::new(weft);
    cmd.current_dir(root).args(["project", "init"]);
    match ulid {
        Some(id) => {
            cmd.args(["--adopt", id]);
        }
        None => {
            if !siblings.is_empty() {
                return Err("a checkout with no project identity cannot register sibling repositories".into());
            }
        }
    }
    if let Some(n) = name {
        cmd.args(["--name", n]);
    }
    for s in siblings {
        cmd.arg("--repo").arg(s);
    }
    cmd.env("HOME", &env.home).env("WEFTOS_MANIFESTS_DIR", &env.manifests_dir).stdin(std::process::Stdio::null()).kill_on_drop(true);
    let out = tokio::time::timeout(REGISTER_TIMEOUT, cmd.output())
        .await
        .map_err(|_| "weft project init timed out".to_owned())?
        .map_err(|e| format!("cannot run weft: {e}"))?;
    if !out.status.success() {
        let text = redact(String::from_utf8_lossy(&out.stderr).trim());
        return Err(format!("registering the workspace failed ({}): {text}", out.status));
    }
    match ulid {
        Some(id) => Ok((id.to_owned(), RegisteredAs::Workspace)),
        None => {
            let m = clawft_types::project::find_by_root(&env.manifests_dir, root)
                .map_err(|e| format!("cannot read the project index: {e}"))?
                .ok_or("weft registered the project but it is not in the project index")?;
            Ok((m.id, RegisteredAs::Project))
        }
    }
}

/// What the router route came to.
pub type RouteOutcome = RoutePlan;

/// Everything an add did and found.
#[derive(Debug, Clone, Serialize)]
pub struct AddReport {
    pub slug: String,
    pub root: PathBuf,
    pub ulid: String,
    pub registered_as: RegisteredAs,
    pub fetcher: String,
    pub repos: Vec<FetchedRepo>,
    pub run: RunKind,
    pub process_compose_port: Option<u16>,
    pub route: RouteOutcome,
    /// Names only; `set_here` says whether this process's environment has them.
    pub env_vars: Vec<EnvVar>,
    /// What was not found or could not be done, in plain words.
    pub notes: Vec<String>,
    /// What to do next.
    pub next: Vec<String>,
}

/// What [`finish`] needs to know about the add.
pub struct Finish<'a> {
    pub slug: &'a str,
    pub root: &'a Path,
    pub siblings: Vec<PathBuf>,
    pub ulid: String,
    pub registered_as: RegisteredAs,
    pub fetcher: &'a str,
    pub repos: Vec<FetchedRepo>,
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Detect, route, receipt, report. Never fails the add.
pub fn finish(env: &InstallEnv, f: Finish<'_>) -> AddReport {
    let det: Detected = detect(f.root, &|n| std::env::var_os(n).is_some_and(|v| !v.is_empty()));
    let mut notes = det.notes.clone();
    let source = ManifestSource { manifests_dir: env.manifests_dir.clone(), overlays_dir: router_overlay::overlays_dir(&env.home) };
    let table = load(&source);
    let mut route = plan_route(&det, f.slug, &f.ulid, &table);
    let mut route_prefix = None;
    if let RoutePlan::Overlay { prefix, port } = &route {
        match router_overlay::set_route(&source.overlays_dir, &f.ulid, &decl_of(prefix, *port)) {
            Ok(()) => route_prefix = Some(prefix.clone()),
            Err(e) => {
                notes.push(format!("the route could not be written: {e}"));
                route = RoutePlan::None { reason: "the route overlay could not be written".into() };
            }
        }
    }
    let receipt = Receipt {
        slug: f.slug.to_owned(),
        root: f.root.to_path_buf(),
        repos: f.siblings.clone(),
        ulid: f.ulid.clone(),
        registered_as: match f.registered_as {
            RegisteredAs::Workspace => "workspace",
            RegisteredAs::Project => "project",
        }
        .into(),
        route_prefix,
        fetcher: f.fetcher.to_owned(),
        added_at: now(),
    };
    if let Err(e) = crate::project_add_receipt::write(&receipts_dir(&env.home), &receipt) {
        notes.push(format!("could not record what was added ({e}); `weaver project remove` will not find it"));
    }
    let next = next_steps(f.slug, &det, &route);
    AddReport {
        slug: f.slug.to_owned(),
        root: f.root.to_path_buf(),
        ulid: f.ulid,
        registered_as: f.registered_as,
        fetcher: f.fetcher.to_owned(),
        repos: f.repos,
        run: det.run.clone(),
        process_compose_port: det.pc_http,
        route,
        env_vars: det.env_vars.clone(),
        notes,
        next,
    }
}

fn next_steps(slug: &str, det: &Detected, route: &RoutePlan) -> Vec<String> {
    let mut next = Vec::new();
    let missing: Vec<&str> = det.env_vars.iter().filter(|e| !e.set_here).map(|e| e.name.as_str()).collect();
    if !missing.is_empty() {
        next.push(format!("set these environment variables before starting (names from the project's examples, values are yours): {}", missing.join(", ")));
    }
    match &det.run {
        RunKind::ProcessCompose { .. } if det.pc_http.is_some() => next.push(format!("weaver project up {slug}")),
        RunKind::ProcessCompose { .. } => next.push(format!("add a `process-compose-http` claim to compose/ports.yaml, then `weaver project up {slug}`")),
        RunKind::PackageScripts { scripts } => next.push(format!("start it by hand (package.json scripts: {}); `project up` needs a process-compose file", scripts.join(", "))),
        RunKind::Unknown => next.push("read the project's README for how to start it".into()),
    }
    match route {
        RoutePlan::Overlay { prefix, .. } => {
            next.push(format!("the app must serve under {prefix} (its base path); the router keeps the prefix on the request"));
            next.push(format!("weaver project status {slug}"));
        }
        RoutePlan::Repo { .. } => next.push(format!("weaver project status {slug}")),
        RoutePlan::Conflict { .. } | RoutePlan::None { .. } => next.push("no route was installed (see notes); the project is not reachable through the tailnet router yet".into()),
    }
    next
}

#[cfg(test)]
#[path = "project_add_tests.rs"]
mod tests;
