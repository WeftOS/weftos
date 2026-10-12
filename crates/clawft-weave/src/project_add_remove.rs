//! `weaver project remove`: undo exactly what `project add` created.
//!
//! The receipt ([`crate::project_add_receipt`]) says what that was: the route
//! this add wrote into the overlay, and the manifest it registered. Both go.
//! The checkout stays unless `--purge`, which also refuses to delete a tree
//! with uncommitted changes (`--force` overrides) or one outside the home
//! directory. A route the repository declares in `compose/ports.yaml` is part
//! of the checkout and is never touched.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::project_add_receipt::{self as receipt, Receipt, receipts_dir};
use crate::project_install::resolve_path;
use crate::project_install_handlers::InstallEnv;
use crate::router_overlay;

/// What a remove did.
#[derive(Debug, Clone, Serialize)]
pub struct RemoveReport {
    pub slug: String,
    pub undone: Vec<String>,
    pub kept: Vec<String>,
}

/// Directories of the add that hold uncommitted changes (`git status --porcelain`).
fn dirty(dirs: &[&Path]) -> Vec<PathBuf> {
    dirs.iter()
        .filter(|d| d.join(".git").exists())
        .filter(|d| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(d)
                .args(["status", "--porcelain", "--untracked-files=normal"])
                .stdin(std::process::Stdio::null())
                .output()
                .map(|o| !o.status.success() || !o.stdout.is_empty())
                .unwrap_or(true)
        })
        .map(|d| d.to_path_buf())
        .collect()
}

fn purge_paths(env: &InstallEnv, r: &Receipt) -> Result<Vec<PathBuf>, String> {
    let mut all = vec![r.root.clone()];
    all.extend(r.repos.iter().cloned());
    for p in &all {
        let s = p.to_str().ok_or("a checkout path is not UTF-8")?;
        resolve_path(Some(s), None, &env.home).map_err(|_| format!("refusing to delete {}: it is not strictly inside your home directory", p.display()))?;
    }
    Ok(all)
}

/// Undo the add of `slug`. Nothing is changed when a refusal is returned.
pub fn remove(env: &InstallEnv, slug: &str, purge: bool, force: bool) -> Result<RemoveReport, String> {
    let rdir = receipts_dir(&env.home);
    let r = receipt::read(&rdir, slug)?.ok_or_else(|| format!("no project {slug:?} was added on this machine with `weaver project add`"))?;
    let doomed = if purge { purge_paths(env, &r)? } else { Vec::new() };
    if purge && !force {
        let d = dirty(&doomed.iter().map(PathBuf::as_path).collect::<Vec<_>>());
        if !d.is_empty() {
            return Err(format!(
                "refusing to --purge: uncommitted changes in {} (commit or stash them, or pass --force)",
                d.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
            ));
        }
    }
    let mut out = RemoveReport { slug: slug.to_owned(), undone: Vec::new(), kept: Vec::new() };
    if let Some(prefix) = &r.route_prefix {
        let dir = router_overlay::overlays_dir(&env.home);
        match router_overlay::remove_route(&dir, &r.ulid, prefix)? {
            true => out.undone.push(format!("removed route {prefix}")),
            false => out.kept.push(format!("route {prefix} was already gone")),
        }
        router_overlay::delete_if_empty(&dir, &r.ulid)?;
    }
    let mpath = clawft_types::project::manifest_path(&env.manifests_dir, &r.ulid).map_err(|e| e.to_string())?;
    match clawft_types::project::find_by_id(&env.manifests_dir, &r.ulid).map_err(|e| format!("cannot read the project index: {e}"))? {
        Some(m) if m.root == r.root || m.root.canonicalize().ok() == r.root.canonicalize().ok() => {
            std::fs::remove_file(&mpath).map_err(|e| format!("cannot unregister: {e}"))?;
            out.undone.push(format!("unregistered project {}", r.ulid));
        }
        Some(m) => out.kept.push(format!("project {} is registered at {} now, not at {}; left registered", r.ulid, m.root.display(), r.root.display())),
        None => out.kept.push(format!("project {} was already unregistered", r.ulid)),
    }
    if purge {
        for p in &doomed {
            match std::fs::remove_dir_all(p) {
                Ok(()) => out.undone.push(format!("deleted {}", p.display())),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => out.kept.push(format!("{} was already gone", p.display())),
                Err(e) => return Err(format!("could not delete {}: {e} (route and registration are already undone)", p.display())),
            }
        }
    } else {
        out.kept.push(format!("checkout {} (pass --purge to delete it)", r.root.display()));
        for p in &r.repos {
            out.kept.push(format!("checkout {}", p.display()));
        }
    }
    receipt::delete(&rdir, slug)?;
    out.undone.push("forgot the add record".into());
    Ok(out)
}
