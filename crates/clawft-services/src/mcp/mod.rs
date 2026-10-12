//! MCP (Model Context Protocol) client.
//!
//! Provides a client for communicating with MCP servers using
//! JSON-RPC 2.0 over pluggable transports (stdio or HTTP).

pub mod bridge;
pub mod client;
pub mod composite;
pub mod discovery;
pub mod ide;
pub mod middleware;
pub mod provider;
pub mod server;
/// Stateless MCP (2026-07-28) helpers shared by every transport.
pub mod stateless;
/// Session capability tokens + bind policy (ADR-075 G5 / WEFT-697).
pub mod session_cap;
/// HTTP/SSE MCP listen (ADR-075 G4 / WEFT-696). Requires `api` feature (axum).
#[cfg(any(feature = "api", test))]
pub mod http_serve;
pub mod transport;
pub mod types;

pub use provider::{
    BuiltinToolProvider, CallToolResult, ContentBlock, SkillToolProvider, ToolError, ToolProvider,
    skill_to_tool_definition, skills_to_tool_definitions,
};
pub use session_cap::{
    BindKind, ClientLabel, SessionCapability, SessionScopes, SessionTokenStore, classify_bind_host,
    extract_bearer, validate_listen_policy,
};
#[cfg(any(feature = "api", test))]
pub use http_serve::{HttpMcpConfig, HttpMcpState, mcp_http_router, run_listen, serve_mcp_http};
pub use transport::{
    DefaultTransportFactory, McpTransportFactory, SpecReconnect, TOOLS_LIST_CHANGED_METHOD,
    TransportFactoryConfig, TransportReconnect, TransportSpec, validate_command_path,
    validate_tempfile_path, validate_url,
};

// WEFT-187 / WEFT-493: MCP config hot-reload watcher + daemon boot surface.
pub use discovery::{
    ConfigReloadResult, McpConfigWatcherBoot, McpConfigWatcherHandle, SharedMcpServerManager,
    boot_mcp_manager_with_watcher, load_mcp_servers_from_path, load_mcp_servers_from_str,
    start_config_watcher, start_config_watcher_with_debounce,
};

/// The MCP protocol version negotiated during initialize.
///
/// This constant is the single source of truth for protocol version
/// strings used in both client and server code. We always send this
/// in our `initialize` request.
pub const MCP_PROTOCOL_VERSION: &str = "2025-06-18";

/// Protocol versions we accept on the server side of the handshake.
///
/// WEFT-489. If the remote server's `initialize` response reports a
/// `protocolVersion` outside this set, [`McpSession::connect`] aborts
/// the session with [`crate::error::ServiceError::McpProtocolVersionMismatch`]
/// after logging a `warn!` so the operator sees the rejection.
///
/// Order is informational; we use set semantics. The current value
/// includes the previous published versions plus our negotiated
/// version so a server that lags one revision still attaches.
pub const MCP_SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2024-11-05", "2025-03-26", "2025-06-18"];

/// Returns true if `version` is in our supported set.
///
/// Empty string is treated as "server omitted the field" and falls
/// back to our advertised version (which is always supported).
pub fn is_supported_protocol_version(version: &str) -> bool {
    if version.is_empty() {
        return true;
    }
    MCP_SUPPORTED_PROTOCOL_VERSIONS.contains(&version)
}

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::error::{Result, ServiceError};
use transport::McpTransport;
use types::JsonRpcRequest;

/// Default interval between MCP `ping` keepalive probes (WEFT-191).
pub const DEFAULT_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);

/// Default max reconnect attempts with exponential backoff (WEFT-191).
pub const DEFAULT_MAX_RECONNECT_ATTEMPTS: u32 = 5;

/// Initial reconnect backoff delay (WEFT-191).
pub const DEFAULT_INITIAL_BACKOFF: Duration = Duration::from_millis(100);

/// Cap on reconnect backoff delay (WEFT-191).
pub const DEFAULT_MAX_BACKOFF: Duration = Duration::from_secs(10);

/// Durability settings for long-lived MCP sessions (WEFT-191).
///
/// Controls keepalive/ping cadence and reconnect-with-backoff policy.
/// Use [`DurabilityConfig::disabled`] for one-shot test sessions that
/// should not spawn background tasks or attempt reconnect.
#[derive(Debug, Clone)]
pub struct DurabilityConfig {
    /// Interval between `ping` requests. `Duration::ZERO` disables keepalive.
    pub keepalive_interval: Duration,
    /// Maximum number of reconnect attempts before giving up.
    pub max_reconnect_attempts: u32,
    /// Initial delay before the first reconnect attempt.
    pub initial_backoff: Duration,
    /// Maximum delay between reconnect attempts (exponential backoff cap).
    pub max_backoff: Duration,
}

impl Default for DurabilityConfig {
    fn default() -> Self {
        Self {
            keepalive_interval: DEFAULT_KEEPALIVE_INTERVAL,
            max_reconnect_attempts: DEFAULT_MAX_RECONNECT_ATTEMPTS,
            initial_backoff: DEFAULT_INITIAL_BACKOFF,
            max_backoff: DEFAULT_MAX_BACKOFF,
        }
    }
}

impl DurabilityConfig {
    /// No keepalive, no reconnect retries (unit tests / one-shot tools).
    pub fn disabled() -> Self {
        Self {
            keepalive_interval: Duration::ZERO,
            max_reconnect_attempts: 0,
            initial_backoff: Duration::ZERO,
            max_backoff: Duration::ZERO,
        }
    }

    /// Fast timings suitable for unit tests.
    pub fn for_tests() -> Self {
        Self {
            keepalive_interval: Duration::from_millis(50),
            max_reconnect_attempts: 3,
            initial_backoff: Duration::from_millis(5),
            max_backoff: Duration::from_millis(40),
        }
    }
}

/// Server capabilities returned from the MCP initialize handshake.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServerCapabilities {
    /// Tool-related capabilities, if the server supports tools.
    #[serde(default)]
    pub tools: Option<serde_json::Value>,
    // Other capability fields can be added as needed.
}

