//! Where agent packages come from: the tree embedded at build time, or an
//! `agents/` directory on disk (`--from`).

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::InitError;

mod embedded {
    include!(concat!(env!("OUT_DIR"), "/embedded_agents.rs"));
}

/// Git commit the binary was built from (`unknown` outside a git checkout).
pub const BUILD_COMMIT: &str = env!("WEFTOS_GIT_COMMIT");

/// Legacy `agents/` directories that predate the package standard.
pub const LEGACY_DIRS: &[&str] = &[
    "clawft",
    "code-reviewer",
    "weftos",
    "weftos-ecc",
    "weftos-kernel",
    "weftos-mesh",
];

/// A flat `agents/`-relative file table (forward-slash paths).
pub struct AgentSource {
    pub files: BTreeMap<String, Cow<'static, [u8]>>,
    /// `embedded` or the `--from` directory.
    pub origin: String,
    pub commit: String,
}

impl AgentSource {
    /// The package tree compiled into this binary.
    pub fn embedded() -> Self {
        let files = embedded::EMBEDDED_AGENTS
            .iter()
            .map(|(p, b)| ((*p).to_string(), Cow::Borrowed(*b)))
            .collect();
        Self {
            files,
            origin: "embedded".into(),
            commit: BUILD_COMMIT.into(),
        }
    }

    /// Read an `agents/` tree from disk. Applies the same filter as the
    /// build-time embed: package dirs and `teams/` only, no `evals/`.
    pub fn from_dir(dir: &Path) -> Result<Self, InitError> {
        if !dir.is_dir() {
            return Err(InitError::Other(format!(
                "--from {}: not a directory",
                dir.display()
            )));
        }
        let mut files = BTreeMap::new();
        for entry in fs::read_dir(dir)?.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if !path.is_dir() || name.starts_with('.') || LEGACY_DIRS.contains(&name.as_str()) {
                continue;
            }
            if name == "teams" || path.join("weftos-package.yaml").is_file() {
                walk(&path, &name, &mut files)?;
            }
        }
        Ok(Self {
            files,
            origin: dir.display().to_string(),
            commit: git_commit(dir).unwrap_or_else(|| "unknown".into()),
        })
    }

    /// Top-level package ids (dirs holding `weftos-package.yaml`), sorted.
    /// A package whose sidecar says `state: inactive` (ADR-121) is never
    /// listed, so it is never selected, planned or applied.
    pub fn package_ids(&self) -> Vec<String> {
        self.files
            .iter()
            .filter_map(|(p, b)| p.strip_suffix("/weftos-package.yaml").map(|id| (id, b)))
            .filter(|(id, _)| !id.contains('/') && !LEGACY_DIRS.contains(id))
            .filter(|(_, manifest)| !is_inactive(manifest))
            .map(|(id, _)| id.to_string())
            .collect()
    }

    pub fn get(&self, rel: &str) -> Option<&[u8]> {
        self.files.get(rel).map(|b| b.as_ref())
    }

    /// Files under `prefix/` as `(path relative to prefix, bytes)`.
    pub fn under<'a>(&'a self, prefix: &'a str) -> impl Iterator<Item = (&'a str, &'a [u8])> + 'a {
        let dir = format!("{prefix}/");
        self.files
            .iter()
            .filter_map(move |(p, b)| p.strip_prefix(dir.as_str()).map(|r| (r, b.as_ref())))
    }
}

/// True when a package sidecar declares `state: inactive`. An unreadable
/// sidecar is not inactive: loading it later reports the parse error.
fn is_inactive(manifest: &[u8]) -> bool {
    serde_yaml::from_slice::<serde_yaml::Value>(manifest)
        .ok()
        .and_then(|v| v.get("state").and_then(|s| s.as_str().map(|s| s == "inactive")))
        .unwrap_or(false)
}

fn walk(
    dir: &Path,
    rel: &str,
    files: &mut BTreeMap<String, Cow<'static, [u8]>>,
) -> Result<(), InitError> {
    for entry in fs::read_dir(dir)?.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') || name == "evals" {
            continue;
        }
        let path: PathBuf = entry.path();
        let child = format!("{rel}/{name}");
        if path.is_dir() {
            walk(&path, &child, files)?;
        } else if path.is_file() {
            files.insert(child, Cow::Owned(fs::read(&path)?));
        }
    }
    Ok(())
}

fn git_commit(dir: &Path) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (out.status.success() && !s.is_empty()).then_some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src(files: &[(&str, &str)]) -> AgentSource {
        AgentSource {
            files: files.iter().map(|(p, t)| (p.to_string(), Cow::Owned(t.as_bytes().to_vec()))).collect(),
            origin: "test".into(),
            commit: "test".into(),
        }
    }

    #[test]
    fn inactive_packages_are_not_applied() {
        let s = src(&[
            ("a/weftos-package.yaml", "name: a\n"),
            ("b/weftos-package.yaml", "name: b\nstate: active\n"),
            ("c/weftos-package.yaml", "name: c\nstate: inactive\n"),
        ]);
        assert_eq!(s.package_ids(), vec!["a", "b"]);
    }
}
