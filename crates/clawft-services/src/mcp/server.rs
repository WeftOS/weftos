//! MCP server shell that handles JSON-RPC protocol over newline-delimited
//! JSON streams.
//!
//! [`McpServerShell`] is generic over `AsyncBufRead + AsyncWrite` so it
//! can be driven by stdio, TCP, or in-memory buffers for testing.

use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

use super::ToolDefinition;
use super::composite::CompositeToolProvider;
use super::middleware::{Middleware, ToolCallRequest};
use super::provider::CallToolResult;
use super::stateless;

// ── Constants ───────────────────────────────────────────────────────────

/// Re-use the canonical protocol version from the MCP module.
const PROTOCOL_VERSION: &str = super::MCP_PROTOCOL_VERSION;
const SERVER_NAME: &str = "clawft";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// JSON-RPC error codes.
const METHOD_NOT_FOUND: i32 = -32601;
const NOT_INITIALIZED: i32 = -32002;
const INVALID_REQUEST: i32 = -32600;

// ── McpServerShell ─────────────────────────────────────────────────────

/// An MCP server that reads newline-delimited JSON-RPC from a reader and
/// writes responses to a writer.
///
/// Handles the `initialize` handshake, `tools/list`, `tools/call`, and
/// `notifications/initialized` methods. Requests carrying
/// `_meta["io.modelcontextprotocol/protocolVersion"]` (spec 2026-07-28), and
/// `server/discover`, are served statelessly without a handshake; see
/// [`super::stateless`]. Unknown methods receive a
/// `-32601 Method not found` error. Requests sent before `initialize`
/// receive a `-32002 Server not initialized` error.
///
/// # Tool list changes (WEFT-200)
///
/// `initialize` advertises `capabilities.tools.listChanged: true`. When
/// the composite provider registry changes after handshake, hosts should
/// call [`Self::emit_tools_list_changed`] (or write
/// [`tools_list_changed_notification`]) so clients re-fetch `tools/list`.
pub struct McpServerShell {
    provider: CompositeToolProvider,
    middlewares: Vec<Box<dyn Middleware>>,
    initialized: bool,
}

/// JSON-RPC notification body for `notifications/tools/list_changed` (WEFT-200).
pub fn tools_list_changed_notification() -> Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "method": super::TOOLS_LIST_CHANGED_METHOD,
        "params": {}
    })
}

impl McpServerShell {
    /// Create a new server shell wrapping the given composite provider.
    pub fn new(provider: CompositeToolProvider) -> Self {
        Self {
            provider,
            middlewares: Vec::new(),
            initialized: false,
        }
    }

    /// Add a middleware to the processing pipeline.
    pub fn add_middleware(&mut self, middleware: Box<dyn Middleware>) {
        self.middlewares.push(middleware);
    }

    /// Mutable access to the composite provider (hot-reload / tests).
    pub fn provider_mut(&mut self) -> &mut CompositeToolProvider {
        &mut self.provider
    }

    /// Emit `notifications/tools/list_changed` on the writer (WEFT-200).
    ///
    /// Call after mutating the tool registry (e.g. `provider_mut().register(...)`)
    /// so clients that respect `listChanged` re-fetch `tools/list`.
    pub async fn emit_tools_list_changed<W: AsyncWrite + Unpin>(
        &self,
        writer: &mut W,
    ) -> std::io::Result<()> {
        write_response(writer, &tools_list_changed_notification()).await
    }

    /// Whether the initialize handshake has completed.
    pub fn is_initialized(&self) -> bool {
        self.initialized
    }

