//! MCP over HTTP inside the gateway (ADR-102 D2).
//!
//! `POST /mcp` carries JSON-RPC (`initialize`, `tools/list`, `tools/call`)
//! for the same origin and the same bearer as the REST API: the gateway's
//! daemon-token middleware authenticates, then every call runs with
//! [`SessionScopes::owner`] (ADR-102 D4: a token is owner-equivalent).
//! The tool surface is whatever [`McpServerShell`] the gateway was built
//! with (the `full` profile in production).
//!
//! Unlike `weft mcp-server --listen`, there is no second token store and no
//! SSE stream here; JSON-RPC over POST only.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use super::ApiState;
use super::auth::TokenMeta;
use crate::mcp::server::McpServerShell;
use crate::mcp::session_cap::SessionScopes;
use crate::mcp::stateless;

/// Largest accepted request body.
const MAX_BODY: usize = 1 << 20;

/// The shell is one `&mut` state machine, so calls are serialised. A caller
/// waits at most this long for its turn before getting 503 (so `tools/list`
/// is not stuck behind a slow tool for long).
pub const QUEUE_TIMEOUT: Duration = Duration::from_secs(10);

/// Longest a single JSON-RPC call may run. Past it the call is cancelled,
/// the lock is released, and the caller gets a JSON-RPC error.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(120);

/// The shell mounted at `/mcp`, with the facts `/api/health` reports.
pub struct McpMount {
    shell: Mutex<McpServerShell>,
    /// Handle the audit middleware reads the client label from.
    audit_label: Arc<RwLock<String>>,
    /// Serve profile name (e.g. `full`).
    pub profile: String,
    queue_timeout: Duration,
    call_timeout: Duration,
    /// Number of tools the shell lists.
    pub tool_count: usize,
}

impl McpMount {
    /// Mount `shell`; `audit_label` is the handle its `AuditLog` exposes.
    pub fn new(
        shell: McpServerShell,
        audit_label: Arc<RwLock<String>>,
        profile: impl Into<String>,
        tool_count: usize,
    ) -> Self {
        Self {
            shell: Mutex::new(shell),
            audit_label,
            profile: profile.into(),
            queue_timeout: QUEUE_TIMEOUT,
            call_timeout: CALL_TIMEOUT,
            tool_count,
        }
    }

    /// Override the queue and per-call timeouts (tests).
    pub fn with_timeouts(mut self, queue: Duration, call: Duration) -> Self {
        self.queue_timeout = queue;
        self.call_timeout = call;
        self
    }
}

/// `/mcp` routes. The caller wraps them in the auth middleware.
pub fn mcp_routes() -> Router<ApiState> {
    Router::new().route("/mcp", post(mcp_post))
}

async fn mcp_post(State(state): State<ApiState>, request: axum::extract::Request) -> Response {
    let Some(mount) = state.mcp.clone() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let label = request.extensions().get::<TokenMeta>().map(|m| m.label.clone());
    let headers = request.headers().clone();
    let body = match axum::body::to_bytes(request.into_body(), MAX_BODY).await {
        Ok(b) => b,
        Err(_) => return StatusCode::PAYLOAD_TOO_LARGE.into_response(),
    };
    let msg: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "jsonrpc": "2.0", "id": null,
                    "error": { "code": -32700, "message": format!("parse error: {e}") },
                })),
            )
                .into_response();
        }
    };
    // Stateless (2026-07-28) requests must mirror version/method/name in headers.
    if let Some(err) = stateless::check_http_request(&msg, |n| {
        headers.get(n).and_then(|v| v.to_str().ok())
    }) {
        return (StatusCode::BAD_REQUEST, Json(err)).into_response();
    }
    // Wait for our turn, but not forever behind a slow tool.
    let Ok(mut shell) = tokio::time::timeout(mount.queue_timeout, mount.shell.lock()).await else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "jsonrpc": "2.0", "id": msg.get("id").cloned().unwrap_or(Value::Null),
                "error": { "code": -32002, "message": "server busy: another call is still running" },
            })),
        )
            .into_response();
    };
    // Set the audit label only now, while we hold the shell: another caller
    // cannot run (and be audited) between this write and our call.
    if let (Some(label), Ok(mut g)) = (label, mount.audit_label.write()) {
        *g = label;
    }
    let id = msg.get("id").cloned();
    let scopes = SessionScopes::owner();
    let call = shell.handle_message(msg, Some(&scopes));
    match tokio::time::timeout(mount.call_timeout, call).await {
        Ok(Some(resp)) => {
            let status = if stateless::is_modern_protocol_error(&resp) {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::OK
            };
            (status, Json(resp)).into_response()
        }
        Ok(None) => StatusCode::ACCEPTED.into_response(),
        Err(_) => (
            StatusCode::OK,
            Json(json!({
                "jsonrpc": "2.0", "id": id.unwrap_or(Value::Null),
                "error": { "code": -32003, "message": "call timed out" },
            })),
        )
            .into_response(),
    }
}
