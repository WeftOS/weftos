//! [`MeshFetcher`]: the `mesh` [`ProjectFetcher`] (ADR-108 P3b). It installs a
//! project from its primary node over the signed mesh channel: one `git
//! bundle` per repository (then `git fetch` of that bundle into a fresh clone
//! whose `origin` is the `weftos://` URL, so later `git pull` goes through
//! `git-remote-weftos` incrementally) and one checksummed tar of the non-git
//! content.
//!
//! Layout (same as the git-remote lane): the `.` repository goes to the target
//! path, a sibling `dir` to `<parent of target>/<dir>`; with no `.` repository
//! every `dir` goes under `<target>/<dir>`. Non-git content always unpacks
//! under the target path.
//!
//! D-C: when the primary reports unmarked non-git content over
//! [`crate::project_fetch_tar::LARGE_BYTES`], the tar is skipped and the report
//! carries a warning naming the biggest entries; the repositories still
//! install. `fetch_large` turns that into a fetch.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use clawft_kernel::chain::ChainManager;
use clawft_kernel::workload_ctl::{PLANE_CHAIN_SOURCE, PlacementControlPlane};
use clawft_types::placement::TrustTier;
use serde_json::{Value, json};

use crate::mesh_names::MeshNames;
use crate::project_fetch_client::{self as client, BundleMode, FetchChannel, PlaneChannel};
use crate::project_fetch_repos::git;
use crate::project_fetch_serve::EVENT_PROJECT_FETCH;
use crate::project_install::{FetchReport, FetchedRepo, InstallRequest, ProjectFetcher};
use crate::weftos_uri::{Authority, WeftosUri};

const GIT_TIMEOUT: Duration = Duration::from_secs(60);
const REFSPECS: [&str; 2] = ["+refs/heads/*:refs/remotes/origin/*", "+refs/tags/*:refs/tags/*"];

enum PlaneSource {
    Fixed(Arc<PlacementControlPlane>),
    /// The daemon's plane, once placement is built.
    Daemon,
}

/// Fetches a project from its primary over the mesh.
pub struct MeshFetcher {
    source: PlaneSource,
    chain: Option<Arc<ChainManager>>,
    /// The mesh the clones are named on (`origin`); `None` resolves the
    /// daemon's own at fetch time.
    names: Option<MeshNames>,
    /// Fetch very large unmarked non-git content instead of warning (D-C).
    pub fetch_large: bool,
}

impl MeshFetcher {
    /// With an explicit control plane (tests, embedded use).
    pub fn with_plane(plane: Arc<PlacementControlPlane>, chain: Option<Arc<ChainManager>>) -> Self {
        Self { source: PlaneSource::Fixed(plane), chain, names: None, fetch_large: false }
    }

    /// The daemon's: resolves the control plane per call. With `None` the
    /// chain is the one placement gave the fetch server, once it is built.
    pub fn daemon(chain: Option<Arc<ChainManager>>) -> Self {
        Self { source: PlaneSource::Daemon, chain, names: None, fetch_large: false }
    }

    /// Name clones on this mesh (tests; the daemon uses its own).
    pub fn with_mesh(mut self, names: MeshNames) -> Self {
        self.names = Some(names);
        self
    }

    fn names(&self) -> MeshNames {
        self.names.unwrap_or_else(MeshNames::local)
    }

    fn chain(&self) -> Option<Arc<ChainManager>> {
        self.chain.clone().or_else(|| match self.source {
            PlaneSource::Daemon => crate::project_fetch_serve::global().and_then(|h| h.chain()),
            PlaneSource::Fixed(_) => None,
        })
    }

    fn plane(&self) -> Option<Arc<PlacementControlPlane>> {
        match &self.source {
            PlaneSource::Fixed(p) => Some(p.clone()),
            PlaneSource::Daemon => crate::workload_place_rpc::plane_if_built(),
        }
    }

    /// The primary is a known target at tier `paired` or `pinned`.
    fn paired(&self, node: &str) -> bool {
        self.plane().is_some_and(|p| p.targets().iter().any(|t| t.node_id == node && t.tier >= TrustTier::Paired))
    }

    fn record(&self, payload: Value) {
        if let Some(c) = self.chain() {
            c.append(PLANE_CHAIN_SOURCE, EVENT_PROJECT_FETCH, Some(payload));
        }
    }
}

/// Where repository `dir` goes.
pub fn layout(target: &Path, has_root: bool, dir: &str) -> PathBuf {
    match (dir, has_root) {
        (".", _) => target.to_path_buf(),
        (d, true) => target.parent().unwrap_or(target).join(d),
        (d, false) => target.join(d),
    }
}

fn empty_or_absent(p: &Path) -> bool {
    match std::fs::read_dir(p) {
        Ok(mut rd) => rd.next().is_none(),
        Err(e) => e.kind() == std::io::ErrorKind::NotFound,
    }
}

#[async_trait]
impl ProjectFetcher for MeshFetcher {
    fn name(&self) -> &'static str {
        "mesh"
    }

    fn can_fetch(&self, req: &InstallRequest) -> bool {
        req.primary.as_ref().is_some_and(|p| self.paired(&p.node_id))
    }

    async fn fetch(&self, req: &InstallRequest, dest: &Path) -> Result<FetchReport, String> {
        let node = req.primary.as_ref().map(|p| p.node_id.clone()).ok_or("no primary node in the request")?;
        if !self.paired(&node) {
            return Err(format!("{node} is not a paired peer of this node"));
        }
        let plane = self.plane().ok_or("placement is not initialised on this node")?;
        let ch = PlaneChannel::new(plane, node.clone());
        let project = req.project_ulid.as_str();
        let mesh = self.names().authority().ok_or("this node has no mesh id or alias to name the project on")?;
        let out = self.fetch_over(&ch, mesh, req, project, dest).await;
        self.record(json!({ "node": node, "project": project, "ok": out.is_ok(), "error": out.as_ref().err(),
            "repos": out.as_ref().map(|r| r.repos.len()).unwrap_or(0), "bytes": out.as_ref().map(|r| r.bytes).unwrap_or(0) }));
        out
    }
}

