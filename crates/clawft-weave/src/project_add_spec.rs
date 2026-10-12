//! What `weaver project add` was asked to add, and how it gets fetched.
//!
//! Four spellings are accepted and told apart here, before anything touches
//! the disk:
//! - `weftos://<mesh>/projects/<ULID>` (ADR-114): fetched over the mesh from the
//!   paired primary;
//! - a bare project ULID: the same, the mesh being the one this node is on;
//! - a git URL (`https://`, `ssh://`, `user@host:path`): cloned with the
//!   member's own git credentials;
//! - a dashboard project id (a UUID): refused with the way forward, because
//!   only the dashboard knows that project's repositories.
//!
//! A mesh name carries no project name, so the slug (the directory, the route
//! and the `weaver project` handle) comes from `--slug`, the `--path`'s last
//! segment, or the git URL's repository name. Never from a guess.

use std::path::{Path, PathBuf};

use crate::project_install::{InstallRequest, SourceRepo, branch_ok, resolve_path, url_ok};
use crate::router_routes::{slugify, valid_slug};
use crate::weftos_uri::{Authority, WeftosUri};

/// What the first argument of `project add` names.
#[derive(Debug, Clone, PartialEq)]
pub enum SourceSpec {
    /// `weftos://<mesh>/projects/<ULID>`.
    Mesh { uri: String, mesh: Authority, ulid: String },
    /// A bare ULID: the mesh is this node's.
    Ulid(String),
    /// A git remote.
    Git(String),
}

/// Why the argument was refused.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum SpecError {
    #[error("a dashboard project id (UUID) cannot be resolved from the command line: the dashboard holds that project's repositories. Press \"Add locally\" on the project, or pass its weftos:// name or its git URL")]
    DashboardId,
    #[error("{0}")]
    Uri(String),
    #[error("{0} is not a project name (weftos://<mesh>/projects/<ULID>), a ULID or a git URL ({1})")]
    Unrecognised(String, &'static str),
    #[error("a mesh name carries no project name: pass --slug <name> (or --path <dir>)")]
    NeedsSlug,
    #[error("a bare ULID does not say which mesh: pass the full name weftos://<mesh>/projects/<ULID> (the project's Info shows it)")]
    NeedsMesh,
    #[error("slug {0:?} must be 1-64 of [a-z0-9-], starting with a letter or digit")]
    Slug(String),
    #[error("{0}")]
    Path(String),
    #[error("branch is not a plain ref name")]
    Branch,
}

fn is_uuid(s: &str) -> bool {
    let p: Vec<&str> = s.split('-').collect();
    p.len() == 5
        && p.iter().zip([8, 4, 4, 4, 12]).all(|(x, n)| x.len() == n && x.chars().all(|c| c.is_ascii_hexdigit()))
}

/// Tell the four spellings apart.
pub fn parse_spec(input: &str) -> Result<SourceSpec, SpecError> {
    let s = input.trim();
    if s.starts_with("weftos://") {
        let u = WeftosUri::parse(s).map_err(|e| SpecError::Uri(e.to_string()))?;
        let (ulid, dir) = u.project_repo().ok_or_else(|| SpecError::Uri("the name must be weftos://<mesh>/projects/<ULID>".into()))?;
        if dir != "." {
            return Err(SpecError::Uri("name the project (projects/<ULID>), not one of its repositories".into()));
        }
        return Ok(SourceSpec::Mesh { uri: s.to_owned(), mesh: u.authority, ulid: ulid.to_owned() });
    }
    if is_uuid(s) {
        return Err(SpecError::DashboardId);
    }
    if clawft_types::project::validate_id(s).is_ok() {
        return Ok(SourceSpec::Ulid(s.to_owned()));
    }
    url_ok(s).map_err(|why| SpecError::Unrecognised(redact_for_error(s), why))?;
    Ok(SourceSpec::Git(s.to_owned()))
}

/// Inputs are echoed in errors only without anything after an `@` before the host.
fn redact_for_error(s: &str) -> String {
    let s: String = s.chars().take(80).collect();
    match (s.find("://"), s.find('@')) {
        (Some(a), Some(b)) if b > a => format!("{}***{}", &s[..a + 3], &s[b..]),
        _ => s,
    }
}

