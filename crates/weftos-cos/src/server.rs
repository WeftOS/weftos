//! Loopback HTTP/SSE shell over [`Conversation`] (contract §3).
//!
//! ```text
//! POST /v1/cos/utterance {text, source}        -> 202 {turn_id}
//! POST /v1/cos/answer {kind, id, value}        -> 202 {turn_id}   (guide touch)
//! POST /v1/cos/ptt {down}                      -> 202 {active, posted?|nothing?}
//! GET  /v1/cos/events                          -> text/event-stream of Event
//! GET  /v1/cos/turns?limit=N                   -> 200 {turns: [{turn_id, role, endpoint, text, ts}]}
//!                                                 (oldest first; limit 1..=100, default 20)
//! GET  /v1/cos/status                          -> 200 {state, reason, task, model, lanes_running}
//!                                                 (latest state/status events; read-only)
//! GET  /v1/cos/traces?since=&limit=N          -> 200 {traces: [Trace]}  (oldest first; since is RFC 3339, inclusive)
//! GET  /v1/cos/traces/<turn_id>                -> 200 Trace | 404
//! GET  /v1/cos/tools                           -> 200 {tools: [{id, description, gate, parameters}]}
//! GET  /v1/cos/devices                         -> 200 devices.status result (read-only, no model turn)
//! GET  /healthz                                -> 200 {ok: true, api: "cos.v1"}
//! ```
//!
//! Binds to a loopback address, plus (cos.v1.1) optionally the machine's
//! tailnet address; any other address is refused before the socket opens.
//! A loopback peer needs no token. Every other peer must present a per-endpoint
//! bearer token and acts only as that endpoint. The push-to-talk route is
//! bound only when the agent owns a microphone (`serve --mic`); otherwise it
//! answers 503, and a remote endpoint is refused (the microphone is the host's).

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{ConnectInfo, FromRequestParts, Path, RawQuery, State};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::routing::{get, post};
use axum::Router;
use futures_util::Stream;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::broadcast::error::RecvError;

use crate::CosError;
use crate::conversation::{Conversation, GuideAnswer, HOST_ENDPOINT, TURNS_DEFAULT_LIMIT, TURNS_MAX_LIMIT, UserUtterance};
use crate::endpoints::{AuthError, Caller, TokenStore, authorize, is_tailnet_ip};
use crate::traces::{TRACES_DEFAULT_LIMIT, TRACES_MAX_LIMIT, valid_since};
use crate::voice::{PttControl, PttEdge, PttOutcome};

/// The frozen contract version (`docs/plans/adr-118-cos-contract.md` section 3).
pub const API_VERSION: &str = "cos.v1";

type Shared = Arc<dyn Conversation>;
type SharedPtt = Option<Arc<dyn PttControl>>;

#[derive(Clone)]
struct Api {
    conv: Shared,
    ptt: SharedPtt,
    /// `None` on a loopback-only server: nothing to authenticate.
    tokens: Option<Arc<TokenStore>>,
}

/// `POST /v1/cos/ptt` body: the button edge.
#[derive(Debug, Clone, Deserialize)]
pub struct PttRequest {
    pub down: bool,
    /// Only `host` is accepted: push-to-talk captures the host microphone.
    #[serde(default)]
    pub endpoint: Option<String>,
}

/// Extractor: who is calling, per [`authorize`].
struct Authed(Caller);

impl FromRequestParts<Api> for Authed {
    type Rejection = (StatusCode, Json<Value>);

    async fn from_request_parts(parts: &mut Parts, api: &Api) -> Result<Self, Self::Rejection> {
        let Some(store) = &api.tokens else { return Ok(Authed(Caller::Local)) };
        let peer = parts.extensions.get::<ConnectInfo<SocketAddr>>().map(|c| c.0.ip());
        let header = parts.headers.get(axum::http::header::AUTHORIZATION).and_then(|v| v.to_str().ok());
        authorize(peer, header, store).map(Authed).map_err(|e| match e {
            AuthError::Missing => (StatusCode::UNAUTHORIZED, Json(json!({"error": "endpoint token required: send Authorization: Bearer <token>"}))),
            AuthError::Invalid => (StatusCode::UNAUTHORIZED, Json(json!({"error": "unknown endpoint token"}))),
        })
    }
}

