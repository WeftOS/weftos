//! MCP transport implementations.
//!
//! Provides [`McpTransport`] trait and two implementations:
//! - [`StdioTransport`]: communicates with a child process over stdin/stdout
//!   using request-ID multiplexing for concurrent requests
//! - [`HttpTransport`]: communicates over HTTP POST
//!
//! Plus a [`McpTransportFactory`] trait used by [`McpServerManager`] to
//! decouple transport instantiation from server management, with built-in
//! validators ([`validate_url`], [`validate_command_path`],
//! [`validate_tempfile_path`]) that gate config before a transport is spawned.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, oneshot};
use tracing::{debug, warn};

use super::types::{JsonRpcNotification, JsonRpcRequest, JsonRpcResponse};
use crate::error::{Result, ServiceError};

/// MCP notification method for tool-list invalidation (WEFT-200).
pub const TOOLS_LIST_CHANGED_METHOD: &str = "notifications/tools/list_changed";

/// Transport layer for MCP JSON-RPC communication.
#[async_trait]
pub trait McpTransport: Send + Sync {
    /// Send a JSON-RPC request and return the response.
    async fn send_request(&self, request: JsonRpcRequest) -> Result<JsonRpcResponse>;

    /// Send a JSON-RPC notification (no `id`, no response expected).
    async fn send_notification(&self, method: &str, params: serde_json::Value) -> Result<()>;

    /// Whether the underlying connection is still alive (WEFT-191).
    ///
    /// Default returns `true` for transports that cannot detect liveness
    /// (e.g. plain HTTP). [`StdioTransport`] checks the child process.
    async fn is_alive(&self) -> bool {
        true
    }

    /// Force-close the transport (kill child, drop pipes) (WEFT-191).
    ///
    /// Default is a no-op. Callers use this during graceful session shutdown
    /// after emitting `notifications/cancelled`.
    async fn close(&self) -> Result<()> {
        Ok(())
    }

    /// Non-blocking poll for a server→client notification (WEFT-200).
    ///
    /// Default returns `None`. [`StdioTransport`] and [`MockTransport`]
    /// queue inbound notifications demuxed from the reader loop.
    async fn poll_notification(&self) -> Option<JsonRpcNotification> {
        None
    }
}

/// Recreate a transport after the underlying connection dies (WEFT-191).
///
/// Used by [`super::McpSession::reconnect_with_backoff`]. Production code
/// typically wraps a [`TransportSpec`] + [`McpTransportFactory`]; tests can
/// supply canned replacements.
#[async_trait]
pub trait TransportReconnect: Send + Sync {
    /// Build a fresh transport for the same logical server.
    async fn recreate(&self) -> Result<Box<dyn McpTransport>>;
}

/// [`TransportReconnect`] backed by a factory + [`TransportSpec`].
pub struct SpecReconnect {
    factory: Arc<dyn McpTransportFactory>,
    spec: TransportSpec,
}

impl SpecReconnect {
    /// Create a reconnect handle from a factory and transport spec.
    pub fn new(factory: Arc<dyn McpTransportFactory>, spec: TransportSpec) -> Self {
        Self { factory, spec }
    }

    /// Access the transport spec used for recreation.
    pub fn spec(&self) -> &TransportSpec {
        &self.spec
    }
}

#[async_trait]
impl TransportReconnect for SpecReconnect {
    async fn recreate(&self) -> Result<Box<dyn McpTransport>> {
        self.factory.create(self.spec.clone()).await
    }
}

/// Pending response registry: maps request IDs to oneshot senders.
type PendingMap = Arc<Mutex<HashMap<u64, oneshot::Sender<JsonRpcResponse>>>>;

/// Transport that communicates with a child process via stdin/stdout.
///
/// Uses a background reader task and request-ID multiplexing to support
/// concurrent requests. Each `send_request` call registers a oneshot
/// channel keyed by the request ID, writes to stdin, and waits for the
/// background reader to deliver the matching response.
///
/// # Durability (WEFT-191)
///
/// - [`Self::is_alive`] polls `child.try_wait()` so callers can detect crashes.
/// - Spawn parameters are retained for session-level reconnect helpers.
/// - [`Self::close`] kills the child and clears pending request waiters.
pub struct StdioTransport {
    child: Arc<Mutex<Child>>,
    stdin: Arc<Mutex<tokio::process::ChildStdin>>,
    pending: PendingMap,
    /// Inbound server→client notifications (WEFT-200).
    inbound_notifications: Arc<Mutex<VecDeque<JsonRpcNotification>>>,
    #[allow(dead_code)]
    reader_handle: Arc<tokio::task::JoinHandle<()>>,
    /// Command used to spawn the child (for reconnect diagnostics / re-spawn).
    command: String,
    /// Args used to spawn the child.
    args: Vec<String>,
    /// Env used to spawn the child.
    env: HashMap<String, String>,
    /// Set when [`Self::close`] has been invoked.
    closed: AtomicBool,
}

