//! The `install`, `update` and `remove` dashboard actions (ADR-108 P3a).
//!
//! - `install`: validate the payload ([`InstallRequest::validate`]), pick a
//!   fetcher with [`fetch_order`] over the injected list (git-remote here; the
//!   mesh fetcher joins the same list), fetch into an absent or empty target,
//!   then register the checkout by running the sibling `weft project init
//!   --adopt <ULID> [--repo DIR ...]` (found next to the running binary, then on
//!   `PATH`; an argument vector, no shell). Any failure removes only what this
//!   install created. After it, the shared add step ([`crate::project_add`]) runs
//!   exactly as for `weaver project add`: detect how the project runs, install its
//!   router route, record a receipt, and put the report in the result as `add`.
//! - `update`: `git pull --ff-only` in each repository of the registered workspace.
//! - `remove`: unregisters the workspace manifest. It never deletes files.
//!
//! Payloads are untrusted and are never echoed back in an error.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::dashboard_actions::{Action, ActionHandler, ActionOutcome};
use crate::project_git::GitRunner;
use crate::project_add::{Finish, finish, register};
use crate::project_install::{InstallRequest, ProjectFetcher, fetch_order};
use crate::project_install_git::GitRemoteFetcher;
use crate::project_install_layout::{Rollback, check_free, plan};
use crate::router_routes::slugify;

/// Most repositories one update touches (same cap as the reporter).
const MAX_UPDATE_REPOS: usize = 8;

/// Everything the handlers need from the machine, injected so tests never touch
/// the real home or manifest store.
#[derive(Debug, Clone)]
pub struct InstallEnv {
    pub home: PathBuf,
    pub manifests_dir: PathBuf,
    /// The `weft` binary; `None` looks next to the running binary, then `PATH`.
    pub weft: Option<PathBuf>,
    pub git: GitRunner,
    /// Where `pair-requests.json` lives (the daemon's runtime dir, next to the
    /// trust files): an install that names a primary no fetcher can reach
    /// records a pair request there (ADR-108 P2b). `None` records nothing.
    pub pair_requests_dir: Option<PathBuf>,
}

impl InstallEnv {
    /// The running user's home and manifest store; `None` without a home.
    pub fn from_process() -> Option<Self> {
        let home = clawft_types::runtime_paths::home_dir()?;
        Some(Self {
            manifests_dir: crate::user_daemon::manifests_dir(&home),
            home,
            weft: None,
            git: GitRunner::default(),
            pair_requests_dir: Some(crate::protocol::runtime_dir()),
        })
    }

    /// No fetcher could serve `req`: when it names a primary, ask to pair with
    /// it so the next attempt can go over the mesh, and say so in the error.
    fn no_fetcher(&self, req: &InstallRequest) -> String {
        let base = "no fetcher can serve this request on this node".to_owned();
        let (Some(primary), Some(dir)) = (&req.primary, &self.pair_requests_dir) else { return base };
        match crate::mesh_pair_requests::record(dir, &primary.node_id, std::slice::from_ref(&req.project_ulid)) {
            Ok(r) => format!(
                "{base}; pair request {} recorded for primary {} (approve it in the dashboard, then retry the install)",
                r.request_id, primary.node_id
            ),
            Err(e) => format!("{base}; could not record a pair request for primary {}: {e}", primary.node_id),
        }
    }

    pub(crate) fn weft_bin(&self) -> Result<PathBuf, String> {
        if let Some(w) = &self.weft {
            return Ok(w.clone());
        }
        let beside = std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join("weft"))).filter(|p| p.is_file());
        let on_path = || std::env::var_os("PATH").and_then(|p| std::env::split_paths(&p).map(|d| d.join("weft")).find(|c| c.is_file()));
        beside.or_else(on_path).ok_or_else(|| "the weft binary was not found next to weaver or on PATH".to_owned())
    }
}

