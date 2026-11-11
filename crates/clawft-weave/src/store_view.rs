//! `store.view.*`: the daemon side of the scoped view (`weftos_store::drive::view`, spec
//! `docs/architecture/store-scoped-view.md` section 6.3). `weaver store view <verb>` calls these.
//!
//! - `open {name, root, spec{...}, read_only?}`: lease the work prefix, materialise the includes and the
//!   work tree under `root`, write `view.json`. `spec` is a `ViewSpec` without `bucket`: the bucket is
//!   always the one `name` binds to, and a `bucket` that differs is refused. `read_only` drops the
//!   work tree (no lease, publish refused).
//! - `status {root, name?}`: the local state (`view.json`, lease, pending changes, conflicts, tampered).
//!   Reads nothing from the store.
//! - `refresh {root, name?}`: bring the base and the work tree up to date. Never writes the store.
//! - `publish {name, root}`: upload the work tree's changes under the lease.
//! - `resolve {name, root, path, keep: local|remote}`: close one open conflict on the work prefix.
//! - `renew {root, name?}`: extend the lease.
//! - `close {name, root, publish|discard}`: exactly one of the two. `publish` refuses, removing nothing,
//!   while conflicts stay open.
//!
//! Wired like the drive verbs: an Admin act on the raw data plane, every `root` checked by [`Guard`]
//! (absolute, resolved, outside the daemon's own state). The view must belong to the store `name`
//! binds to. `open`, `publish`, `resolve` and `close` change shared state or the view and are recorded
//! in the activity log and on the chain, success or failure, with counts and not file lists. `status`,
//! `refresh` and `renew` record nothing (`renew` rewrites only the lease object; the drive records no
//! claim verbs either).

use serde::Deserialize;
use serde_json::{Value, json};
use weftos_store::drive::view::{CloseMode, ScopedView, ViewSpec};
use weftos_store::drive::{ChangeKind, Keep};
use weftos_store::{Outcome, StoreName, StoreService};

use crate::store_drive::{down_json, skipped, up_json};
use crate::store_worktree::Op;
use crate::store_worktree_paths::{Guard, Use};

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct P {
    name: Option<String>,
    root: Option<String>,
    spec: Option<Value>,
    path: Option<String>,
    keep: Option<String>,
    #[serde(default)]
    read_only: bool,
    #[serde(default)]
    publish: bool,
    #[serde(default)]
    discard: bool,
    #[allow(dead_code)]
    #[serde(default)]
    json: bool,
    #[allow(dead_code)]
    agent: Option<String>,
    #[allow(dead_code)]
    host: Option<String>,
}

fn need<'a>(v: &'a Option<String>, what: &str) -> Result<&'a str, String> {
    v.as_deref().filter(|s| !s.is_empty()).ok_or_else(|| format!("missing --{what}"))
}

/// Counts instead of lists, at any depth, for the chain payload.
fn counts(v: &Value) -> Value {
    match v {
        Value::Array(a) => json!(a.len()),
        Value::Object(m) => Value::Object(m.iter().map(|(k, x)| (k.clone(), counts(x))).collect()),
        o => o.clone(),
    }
}

fn kind_text(k: &ChangeKind) -> Value {
    serde_json::to_value(k).unwrap_or(Value::Null)
}

/// Run one verb. `method` is `store.view.<verb>`.
/// `error_kind` when `root` is refused by the path guard (not absolute, `..`, a symlink, or the
/// daemon's own state). Clients branch on this, not on the message text.
pub const PATH_REFUSED_KIND: &str = "path_refused";
/// `error_kind` when the params are missing or malformed.
pub const INVALID_PARAMS_KIND: &str = "invalid_params";

/// A refusal: a message, and a stable kind when the refusal has one.
#[derive(Debug)]
pub struct Fail {
    pub kind: Option<&'static str>,
    pub msg: String,
}

impl From<String> for Fail {
    fn from(msg: String) -> Self {
        Self { kind: None, msg }
    }
}

impl From<&str> for Fail {
    fn from(msg: &str) -> Self {
        Self { kind: None, msg: msg.to_owned() }
    }
}

impl Fail {
    fn of(kind: &'static str, msg: String) -> Self {
        Self { kind: Some(kind), msg }
    }
}

pub fn run(svc: &StoreService, method: &str, params: Value) -> Result<Value, Fail> {
    run_kinded(svc, method, params, &Guard::from_runtime())
}

/// [`run_kinded`] with the failure as its message only.
pub fn run_with(svc: &StoreService, method: &str, params: Value, guard: &Guard) -> Result<Value, String> {
    run_kinded(svc, method, params, guard).map_err(|f| f.msg)
}