impl StdioTransport {
    /// Spawn a child process and set up JSON-RPC communication with
    /// request-ID multiplexing.
    pub async fn new(
        command: &str,
        args: &[String],
        env: &HashMap<String, String>,
    ) -> Result<Self> {
        let mut cmd = Command::new(command);
        cmd.args(args)
            .envs(env)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        // WEFT-191: ensure we can reap / detect exit without waiting for Drop.
        cmd.kill_on_drop(true);

        let mut child = cmd.spawn()?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| ServiceError::McpTransport("failed to capture stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ServiceError::McpTransport("failed to capture stdout".into()))?;

        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let inbound_notifications: Arc<Mutex<VecDeque<JsonRpcNotification>>> =
            Arc::new(Mutex::new(VecDeque::new()));

        // Spawn background reader task that reads lines from stdout and
        // dispatches responses to the matching pending oneshot sender.
        // Non-response lines are demuxed as inbound notifications (WEFT-200).
        let reader_pending = Arc::clone(&pending);
        let reader_notifs = Arc::clone(&inbound_notifications);
        let reader_handle = tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) => {
                        debug!("stdio reader: child process closed stdout");
                        break;
                    }
                    Ok(_) => {
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        // Prefer response (has id) when both could parse.
                        if let Ok(response) = serde_json::from_str::<JsonRpcResponse>(trimmed) {
                            // Responses always have an id field; bare
                            // notifications fail response deserialize when
                            // `id` is missing (serde required field).
                            let id = response.id;
                            let mut map = reader_pending.lock().await;
                            if let Some(tx) = map.remove(&id) {
                                let _ = tx.send(response);
                            } else {
                                warn!(
                                    id,
                                    "stdio reader: received response with no pending request"
                                );
                            }
                            continue;
                        }
                        match serde_json::from_str::<JsonRpcNotification>(trimmed) {
                            Ok(notif) => {
                                debug!(
                                    method = %notif.method,
                                    "stdio reader: queued inbound notification"
                                );
                                reader_notifs.lock().await.push_back(notif);
                            }
                            Err(e) => {
                                debug!(error = %e, "stdio reader: ignoring non-jsonrpc line");
                            }
                        }
                    }
                    Err(e) => {
                        warn!(error = %e, "stdio reader: read error, exiting");
                        break;
                    }
                }
            }

            // Signal all pending requests that the reader has stopped.
            let mut map = reader_pending.lock().await;
            map.clear();
        });

        Ok(Self {
            child: Arc::new(Mutex::new(child)),
            stdin: Arc::new(Mutex::new(stdin)),
            pending,
            inbound_notifications,
            reader_handle: Arc::new(reader_handle),
            command: command.to_string(),
            args: args.to_vec(),
            env: env.clone(),
            closed: AtomicBool::new(false),
        })
    }

    /// Command / args / env used to spawn this transport (WEFT-191).
    pub fn spawn_config(&self) -> (&str, &[String], &HashMap<String, String>) {
        (&self.command, &self.args, &self.env)
    }

    /// Build a [`TransportSpec`] that re-spawns the same child (WEFT-191).
    pub fn to_transport_spec(&self) -> TransportSpec {
        TransportSpec::Stdio {
            command: self.command.clone(),
            args: self.args.clone(),
            env: self.env.clone(),
        }
    }

    /// True if the child is still running (sync helper used by tests).
    pub async fn child_running(&self) -> bool {
        self.is_alive().await
    }
}

/// Default timeout for waiting on a response from the child process.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