impl MeshFetcher {
    /// Fetch `project` over any channel (the daemon's plane, or a CLI's socket to
    /// the daemon), naming the clones on `mesh`.
    pub(crate) async fn fetch_over(&self, ch: &dyn FetchChannel, mesh: Authority, req: &InstallRequest, project: &str, dest: &Path) -> Result<FetchReport, String> {
        let listing = client::list(ch, project).await?;
        let repos: Vec<(String, Option<String>)> = listing["repos"]
            .as_array()
            .ok_or("list: malformed answer")?
            .iter()
            .filter_map(|r| Some((r["dir"].as_str()?.to_owned(), r["branch"].as_str().map(str::to_owned))))
            .filter(|(d, _)| crate::project_install::dir_ok(d))
            .collect();
        let has_root = repos.iter().any(|(d, _)| d == ".");
        let mut report = FetchReport { fetcher: "mesh", ..Default::default() };
        for (dir, branch) in &repos {
            let path = layout(dest, has_root, dir);
            if !empty_or_absent(&path) {
                return Err(format!("{} exists and is not empty", path.display()));
            }
            std::fs::create_dir_all(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            let url = WeftosUri::for_project_repo(mesh, project, dir).to_string();
            git(&path, &["init", "--quiet"], None, GIT_TIMEOUT)?;
            git(&path, &["remote", "add", "origin", &url], None, GIT_TIMEOUT)?;
            let refs = client::refs(ch, project, dir).await?;
            let want: Vec<String> = refs.refs.iter().map(|(_, n)| n.clone()).collect();
            if !want.is_empty() {
                let specs = REFSPECS.iter().map(|s| s.to_string()).collect();
                client::fetch_bundle(ch, project, dir, &want, &[], &path, BundleMode::Fetch(specs)).await?;
                let wanted = req.sources.iter().find(|s| &s.dir == dir).and_then(|s| s.branch.clone());
                let head = refs.head.as_deref().and_then(|h| h.strip_prefix("refs/heads/")).map(str::to_owned);
                if let Some(b) = wanted.or(head).or_else(|| branch.clone()) {
                    checkout(&path, &b);
                }
            }
            report.repos.push(FetchedRepo { dir: dir.clone(), head: crate::project_fetch_repos::head_short(&path), remote: Some(url) });
        }
        if let Some(a) = listing["archived"].as_array() {
            report.archived.extend(a.iter().filter_map(|v| v.as_str().map(str::to_owned)));
        }
        let nongit = &listing["nongit"];
        let files = nongit["files"].as_u64().unwrap_or(0);
        if files > 0 && nongit["large"] == true && !self.fetch_large {
            let largest: Vec<String> = nongit["largest"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect()).unwrap_or_default();
            report.warnings.push(format!(
                "non-git content not fetched: {} MiB in {files} files is over the limit and not marked in .weftos/archive.toml (largest: {})",
                nongit["bytes"].as_u64().unwrap_or(0) >> 20,
                largest.join(", ")
            ));
        } else if files > 0 {
            std::fs::create_dir_all(dest).map_err(|e| format!("{}: {e}", dest.display()))?;
            if let Some(t) = client::fetch_tar(ch, project, dest).await? {
                report.bytes = t.bytes;
                for a in t.archived {
                    if !report.archived.contains(&a) {
                        report.archived.push(a);
                    }
                }
                if t.excluded > 0 {
                    report.warnings.push(format!("{} symlink(s) or credential-shaped file(s) stayed on the primary", t.excluded));
                }
                if t.truncated {
                    report.warnings.push("non-git content was cut at the file cap; mark large trees in .weftos/archive.toml".into());
                }
                if t.unpacked.refused > 0 {
                    report.warnings.push(format!("{} tar entries refused (links or escaping paths)", t.unpacked.refused));
                }
            }
        }
        Ok(report)
    }
}

/// From a CLI process: fetch `ulid` over the mesh through the local user daemon,
/// which resolves `uri` to the project's paired primary (the helper's route).
pub async fn fetch_via_daemon(uri: &str, mesh: Authority, ulid: &str, req: &InstallRequest, dest: &Path) -> Result<FetchReport, String> {
    let ch = client::DaemonChannel::new(uri.to_owned(), None);
    MeshFetcher::daemon(None).fetch_over(&ch, mesh, req, ulid, dest).await
}

/// Check out `branch` tracking `origin/<branch>` when the fetch brought it.
fn checkout(repo: &Path, branch: &str) {
    let remote = format!("refs/remotes/origin/{branch}");
    if git(repo, &["rev-parse", "--verify", "--quiet", &remote], None, GIT_TIMEOUT).is_err() {
        return;
    }
    let _ = git(repo, &["symbolic-ref", "refs/remotes/origin/HEAD", &remote], None, GIT_TIMEOUT);
    let _ = git(repo, &["checkout", "--quiet", "-b", branch, "--track", &format!("origin/{branch}")], None, GIT_TIMEOUT);
}

#[cfg(test)]
#[path = "project_fetch_tests.rs"]
pub(crate) mod tests;
