//! Loopback HTTP/SSE shell over [`Conversation`] (contract §3).
//!
//! ```text
//! POST /v1/cos/utterance {text, source}        -> 202 {turn_id}
//! POST /v1/cos/answer {kind, id, value}        -> 202 {turn_id}   (guide touch)
//! GET  /v1/cos/events                          -> text/event-stream of Event
//! GET  /healthz                          -> 200 {ok: true}
//! ```
//!
//! Binds only to a loopback address; anything else is refused before the
//! socket opens.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::routing::{get, post};
use axum::Router;
use futures_util::Stream;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::broadcast::error::RecvError;

use crate::CosError;
use crate::conversation::{Conversation, GuideAnswer, UserUtterance};

type Shared = Arc<dyn Conversation>;

/// Build the router over a conversation.
pub fn router(conv: Shared) -> Router {
    Router::new()
        .route("/v1/cos/utterance", post(utterance))
        .route("/v1/cos/answer", post(answer))
        .route("/v1/cos/events", get(events))
        .route("/healthz", get(|| async { Json(json!({"ok": true})) }))
        .with_state(conv)
}

/// Bind `addr` (loopback only) and return the bound address with the serve
/// future, so a caller can learn an ephemeral port before awaiting it.
pub async fn bind(conv: Shared, addr: SocketAddr) -> Result<(SocketAddr, impl std::future::Future<Output = std::io::Result<()>>), CosError> {
    if !addr.ip().is_loopback() {
        return Err(CosError::Config(format!("refusing to bind {addr}: loopback only")));
    }
    let listener = TcpListener::bind(addr).await.map_err(|e| CosError::Config(format!("bind {addr}: {e}")))?;
    let local = listener.local_addr().map_err(|e| CosError::Config(e.to_string()))?;
    let app = router(conv);
    Ok((local, async move { axum::serve(listener, app).await }))
}

/// Bind and serve until the process ends.
pub async fn serve(conv: Shared, addr: SocketAddr) -> Result<(), CosError> {
    let (local, fut) = bind(conv, addr).await?;
    tracing::info!(%local, "chief-of-staff conversation API listening");
    fut.await.map_err(|e| CosError::Config(format!("serve: {e}")))
}

async fn utterance(State(conv): State<Shared>, body: Result<Json<UserUtterance>, axum::extract::rejection::JsonRejection>) -> (StatusCode, Json<Value>) {
    let Json(utt) = match body {
        Ok(b) => b,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(json!({"error": format!("invalid utterance: {e}")}))),
    };
    match conv.utter(utt).await {
        Ok(turn_id) => (StatusCode::ACCEPTED, Json(json!({"turn_id": turn_id}))),
        Err(CosError::Config(msg)) => (StatusCode::BAD_REQUEST, Json(json!({"error": msg}))),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))),
    }
}

/// `POST /v1/cos/answer`: a touch interaction with a guide view.
async fn answer(State(conv): State<Shared>, body: Result<Json<GuideAnswer>, axum::extract::rejection::JsonRejection>) -> (StatusCode, Json<Value>) {
    let Json(ans) = match body {
        Ok(b) => b,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(json!({"error": format!("invalid answer: {e}")}))),
    };
    match conv.answer(ans).await {
        Ok(turn_id) => (StatusCode::ACCEPTED, Json(json!({"turn_id": turn_id}))),
        Err(CosError::Config(msg)) => (StatusCode::BAD_REQUEST, Json(json!({"error": msg}))),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))),
    }
}

async fn events(State(conv): State<Shared>) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    let rx = conv.events();
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
