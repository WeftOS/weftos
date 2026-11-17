//! `project.*` RPCs (ADR-103 Phase 1, package D).
//!
//! Read and write the per-user project manifests (`~/.weftos/projects/`)
//! through the `clawft-types` project store. Registered in
//! `rpc_ext::ROUTES` with explicit capabilities:
//!
//! | method             | capability | params                       |
//! |--------------------|------------|------------------------------|
//! | `project.list`     | `Read`     | none                         |
//! | `project.show`     | `Read`     | `{id}` or `{root}`           |
//! | `project.register` | `Admin`    | `{root, name?}` (local owner)|
//!
//! `register` adopts a project root into the manifest store exactly like
//! `weft project init` (`adopt_or_init`): an existing `project.toml` id
//! wins, a seeded manifest is adopted, else a ULID is minted. It refuses a
//! relative root, `/`, `$HOME`, and a root whose id is registered for a
//! different live root.
//!
//! The handlers work on an injected manifests dir; only the route glue
//! reads the daemon's configured one.

use std::path::{Path, PathBuf};
use std::sync::RwLock;

use clawft_rpc::Response;
use clawft_types::project::{ProjectError, adopt_or_init, find_by_id, find_by_root, list_manifests};
use serde_json::{Value, json};

use crate::rpc_ext::{ExtCall, ExtFuture};

static MANIFESTS: RwLock<Option<PathBuf>> = RwLock::new(None);

/// Point the handlers at a manifests dir (daemon startup, tests).
pub fn init_manifests_dir(dir: PathBuf) {
    *MANIFESTS.write().expect("MANIFESTS lock poisoned") = Some(dir);
}

pub(crate) fn configured_dir() -> Option<PathBuf> {
    MANIFESTS
        .read()
        .expect("MANIFESTS lock poisoned")
        .clone()
        .or_else(|| {
            // Unit tests never fall back to the real home's store; they pin
            // it with `init_manifests_dir` (integration tests do too).
            if cfg!(test) {
                return None;
            }
            clawft_types::runtime_paths::home_dir().map(|h| crate::user_daemon::manifests_dir(&h))
        })
}

fn no_store() -> Response {
    Response::error_with_kind(
        "project_store_unavailable",
        "cannot locate the project manifest store (no home directory)",
    )
}

fn project_error(e: &ProjectError) -> Response {
    let kind = match e {
        ProjectError::RootConflict { .. } => "root_conflict",
        ProjectError::BadRoot(_) => "bad_root",
        ProjectError::InvalidId(_) => "invalid_project",
        _ => "project_error",
    };
    Response::error_with_kind(kind, e.to_string())
}

/// `project.list`.
pub fn list(dir: &Path) -> Response {
    match list_manifests(dir) {
        Ok(l) => Response::success(json!({
            "projects": l.manifests,
            "skipped": l.skipped.iter()
                .map(|(p, why)| json!({"path": p.display().to_string(), "reason": why}))
                .collect::<Vec<_>>(),
        })),
        Err(e) => project_error(&e),
    }
}

/// `project.show`: by `id`, or by `root`.
pub fn show(dir: &Path, params: &Value) -> Response {
    let found = match (params.get("id").and_then(Value::as_str), params.get("root").and_then(Value::as_str)) {
        (Some(id), _) => find_by_id(dir, id),
        (None, Some(root)) => find_by_root(dir, Path::new(root)),
        (None, None) => {
            return Response::error_with_kind("invalid_params", "project.show needs `id` or `root`");
        }
    };
    match found {
        Ok(Some(m)) => Response::success(json!({ "project": m })),
        Ok(None) => Response::error_with_kind("project_not_found", "no such project"),
        Err(e) => project_error(&e),
    }
}

/// Why `canon` must not become a project root, if it must not: the
/// filesystem root and its direct children (`/etc`, `/Users`), `$HOME` and
/// every ancestor of it, anything inside `~/.weftos` or `~/.clawft`, and
/// dot-dirs directly under `$HOME` (`~/.ssh`, `~/.config`).
fn refuse_root(canon: &Path, home: Option<&Path>) -> Option<&'static str> {
    if canon.parent().is_none_or(|p| p.parent().is_none()) {
        return Some("refusing to register the filesystem root or a top-level system directory");
    }
    let home = home?;
    let home = home.canonicalize().unwrap_or_else(|_| home.to_path_buf());
    if home.starts_with(canon) {
        return Some("refusing to register $HOME or one of its ancestors as a project");
    }
    for state in [".weftos", ".clawft"] {
        if canon.starts_with(home.join(state)) {
            return Some("refusing to register WeftOS's own state directory as a project");
        }
    }
    let dot_dir_under_home = canon.parent() == Some(home.as_path())
        && canon
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with('.'));
    dot_dir_under_home.then_some("refusing to register a dot-directory under $HOME as a project")
}

/// `project.register`: adopt `root` into the manifest store.
pub fn register(dir: &Path, home: Option<&Path>, params: &Value) -> Response {
    let Some(root) = params.get("root").and_then(Value::as_str) else {
        return Response::error_with_kind("invalid_params", "project.register needs `root`");
    };
    let root = Path::new(root);
    if !root.is_absolute() {
        return Response::error_with_kind("bad_root", format!("{}: root must be absolute", root.display()));
    }
    let canon = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    if let Some(why) = refuse_root(&canon, home) {
        return Response::error_with_kind("bad_root", format!("{}: {why}", canon.display()));
    }
    let name = params.get("name").and_then(Value::as_str);
    match adopt_or_init(&canon, dir, name) {
        Ok(m) => {
            // A (re-)registration can change the entry `shared.*` cached the
            // limits of (state, `[shared]`): the next shared call reloads it.
            #[cfg(any(unix, windows))]
            crate::shared_state::invalidate(&m.id);
            Response::success(json!({ "project": m }))
        }
        Err(e) => project_error(&e),
    }
}

