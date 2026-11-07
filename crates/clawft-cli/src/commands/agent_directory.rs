//! Agent directory: one view that joins declared, running and exposed agents
//! (design note `docs/grok-build-fork/agent-directory-design.md`).
//!
//! - declared: `agents/catalog.json` (generated from the `agents/<id>/`
//!   packages by `scripts/agents-catalog.mjs`),
//! - running: weave `agent.list`,
//! - exposed: weave `expose.list` rows of kind `harness` (ADR-120 section 3).
//!
//! The key is `weftos://agent/<id>`: no pid, no node, so it survives a daemon
//! restart. A source that cannot be read is named in `sources` with its
//! error; it is never reported as an empty list, and an entry whose status
//! depends on it says `unknown` instead of guessing.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

/// URI prefix of every directory key.
pub const URI_PREFIX: &str = "weftos://agent/";
/// Exposure id prefix for harness exposures that name an agent.
const HARNESS_PREFIX: &str = "harness.";
const MAX_ID_LEN: usize = 128;

/// One source's outcome: its rows, or why it could not be read.
pub type Source<T> = Result<T, String>;

/// Percent-encode everything outside `[A-Za-z0-9._-]`, so any agent id string
/// (running agents choose their own) becomes a safe URI segment.
pub fn encode_id(id: &str) -> String {
    let mut out = String::with_capacity(id.len());
    for b in id.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Directory key for an agent id.
pub fn uri_for(id: &str) -> String {
    format!("{URI_PREFIX}{}", encode_id(id))
}

/// Accept `weftos://agent/<id>` or a bare id; refuse anything else (empty,
/// over-long, another scheme or kind).
pub fn normalize_key(input: &str) -> Result<String, String> {
    let t = input.trim();
    if t.is_empty() {
        return Err("agent id is empty".into());
    }
    if t.len() > MAX_ID_LEN * 3 + URI_PREFIX.len() {
        return Err("agent id is too long".into());
    }
    if t.contains("://") {
        let rest = t
            .strip_prefix(URI_PREFIX)
            .ok_or_else(|| format!("not an agent URI (expected {URI_PREFIX}<id>): {t}"))?;
        if rest.is_empty() || rest.contains('/') {
            return Err(format!("malformed agent URI: {t}"));
        }
        return Ok(format!("{URI_PREFIX}{rest}"));
    }
    if t.contains('/') {
        return Err(format!("malformed agent id: {t}"));
    }
    Ok(uri_for(t))
}

/// Find `agents/catalog.json`: `$WEFTOS_AGENTS_DIR/catalog.json`, else walk up
/// from `start` looking for `agents/catalog.json`.
pub fn locate_catalog(start: &Path, env_dir: Option<&str>) -> Source<PathBuf> {
    if let Some(d) = env_dir.filter(|d| !d.is_empty()) {
        let p = Path::new(d).join("catalog.json");
        return if p.is_file() { Ok(p) } else { Err(format!("no catalog at {}", p.display())) };
    }
    for dir in start.ancestors() {
        let p = dir.join("agents").join("catalog.json");
        if p.is_file() {
            return Ok(p);
        }
    }
    Err(format!(
        "no agents/catalog.json at or above {} (set WEFTOS_AGENTS_DIR)",
        start.display()
    ))
}

/// Read the declared agents from a catalog file.
pub fn load_catalog(path: &Path) -> Source<Vec<Value>> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let doc: Value =
        serde_json::from_slice(&bytes).map_err(|e| format!("{}: not valid JSON: {e}", path.display()))?;
    match doc.get("agents") {
        Some(Value::Array(a)) => Ok(a.clone()),
        _ => Err(format!("{}: no \"agents\" array", path.display())),
    }
}

#[derive(Default)]
struct Acc {
    name: Option<String>,
    declared: Option<Value>,
    running: Vec<Value>,
    exposed: Vec<Value>,
}

fn str_of<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str)
}