/// The handlers of this module, ready for `default_handlers()`.
pub fn handlers(env: InstallEnv) -> Vec<Arc<dyn ActionHandler>> {
    #[allow(unused_mut)]
    let mut fetchers: Vec<Arc<dyn ProjectFetcher>> = vec![Arc::new(GitRemoteFetcher::with_runner(env.git.clone()))];
    // ADR-108 P3b: the mesh fetcher goes first through `fetch_order`; it says
    // no until placement is built and the primary is a paired peer. Its chain
    // is the daemon's, found once placement has it.
    #[cfg(all(feature = "placement", unix))]
    fetchers.push(Arc::new(crate::project_fetch_mesh::MeshFetcher::daemon(None)));
    vec![
        Arc::new(InstallHandler::new(env.clone(), fetchers)),
        Arc::new(UpdateHandler { env: env.clone() }),
        Arc::new(RemoveHandler { env }),
    ]
}

/// `install`.
pub struct InstallHandler {
    env: InstallEnv,
    fetchers: Vec<Arc<dyn ProjectFetcher>>,
}

impl InstallHandler {
    pub fn new(env: InstallEnv, fetchers: Vec<Arc<dyn ProjectFetcher>>) -> Self {
        Self { env, fetchers }
    }

    async fn install(&self, payload: &Value) -> Result<Value, String> {
        let req: InstallRequest =
            serde_json::from_value(payload.clone()).map_err(|_| "payload is not a valid install request".to_owned())?;
        let target = req.validate(&self.env.home).map_err(|e| e.to_string())?;
        let placements = plan(&req, &target)?;
        check_free(&placements, &target)?;
        let fetcher = fetch_order(&self.fetchers, &req).ok_or_else(|| self.env.no_fetcher(&req))?.clone();
        let weft = self.env.weft_bin()?;
        let rollback = Rollback::snapshot(&placements, &target);
        let done = async {
            let report = fetcher.fetch(&req, &target).await?;
            std::fs::create_dir_all(&target).map_err(|e| format!("cannot create {}: {e}", target.display()))?;
            let siblings: Vec<&Path> =
                placements.iter().map(|p| p.dest.as_path()).filter(|d| !d.starts_with(&target)).collect();
            let (ulid, registered_as) =
                register(&self.env, &weft, &target, Some(&req.project_ulid), req.slug.as_deref(), &siblings).await?;
            Ok::<_, String>((report, ulid, registered_as))
        };
        match done.await {
            Ok((r, ulid, registered_as)) => {
                let mut result = json!({ "fetcher": r.fetcher, "root": target, "repos": r.repos, "bytes": r.bytes, "archived": r.archived });
                // The same detect / route / receipt step `weaver project add` runs.
                let slug = req.slug.clone().or_else(|| target.file_name().and_then(|n| n.to_str()).and_then(slugify));
                if let Some(slug) = slug {
                    let siblings = placements.iter().map(|p| p.dest.clone()).filter(|d| !d.starts_with(&target)).collect();
                    let report = finish(&self.env, Finish { slug: &slug, root: &target, siblings, ulid, registered_as, fetcher: r.fetcher, repos: r.repos.clone() });
                    result["add"] = serde_json::to_value(&report).unwrap_or(Value::Null);
                    if let Some(h) = crate::router_state::global() {
                        h.reload();
                    }
                }
                Ok(result)
            }
            Err(e) => {
                rollback.undo();
                Err(e)
            }
        }
    }
}