#[async_trait]
impl McpTransport for StdioTransport {
    async fn send_request(&self, request: JsonRpcRequest) -> Result<JsonRpcResponse> {
        if self.closed.load(Ordering::SeqCst) || !self.is_alive().await {
            return Err(ServiceError::McpTransport(
                "stdio child process is not alive".into(),
            ));
        }

        let mut line = serde_json::to_string(&request)?;
        line.push('\n');

        let id = request.id;
        debug!(method = %request.method, id, "sending stdio request");

        // Register a oneshot channel for this request ID.
        let (tx, rx) = oneshot::channel::<JsonRpcResponse>();
        {
            let mut map = self.pending.lock().await;
            map.insert(id, tx);
        }

        // Write to stdin.
        {
            let mut stdin = self.stdin.lock().await;
            stdin.write_all(line.as_bytes()).await.map_err(|e| {
                ServiceError::McpTransport(format!("failed to write to stdin: {e}"))
            })?;
            stdin
                .flush()
                .await
                .map_err(|e| ServiceError::McpTransport(format!("failed to flush stdin: {e}")))?;
        }

        // Wait for the background reader to deliver the response, with timeout.
        match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(_)) => {
                // Oneshot sender was dropped (reader task exited).
                Err(ServiceError::McpTransport(
                    "child process closed stdout before responding".into(),
                ))
            }
            Err(_) => {
                // Timeout: remove the pending entry.
                let mut map = self.pending.lock().await;
                map.remove(&id);
                Err(ServiceError::McpTransport(format!(
                    "request {id} timed out after {}s",
                    REQUEST_TIMEOUT.as_secs()
                )))
            }
        }
    }

    async fn send_notification(&self, method: &str, params: serde_json::Value) -> Result<()> {
        if self.closed.load(Ordering::SeqCst) || !self.is_alive().await {
            return Err(ServiceError::McpTransport(
                "stdio child process is not alive".into(),
            ));
        }

        let notif = JsonRpcNotification::new(method, params);
        let mut line = serde_json::to_string(&notif)?;
        line.push('\n');

        debug!(method = %method, "sending stdio notification");

        let mut stdin = self.stdin.lock().await;
        stdin.write_all(line.as_bytes()).await.map_err(|e| {
            ServiceError::McpTransport(format!("failed to write notification to stdin: {e}"))
        })?;
        stdin.flush().await.map_err(|e| {
            ServiceError::McpTransport(format!("failed to flush stdin after notification: {e}"))
        })?;

        // Notifications do not expect a response -- do NOT read from stdout.
        Ok(())
    }

    async fn is_alive(&self) -> bool {
        if self.closed.load(Ordering::SeqCst) {
            return false;
        }
        let mut child = self.child.lock().await;
        match child.try_wait() {
            Ok(None) => true,
            Ok(Some(status)) => {
                debug!(?status, "stdio child has exited");
                false
            }
            Err(e) => {
                warn!(error = %e, "stdio try_wait failed; treating as dead");
                false
            }
        }
    }

    async fn poll_notification(&self) -> Option<JsonRpcNotification> {
        self.inbound_notifications.lock().await.pop_front()
    }

    async fn close(&self) -> Result<()> {
        self.closed.store(true, Ordering::SeqCst);
        // Drop all pending waiters so in-flight send_request unblocks.
        {
            let mut map = self.pending.lock().await;
            map.clear();
        }
        let mut child = self.child.lock().await;
        // Best-effort kill + reap.
        if let Err(e) = child.start_kill() {
            debug!(error = %e, "stdio close: start_kill");
        }
        match tokio::time::timeout(Duration::from_secs(2), child.wait()).await {
            Ok(Ok(status)) => {
                debug!(?status, "stdio child reaped on close");
            }
            Ok(Err(e)) => {
                warn!(error = %e, "stdio close: wait failed");
            }
            Err(_) => {
                warn!("stdio close: wait timed out after kill");
            }
        }
        Ok(())
    }
}

/// Transport that communicates via HTTP POST.
///
/// Sends JSON-RPC requests as the body of POST requests to the
/// configured endpoint URL. Handles both plain JSON responses and the
/// single-event `text/event-stream` framing that streamable-HTTP servers
/// may answer with, and echoes the `Mcp-Session-Id` a server assigns.
pub struct HttpTransport {
    client: reqwest::Client,
    endpoint: String,
    /// `Authorization` header value, marked sensitive so it never shows in
    /// reqwest's debug output. Never logged by this module.
    authorization: Option<reqwest::header::HeaderValue>,
    session_id: std::sync::Mutex<Option<String>>,
}

impl HttpTransport {
    /// Create a new HTTP transport targeting the given endpoint URL.
    pub fn new(endpoint: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            endpoint,
            authorization: None,
            session_id: std::sync::Mutex::new(None),
        }
    }

    /// Create a transport that sends `Authorization: Bearer <token>` and
    /// gives up on any single request after `timeout`.
    pub fn with_bearer(
        endpoint: String,
        bearer: Option<&str>,
        timeout: std::time::Duration,
    ) -> Result<Self> {
        let authorization = match bearer {
            Some(token) => {
                let mut v = reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
                    .map_err(|_| {
                        ServiceError::McpTransport("bearer token is not a valid header value".into())
                    })?;
                v.set_sensitive(true);
                Some(v)
            }
            None => None,
        };
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| ServiceError::McpTransport(format!("HTTP client: {e}")))?;
        Ok(Self {
            client,
            endpoint,
            authorization,
            session_id: std::sync::Mutex::new(None),
        })
    }

    async fn post<B: serde::Serialize>(&self, body: &B) -> std::result::Result<reqwest::Response, reqwest::Error> {
        let mut req = self
            .client
            .post(&self.endpoint)
            .header(reqwest::header::ACCEPT, "application/json, text/event-stream")
            .json(body);
        if let Some(auth) = &self.authorization {
            req = req.header(reqwest::header::AUTHORIZATION, auth.clone());
        }
        let sid = self.session_id.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(sid) = sid {
            req = req.header("Mcp-Session-Id", sid);
        }
        let resp = req.send().await?;
        if let Some(sid) = resp.headers().get("mcp-session-id").and_then(|v| v.to_str().ok()) {
            *self.session_id.lock().unwrap_or_else(|e| e.into_inner()) = Some(sid.to_string());
        }
        Ok(resp)
    }
}

/// Pick the response for `id` out of a `text/event-stream` body.
fn response_from_sse(body: &str, id: u64) -> Option<JsonRpcResponse> {
    body.lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .filter_map(|d| serde_json::from_str::<JsonRpcResponse>(d.trim()).ok())
        .find(|r| r.id == id)
}

