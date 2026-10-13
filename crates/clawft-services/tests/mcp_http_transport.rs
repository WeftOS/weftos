//! WEFT-500: MCP HTTP transport against a real local HTTP server.
//!
//! Unit tests only exercise [`MockTransport`] (in-process, no sockets).
//! These integration tests stand up a real listening server so
//! [`HttpTransport`] hits the full reqwest path: JSON body encoding,
//! Content-Type, status handling, response JSON parse, and factory
//! wiring.
//!
//! Coverage:
//! - axum `TcpListener` MCP endpoint: request/response round-trip
//! - axum: MCP session handshake (`initialize` + `notifications/initialized`)
//! - axum: `tools/list` via [`McpClient`]
//! - axum: large / content-length response body
//! - wiremock: HTTP 4xx/5xx surface as transport errors
//! - wiremock: malformed JSON body surfaces parse error
//! - wiremock: notifications are fire-and-forget (non-2xx still Ok)

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Router, body::Bytes};
use clawft_services::mcp::transport::{
    DefaultTransportFactory, HttpTransport, McpTransport, McpTransportFactory, TransportFactoryConfig,
    TransportSpec,
};
use clawft_services::mcp::types::{JsonRpcNotification, JsonRpcRequest, JsonRpcResponse};
use clawft_services::mcp::{DurabilityConfig, McpClient, McpSession};
use tokio::net::TcpListener;
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ── axum local MCP stub ────────────────────────────────────────────────

#[derive(Clone, Default)]
struct AxumMcpState {
    notifications: Arc<AtomicUsize>,
}

async fn axum_mcp_handler(
    State(state): State<AxumMcpState>,
    body: Bytes,
) -> impl IntoResponse {
    // Notifications have no `id`; treat them as fire-and-forget.
    if let Ok(notif) = serde_json::from_slice::<JsonRpcNotification>(&body)
        && notif.jsonrpc == "2.0"
        && serde_json::from_slice::<JsonRpcRequest>(&body).is_err()
    {
        state.notifications.fetch_add(1, Ordering::SeqCst);
        return (StatusCode::ACCEPTED, Json(serde_json::json!({}))).into_response();
    }

    let req: JsonRpcRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 0,
                    "error": { "code": -32700, "message": format!("parse error: {e}") }
                })),
            )
                .into_response();
        }
    };

    let result = match req.method.as_str() {
        "initialize" => serde_json::json!({
            "protocolVersion": clawft_services::mcp::MCP_PROTOCOL_VERSION,
            "capabilities": { "tools": { "listChanged": true } },
            "serverInfo": { "name": "weft-500-axum-mcp", "version": "0.1.0" }
        }),
        "tools/list" => serde_json::json!({
            "tools": [{
                "name": "echo",
                "description": "Echoes input",
                "inputSchema": {
                    "type": "object",
                    "properties": { "text": { "type": "string" } }
                }
            }]
        }),
        "tools/call" => {
            let name = req.params.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let args = req.params.get("arguments").cloned().unwrap_or_default();
            serde_json::json!({
                "content": [{
                    "type": "text",
                    "text": format!("called {name} with {args}")
                }]
            })
        }
        "ping" => serde_json::json!({}),
        "bulk/payload" => {
            // Large body to exercise content-length / full-buffer parse.
            let blob = "x".repeat(64 * 1024);
            serde_json::json!({ "blob": blob, "len": blob.len() })
        }
        other => {
            let resp = JsonRpcResponse {
                jsonrpc: "2.0".into(),
                id: req.id,
                result: None,
                error: Some(clawft_services::mcp::types::JsonRpcError {
                    code: -32601,
                    message: format!("method not found: {other}"),
                    data: None,
                }),
            };
            return (StatusCode::OK, Json(resp)).into_response();
        }
    };

    let resp = JsonRpcResponse {
        jsonrpc: "2.0".into(),
        id: req.id,
        result: Some(result),
        error: None,
    };
    (StatusCode::OK, Json(resp)).into_response()
}

/// Bind an axum MCP stub on an ephemeral localhost port.
async fn spawn_axum_mcp() -> (SocketAddr, AxumMcpState, tokio::task::JoinHandle<()>) {
    let state = AxumMcpState::default();
    let app = Router::new()
        .route("/mcp", post(axum_mcp_handler))
        .with_state(state.clone());

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral localhost");
    let addr = listener.local_addr().expect("local_addr");
    let handle = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("axum serve");
    });
    // Brief yield so the accept loop is ready.
    tokio::task::yield_now().await;
    (addr, state, handle)
}

fn endpoint(addr: SocketAddr) -> String {
    format!("http://{addr}/mcp")
}

// ── axum: transport round-trip ─────────────────────────────────────────

#[tokio::test]
async fn axum_http_transport_send_request_round_trip() {
    let (addr, _state, handle) = spawn_axum_mcp().await;
    let transport = HttpTransport::new(endpoint(addr));

    let req = JsonRpcRequest::new(7, "tools/list", serde_json::json!({}));
    let resp = transport
        .send_request(req)
        .await
        .expect("HTTP request over real socket");

    assert_eq!(resp.id, 7);
    assert!(resp.error.is_none());
    let tools = resp
        .result
        .as_ref()
        .and_then(|r| r.get("tools"))
        .and_then(|t| t.as_array())
        .expect("tools array");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["name"], "echo");

    handle.abort();
}

