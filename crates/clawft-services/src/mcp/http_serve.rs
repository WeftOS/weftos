//! HTTP / SSE MCP server listen mode (ADR-075 G4 / WEFT-696).
//!
//! Serves JSON-RPC over `POST /mcp` (streamable HTTP compatible with the
//! existing [`super::transport::HttpTransport`] client) and optional
//! `GET /mcp/sse` for server→client keepalives / notifications.
//!
//! Auth: Bearer capability tokens ([`super::session_cap`]). Auth is required
//! by default; public bind requires `--dangerously-bind-public`.
//!
//! TLS: terminate at a reverse proxy (nginx, Caddy, cloud LB). This module
//! serves plain HTTP intentionally — operators put TLS at the edge.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::RwLock;
use std::time::Duration;

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Router, body::Bytes};
use futures_util::stream::{self, Stream};
use serde_json::Value;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tracing::{info, warn};

use super::server::McpServerShell;
use super::stateless;
use super::session_cap::{
    ClientLabel, SessionCapability, SessionTokenStore, extract_bearer, validate_listen_policy,
};

/// Shared state for the HTTP MCP endpoint.
pub struct HttpMcpState {
    pub shell: Mutex<McpServerShell>,
    pub tokens: Arc<SessionTokenStore>,
    /// Shared audit client label (updated per authenticated request).
    pub audit_client_label: Arc<RwLock<String>>,
}

impl HttpMcpState {
    /// Build state from a ready shell + token store.
    pub fn new(
        shell: McpServerShell,
        tokens: Arc<SessionTokenStore>,
        audit_client_label: Arc<RwLock<String>>,
    ) -> Self {
        Self {
            shell: Mutex::new(shell),
            tokens,
            audit_client_label,
        }
    }
}

/// Configuration for [`serve_mcp_http`].
#[derive(Debug, Clone)]
pub struct HttpMcpConfig {
    /// Host portion of `--listen` (e.g. `127.0.0.1`, `0.0.0.0`).
    pub host: String,
    /// Port.
    pub port: u16,
    /// Allow unauthenticated access (loopback only; never with public bind).
    pub allow_unauthenticated: bool,
    /// Explicit opt-in for non-loopback bind.
    pub dangerously_bind_public: bool,
}

impl HttpMcpConfig {
    /// Parse `host:port` listen string.
    pub fn parse_listen(listen: &str) -> Result<(String, u16), String> {
        let listen = listen.trim();
        // IPv6: [::1]:8742
        if let Some(rest) = listen.strip_prefix('[') {
            let (host, port_part) = rest
                .split_once("]:")
                .ok_or_else(|| format!("invalid IPv6 listen address: {listen}"))?;
            let port: u16 = port_part
                .parse()
                .map_err(|_| format!("invalid port in listen address: {listen}"))?;
            return Ok((host.to_string(), port));
        }
        let (host, port_s) = listen
            .rsplit_once(':')
            .ok_or_else(|| format!("listen must be host:port, got '{listen}'"))?;
        let port: u16 = port_s
            .parse()
            .map_err(|_| format!("invalid port in listen address: {listen}"))?;
        if host.is_empty() {
            return Err("listen host is empty".into());
        }
        Ok((host.to_string(), port))
    }
}

/// Build the axum router for MCP HTTP/SSE.
pub fn mcp_http_router(state: Arc<HttpMcpState>) -> Router {
    Router::new()
        .route("/mcp", post(mcp_post_handler).get(mcp_sse_handler))
        .route("/mcp/sse", get(mcp_sse_handler))
        .route("/health", get(health_handler))
        .with_state(state)
}

