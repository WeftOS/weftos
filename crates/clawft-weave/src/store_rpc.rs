//! Operator verbs for the object store (ADR-126 S1). All Admin.
//!
//! - `store.ls {name?, prefix?}`: every store; with `name`, that store's grants and objects.
//! - `store.create {name, admin?[], write?[], read?[]}`.
//! - `store.grant {name, principal, access?, enabled?}`: with `access`, grant it; with only
//!   `enabled`, switch the grant on or off.
//! - `store.revoke {name, principal}`.
//! - `store.promote {name, prefixes[], archive_rest?}`: a worktree store into its project store.
//! - `store.archive {name, delete?}`.
//! - `store.node.retire {name}`: a node was unpaired; archive its store and retire the id.
//! - `store.archive.gc {node_days?, project_days?, release_days?, confirmed?}`: delete archives of
//!   node, project and release stores older than the days given (none given: nothing expires). A
//!   dry run unless `confirmed`. Worktree archives are `store worktree gc`'s.
//! - `store.archive.keep {name, days?, forever?}`: how long this store's archives are kept, over
//!   the kind's limit (`days`, or `forever`; neither clears the override). Not for worktrees.
//! - `store.put {name, key, principal, text|b64}`, `store.get {name, key, principal}`:
//!   a governed request as that principal, the same path a cog's request takes.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use clawft_rpc::Response;
use serde::Deserialize;
use serde_json::{Value, json};
use weftos_store::service::{Caller, Parties};
use weftos_store::{Access, Principal, StoreName, StoreService};

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct P {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    prefix: Option<String>,
    #[serde(default)]
    principal: Option<String>,
    #[serde(default)]
    access: Option<String>,
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    admin: Vec<String>,
    #[serde(default)]
    write: Vec<String>,
    #[serde(default)]
    read: Vec<String>,
    #[serde(default)]
    prefixes: Vec<String>,
    #[serde(default)]
    archive_rest: bool,
    #[serde(default)]
    delete: bool,
    #[serde(default)]
    node_days: Option<u64>,
    #[serde(default)]
    project_days: Option<u64>,
    #[serde(default)]
    release_days: Option<u64>,
    #[serde(default)]
    confirmed: bool,
    #[serde(default)]
    days: Option<u64>,
    #[serde(default)]
    forever: bool,
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    b64: Option<String>,
}

fn need<'a>(v: &'a Option<String>, what: &str) -> Result<&'a str, String> {
    v.as_deref().filter(|s| !s.is_empty()).ok_or_else(|| format!("missing {what}"))
}

fn store(p: &P) -> Result<StoreName, String> {
    StoreName::parse(need(&p.name, "name")?).map_err(|e| e.to_string())
}

fn principal(s: &str) -> Result<Principal, String> {
    Principal::parse(s).map_err(|e| e.to_string())
}

fn principals(v: &[String]) -> Result<Vec<Principal>, String> {
    v.iter().map(|s| principal(s)).collect()
}