/// The repository name of a git URL as a slug (`git@h:org/My_Repo.git` -> `my-repo`).
pub fn slug_from_git_url(url: &str) -> Option<String> {
    let last = url.trim_end_matches('/').rsplit(['/', ':']).next()?;
    slugify(&last.trim_end_matches(".git").replace('_', "-"))
}

/// The slug: `--slug`, else the `--path`'s last segment, else the git URL's name.
pub fn choose_slug(spec: &SourceSpec, slug: Option<&str>, path: Option<&Path>) -> Result<String, SpecError> {
    if let Some(s) = slug {
        return if valid_slug(s) { Ok(s.to_owned()) } else { Err(SpecError::Slug(s.to_owned())) };
    }
    if let Some(name) = path.and_then(Path::file_name).and_then(|n| n.to_str()) {
        return slugify(name).ok_or_else(|| SpecError::Slug(name.to_owned()));
    }
    match spec {
        SourceSpec::Git(url) => slug_from_git_url(url).ok_or(SpecError::NeedsSlug),
        _ => Err(SpecError::NeedsSlug),
    }
}

/// How the checkout will be fetched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchPlan {
    /// From the paired primary over the mesh (`project.fetch` through the user daemon).
    Mesh,
    /// From the project's own git remote.
    Git,
}

/// The fetcher for a spec. Mesh names go over the mesh only (they name no git
/// remote to fall back to); git URLs go to git. The refusal for "no primary
/// reachable" comes from the fetch itself and says what to do.
pub fn choose_fetcher(spec: &SourceSpec) -> FetchPlan {
    match spec {
        SourceSpec::Mesh { .. } | SourceSpec::Ulid(_) => FetchPlan::Mesh,
        SourceSpec::Git(_) => FetchPlan::Git,
    }
}

/// A validated request: the slug, the resolved target and the `InstallRequest`
/// the fetchers read. For a git URL the request carries no project ULID (the
/// project is registered under a fresh identity), so `project_ulid` is empty and
/// only the git fetcher, which never reads it, may be given such a request.
#[derive(Debug, Clone)]
pub struct AddPlan {
    pub spec: SourceSpec,
    pub slug: String,
    pub target: PathBuf,
    pub fetch: FetchPlan,
    pub request: InstallRequest,
}

/// Validate every argument of `project add` against `home`.
pub fn plan_add(input: &str, slug: Option<&str>, path: Option<&str>, branch: Option<&str>, home: &Path) -> Result<AddPlan, SpecError> {
    let spec = parse_spec(input)?;
    if matches!(spec, SourceSpec::Ulid(_)) {
        return Err(SpecError::NeedsMesh);
    }
    if branch.is_some_and(|b| !branch_ok(b)) {
        return Err(SpecError::Branch);
    }
    let path_arg = path.map(str::trim).filter(|p| !p.is_empty());
    let slug = choose_slug(&spec, slug, path_arg.map(Path::new))?;
    let target = resolve_path(path_arg, Some(&slug), home).map_err(|e| SpecError::Path(e.to_string()))?;
    let (project_ulid, sources) = match &spec {
        SourceSpec::Mesh { ulid, uri, .. } => {
            // Only the branch of the root repository is read from a mesh request's sources.
            (ulid.clone(), branch.map(|b| vec![SourceRepo { url: uri.clone(), branch: Some(b.to_owned()), dir: ".".into() }]).unwrap_or_default())
        }
        SourceSpec::Ulid(ulid) => (ulid.clone(), Vec::new()),
        SourceSpec::Git(url) => (String::new(), vec![SourceRepo { url: url.clone(), branch: branch.map(str::to_owned), dir: ".".into() }]),
    };
    let request = InstallRequest {
        project_ulid,
        target_path: Some(target.to_string_lossy().into_owned()),
        slug: Some(slug.clone()),
        sources,
        primary: None,
    };
    Ok(AddPlan { fetch: choose_fetcher(&spec), spec, slug, target, request })
}

#[cfg(test)]
#[path = "project_add_spec_tests.rs"]
mod tests;