/// Bind and serve until the process is cancelled.
pub async fn serve_mcp_http(
    config: HttpMcpConfig,
    state: Arc<HttpMcpState>,
) -> Result<(), String> {
    let auth_required = !config.allow_unauthenticated;
    // Ensure store policy matches flags.
    if auth_required != state.tokens.auth_required() {
        warn!(
            auth_required,
            store = state.tokens.auth_required(),
            "token store auth_required mismatch with listen flags"
        );
    }

    validate_listen_policy(
        &config.host,
        auth_required,
        config.allow_unauthenticated,
        config.dangerously_bind_public,
    )?;

    let addr: SocketAddr = format!("{}:{}", config.host, config.port)
        .parse()
        .map_err(|e| format!("invalid listen address: {e}"))?;

    let listener = TcpListener::bind(addr)
        .await
        .map_err(|e| format!("bind {addr}: {e}"))?;
    let bound = listener
        .local_addr()
        .map_err(|e| format!("local_addr: {e}"))?;

    info!(
        %bound,
        auth_required,
        "MCP HTTP/SSE server listening (POST /mcp, GET /mcp/sse, GET /health)"
    );
    if auth_required {
        info!("MCP auth: Authorization: Bearer <token> required");
    } else {
        warn!("MCP auth DISABLED (--allow-unauthenticated); loopback only");
    }

    let app = mcp_http_router(state);
    axum::serve(listener, app)
        .await
        .map_err(|e| format!("serve error: {e}"))?;
    Ok(())
}

async fn health_handler() -> impl IntoResponse {
    Json(serde_json::json!({ "ok": true, "service": "weft-mcp" }))
}

/// Authenticate request headers → session capability.
fn auth_from_headers(
    tokens: &SessionTokenStore,
    headers: &HeaderMap,
) -> Result<SessionCapability, StatusCode> {
    let auth_hdr = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    let bearer = extract_bearer(auth_hdr);

    let label_hint = headers
        .get("x-mcp-client")
        .or_else(|| headers.get("X-MCP-Client"))
        .and_then(|v| v.to_str().ok());

    tokens
        .validate_with_label(bearer, label_hint)
        .ok_or(StatusCode::UNAUTHORIZED)
}

fn set_audit_label(state: &HttpMcpState, cap: &SessionCapability) {
    if let Ok(mut g) = state.audit_client_label.write() {
        *g = cap.client_label.as_str().to_string();
    }
}

async fn mcp_post_handler(
    State(state): State<Arc<HttpMcpState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let cap = match auth_from_headers(&state.tokens, &headers) {
        Ok(c) => c,
        Err(status) => {
            return (
                status,
                [(header::WWW_AUTHENTICATE, "Bearer")],
                Json(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": null,
                    "error": { "code": -32001, "message": "unauthorized: valid Bearer token required" }
                })),
            )
                .into_response();
        }
    };
    set_audit_label(&state, &cap);

    // Notifications / requests
    let msg: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": null,
                    "error": { "code": -32700, "message": format!("parse error: {e}") }
                })),
            )
                .into_response();
        }
    };

    // Prefer clientInfo.name on initialize for label.
    if msg.get("method").and_then(|m| m.as_str()) == Some("initialize")
        && let Some(name) = msg
            .pointer("/params/clientInfo/name")
            .and_then(|v| v.as_str())
        {
            let label = ClientLabel::parse(name);
            if label != ClientLabel::Unknown
                && let Ok(mut g) = state.audit_client_label.write()
            {
                *g = label.as_str().to_string();
            }
        }

    // Stateless (2026-07-28) requests must mirror version/method/name in headers.
    if let Some(err) = stateless::check_http_request(&msg, |n| {
        headers.get(n).and_then(|v| v.to_str().ok())
    }) {
        return (StatusCode::BAD_REQUEST, Json(err)).into_response();
    }

    let mut shell = state.shell.lock().await;
    match shell.handle_message(msg, Some(&cap.scopes)).await {
        Some(resp) => {
            let status = if stateless::is_modern_protocol_error(&resp) {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::OK
            };
            (status, Json(resp)).into_response()
        }
        None => {
            // Notification — accepted, no body required.
            StatusCode::ACCEPTED.into_response()
        }
    }
}