    /// Handle a single JSON-RPC message (request or notification).
    ///
    /// Returns `Some(response)` for requests, `None` for notifications.
    /// Used by both the stdio loop and HTTP/SSE serve (WEFT-696).
    ///
    /// `session_scopes` optionally further restricts tools beyond the
    /// shell middleware (session capability tokens — WEFT-697).
    pub async fn handle_message(
        &mut self,
        msg: Value,
        session_scopes: Option<&super::session_cap::SessionScopes>,
    ) -> Option<Value> {
        let method = msg.get("method").and_then(|v| v.as_str()).unwrap_or("");
        let id = msg.get("id").cloned();
        let params = msg
            .get("params")
            .cloned()
            .unwrap_or_else(|| Value::Object(Default::default()));

        // Notifications have no id -- never send a response.
        let is_notification = id.is_none();

        // Stateless (2026-07-28) request: version in `_meta`, no handshake.
        // `initialize` always selects the legacy path; a bare `server/discover`
        // is the stdio compatibility probe and is answered either way.
        if method != "initialize" {
            let version = stateless::declared_version(&msg);
            if version.is_some() || method == "server/discover" {
                return self
                    .handle_modern(version, method, id, &params, session_scopes)
                    .await;
            }
        }

        match method {
            "initialize" => {
                self.initialized = true;
                let result = serde_json::json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {
                        "tools": { "listChanged": true }
                    },
                    "serverInfo": {
                        "name": SERVER_NAME,
                        "version": SERVER_VERSION
                    }
                });
                id.map(|id| make_success_response(id, result))
            }

            "notifications/initialized" => None,

            "ping" => {
                // MCP keepalive (WEFT-191 / remote health).
                if !self.initialized && !is_notification {
                    return Some(make_error_response(
                        id.unwrap_or(Value::Null),
                        NOT_INITIALIZED,
                        "Server not initialized",
                    ));
                }
                id.map(|id| make_success_response(id, serde_json::json!({})))
            }

            _ if !self.initialized => {
                if is_notification {
                    None
                } else {
                    Some(make_error_response(
                        id.unwrap_or(Value::Null),
                        NOT_INITIALIZED,
                        "Server not initialized",
                    ))
                }
            }

            "tools/list" => {
                let result = self.tools_list(session_scopes).await;
                id.map(|id| make_success_response(id, result))
            }

            "tools/call" => {
                let result = self.tools_call(&params, session_scopes).await;
                id.map(|id| make_success_response(id, result))
            }

