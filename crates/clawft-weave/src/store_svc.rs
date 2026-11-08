//! The object store as a cog data plane service (ADR-126 S1, ADR-122 section 12): `svc.store/<node>`.
//!
//! A cog that lists `svc.store` (schema `store.v1`) as an `[[inputs]]` entry calls the daemon's
//! store through the broker, with no socket to the store and no keys of its own. The daemon
//! serves the topic in-process, one provider per project so that a call stays inside its project
//! (a cross-project call would need a policy grant, and the node's own bucket cannot be granted):
//! `svc.store/<project ULID, lower case>` for cogs placed in that project, whose default store
//! is the project's, and `svc.store/<node>` for cogs with no project, whose default store is the
//! node's. The broker governs every call
//! (`cogbus.call`), and the store service governs it again: the principal is `cog:<instance id>`
//! and needs a grant on the store, so a cog without one is refused and the refusal is in the
//! store's activity log.
//!
//! Request body (at most 64 KiB, the broker's limit):
//! `{"op": "put"|"get"|"head"|"list"|"delete", "key": …, "text": … | "b64": …, "prefix": …,
//!   "if_none_match": bool, "if_match": "<etag>", "store": "weftos://…"}`.
//! `store` is optional: the default is the provider's store (above). Reply: `put`/`head` give `{key,size,etag}`; `get` gives
//! `{key,size,b64,text}` (an object over the reply limit is an error, not a truncation); `list`
//! gives `{objects:[{key,size,etag}]}`; `delete` gives `{key,deleted:true}`.

use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use clawft_types::cog_topics::{StoreCall as Req, TopicClass};
use serde_json::{Value, json};
use weftos_cogbus::{Broker, CallDelivery, Event, InputSpec, InstanceSpec, OutputSpec, Session};
use weftos_store::service::Caller;
use weftos_store::{Principal, PrincipalKind, StoreName, StoreService};

/// The broker instance that provides the service.
pub const INSTANCE: &str = "weftos-store-svc";
/// Its cog id.
pub const COG: &str = "weftos-store";
/// Topic kind of the service.
pub const KIND: &str = "svc.store";
/// Schema of the service.
pub const SCHEMA: &str = "store.v1";
/// The broker's body limit (`weftos_cogbus::proto::MAX_BODY`); a reply over it is refused there.
const MAX_REPLY: usize = 64 * 1024;

/// The topic's source id for this node.
pub fn source_id(node_id: &str) -> String {
    crate::store_rt::node_token(node_id)
}

/// Source id of a project's provider: its ULID in lower case.
pub fn project_source(ulid: &str) -> String {
    ulid.to_ascii_lowercase()
}

/// Run one call. `project` is the project the provider serves (None: the node).
pub fn call(svc: &StoreService, node_id: &str, from: &str, project: Option<&str>, body: Value) -> Result<Value, String> {
    let r: Req = serde_json::from_value(body).map_err(|e| format!("invalid request: {e}"))?;
    r.validate().map_err(|e| format!("invalid request: {e}"))?;
    let who = Caller::local(Principal::new(PrincipalKind::Cog, from).map_err(|e| e.to_string())?);
    let mesh = crate::store_rt::mesh_token();
    let name = match (&r.store, project) {
        (Some(s), _) => StoreName::parse(s).map_err(|e| e.to_string())?,
        (None, Some(p)) => StoreName::project(&mesh, p).map_err(|e| e.to_string())?,
        (None, None) => StoreName::node(&mesh, &source_id(node_id)).map_err(|e| e.to_string())?,
    };
    let e = |e: weftos_store::StoreError| e.to_string();
    let key = || r.key.clone().filter(|k| !k.is_empty()).ok_or_else(|| format!("{} needs a key", r.op));
    match r.op.as_str() {
        "put" => {
            let data = match (&r.text, &r.b64) {
                (Some(t), None) => t.clone().into_bytes(),
                (None, Some(b)) => B64.decode(b).map_err(|e| format!("b64: {e}"))?,
                _ => return Err("put needs exactly one of text, b64".into()),
            };
            let m = match &r.if_match {
                Some(etag) => svc.put_if_match(&who, &name, &key()?, &data, etag),
                None => svc.put(&who, &name, &key()?, &data, r.if_none_match),
            }
            .map_err(e)?;
            Ok(json!({"key": m.key, "size": m.size, "etag": m.etag}))
        }
        "get" => {
            let k = key()?;
            let d = svc.get(&who, &name, &k).map_err(e)?;
            // base64 is 4/3 the size, plus the envelope.
            if d.len().div_ceil(3) * 4 + 512 > MAX_REPLY {
                return Err(format!("{k} is {} bytes: over what a service reply can carry", d.len()));
            }
            Ok(json!({"key": k, "size": d.len(), "b64": B64.encode(&d), "text": String::from_utf8(d).ok()}))
        }
        "head" => {
            let m = svc.head(&who, &name, &key()?).map_err(e)?;
            Ok(json!({"key": m.key, "size": m.size, "etag": m.etag}))
        }
        "delete" => {
            let k = key()?;
            svc.delete(&who, &name, &k).map_err(e)?;
            Ok(json!({"key": k, "deleted": true}))
        }
        "list" => {
            let mut objs: Vec<Value> = Vec::new();
            let mut size = 64;
            for m in svc.list(&who, &name, r.prefix.as_deref().unwrap_or("")).map_err(e)? {
                size += m.key.len() + m.etag.len() + 64;
                if size > MAX_REPLY {
                    return Err("too many objects for one reply: narrow the prefix".into());
                }
                objs.push(json!({"key": m.key, "size": m.size, "etag": m.etag}));
            }
            Ok(json!({"objects": objs}))
        }
        other => Err(format!("unknown op {other:?}: put, get, head, list or delete")),
    }
}