/// Run one verb with an explicit set of protected paths; a failure carries its kind.
pub fn run_kinded(svc: &StoreService, method: &str, params: Value, guard: &Guard) -> Result<Value, Fail> {
    let verb = method.strip_prefix("store.view.").unwrap_or(method).to_owned();
    let who = (params["name"].as_str().map(str::to_owned), params["agent"].as_str().map(str::to_owned), params["host"].as_str().map(str::to_owned));
    let r = dispatch(svc, &verb, params, guard);
    record_verb(svc, &verb, who, &r);
    r
}

fn record_verb(svc: &StoreService, verb: &str, who: (Option<String>, Option<String>, Option<String>), r: &Result<Value, Fail>) {
    let (kind, op) = match verb {
        "open" => ("store.view.open", "ViewOpen"),
        "publish" => ("store.view.publish", "ViewPublish"),
        "resolve" => ("store.view.resolve", "ViewResolve"),
        "close" => ("store.view.close", "ViewClose"),
        _ => return,
    };
    // Every recorded verb requires `name`; a call that failed before naming one has nothing to attribute to.
    let Some(Ok(name)) = who.0.as_deref().map(StoreName::parse) else { return };
    let actor = format!("{}@{}", who.1.as_deref().unwrap_or("agent"), who.2.as_deref().unwrap_or("?"));
    let (outcome, detail) = match r {
        Ok(v) => (Outcome::Ok, counts(v)),
        Err(e) => (Outcome::Error(e.msg.clone()), json!({"error": e.msg})),
    };
    let bytes = detail["bytes"].as_u64().or_else(|| detail["published"]["bytes_put"].as_u64()).or_else(|| detail["bytes_put"].as_u64()).unwrap_or(0);
    svc.record_act(&actor, &name, op, None, bytes, outcome, Some((kind, json!({"store": name.to_string(), "actor": actor, "result": detail}))));
}

fn dispatch(svc: &StoreService, verb: &str, params: Value, guard: &Guard) -> Result<Value, Fail> {
    let p: P = serde_json::from_value(params).map_err(|e| Fail::of(INVALID_PARAMS_KIND, format!("invalid params: {e}")))?;
    let name = live_store(svc, &p)?;
    let root = || -> Result<std::path::PathBuf, Fail> {
        let raw = need(&p.root, "root").map_err(|m| Fail::of(INVALID_PARAMS_KIND, m))?;
        guard.check(raw, "root", Use::WriteDir).map_err(|m| Fail::of(PATH_REFUSED_KIND, m))
    };
    let b = Op(svc.operator_objects());
    let e = |e: weftos_store::drive::view::ViewError| e.to_string();
    match verb {
        "open" => {
            let name = name.ok_or("missing --name")?;
            let spec = build_spec(&p, &name)?;
            let (v, r) = ScopedView::open(&b, &spec, &root()?).map_err(e)?;
            let m = v.manifest();
            Ok(json!({
                "root": m.root, "manifest": m.root.join("view.json"), "spec_hash": m.spec_hash, "read_only": spec.work.is_none(),
                "lease": m.lease, "files": r.files, "bytes": r.bytes, "fetched": r.fetched,
                "recovered": r.recovered, "took_over": r.took_over,
                "skipped": r.skipped.iter().map(|(prefix, s)| json!({"prefix": prefix, "path": s.path, "why": s.why, "detail": s.detail})).collect::<Vec<_>>(),
            }))
        }
        "status" => {
            let v = load(&root()?, name.as_ref())?;
            let s = v.status().map_err(e)?;
            let inc = |i: &weftos_store::drive::view::IncludeStatus| json!({"prefix": i.prefix, "at": i.at, "files": i.files, "bytes": i.bytes, "open_conflicts": i.open_conflicts});
            Ok(json!({
                "root": s.root, "spec_hash": s.spec_hash, "read_only": s.read_only, "lease": s.lease, "lease_expired": s.lease_expired,
                "includes": s.includes.iter().map(inc).collect::<Vec<_>>(), "work": s.work.as_ref().map(inc),
                "pending": s.pending.iter().map(|(p, k)| json!({"path": p, "change": kind_text(k)})).collect::<Vec<_>>(),
                "tampered": s.tampered, "manifest": v.manifest(),
            }))
        }
        "refresh" => {
            let mut v = load(&root()?, name.as_ref())?;
            let r = v.refresh(&b).map_err(e)?;
            Ok(json!({
                "base": r.base.iter().map(down_json).collect::<Vec<_>>(), "repaired": r.repaired, "work": r.work.as_ref().map(down_json),
                "work_updated": r.work_updated, "work_removed": r.work_removed, "work_conflicts": r.work_conflicts,
                "work_not_refreshed": r.work_not_refreshed, "skipped": skipped(&r.skipped),
            }))
        }
        "publish" => {
            let name = name.ok_or("missing --name")?;
            let mut v = load(&root()?, Some(&name))?;
            Ok(publish_json(&v.publish(&b).map_err(e)?))
        }
        "resolve" => {
            let name = name.ok_or("missing --name")?;
            let mut v = load(&root()?, Some(&name))?;
            let path = need(&p.path, "path")?;
            let keep = match need(&p.keep, "keep")? {
                "local" => Keep::Local,
                "remote" => Keep::Remote,
                o => return Err(Fail::of(INVALID_PARAMS_KIND, format!("--keep must be local or remote, not {o:?}"))),
            };
            v.resolve(path, keep).map_err(e)?;
            Ok(json!({"resolved": path, "kept": p.keep}))
        }
        "renew" => {
            let mut v = load(&root()?, name.as_ref())?;
            v.renew(&b).map_err(e)?;
            Ok(json!({"lease": v.lease().map(|l| json!({"id": l.body.id, "holder": l.body.holder, "expires_ms": l.body.expires_ms}))}))
        }
        "close" => {
            let name = name.ok_or("missing --name")?;
            let mode = match (p.publish, p.discard) {
                (true, false) => CloseMode::Publish,
                (false, true) => CloseMode::Discard,
                _ => return Err(Fail::of(INVALID_PARAMS_KIND, "close needs exactly one of --publish, --discard".into())),
            };
            let v = load(&root()?, Some(&name))?;
            let r = v.close(&b, mode).map_err(e)?;
            Ok(json!({
                "mode": if mode == CloseMode::Publish { "publish" } else { "discard" },
                "published": r.published.as_ref().map(publish_json), "lease_released": r.lease_released, "lease_error": r.lease_error,
                "removed": r.removed, "root_removed": r.root_removed, "left": r.left,
            }))
        }
        other => Err(format!("unknown view verb: {other}").into()),
    }
}