            _ => {
                if is_notification {
                    None
                } else {
                    Some(make_error_response(
                        id.unwrap_or(Value::Null),
                        METHOD_NOT_FOUND,
                        &format!("Method not found: {method}"),
                    ))
                }
            }
        }
    }

    /// `tools/list` result body, shared by the legacy and stateless paths.
    ///
    /// Sorted by name so the list is deterministic (spec 2026-07-28 asks for
    /// it to help client and prompt caching).
    async fn tools_list(
        &self,
        session_scopes: Option<&super::session_cap::SessionScopes>,
    ) -> Value {
        let mut tools = self.provider.list_tools_all();

        // Apply middleware filter_tools in order.
        for mw in &self.middlewares {
            tools = mw.filter_tools(tools).await;
        }

        // Session capability scopes (WEFT-697).
        if let Some(scopes) = session_scopes {
            tools.retain(|t| scopes.allows_tool(&t.name));
        }

        tools.sort_by(|a, b| a.name.cmp(&b.name));
        serde_json::json!({ "tools": serialize_tools(&tools) })
    }

    /// `tools/call` result body, shared by the legacy and stateless paths.
    async fn tools_call(
        &self,
        params: &Value,
        session_scopes: Option<&super::session_cap::SessionScopes>,
    ) -> Value {
        let name = params
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let args = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| Value::Object(Default::default()));

        // Session capability gate before middleware (WEFT-697).
        if let Some(scopes) = session_scopes
            && !scopes.allows_tool(&name)
        {
            let err_result = CallToolResult::error(format!(
                "permission denied: session capability lacks tool '{name}'"
            ));
            return serde_json::to_value(&err_result).unwrap_or(Value::Null);
        }

        // Workspace root check for common path args (best-effort).
        if let Some(scopes) = session_scopes {
            for key in ["path", "file", "file_path", "dir", "directory"] {
                if let Some(p) = args.get(key).and_then(|v| v.as_str()) {
                    let path = std::path::Path::new(p);
                    if !scopes.allows_workspace_path(path) {
                        let err_result = CallToolResult::error(format!(
                            "permission denied: path '{p}' outside session workspace roots"
                        ));
                        return serde_json::to_value(&err_result).unwrap_or(Value::Null);
                    }
                }
            }
        }

        let mut request = ToolCallRequest {
            name: name.clone(),
            args,
        };

        // Apply middleware before_call hooks.
        let mut mw_error = None;
        for mw in &self.middlewares {
            match mw.before_call(request).await {
                Ok(r) => request = r,
                Err(e) => {
                    mw_error = Some(e);
                    // Reconstruct a minimal request for the error path.
                    request = ToolCallRequest {
                        name,
                        args: Value::Object(Default::default()),
                    };
                    break;
                }
            }
        }

        let call_result = if let Some(err) = mw_error {
            Err(err)
        } else {
            self.provider
                .call_tool(&request.name, request.args.clone())
                .await
        };

        match call_result {
            Ok(mut result) => {
                // Apply middleware after_call hooks.
                for mw in &self.middlewares {
                    match mw.after_call(&request, result).await {
                        Ok(r) => result = r,
                        Err(e) => {
                            result = CallToolResult::error(e.to_string());
                            break;
                        }
                    }
                }
                serde_json::to_value(&result).unwrap_or(Value::Null)
            }
            Err(e) => {
                let err_result = CallToolResult::error(e.to_string());
                serde_json::to_value(&err_result).unwrap_or(Value::Null)
            }
        }
    }

    /// Serve one stateless (spec 2026-07-28) message.
    ///
    /// No handshake and no shell state: the era is read from `_meta`, so
    /// concurrent modern and legacy clients cannot affect each other.
    async fn handle_modern(
        &self,
        version: Option<&str>,
        method: &str,
        id: Option<Value>,
        params: &Value,
        session_scopes: Option<&super::session_cap::SessionScopes>,
    ) -> Option<Value> {
        // Notifications get no reply.
        let id = id?;
        if let Some(v) = version
            && !stateless::is_supported(v)
        {
            return Some(stateless::unsupported_version_error(id, v));
        }
        let info = stateless::server_info(SERVER_NAME, SERVER_VERSION);
        let (mut result, cacheable) = match method {
            "server/discover" => {
                return Some(make_success_response(
                    id,
                    stateless::discover_result(info, None),
                ));
            }
            "tools/list" => (self.tools_list(session_scopes).await, true),
            "tools/call" => (self.tools_call(params, session_scopes).await, false),
            // `ping`, `logging/setLevel` and friends were removed in 2026-07-28.
            _ => {
                return Some(make_error_response(
                    id,
                    METHOD_NOT_FOUND,
                    &format!("Method not found: {method}"),
                ));
            }
        };
        stateless::decorate_result(&mut result, info, cacheable);
        Some(make_success_response(id, result))
    }

    /// Run the server loop, reading lines from `reader` and writing
    /// responses to `writer` until EOF.
    pub async fn run<R, W>(&mut self, reader: R, mut writer: W) -> std::io::Result<()>
    where
        R: AsyncBufRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let mut lines = reader.lines();

        while let Some(line) = lines.next_line().await? {
            let line = line.trim().to_string();
            if line.is_empty() {
                continue;
            }

            // Parse the incoming JSON.
            let msg: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => {
                    let resp = make_error_response(Value::Null, INVALID_REQUEST, "Parse error");
                    write_response(&mut writer, &resp).await?;
                    continue;
                }
            };

            // stdio: implicit owner capability (no extra scope filter).
            if let Some(resp) = self.handle_message(msg, None).await {
                write_response(&mut writer, &resp).await?;
            }
        }

        Ok(())
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────

fn make_success_response(id: Value, result: Value) -> Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result
    })
}