/// Build the router over a conversation, without push-to-talk.
pub fn router(conv: Shared) -> Router {
    router_with(conv, None)
}

/// Build the router over a conversation and, when the agent owns the
/// microphone, the push-to-talk control.
pub fn router_with(conv: Shared, ptt: SharedPtt) -> Router {
    router_auth(conv, ptt, None)
}

/// [`router_with`] with per-endpoint token checks for non-loopback peers.
/// With `tokens`, a request whose peer address is unknown is treated as
/// remote.
pub fn router_auth(conv: Shared, ptt: SharedPtt, tokens: Option<Arc<TokenStore>>) -> Router {
    Router::new()
        .route("/v1/cos/utterance", post(utterance))
        .route("/v1/cos/answer", post(answer))
        .route("/v1/cos/ptt", post(ptt_edge))
        .route("/v1/cos/events", get(events))
        .route("/v1/cos/turns", get(turns))
        .route("/v1/cos/status", get(status))
        .route("/v1/cos/traces", get(traces))
        .route("/v1/cos/traces/{turn_id}", get(trace))
        .route("/v1/cos/tools", get(tools))
        .route("/v1/cos/devices", get(devices))
        .route("/healthz", get(|_caller: Authed| async { Json(json!({"ok": true, "api": API_VERSION})) }))
        .with_state(Api { conv, ptt, tokens })
}

/// Bind `addr` (loopback only) and return the bound address with the serve
/// future, so a caller can learn an ephemeral port before awaiting it.
pub async fn bind(conv: Shared, addr: SocketAddr) -> Result<(SocketAddr, impl std::future::Future<Output = std::io::Result<()>>), CosError> {
    bind_with(conv, None, addr).await
}

/// [`bind`] with a push-to-talk control on `POST /v1/cos/ptt`.
pub async fn bind_with(conv: Shared, ptt: SharedPtt, addr: SocketAddr) -> Result<(SocketAddr, impl std::future::Future<Output = std::io::Result<()>>), CosError> {
    if !addr.ip().is_loopback() {
        return Err(CosError::Config(format!("refusing to bind {addr}: loopback only")));
    }
    let listener = TcpListener::bind(addr).await.map_err(|e| CosError::Config(format!("bind {addr}: {e}")))?;
    let local = listener.local_addr().map_err(|e| CosError::Config(e.to_string()))?;
    let app = router_with(conv, ptt);
    Ok((local, async move { axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await }))
}

/// Where [`bind_remote`] is listening.
#[derive(Debug, Clone, Copy)]
pub struct Bound {
    pub loopback: SocketAddr,
    pub tailnet: SocketAddr,
}

/// Serve on `loopback` (token-free) and on `tailnet` (token required). The
/// second address must be a Tailscale one (100.64.0.0/10); anything else,
/// including a wildcard, is refused before a socket opens.
pub async fn bind_remote(
    conv: Shared,
    ptt: SharedPtt,
    loopback: SocketAddr,
    tailnet: SocketAddr,
    tokens: TokenStore,
) -> Result<(Bound, impl std::future::Future<Output = std::io::Result<()>>), CosError> {
    if !loopback.ip().is_loopback() {
        return Err(CosError::Config(format!("refusing to bind {loopback}: the local listener is loopback only")));
    }
    if !is_tailnet_ip(tailnet.ip()) {
        return Err(CosError::Config(format!("refusing to bind {tailnet}: not a tailnet address (100.64.0.0/10)")));
    }
    let lo = TcpListener::bind(loopback).await.map_err(|e| CosError::Config(format!("bind {loopback}: {e}")))?;
    let tn = TcpListener::bind(tailnet).await.map_err(|e| CosError::Config(format!("bind {tailnet}: {e}")))?;
    let bound = Bound {
        loopback: lo.local_addr().map_err(|e| CosError::Config(e.to_string()))?,
        tailnet: tn.local_addr().map_err(|e| CosError::Config(e.to_string()))?,
    };
    let app = router_auth(conv, ptt, Some(Arc::new(tokens)));
    let (a, b) = (app.clone(), app);
    Ok((bound, async move {
        tokio::try_join!(
            axum::serve(lo, a.into_make_service_with_connect_info::<SocketAddr>()),
            axum::serve(tn, b.into_make_service_with_connect_info::<SocketAddr>()),
        )
        .map(|_| ())
    }))
}