/// Serve the node's provider and one per existing project store, on the cog data plane.
/// Needs a running tokio runtime. Returns false when the daemon has no object store.
pub fn attach(broker: &Arc<Broker>, node_id: &str) -> Result<bool, String> {
    let Some(store) = crate::store_rt::global() else { return Ok(false) };
    attach_with(broker, node_id, store).map(|()| true)
}

/// [`attach`] for a given store service.
pub fn attach_with(broker: &Arc<Broker>, node_id: &str, store: Arc<StoreService>) -> Result<(), String> {
    serve(broker, node_id, store.clone(), None)?;
    for info in store.list_stores().into_iter().filter(|i| i.kind == "project") {
        if let Ok(StoreName::Project { project, .. }) = StoreName::parse(&info.name)
            && let Err(e) = serve(broker, node_id, store.clone(), Some(&project.to_string()))
        {
            tracing::warn!(error = %e, store = %info.name, "project store not offered to cogs");
        }
    }
    Ok(())
}

/// The `project add` hook: offer a new project's store to its cogs, if the cog data plane is up.
pub fn project_added(ulid: &str) -> Result<(), String> {
    let (Some(rt), Some(store)) = (crate::cogbus_serve::runtime(), crate::store_rt::global()) else { return Ok(()) };
    let _guard = tokio::runtime::Handle::try_current().map_err(|_| "no async runtime".to_owned())?;
    serve(&rt.broker, rt.broker.node_id(), store, Some(ulid))
}

/// Register one provider instance and answer its calls. Idempotent per provider.
fn serve(broker: &Arc<Broker>, node_id: &str, store: Arc<StoreService>, project: Option<&str>) -> Result<(), String> {
    let (instance, src) = match project {
        Some(p) => (format!("{INSTANCE}-{}", project_source(p)), project_source(p)),
        None => (INSTANCE.to_owned(), source_id(node_id)),
    };
    if broker.serves(&format!("{KIND}/{src}")) {
        return Ok(());
    }
    let spec = InstanceSpec {
        instance_id: instance.clone(),
        cog_id: COG.into(),
        project_id: project.map(str::to_owned),
        uid: None,
        outputs: vec![OutputSpec { topic: KIND.into(), schema: SCHEMA.into(), class: TopicClass::Reading }],
        inputs: Vec::<InputSpec>::new(),
    };
    // The token is never given to anyone: the session is opened here.
    let token: String = rand::random::<[u8; 32]>().iter().map(|b| format!("{b:02x}")).collect();
    broker.register(spec, &token).map_err(|e| format!("register {instance}: {}", e.message))?;
    let mut session = match broker.authenticate(&token, None) {
        Ok(s) => s,
        Err(e) => {
            broker.revoke(&instance);
            return Err(format!("open {instance}: {}", e.message));
        }
    };
    if let Err(e) = broker.serve_topic(&session, KIND, &src, SCHEMA) {
        broker.revoke(&instance);
        return Err(format!("serve {KIND}/{src}: {}", e.message));
    }
    let Some(mut rx) = session.take_pushed() else {
        broker.revoke(&instance);
        return Err("session has no push channel".into());
    };
    let (broker, session, node, project) = (broker.clone(), Arc::new(session), node_id.to_owned(), project.map(str::to_owned));
    tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            let Event::Call(d) = ev else { continue };
            let (broker, session, store, node, project) = (broker.clone(), session.clone(), store.clone(), node.clone(), project.clone());
            // The RustFS backend blocks, so a call never runs on the broker's tasks.
            tokio::task::spawn_blocking(move || answer(&broker, &session, &store, &node, project.as_deref(), d));
        }
    });
    tracing::info!(topic = %format!("{KIND}/{src}"), "cog store service up");
    Ok(())
}

fn answer(broker: &Broker, session: &Session, store: &StoreService, node: &str, project: Option<&str>, d: CallDelivery) {
    let res = call(store, node, &d.from, project, d.body.clone());
    if let Err(e) = broker.reply_call(session, d.call, res) {
        tracing::debug!(target: "store", call = d.call, error = %e.message, "store service reply");
    }
}

#[cfg(test)]
#[path = "store_svc_tests.rs"]
mod tests;
