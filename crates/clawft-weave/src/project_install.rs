//! Project install contract (ADR-108 P3): the `install` action payload and the
//! [`ProjectFetcher`] seam that puts a project's repositories at a local path.
//!
//! Two fetchers plug in here:
//! - **git-remote** (P3a): clones each repository from the project's own git
//!   remotes (the dashboard's `source.<i>.*` rows) with the member's own git
//!   credentials.
//! - **mesh** (P3b): fetches from the project's primary node over the signed
//!   mesh channel (`project.fetch`, `git-remote-weftos`, tar stream), ADR-108
//!   decision 4. Needs the two nodes paired (P2b).
//!
//! The install handler tries fetchers in [`fetch_order`] and uses the first
//! whose [`ProjectFetcher::can_fetch`] is true, then registers the checkout as
//! an ADR-108 workspace (`weft project init --adopt <ULID> [--repo DIR ...]`).
//! Payloads come from the dashboard and are untrusted: [`InstallRequest::validate`]
//! runs before anything touches the disk.

use std::path::{Component, Path, PathBuf};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Most repositories one install may fetch (matches the reporter's cap).
pub const MAX_REPOS: usize = 8;

/// The `install` action payload, as the dashboard queues it.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InstallRequest {
    /// The project's WeftOS ULID (26 Crockford base32 characters).
    pub project_ulid: String,
    /// Target path; `~/` is expanded. The dashboard always sends one (its
    /// default is `~/Projects/<slug>`, owner decision D-B).
    #[serde(default)]
    pub target_path: Option<String>,
    /// The project's slug, used for the default path when `target_path` is absent.
    #[serde(default)]
    pub slug: Option<String>,
    /// The project's repositories as the dashboard knows them (`source.<i>.*`).
    #[serde(default)]
    pub sources: Vec<SourceRepo>,
    /// The project's primary node, when the dashboard knows it (mesh fetch).
    #[serde(default)]
    pub primary: Option<PrimaryRef>,
}

/// One repository of a project.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SourceRepo {
    /// Clone URL (`https://`, `ssh://` or scp-like `git@host:org/repo`). Never with credentials.
    pub url: String,
    #[serde(default)]
    pub branch: Option<String>,
    /// Directory relative to the install root: `.` for the root repository, or one
    /// plain path segment for a sibling (`../<name>` is not allowed here).
    #[serde(default = "root_dir")]
    pub dir: String,
}

fn root_dir() -> String {
    ".".into()
}

/// The project's primary node on the mesh.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PrimaryRef {
    /// The primary's mesh node id (as in `workload-peers.json`). Field names may
    /// not contain "key", "token", "secret" or "password": the dashboard's
    /// `node_actions.payload` check refuses them.
    pub node_id: String,
}

/// Why an install request was refused before anything ran.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum InstallRequestError {
    #[error("project_ulid is not a ULID")]
    Ulid,
    #[error("slug must be 1-64 of [a-z0-9-]")]
    Slug,
    #[error("path must be absolute (or start with ~/) and stay inside the home directory")]
    Path,
    #[error("at most {MAX_REPOS} repositories")]
    TooManyRepos,
    #[error("source {0}: {1}")]
    Source(usize, &'static str),
    #[error("no source and no primary: nothing to fetch from")]
    NothingToFetch,
}

fn is_ulid(s: &str) -> bool {
    s.len() == 26 && s.chars().all(|c| matches!(c, '0'..='9' | 'A'..='H' | 'J' | 'K' | 'M' | 'N' | 'P'..='T' | 'V'..='Z'))
}

pub(crate) fn url_ok(url: &str) -> Result<(), &'static str> {
    if url.len() > 512 || url.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err("url has whitespace or control characters");
    }
    if url.starts_with('-') {
        return Err("url may not start with '-'");
    }
    if let Some(rest) = url.strip_prefix("https://").or_else(|| url.strip_prefix("ssh://")) {
        let host = rest.split('/').next().unwrap_or("");
        if host.contains('@') && url.starts_with("https://") {
            return Err("url carries credentials");
        }
        if host.contains(':') && url.starts_with("https://") && host.rsplit(':').next().is_some_and(|p| !p.chars().all(|c| c.is_ascii_digit())) {
            return Err("url carries credentials");
        }
        return if host.is_empty() { Err("url has no host") } else { Ok(()) };
    }
    // scp-like: user@host:path, no scheme, no "::" (git's transport helper syntax).
    if !url.contains("://") && !url.contains("::") && url.split_once(':').is_some_and(|(h, p)| !h.is_empty() && !p.is_empty() && !h.contains('/')) {
        return Ok(());
    }
    Err("url must be https://, ssh:// or user@host:path")
}