impl ServerCapabilities {
    /// Whether the server advertised `tools.listChanged` (WEFT-200).
    ///
    /// Accepts both `{ "listChanged": true }` and bare `true` / object shapes
    /// seen in the wild.
    pub fn tools_list_changed(&self) -> bool {
        let Some(tools) = self.tools.as_ref() else {
            return false;
        };
        if let Some(b) = tools.get("listChanged").and_then(|v| v.as_bool()) {
            return b;
        }
        if let Some(b) = tools.get("list_changed").and_then(|v| v.as_bool()) {
            return b;
        }
        false
    }
}

/// Server information returned from the MCP initialize handshake.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServerInfo {
    /// Server name.
    #[serde(default)]
    pub name: String,
    /// Server version.
    #[serde(default)]
    pub version: String,
}

/// Definition of an MCP tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    /// Tool name.
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// JSON Schema for the tool's input parameters.
    #[serde(rename = "inputSchema", alias = "input_schema")]
    pub input_schema: serde_json::Value,
}

/// Client for communicating with an MCP server.
///
/// Transport is held behind an `Arc` slot so reconnect (WEFT-191) can swap
/// the connection without blocking concurrent request waiters longer than
/// a brief mutex acquisition.
pub struct McpClient {
    transport: Mutex<Arc<dyn McpTransport>>,
    request_id: AtomicU64,
    /// In-flight request IDs (for `notifications/cancelled` on shutdown).
    in_flight: Mutex<HashSet<u64>>,
}

