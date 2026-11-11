//! `store.drive.*`: the daemon side of the ADR-131 materialise-plus-watch prototype
//! (`weftos_store::drive`). `weaver store drive <verb>` calls these.
//!
//! - `materialise {name, prefix?, dest, read_only?}`: bind `dest` to the store and fill or refresh it.
//! - `sync-down {root, name?}`: apply remote changes to an existing drive.
//! - `sync-up {name, root}`: push local changes (refused with `read_only`).
//! - `resolve {name, root, path, keep: local|remote}`: close one open conflict.
//! - `watch {root}`: ONE scan per call, reporting pending local changes. It does not block. The daemon
//!   loop is ADR-131 work; a shell loop can call this and seal when two reports in a row match.
//!
//! Wired like the S2 verbs: an Admin act on the raw data plane, every path checked by
//! [`Guard`] (absolute, resolved, outside the daemon's own state), the library's secret denylist and
//! ceilings untouched (`Options::default().limits`). `materialise`, `sync-up` and `resolve` change
//! shared state or the drive and are recorded in the activity log and on the chain, success or
//! failure. `sync-down` and `watch` read and record nothing. The verbs other than `materialise`
//! take the existing drive as `root` (`dest` is accepted as the same thing).

use serde::Deserialize;
use serde_json::{Value, json};
use weftos_store::drive::{self, DownReport, Index, Keep, Options, UpReport};
use weftos_store::{Outcome, StoreName, StoreService};

use crate::store_worktree::Op;
use crate::store_worktree_paths::{Guard, Use};

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct P {
    name: Option<String>,
    prefix: Option<String>,
    dest: Option<String>,
    root: Option<String>,
    path: Option<String>,
    keep: Option<String>,
    #[serde(default)]
    read_only: bool,
    /// Output format; the CLI formats, the daemon ignores it.
    #[serde(default)]
    #[allow(dead_code)]
    json: bool,
    /// Who is acting; `run_with` reads them from the raw params for the records.
    #[allow(dead_code)]
    agent: Option<String>,
    #[allow(dead_code)]
    host: Option<String>,
}

fn need<'a>(v: &'a Option<String>, what: &str) -> Result<&'a str, String> {
    v.as_deref().filter(|s| !s.is_empty()).ok_or_else(|| format!("missing --{what}"))
}

/// `root`, or `dest` as its synonym; two different values are refused rather than guessed between.
fn drive_root(p: &P) -> Result<&str, String> {
    match (p.root.as_deref().filter(|s| !s.is_empty()), p.dest.as_deref().filter(|s| !s.is_empty())) {
        (Some(r), Some(d)) if r != d => Err("--root and --dest differ; give one".into()),
        (Some(r), _) | (None, Some(r)) => Ok(r),
        (None, None) => Err("missing --root".into()),
    }
}

pub(crate) fn skipped(v: &[drive::Skipped]) -> Value {
    v.iter().map(|s| json!({"path": s.path, "why": s.why, "detail": s.detail})).collect()
}

pub(crate) fn down_json(r: &DownReport) -> Value {
    json!({
        "created": r.created, "updated": r.updated, "mode_updated": r.mode_updated, "removed": r.removed, "conflicts": r.conflicts,
        "open_conflicts": r.open_conflicts, "unchanged": r.unchanged, "fetched": r.fetched,
        "bytes_fetched": r.bytes_fetched, "skipped": skipped(&r.skipped),
    })
}

pub(crate) fn up_json(r: &UpReport) -> Value {
    json!({
        "put": r.put, "deleted": r.deleted, "conflicts": r.conflicts, "open_conflicts": r.open_conflicts,
        "bytes_put": r.bytes_put, "conditional": r.conditional, "head_then_write": r.head_then_write,
        "skipped": skipped(&r.skipped),
    })
}

/// Counts instead of path lists, for the chain payload.
fn summary(v: &Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(m.iter().map(|(k, x)| (k.clone(), if let Value::Array(a) = x { json!(a.len()) } else { x.clone() })).collect()),
        o => o.clone(),
    }
}

/// Run one verb. `method` is `store.drive.<verb>`.
pub fn run(svc: &StoreService, method: &str, params: Value) -> Result<Value, String> {
    run_with(svc, method, params, &Guard::from_runtime())
}

/// [`run`] with an explicit set of protected paths.
pub fn run_with(svc: &StoreService, method: &str, params: Value, guard: &Guard) -> Result<Value, String> {
    let verb = method.strip_prefix("store.drive.").unwrap_or(method).to_owned();
    let who = (params["name"].as_str().map(str::to_owned), params["agent"].as_str().map(str::to_owned), params["host"].as_str().map(str::to_owned));
    let r = dispatch(svc, &verb, params, guard);
    record_verb(svc, &verb, who, &r);
    r
}