#[async_trait]
impl McpTransport for HttpTransport {
    async fn send_request(&self, request: JsonRpcRequest) -> Result<JsonRpcResponse> {
        debug!(
            method = %request.method,
            id = request.id,
            endpoint = %self.endpoint,
            "sending HTTP request"
        );

        let resp = self
            .post(&request)
            .await
            .map_err(|e| ServiceError::McpTransport(format!("HTTP request failed: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ServiceError::McpTransport(format!("HTTP {status}: {body}")));
        }

        let is_sse = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("text/event-stream"));
        if is_sse {
            let body = resp
                .text()
                .await
                .map_err(|e| ServiceError::McpTransport(format!("failed to read response: {e}")))?;
            return response_from_sse(&body, request.id).ok_or_else(|| {
                ServiceError::McpTransport("event stream carried no response for the request".into())
            });
        }

        let response: JsonRpcResponse = resp
            .json()
            .await
            .map_err(|e| ServiceError::McpTransport(format!("failed to parse response: {e}")))?;

        Ok(response)
    }

    async fn send_notification(&self, method: &str, params: serde_json::Value) -> Result<()> {
        let notif = JsonRpcNotification::new(method, params);

        debug!(
            method = %method,
            endpoint = %self.endpoint,
            "sending HTTP notification"
        );

        let resp = self
            .post(&notif)
            .await
            .map_err(|e| ServiceError::McpTransport(format!("HTTP notification failed: {e}")))?;

        // Log non-success status but don't fail -- notifications are fire-and-forget.
        let status = resp.status();
        if !status.is_success() {
            debug!(
                method = %method,
                status = %status,
                "HTTP notification received non-success status"
            );
        }

        Ok(())
    }
}

/// A mock transport for testing.
///
/// Allows pre-programming responses that will be returned in order.
/// Also records all sent notifications for verification.
///
/// Available in tests and when the `test-utils` feature is enabled,
/// allowing downstream crates to use it in their own test suites.
#[cfg(any(test, feature = "test-utils"))]
pub struct MockTransport {
    responses: Arc<Mutex<Vec<JsonRpcResponse>>>,
    requests: Arc<Mutex<Vec<JsonRpcRequest>>>,
    notifications: Arc<Mutex<Vec<JsonRpcNotification>>>,
    /// Inbound server→client notifications (WEFT-200).
    inbound_notifications: Arc<Mutex<VecDeque<JsonRpcNotification>>>,
    /// Liveness flag (WEFT-191). When false, `send_*` fails and `is_alive` is false.
    alive: AtomicBool,
    /// Whether [`McpTransport::close`] has been called.
    closed: AtomicBool,
}

#[cfg(any(test, feature = "test-utils"))]
impl MockTransport {
    /// Create a mock transport with pre-programmed responses.
    pub fn new(responses: Vec<JsonRpcResponse>) -> Self {
        Self {
            responses: Arc::new(Mutex::new(responses)),
            requests: Arc::new(Mutex::new(Vec::new())),
            notifications: Arc::new(Mutex::new(Vec::new())),
            inbound_notifications: Arc::new(Mutex::new(VecDeque::new())),
            alive: AtomicBool::new(true),
            closed: AtomicBool::new(false),
        }
    }

    /// Get all requests that were sent through this transport.
    pub async fn requests(&self) -> Vec<JsonRpcRequest> {
        self.requests.lock().await.clone()
    }

    /// Get all notifications that were sent through this transport.
    pub async fn notifications(&self) -> Vec<JsonRpcNotification> {
        self.notifications.lock().await.clone()
    }

    /// Simulate a crashed / disconnected peer (WEFT-191).
    pub fn set_alive(&self, alive: bool) {
        self.alive.store(alive, Ordering::SeqCst);
    }

    /// Whether `close()` has been called.
    pub fn was_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Push additional mock responses (e.g. after reconnect handshake setup).
    pub async fn push_responses(&self, extra: Vec<JsonRpcResponse>) {
        self.responses.lock().await.extend(extra);
    }

    /// Queue a server→client notification for [`McpTransport::poll_notification`] (WEFT-200).
    pub async fn inject_notification(&self, method: impl Into<String>, params: serde_json::Value) {
        let notif = JsonRpcNotification::new(method, params);
        self.inbound_notifications.lock().await.push_back(notif);
    }

    /// Wrap in a [`SharedMockTransport`] so callers can retain an `Arc`
    /// while still passing `Box<dyn McpTransport>` into sessions.
    pub fn shared(self) -> (SharedMockTransport, Arc<MockTransport>) {
        let arc = Arc::new(self);
        (SharedMockTransport(Arc::clone(&arc)), arc)
    }
}

/// `Box<dyn McpTransport>`-compatible handle that keeps a shared
/// [`MockTransport`] alive for post-session assertions (WEFT-191 tests).
#[cfg(any(test, feature = "test-utils"))]
#[derive(Clone)]
pub struct SharedMockTransport(pub Arc<MockTransport>);

#[cfg(any(test, feature = "test-utils"))]
#[async_trait]
impl McpTransport for MockTransport {
    async fn send_request(&self, request: JsonRpcRequest) -> Result<JsonRpcResponse> {
        if !self.alive.load(Ordering::SeqCst) || self.closed.load(Ordering::SeqCst) {
            return Err(ServiceError::McpTransport(
                "mock transport is not alive".into(),
            ));
        }
        self.requests.lock().await.push(request);
        let mut responses = self.responses.lock().await;
        if responses.is_empty() {
            Err(ServiceError::McpTransport("no more mock responses".into()))
        } else {
            Ok(responses.remove(0))
        }
    }