/// Minimal SSE endpoint for remote clients that expect an event stream.
///
/// Emits periodic comments as keepalive. Tool-list-changed notifications
/// can be extended later; JSON-RPC traffic remains on POST /mcp.
async fn mcp_sse_handler(
    State(state): State<Arc<HttpMcpState>>,
    headers: HeaderMap,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, Response> {
    let cap = match auth_from_headers(&state.tokens, &headers) {
        Ok(c) => c,
        Err(status) => {
            return Err((
                status,
                [(header::WWW_AUTHENTICATE, "Bearer")],
                "unauthorized",
            )
                .into_response());
        }
    };
    set_audit_label(&state, &cap);

    let stream = stream::unfold(0u64, |n| async move {
        tokio::time::sleep(Duration::from_secs(15)).await;
        let ev = Event::default().comment(format!("keepalive {n}"));
        Some((Ok::<_, Infallible>(ev), n + 1))
    });

    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

/// Convenience: parse listen, build config, serve.
pub async fn run_listen(
    listen: &str,
    allow_unauthenticated: bool,
    dangerously_bind_public: bool,
    state: Arc<HttpMcpState>,
) -> Result<(), String> {
    let (host, port) = HttpMcpConfig::parse_listen(listen)?;
    let config = HttpMcpConfig {
        host,
        port,
        allow_unauthenticated,
        dangerously_bind_public,
    };
    serve_mcp_http(config, state).await
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::composite::CompositeToolProvider;
    use crate::mcp::middleware::AuditLog;
    use crate::mcp::provider::{CallToolResult, ToolError, ToolProvider};
    use crate::mcp::{ToolDefinition, server::McpServerShell};
    use async_trait::async_trait;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    struct EchoProvider;

    #[async_trait]
    impl ToolProvider for EchoProvider {
        fn namespace(&self) -> &str {
            ""
        }
        fn list_tools(&self) -> Vec<ToolDefinition> {
            vec![
                ToolDefinition {
                    name: "read_file".into(),
                    description: "r".into(),
                    input_schema: serde_json::json!({"type": "object"}),
                },
                ToolDefinition {
                    name: "agent_spawn".into(),
                    description: "s".into(),
                    input_schema: serde_json::json!({"type": "object"}),
                },
            ]
        }
        async fn call_tool(
            &self,
            name: &str,
            _args: Value,
        ) -> Result<CallToolResult, ToolError> {
            Ok(CallToolResult::text(format!("ok:{name}")))
        }
    }

    fn test_state(auth: bool, token: Option<&str>) -> Arc<HttpMcpState> {
        let mut composite = CompositeToolProvider::new();
        composite.register(Box::new(EchoProvider));
        let mut shell = McpServerShell::new(composite);
        let audit = AuditLog::new();
        let label = audit.label_handle();
        shell.add_middleware(Box::new(audit));

        let tokens = Arc::new(SessionTokenStore::new(auth));
        if let Some(t) = token {
            tokens.add_static_token(t);
        }
        Arc::new(HttpMcpState::new(shell, tokens, label))
    }

    #[test]
    fn parse_listen_ipv4() {
        let (h, p) = HttpMcpConfig::parse_listen("127.0.0.1:8742").unwrap();
        assert_eq!(h, "127.0.0.1");
        assert_eq!(p, 8742);
    }

    #[test]
    fn parse_listen_ipv6() {
        let (h, p) = HttpMcpConfig::parse_listen("[::1]:9000").unwrap();
        assert_eq!(h, "::1");
        assert_eq!(p, 9000);
    }

    #[tokio::test]
    async fn rejects_missing_auth() {
        let state = test_state(true, Some("sekret"));
        let app = mcp_http_router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn accepts_bearer_and_initialize() {
        let state = test_state(true, Some("sekret"));
        let app = mcp_http_router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer sekret")
                    .header("x-mcp-client", "grok")
                    .body(Body::from(
                        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"grok","version":"1"}}}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["id"], 1);
        assert!(v["result"]["serverInfo"]["name"].is_string());
    }

    #[tokio::test]
    async fn health_is_public() {
        let state = test_state(true, Some("sekret"));
        let app = mcp_http_router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn session_scope_filters_tools() {
        use crate::mcp::session_cap::{ClientLabel, SessionScopes};

        let state = test_state(true, None);
        let scopes = SessionScopes {
            tools: Some(vec!["read_file".into()]),
            workspace_roots: None,
            agent_spawn: false,
        };
        let tok = state
            .tokens
            .issue(scopes, ClientLabel::Grok, None)
            .unwrap();

        // initialize
        let app = mcp_http_router(Arc::clone(&state));
        let _ = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header("authorization", format!("Bearer {tok}"))
                    .body(Body::from(
                        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        let app = mcp_http_router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header("authorization", format!("Bearer {tok}"))
                    .body(Body::from(
                        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        let tools = v["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools
            .iter()
            .filter_map(|t| t["name"].as_str())
            .collect();
        assert!(names.contains(&"read_file"));
        assert!(!names.contains(&"agent_spawn"));
    }

    async fn post(
        state: &Arc<HttpMcpState>,
        headers: &[(&str, &str)],
        body: &str,
    ) -> (StatusCode, Value) {
        let mut b = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("authorization", "Bearer sekret");
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        let resp = mcp_http_router(Arc::clone(state))
            .oneshot(b.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let v = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, v)
    }

    const MODERN_LIST: &str = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28"}}}"#;
    const MODERN_HDRS: [(&str, &str); 2] =
        [("mcp-protocol-version", "2026-07-28"), ("mcp-method", "tools/list")];

    #[tokio::test]
    async fn stateless_http_needs_no_initialize() {
        let state = test_state(true, Some("sekret"));
        let (status, v) = post(&state, &MODERN_HDRS, MODERN_LIST).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(v["result"]["resultType"], "complete");
        assert!(v["result"]["tools"].as_array().unwrap().len() >= 2);
        assert!(!state.shell.lock().await.is_initialized());
    }

    #[tokio::test]
    async fn stateless_http_discover_and_legacy_coexist() {
        let state = test_state(true, Some("sekret"));
        let discover = r#"{"jsonrpc":"2.0","id":1,"method":"server/discover","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28"}}}"#;
        let hdrs = [("mcp-protocol-version", "2026-07-28"), ("mcp-method", "server/discover")];
        let (status, v) = post(&state, &hdrs, discover).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(v["result"]["supportedVersions"][0], "2026-07-28");

        // A legacy client on the same listener still handshakes and lists.
        let init = r#"{"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#;
        let (status, v) = post(&state, &[], init).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(v["result"]["protocolVersion"], "2025-06-18");
        let legacy = r#"{"jsonrpc":"2.0","id":3,"method":"tools/list","params":{}}"#;
        let (_, v) = post(&state, &[], legacy).await;
        assert!(v["result"].get("resultType").is_none());

        // And the stateless client is unaffected by the legacy handshake.
        let (status, v) = post(&state, &MODERN_HDRS, MODERN_LIST).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(v["result"]["resultType"], "complete");
    }

    #[tokio::test]
    async fn stateless_http_refuses_header_mismatch_and_missing() {
        let state = test_state(true, Some("sekret"));
        let cases: [&[(&str, &str)]; 3] = [
            &[],
            &[("mcp-protocol-version", "2026-07-28"), ("mcp-method", "tools/call")],
            &[("mcp-protocol-version", "2025-06-18"), ("mcp-method", "tools/list")],
        ];
        for hdrs in cases {
            let (status, v) = post(&state, hdrs, MODERN_LIST).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert_eq!(v["error"]["code"], stateless::HEADER_MISMATCH);
        }
        // tools/call requires a matching Mcp-Name.
        let call = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"read_file","_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28"}}}"#;
        let base = [("mcp-protocol-version", "2026-07-28"), ("mcp-method", "tools/call")];
        let (status, _) = post(&state, &base, call).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let mut ok = base.to_vec();
        ok.push(("mcp-name", "wrong"));
        let (status, _) = post(&state, &ok, call).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        ok.pop();
        ok.push(("mcp-name", "read_file"));
        let (status, v) = post(&state, &ok, call).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(v["result"]["resultType"], "complete");
    }

    #[tokio::test]
    async fn stateless_http_unsupported_version_is_400() {
        let state = test_state(true, Some("sekret"));
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"1900-01-01"}}}"#;
        let hdrs = [("mcp-protocol-version", "1900-01-01"), ("mcp-method", "tools/list")];
        let (status, v) = post(&state, &hdrs, body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"]["code"], stateless::UNSUPPORTED_PROTOCOL_VERSION);
        assert_eq!(v["error"]["data"]["supported"][0], "2026-07-28");
    }
}