/// Join the three sources. Entries are sorted by key.
pub fn join(
    declared: &Source<Vec<Value>>,
    running: &Source<Vec<Value>>,
    exposed: &Source<Value>,
) -> Value {
    let mut map: BTreeMap<String, Acc> = BTreeMap::new();

    if let Ok(rows) = declared {
        for r in rows {
            let Some(id) = str_of(r, "id") else { continue };
            let a = map.entry(uri_for(id)).or_default();
            a.name = Some(id.to_string());
            a.declared = Some(json!({
                "id": id,
                "version": r.get("version"),
                "kind": r.get("kind"),
                "role": r.get("role"),
                "trust_tier": r.get("trust_tier"),
                "state": r.get("state"),
                "digest": r.get("digest"),
                "path": r.get("path"),
            }));
        }
    }
    if let Ok(rows) = running {
        for r in rows {
            let Some(id) = str_of(r, "agent_id") else { continue };
            let a = map.entry(uri_for(id)).or_default();
            a.name.get_or_insert_with(|| id.to_string());
            a.running.push(json!({
                "pid": r.get("pid"),
                "state": r.get("state"),
                "parent_pid": r.get("parent_pid"),
            }));
        }
    }
    if let Ok(doc) = exposed {
        let rows = doc.get("exposures").and_then(Value::as_array).cloned().unwrap_or_default();
        for r in rows {
            if str_of(&r, "kind") != Some("harness") {
                continue;
            }
            let Some(eid) = str_of(&r, "id") else { continue };
            let Some(agent) = eid.strip_prefix(HARNESS_PREFIX).filter(|s| !s.is_empty()) else {
                continue;
            };
            let a = map.entry(uri_for(agent)).or_default();
            a.name.get_or_insert_with(|| agent.to_string());
            a.exposed.push(json!({
                "exposure_id": eid,
                "state": r.get("state"),
                "audience": r.get("audience"),
                "announced": r.get("announced"),
                "served": r.get("served"),
                "refused": r.get("refused"),
            }));
        }
    }

    let running_known = running.is_ok();
    let agents: Vec<Value> = map
        .into_iter()
        .map(|(key, a)| entry(key, a, declared.is_ok(), running_known))
        .collect();

    let src = |label: &str, s: Result<usize, &String>| match s {
        Ok(n) => json!({ "source": label, "ok": true, "count": n }),
        Err(e) => json!({ "source": label, "ok": false, "error": e }),
    };
    let exposed_n = exposed.as_ref().map(|d| {
        d.get("exposures")
            .and_then(Value::as_array)
            .map_or(0, |a| a.iter().filter(|r| str_of(r, "kind") == Some("harness")).count())
    });
    json!({
        "count": agents.len(),
        "agents": agents,
        "sources": {
            "declared": src("declared", declared.as_ref().map(Vec::len)),
            "running": src("running", running.as_ref().map(Vec::len)),
            "exposed": src("exposed", exposed_n),
        },
    })
}

fn entry(key: String, a: Acc, declared_ok: bool, running_ok: bool) -> Value {
    let mut from = Vec::new();
    if a.declared.is_some() {
        from.push("declared");
    }
    if !a.running.is_empty() {
        from.push("running");
    }
    if !a.exposed.is_empty() {
        from.push("exposed");
    }
    let inactive = a.declared.as_ref().and_then(|d| str_of(d, "state")) == Some("inactive");
    let exposed_live = a.exposed.iter().any(|e| e.get("state").and_then(Value::as_str) == Some("on"));

    let mut notes: Vec<&str> = Vec::new();
    if declared_ok && a.declared.is_none() {
        notes.push("undeclared: not in the catalog");
    }
    if !a.running.is_empty() && inactive {
        notes.push("inactive_but_running: catalog says inactive");
    }
    if running_ok && a.running.is_empty() && exposed_live {
        notes.push("exposed_not_running: exposure is on but no instance is running");
    }
    if !running_ok {
        notes.push("running_unknown: agent.list could not be read");
    }

    let status = if !a.running.is_empty() {
        "running"
    } else if !running_ok {
        "unknown"
    } else if !a.exposed.is_empty() {
        "stopped"
    } else if inactive {
        "inactive"
    } else {
        "declared"
    };

    json!({
        "id": key,
        "name": a.name,
        "status": status,
        "from": from,
        "declared": a.declared,
        "running": a.running,
        "exposed": a.exposed,
        "notes": notes,
    })
}