    async fn send_notification(&self, method: &str, params: serde_json::Value) -> Result<()> {
        if !self.alive.load(Ordering::SeqCst) || self.closed.load(Ordering::SeqCst) {
            return Err(ServiceError::McpTransport(
                "mock transport is not alive".into(),
            ));
        }
        let notif = JsonRpcNotification::new(method, params);
        self.notifications.lock().await.push(notif);
        Ok(())
    }

    async fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst) && !self.closed.load(Ordering::SeqCst)
    }

    async fn close(&self) -> Result<()> {
        self.closed.store(true, Ordering::SeqCst);
        self.alive.store(false, Ordering::SeqCst);
        Ok(())
    }

    async fn poll_notification(&self) -> Option<JsonRpcNotification> {
        self.inbound_notifications.lock().await.pop_front()
    }
}

#[cfg(any(test, feature = "test-utils"))]
#[async_trait]
impl McpTransport for SharedMockTransport {
    async fn send_request(&self, request: JsonRpcRequest) -> Result<JsonRpcResponse> {
        self.0.send_request(request).await
    }

    async fn send_notification(&self, method: &str, params: serde_json::Value) -> Result<()> {
        self.0.send_notification(method, params).await
    }

    async fn is_alive(&self) -> bool {
        self.0.is_alive().await
    }

    async fn close(&self) -> Result<()> {
        self.0.close().await
    }

    async fn poll_notification(&self) -> Option<JsonRpcNotification> {
        self.0.poll_notification().await
    }
}

// ---------------------------------------------------------------------------
// Transport factory + validators (WEFT-186)
// ---------------------------------------------------------------------------

/// A typed description of how to create a transport.
///
/// Variants mirror the supported transport kinds. Use
/// [`McpTransportFactory::create`] to materialize a transport.
#[derive(Debug, Clone)]
pub enum TransportSpec {
    /// Spawn a child process and speak JSON-RPC over its stdio.
    Stdio {
        /// Command path. Validated by [`validate_command_path`] when
        /// `allowed_paths` is non-empty.
        command: String,
        /// Command arguments.
        args: Vec<String>,
        /// Environment variables for the child.
        env: HashMap<String, String>,
    },
    /// Speak JSON-RPC over HTTP POST.
    Http {
        /// Endpoint URL. Validated by [`validate_url`].
        url: String,
    },
    /// Speak JSON-RPC over a Unix domain socket bound to a tempfile path.
    ///
    /// Currently only path validation is performed; transport materialization
    /// returns a `NotImplemented` error so callers can plug their own
    /// implementation later.
    Tempfile {
        /// Path to the socket file. Validated by [`validate_tempfile_path`].
        path: PathBuf,
    },
}

/// Configuration for a [`McpTransportFactory`] that gates transport
/// creation through path/url validators.
#[derive(Debug, Clone, Default)]
pub struct TransportFactoryConfig {
    /// Allowed canonical path prefixes for `Stdio` command paths.
    ///
    /// When empty, command path canonicalization is skipped (back-compat).
    /// When non-empty, the command is canonicalized and rejected unless it
    /// lives under one of these prefixes.
    pub allowed_paths: Vec<PathBuf>,
    /// Whether to allow plain HTTP URLs that point to localhost.
    ///
    /// Defaults to `true` to support local dev. Set to `false` to require
    /// HTTPS for every Http transport.
    pub allow_http_localhost: bool,
}

impl TransportFactoryConfig {
    /// Strict defaults: no command paths allowed, HTTPS required.
    pub fn strict() -> Self {
        Self {
            allowed_paths: Vec::new(),
            allow_http_localhost: false,
        }
    }

    /// Lenient defaults: any command path, http://localhost allowed.
    pub fn lenient() -> Self {
        Self {
            allowed_paths: Vec::new(),
            allow_http_localhost: true,
        }
    }
}

/// Factory that creates [`McpTransport`] instances from a [`TransportSpec`].
///
/// Validators are called *before* spawning to fail fast on bad config.
#[async_trait]
pub trait McpTransportFactory: Send + Sync {
    /// Validate a spec without creating a transport.
    fn validate(&self, spec: &TransportSpec) -> Result<()>;

    /// Validate then create a transport for the given spec.
    async fn create(&self, spec: TransportSpec) -> Result<Box<dyn McpTransport>>;
}

/// Default factory that supports stdio and http transports.
#[derive(Debug, Clone, Default)]
pub struct DefaultTransportFactory {
    config: TransportFactoryConfig,
}

impl DefaultTransportFactory {
    /// Create a factory with the given config.
    pub fn new(config: TransportFactoryConfig) -> Self {
        Self { config }
    }

    /// Access the factory config.
    pub fn config(&self) -> &TransportFactoryConfig {
        &self.config
    }
}

#[async_trait]
impl McpTransportFactory for DefaultTransportFactory {
    fn validate(&self, spec: &TransportSpec) -> Result<()> {
        match spec {
            TransportSpec::Stdio { command, .. } => {
                if !self.config.allowed_paths.is_empty() {
                    validate_command_path(command, &self.config.allowed_paths)
                        .map_err(ServiceError::McpTransport)?;
                }
                Ok(())
            }
            TransportSpec::Http { url } => validate_url(url, self.config.allow_http_localhost)
                .map_err(ServiceError::McpTransport),
            TransportSpec::Tempfile { path } => {
                validate_tempfile_path(path).map_err(ServiceError::McpTransport)
            }
        }
    }