fn publish_json(r: &weftos_store::drive::view::PublishReport) -> Value {
    let mut v = up_json(&r.up);
    v["applied"] = r.applied.iter().map(|(p, k)| json!({"path": p, "change": kind_text(k)})).collect();
    v["held"] = json!(r.held);
    v["view_open_conflicts"] = json!(r.open_conflicts);
    v["view_skipped"] = skipped(&r.skipped);
    v
}

/// The spec the call carries, bound to the store `name` names. `read_only` drops the work tree.
fn build_spec(p: &P, name: &StoreName) -> Result<ViewSpec, String> {
    let mut m = match p.spec.clone() {
        Some(Value::Object(m)) => m,
        Some(_) => return Err("--spec must be a JSON object".into()),
        None => return Err("missing --spec (or --include/--work flags)".into()),
    };
    let bucket = name.bucket();
    match m.insert("bucket".into(), json!(bucket)) {
        Some(Value::String(s)) if s != bucket => return Err(format!("the spec's bucket {s} is not the bucket of {name}")),
        Some(Value::String(_)) | None => {}
        Some(_) => return Err("the spec's bucket must be a string".into()),
    }
    let mut spec: ViewSpec = serde_json::from_value(Value::Object(m)).map_err(|e| format!("bad view spec: {e}"))?;
    if p.read_only {
        spec.work = None;
    }
    Ok(spec)
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

/// Reopen the view at `root`; when a store was named, the view must be bound to its bucket.
fn load(root: &std::path::Path, name: Option<&StoreName>) -> Result<ScopedView, String> {
    let v = ScopedView::load(root).map_err(|e| e.to_string())?;
    match name {
        Some(n) if v.spec().bucket != n.bucket() => Err(format!("{} is bound to bucket {}, not {n}", root.display(), v.spec().bucket)),
        _ => Ok(v),
    }
}

/// Dispatch one `store.view.*` verb on the daemon's service.
pub async fn handle(method: &str, params: Value) -> clawft_rpc::Response {
    let Some(svc) = crate::store_rt::global() else {
        return clawft_rpc::Response::error("the object store is not running on this daemon".to_string());
    };
    let method = method.to_owned();
    match tokio::task::spawn_blocking(move || run(&svc, &method, params)).await {
        Ok(Ok(v)) => clawft_rpc::Response::success(v),
        Ok(Err(Fail { kind: Some(k), msg })) => clawft_rpc::Response::error_with_kind(k, msg),
        Ok(Err(Fail { kind: None, msg })) => clawft_rpc::Response::error(msg),
        Err(e) => clawft_rpc::Response::error(format!("store task failed: {e}")),
    }
}

#[cfg(test)]
#[path = "store_view_tests.rs"]
mod tests;