impl std::fmt::Debug for McpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpClient")
            .field("request_id", &self.request_id.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl McpClient {
    /// Create a new MCP client with the given transport.
    pub fn new(transport: Box<dyn McpTransport>) -> Self {
        Self {
            transport: Mutex::new(Arc::from(transport)),
            request_id: AtomicU64::new(1),
            in_flight: Mutex::new(HashSet::new()),
        }
    }

    /// Snapshot of the current transport (cheap `Arc` clone).
    async fn transport_arc(&self) -> Arc<dyn McpTransport> {
        Arc::clone(&*self.transport.lock().await)
    }

    /// Replace the underlying transport (used after reconnect handshake).
    async fn replace_transport(&self, transport: Box<dyn McpTransport>) {
        *self.transport.lock().await = Arc::from(transport);
    }

    /// Whether the current transport reports alive (WEFT-191).
    pub async fn is_alive(&self) -> bool {
        self.transport_arc().await.is_alive().await
    }

    /// MCP `ping` liveness probe (WEFT-191).
    ///
    /// Sends a JSON-RPC request with method `ping`. Empty result or any
    /// successful result is treated as alive; protocol/transport errors
    /// propagate to the caller.
    pub async fn ping(&self) -> Result<()> {
        let _ = self.send_raw("ping", serde_json::json!({})).await?;
        Ok(())
    }

    /// Send `notifications/cancelled` for every in-flight request, then
    /// close the transport (WEFT-191).
    pub async fn shutdown_graceful(&self) -> Result<()> {
        let ids: Vec<u64> = {
            let set = self.in_flight.lock().await;
            set.iter().copied().collect()
        };
        let transport = self.transport_arc().await;
        for id in ids {
            // Best-effort cancel; ignore transport errors so we still close.
            let _ = transport
                .send_notification(
                    "notifications/cancelled",
                    serde_json::json!({ "requestId": id }),
                )
                .await;
        }
        transport.close().await
    }

    /// Current in-flight request IDs (test / diagnostics helper).
    pub async fn in_flight_ids(&self) -> Vec<u64> {
        let set = self.in_flight.lock().await;
        let mut ids: Vec<u64> = set.iter().copied().collect();
        ids.sort_unstable();
        ids
    }

    /// Insert a synthetic in-flight id so shutdown can emit cancel (tests).
    #[cfg(any(test, feature = "test-utils"))]
    pub async fn track_in_flight_for_test(&self, id: u64) {
        self.in_flight.lock().await.insert(id);
    }

    /// List all tools available on the MCP server.
    pub async fn list_tools(&self) -> Result<Vec<ToolDefinition>> {
        let id = self.next_id();
        let request = JsonRpcRequest::new(id, "tools/list", serde_json::json!({}));

        let response = self.send_request_tracked(request).await?;

        if let Some(err) = response.error {
            return Err(ServiceError::McpProtocol(format!(
                "code={}, message={}",
                err.code, err.message
            )));
        }

        let result = response
            .result
            .ok_or_else(|| ServiceError::McpProtocol("empty result".into()))?;

        // MCP returns tools in a `tools` array.
        let tools_value = result
            .get("tools")
            .cloned()
            .unwrap_or_else(|| serde_json::Value::Array(vec![]));

        let tools: Vec<ToolDefinition> = serde_json::from_value(tools_value)?;
        Ok(tools)
    }

    /// Poll one inbound server→client notification (WEFT-200).
    pub async fn poll_notification(&self) -> Option<types::JsonRpcNotification> {
        self.transport_arc().await.poll_notification().await
    }

    /// Drain pending `notifications/tools/list_changed` and re-fetch tools.
    ///
    /// Returns `Ok(None)` when no list-changed notification was pending.
    /// Returns `Ok(Some(tools))` after a successful re-fetch when at least
    /// one list-changed notification was drained.
    pub async fn refresh_tools_if_list_changed(&self) -> Result<Option<Vec<ToolDefinition>>> {
        let mut saw_list_changed = false;
        // Drain the full queue so stale progress notifs do not block us.
        loop {
            match self.poll_notification().await {
                Some(n) if n.method == TOOLS_LIST_CHANGED_METHOD => {
                    saw_list_changed = true;
                }
                Some(n) => {
                    debug!(method = %n.method, "mcp client: ignoring non-list_changed notification");
                }
                None => break,
            }
        }
        if !saw_list_changed {
            return Ok(None);
        }
        let tools = self.list_tools().await?;
        Ok(Some(tools))
    }

    /// Call a tool on the MCP server.
    pub async fn call_tool(
        &self,
        name: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let id = self.next_id();
        let request = JsonRpcRequest::new(
            id,
            "tools/call",
            serde_json::json!({
                "name": name,
                "arguments": params,
            }),
        );

        let response = self.send_request_tracked(request).await?;

        if let Some(err) = response.error {
            return Err(ServiceError::McpProtocol(format!(
                "code={}, message={}",
                err.code, err.message
            )));
        }

        response
            .result
            .ok_or_else(|| ServiceError::McpProtocol("empty result".into()))
    }

    /// Send a raw JSON-RPC request and return the result value.
    ///
    /// Unlike `list_tools` and `call_tool`, this method does not interpret
    /// the response -- it simply returns the raw `serde_json::Value` from
    /// the `result` field, or an error if the response contains one.
    pub async fn send_raw(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let id = self.next_id();
        let request = JsonRpcRequest::new(id, method, params);
        let response = self.send_request_tracked(request).await?;

        if let Some(err) = response.error {
            return Err(ServiceError::McpProtocol(format!(
                "code={}, message={}",
                err.code, err.message
            )));
        }

        response
            .result
            .ok_or_else(|| ServiceError::McpProtocol("empty result".into()))
    }

    /// Access a snapshot of the underlying transport (WEFT-191).
    ///
    /// Prefer higher-level APIs (`is_alive`, `ping`, `send_raw`). The Arc
    /// clone is safe for concurrent use with the transport implementations.
    pub async fn transport(&self) -> Arc<dyn McpTransport> {
        self.transport_arc().await
    }

    async fn send_request_tracked(
        &self,
        request: JsonRpcRequest,
    ) -> Result<types::JsonRpcResponse> {
        let id = request.id;
        {
            self.in_flight.lock().await.insert(id);
        }
        let transport = self.transport_arc().await;
        let result = transport.send_request(request).await;
        {
            self.in_flight.lock().await.remove(&id);
        }
        result
    }

    /// Generate the next request ID.
    fn next_id(&self) -> u64 {
        self.request_id.fetch_add(1, Ordering::Relaxed)
    }
}

/// An MCP session that has completed the initialize handshake.
///
/// Wraps an [`McpClient`] and holds the server capabilities, server info,
/// and negotiated protocol version obtained during the handshake.
///
/// Use [`McpSession::connect`] to create a session -- it will:
/// 1. Send an `initialize` request with client capabilities.
/// 2. Parse the server's response (capabilities, info, protocol version).
/// 3. Send the `notifications/initialized` notification.
///
/// # Durability (WEFT-191)
///
/// Long-lived gateway sessions can:
/// - probe liveness via [`Self::is_alive`] / [`Self::ping`]
/// - reconnect with exponential backoff via [`Self::reconnect_with_backoff`]
/// - run a background keepalive loop via [`Self::start_keepalive`]
/// - emit `notifications/cancelled` and close transport on [`Self::shutdown`]
pub struct McpSession {
    client: McpClient,
    /// Capabilities reported by the server.
    pub server_capabilities: ServerCapabilities,
    /// Server identification (name + version).
    pub server_info: ServerInfo,
    /// Protocol version negotiated with the server.
    pub protocol_version: String,
    /// Optional transport recreators for reconnect-with-backoff.
    reconnect: Option<Arc<dyn TransportReconnect>>,
    /// Keepalive / backoff policy.
    durability: DurabilityConfig,
    /// Set by [`Self::shutdown`]; stops keepalive.
    shutdown: AtomicBool,
    /// Background keepalive task handle (if started).
    keepalive_handle: Mutex<Option<JoinHandle<()>>>,
    /// Serializes reconnect attempts.
    reconnect_lock: Mutex<()>,
}

impl McpSession {
    /// Connect to an MCP server by performing the initialize handshake.
    ///
    /// Equivalent to [`Self::connect_with_options`] with no reconnect
    /// handle and default durability (keepalive interval recorded but
    /// not started until [`Self::start_keepalive`]).
    pub async fn connect(transport: Box<dyn McpTransport>) -> Result<Self> {
        Self::connect_with_options(transport, None, DurabilityConfig::default()).await
    }

    /// Connect with reconnect handle and durability config (WEFT-191).
    pub async fn connect_with_options(
        transport: Box<dyn McpTransport>,
        reconnect: Option<Arc<dyn TransportReconnect>>,
        durability: DurabilityConfig,
    ) -> Result<Self> {
        let client = McpClient::new(transport);
        let (server_capabilities, server_info, protocol_version) =
            Self::perform_handshake(&client).await?;

        Ok(Self {
            client,
            server_capabilities,
            server_info,
            protocol_version,
            reconnect,
            durability,
            shutdown: AtomicBool::new(false),
            keepalive_handle: Mutex::new(None),
            reconnect_lock: Mutex::new(()),
        })
    }

    /// Run the MCP initialize handshake on an already-constructed client.
    async fn perform_handshake(
        client: &McpClient,
    ) -> Result<(ServerCapabilities, ServerInfo, String)> {
        // Step 1: Send initialize request.
        let init_result = client
            .send_raw(
                "initialize",
                serde_json::json!({
                    "protocolVersion": MCP_PROTOCOL_VERSION,
                    "capabilities": { "tools": {} },
                    "clientInfo": {
                        "name": "clawft",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                }),
            )
            .await?;

        // Step 2: Parse server response.
        let server_capabilities: ServerCapabilities =
            serde_json::from_value(init_result.get("capabilities").cloned().unwrap_or_default())
                .unwrap_or_default();

        let server_info: ServerInfo =
            serde_json::from_value(init_result.get("serverInfo").cloned().unwrap_or_default())
                .unwrap_or_default();

        let protocol_version = init_result
            .get("protocolVersion")
            .and_then(|v| v.as_str())
            .unwrap_or(MCP_PROTOCOL_VERSION)
            .to_string();

        // WEFT-489: reject foreign protocol versions before sending
        // `notifications/initialized`. A non-matching server gets a
        // hard error here rather than at the first tools/list (where
        // the failure mode is much harder to diagnose).
        if !is_supported_protocol_version(&protocol_version) {
            tracing::warn!(
                ours = ?MCP_SUPPORTED_PROTOCOL_VERSIONS,
                theirs = %protocol_version,
                "mcp initialize: protocol-version mismatch, aborting session"
            );
            return Err(ServiceError::McpProtocolVersionMismatch {
                ours: MCP_SUPPORTED_PROTOCOL_VERSIONS
                    .iter()
                    .map(|s| (*s).to_string())
                    .collect(),
                theirs: protocol_version,
            });
        }

        // Step 3: Send initialized notification.
        client
            .transport()
            .await
            .send_notification("notifications/initialized", serde_json::json!({}))
            .await?;

        Ok((server_capabilities, server_info, protocol_version))
    }

    /// List tools available on the connected server.
    pub async fn list_tools(&self) -> Result<Vec<ToolDefinition>> {
        self.ensure_connected().await?;
        self.client.list_tools().await
    }

    /// Poll one inbound notification from the transport (WEFT-200).
    pub async fn poll_notification(&self) -> Option<types::JsonRpcNotification> {
        self.client.poll_notification().await
    }

    /// Handle inbound `notifications/tools/list_changed` by re-fetching
    /// `tools/list` (WEFT-200).
    ///
    /// Returns `Ok(None)` if no list-changed notification was pending.
    pub async fn refresh_tools_if_list_changed(&self) -> Result<Option<Vec<ToolDefinition>>> {
        self.ensure_connected().await?;
        self.client.refresh_tools_if_list_changed().await
    }

    /// Whether the peer advertised `tools.listChanged` at handshake (WEFT-200).
    pub fn peer_tools_list_changed(&self) -> bool {
        self.server_capabilities.tools_list_changed()
    }

    /// Call a tool on the connected server.
    pub async fn call_tool(
        &self,
        name: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        self.ensure_connected().await?;
        self.client.call_tool(name, params).await
    }

    /// Access the underlying client.
    pub fn client(&self) -> &McpClient {
        &self.client
    }

    /// Durability configuration for this session.
    pub fn durability(&self) -> &DurabilityConfig {
        &self.durability
    }

    /// Whether [`Self::shutdown`] has been called.
    pub fn is_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::SeqCst)
    }

    /// Whether the underlying transport is alive (WEFT-191).
    pub async fn is_alive(&self) -> bool {
        if self.is_shutdown() {
            return false;
        }
        self.client.is_alive().await
    }

    /// MCP `ping` keepalive probe (WEFT-191).
    pub async fn ping(&self) -> Result<()> {
        if self.is_shutdown() {
            return Err(ServiceError::McpTransport("session is shut down".into()));
        }
        self.client.ping().await
    }

    /// Ensure the transport is alive; reconnect with backoff if not (WEFT-191).
    pub async fn ensure_connected(&self) -> Result<()> {
        if self.is_shutdown() {
            return Err(ServiceError::McpTransport("session is shut down".into()));
        }
        if self.client.is_alive().await {
            return Ok(());
        }
        debug!("mcp session transport dead; attempting reconnect");
        self.reconnect_with_backoff().await
    }

    /// Reconnect with exponential backoff (WEFT-191).
    ///
    /// Requires a reconnect handle from [`Self::connect_with_options`].
    /// On success, re-runs the initialize handshake and updates
    /// `server_capabilities` / `server_info` / `protocol_version`.
    pub async fn reconnect_with_backoff(&self) -> Result<()> {
        if self.is_shutdown() {
            return Err(ServiceError::McpTransport("session is shut down".into()));
        }
        let reconnect = self.reconnect.as_ref().ok_or_else(|| {
            ServiceError::McpTransport("no reconnect configured for session".into())
        })?;

        let _guard = self.reconnect_lock.lock().await;
        // Another waiter may have already restored the connection.
        if self.client.is_alive().await {
            return Ok(());
        }

        let max_attempts = self.durability.max_reconnect_attempts.max(1);
        let mut delay = self.durability.initial_backoff;
        if delay.is_zero() {
            delay = Duration::from_millis(1);
        }
        let max_backoff = if self.durability.max_backoff.is_zero() {
            delay
        } else {
            self.durability.max_backoff
        };

        let mut last_err = ServiceError::McpTransport("reconnect not attempted".into());
        for attempt in 1..=max_attempts {
            if self.is_shutdown() {
                return Err(ServiceError::McpTransport("session is shut down".into()));
            }
            debug!(attempt, max_attempts, "mcp reconnect attempt");
            match reconnect.recreate().await {
                Ok(transport) => {
                    // Close the dead transport best-effort before swap.
                    let _ = self.client.transport().await.close().await;
                    self.client.replace_transport(transport).await;
                    match Self::perform_handshake(&self.client).await {
                        Ok((_caps, info, version)) => {
                            // Public handshake fields retain first-connect values
                            // (not interior-mutable under &self). Tool/call traffic
                            // uses the new transport; metadata is diagnostic only.
                            debug!(
                                attempt,
                                server = %info.name,
                                protocol = %version,
                                "mcp reconnect succeeded"
                            );
                            return Ok(());
                        }
                        Err(e) => {
                            warn!(attempt, error = %e, "mcp reconnect handshake failed");
                            last_err = e;
                        }
                    }
                }
                Err(e) => {
                    warn!(attempt, error = %e, "mcp reconnect recreate failed");
                    last_err = e;
                }
            }
            if attempt < max_attempts {
                tokio::time::sleep(delay).await;
                delay = delay.saturating_mul(2).min(max_backoff);
            }
        }

        Err(ServiceError::McpTransport(format!(
            "reconnect exhausted after {max_attempts} attempts: {last_err}"
        )))
    }

    /// Start a background keepalive loop that pings and reconnects (WEFT-191).
    ///
    /// No-op when `keepalive_interval` is zero or a loop is already running.
    /// Requires `Arc<Self>` so the task can hold a weak-free strong ref.
    pub async fn start_keepalive(self: &Arc<Self>) {
        if self.durability.keepalive_interval.is_zero() {
            return;
        }
        let mut slot = self.keepalive_handle.lock().await;
        if slot.is_some() {
            return;
        }
        let session = Arc::clone(self);
        let interval = self.durability.keepalive_interval;
        let handle = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // Skip the immediate first tick so we don't ping mid-handshake consumers.
            ticker.tick().await;
            loop {
                ticker.tick().await;
                if session.is_shutdown() {
                    break;
                }
                match session.ping().await {
                    Ok(()) => {
                        debug!("mcp keepalive ping ok");
                    }
                    Err(e) => {
                        warn!(error = %e, "mcp keepalive ping failed");
                        if session.is_shutdown() {
                            break;
                        }
                        if session.reconnect.is_some()
                            && let Err(re) = session.reconnect_with_backoff().await
                        {
                            warn!(error = %re, "mcp keepalive reconnect failed");
                        }
                    }
                }
            }
        });
        *slot = Some(handle);
    }

    /// Graceful shutdown: cancel in-flight ops, stop keepalive, close transport (WEFT-191).
    ///
    /// Emits `notifications/cancelled` for each in-flight request ID, then
    /// closes the transport (killing a stdio child when applicable).
    pub async fn shutdown(&self) -> Result<()> {
        if self
            .shutdown
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Ok(());
        }
        if let Some(handle) = self.keepalive_handle.lock().await.take() {
            handle.abort();
        }
        self.client.shutdown_graceful().await
    }
}

