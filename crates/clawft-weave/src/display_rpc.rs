//! `display.*`: an app drives a leaf's display through the daemon
//! (ADR-117 D1/D2). The daemon owns the certified-leaf channel; the app
//! (the chief-of-staff console host) holds a session over this RPC and
//! never speaks to the pad.
//!
//! | method | capability | params -> result |
//! |---|---|---|
//! | `display.open` | Write | `{leaf_id, subject, resource?, proof_hex?, display?}` -> session record |
//! | `display.ops` | Write | `{session, cbor_b64}` (a CBOR `SceneEnvelope`) -> `{ok}` |
//! | `display.poll` | Write | `{session, max?}` -> `{events, rejected, ended, dropped}` |
//! | `display.close` | Write | `{session}` -> `{ok}` |
//! | `display.revoke` | Admin | `{subject}` -> `{revoked}`; ends that subject's sessions on their next call |
//! | `display.status` | Read | `{leaf_id}` -> `{connected, session}`: does the leaf have a route to this daemon, and who holds its screen |
//!
//! Outside a project these are user-level Admin acts (`scope_gate`). Failures carry an `error_kind` (`display_denied`, `display_refused`,
//! `display_unreachable`, `display_no_session`, `display_ended`,
//! `display_bad_request`). Open, close and revocation land on the chain.

use std::sync::{Arc, Mutex, OnceLock};

use serde::Deserialize;
use serde_json::{json, Value};
use weftos_leaf_scene::session::b64;
use weftos_leaf_scene::{codec, SceneEnvelope};

use crate::display_endpoint::{DisplayEndpoint, EndpointConfig, EndpointError, RevocablePolicy, RouterSink};
use crate::protocol::Response;
use crate::rpc_ext::{ExtCall, ExtFuture, KernelRef};

struct Entry {
    router_key: usize,
    endpoint: Arc<DisplayEndpoint>,
    policy: Arc<RevocablePolicy>,
}

fn registry() -> &'static Mutex<Vec<Arc<Entry>>> {
    static R: OnceLock<Mutex<Vec<Arc<Entry>>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(Vec::new()))
}

/// The endpoint bound to this kernel's router, made on first use.
async fn entry(kernel: &KernelRef) -> Arc<Entry> {
    let router = kernel.read().await.a2a_router().clone();
    let key = Arc::as_ptr(&router) as usize;
    let mut all = registry().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(e) = all.iter().find(|e| e.router_key == key) {
        return Arc::clone(e);
    }
    let policy = Arc::new(RevocablePolicy::default());
    let endpoint = Arc::new(DisplayEndpoint::new(Arc::new(RouterSink::new(router)), policy.clone(), EndpointConfig::default()));
    let e = Arc::new(Entry { router_key: key, endpoint, policy });
    all.push(Arc::clone(&e));
    e
}

#[derive(Deserialize)]
struct OpenParams {
    leaf_id: String,
    subject: String,
    #[serde(default)]
    resource: String,
    #[serde(default)]
    proof_hex: String,
    #[serde(default)]
    display: u8,
}

#[derive(Deserialize)]
struct OpsParams {
    session: u64,
    cbor_b64: String,
}

#[derive(Deserialize)]
struct PollParams {
    session: u64,
    #[serde(default = "default_max")]
    max: usize,
}

fn default_max() -> usize {
    256
}

#[derive(Deserialize)]
struct SessionParams {
    session: u64,
}

#[derive(Deserialize)]
struct RevokeParams {
    subject: String,
}

pub fn handle(call: ExtCall) -> ExtFuture {
    Box::pin(async move {
        let e = entry(&call.ctx.kernel).await;
        match call.method.as_str() {
            "display.open" => open(&call, &e).await,
            "display.ops" => ops(&call, &e).await,
            "display.poll" => poll(&call, &e).await,
            "display.close" => close(&call, &e).await,
            "display.revoke" => revoke(&call, &e).await,
            "display.status" => status(&call, &e).await,
            other => Response::error(format!("unknown display method {other}")),
        }
    })
}

fn params<T: serde::de::DeserializeOwned>(call: &ExtCall) -> Result<T, String> {
    serde_json::from_value(call.params.clone()).map_err(|e| format!("invalid params: {e}"))
}

fn fail(e: EndpointError) -> Response {
    Response::error_with_kind(e.kind().to_owned(), e.to_string())
}

