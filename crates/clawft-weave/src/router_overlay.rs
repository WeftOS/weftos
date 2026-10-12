//! Dashboard route overlays (ADR-116 R3): `~/.weftos/routes/<ULID>.yaml`, one
//! per registered project, holding a `routes:` list in the same shape as
//! `compose/ports.yaml`. The dashboard's `route` action writes them (mode 0600,
//! atomic); the router merges them after the project's repository routes, so
//! the repository wins on the same prefix. A repository file is never edited.
//!
//! Writes go through `serde_yaml::Value` so that keys this build does not
//! know, and the other routes in the file, are preserved as written.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_yaml::{Mapping, Value};

use crate::router_routes::{MAX_ROUTES_PER_PROJECT, Refused, Route, RouteDecl, Source, admit_decl, normalize_prefix};

/// Directory under `~/.weftos` that holds the overlays.
pub const OVERLAYS_DIR: &str = "routes";
/// Largest overlay read.
pub const MAX_OVERLAY_BYTES: u64 = 64 * 1024;

/// `~/.weftos/routes` for `home`.
pub fn overlays_dir(home: &Path) -> PathBuf {
    clawft_types::runtime_paths::user_weftos_dir(home).join(OVERLAYS_DIR)
}

/// The overlay file of a project.
pub fn overlay_path(dir: &Path, ulid: &str) -> PathBuf {
    dir.join(format!("{ulid}.yaml"))
}

#[derive(Debug, Default, Deserialize)]
struct OverlayFile {
    #[serde(default)]
    routes: Vec<RouteDecl>,
}

/// Parse an overlay for project `slug`: every route carries `source: dashboard`.
/// A file that does not parse refuses the whole overlay; a bad route only itself.
pub fn parse_overlay(text: &str, slug: &str) -> (Vec<Route>, Vec<Refused>) {
    let refused_file = |reason: String| {
        (Vec::new(), vec![Refused { project: slug.to_owned(), prefix: "-".into(), port: 0, reason, source: Source::Dashboard }])
    };
    let file: OverlayFile = match serde_yaml::from_str(text) {
        Ok(f) => f,
        Err(e) => return refused_file(format!("routes overlay: {e}")),
    };
    let mut routes = Vec::new();
    let mut refused = Vec::new();
    if file.routes.len() > MAX_ROUTES_PER_PROJECT {
        refused.push(Refused { project: slug.to_owned(), prefix: "-".into(), port: 0, reason: format!("routes overlay: more than {MAX_ROUTES_PER_PROJECT} routes"), source: Source::Dashboard });
    }
    for d in file.routes.into_iter().take(MAX_ROUTES_PER_PROJECT) {
        match admit_decl(d, slug, Source::Dashboard) {
            Ok(r) => routes.push(r),
            Err(x) => refused.push(x),
        }
    }
    (routes, refused)
}

/// Read the overlay file as a YAML mapping (absent: empty).
fn read_mapping(path: &Path) -> Result<Mapping, String> {
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Mapping::new()),
        Err(e) => return Err(format!("{}: {e}", path.display())),
        Ok(m) if !m.is_file() => return Err(format!("{} is not a regular file", path.display())),
        Ok(m) if m.len() > MAX_OVERLAY_BYTES => return Err(format!("{} is larger than {MAX_OVERLAY_BYTES} bytes", path.display())),
        Ok(_) => {}
    }
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    match serde_yaml::from_str::<Value>(&text).map_err(|e| format!("{}: {e}", path.display()))? {
        Value::Null => Ok(Mapping::new()),
        Value::Mapping(m) => Ok(m),
        _ => Err(format!("{} is not a YAML mapping", path.display())),
    }
}

fn routes_of(m: &mut Mapping) -> Result<&mut Vec<Value>, String> {
    let key = Value::String("routes".into());
    let entry = m.entry(key).or_insert_with(|| Value::Sequence(Vec::new()));
    if entry.is_null() {
        *entry = Value::Sequence(Vec::new());
    }
    entry.as_sequence_mut().ok_or_else(|| "`routes` is not a list".to_owned())
}

/// The normalised prefix of a written entry, when it has one that normalises.
fn entry_prefix(v: &Value) -> Option<String> {
    v.get("prefix").and_then(Value::as_str).and_then(|p| normalize_prefix(p).ok())
}

fn write_mapping(path: &Path, m: &Mapping) -> Result<(), String> {
    let dir = path.parent().ok_or("overlay path has no directory")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    let text = serde_yaml::to_string(&Value::Mapping(m.clone())).map_err(|e| format!("overlay: {e}"))?;
    crate::dashboard_token::write_atomic(path, text.trim_end()).map_err(|e| format!("{}: {e}", path.display()))
}

/// Write or replace the route with `decl.prefix` in project `ulid`'s overlay.
/// Other entries and unknown keys are kept. `decl.prefix` must be set and valid.
pub fn set_route(dir: &Path, ulid: &str, decl: &RouteDecl) -> Result<(), String> {
    let prefix = normalize_prefix(decl.prefix.as_deref().ok_or("overlay route needs a prefix")?)?;
    let path = overlay_path(dir, ulid);
    let mut m = read_mapping(&path)?;
    let routes = routes_of(&mut m)?;
    let entry = serde_yaml::to_value(RouteDecl { prefix: Some(prefix.clone()), ..decl.clone() }).map_err(|e| format!("overlay: {e}"))?;
    match routes.iter().position(|v| entry_prefix(v).as_deref() == Some(prefix.as_str())) {
        Some(i) => routes[i] = entry,
        None if routes.len() >= MAX_ROUTES_PER_PROJECT => return Err(format!("the overlay already holds {MAX_ROUTES_PER_PROJECT} routes")),
        None => routes.push(entry),
    }
    write_mapping(&path, &m)
}

