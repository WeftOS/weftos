//! Path checks for the `store.worktree.*` verbs (ADR-126 S2, adversarial review F1).
//!
//! The verbs carry filesystem paths that the daemon reads or writes as its own user. The ADR
//! says the CLI and the daemon share a host and a user, but nothing enforced it, and an owner
//! token can reach the verbs from elsewhere. So every path is checked here, whoever sends it:
//! absolute, no `..`, resolved through symlinks, and kept away from the daemon's own state.
//!
//! "Protected" paths are the daemon's runtime directory (which holds `store-secrets/` and, with the
//! local backend, the store itself) and the RustFS environment file. A verb may not read from or
//! write into them, and a destination may not contain them.

use std::path::{Component, Path, PathBuf};

/// What a path will be used for.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Use {
    /// An existing directory that is read (the worktree root).
    ReadDir,
    /// An existing regular file that is read (a list or a bundle).
    ReadFile,
    /// A directory files are written into; created when missing.
    WriteDir,
    /// A file that is written; its final component must not be a symlink.
    WriteFile,
}

/// The paths a verb must never touch.
#[derive(Debug, Clone, Default)]
pub struct Guard {
    protected: Vec<PathBuf>,
}

impl Guard {
    /// No protected paths (tests; a daemon with no runtime directory).
    pub fn none() -> Self {
        Self::default()
    }

    /// Protect `paths`. Each is resolved now; one that does not exist yet is kept as given.
    pub fn new<I: IntoIterator<Item = PathBuf>>(paths: I) -> Self {
        Self { protected: paths.into_iter().map(|p| resolve_lenient(&p).unwrap_or(p)).collect() }
    }

    /// The daemon's own: its runtime directory and RustFS environment file.
    pub fn from_runtime() -> Self {
        Self::new(crate::store_rt::protected_paths())
    }

    fn inside(&self, p: &Path) -> bool {
        self.protected.iter().any(|g| p.starts_with(g))
    }

    fn contains(&self, p: &Path) -> bool {
        self.protected.iter().any(|g| g.starts_with(p))
    }

    /// Whether `p` (resolved) is inside a protected path.
    pub fn is_protected(&self, p: &Path) -> bool {
        self.inside(&resolve_lenient(p).unwrap_or_else(|_| p.to_path_buf()))
    }

    /// Check and resolve one path param. Returns the canonical path to use.
    pub fn check(&self, raw: &str, what: &str, how: Use) -> Result<PathBuf, String> {
        let p = Path::new(raw);
        if !p.is_absolute() {
            return Err(format!("--{what} must be an absolute path"));
        }
        if p.components().any(|c| matches!(c, Component::ParentDir | Component::CurDir)) {
            return Err(format!("--{what} must not contain '.' or '..'"));
        }
        // A symlink as the last component is never followed for a write, and never accepted for
        // a read of a file the caller named: the check and the use must see the same thing.
        if matches!(how, Use::WriteFile | Use::ReadFile)
            && std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink())
        {
            return Err(format!("--{what} is a symlink; refused"));
        }
        let c = resolve_lenient(p).map_err(|e| format!("--{what}: {e}"))?;
        if self.inside(&c) {
            return Err(format!("--{what} is inside the daemon's own state; refused"));
        }
        match how {
            Use::ReadDir if !c.is_dir() => return Err(format!("--{what} must be an existing directory")),
            Use::ReadFile if !c.is_file() => return Err(format!("--{what} must be an existing regular file")),
            Use::WriteDir if self.contains(&c) => {
                return Err(format!("--{what} would contain the daemon's own state; refused"));
            }
            Use::WriteDir if c.exists() && !c.is_dir() => return Err(format!("--{what} is not a directory")),
            Use::WriteFile if c.is_dir() => return Err(format!("--{what} is a directory")),
            _ => {}
        }
        Ok(c)
    }
}

/// Resolve `p` through symlinks as far as it exists, then append the rest unchanged. The rest has
/// no `..` (the caller refused those), so it cannot climb out.
fn resolve_lenient(p: &Path) -> std::io::Result<PathBuf> {
    let mut rest: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = p.to_path_buf();
    loop {
        match cur.canonicalize() {
            Ok(mut c) => {
                c.extend(rest.iter().rev());
                return Ok(c);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let Some(name) = cur.file_name().map(|n| n.to_owned()) else { return Err(e) };
                rest.push(name);
                if !cur.pop() {
                    return Err(e);
                }
            }
            Err(e) => return Err(e),
        }
    }
}

#[cfg(test)]
#[path = "store_worktree_paths_tests.rs"]
mod tests;