fn record_verb(svc: &StoreService, verb: &str, who: (Option<String>, Option<String>, Option<String>), r: &Result<Value, String>) {
    let (kind, op) = match verb {
        "materialise" => ("store.drive.materialise", "DriveMaterialise"),
        "sync-up" => ("store.drive.sync-up", "DriveSyncUp"),
        "resolve" => ("store.drive.resolve", "DriveResolve"),
        _ => return,
    };
    // Every recorded verb requires `name`; a call that failed before naming one has nothing to attribute to.
    let Some(Ok(name)) = who.0.as_deref().map(StoreName::parse) else { return };
    let actor = format!("{}@{}", who.1.as_deref().unwrap_or("agent"), who.2.as_deref().unwrap_or("?"));
    let (outcome, detail) = match r {
        Ok(v) => (Outcome::Ok, summary(v)),
        Err(e) => (Outcome::Error(e.clone()), json!({"error": e})),
    };
    let bytes = detail["bytes_fetched"].as_u64().or_else(|| detail["bytes_put"].as_u64()).unwrap_or(0);
    svc.record_act(&actor, &name, op, None, bytes, outcome, Some((kind, json!({"store": name.to_string(), "actor": actor, "result": detail}))));
}

fn dispatch(svc: &StoreService, verb: &str, params: Value, guard: &Guard) -> Result<Value, String> {
    let p: P = serde_json::from_value(params).map_err(|e| format!("invalid params: {e}"))?;
    let opts = Options { read_only: p.read_only, ..Options::default() };
    let e = |e: drive::DriveError| e.to_string();
    match verb {
        "materialise" => {
            let name = live_store(svc, &p)?.ok_or("missing --name")?;
            let dest = guard.check(need(&p.dest, "dest")?, "dest", Use::WriteDir)?;
            let b = Op(svc.operator_objects());
            let r = drive::materialise_with(&b, &name.bucket(), p.prefix.as_deref().unwrap_or(""), &dest, &opts).map_err(e)?;
            Ok(down_json(&r))
        }
        "sync-down" => {
            let root = guard.check(drive_root(&p)?, "root", Use::WriteDir)?;
            if let Some(name) = live_store(svc, &p)? {
                bound_to(&name, &root)?;
            }
            let r = drive::sync_down_with(&Op(svc.operator_objects()), &root, &opts).map_err(e)?;
            Ok(down_json(&r))
        }
        "sync-up" => {
            let name = live_store(svc, &p)?.ok_or("missing --name")?;
            let root = guard.check(drive_root(&p)?, "root", Use::WriteDir)?;
            bound_to(&name, &root)?;
            let r = drive::sync_up_with(&Op(svc.operator_objects()), &root, &opts).map_err(e)?;
            Ok(up_json(&r))
        }
        "resolve" => {
            let name = live_store(svc, &p)?.ok_or("missing --name")?;
            let root = guard.check(drive_root(&p)?, "root", Use::WriteDir)?;
            bound_to(&name, &root)?;
            let path = need(&p.path, "path")?;
            let keep = match need(&p.keep, "keep")? {
                "local" => Keep::Local,
                "remote" => Keep::Remote,
                o => return Err(format!("--keep must be local or remote, not {o:?}")),
            };
            drive::resolve(&root, path, keep).map_err(e)?;
            Ok(json!({"resolved": path, "kept": p.keep}))
        }
        "watch" => {
            let root = guard.check(drive_root(&p)?, "root", Use::WriteDir)?;
            let ix = Index::load(&root).map_err(e)?.ok_or_else(|| format!("{} is not a drive", root.display()))?;
            let s = drive::scan(&root, &ix, &opts.limits).map_err(e)?;
            let ch: Vec<Value> = s.changes.iter().map(|c| json!({"path": c.path, "change": c.kind, "size": c.size})).collect();
            Ok(json!({"changes": ch, "skipped": skipped(&s.skipped), "open_conflicts": ix.conflicts.len()}))
        }
        other => Err(format!("unknown drive verb: {other}")),
    }
}

/// The named store, if a name was given. It must not be archived.
fn live_store(svc: &StoreService, p: &P) -> Result<Option<StoreName>, String> {
    let Some(n) = p.name.as_deref().filter(|s| !s.is_empty()) else { return Ok(None) };
    let name = StoreName::parse(n).map_err(|e| e.to_string())?;
    if svc.is_archived(&name) == Some(true) {
        return Err(format!("{name} is archived; refusing"));
    }
    Ok(Some(name))
}

/// The drive must be bound to the store the call names.
fn bound_to(name: &StoreName, root: &std::path::Path) -> Result<(), String> {
    let ix = Index::load(root).map_err(|e| e.to_string())?.ok_or_else(|| format!("{} is not a drive", root.display()))?;
    if ix.bucket == name.bucket() {
        Ok(())
    } else {
        Err(format!("{} is bound to bucket {}, not {name}", root.display(), ix.bucket))
    }
}

/// Dispatch one `store.drive.*` verb on the daemon's service.
pub async fn handle(method: &str, params: Value) -> clawft_rpc::Response {
    let Some(svc) = crate::store_rt::global() else {
        return clawft_rpc::Response::error("the object store is not running on this daemon".to_string());
    };
    let method = method.to_owned();
    match tokio::task::spawn_blocking(move || run(&svc, &method, params)).await {
        Ok(Ok(v)) => clawft_rpc::Response::success(v),
        Ok(Err(e)) => clawft_rpc::Response::error(e),
        Err(e) => clawft_rpc::Response::error(format!("store task failed: {e}")),
    }
}

#[cfg(test)]
#[path = "store_drive_tests.rs"]
mod tests;