/// Run one verb against `svc`.
pub fn run(svc: &StoreService, method: &str, params: Value) -> Result<Value, String> {
    let p: P = serde_json::from_value(params).map_err(|e| format!("invalid params: {e}"))?;
    let e = |e: weftos_store::StoreError| e.to_string();
    match method {
        "store.ls" => match &p.name {
            None => Ok(json!({"stores": svc.list_stores()})),
            Some(_) => {
                let name = store(&p)?;
                let info = svc.info(&name).map_err(e)?;
                // The listing is an operator read of metadata, done as the daemon, not as a principal.
                let objs = svc.list_as_operator(&name, p.prefix.as_deref().unwrap_or("")).map_err(e)?;
                let objects: Vec<Value> = objs.iter().map(|m| json!({"key": m.key, "size": m.size, "etag": m.etag})).collect();
                Ok(json!({"store": info, "objects": objects}))
            }
        },
        "store.create" => {
            let name = store(&p)?;
            let mut parties = Parties::defaults_for(&name);
            parties.admins.extend(principals(&p.admin)?);
            parties.writers.extend(principals(&p.write)?);
            parties.readers.extend(principals(&p.read)?);
            Ok(json!(svc.create_store(&name, "cli", parties).map_err(e)?))
        }
        "store.grant" => {
            let name = store(&p)?;
            let who = principal(need(&p.principal, "principal")?)?;
            match (&p.access, p.enabled) {
                (Some(a), en) => {
                    let g = svc.grant(&name, who.clone(), Access::parse(a).map_err(e)?, "cli").map_err(e)?;
                    if en == Some(false) {
                        svc.set_grant_enabled(&name, &who, false, "cli").map_err(e)?;
                    }
                    Ok(json!(g))
                }
                (None, Some(en)) => {
                    svc.set_grant_enabled(&name, &who, en, "cli").map_err(e)?;
                    Ok(json!({"store": name.to_string(), "principal": who.to_string(), "enabled": en}))
                }
                (None, None) => Err("store.grant needs access, or enabled".into()),
            }
        }
        "store.revoke" => {
            let (name, who) = (store(&p)?, principal(need(&p.principal, "principal")?)?);
            svc.revoke(&name, &who, "cli").map_err(e)?;
            Ok(json!({"store": name.to_string(), "principal": who.to_string(), "revoked": true}))
        }
        "store.promote" => {
            let name = store(&p)?;
            if p.prefixes.is_empty() {
                return Err("store.promote needs at least one promotable prefix".into());
            }
            Ok(json!(svc.promote(&name, &p.prefixes, p.archive_rest, "cli").map_err(e)?))
        }
        "store.archive" => {
            let name = store(&p)?;
            let n = svc.archive(&name, p.delete, "cli").map_err(e)?;
            Ok(json!({"store": name.to_string(), "archived_objects": n, "deleted": p.delete}))
        }
        "store.archive.gc" => {
            let keep = weftos_store::service::ArchiveKeep { node_days: p.node_days, project_days: p.project_days, release_days: p.release_days };
            Ok(json!(svc.archive_gc(&keep, p.confirmed, "cli").map_err(e)?))
        }
        "store.archive.keep" => {
            let name = store(&p)?;
            let keep = match (p.days, p.forever) {
                (Some(_), true) => return Err("store.archive.keep takes days or forever, not both".into()),
                (Some(d), false) => Some(weftos_store::service::KeepFor::Days(d)),
                (None, true) => Some(weftos_store::service::KeepFor::Forever),
                (None, false) => None,
            };
            svc.set_archive_keep(&name, keep, "cli").map_err(e)?;
            Ok(json!({"store": name.to_string(), "keep": keep}))
        }
        // A node was unpaired: archive its node store and retire the id (ADR-126; S3 calls this).
        "store.node.retire" => {
            let name = store(&p)?;
            let n = svc.retire_node_store(&name, "cli").map_err(e)?;
            Ok(json!({"store": name.to_string(), "archived_objects": n, "retired": true}))
        }
        "store.put" => {
            let (name, key) = (store(&p)?, need(&p.key, "key")?.to_owned());
            let who = Caller::local(principal(need(&p.principal, "principal")?)?);
            svc.note_operator("cli", "store.put", &who.principal, &name, &key);
            let data = match (&p.text, &p.b64) {
                (Some(t), None) => t.clone().into_bytes(),
                (None, Some(b)) => B64.decode(b).map_err(|e| format!("b64: {e}"))?,
                _ => return Err("store.put needs exactly one of text, b64".into()),
            };
            let m = svc.put(&who, &name, &key, &data, false).map_err(e)?;
            Ok(json!({"key": m.key, "size": m.size, "etag": m.etag}))
        }
        "store.get" => {
            let (name, key) = (store(&p)?, need(&p.key, "key")?.to_owned());
            let who = Caller::local(principal(need(&p.principal, "principal")?)?);
            svc.note_operator("cli", "store.get", &who.principal, &name, &key);
            let d = svc.get(&who, &name, &key).map_err(e)?;
            Ok(json!({"key": key, "size": d.len(), "b64": B64.encode(&d), "text": String::from_utf8(d).ok()}))
        }
        other => Err(format!("unknown method: {other}")),
    }
}

/// Dispatch one `store.*` verb on the daemon's service.
pub async fn handle(method: &str, params: Value) -> Response {
    let Some(svc) = crate::store_rt::global() else {
        return Response::error("the object store is not running on this daemon".to_string());
    };
    let method = method.to_owned();
    match tokio::task::spawn_blocking(move || run(&svc, &method, params)).await {
        Ok(Ok(v)) => Response::success(v),
        Ok(Err(e)) => Response::error(e),
        Err(e) => Response::error(format!("store task failed: {e}")),
    }
}

#[cfg(test)]
#[path = "store_rpc_tests.rs"]
mod tests;