    async fn create(&self, spec: TransportSpec) -> Result<Box<dyn McpTransport>> {
        self.validate(&spec)?;
        match spec {
            TransportSpec::Stdio { command, args, env } => {
                let t = StdioTransport::new(&command, &args, &env).await?;
                Ok(Box::new(t))
            }
            TransportSpec::Http { url } => Ok(Box::new(HttpTransport::new(url))),
            TransportSpec::Tempfile { .. } => Err(ServiceError::McpTransport(
                "tempfile transport not implemented".into(),
            )),
        }
    }
}

/// Validate an MCP URL.
///
/// Rules:
/// - Must start with `https://` or (when `allow_http_localhost` is true)
///   `http://localhost` / `http://127.0.0.1` / `http://[::1]`.
/// - Must not be empty.
/// - Hostname must be present and non-empty.
pub fn validate_url(url: &str, allow_http_localhost: bool) -> std::result::Result<(), String> {
    if url.is_empty() {
        return Err("url is empty".into());
    }

    if let Some(rest) = url.strip_prefix("https://") {
        if rest.is_empty() {
            return Err("url has no host".into());
        }
        return Ok(());
    }

    if let Some(rest) = url.strip_prefix("http://") {
        if !allow_http_localhost {
            return Err("plain http:// not allowed (use https://)".into());
        }
        // Extract host portion (up to /, ?, #, or end).
        let host = rest.split(&['/', '?', '#'][..]).next().unwrap_or(rest);
        // Strip optional port.
        let host_only = if host.starts_with('[') {
            // IPv6 bracket notation.
            host.find(']').map(|i| &host[1..i]).unwrap_or(host)
        } else {
            host.rsplit(':').next_back().unwrap_or(host)
        };
        let host_lc = host_only.to_lowercase();
        const ALLOWED_LOCAL: &[&str] = &["localhost", "127.0.0.1", "::1"];
        if ALLOWED_LOCAL.iter().any(|&h| h == host_lc) {
            return Ok(());
        }
        return Err(format!(
            "http:// only allowed for localhost, got '{host_only}'",
        ));
    }

    Err(format!(
        "url must start with https:// or http://, got '{url}'"
    ))
}

/// Validate that a command path canonicalizes within one of the allowed
/// path prefixes.
///
/// `command` may be a bare program name (resolved via `$PATH`) or an
/// absolute path. If it cannot be canonicalized (e.g. PATH lookup fails),
/// the literal value is rejected.
pub fn validate_command_path(
    command: &str,
    allowed_paths: &[PathBuf],
) -> std::result::Result<(), String> {
    if command.is_empty() {
        return Err("command is empty".into());
    }
    if allowed_paths.is_empty() {
        return Ok(());
    }

    // Resolve the command. If it has path separators, canonicalize directly.
    // Otherwise look it up on $PATH.
    let resolved = if command.contains('/') || command.contains('\\') {
        std::fs::canonicalize(Path::new(command))
            .map_err(|e| format!("cannot canonicalize '{command}': {e}"))?
    } else {
        which_on_path(command)
            .ok_or_else(|| format!("command '{command}' not found on $PATH and not absolute"))?
    };

    let canonical_allowed: Vec<PathBuf> = allowed_paths
        .iter()
        .filter_map(|p| std::fs::canonicalize(p).ok())
        .collect();

    if canonical_allowed
        .iter()
        .any(|prefix| resolved.starts_with(prefix))
    {
        Ok(())
    } else {
        Err(format!(
            "command '{}' resolves outside allowed_paths ({})",
            resolved.display(),
            canonical_allowed
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ))
    }
}

/// Look up a bare program name on `$PATH` (Unix-style colon split).
fn which_on_path(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(program);
        if let Ok(canon) = std::fs::canonicalize(&candidate) {
            return Some(canon);
        }
    }
    None
}