#[tokio::test]
async fn axum_http_transport_notification_counts() {
    let (addr, state, handle) = spawn_axum_mcp().await;
    let transport = HttpTransport::new(endpoint(addr));

    transport
        .send_notification("notifications/initialized", serde_json::json!({}))
        .await
        .expect("notification");
    transport
        .send_notification(
            "notifications/progress",
            serde_json::json!({ "token": "t1", "progress": 0.5 }),
        )
        .await
        .expect("progress notification");

    // Give the server a tick to process.
    tokio::task::yield_now().await;
    assert_eq!(
        state.notifications.load(Ordering::SeqCst),
        2,
        "both notifications should hit the real HTTP handler"
    );

    handle.abort();
}

#[tokio::test]
async fn axum_mcp_client_list_and_call_tools() {
    let (addr, _state, handle) = spawn_axum_mcp().await;
    let transport = HttpTransport::new(endpoint(addr));
    let client = McpClient::new(Box::new(transport));

    let tools = client.list_tools().await.expect("list_tools over HTTP");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "echo");

    let result = client
        .call_tool("echo", serde_json::json!({ "text": "hi" }))
        .await
        .expect("call_tool over HTTP");
    let text = result["content"][0]["text"].as_str().unwrap_or_default();
    assert!(text.contains("echo"), "got: {text}");
    assert!(text.contains("hi"), "got: {text}");

    handle.abort();
}

#[tokio::test]
async fn axum_mcp_session_handshake_over_http() {
    let (addr, state, handle) = spawn_axum_mcp().await;
    let transport = HttpTransport::new(endpoint(addr));

    let session = McpSession::connect_with_options(
        Box::new(transport),
        None,
        DurabilityConfig::disabled(),
    )
    .await
    .expect("handshake over real HTTP");

    assert_eq!(session.server_info.name, "weft-500-axum-mcp");
    assert_eq!(
        session.protocol_version,
        clawft_services::mcp::MCP_PROTOCOL_VERSION
    );
    assert!(session.peer_tools_list_changed());

    // `notifications/initialized` is part of the handshake.
    assert!(
        state.notifications.load(Ordering::SeqCst) >= 1,
        "handshake must POST notifications/initialized"
    );

    let tools = session.list_tools().await.expect("list after handshake");
    assert_eq!(tools[0].name, "echo");

    session.shutdown().await.expect("shutdown");
    handle.abort();
}

#[tokio::test]
async fn axum_factory_create_http_transport_and_ping() {
    let (addr, _state, handle) = spawn_axum_mcp().await;
    let factory = DefaultTransportFactory::new(TransportFactoryConfig::lenient());
    let spec = TransportSpec::Http {
        url: endpoint(addr),
    };
    let transport = factory
        .create(spec)
        .await
        .expect("factory create Http transport");

    let client = McpClient::new(transport);
    client.ping().await.expect("ping over factory transport");

    handle.abort();
}

#[tokio::test]
async fn axum_large_response_body_parses() {
    let (addr, _state, handle) = spawn_axum_mcp().await;
    let transport = HttpTransport::new(endpoint(addr));

    let req = JsonRpcRequest::new(99, "bulk/payload", serde_json::json!({}));
    let resp = transport
        .send_request(req)
        .await
        .expect("large body over HTTP");

    assert_eq!(resp.id, 99);
    let len = resp
        .result
        .as_ref()
        .and_then(|r| r.get("len"))
        .and_then(|v| v.as_u64())
        .expect("len");
    assert_eq!(len, 64 * 1024);

    handle.abort();
}

// ── wiremock: HTTP status / parse edge cases ───────────────────────────

#[tokio::test]
async fn wiremock_http_500_is_transport_error() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/mcp"))
        .respond_with(
            ResponseTemplate::new(500)
                .set_body_string("internal boom")
                .insert_header("content-type", "text/plain"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let transport = HttpTransport::new(format!("{}/mcp", server.uri()));
    let req = JsonRpcRequest::new(1, "tools/list", serde_json::json!({}));
    let err = transport
        .send_request(req)
        .await
        .expect_err("HTTP 500 must fail");
    let msg = err.to_string();
    assert!(msg.contains("HTTP 500") || msg.contains("500"), "got: {msg}");
    assert!(msg.contains("internal boom"), "got: {msg}");
}

#[tokio::test]
async fn wiremock_http_404_is_transport_error() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/missing"))
        .respond_with(ResponseTemplate::new(404).set_body_string("nope"))
        .expect(1)
        .mount(&server)
        .await;

    let transport = HttpTransport::new(format!("{}/missing", server.uri()));
    let req = JsonRpcRequest::new(1, "tools/list", serde_json::json!({}));
    let err = transport.send_request(req).await.expect_err("HTTP 404");
    assert!(
        err.to_string().contains("404"),
        "got: {}",
        err
    );
}

