//! What `weaver project add` created, so `remove` can undo exactly that.
//!
//! One JSON file per slug in `~/.weftos/dev-projects/` (mode 0600, written
//! atomically). Holds paths, the ULID and the route prefix; no secrets, no
//! environment values.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Directory under `~/.weftos` that holds the receipts.
pub const RECEIPTS_DIR: &str = "dev-projects";

/// The record of one `add`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipt {
    pub slug: String,
    pub root: PathBuf,
    /// Sibling repositories outside the root that this add fetched.
    #[serde(default)]
    pub repos: Vec<PathBuf>,
    /// The registered project's ULID (the manifest `add` created).
    pub ulid: String,
    /// `workspace` (adopted) or `project` (registered under a fresh identity).
    pub registered_as: String,
    /// The overlay route this add wrote (`~/.weftos/routes/<ULID>.yaml`), if any.
    #[serde(default)]
    pub route_prefix: Option<String>,
    pub fetcher: String,
    pub added_at: String,
}

/// `~/.weftos/dev-projects` for `home`.
pub fn receipts_dir(home: &Path) -> PathBuf {
    clawft_types::runtime_paths::user_weftos_dir(home).join(RECEIPTS_DIR)
}

fn path_of(dir: &Path, slug: &str) -> Result<PathBuf, String> {
    if !crate::router_routes::valid_slug(slug) {
        return Err(format!("{slug:?} is not a project slug ([a-z0-9-])"));
    }
    Ok(dir.join(format!("{slug}.json")))
}

/// Write (replace) the receipt of `r.slug`.
pub fn write(dir: &Path, r: &Receipt) -> Result<(), String> {
    let path = path_of(dir, &r.slug)?;
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let text = serde_json::to_string_pretty(r).map_err(|e| e.to_string())?;
    crate::dashboard_token::write_atomic(&path, &text).map_err(|e| format!("{}: {e}", path.display()))
}

/// The receipt of `slug`; `Ok(None)` when this machine never added it.
pub fn read(dir: &Path, slug: &str) -> Result<Option<Receipt>, String> {
    let path = path_of(dir, slug)?;
    match std::fs::read_to_string(&path) {
        Ok(t) => serde_json::from_str(&t).map(Some).map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// Delete the receipt of `slug`. `Ok(false)` when there was none.
pub fn delete(dir: &Path, slug: &str) -> Result<bool, String> {
    let path = path_of(dir, slug)?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// Every receipt (unreadable ones skipped), by slug.
pub fn list(dir: &Path) -> Vec<Receipt> {
    let mut out: Vec<Receipt> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| serde_json::from_str(&std::fs::read_to_string(e.path()).ok()?).ok())
        .collect();
    out.sort_by(|a: &Receipt, b| a.slug.cmp(&b.slug));
    out
}