async fn open(call: &ExtCall, e: &Entry) -> Response {
    let p: OpenParams = match params(call) {
        Ok(p) => p,
        Err(msg) => return Response::error(msg),
    };
    let proof = match decode_hex(&p.proof_hex) {
        Some(b) => b,
        None => return fail(EndpointError::Bad("proof_hex is not hex".into())),
    };
    {
        // Subscribe to the leaf's `.input` before the Open goes out, so
        // the `Opened` answer cannot be missed.
        let k = call.ctx.kernel.read().await;
        e.endpoint.attach_input(k.a2a_router().topic_router(), &p.leaf_id);
    }
    let resource = if p.resource.is_empty() { p.leaf_id.clone() } else { p.resource.clone() };
    let result = e.endpoint.open(&p.leaf_id, &p.subject, &resource, proof, p.display).await;
    chain(&call.ctx.kernel, "display.open", json!({
        "leaf_id": p.leaf_id, "subject": p.subject, "resource": resource,
        "result": match &result { Ok(r) => json!({"session": r.session}), Err(e) => json!({"error": e.kind()}) },
    })).await;
    match result {
        Ok(r) => Response::success(serde_json::to_value(r).unwrap_or(Value::Null)),
        Err(e) => fail(e),
    }
}

async fn ops(call: &ExtCall, e: &Entry) -> Response {
    let p: OpsParams = match params(call) {
        Ok(p) => p,
        Err(msg) => return Response::error(msg),
    };
    let env: SceneEnvelope = match b64::decode(p.cbor_b64.as_bytes()).ok_or("cbor_b64 is not base64").and_then(|b| {
        codec::decode_scene_envelope(&b).map_err(|_| "cbor_b64 is not a SceneEnvelope of this wire version")
    }) {
        Ok(env) => env,
        Err(why) => return fail(EndpointError::Bad(why.into())),
    };
    match e.endpoint.ops(p.session, env).await {
        Ok(()) => Response::success(json!({ "ok": true })),
        Err(err) => fail(err),
    }
}

async fn poll(call: &ExtCall, e: &Entry) -> Response {
    let p: PollParams = match params(call) {
        Ok(p) => p,
        Err(msg) => return Response::error(msg),
    };
    match e.endpoint.poll(p.session, p.max).await {
        Ok(r) => Response::success(serde_json::to_value(r).unwrap_or(Value::Null)),
        Err(err) => fail(err),
    }
}

async fn close(call: &ExtCall, e: &Entry) -> Response {
    let p: SessionParams = match params(call) {
        Ok(p) => p,
        Err(msg) => return Response::error(msg),
    };
    let _ = e.endpoint.close(p.session).await;
    chain(&call.ctx.kernel, "display.close", json!({ "session": p.session })).await;
    Response::success(json!({ "ok": true }))
}

async fn revoke(call: &ExtCall, e: &Entry) -> Response {
    let p: RevokeParams = match params(call) {
        Ok(p) => p,
        Err(msg) => return Response::error(msg),
    };
    e.policy.revoke(&p.subject);
    chain(&call.ctx.kernel, "display.revoke", json!({ "subject": p.subject })).await;
    Response::success(json!({ "revoked": p.subject }))
}

#[derive(Deserialize)]
struct StatusParams {
    leaf_id: String,
}

/// Is the leaf connected (a mesh route for its push topic) and who holds
/// its screen. The route check is the same one a push makes.
async fn status(call: &ExtCall, e: &Entry) -> Response {
    let p: StatusParams = match params(call) {
        Ok(p) => p,
        Err(msg) => return Response::error(msg),
    };
    let topic = weftos_leaf_types::push_topic(&p.leaf_id);
    #[cfg(feature = "mesh")]
    let connected = {
        let k = call.ctx.kernel.read().await;
        k.a2a_router().mesh_runtime().is_some_and(|rt| !rt.peers_for_topic(&topic).is_empty())
    };
    #[cfg(not(feature = "mesh"))]
    let connected = false;
    Response::success(json!({
        "leaf_id": p.leaf_id,
        "push_topic": topic,
        "connected": connected,
        "session": e.endpoint.holder(&p.leaf_id),
    }))
}

fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
}

/// Record on the chain when it is running; no payload carries op data.
async fn chain(kernel: &KernelRef, kind: &str, payload: Value) {
    #[cfg(feature = "exochain")]
    {
        let k = kernel.read().await;
        if let Some(cm) = k.chain_manager() {
            cm.append("display", kind, Some(payload));
        }
    }
    #[cfg(not(feature = "exochain"))]
    {
        let _ = (kernel, kind, payload);
    }
}