/// Look one entry up in a joined directory.
pub fn get(dir: &Value, key: &str) -> Result<Value, String> {
    let key = normalize_key(key)?;
    dir["agents"]
        .as_array()
        .and_then(|a| a.iter().find(|e| e["id"] == key.as_str()))
        .map(|e| {
            let mut e = e.clone();
            e["sources"] = dir["sources"].clone();
            e
        })
        .ok_or_else(|| format!("no agent {key} in the directory (declared, running or exposed)"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cat(rows: &[(&str, &str)]) -> Source<Vec<Value>> {
        Ok(rows.iter().map(|(i, s)| json!({"id": i, "version": "1", "state": s})).collect())
    }
    fn run(rows: &[(u64, &str)]) -> Source<Vec<Value>> {
        Ok(rows.iter().map(|(p, i)| json!({"pid": p, "agent_id": i, "state": "running"})).collect())
    }
    fn exp(rows: &[(&str, &str, &str)]) -> Source<Value> {
        Ok(json!({"exposures": rows.iter().map(|(id, kind, st)| json!({"id": id, "kind": kind, "state": st})).collect::<Vec<_>>()}))
    }
    fn find<'a>(d: &'a Value, id: &str) -> &'a Value {
        d["agents"].as_array().unwrap().iter().find(|e| e["id"] == uri_for(id).as_str()).unwrap()
    }

    #[test]
    fn same_entry_before_and_after_restart() {
        let c = cat(&[("developer", "active")]);
        let e = exp(&[]);
        let before = join(&c, &run(&[(10, "developer")]), &e);
        let after = join(&c, &run(&[(3, "developer")]), &e);
        assert_eq!(find(&before, "developer")["id"], find(&after, "developer")["id"]);
        assert_eq!(find(&before, "developer")["id"], "weftos://agent/developer");
        assert_eq!(find(&after, "developer")["running"][0]["pid"], 3);
    }

    #[test]
    fn declared_not_running() {
        let d = join(&cat(&[("a", "active"), ("b", "inactive")]), &run(&[]), &exp(&[]));
        assert_eq!(find(&d, "a")["status"], "declared");
        assert_eq!(find(&d, "b")["status"], "inactive");
        assert_eq!(find(&d, "a")["from"], json!(["declared"]));
    }

    #[test]
    fn running_but_undeclared_is_flagged() {
        let d = join(&cat(&[]), &run(&[(1, "stray"), (2, "stray")]), &exp(&[]));
        let e = find(&d, "stray");
        assert_eq!(e["status"], "running");
        assert_eq!(e["running"].as_array().unwrap().len(), 2);
        assert!(e["notes"][0].as_str().unwrap().starts_with("undeclared"));
    }

    #[test]
    fn exposed_but_stopped() {
        let d = join(&cat(&[("runner", "active")]), &run(&[]), &exp(&[("harness.runner", "harness", "on")]));
        let e = find(&d, "runner");
        assert_eq!(e["status"], "stopped");
        assert_eq!(e["from"], json!(["declared", "exposed"]));
        assert!(e["notes"].to_string().contains("exposed_not_running"));
    }

    #[test]
    fn non_harness_exposures_are_ignored() {
        let d = join(&cat(&[]), &run(&[]), &exp(&[("infer.chat", "inference", "on"), ("harness.x", "harness", "off")]));
        assert_eq!(d["count"], 1);
        assert_eq!(d["sources"]["exposed"]["count"], 1);
    }

    #[test]
    fn unreadable_source_is_named_and_status_is_unknown() {
        let d = join(&cat(&[("a", "active")]), &Err("daemon down".into()), &exp(&[]));
        assert_eq!(d["sources"]["running"]["ok"], false);
        assert_eq!(d["sources"]["running"]["error"], "daemon down");
        assert_eq!(find(&d, "a")["status"], "unknown");
        let d = join(&Err("no catalog".into()), &run(&[(1, "a")]), &exp(&[]));
        assert_eq!(d["sources"]["declared"]["ok"], false);
        assert_eq!(find(&d, "a")["status"], "running");
        assert!(!find(&d, "a")["notes"].to_string().contains("undeclared"));
    }

    #[test]
    fn odd_running_ids_are_encoded() {
        let d = join(&cat(&[]), &run(&[(1, "a b/c")]), &exp(&[]));
        assert_eq!(d["agents"][0]["id"], "weftos://agent/a%20b%2Fc");
        assert!(get(&d, "a b/c").is_err(), "a bare id with a slash is malformed");
        assert!(get(&d, "weftos://agent/a%20b%2Fc").is_ok());
    }

    #[test]
    fn get_accepts_bare_and_uri_and_refuses_bad_input() {
        let d = join(&cat(&[("a", "active")]), &run(&[]), &exp(&[]));
        assert_eq!(get(&d, "a").unwrap()["id"], "weftos://agent/a");
        assert_eq!(get(&d, "weftos://agent/a").unwrap()["id"], "weftos://agent/a");
        assert!(get(&d, "a").unwrap()["sources"]["declared"]["ok"].as_bool().unwrap());
        for bad in ["", "  ", "http://x/a", "weftos://mesh.bot.x", "weftos://agent/", "weftos://agent/a/b"] {
            assert!(get(&d, bad).is_err(), "{bad:?} must be refused");
        }
        assert!(get(&d, "missing").unwrap_err().contains("no agent"));
    }

    #[test]
    fn catalog_loader_refuses_instead_of_returning_empty() {
        let t = tempfile::tempdir().unwrap();
        assert!(locate_catalog(t.path(), None).is_err());
        let p = t.path().join("catalog.json");
        std::fs::write(&p, "{").unwrap();
        assert!(load_catalog(&p).unwrap_err().contains("not valid JSON"));
        std::fs::write(&p, "{\"teams\":[]}").unwrap();
        assert!(load_catalog(&p).unwrap_err().contains("agents"));
        std::fs::write(&p, "{\"agents\":[{\"id\":\"x\"}]}").unwrap();
        assert_eq!(load_catalog(&p).unwrap().len(), 1);
        let found = locate_catalog(t.path(), Some(t.path().to_str().unwrap())).unwrap();
        assert_eq!(found, p);
    }

    #[test]
    fn locate_walks_up() {
        let t = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(t.path().join("agents")).unwrap();
        std::fs::write(t.path().join("agents/catalog.json"), "{}").unwrap();
        let deep = t.path().join("a/b");
        std::fs::create_dir_all(&deep).unwrap();
        assert!(locate_catalog(&deep, None).is_ok());
    }

    #[test]
    fn real_catalog_loads() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../agents/catalog.json");
        let rows = load_catalog(&root).unwrap();
        assert!(rows.iter().any(|r| r["id"] == "developer"));
    }
}
