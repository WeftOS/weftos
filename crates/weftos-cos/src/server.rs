//! Loopback HTTP/SSE shell over [`Conversation`] (contract §3).
//!
//! ```text
//! POST /v1/cos/utterance {text, source}        -> 202 {turn_id}
//! POST /v1/cos/answer {kind, id, value}        -> 202 {turn_id}   (guide touch)
//! POST /v1/cos/ptt {down}                      -> 202 {active, posted?|nothing?}
//! GET  /v1/cos/events                          -> text/event-stream of Event
//! GET  /healthz                                -> 200 {ok: true, api: "cos.v1"}
//! ```
//!
//! Binds only to a loopback address; anything else is refused before the
//! socket opens. The push-to-talk route is bound only when the agent owns a
//! microphone (`serve --mic`); otherwise it answers 503.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use axum::http::StatusCode;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::routing::{get, post};
use axum::Router;
use futures_util::Stream;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::broadcast::error::RecvError;

use crate::CosError;
use crate::conversation::{Conversation, GuideAnswer, UserUtterance};
use crate::voice::{PttControl, PttEdge, PttOutcome};

/// The frozen contract version (`docs/plans/adr-118-cos-contract.md` section 3).
pub const API_VERSION: &str = "cos.v1";

type Shared = Arc<dyn Conversation>;
type SharedPtt = Option<Arc<dyn PttControl>>;

#[derive(Clone)]
struct Api {
    conv: Shared,
    ptt: SharedPtt,
}

/// `POST /v1/cos/ptt` body: the button edge.
#[derive(Debug, Clone, Copy, Deserialize)]
pub struct PttRequest {
    pub down: bool,
}

/// Build the router over a conversation, without push-to-talk.
pub fn router(conv: Shared) -> Router {
    router_with(conv, None)
}

/// Build the router over a conversation and, when the agent owns the
/// microphone, the push-to-talk control.
pub fn router_with(conv: Shared, ptt: SharedPtt) -> Router {
    Router::new()
        .route("/v1/cos/utterance", post(utterance))
        .route("/v1/cos/answer", post(answer))
        .route("/v1/cos/ptt", post(ptt_edge))
        .route("/v1/cos/events", get(events))
        .route("/healthz", get(|| async { Json(json!({"ok": true, "api": API_VERSION})) }))
        .with_state(Api { conv, ptt })
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
    Ok((local, async move { axum::serve(listener, app).await }))
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

async fn utterance(State(api): State<Api>, body: Result<Json<UserUtterance>, JsonRejection>) -> (StatusCode, Json<Value>) {
    let Json(utt) = match body {
        Ok(b) => b,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(json!({"error": format!("invalid utterance: {e}")}))),
    };
    match api.conv.utter(utt).await {
        Ok(turn_id) => (StatusCode::ACCEPTED, Json(json!({"turn_id": turn_id}))),
        Err(e) => failure(e),
    }
}

/// `POST /v1/cos/answer`: a touch interaction with a guide view.
async fn answer(State(api): State<Api>, body: Result<Json<GuideAnswer>, JsonRejection>) -> (StatusCode, Json<Value>) {
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
async fn ptt_edge(State(api): State<Api>, body: Result<Json<PttRequest>, JsonRejection>) -> (StatusCode, Json<Value>) {
    let Json(req) = match body {
        Ok(b) => b,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(json!({"error": format!("invalid ptt body: {e}")}))),
    };
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

async fn events(State(api): State<Api>) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
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