/// Bind and serve until the process ends.
pub async fn serve(conv: Shared, addr: SocketAddr) -> Result<(), CosError> {
    let (local, fut) = bind(conv, addr).await?;
    tracing::info!(%local, "chief-of-staff conversation API listening");
    fut.await.map_err(|e| CosError::Config(format!("serve: {e}")))
}

fn failure(e: CosError) -> (StatusCode, Json<Value>) {
    match e {
        CosError::Config(msg) => (StatusCode::BAD_REQUEST, Json(json!({"error": msg}))),
        e => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))),
    }
}

async fn utterance(State(api): State<Api>, Authed(caller): Authed, body: Result<Json<UserUtterance>, JsonRejection>) -> (StatusCode, Json<Value>) {
    let Json(mut utt) = match body {
        Ok(b) => b,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(json!({"error": format!("invalid utterance: {e}")}))),
    };
    if let Caller::Endpoint(name) = caller {
        match utt.endpoint.as_deref() {
            None => utt.endpoint = Some(name),
            Some(claimed) if claimed == name => {}
            Some(claimed) => return (StatusCode::FORBIDDEN, Json(json!({"error": format!("this token belongs to endpoint {name:?}, not {claimed:?}")}))),
        }
    }
    match api.conv.utter(utt).await {
        Ok(turn_id) => (StatusCode::ACCEPTED, Json(json!({"turn_id": turn_id}))),
        Err(e) => failure(e),
    }
}

/// `POST /v1/cos/answer`: a touch interaction with a guide view.
async fn answer(State(api): State<Api>, _caller: Authed, body: Result<Json<GuideAnswer>, JsonRejection>) -> (StatusCode, Json<Value>) {
    let Json(ans) = match body {
        Ok(b) => b,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(json!({"error": format!("invalid answer: {e}")}))),
    };
    match api.conv.answer(ans).await {
        Ok(turn_id) => (StatusCode::ACCEPTED, Json(json!({"turn_id": turn_id}))),
        Err(e) => failure(e),
    }
}

/// `POST /v1/cos/ptt`: a button edge from the console host. `down` starts
/// capture; `up` stops it, transcribes and posts the voice utterance, and
/// the reply says what was posted so the host can show the transcript.
async fn ptt_edge(State(api): State<Api>, Authed(caller): Authed, body: Result<Json<PttRequest>, JsonRejection>) -> (StatusCode, Json<Value>) {
    let Json(req) = match body {
        Ok(b) => b,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(json!({"error": format!("invalid ptt body: {e}")}))),
    };
    if let Caller::Endpoint(name) = &caller {
        return (StatusCode::FORBIDDEN, Json(json!({"error": format!("endpoint {name:?} cannot press push-to-talk: it captures the host microphone")})));
    }
    if let Some(ep) = req.endpoint.as_deref().filter(|e| *e != HOST_ENDPOINT) {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": format!("push-to-talk captures the host microphone; endpoint {ep:?} must speak through POST /v1/cos/utterance")})));
    }
    let Some(ptt) = api.ptt.as_ref() else {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error": "push-to-talk is not enabled; start the agent with --mic"})));
    };
    let edge = if req.down { PttEdge::Down } else { PttEdge::Up };
    match ptt.on_edge(edge).await {
        Ok(None) => (StatusCode::ACCEPTED, Json(json!({"active": ptt.is_active()}))),
        Ok(Some(PttOutcome::Posted { text, turn_id })) => (StatusCode::ACCEPTED, Json(json!({"active": false, "posted": {"text": text, "turn_id": turn_id}}))),
        Ok(Some(PttOutcome::Nothing { reason })) => (StatusCode::ACCEPTED, Json(json!({"active": false, "nothing": reason}))),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))),
    }
}