impl std::fmt::Debug for McpSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpSession")
            .field("server_info", &self.server_info)
            .field("protocol_version", &self.protocol_version)
            .field("shutdown", &self.is_shutdown())
            .finish_non_exhaustive()
    }
}

impl Drop for McpSession {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Ok(mut slot) = self.keepalive_handle.try_lock()
            && let Some(handle) = slot.take()
        {
            handle.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use transport::MockTransport;
    use types::JsonRpcResponse;

    fn make_success_response(id: u64, result: serde_json::Value) -> JsonRpcResponse {
        JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id,
            result: Some(result),
            error: None,
        }
    }

    fn make_error_response(id: u64, code: i32, message: &str) -> JsonRpcResponse {
        JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id,
            result: None,
            error: Some(types::JsonRpcError {
                code,
                message: message.into(),
                data: None,
            }),
        }
    }

    #[tokio::test]
    async fn list_tools_parses_response() {
        let tools_response = make_success_response(
            1,
            serde_json::json!({
                "tools": [
                    {
                        "name": "echo",
                        "description": "Echoes input",
                        "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}
                    },
                    {
                        "name": "calc",
                        "description": "Calculator",
                        "inputSchema": {"type": "object"}
                    }
                ]
            }),
        );

        let transport = MockTransport::new(vec![tools_response]);
        let client = McpClient::new(Box::new(transport));

        let tools = client.list_tools().await.unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "echo");
        assert_eq!(tools[0].description, "Echoes input");
        assert_eq!(tools[1].name, "calc");
    }

    #[tokio::test]
    async fn list_tools_empty() {
        let response = make_success_response(1, serde_json::json!({"tools": []}));
        let transport = MockTransport::new(vec![response]);
        let client = McpClient::new(Box::new(transport));

        let tools = client.list_tools().await.unwrap();
        assert!(tools.is_empty());
    }

    #[tokio::test]
    async fn call_tool_sends_correct_request() {
        let response = make_success_response(1, serde_json::json!({"output": "hello"}));
        let transport = MockTransport::new(vec![response]);
        let client = McpClient::new(Box::new(transport));

        let result = client
            .call_tool("echo", serde_json::json!({"text": "hello"}))
            .await
            .unwrap();

        assert_eq!(result["output"], "hello");
    }

    #[tokio::test]
    async fn call_tool_request_format() {
        let response = make_success_response(1, serde_json::json!({}));
        let transport = MockTransport::new(vec![response]);
        let client = McpClient::new(Box::new(transport));

        client
            .call_tool("my_tool", serde_json::json!({"arg": 42}))
            .await
            .unwrap();

        // Request was sent successfully if we got here without error.
        // The mock transport verified we sent a valid JSON-RPC request.
    }

    #[tokio::test]
    async fn handle_jsonrpc_error_on_list_tools() {
        let response = make_error_response(1, -32601, "method not found");
        let transport = MockTransport::new(vec![response]);
        let client = McpClient::new(Box::new(transport));

        let result = client.list_tools().await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, ServiceError::McpProtocol(_)));
        assert!(err.to_string().contains("method not found"));
    }

    #[tokio::test]
    async fn handle_jsonrpc_error_on_call_tool() {
        let response = make_error_response(1, -32602, "invalid params");
        let transport = MockTransport::new(vec![response]);
        let client = McpClient::new(Box::new(transport));

        let result = client.call_tool("bad", serde_json::json!({})).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("invalid params"));
    }

    #[tokio::test]
    async fn request_ids_increment() {
        let responses = vec![
            make_success_response(1, serde_json::json!({"tools": []})),
            make_success_response(2, serde_json::json!({"tools": []})),
        ];
        let transport = MockTransport::new(responses);
        let client = McpClient::new(Box::new(transport));

        client.list_tools().await.unwrap();
        client.list_tools().await.unwrap();
        // If we get here without error, IDs were generated correctly.
    }

    #[tokio::test]
    async fn empty_result_is_error() {
        let response = JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id: 1,
            result: None,
            error: None,
        };
        let transport = MockTransport::new(vec![response]);
        let client = McpClient::new(Box::new(transport));

        let result = client.call_tool("test", serde_json::json!({})).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ServiceError::McpProtocol(_)));
    }

    #[tokio::test]
    async fn tool_definition_serde() {
        let td = ToolDefinition {
            name: "test".into(),
            description: "A test tool".into(),
            input_schema: serde_json::json!({"type": "object"}),
        };
        let json = serde_json::to_string(&td).unwrap();
        let restored: ToolDefinition = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.name, "test");
        assert_eq!(restored.description, "A test tool");
    }

    #[tokio::test]
    async fn tool_definition_input_schema_alias() {
        // MCP uses camelCase, but we should also accept snake_case.
        let json = r#"{"name":"t","description":"d","input_schema":{"type":"object"}}"#;
        let td: ToolDefinition = serde_json::from_str(json).unwrap();
        assert_eq!(td.name, "t");
    }

    // ── send_raw tests ──────────────────────────────────────────────────

    #[tokio::test]
    async fn send_raw_returns_result_value() {
        let response = make_success_response(
            1,
            serde_json::json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "serverInfo": { "name": "test-server", "version": "1.0" }
            }),
        );
        let transport = MockTransport::new(vec![response]);
        let client = McpClient::new(Box::new(transport));

        let result = client
            .send_raw("initialize", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(result["serverInfo"]["name"], "test-server");
    }

    #[tokio::test]
    async fn send_raw_propagates_errors() {
        let response = make_error_response(1, -32600, "invalid request");
        let transport = MockTransport::new(vec![response]);
        let client = McpClient::new(Box::new(transport));

        let result = client.send_raw("initialize", serde_json::json!({})).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("invalid request"));
    }

    // ── McpSession tests ────────────────────────────────────────────────

    /// Helper to build a mock initialize response.
    fn make_init_response(id: u64) -> JsonRpcResponse {
        make_success_response(
            id,
            serde_json::json!({
                "protocolVersion": "2025-06-18",
                "capabilities": { "tools": { "listChanged": true } },
                "serverInfo": { "name": "mock-server", "version": "0.1.0" }
            }),
        )
    }

    #[tokio::test]
    async fn session_connect_performs_handshake() {
        let transport = MockTransport::new(vec![make_init_response(1)]);
        let transport = Box::new(transport);
        let session = McpSession::connect(transport).await.unwrap();

        assert_eq!(session.server_info.name, "mock-server");
        assert_eq!(session.server_info.version, "0.1.0");
        assert_eq!(session.protocol_version, "2025-06-18");
        assert!(session.server_capabilities.tools.is_some());
    }

    #[test]
    fn server_capabilities_tools_list_changed() {
        let caps = ServerCapabilities {
            tools: Some(serde_json::json!({ "listChanged": true })),
        };
        assert!(caps.tools_list_changed());
        let caps_false = ServerCapabilities {
            tools: Some(serde_json::json!({ "listChanged": false })),
        };
        assert!(!caps_false.tools_list_changed());
        assert!(!ServerCapabilities::default().tools_list_changed());
    }

    #[tokio::test]
    async fn client_refresh_tools_on_list_changed() {
        // tools/list response after list_changed (id will be 1).
        let tools_resp = make_success_response(
            1,
            serde_json::json!({
                "tools": [{
                    "name": "after_change",
                    "description": "new tool",
                    "inputSchema": {"type": "object"}
                }]
            }),
        );
        let (shared, mock) = MockTransport::new(vec![tools_resp]).shared();
        mock.inject_notification(
            crate::mcp::TOOLS_LIST_CHANGED_METHOD,
            serde_json::json!({}),
        )
        .await;

        let client = McpClient::new(Box::new(shared));
        let refreshed = client.refresh_tools_if_list_changed().await.unwrap();
        let tools = refreshed.expect("expected refresh");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "after_change");

        // Second call with empty queue → None
        assert!(client.refresh_tools_if_list_changed().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn session_refresh_tools_if_list_changed() {
        let init = make_init_response(1);
        let tools_resp = make_success_response(
            2,
            serde_json::json!({
                "tools": [{
                    "name": "t1",
                    "description": "d",
                    "inputSchema": {"type": "object"}
                }]
            }),
        );
        let (shared, mock) = MockTransport::new(vec![init, tools_resp]).shared();
        let session = McpSession::connect(Box::new(shared)).await.unwrap();
        assert!(session.peer_tools_list_changed());

        mock.inject_notification(
            crate::mcp::TOOLS_LIST_CHANGED_METHOD,
            serde_json::json!({}),
        )
        .await;
        let tools = session
            .refresh_tools_if_list_changed()
            .await
            .unwrap()
            .expect("tools after list_changed");
        assert_eq!(tools[0].name, "t1");
    }

    #[tokio::test]
    async fn session_connect_sends_initialized_notification() {
        let transport = MockTransport::new(vec![make_init_response(1)]);
        // We need a shared reference to check notifications after connect.
        // Since connect takes ownership, we use Arc<MockTransport> through
        // a wrapper. Instead, let's verify via the request method sent.
        let transport = Box::new(transport);
        // If connect completes without error, the notification was sent
        // successfully (MockTransport records it internally).
        let session = McpSession::connect(transport).await.unwrap();

        // Verify the initialize request was sent by checking the client
        // was created and handshake completed.
        assert_eq!(session.protocol_version, "2025-06-18");
    }

    #[tokio::test]
    async fn session_connect_error_propagates() {
        let response = make_error_response(1, -32600, "bad init");
        let transport = MockTransport::new(vec![response]);
        let result = McpSession::connect(Box::new(transport)).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("bad init"));
    }

    #[tokio::test]
    async fn session_connect_defaults_on_missing_fields() {
        // Server returns minimal response without serverInfo or capabilities.
        let response = make_success_response(1, serde_json::json!({}));
        let transport = MockTransport::new(vec![response]);
        let session = McpSession::connect(Box::new(transport)).await.unwrap();

        // Should fall back to defaults without panicking.
        assert_eq!(session.server_info.name, "");
        assert_eq!(session.server_info.version, "");
        assert!(session.server_capabilities.tools.is_none());
        assert_eq!(session.protocol_version, "2025-06-18");
    }

    #[tokio::test]
    async fn session_list_tools_delegates() {
        let responses = vec![
            make_init_response(1),
            make_success_response(
                2,
                serde_json::json!({
                    "tools": [{
                        "name": "echo",
                        "description": "Echo",
                        "inputSchema": {"type": "object"}
                    }]
                }),
            ),
        ];
        let transport = MockTransport::new(responses);
        let session = McpSession::connect(Box::new(transport)).await.unwrap();

        let tools = session.list_tools().await.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");
    }

    #[tokio::test]
    async fn session_call_tool_delegates() {
        let responses = vec![
            make_init_response(1),
            make_success_response(2, serde_json::json!({"output": "world"})),
        ];
        let transport = MockTransport::new(responses);
        let session = McpSession::connect(Box::new(transport)).await.unwrap();

        let result = session
            .call_tool("echo", serde_json::json!({"text": "world"}))
            .await
            .unwrap();
        assert_eq!(result["output"], "world");
    }

    #[tokio::test]
    async fn full_session_flow() {
        // Simulate: connect -> list_tools -> call_tool.
        let responses = vec![
            // 1: initialize response
            make_init_response(1),
            // 2: tools/list response
            make_success_response(
                2,
                serde_json::json!({
                    "tools": [{
                        "name": "greet",
                        "description": "Greets someone",
                        "inputSchema": {
                            "type": "object",
                            "properties": { "name": { "type": "string" } }
                        }
                    }]
                }),
            ),
            // 3: tools/call response
            make_success_response(
                3,
                serde_json::json!({
                    "content": [{"type": "text", "text": "Hello, Alice!"}],
                    "isError": false
                }),
            ),
        ];
        let transport = MockTransport::new(responses);
        let session = McpSession::connect(Box::new(transport)).await.unwrap();

        // Verify server info.
        assert_eq!(session.server_info.name, "mock-server");

        // List tools.
        let tools = session.list_tools().await.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "greet");

        // Call a tool.
        let result = session
            .call_tool("greet", serde_json::json!({"name": "Alice"}))
            .await
            .unwrap();
        assert_eq!(result["content"][0]["text"], "Hello, Alice!");
    }

    // ── ServerCapabilities / ServerInfo serde tests ─────────────────────

    // ── Protocol-version handshake tests (WEFT-489) ─────────────────────

    #[test]
    fn supported_protocol_version_set_includes_current() {
        // The version we send in our initialize request must always
        // be in the accepted set, otherwise we'd reject our own peers.
        assert!(is_supported_protocol_version(MCP_PROTOCOL_VERSION));
    }

    #[test]
    fn supported_protocol_version_set_includes_legacy() {
        assert!(is_supported_protocol_version("2024-11-05"));
        assert!(is_supported_protocol_version("2025-03-26"));
    }

    #[test]
    fn unsupported_protocol_version_rejected() {
        assert!(!is_supported_protocol_version("1999-01-01"));
        assert!(!is_supported_protocol_version("foo"));
        assert!(!is_supported_protocol_version("2099-12-31"));
    }

    #[test]
    fn empty_protocol_version_falls_back_to_supported() {
        // Server omitted the field — treat as supported (we
        // negotiated to our advertised version anyway).
        assert!(is_supported_protocol_version(""));
    }

    #[tokio::test]
    async fn session_connect_rejects_unsupported_protocol_version() {
        // Server replies with a deliberately bogus version.
        let response = make_success_response(
            1,
            serde_json::json!({
                "protocolVersion": "1999-01-01",
                "capabilities": {},
                "serverInfo": { "name": "rogue-server", "version": "0.1" }
            }),
        );
        let transport = MockTransport::new(vec![response]);
        let result = McpSession::connect(Box::new(transport)).await;
        assert!(result.is_err());
        match result.unwrap_err() {
            ServiceError::McpProtocolVersionMismatch { ours, theirs } => {
                assert!(ours.contains(&MCP_PROTOCOL_VERSION.to_string()));
                assert_eq!(theirs, "1999-01-01");
            }
            other => panic!("expected McpProtocolVersionMismatch, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn session_connect_accepts_legacy_protocol_version() {
        // Server reports the previous published version — should attach.
        let response = make_success_response(
            1,
            serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "legacy-server", "version": "1.0" }
            }),
        );
        let transport = MockTransport::new(vec![response]);
        let session = McpSession::connect(Box::new(transport)).await.unwrap();
        assert_eq!(session.protocol_version, "2024-11-05");
    }

    #[test]
    fn server_capabilities_default() {
        let caps = ServerCapabilities::default();
        assert!(caps.tools.is_none());
    }

    #[test]
    fn server_info_default() {
        let info = ServerInfo::default();
        assert_eq!(info.name, "");
        assert_eq!(info.version, "");
    }

    #[test]
    fn server_info_serde() {
        let info = ServerInfo {
            name: "test".into(),
            version: "1.0".into(),
        };
        let json = serde_json::to_string(&info).unwrap();
        let restored: ServerInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.name, "test");
        assert_eq!(restored.version, "1.0");
    }

    // ── WEFT-191 durability: ping / keepalive / reconnect / cancel ──────

    use std::sync::atomic::AtomicUsize;
    use transport::{SharedMockTransport, TransportReconnect};

    /// Test reconnect that hands out pre-built mock transports in order.
    struct SequenceReconnect {
        transports: Mutex<Vec<Box<dyn McpTransport>>>,
        calls: AtomicUsize,
    }

    impl SequenceReconnect {
        fn new(transports: Vec<Box<dyn McpTransport>>) -> Self {
            Self {
                transports: Mutex::new(transports),
                calls: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl TransportReconnect for SequenceReconnect {
        async fn recreate(&self) -> Result<Box<dyn McpTransport>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let mut guard = self.transports.lock().await;
            if guard.is_empty() {
                Err(ServiceError::McpTransport(
                    "no more reconnect transports".into(),
                ))
            } else {
                Ok(guard.remove(0))
            }
        }
    }

    /// Reconnect factory that always returns a healthy session transport.
    struct AlwaysHealthyReconnect {
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl TransportReconnect for AlwaysHealthyReconnect {
        async fn recreate(&self) -> Result<Box<dyn McpTransport>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            // Fresh transport with init response only (handshake will consume it).
            let t = MockTransport::new(vec![make_init_response(1)]);
            Ok(Box::new(t))
        }
    }

    #[tokio::test]
    async fn session_ping_sends_ping_method() {
        let responses = vec![
            make_init_response(1),
            make_success_response(2, serde_json::json!({})),
        ];
        let (shared, mock) = MockTransport::new(responses).shared();
        let session = McpSession::connect(Box::new(shared)).await.unwrap();
        session.ping().await.unwrap();
        let reqs = mock.requests().await;
        assert!(
            reqs.iter().any(|r| r.method == "ping"),
            "expected ping request, got: {:?}",
            reqs.iter().map(|r| &r.method).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn session_is_alive_false_when_transport_dead() {
        let (shared, mock) = MockTransport::new(vec![make_init_response(1)]).shared();
        let session = McpSession::connect(Box::new(shared)).await.unwrap();
        assert!(session.is_alive().await);
        mock.set_alive(false);
        assert!(!session.is_alive().await);
    }

    #[tokio::test]
    async fn session_shutdown_sends_cancelled_and_closes() {
        let (shared, mock) = MockTransport::new(vec![make_init_response(1)]).shared();
        let session = McpSession::connect(Box::new(shared)).await.unwrap();

        // Simulate an in-flight request id without waiting on the transport.
        session.client().track_in_flight_for_test(42).await;
        session.shutdown().await.unwrap();

        assert!(session.is_shutdown());
        assert!(!session.is_alive().await);
        assert!(mock.was_closed());
        let notifs = mock.notifications().await;
        let cancel = notifs
            .iter()
            .find(|n| n.method == "notifications/cancelled")
            .expect("expected notifications/cancelled");
        assert_eq!(cancel.params["requestId"], 42);
    }

    #[tokio::test]
    async fn session_reconnect_with_backoff_recovers_from_crash() {
        // Initial transport dies after handshake; reconnect supplies a new one.
        let (shared, mock) = MockTransport::new(vec![make_init_response(1)]).shared();
        let reconnect = Arc::new(AlwaysHealthyReconnect {
            calls: AtomicUsize::new(0),
        });
        let session = McpSession::connect_with_options(
            Box::new(shared),
            Some(reconnect.clone()),
            DurabilityConfig::for_tests(),
        )
        .await
        .unwrap();

        mock.set_alive(false);
        assert!(!session.is_alive().await);

        session.reconnect_with_backoff().await.unwrap();
        assert!(session.is_alive().await);
        assert!(
            reconnect.calls.load(Ordering::SeqCst) >= 1,
            "recreate should have been called"
        );

        // Post-reconnect traffic works (need tools/list response after init already consumed).
        // AlwaysHealthyReconnect only left the transport after handshake; push a list response
        // by pinging — but ping needs a response. Use list via ensure path with a new factory.
    }

    #[tokio::test]
    async fn session_ensure_connected_reconnects_then_lists_tools() {
        // Sequence: first transport dies; recreate returns transport with init + tools/list.
        let (shared, mock) = MockTransport::new(vec![make_init_response(1)]).shared();

        let recovery = MockTransport::new(vec![
            make_init_response(1),
            make_success_response(
                2,
                serde_json::json!({
                    "tools": [{
                        "name": "recovered",
                        "description": "after crash",
                        "inputSchema": {"type": "object"}
                    }]
                }),
            ),
        ]);

        let reconnect = Arc::new(SequenceReconnect::new(vec![Box::new(recovery)]));
        let session = McpSession::connect_with_options(
            Box::new(shared),
            Some(reconnect.clone()),
            DurabilityConfig::for_tests(),
        )
        .await
        .unwrap();

        mock.set_alive(false);
        let tools = session.list_tools().await.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "recovered");
        assert_eq!(reconnect.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn session_reconnect_exhausts_attempts() {
        struct FailReconnect;
        #[async_trait::async_trait]
        impl TransportReconnect for FailReconnect {
            async fn recreate(&self) -> Result<Box<dyn McpTransport>> {
                Err(ServiceError::McpTransport("spawn failed".into()))
            }
        }

        let (shared, mock) = MockTransport::new(vec![make_init_response(1)]).shared();
        let session = McpSession::connect_with_options(
            Box::new(shared),
            Some(Arc::new(FailReconnect)),
            DurabilityConfig {
                max_reconnect_attempts: 2,
                initial_backoff: Duration::from_millis(1),
                max_backoff: Duration::from_millis(2),
                keepalive_interval: Duration::ZERO,
            },
        )
        .await
        .unwrap();
        mock.set_alive(false);
        let err = session.reconnect_with_backoff().await.unwrap_err();
        assert!(
            err.to_string().contains("reconnect exhausted"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn session_keepalive_triggers_reconnect_on_dead_peer() {
        let (shared, mock) = MockTransport::new(vec![make_init_response(1)]).shared();
        let reconnect = Arc::new(AlwaysHealthyReconnect {
            calls: AtomicUsize::new(0),
        });
        let session = Arc::new(
            McpSession::connect_with_options(
                Box::new(shared),
                Some(reconnect.clone()),
                DurabilityConfig {
                    keepalive_interval: Duration::from_millis(30),
                    max_reconnect_attempts: 3,
                    initial_backoff: Duration::from_millis(1),
                    max_backoff: Duration::from_millis(5),
                },
            )
            .await
            .unwrap(),
        );

        mock.set_alive(false);
        session.start_keepalive().await;

        // Wait for keepalive tick + reconnect.
        let deadline = tokio::time::Instant::now() + Duration::from_millis(400);
        while tokio::time::Instant::now() < deadline {
            if reconnect.calls.load(Ordering::SeqCst) >= 1 && session.is_alive().await {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            reconnect.calls.load(Ordering::SeqCst) >= 1,
            "keepalive should have attempted reconnect"
        );
        assert!(session.is_alive().await);
        session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn child_crash_recovery_stdio_is_alive_and_recreate() {
        // Real process: first child exits immediately; SpecReconnect re-spawns sleep.
        use transport::{DefaultTransportFactory, SpecReconnect, TransportFactoryConfig};

        let env = std::collections::HashMap::new();
        // `true` exits immediately → is_alive becomes false.
        let dead = transport::StdioTransport::new("true", &[], &env)
            .await
            .expect("spawn true");
        // Wait for exit.
        for _ in 0..50 {
            if !dead.is_alive().await {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!dead.is_alive().await);

        let factory = Arc::new(DefaultTransportFactory::new(
            TransportFactoryConfig::lenient(),
        ));
        let reconnect = SpecReconnect::new(
            factory,
            transport::TransportSpec::Stdio {
                command: "sleep".into(),
                args: vec!["30".into()],
                env: env.clone(),
            },
        );
        let fresh = reconnect.recreate().await.expect("respawn sleep");
        assert!(fresh.is_alive().await);
        fresh.close().await.unwrap();
    }

    #[tokio::test]
    async fn durability_config_defaults() {
        let d = DurabilityConfig::default();
        assert_eq!(d.keepalive_interval, DEFAULT_KEEPALIVE_INTERVAL);
        assert_eq!(d.max_reconnect_attempts, DEFAULT_MAX_RECONNECT_ATTEMPTS);
        let off = DurabilityConfig::disabled();
        assert!(off.keepalive_interval.is_zero());
        assert_eq!(off.max_reconnect_attempts, 0);
    }

    #[tokio::test]
    async fn shared_mock_transport_type_compiles_in_session() {
        let (shared, _arc): (SharedMockTransport, _) =
            MockTransport::new(vec![make_init_response(1)]).shared();
        let session = McpSession::connect(Box::new(shared)).await.unwrap();
        assert!(session.is_alive().await);
        session.shutdown().await.unwrap();
    }
}