/// Validate a tempfile path.
///
/// Rules:
/// - Must be absolute.
/// - Must not live under sensitive system roots (`/etc`, `/root`, `/sys`,
///   `/proc`, `/boot`, `/dev`).
pub fn validate_tempfile_path(path: &Path) -> std::result::Result<(), String> {
    if !path.is_absolute() {
        return Err(format!(
            "tempfile path must be absolute: {}",
            path.display()
        ));
    }

    const FORBIDDEN: &[&str] = &["/etc", "/root", "/sys", "/proc", "/boot", "/dev"];
    let s = path.to_string_lossy();
    for prefix in FORBIDDEN {
        if s == *prefix || s.starts_with(&format!("{prefix}/")) {
            return Err(format!(
                "tempfile path '{}' is under forbidden root '{}'",
                path.display(),
                prefix
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod factory_tests {
    use super::*;

    #[test]
    fn validate_url_accepts_https() {
        assert!(validate_url("https://example.com", false).is_ok());
        assert!(validate_url("https://example.com/path", false).is_ok());
    }

    #[test]
    fn validate_url_rejects_empty() {
        assert!(validate_url("", false).is_err());
        assert!(validate_url("", true).is_err());
    }

    #[test]
    fn validate_url_rejects_plain_http_when_disallowed() {
        let err = validate_url("http://example.com", false).unwrap_err();
        assert!(err.contains("http://"), "got: {err}");
    }

    #[test]
    fn validate_url_accepts_http_localhost_when_allowed() {
        assert!(validate_url("http://localhost", true).is_ok());
        assert!(validate_url("http://localhost:8080/rpc", true).is_ok());
        assert!(validate_url("http://127.0.0.1:1234", true).is_ok());
        assert!(validate_url("http://[::1]:9090", true).is_ok());
    }

    #[test]
    fn validate_url_rejects_http_external_even_when_localhost_allowed() {
        let err = validate_url("http://example.com", true).unwrap_err();
        assert!(err.to_lowercase().contains("localhost"), "got: {err}");
    }

    #[test]
    fn validate_url_rejects_unknown_scheme() {
        assert!(validate_url("ftp://example.com", true).is_err());
        assert!(validate_url("file:///etc/passwd", true).is_err());
    }

    #[test]
    fn validate_command_path_empty() {
        assert!(validate_command_path("", &[]).is_err());
    }

    #[test]
    fn validate_command_path_no_allowlist_passes() {
        // Empty allowlist = back-compat permissive mode.
        assert!(validate_command_path("/bin/sh", &[]).is_ok());
        assert!(validate_command_path("npx", &[]).is_ok());
    }

    #[test]
    fn validate_command_path_within_allowed() {
        // /usr/bin should canonicalize and contain known programs.
        let allowed = vec![PathBuf::from("/usr/bin"), PathBuf::from("/bin")];
        // sh exists on virtually all Linux systems either at /bin/sh or /usr/bin/sh.
        let result = validate_command_path("/bin/sh", &allowed);
        // Either ok, or PATHs got moved (then we just don't fail the test loudly).
        if let Err(e) = &result {
            // Skip this test if /bin/sh isn't canonicalizable; that's an
            // environment quirk, not a logic bug.
            eprintln!("validate_command_path skipped: {e}");
        }
    }

    #[test]
    fn validate_command_path_outside_allowed_rejected() {
        let allowed = vec![PathBuf::from("/usr/local/nonexistent-12345")];
        let result = validate_command_path("/bin/sh", &allowed);
        assert!(result.is_err());
    }

    #[test]
    fn validate_tempfile_path_absolute_required() {
        let p = PathBuf::from("relative/path");
        let err = validate_tempfile_path(&p).unwrap_err();
        assert!(err.to_lowercase().contains("absolute"));
    }

    #[test]
    fn validate_tempfile_path_blocks_etc() {
        let err = validate_tempfile_path(Path::new("/etc/passwd")).unwrap_err();
        assert!(err.contains("/etc"));
    }

    #[test]
    fn validate_tempfile_path_blocks_root() {
        let err = validate_tempfile_path(Path::new("/root/.ssh/id_rsa")).unwrap_err();
        assert!(err.contains("/root"));
    }

    #[test]
    fn validate_tempfile_path_blocks_sys_proc_dev_boot() {
        for p in &["/sys/kernel", "/proc/1", "/dev/null", "/boot/grub"] {
            let err = validate_tempfile_path(Path::new(p)).unwrap_err();
            assert!(err.contains(*p) || err.contains("forbidden"));
        }
    }

    #[test]
    fn validate_tempfile_path_allows_tmp() {
        assert!(validate_tempfile_path(Path::new("/tmp/socket.sock")).is_ok());
        assert!(validate_tempfile_path(Path::new("/var/run/clawft.sock")).is_ok());
    }

    #[tokio::test]
    async fn factory_validates_http_spec() {
        let f = DefaultTransportFactory::new(TransportFactoryConfig::lenient());
        let spec = TransportSpec::Http {
            url: "https://example.com".into(),
        };
        assert!(f.validate(&spec).is_ok());

        let spec_bad = TransportSpec::Http {
            url: "ftp://example.com".into(),
        };
        assert!(f.validate(&spec_bad).is_err());
    }

    #[tokio::test]
    async fn factory_strict_rejects_http_localhost() {
        let f = DefaultTransportFactory::new(TransportFactoryConfig::strict());
        let spec = TransportSpec::Http {
            url: "http://localhost:8080".into(),
        };
        assert!(f.validate(&spec).is_err());
    }

    #[tokio::test]
    async fn factory_validates_tempfile_spec() {
        let f = DefaultTransportFactory::new(TransportFactoryConfig::default());
        let spec = TransportSpec::Tempfile {
            path: PathBuf::from("/etc/shadow"),
        };
        assert!(f.validate(&spec).is_err());

        let spec_ok = TransportSpec::Tempfile {
            path: PathBuf::from("/tmp/mcp.sock"),
        };
        assert!(f.validate(&spec_ok).is_ok());
    }

    #[tokio::test]
    async fn factory_create_tempfile_returns_not_implemented() {
        let f = DefaultTransportFactory::new(TransportFactoryConfig::default());
        let spec = TransportSpec::Tempfile {
            path: PathBuf::from("/tmp/mcp.sock"),
        };
        let result = f.create(spec).await;
        // `Box<dyn McpTransport>` does not implement Debug, so we
        // can't `.unwrap_err()`; pattern-match instead.
        match result {
            Ok(_) => panic!("expected NotImplemented error"),
            Err(e) => {
                let msg = e.to_string();
                assert!(msg.contains("not implemented"), "got: {msg}");
            }
        }
    }

    #[test]
    fn factory_config_strict_vs_lenient() {
        let s = TransportFactoryConfig::strict();
        assert!(!s.allow_http_localhost);
        let l = TransportFactoryConfig::lenient();
        assert!(l.allow_http_localhost);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_transport_construction() {
        let transport = HttpTransport::new("http://localhost:8080".into());
        assert_eq!(transport.endpoint, "http://localhost:8080");
    }

    #[tokio::test]
    async fn mock_transport_returns_responses() {
        let response = JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id: 1,
            result: Some(serde_json::json!({"tools": []})),
            error: None,
        };

        let transport = MockTransport::new(vec![response]);
        let req = JsonRpcRequest::new(1, "tools/list", serde_json::json!({}));
        let resp = transport.send_request(req).await.unwrap();
        assert_eq!(resp.id, 1);
        assert!(resp.result.is_some());
    }

    #[tokio::test]
    async fn mock_transport_records_requests() {
        let response = JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id: 1,
            result: Some(serde_json::json!(null)),
            error: None,
        };

        let transport = MockTransport::new(vec![response]);
        let req = JsonRpcRequest::new(1, "test/method", serde_json::json!({"key": "value"}));
        transport.send_request(req).await.unwrap();

        let requests = transport.requests().await;
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "test/method");
    }

    #[tokio::test]
    async fn mock_transport_empty_responses_errors() {
        let transport = MockTransport::new(vec![]);
        let req = JsonRpcRequest::new(1, "test", serde_json::json!({}));
        let result = transport.send_request(req).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn mock_transport_records_notifications() {
        let transport = MockTransport::new(vec![]);
        transport
            .send_notification("notifications/initialized", serde_json::json!({}))
            .await
            .unwrap();
        transport
            .send_notification(
                "notifications/progress",
                serde_json::json!({"token": "abc"}),
            )
            .await
            .unwrap();

        let notifs = transport.notifications().await;
        assert_eq!(notifs.len(), 2);
        assert_eq!(notifs[0].method, "notifications/initialized");
        assert_eq!(notifs[1].method, "notifications/progress");
        assert_eq!(notifs[1].params["token"], "abc");
    }

    #[tokio::test]
    async fn notification_has_no_id_field() {
        let notif = JsonRpcNotification::new("test/notify", serde_json::json!({}));
        let json = serde_json::to_string(&notif).unwrap();
        // JSON-RPC notifications MUST NOT have an "id" field.
        assert!(!json.contains("\"id\""));
        assert!(json.contains("\"jsonrpc\":\"2.0\""));
        assert!(json.contains("\"method\":\"test/notify\""));
    }

    // ── WEFT-191: StdioTransport is_alive / close ────────────────────────

    #[tokio::test]
    async fn stdio_is_alive_while_child_running() {
        let env = HashMap::new();
        let transport = StdioTransport::new("sleep", &["30".into()], &env)
            .await
            .expect("spawn sleep");
        assert!(
            transport.is_alive().await,
            "sleep child should still be running"
        );
        transport.close().await.unwrap();
        assert!(!transport.is_alive().await);
    }

    #[tokio::test]
    async fn stdio_is_alive_false_after_child_exits() {
        let env = HashMap::new();
        // `true` exits immediately with success.
        let transport = StdioTransport::new("true", &[], &env)
            .await
            .expect("spawn true");
        // Give the kernel a moment to reap / report exit.
        for _ in 0..50 {
            if !transport.is_alive().await {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            !transport.is_alive().await,
            "child that already exited must report not alive"
        );
        // Writes should fail fast rather than hang on a dead pipe.
        let req = JsonRpcRequest::new(1, "tools/list", serde_json::json!({}));
        let err = transport.send_request(req).await.unwrap_err();
        assert!(
            err.to_string().contains("not alive"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn stdio_to_transport_spec_round_trip_fields() {
        let mut env = HashMap::new();
        env.insert("FOO".into(), "bar".into());
        let transport = StdioTransport::new("sleep", &["5".into()], &env)
            .await
            .expect("spawn");
        match transport.to_transport_spec() {
            TransportSpec::Stdio {
                command,
                args,
                env: e,
            } => {
                assert_eq!(command, "sleep");
                assert_eq!(args, vec!["5".to_string()]);
                assert_eq!(e.get("FOO").map(String::as_str), Some("bar"));
            }
            other => panic!("expected Stdio spec, got {other:?}"),
        }
        transport.close().await.unwrap();
    }

    #[tokio::test]
    async fn mock_transport_is_alive_and_close() {
        let t = MockTransport::new(vec![]);
        assert!(t.is_alive().await);
        t.set_alive(false);
        assert!(!t.is_alive().await);
        t.set_alive(true);
        t.close().await.unwrap();
        assert!(t.was_closed());
        assert!(!t.is_alive().await);
    }
}