fn run_blocking(f: impl FnOnce() -> Response + Send + 'static) -> ExtFuture {
    Box::pin(async move {
        tokio::task::spawn_blocking(f)
            .await
            .unwrap_or_else(|e| Response::error(format!("project rpc task failed: {e}")))
    })
}

/// `project.list` route handler (`Read`).
pub fn handle_list(_call: ExtCall) -> ExtFuture {
    match configured_dir() {
        Some(dir) => run_blocking(move || list(&dir)),
        None => Box::pin(async { no_store() }),
    }
}

/// `project.show` route handler (`Read`).
pub fn handle_show(call: ExtCall) -> ExtFuture {
    match configured_dir() {
        Some(dir) => run_blocking(move || show(&dir, &call.params)),
        None => Box::pin(async { no_store() }),
    }
}

/// `project.register` route handler (`Admin`).
pub fn handle_register(call: ExtCall) -> ExtFuture {
    match configured_dir() {
        Some(dir) => run_blocking(move || {
            let home = clawft_types::runtime_paths::home_dir();
            register(&dir, home.as_deref(), &call.params)
        }),
        None => Box::pin(async { no_store() }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dirs() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
        let t = tempfile::tempdir().unwrap();
        let mdir = t.path().join("home/.weftos/projects");
        let home = t.path().join("home");
        let proj = t.path().join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        (t, mdir, home, proj)
    }

    fn ok(r: Response) -> Value {
        assert!(r.ok, "{:?}", r.error);
        r.result.unwrap()
    }

    #[test]
    fn list_of_a_missing_store_is_empty() {
        let (_t, mdir, _h, _p) = dirs();
        let v = ok(list(&mdir));
        assert_eq!(v["projects"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn register_then_list_and_show() {
        let (_t, mdir, home, proj) = dirs();
        let v = ok(register(&mdir, Some(&home), &json!({"root": proj, "name": "demo"})));
        let id = v["project"]["id"].as_str().unwrap().to_owned();
        assert_eq!(v["project"]["name"], "demo");
        assert!(proj.join(".weftos/project.toml").is_file());

        // Idempotent: same id again.
        let again = ok(register(&mdir, Some(&home), &json!({"root": proj})));
        assert_eq!(again["project"]["id"], id.as_str());

        let l = ok(list(&mdir));
        assert_eq!(l["projects"].as_array().unwrap().len(), 1);
        assert_eq!(ok(show(&mdir, &json!({"id": id})))["project"]["id"], id.as_str());
        assert_eq!(ok(show(&mdir, &json!({"root": proj})))["project"]["id"], id.as_str());
    }

    #[test]
    fn show_errors_are_structured() {
        let (_t, mdir, _h, _p) = dirs();
        let r = show(&mdir, &json!({}));
        assert_eq!(r.error_kind.as_deref(), Some("invalid_params"));
        let r = show(&mdir, &json!({"id": "01J0000000000000000000000A"}));
        assert_eq!(r.error_kind.as_deref(), Some("project_not_found"));
        let r = show(&mdir, &json!({"id": "../x"}));
        assert_eq!(r.error_kind.as_deref(), Some("invalid_project"));
    }

    #[test]
    fn register_refuses_relative_home_root_and_missing_dir() {
        let (t, mdir, home, _p) = dirs();
        std::fs::create_dir_all(&home).unwrap();
        for bad in [json!({"root": "rel/dir"}), json!({"root": home}), json!({"root": "/"}), json!({"root": t.path().join("nope")}), json!({})] {
            let r = register(&mdir, Some(&home), &bad);
            assert!(!r.ok, "{bad}");
        }
        assert_eq!(register(&mdir, Some(&home), &json!({"root": home})).error_kind.as_deref(), Some("bad_root"));
        assert!(!mdir.exists() || list_manifests(&mdir).unwrap().manifests.is_empty());
    }

    #[test]
    fn register_refuses_ancestors_state_dirs_and_dot_dirs_under_home() {
        let (t, mdir, home, _p) = dirs();
        for d in [".ssh", ".config", ".weftos/run", ".weftos", ".clawft"] {
            std::fs::create_dir_all(home.join(d)).unwrap();
        }
        let parent = home.parent().unwrap().to_path_buf();
        for bad in [
            parent,
            home.join(".ssh"),
            home.join(".config"),
            home.join(".weftos"),
            home.join(".weftos/run"),
            home.join(".clawft"),
        ] {
            let r = register(&mdir, Some(&home), &json!({"root": bad}));
            assert_eq!(r.error_kind.as_deref(), Some("bad_root"), "{}", bad.display());
            assert!(!bad.join(".weftos/project.toml").exists(), "{}", bad.display());
        }
        // A normal directory under home, and a non-dot subdir, are fine.
        let ok_dir = home.join("work/app");
        std::fs::create_dir_all(&ok_dir).unwrap();
        assert!(register(&mdir, Some(&home), &json!({"root": ok_dir})).ok);
        let _ = t;
    }

    #[test]
    fn register_refuses_a_copy_of_a_registered_project() {
        let (t, mdir, home, proj) = dirs();
        ok(register(&mdir, Some(&home), &json!({"root": proj})));
        let copy = t.path().join("copy");
        std::fs::create_dir_all(copy.join(".weftos")).unwrap();
        std::fs::copy(proj.join(".weftos/project.toml"), copy.join(".weftos/project.toml")).unwrap();
        let r = register(&mdir, Some(&home), &json!({"root": copy}));
        assert_eq!(r.error_kind.as_deref(), Some("root_conflict"));
    }
}
