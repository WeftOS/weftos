//! The `agent.uninstall` dashboard action: the dashboard approved removing an
//! agent package or team from a project checkout registered on this node.
//!
//! It is `weftos team uninstall`'s own code (`weftos::init::team`). Files the
//! user edited since the install are never deleted and never silently kept
//! either: the request is planned first, and when any file would stay the
//! whole request is refused, naming the paths, with nothing written. A package
//! an applied team still needs is refused too (remove the team). Chained as
//! `agent.uninstall` or `agent.uninstall.refused`, source `agent.uninstall`.
//!
//! The payload is untrusted: every field is checked before the disk is
//! touched, and it is never echoed back in an error.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use weftos::init::digest::Subject;
use weftos::init::team::{uninstall, uninstall_package};

use crate::agent_install::{AgentInstallEnv, PackageSource, Refusal, fail, name_ok, refuse, uuid_ok};
use crate::dashboard_actions::{Action, ActionHandler, ActionOutcome};

/// The action kind.
pub const KIND: &str = "agent.uninstall";
/// Chain source for the records.
pub const CHAIN_SOURCE: &str = "agent.uninstall";
pub const CHAIN_UNINSTALLED: &str = "agent.uninstall";
pub const CHAIN_REFUSED: &str = "agent.uninstall.refused";

/// The payload as the dashboard queues it (`agent_uninstall`).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentUninstallPayload {
    pub request_id: String,
    pub project_ulid: String,
    /// `agent` or `team`.
    pub subject: String,
    pub name: String,
}

fn record(env: &AgentInstallEnv, kind: &str, record: Value) {
    match &env.chain {
        Some(cm) => {
            cm.append(CHAIN_SOURCE, kind, Some(record));
        }
        None => tracing::warn!(kind, "no chain on this node: agent uninstall not chained"),
    }
}

/// Answers `agent.uninstall` actions. Shares [`AgentInstallEnv`] with install.
pub struct AgentUninstallHandler {
    env: Option<Arc<AgentInstallEnv>>,
}

impl AgentUninstallHandler {
    pub fn new(env: AgentInstallEnv) -> Self {
        Self { env: Some(Arc::new(env)) }
    }

    /// For `default_handlers()`: fails clearly when there is no home directory.
    /// The daemon replaces it with one that holds its chain.
    pub fn from_process() -> Self {
        Self { env: clawft_types::runtime_paths::home_dir().map(|h| Arc::new(AgentInstallEnv::from_home(&h, None))) }
    }
}

#[async_trait]
impl ActionHandler for AgentUninstallHandler {
    fn kind(&self) -> &str {
        KIND
    }

    async fn handle(&self, action: &Action) -> ActionOutcome {
        let Some(env) = self.env.clone() else { return outcome(Err(fail("no home directory on this node")), None) };
        let payload = action.payload.clone();
        tokio::task::spawn_blocking(move || apply_action(&env, payload))
            .await
            .unwrap_or_else(|e| outcome(Err(fail(format!("agent uninstall did not finish: {e}"))), None))
    }
}

fn outcome(r: Result<Value, Refusal>, request_id: Option<&str>) -> ActionOutcome {
    match r {
        Ok(result) => ActionOutcome { status: "succeeded", result },
        Err(Refusal { refused, message }) => ActionOutcome {
            status: "failed",
            result: json!({ "error": message, "kind": KIND, "refused": refused, "request_id": request_id }),
        },
    }
}

/// Validate, plan, refuse on edited files, uninstall, chain.
pub fn apply_action(env: &AgentInstallEnv, payload: Value) -> ActionOutcome {
    let p: AgentUninstallPayload = match serde_json::from_value(payload) {
        Ok(p) => p,
        Err(_) => return outcome(Err(refuse("payload is not a valid agent uninstall request")), None),
    };
    // Only a checked id is echoed back.
    let request_id = uuid_ok(&p.request_id).then(|| p.request_id.clone());
    let result = run(env, &p);
    if let Err((r, kept)) = &result
        && r.refused
    {
        record(
            env,
            CHAIN_REFUSED,
            json!({ "request_id": request_id, "project_ulid": p.project_ulid.chars().take(26).collect::<String>(),
                    "subject": p.subject.chars().take(8).collect::<String>(), "name": p.name.chars().take(64).collect::<String>(),
                    "reason": r.message, "edited": kept }),
        );
    }
    match result {
        Ok(v) => outcome(Ok(v), request_id.as_deref()),
        Err((r, kept)) => {
            let mut o = outcome(Err(r), request_id.as_deref());
            if !kept.is_empty() {
                o.result["edited"] = json!(kept);
            }
            o
        }
    }
}

/// The error carries the edited paths when they are why.
type Failed = (Refusal, Vec<String>);

fn no_paths(r: Refusal) -> Failed {
    (r, Vec::new())
}

fn run(env: &AgentInstallEnv, p: &AgentUninstallPayload) -> Result<Value, Failed> {
    if !uuid_ok(&p.request_id) {
        return Err(no_paths(refuse("request_id is not a UUID")));
    }
    if !crate::mesh_pair_requests::is_ulid(&p.project_ulid) {
        return Err(no_paths(refuse("project_ulid is not a ULID")));
    }
    let subject = Subject::parse(&p.subject).ok_or_else(|| no_paths(refuse("subject must be agent or team")))?;
    if !name_ok(&p.name) {
        return Err(no_paths(refuse("name is not a package name")));
    }
    let manifest = clawft_types::project::find_by_id(&env.manifests_dir, &p.project_ulid)
        .map_err(|e| no_paths(fail(format!("cannot read the project index: {e}"))))?
        .ok_or_else(|| no_paths(refuse("this project is not registered on this node")))?;
    let root = manifest.root;
    if !root.is_dir() {
        return Err(no_paths(refuse("the project's checkout is not on disk")));
    }
    let from = match &env.source {
        PackageSource::Dir(d) => Some(d.clone()),
        PackageSource::Embedded => None,
    };
    let go = |dry_run: bool| match subject {
        Subject::Team => uninstall(&root, Some(&p.name), &from, dry_run),
        Subject::Agent => uninstall_package(&root, &p.name, &from, dry_run),
    };
    let to_failed = |e: weftos::init::team::TeamError| no_paths(Refusal { refused: e.refused, message: e.message });

    // Plan first: anything edited since the install would stay behind, so
    // nothing is written and the dashboard is told which files.
    let plan = go(true).map_err(to_failed)?;
    if !plan.kept.is_empty() {
        return Err((
            refuse(format!("{} file(s) were edited since the install; revert or delete them, then retry", plan.kept.len())),
            plan.kept,
        ));
    }
    let done = go(false).map_err(to_failed)?;
    record(
        env,
        CHAIN_UNINSTALLED,
        json!({ "request_id": p.request_id, "project_ulid": p.project_ulid, "subject": p.subject, "name": p.name,
                "removed": done.removed, "files": done.touched }),
    );
    Ok(json!({ "request_id": p.request_id, "subject": p.subject, "name": p.name, "root": root,
               "removed": done.removed, "files": done.touched }))
}

#[cfg(test)]
#[path = "agent_uninstall_tests.rs"]
mod tests;