/// The `limit` of a `GET /v1/cos/turns` query string: absent is the default,
/// anything but a whole number in 1..=[`TURNS_MAX_LIMIT`] is refused.
fn turns_limit(query: Option<&str>) -> Result<usize, String> {
    let bad = || format!("limit must be a whole number from 1 to {TURNS_MAX_LIMIT}");
    let mut limit = None;
    for pair in query.unwrap_or("").split('&').filter(|p| !p.is_empty()) {
        match pair.split_once('=') {
            Some(("limit", raw)) => limit = Some(raw.parse::<usize>().ok().filter(|n| (1..=TURNS_MAX_LIMIT).contains(n)).ok_or_else(bad)?),
            None if pair == "limit" => return Err(bad()),
            _ => {}
        }
    }
    Ok(limit.unwrap_or(TURNS_DEFAULT_LIMIT))
}

/// `GET /v1/cos/turns`: the most recent final messages, oldest first.
async fn turns(State(api): State<Api>, _caller: Authed, RawQuery(query): RawQuery) -> (StatusCode, Json<Value>) {
    let limit = match turns_limit(query.as_deref()) {
        Ok(n) => n,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(json!({"error": e}))),
    };
    (StatusCode::OK, Json(json!({"turns": api.conv.recent_turns(limit)})))
}

/// `GET /v1/cos/status`: the agent's latest state, task and model.
async fn status(State(api): State<Api>, _caller: Authed) -> Json<Value> {
    Json(api.conv.status())
}

/// `since` and `limit` of `GET /v1/cos/traces`: absent is "all" and the
/// default; anything malformed is refused, never read as "no filter".
fn traces_query(query: Option<&str>) -> Result<(Option<String>, usize), String> {
    let mut since = None;
    let mut limit = TRACES_DEFAULT_LIMIT;
    for pair in query.unwrap_or("").split('&').filter(|p| !p.is_empty()) {
        match pair.split_once('=') {
            Some(("since", raw)) => {
                let raw = raw.replace("%3A", ":").replace("%3a", ":").replace("%2B", "+").replace("%2b", "+");
                if !valid_since(&raw) {
                    return Err("since must be an RFC 3339 timestamp".into());
                }
                since = Some(raw);
            }
            Some(("limit", raw)) => {
                limit = raw
                    .parse::<usize>()
                    .ok()
                    .filter(|n| (1..=TRACES_MAX_LIMIT).contains(n))
                    .ok_or_else(|| format!("limit must be a whole number from 1 to {TRACES_MAX_LIMIT}"))?;
            }
            None if pair == "since" || pair == "limit" => return Err(format!("{pair} needs a value")),
            _ => {}
        }
    }
    Ok((since, limit))
}

/// `GET /v1/cos/traces`: how recent turns were answered.
async fn traces(State(api): State<Api>, _caller: Authed, RawQuery(query): RawQuery) -> (StatusCode, Json<Value>) {
    match traces_query(query.as_deref()) {
        Ok((since, limit)) => (StatusCode::OK, Json(json!({"traces": api.conv.traces(since.as_deref(), limit)}))),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"error": e}))),
    }
}

/// `GET /v1/cos/traces/<turn_id>`: one turn's trace, once the turn has ended.
async fn trace(State(api): State<Api>, _caller: Authed, Path(turn_id): Path<String>) -> (StatusCode, Json<Value>) {
    match api.conv.trace(&turn_id) {
        Some(t) => (StatusCode::OK, Json(json!(t))),
        None => (StatusCode::NOT_FOUND, Json(json!({"error": "no trace for that turn (unknown, still running, or older than the last 200)"}))),
    }
}

/// `GET /v1/cos/tools`: the tools bound on this host.
async fn tools(State(api): State<Api>, _caller: Authed) -> Json<Value> {
    Json(json!({"tools": api.conv.tools()}))
}

/// `GET /v1/cos/devices`: `devices.status`, read-only, without a model turn.
async fn devices(State(api): State<Api>, _caller: Authed) -> (StatusCode, Json<Value>) {
    match api.conv.devices_status().await {
        Ok(v) => (StatusCode::OK, Json(v)),
        Err(e) => failure(e),
    }
}

async fn events(State(api): State<Api>, _caller: Authed) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    let rx = api.conv.events();
    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(ev) => {
                    let data = serde_json::to_string(&ev).unwrap_or_else(|_| "{}".into());
                    return Some((Ok(SseEvent::default().data(data)), rx));
                }
                Err(RecvError::Lagged(n)) => {
                    tracing::warn!(skipped = n, "SSE subscriber lagged");
                    continue;
                }
                Err(RecvError::Closed) => return None,
            }
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}