fn make_error_response(id: Value, code: i32, message: &str) -> Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message
        }
    })
}

fn serialize_tools(tools: &[ToolDefinition]) -> Value {
    serde_json::to_value(tools).unwrap_or_else(|_| Value::Array(vec![]))
}

async fn write_response<W: AsyncWrite + Unpin>(
    writer: &mut W,
    response: &Value,
) -> std::io::Result<()> {
    let mut line = serde_json::to_string(response).map_err(std::io::Error::other)?;
    line.push('\n');
    writer.write_all(line.as_bytes()).await?;
    writer.flush().await?;
    Ok(())
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::super::composite::CompositeToolProvider;
    use super::super::middleware::{Middleware, ToolCallRequest};
    use super::super::provider::{ContentBlock, ToolError, ToolProvider};
    use super::*;
    use async_trait::async_trait;
    use serde_json::json;
    use std::io::Cursor;

    // ── Test helpers ────────────────────────────────────────────────────

    /// Build a JSON-RPC request line (with id).
    fn request_line(id: u64, method: &str, params: Value) -> String {
        let req = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        format!("{}\n", serde_json::to_string(&req).unwrap())
    }

    /// Build a notification line (no id).
    fn notification_line(method: &str, params: Value) -> String {
        let req = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        format!("{}\n", serde_json::to_string(&req).unwrap())
    }

    /// Parse response lines from the output buffer.
    fn parse_responses(output: &[u8]) -> Vec<Value> {
        let text = String::from_utf8_lossy(output);
        text.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).expect("invalid JSON response"))
            .collect()
    }

    /// Standard initialize request line.
    fn init_line(id: u64) -> String {
        request_line(
            id,
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "test", "version": "0.1" }
            }),
        )
    }

    // ── Mock provider ───────────────────────────────────────────────────

    struct EchoProvider;

    #[async_trait]
    impl ToolProvider for EchoProvider {
        fn namespace(&self) -> &str {
            "echo"
        }

        fn list_tools(&self) -> Vec<ToolDefinition> {
            vec![ToolDefinition {
                name: "say".to_string(),
                description: "Echoes text".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": { "text": { "type": "string" } }
                }),
            }]
        }

        async fn call_tool(&self, name: &str, args: Value) -> Result<CallToolResult, ToolError> {
            if name == "say" {
                let text = args
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("(empty)");
                Ok(CallToolResult::text(text))
            } else {
                Err(ToolError::NotFound(name.to_string()))
            }
        }
    }

    fn make_server() -> McpServerShell {
        let mut provider = CompositeToolProvider::new();
        provider.register(Box::new(EchoProvider));
        McpServerShell::new(provider)
    }

    // ── Protocol tests ──────────────────────────────────────────────────

    #[tokio::test]
    async fn initialize_handshake() {
        let mut server = make_server();
        let input = init_line(1);

        let reader = Cursor::new(input.into_bytes());
        let mut output = Vec::new();

        server.run(reader, &mut output).await.unwrap();

        let responses = parse_responses(&output);
        assert_eq!(responses.len(), 1);
        let resp = &responses[0];
        assert_eq!(resp["id"], 1);
        assert_eq!(resp["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert!(resp["result"]["capabilities"]["tools"].is_object());
        assert_eq!(resp["result"]["serverInfo"]["name"], SERVER_NAME);
    }

    #[tokio::test]
    async fn not_initialized_rejection() {
        let mut server = make_server();
        let input = request_line(1, "tools/list", json!({}));

        let reader = Cursor::new(input.into_bytes());
        let mut output = Vec::new();

        server.run(reader, &mut output).await.unwrap();

        let responses = parse_responses(&output);
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0]["error"]["code"], NOT_INITIALIZED);
    }

    #[tokio::test]
    async fn tools_list_returns_tools() {
        let mut server = make_server();
        let mut input = init_line(1);
        input.push_str(&notification_line("notifications/initialized", json!({})));
        input.push_str(&request_line(2, "tools/list", json!({})));

        let reader = Cursor::new(input.into_bytes());
        let mut output = Vec::new();

        server.run(reader, &mut output).await.unwrap();

        let responses = parse_responses(&output);
        // init response + tools/list response (notification produces none).
        assert_eq!(responses.len(), 2);

        let list_resp = &responses[1];
        assert_eq!(list_resp["id"], 2);
        let tools = list_resp["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "echo__say");
    }

    #[tokio::test]
    async fn tools_call_routes_correctly() {
        let mut server = make_server();
        let mut input = init_line(1);
        input.push_str(&notification_line("notifications/initialized", json!({})));
        input.push_str(&request_line(
            2,
            "tools/call",
            json!({
                "name": "echo__say",
                "arguments": { "text": "hello world" }
            }),
        ));

        let reader = Cursor::new(input.into_bytes());
        let mut output = Vec::new();

        server.run(reader, &mut output).await.unwrap();

        let responses = parse_responses(&output);
        assert_eq!(responses.len(), 2);

        let call_resp = &responses[1];
        assert_eq!(call_resp["id"], 2);
        let content = call_resp["result"]["content"].as_array().unwrap();
        assert_eq!(content[0]["text"], "hello world");
    }

    #[tokio::test]
    async fn tools_call_not_found() {
        let mut server = make_server();
        let mut input = init_line(1);
        input.push_str(&request_line(
            2,
            "tools/call",
            json!({
                "name": "nonexistent__tool",
                "arguments": {}
            }),
        ));

        let reader = Cursor::new(input.into_bytes());
        let mut output = Vec::new();

        server.run(reader, &mut output).await.unwrap();

        let responses = parse_responses(&output);
        assert_eq!(responses.len(), 2);

        let call_resp = &responses[1];
        // Tool-not-found is returned as a CallToolResult with is_error=true.
        assert!(call_resp["result"]["isError"].as_bool().unwrap_or(false));
    }

    #[tokio::test]
    async fn unknown_method_returns_error() {
        let mut server = make_server();
        let mut input = init_line(1);
        input.push_str(&request_line(2, "completions/complete", json!({})));

        let reader = Cursor::new(input.into_bytes());
        let mut output = Vec::new();

        server.run(reader, &mut output).await.unwrap();

        let responses = parse_responses(&output);
        assert_eq!(responses.len(), 2);

        let resp = &responses[1];
        assert_eq!(resp["error"]["code"], METHOD_NOT_FOUND);
        assert!(
            resp["error"]["message"]
                .as_str()
                .unwrap()
                .contains("completions/complete")
        );
    }

    #[test]
    fn tools_list_changed_notification_wire_format() {
        let n = tools_list_changed_notification();
        assert_eq!(n["jsonrpc"], "2.0");
        assert_eq!(n["method"], "notifications/tools/list_changed");
        assert!(n.get("id").is_none());
        assert!(n["params"].is_object());
    }

    #[tokio::test]
    async fn initialize_advertises_tools_list_changed_true() {
        let mut server = McpServerShell::new(CompositeToolProvider::new());
        let input = init_line(1);
        let reader = Cursor::new(input.into_bytes());
        let mut output = Vec::new();
        server.run(reader, &mut output).await.unwrap();
        let resps = parse_responses(&output);
        assert_eq!(resps.len(), 1);
        assert_eq!(
            resps[0]["result"]["capabilities"]["tools"]["listChanged"],
            true
        );
    }

    #[tokio::test]
    async fn emit_tools_list_changed_writes_notification() {
        let server = McpServerShell::new(CompositeToolProvider::new());
        let mut output = Vec::new();
        server.emit_tools_list_changed(&mut output).await.unwrap();
        let resps = parse_responses(&output);
        assert_eq!(resps.len(), 1);
        assert_eq!(resps[0]["method"], "notifications/tools/list_changed");
        assert!(resps[0].get("id").is_none());
    }

    #[tokio::test]
    async fn notification_produces_no_response() {
        let mut server = make_server();
        let mut input = init_line(1);
        input.push_str(&notification_line("notifications/initialized", json!({})));

        let reader = Cursor::new(input.into_bytes());
        let mut output = Vec::new();

        server.run(reader, &mut output).await.unwrap();

        let responses = parse_responses(&output);
        // Only the initialize response.
        assert_eq!(responses.len(), 1);
    }

    #[tokio::test]
    async fn malformed_json_returns_parse_error() {
        let mut server = make_server();
        let input = "not valid json\n".to_string();

        let reader = Cursor::new(input.into_bytes());
        let mut output = Vec::new();

        server.run(reader, &mut output).await.unwrap();

        let responses = parse_responses(&output);
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0]["error"]["code"], INVALID_REQUEST);
    }

    #[tokio::test]
    async fn empty_lines_are_skipped() {
        let mut server = make_server();
        let mut input = String::new();
        input.push('\n');
        input.push_str("   \n");
        input.push_str(&init_line(1));

        let reader = Cursor::new(input.into_bytes());
        let mut output = Vec::new();

        server.run(reader, &mut output).await.unwrap();

        let responses = parse_responses(&output);
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0]["id"], 1);
    }

    // ── Middleware integration tests ────────────────────────────────────

    /// Middleware that uppercases all tool descriptions.
    struct UppercaseMiddleware;

    #[async_trait]
    impl Middleware for UppercaseMiddleware {
        async fn filter_tools(&self, tools: Vec<ToolDefinition>) -> Vec<ToolDefinition> {
            tools
                .into_iter()
                .map(|mut t| {
                    t.description = t.description.to_uppercase();
                    t
                })
                .collect()
        }
    }

    #[tokio::test]
    async fn middleware_filter_tools_applied() {
        let mut provider = CompositeToolProvider::new();
        provider.register(Box::new(EchoProvider));
        let mut server = McpServerShell::new(provider);
        server.add_middleware(Box::new(UppercaseMiddleware));

        let mut input = init_line(1);
        input.push_str(&request_line(2, "tools/list", json!({})));

        let reader = Cursor::new(input.into_bytes());
        let mut output = Vec::new();

        server.run(reader, &mut output).await.unwrap();

        let responses = parse_responses(&output);
        let tools = responses[1]["result"]["tools"].as_array().unwrap();
        assert_eq!(tools[0]["description"], "ECHOES TEXT");
    }

    /// Middleware that rejects calls to a specific tool.
    struct BlockingMiddleware {
        blocked_tool: String,
    }

    #[async_trait]
    impl Middleware for BlockingMiddleware {
        async fn before_call(
            &self,
            request: ToolCallRequest,
        ) -> Result<ToolCallRequest, ToolError> {
            if request.name == self.blocked_tool {
                Err(ToolError::ExecutionFailed("tool is blocked".into()))
            } else {
                Ok(request)
            }
        }
    }

    #[tokio::test]
    async fn middleware_before_call_can_reject() {
        let mut provider = CompositeToolProvider::new();
        provider.register(Box::new(EchoProvider));
        let mut server = McpServerShell::new(provider);
        server.add_middleware(Box::new(BlockingMiddleware {
            blocked_tool: "echo__say".to_string(),
        }));

        let mut input = init_line(1);
        input.push_str(&request_line(
            2,
            "tools/call",
            json!({
                "name": "echo__say",
                "arguments": { "text": "blocked" }
            }),
        ));

        let reader = Cursor::new(input.into_bytes());
        let mut output = Vec::new();

        server.run(reader, &mut output).await.unwrap();

        let responses = parse_responses(&output);
        let call_resp = &responses[1];
        assert!(call_resp["result"]["isError"].as_bool().unwrap_or(false));
        let content = call_resp["result"]["content"].as_array().unwrap();
        assert!(content[0]["text"].as_str().unwrap().contains("blocked"));
    }

    /// Middleware that appends a suffix content block.
    struct SuffixMiddleware;

    #[async_trait]
    impl Middleware for SuffixMiddleware {
        async fn after_call(
            &self,
            _request: &ToolCallRequest,
            mut result: CallToolResult,
        ) -> Result<CallToolResult, ToolError> {
            result.content.push(ContentBlock::Text {
                text: "[suffix]".into(),
            });
            Ok(result)
        }
    }

    #[tokio::test]
    async fn middleware_after_call_applied() {
        let mut provider = CompositeToolProvider::new();
        provider.register(Box::new(EchoProvider));
        let mut server = McpServerShell::new(provider);
        server.add_middleware(Box::new(SuffixMiddleware));

        let mut input = init_line(1);
        input.push_str(&request_line(
            2,
            "tools/call",
            json!({
                "name": "echo__say",
                "arguments": { "text": "hello" }
            }),
        ));

        let reader = Cursor::new(input.into_bytes());
        let mut output = Vec::new();

        server.run(reader, &mut output).await.unwrap();

        let responses = parse_responses(&output);
        let content = responses[1]["result"]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["text"], "hello");
        assert_eq!(content[1]["text"], "[suffix]");
    }

    // ── Full session integration test ───────────────────────────────────

    #[tokio::test]
    async fn full_session_flow() {
        let mut server = make_server();

        let mut input = String::new();
        // 1. Initialize.
        input.push_str(&init_line(1));
        // 2. Initialized notification.
        input.push_str(&notification_line("notifications/initialized", json!({})));
        // 3. List tools.
        input.push_str(&request_line(2, "tools/list", json!({})));
        // 4. Call a tool.
        input.push_str(&request_line(
            3,
            "tools/call",
            json!({
                "name": "echo__say",
                "arguments": { "text": "integration test" }
            }),
        ));
        // 5. Unknown method.
        input.push_str(&request_line(4, "unknown/method", json!({})));

        let reader = Cursor::new(input.into_bytes());
        let mut output = Vec::new();

        server.run(reader, &mut output).await.unwrap();

        let responses = parse_responses(&output);
        // 4 responses (init, list, call, unknown). Notification produces none.
        assert_eq!(responses.len(), 4);

        // 1: Initialize OK.
        assert!(responses[0]["result"]["protocolVersion"].is_string());

        // 2: tools/list.
        let tools = responses[1]["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);

        // 3: tools/call.
        let content = responses[2]["result"]["content"].as_array().unwrap();
        assert_eq!(content[0]["text"], "integration test");

        // 4: Unknown method error.
        assert_eq!(responses[3]["error"]["code"], METHOD_NOT_FOUND);
    }

    // ── Stateless (2026-07-28) ──────────────────────────────────────────

    fn modern_meta() -> Value {
        json!({ "_meta": { "io.modelcontextprotocol/protocolVersion": "2026-07-28" } })
    }

    fn modern_params(extra: Value) -> Value {
        let mut p = modern_meta();
        for (k, v) in extra.as_object().unwrap() {
            p[k] = v.clone();
        }
        p
    }

    async fn run_lines(server: &mut McpServerShell, input: String) -> Vec<Value> {
        let mut output = Vec::new();
        server
            .run(Cursor::new(input.into_bytes()), &mut output)
            .await
            .unwrap();
        parse_responses(&output)
    }

    #[tokio::test]
    async fn stateless_list_and_call_without_initialize() {
        let mut server = make_server();
        let mut input = request_line(1, "tools/list", modern_meta());
        input.push_str(&request_line(
            2,
            "tools/call",
            modern_params(json!({ "name": "echo__say", "arguments": { "text": "hi" } })),
        ));
        let r = run_lines(&mut server, input).await;
        assert_eq!(r.len(), 2);
        assert_eq!(r[0]["result"]["resultType"], "complete");
        assert_eq!(r[0]["result"]["cacheScope"], "private");
        assert!(r[0]["result"]["ttlMs"].is_u64());
        assert_eq!(
            r[0]["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
            SERVER_NAME
        );
        assert_eq!(r[0]["result"]["tools"][0]["name"], "echo__say");
        assert_eq!(r[1]["result"]["resultType"], "complete");
        assert!(r[1]["result"].get("ttlMs").is_none());
        assert_eq!(r[1]["result"]["content"][0]["text"], "hi");
        // Stateless traffic never flips the legacy handshake flag.
        assert!(!server.is_initialized());
    }

    #[tokio::test]
    async fn discover_with_and_without_meta() {
        let mut server = make_server();
        let mut input = request_line(1, "server/discover", json!({}));
        input.push_str(&request_line(2, "server/discover", modern_meta()));
        let r = run_lines(&mut server, input).await;
        for resp in &r {
            let res = &resp["result"];
            assert_eq!(res["supportedVersions"], json!(["2026-07-28"]));
            assert!(res["capabilities"]["tools"].is_object());
            assert_eq!(res["resultType"], "complete");
            assert!(res["ttlMs"].is_u64());
        }
        assert!(!server.is_initialized());
    }

    #[tokio::test]
    async fn unsupported_version_is_refused_with_supported_list() {
        let mut server = make_server();
        let params = json!({ "_meta": { "io.modelcontextprotocol/protocolVersion": "1900-01-01" } });
        for method in ["tools/list", "server/discover"] {
            let r = run_lines(&mut server, request_line(1, method, params.clone())).await;
            assert_eq!(r[0]["error"]["code"], stateless::UNSUPPORTED_PROTOCOL_VERSION);
            assert_eq!(r[0]["error"]["data"]["supported"], json!(["2026-07-28"]));
            assert_eq!(r[0]["error"]["data"]["requested"], "1900-01-01");
        }
    }

    #[tokio::test]
    async fn stateless_ping_is_removed() {
        let mut server = make_server();
        let r = run_lines(&mut server, request_line(1, "ping", modern_meta())).await;
        assert_eq!(r[0]["error"]["code"], METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn stateless_notification_gets_no_reply() {
        let mut server = make_server();
        let r = run_lines(&mut server, notification_line("tools/list", modern_meta())).await;
        assert!(r.is_empty());
    }

    #[tokio::test]
    async fn legacy_results_stay_unchanged_after_stateless_traffic() {
        let mut server = make_server();
        let mut input = request_line(1, "tools/list", modern_meta());
        input.push_str(&init_line(2));
        input.push_str(&request_line(3, "tools/list", json!({})));
        let r = run_lines(&mut server, input).await;
        assert_eq!(r[1]["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert!(r[2]["result"].get("resultType").is_none());
        assert!(r[2]["result"].get("ttlMs").is_none());
        // Legacy gate still refuses on a fresh shell.
        let mut fresh = make_server();
        let r = run_lines(&mut fresh, request_line(1, "tools/list", json!({}))).await;
        assert_eq!(r[0]["error"]["code"], NOT_INITIALIZED);
    }

    #[tokio::test]
    async fn tools_list_is_sorted_by_name() {
        struct Two;
        #[async_trait]
        impl ToolProvider for Two {
            fn namespace(&self) -> &str {
                ""
            }
            fn list_tools(&self) -> Vec<ToolDefinition> {
                ["zeta", "alpha", "mid"]
                    .iter()
                    .map(|n| ToolDefinition {
                        name: (*n).into(),
                        description: String::new(),
                        input_schema: json!({"type": "object"}),
                    })
                    .collect()
            }
            async fn call_tool(&self, n: &str, _: Value) -> Result<CallToolResult, ToolError> {
                Err(ToolError::NotFound(n.into()))
            }
        }
        let mut c = CompositeToolProvider::new();
        c.register(Box::new(Two));
        let mut server = McpServerShell::new(c);
        let r = run_lines(&mut server, request_line(1, "tools/list", modern_meta())).await;
        let names: Vec<&str> = r[0]["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["alpha", "mid", "zeta"]);
    }
}