#[async_trait]
impl ActionHandler for InstallHandler {
    fn kind(&self) -> &str {
        "install"
    }
    async fn handle(&self, action: &Action) -> ActionOutcome {
        match self.install(&action.payload).await {
            Ok(result) => ActionOutcome { status: "succeeded", result },
            Err(e) => ActionOutcome::failed(e, "install"),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectRef {
    project_ulid: String,
}

fn parse_ref(payload: &Value) -> Result<ProjectRef, String> {
    let r: ProjectRef = serde_json::from_value(payload.clone()).map_err(|_| "payload must be {project_ulid}".to_owned())?;
    clawft_types::project::validate_id(&r.project_ulid).map_err(|_| "project_ulid is not a ULID".to_owned())?;
    Ok(r)
}

/// `update`.
pub struct UpdateHandler {
    pub env: InstallEnv,
}

impl UpdateHandler {
    async fn update(&self, payload: &Value) -> Result<ActionOutcome, String> {
        let id = parse_ref(payload)?.project_ulid;
        let m = clawft_types::project::find_by_id(&self.env.manifests_dir, &id)
            .map_err(|e| format!("cannot read the project index: {e}"))?
            .ok_or("this project is not registered on this machine")?;
        let mut dirs: Vec<(String, PathBuf)> = Vec::new();
        if m.root.join(".git").exists() {
            dirs.push((".".into(), m.root.clone()));
        }
        for extra in m.workspace_repos() {
            if extra.join(".git").exists() {
                dirs.push((extra.display().to_string(), extra));
            }
        }
        if let Ok(rd) = std::fs::read_dir(&m.root) {
            let mut subs: Vec<_> = rd.flatten().map(|e| e.path()).filter(|p| p.join(".git").exists() && p.is_dir()).collect();
            subs.sort();
            for p in subs {
                let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                dirs.push((name, p));
            }
        }
        dirs.truncate(MAX_UPDATE_REPOS);
        if dirs.is_empty() {
            return Err("no git repositories found in this workspace".into());
        }
        let git = &self.env.git;
        let head = |d: &Path| {
            let d = d.to_owned();
            async move { git.run(&d, &["rev-parse", "--short=8", "HEAD"]).await.ok().map(|s| s.trim().to_owned()) }
        };
        let mut repos = Vec::new();
        let mut failed = false;
        for (label, dir) in &dirs {
            let before = head(dir).await;
            let pulled = git.run(dir, &["pull", "--ff-only", "--quiet"]).await;
            let after = head(dir).await;
            let mut r = json!({ "path": label, "head": after, "updated": before != after });
            if let Err(e) = pulled {
                failed = true;
                r["error"] = json!(e);
            }
            repos.push(r);
        }
        let mut result = json!({ "project_ulid": id, "root": m.root, "repos": repos });
        if failed {
            result["error"] = json!("git pull --ff-only failed in at least one repository");
            result["kind"] = json!("update");
        }
        Ok(ActionOutcome { status: if failed { "failed" } else { "succeeded" }, result })
    }
}

#[async_trait]
impl ActionHandler for UpdateHandler {
    fn kind(&self) -> &str {
        "update"
    }
    async fn handle(&self, action: &Action) -> ActionOutcome {
        self.update(&action.payload).await.unwrap_or_else(|e| ActionOutcome::failed(e, "update"))
    }
}

/// `remove`.
pub struct RemoveHandler {
    pub env: InstallEnv,
}

impl RemoveHandler {
    fn remove(&self, payload: &Value) -> Result<Value, String> {
        use clawft_types::project::{find_by_id, manifest_path};
        let id = parse_ref(payload)?.project_ulid;
        let dir = &self.env.manifests_dir;
        let m = find_by_id(dir, &id)
            .map_err(|e| format!("cannot read the project index: {e}"))?
            .ok_or("this project is not registered on this machine")?;
        if !m.is_workspace() {
            return Err("this is the project's own home, not a workspace; refusing to unregister it".into());
        }
        let path = manifest_path(dir, &id).map_err(|e| e.to_string())?;
        std::fs::remove_file(&path).map_err(|e| format!("cannot unregister: {e}"))?;
        Ok(json!({
            "project_ulid": id,
            "root": m.root,
            "repos": m.workspace_repos(),
            "unregistered": true,
            "deleted_files": false,
            "note": "no files were deleted; remove the directories by hand if you no longer need them",
        }))
    }
}

#[async_trait]
impl ActionHandler for RemoveHandler {
    fn kind(&self) -> &str {
        "remove"
    }
    async fn handle(&self, action: &Action) -> ActionOutcome {
        match self.remove(&action.payload) {
            Ok(result) => ActionOutcome { status: "succeeded", result },
            Err(e) => ActionOutcome::failed(e, "remove"),
        }
    }
}

#[cfg(test)]
#[path = "project_install_handlers_tests.rs"]
mod tests;