#[tokio::test]
async fn wiremock_malformed_json_is_parse_error() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/mcp"))
        .and(header("content-type", "application/json"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("this is not json {{{")
                .insert_header("content-type", "application/json"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let transport = HttpTransport::new(format!("{}/mcp", server.uri()));
    let req = JsonRpcRequest::new(1, "tools/list", serde_json::json!({}));
    let err = transport
        .send_request(req)
        .await
        .expect_err("malformed JSON must fail");
    let msg = err.to_string().to_lowercase();
    assert!(
        msg.contains("parse") || msg.contains("json") || msg.contains("failed"),
        "got: {msg}"
    );
}

#[tokio::test]
async fn wiremock_successful_jsonrpc_with_content_type() {
    let server = MockServer::start().await;

    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 42,
        "result": { "tools": [] }
    });

    Mock::given(method("POST"))
        .and(path("/mcp"))
        .and(header("content-type", "application/json"))
        .and(body_partial_json(serde_json::json!({
            "jsonrpc": "2.0",
            "method": "tools/list"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(&body))
        .expect(1)
        .mount(&server)
        .await;

    let transport = HttpTransport::new(format!("{}/mcp", server.uri()));
    let req = JsonRpcRequest::new(42, "tools/list", serde_json::json!({}));
    let resp = transport.send_request(req).await.expect("ok");
    assert_eq!(resp.id, 42);
    assert!(resp.result.is_some());
}

#[tokio::test]
async fn wiremock_notification_non_success_still_ok() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/mcp"))
        .respond_with(ResponseTemplate::new(503).set_body_string("unavailable"))
        .expect(1)
        .mount(&server)
        .await;

    let transport = HttpTransport::new(format!("{}/mcp", server.uri()));
    // Notifications are fire-and-forget: non-2xx is logged, not an error.
    transport
        .send_notification("notifications/initialized", serde_json::json!({}))
        .await
        .expect("notification must not fail on non-2xx");
}

#[tokio::test]
async fn wiremock_connection_refused_is_transport_error() {
    // Nothing listening — pure reqwest connect failure path.
    let transport = HttpTransport::new("http://127.0.0.1:1/mcp".into());
    let req = JsonRpcRequest::new(1, "ping", serde_json::json!({}));
    let err = transport
        .send_request(req)
        .await
        .expect_err("connection refused");
    let msg = err.to_string().to_lowercase();
    assert!(
        msg.contains("http request failed") || msg.contains("connect") || msg.contains("refused"),
        "got: {msg}"
    );
}

#[tokio::test]
async fn factory_rejects_non_localhost_http_when_strict() {
    let factory = DefaultTransportFactory::new(TransportFactoryConfig::strict());
    let spec = TransportSpec::Http {
        url: "http://example.com/mcp".into(),
    };
    assert!(factory.validate(&spec).is_err());
}

#[tokio::test]
async fn factory_allows_localhost_http_when_lenient() {
    let factory = DefaultTransportFactory::new(TransportFactoryConfig::lenient());
    // Port that may or may not be up — validate only, no create.
    let spec = TransportSpec::Http {
        url: "http://127.0.0.1:9/mcp".into(),
    };
    assert!(factory.validate(&spec).is_ok());
    // create still succeeds (HttpTransport::new is lazy); send would fail.
    let t = factory.create(spec).await;
    assert!(t.is_ok());
    let _unused_env: HashMap<String, String> = HashMap::new();
}

// ── bearer auth and event-stream answers ───────────────────────────────

async fn bearer_sse_handler(headers: axum::http::HeaderMap, body: Bytes) -> axum::response::Response {
    let authed = headers.get("authorization").and_then(|v| v.to_str().ok()) == Some("Bearer s3cret-token");
    if !authed {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let req: JsonRpcRequest = serde_json::from_slice(&body).unwrap();
    let event = format!(
        "event: message\ndata: {}\n\n",
        serde_json::json!({"jsonrpc": "2.0", "id": req.id, "result": {"ok": true}})
    );
    ([(axum::http::header::CONTENT_TYPE, "text/event-stream"), (axum::http::HeaderName::from_static("mcp-session-id"), "sess-1")], event).into_response()
}

#[tokio::test]
async fn bearer_header_is_sent_and_event_stream_answers_are_parsed() {
    let app = Router::new().route("/", post(bearer_sse_handler));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let url = format!("http://{addr}/");
    let timeout = std::time::Duration::from_secs(5);

    let ok = HttpTransport::with_bearer(url.clone(), Some("s3cret-token"), timeout).unwrap();
    let resp = ok.send_request(JsonRpcRequest::new(7, "ping", serde_json::json!({}))).await.unwrap();
    assert_eq!(resp.id, 7);
    assert_eq!(resp.result.unwrap()["ok"], true);

    let anon = HttpTransport::with_bearer(url.clone(), None, timeout).unwrap();
    let err = anon.send_request(JsonRpcRequest::new(8, "ping", serde_json::json!({}))).await.unwrap_err();
    assert!(err.to_string().contains("401"), "{err}");

    assert!(HttpTransport::with_bearer(url, Some("bad\ntoken"), timeout).is_err());
}
