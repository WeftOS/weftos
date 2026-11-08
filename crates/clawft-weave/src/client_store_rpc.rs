//! Operator verbs for the per-client store (ADR-126 S5). All Admin (every `store.` verb is).
//!
//! - `store.client.status {}`; `store.client.hold {ulid}`.
//! - `store.client.grant {name, principal, access}`, `store.client.revoke {name, principal}`:
//!   a local principal (`cog:c1`, `member:ann`, ...).
//! - `store.client.weftos_grant {name, id, access}`, `store.client.weftos_revoke {name, id}`:
//!   the WeftOS account, which is refused until one of these names it.
//! - `store.client.snapshot {}`: publish a signed snapshot of the gate on the chain (replay then
//!   starts from it); `{"through_seq": n}`.
//! - `store.client.xmesh_trust {mesh, key}`, `store.client.xmesh_install {grant}`,
//!   `store.client.xmesh_revoke {grant}`, `store.client.sweep {}`: cross-mesh grants (ADR-114 N5).
//! - `store.client.put|get|ls|delete {name, key|prefix, as, text|b64}`: a governed request as the
//!   caller in `as`: `{"kind":"local","principal":"cog:c1"}`, `{"kind":"weftos","id":"weaver"}` or
//!   `{"kind":"foreign","mesh":"<64 hex>","id":"x"}`. The gate and the service both apply.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use clawft_rpc::Response;
use ed25519_dalek::VerifyingKey;
use serde::Deserialize;
use serde_json::{Value, json};
use weftos_store::{Principal as S1Principal, StoreName};

use crate::client_store::{ClientCaller, ClientStore};
use crate::store_account::{Access, CrossMeshGrant};

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
enum As {
    Local { principal: String },
    Weftos { id: String },
    Foreign { mesh: String, id: String },
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct P {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    ulid: Option<String>,
    #[serde(default)]
    principal: Option<String>,
    #[serde(default)]
    access: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    mesh: Option<String>,
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    prefix: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    b64: Option<String>,
    #[serde(default)]
    grant: Option<Value>,
    #[serde(default, rename = "as")]
    caller: Option<Value>,
}

fn need<'a>(v: &'a Option<String>, what: &str) -> Result<&'a str, String> {
    v.as_deref().filter(|s| !s.is_empty()).ok_or_else(|| format!("missing {what}"))
}

fn store(p: &P) -> Result<StoreName, String> {
    StoreName::parse(need(&p.name, "name")?).map_err(|e| e.to_string())
}

fn access(p: &P) -> Result<Access, String> {
    match need(&p.access, "access")? {
        "read" => Ok(Access::Read),
        "write" => Ok(Access::Write),
        "admin" => Ok(Access::Admin),
        o => Err(format!("access must be read, write or admin, not {o:?}")),
    }
}

fn hex32(s: &str, what: &str) -> Result<[u8; 32], String> {
    hex::decode(s).ok().and_then(|v| <[u8; 32]>::try_from(v).ok()).ok_or_else(|| format!("{what} must be 64 hex characters"))
}

fn local(s: &str) -> Result<S1Principal, String> {
    S1Principal::parse(s).map_err(|e| e.to_string())
}

fn grant(p: &P) -> Result<CrossMeshGrant, String> {
    CrossMeshGrant::from_json(p.grant.as_ref().ok_or("missing grant")?)
}

fn caller(p: &P) -> Result<ClientCaller, String> {
    let v = p.caller.clone().ok_or("missing as")?;
    match serde_json::from_value::<As>(v).map_err(|e| format!("invalid as: {e}"))? {
        As::Local { principal } => Ok(ClientCaller::Local(local(&principal)?)),
        As::Weftos { id } => Ok(ClientCaller::Weftos { id }),
        As::Foreign { mesh, id } => Ok(ClientCaller::Foreign { mesh: hex32(&mesh, "as.mesh")?, id }),
    }
}