/// Remove the overlay route with `prefix` from project `ulid`'s overlay.
/// `Ok(false)` when there is no such entry (a repository route is never touched).
pub fn remove_route(dir: &Path, ulid: &str, prefix: &str) -> Result<bool, String> {
    let prefix = normalize_prefix(prefix)?;
    let path = overlay_path(dir, ulid);
    if !path.exists() {
        return Ok(false);
    }
    let mut m = read_mapping(&path)?;
    let routes = routes_of(&mut m)?;
    let Some(i) = routes.iter().position(|v| entry_prefix(v).as_deref() == Some(prefix.as_str())) else { return Ok(false) };
    routes.remove(i);
    write_mapping(&path, &m)?;
    Ok(true)
}

/// Delete project `ulid`'s overlay file when it holds no routes and nothing else
/// (what `remove_route` leaves behind after the last route). `Ok(true)` when deleted.
pub fn delete_if_empty(dir: &Path, ulid: &str) -> Result<bool, String> {
    let path = overlay_path(dir, ulid);
    let m = read_mapping(&path)?;
    let empty_routes = m.get("routes").is_none_or(|r| r.is_null() || r.as_sequence().is_some_and(Vec::is_empty));
    if !path.exists() || !empty_routes || m.keys().any(|k| k.as_str() != Some("routes")) {
        return Ok(false);
    }
    std::fs::remove_file(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ULID: &str = "01K00000000000000000000000";

    fn decl(prefix: &str, port: u64) -> RouteDecl {
        RouteDecl { prefix: Some(prefix.into()), port, ..Default::default() }
    }

    #[test]
    fn set_creates_a_private_file_and_replaces_by_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("routes");
        set_route(&dir, ULID, &decl("/a", 3000)).unwrap();
        set_route(&dir, ULID, &RouteDecl { allow: vec!["Alice@Example.com".into()], ..decl("/b/", 3001) }).unwrap();
        set_route(&dir, ULID, &decl("/a", 3002)).unwrap();
        let path = overlay_path(&dir, ULID);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        let (routes, refused) = parse_overlay(&std::fs::read_to_string(&path).unwrap(), "p");
        assert!(refused.is_empty(), "{refused:?}");
        assert_eq!(routes.iter().map(|r| (r.prefix.as_str(), r.port)).collect::<Vec<_>>(), [("/a", 3002), ("/b", 3001)]);
        assert_eq!(routes[1].allow, ["alice@example.com"]);
        assert!(routes.iter().all(|r| r.source == Source::Dashboard));
    }

    #[test]
    fn delete_if_empty_removes_only_a_file_with_no_routes_and_nothing_else() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("routes");
        assert!(!delete_if_empty(&dir, ULID).unwrap(), "no file: nothing deleted");
        set_route(&dir, ULID, &decl("/a", 3000)).unwrap();
        assert!(!delete_if_empty(&dir, ULID).unwrap(), "a route is left");
        assert!(remove_route(&dir, ULID, "/a").unwrap());
        assert!(delete_if_empty(&dir, ULID).unwrap());
        assert!(!overlay_path(&dir, ULID).exists());
        std::fs::write(overlay_path(&dir, ULID), "note: keep me\nroutes: []\n").unwrap();
        assert!(!delete_if_empty(&dir, ULID).unwrap(), "unknown keys keep the file");
    }

    #[test]
    fn unknown_keys_and_other_entries_survive_a_write_and_remove_reports_absence() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("routes");
        std::fs::create_dir_all(&dir).unwrap();
        let path = overlay_path(&dir, ULID);
        std::fs::write(&path, "note: keep me\nroutes:\n  - { prefix: /x, port: 4000, colour: blue }\n").unwrap();
        set_route(&dir, ULID, &decl("/y", 4001)).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("keep me") && text.contains("colour: blue") && text.contains("/y"), "{text}");
        assert!(remove_route(&dir, ULID, "/x").unwrap());
        assert!(!remove_route(&dir, ULID, "/x").unwrap());
        assert!(!remove_route(&dir, "01K00000000000000000000001", "/x").unwrap());
        let (routes, _) = parse_overlay(&std::fs::read_to_string(&path).unwrap(), "p");
        assert_eq!(routes.iter().map(|r| r.prefix.as_str()).collect::<Vec<_>>(), ["/y"]);
    }

    #[test]
    fn a_broken_overlay_refuses_as_dashboard_and_the_cap_holds() {
        let (routes, refused) = parse_overlay("routes: [", "p");
        assert!(routes.is_empty());
        assert_eq!(refused[0].source, Source::Dashboard);
        assert!(refused[0].reason.starts_with("routes overlay:"), "{}", refused[0].reason);
        let tmp = tempfile::tempdir().unwrap();
        for i in 0..MAX_ROUTES_PER_PROJECT {
            set_route(tmp.path(), ULID, &decl(&format!("/r{i}"), 3000 + i as u64)).unwrap();
        }
        let e = set_route(tmp.path(), ULID, &decl("/one-more", 5000)).unwrap_err();
        assert!(e.contains("32 routes"), "{e}");
    }
}