/// `.` or one plain path segment (shared with the mesh fetcher's URL and layout checks).
pub(crate) fn dir_ok(dir: &str) -> bool {
    dir == "." || (!dir.is_empty() && dir.len() <= 64 && !dir.starts_with('.') && dir.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')))
}

pub(crate) fn branch_ok(b: &str) -> bool {
    !b.is_empty() && b.len() <= 200 && !b.starts_with('-') && !b.contains("..") && b.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '_' | '.'))
}

impl InstallRequest {
    /// Check every field and resolve the target path against `home`.
    pub fn validate(&self, home: &Path) -> Result<PathBuf, InstallRequestError> {
        if !is_ulid(&self.project_ulid) {
            return Err(InstallRequestError::Ulid);
        }
        if let Some(slug) = &self.slug
            && (slug.is_empty() || slug.len() > 64 || !slug.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'))
        {
            return Err(InstallRequestError::Slug);
        }
        if self.sources.len() > MAX_REPOS {
            return Err(InstallRequestError::TooManyRepos);
        }
        let mut dirs = std::collections::HashSet::new();
        for (i, s) in self.sources.iter().enumerate() {
            url_ok(&s.url).map_err(|e| InstallRequestError::Source(i, e))?;
            if !dir_ok(&s.dir) {
                return Err(InstallRequestError::Source(i, "dir must be '.' or one plain path segment"));
            }
            if !dirs.insert(s.dir.as_str()) {
                return Err(InstallRequestError::Source(i, "two sources share a dir"));
            }
            if s.branch.as_deref().is_some_and(|b| !branch_ok(b)) {
                return Err(InstallRequestError::Source(i, "branch is not a plain ref name"));
            }
        }
        if self.sources.is_empty() && self.primary.is_none() {
            return Err(InstallRequestError::NothingToFetch);
        }
        resolve_path(self.target_path.as_deref(), self.slug.as_deref(), home)
    }
}

/// `~/Projects/<slug>` by default; otherwise the given path with `~/` expanded.
/// The result is absolute, normal (no `.`/`..`) and strictly inside `home`.
pub fn resolve_path(path: Option<&str>, slug: Option<&str>, home: &Path) -> Result<PathBuf, InstallRequestError> {
    let p = match path.map(str::trim).filter(|p| !p.is_empty()) {
        None => home.join("Projects").join(slug.ok_or(InstallRequestError::Path)?),
        Some(p) if p.starts_with("~/") => home.join(&p[2..]),
        Some(p) => PathBuf::from(p),
    };
    if !p.is_absolute() || p.components().any(|c| matches!(c, Component::ParentDir | Component::CurDir)) {
        return Err(InstallRequestError::Path);
    }
    if p == home || !p.starts_with(home) {
        return Err(InstallRequestError::Path);
    }
    Ok(p)
}

/// One repository a fetcher put on disk.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FetchedRepo {
    /// `.` or the sibling directory name, as in [`SourceRepo::dir`].
    pub dir: String,
    /// First 8 characters of the checked-out commit.
    pub head: Option<String>,
    /// The `origin` URL left in the clone (credential-free).
    pub remote: Option<String>,
}

/// What a fetch did; goes into the action result (no file contents, ever).
#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct FetchReport {
    pub fetcher: &'static str,
    pub repos: Vec<FetchedRepo>,
    /// Bytes written for non-git content (mesh tar stream); 0 for git-only.
    pub bytes: u64,
    /// Paths the primary marked for archiving and did not send (owner decision D-C).
    pub archived: Vec<String>,
    /// What the member should know but which did not fail the fetch: very large
    /// unmarked non-git content left on the primary (D-C "warn before fetching"),
    /// symlinks or credential-shaped files not sent. Empty for git-only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// Puts a project's repositories under `dest` (which does not exist yet or is empty).
#[async_trait]
pub trait ProjectFetcher: Send + Sync {
    /// `git-remote` or `mesh`.
    fn name(&self) -> &'static str;
    /// Whether this fetcher can serve the request on this node right now
    /// (for mesh: a primary is named and is a paired peer with fetch access).
    fn can_fetch(&self, req: &InstallRequest) -> bool;
    async fn fetch(&self, req: &InstallRequest, dest: &Path) -> Result<FetchReport, String>;
}

/// Mesh first (ADR-108 decision 4), then the project's own git remotes.
pub fn fetch_order<'a>(fetchers: &'a [std::sync::Arc<dyn ProjectFetcher>], req: &InstallRequest) -> Option<&'a std::sync::Arc<dyn ProjectFetcher>> {
    let rank = |f: &std::sync::Arc<dyn ProjectFetcher>| if f.name() == "mesh" { 0 } else { 1 };
    let mut ok: Vec<_> = fetchers.iter().filter(|f| f.can_fetch(req)).collect();
    ok.sort_by_key(|f| rank(f));
    ok.into_iter().next()
}

#[cfg(test)]
#[path = "project_install_tests.rs"]
mod tests;