/// Run one verb against `cs`.
pub fn run(cs: &ClientStore, method: &str, params: Value) -> Result<Value, String> {
    let p: P = serde_json::from_value(params).map_err(|e| format!("invalid params: {e}"))?;
    let e = |e: weftos_store::StoreError| e.to_string();
    match method {
        "store.client.status" => Ok(json!({
            "account": cs.spec.account, "uid": cs.spec.uid, "root": cs.spec.root, "listen": cs.spec.listen.to_string(),
        })),
        "store.client.hold" => {
            let name = cs.hold_project(need(&p.ulid, "ulid")?, "cli").map_err(e)?;
            Ok(json!({"store": name.to_string()}))
        }
        "store.client.grant" => {
            let (name, who, a) = (store(&p)?, local(need(&p.principal, "principal")?)?, access(&p)?);
            cs.grant_local(&who, &name, a, "cli").map_err(e)?;
            Ok(json!({"store": name.to_string(), "principal": who.to_string(), "access": need(&p.access, "access")?}))
        }
        "store.client.revoke" => {
            let (name, who) = (store(&p)?, local(need(&p.principal, "principal")?)?);
            cs.revoke_local(&who, &name, "cli").map_err(e)?;
            Ok(json!({"store": name.to_string(), "principal": who.to_string(), "revoked": true}))
        }
        "store.client.weftos_grant" => {
            let (name, id, a) = (store(&p)?, need(&p.id, "id")?, access(&p)?);
            cs.grant_weftos(id, &name, a, "cli").map_err(e)?;
            Ok(json!({"store": name.to_string(), "weftos": id, "access": need(&p.access, "access")?}))
        }
        "store.client.weftos_revoke" => {
            let (name, id) = (store(&p)?, need(&p.id, "id")?);
            cs.revoke_weftos(id, &name, "cli").map_err(e)?;
            Ok(json!({"store": name.to_string(), "weftos": id, "revoked": true}))
        }
        "store.client.xmesh_trust" => {
            let (mesh, key) = (hex32(need(&p.mesh, "mesh")?, "mesh")?, hex32(need(&p.key, "key")?, "key")?);
            let vk = VerifyingKey::from_bytes(&key).map_err(|e| format!("key: {e}"))?;
            cs.trust_mesh(mesh, vk).map_err(e)?;
            Ok(json!({"trusted": hex::encode(mesh)}))
        }
        "store.client.xmesh_install" => {
            let g = grant(&p)?;
            cs.install_xmesh(g.clone(), "cli").map_err(e)?;
            Ok(json!({"installed": g.to_json()}))
        }
        "store.client.xmesh_revoke" => {
            let g = grant(&p)?;
            cs.revoke_xmesh(&g, "cli").map_err(e)?;
            Ok(json!({"revoked": g.to_json()}))
        }
        "store.client.sweep" => Ok(json!({"expired": cs.sweep("cli")})),
        "store.client.snapshot" => match cs.snapshot().map_err(e)? {
            Some(through_seq) => Ok(json!({"through_seq": through_seq})),
            None => Err("this client store keeps its gate in gate.json; there is no chain history to compact".into()),
        },
        "store.client.put" => {
            let (name, key, who) = (store(&p)?, need(&p.key, "key")?, caller(&p)?);
            let data = match (&p.text, &p.b64) {
                (Some(t), None) => t.clone().into_bytes(),
                (None, Some(b)) => B64.decode(b).map_err(|e| format!("b64: {e}"))?,
                _ => return Err("store.client.put needs exactly one of text, b64".into()),
            };
            let m = cs.put(&who, &name, key, &data, false).map_err(e)?;
            Ok(json!({"key": m.key, "size": m.size, "etag": m.etag}))
        }
        "store.client.get" => {
            let (name, key, who) = (store(&p)?, need(&p.key, "key")?, caller(&p)?);
            let d = cs.get(&who, &name, key).map_err(e)?;
            Ok(json!({"key": key, "size": d.len(), "b64": B64.encode(&d), "text": String::from_utf8(d).ok()}))
        }
        "store.client.ls" => {
            let (name, who) = (store(&p)?, caller(&p)?);
            let objs = cs.list(&who, &name, p.prefix.as_deref().unwrap_or("")).map_err(e)?;
            let objects: Vec<Value> = objs.iter().map(|m| json!({"key": m.key, "size": m.size, "etag": m.etag})).collect();
            Ok(json!({"store": name.to_string(), "objects": objects}))
        }
        "store.client.delete" => {
            let (name, key, who) = (store(&p)?, need(&p.key, "key")?, caller(&p)?);
            cs.delete(&who, &name, key).map_err(e)?;
            Ok(json!({"key": key, "deleted": true}))
        }
        other => Err(format!("unknown method: {other}")),
    }
}

/// Dispatch one `store.client.*` verb on the daemon's client store.
pub async fn handle(method: &str, params: Value) -> Response {
    let Some(cs) = crate::client_store_rt::global() else {
        return Response::error(
            "this daemon has no client store (no client-store.json, or it is not running as the client user)".to_string(),
        );
    };
    let method = method.to_owned();
    match tokio::task::spawn_blocking(move || run(&cs, &method, params)).await {
        Ok(Ok(v)) => Response::success(v),
        Ok(Err(e)) => Response::error(e),
        Err(e) => Response::error(format!("client store task failed: {e}")),
    }
}

#[cfg(test)]
#[path = "client_store_rpc_tests.rs"]
mod tests;
