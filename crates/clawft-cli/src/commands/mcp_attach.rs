//! Attach façade for `weft mcp-server --attach` (ADR-076 C3 / WEFT-701).
//!
//! Control-plane MCP tools (`status`, `agent_list`, `agent_spawn`,
//! `agent_stop`) dispatch to a live kernel/weave instance via
//! [`clawft_rpc::DaemonClient`] instead of a standalone empty substrate.
//!
//! Standalone mode (no `--attach`) remains the default for offline/dev.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use clawft_rpc::resolve::Resolution;
use clawft_rpc::{DaemonClient, Request};

use super::agent_directory;
use super::daemon_conn::{Outcome, outcome, outcome_for};
use clawft_services::mcp::ToolDefinition;
use clawft_services::mcp::provider::{CallToolResult, ToolError, ToolProvider};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tracing::{info, warn};

// ── Public MCP tool names (catalog / ADR-076) ─────────────────────────

/// Public MCP tool names served by the attach façade.
pub const ATTACH_TOOL_NAMES: &[&str] = &[
    "status",
    "agent_list",
    "agent_get",
    "agent_spawn",
    "agent_stop",
];

/// Actionable error when the kernel daemon is not reachable.
pub fn daemon_unavailable_error() -> String {
    format!(
        "no kernel daemon running (attach mode requires a live instance).\n\
         Start the daemon with: weaver kernel start\n\
         Socket path: {}\n\
         Or use standalone mode: weft mcp-server  (without --attach)",
        super::daemon_conn::current_socket_path().display()
    )
}

// ── AttachFacade trait ────────────────────────────────────────────────

/// Live kernel/weave surface used by attach-mode control tools.
///
/// Prefer this small trait over inlining `DaemonClient` in the MCP
/// provider so unit tests can inject a mock without a real socket.
#[async_trait]
pub trait AttachFacade: Send + Sync {
    /// `kernel.status` → public MCP `status`.
    async fn status(&self) -> Result<Value, String>;

    /// `agent.list` → public MCP `agent_list`.
    async fn agent_list(&self) -> Result<Value, String>;

    /// `expose.list` → the exposed source of the agent directory.
    async fn expose_list(&self) -> Result<Value, String>;

    /// `agent.sources {mesh: true}` → every node's agents and exposures
    /// (this node first), for the whole-mesh directory.
    async fn agent_sources_mesh(&self) -> Result<Value, String>;

    /// `agent.spawn` → public MCP `agent_spawn`.
    async fn agent_spawn(
        &self,
        agent_id: String,
        parent_pid: Option<u64>,
    ) -> Result<Value, String>;

    /// `agent.stop` → public MCP `agent_stop`.
    async fn agent_stop(&self, pid: u64, force: bool) -> Result<Value, String>;
}

// ── DaemonAttachFacade ────────────────────────────────────────────────

/// Production façade: each RPC goes over the kernel Unix-socket client.
///
/// Connections are re-established when the previous client drops so a
/// long-lived `weft mcp-server --attach` survives daemon restarts
/// between tool calls (still fails clear if the daemon is down).
pub struct DaemonAttachFacade {
    /// Explicit socket override (tests / multi-instance). `None` = default.
    socket_path: Option<PathBuf>,
    /// Pre-resolved endpoint (tests); `None` resolves from flags/env each time.
    resolution: Option<Resolution>,
    client: Mutex<Option<DaemonClient>>,
}

impl DaemonAttachFacade {
    /// Connect once at startup; fail immediately if the daemon is down.
    pub async fn connect() -> Result<Self, String> {
        Self::connect_path(None).await
    }

    /// Connect to an explicit socket path (or default when `None`).
    pub async fn connect_path(socket_path: Option<PathBuf>) -> Result<Self, String> {
        let client = match &socket_path {
            Some(p) => DaemonClient::connect_path(p).await,
            None => crate::commands::daemon_conn::connect_opt().await,
        }
        .ok_or_else(daemon_unavailable_error)?;

        info!(
            socket = %socket_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| {
                    crate::commands::daemon_conn::current_socket_path().display().to_string()
                }),
            "attach façade connected to kernel daemon"
        );

        Ok(Self {
            socket_path,
            resolution: None,
            client: Mutex::new(Some(client)),
        })
    }

    /// Connect to a pre-resolved endpoint. A wrong daemon is an `Err`.
    #[cfg(test)]
    async fn connect_resolution(res: Resolution) -> Result<Self, String> {
        let client = match outcome_for(&res).await {
            Outcome::Reachable(c) => c.client,
            Outcome::Unreachable => return Err(daemon_unavailable_error()),
            Outcome::Wrong(text) => return Err(text),
        };
        Ok(Self {
            socket_path: None,
            resolution: Some(res),
            client: Mutex::new(Some(client)),
        })
    }

    async fn ensure_client(&self) -> Result<(), String> {
        let mut guard = self.client.lock().await;
        if guard.is_some() {
            return Ok(());
        }
        // Runs inside a long-lived stdio server: a daemon that came back as
        // the wrong one (other project, node or protocol) is an error for
        // this tool call, never a process exit.
        let client = match (&self.socket_path, &self.resolution) {
            (Some(p), _) => DaemonClient::connect_path(p)
                .await
                .ok_or_else(daemon_unavailable_error)?,
            (None, res) => {
                let found = match res {
                    Some(r) => outcome_for(r).await,
                    None => outcome().await,
                };
                match found {
                    Outcome::Reachable(c) => c.client,
                    Outcome::Unreachable => return Err(daemon_unavailable_error()),
                    Outcome::Wrong(text) => return Err(text),
                }
            }
        };
        *guard = Some(client);
        Ok(())
    }

    async fn rpc(&self, request: Request) -> Result<Value, String> {
        self.ensure_client().await?;
        let mut guard = self.client.lock().await;
        let client = guard
            .as_mut()
            .ok_or_else(daemon_unavailable_error)?;
        match client.call(request).await {
            Ok(resp) => {
                if !resp.ok {
                    let msg = resp
                        .error
                        .unwrap_or_else(|| "daemon RPC failed".into());
                    Err(msg)
                } else {
                    Ok(resp.result.unwrap_or(Value::Null))
                }
            }
            Err(e) => {
                // Drop dead client so the next call re-connects.
                *guard = None;
                warn!(error = %e, "attach façade RPC transport error");
                Err(format!(
                    "daemon RPC failed: {e}\n{}",
                    daemon_unavailable_error()
                ))
            }
        }
    }
}

#[async_trait]
impl AttachFacade for DaemonAttachFacade {
    async fn status(&self) -> Result<Value, String> {
        self.rpc(Request::new("kernel.status")).await
    }

    async fn agent_list(&self) -> Result<Value, String> {
        self.rpc(Request::new("agent.list")).await
    }

    async fn expose_list(&self) -> Result<Value, String> {
        self.rpc(Request::new("expose.list")).await
    }

    async fn agent_sources_mesh(&self) -> Result<Value, String> {
        self.rpc(Request::with_params("agent.sources", json!({"mesh": true}))).await
    }

    async fn agent_spawn(
        &self,
        agent_id: String,
        parent_pid: Option<u64>,
    ) -> Result<Value, String> {
        let params = json!({
            "agent_id": agent_id,
            "parent_pid": parent_pid,
        });
        self.rpc(Request::with_params("agent.spawn", params)).await
    }

    async fn agent_stop(&self, pid: u64, force: bool) -> Result<Value, String> {
        let params = json!({
            "pid": pid,
            "graceful": !force,
        });
        self.rpc(Request::with_params("agent.stop", params)).await
    }
}

// ── Attach tool definitions ───────────────────────────────────────────

/// Tool definitions for the attach control surface (catalog public names).
pub fn attach_tool_definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "status".into(),
            description: "Kernel / instance status from the live WeftOS daemon \
                (weave `kernel.status`). Requires `weft mcp-server --attach`."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: "agent_list".into(),
            description: "Agent directory: declared (the agents/ package tree), running \
                (weave `agent.list`) and exposed (weave `expose.list`, harness \
                exposures) agents joined under a stable id `weftos://agent/<id>`. \
                Each entry names its sources and status; unreadable sources are \
                reported, never silently empty. `mesh: true` also asks every known \
                peer over the signed mesh wire (needs Admin); each instance then names \
                its node, and an unreachable peer is listed in `sources.nodes`. \
                Requires `weft mcp-server --attach`."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "mesh": {
                        "type": "boolean",
                        "description": "Include every known peer (default false: this node only)"
                    }
                },
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: "agent_get".into(),
            description: "One agent directory entry by stable id (`weftos://agent/<id>` or \
                the bare id). Same shape as an `agent_list` entry plus `sources`. \
                Requires `weft mcp-server --attach`."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "Stable id: weftos://agent/<id> or the bare agent id"
                    },
                    "mesh": {
                        "type": "boolean",
                        "description": "Include every known peer (default false: this node only)"
                    }
                },
                "required": ["id"],
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: "agent_spawn".into(),
            description: "Spawn a supervised agent on the live kernel (weave `agent.spawn`). \
                Requires `weft mcp-server --attach`."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "agent_id": {
                        "type": "string",
                        "description": "Unique agent identifier"
                    },
                    "parent_pid": {
                        "type": "integer",
                        "description": "Optional parent PID for spawn lineage"
                    }
                },
                "required": ["agent_id"],
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: "agent_stop".into(),
            description: "Stop a running agent on the live kernel (weave `agent.stop`). \
                Requires `weft mcp-server --attach`."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pid": {
                        "type": "integer",
                        "description": "PID of the agent process to stop"
                    },
                    "force": {
                        "type": "boolean",
                        "description": "Force immediate stop (skip graceful shutdown)",
                        "default": false
                    }
                },
                "required": ["pid"],
                "additionalProperties": false
            }),
        },
    ]
}

/// Filter attach tool defs by serve profile (control tools only).
pub fn filter_attach_tools_by_profile(
    tools: Vec<ToolDefinition>,
    allows: impl Fn(&str) -> bool,
) -> Vec<ToolDefinition> {
    tools.into_iter().filter(|t| allows(&t.name)).collect()
}

// ── AttachToolProvider ────────────────────────────────────────────────

/// MCP [`ToolProvider`] that dispatches control tools to an [`AttachFacade`].
///
/// Namespace is empty so tools list as public names (`status`, `agent_list`,
/// …) when composed via [`CompositeToolProvider`] (empty-ns = no prefix).
pub struct AttachToolProvider {
    tools: Vec<ToolDefinition>,
    facade: Arc<dyn AttachFacade>,
    agents_dir: Option<PathBuf>,
}

impl AttachToolProvider {
    /// Build a provider with the given façade and (already profile-filtered) tools.
    pub fn new(facade: Arc<dyn AttachFacade>, tools: Vec<ToolDefinition>) -> Self {
        Self {
            tools,
            facade,
            agents_dir: None,
        }
    }

    /// Convenience: full attach surface filtered by a profile allow predicate.
    pub fn with_profile(
        facade: Arc<dyn AttachFacade>,
        allows: impl Fn(&str) -> bool,
    ) -> Self {
        let tools = filter_attach_tools_by_profile(attach_tool_definitions(), allows);
        Self::new(facade, tools)
    }
}

impl AttachToolProvider {
    /// Read declared agents from this `agents/` dir instead of locating one.
    #[allow(dead_code)] // tests; the server locates the tree itself
    pub fn with_agents_dir(mut self, dir: PathBuf) -> Self {
        self.agents_dir = Some(dir);
        self
    }

    /// Join declared, running and exposed agents (see [`agent_directory`]),
    /// for this node or, with `mesh`, for every node.
    async fn directory(&self, mesh: bool) -> Result<Value, String> {
        let declared = match &self.agents_dir {
            Some(d) => agent_directory::load_declared(std::path::Path::new("/"), d.to_str()),
            None => match std::env::current_dir() {
                Ok(cwd) => agent_directory::load_declared(
                    &cwd,
                    std::env::var("WEFTOS_AGENTS_DIR").ok().as_deref(),
                ),
                Err(e) => agent_directory::Declared {
                    rows: Err(format!("cwd: {e}")),
                    origin: json!({"origin": null}),
                },
            },
        };
        let nodes: Vec<agent_directory::NodeInput> = if mesh {
            let doc = self
                .facade
                .agent_sources_mesh()
                .await
                .map_err(|e| format!("mesh directory unavailable: {e}"))?;
            let blocks = doc
                .get("nodes")
                .and_then(Value::as_array)
                .ok_or("mesh directory unavailable: agent.sources returned no nodes array")?;
            blocks.iter().map(agent_directory::NodeInput::from_block).collect()
        } else {
            let running = self.facade.agent_list().await.and_then(|v| match v {
                Value::Array(a) => Ok(a),
                other => Err(format!("agent.list returned {other}, expected an array")),
            });
            let exposed = self.facade.expose_list().await;
            vec![agent_directory::NodeInput { node: None, running, exposed }]
        };
        if let Err(d) = &declared.rows
            && nodes.iter().all(|n| n.running.is_err() && n.exposed.is_err())
        {
            let n = &nodes[0];
            return Err(format!(
                "agent directory unavailable: declared: {d}; running: {}; exposed: {}",
                n.running.as_ref().err().map_or("", String::as_str),
                n.exposed.as_ref().err().map_or("", String::as_str)
            ));
        }
        Ok(agent_directory::join(&declared, &nodes))
    }
}

/// The optional `mesh` argument: absent is false, anything but a boolean is refused.
fn mesh_arg(args: &Value) -> Result<bool, ToolError> {
    match args.get("mesh") {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err(ToolError::ExecutionFailed("'mesh' must be true or false".into())),
    }
}

impl std::fmt::Debug for AttachToolProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AttachToolProvider")
            .field("tool_count", &self.tools.len())
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl ToolProvider for AttachToolProvider {
    fn namespace(&self) -> &str {
        // Empty → CompositeToolProvider lists public names (ADR-076 §3).
        ""
    }

    fn list_tools(&self) -> Vec<ToolDefinition> {
        self.tools.clone()
    }

    async fn call_tool(&self, name: &str, args: Value) -> Result<CallToolResult, ToolError> {
        if !self.tools.iter().any(|t| t.name == name) {
            return Err(ToolError::NotFound(name.to_string()));
        }

        let result = match name {
            "status" => self.facade.status().await,
            "agent_list" => self.directory(mesh_arg(&args)?).await,
            "agent_get" => {
                let id = args.get("id").and_then(|v| v.as_str()).ok_or_else(|| {
                    ToolError::ExecutionFailed("missing required argument 'id'".into())
                })?;
                match self.directory(mesh_arg(&args)?).await {
                    Ok(dir) => agent_directory::get(&dir, id),
                    Err(e) => Err(e),
                }
            }
            "agent_spawn" => {
                let agent_id = args
                    .get("agent_id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        ToolError::ExecutionFailed(
                            "missing required argument 'agent_id'".into(),
                        )
                    })?
                    .to_string();
                let parent_pid = args.get("parent_pid").and_then(|v| v.as_u64());
                self.facade.agent_spawn(agent_id, parent_pid).await
            }
            "agent_stop" => {
                let pid = args.get("pid").and_then(|v| v.as_u64()).ok_or_else(|| {
                    ToolError::ExecutionFailed("missing required argument 'pid'".into())
                })?;
                let force = args
                    .get("force")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                self.facade.agent_stop(pid, force).await
            }
            other => {
                return Err(ToolError::NotFound(other.to_string()));
            }
        };

        match result {
            Ok(value) => {
                let text = serde_json::to_string_pretty(&value)
                    .unwrap_or_else(|_| value.to_string());
                Ok(CallToolResult::text(text))
            }
            Err(msg) => Ok(CallToolResult::error(msg)),
        }
    }
}

// ── Standalone stub (optional explicit empty surface) ─────────────────

/// Standalone façade that always fails with an attach-required message.
///
/// Not used by default (standalone mode simply omits attach tools), but
/// available if a caller wants explicit fail-closed control tools.
#[allow(dead_code)] // exercised in unit tests; reserved for explicit fail-closed wiring
pub struct StandaloneAttachFacade;

#[async_trait]
impl AttachFacade for StandaloneAttachFacade {
    async fn status(&self) -> Result<Value, String> {
        Err(standalone_control_error("status"))
    }
    async fn agent_list(&self) -> Result<Value, String> {
        Err(standalone_control_error("agent_list"))
    }
    async fn expose_list(&self) -> Result<Value, String> {
        Err(standalone_control_error("agent_list"))
    }
    async fn agent_sources_mesh(&self) -> Result<Value, String> {
        Err(standalone_control_error("agent_list"))
    }
    async fn agent_spawn(
        &self,
        _agent_id: String,
        _parent_pid: Option<u64>,
    ) -> Result<Value, String> {
        Err(standalone_control_error("agent_spawn"))
    }
    async fn agent_stop(&self, _pid: u64, _force: bool) -> Result<Value, String> {
        Err(standalone_control_error("agent_stop"))
    }
}

#[allow(dead_code)] // used by StandaloneAttachFacade (test + reserved API)
pub fn standalone_control_error(tool: &str) -> String {
    format!(
        "tool '{tool}' requires a live kernel (standalone MCP server has no instance).\n\
         Re-run with: weft mcp-server --attach\n\
         And ensure the daemon is up: weaver kernel start"
    )
}

// ── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use clawft_services::mcp::provider::ContentBlock;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// In-memory mock façade for unit tests (no daemon).
    struct MockAttachFacade {
        status_calls: AtomicUsize,
        list_calls: AtomicUsize,
        spawn_calls: AtomicUsize,
        stop_calls: AtomicUsize,
        fail: bool,
    }

    impl MockAttachFacade {
        fn new() -> Self {
            Self {
                status_calls: AtomicUsize::new(0),
                list_calls: AtomicUsize::new(0),
                spawn_calls: AtomicUsize::new(0),
                stop_calls: AtomicUsize::new(0),
                fail: false,
            }
        }

        fn failing() -> Self {
            Self {
                fail: true,
                ..Self::new()
            }
        }
    }

    #[async_trait]
    impl AttachFacade for MockAttachFacade {
        async fn status(&self) -> Result<Value, String> {
            self.status_calls.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                return Err(daemon_unavailable_error());
            }
            Ok(json!({
                "ok": true,
                "mode": "mock",
                "uptime_secs": 42
            }))
        }

        async fn agent_list(&self) -> Result<Value, String> {
            self.list_calls.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                return Err(daemon_unavailable_error());
            }
            Ok(json!([
                {"pid": 1, "agent_id": "alpha", "state": "running"},
                {"pid": 2, "agent_id": "beta", "state": "idle"}
            ]))
        }

        async fn expose_list(&self) -> Result<Value, String> {
            if self.fail {
                return Err(daemon_unavailable_error());
            }
            Ok(json!({"exposures": [
                {"id": "harness.alpha-run", "kind": "harness", "state": "on", "audience": "paired", "agent": "weftos://agent/alpha"},
                {"id": "infer.chat", "kind": "inference", "state": "on"}
            ]}))
        }

        async fn agent_sources_mesh(&self) -> Result<Value, String> {
            if self.fail {
                return Err(daemon_unavailable_error());
            }
            Ok(json!({"nodes": [
                {"node_id": "n1", "ok": true,
                 "agents": [{"pid": 1, "agent_id": "alpha", "state": "running"}],
                 "exposures": {"exposures": []}},
                {"node_id": "n2", "ok": true,
                 "agents": [{"pid": 7, "agent_id": "alpha", "state": "running"}, {"pid": 8, "agent_id": "gamma", "state": "running"}],
                 "exposures": {"exposures": []}},
                {"node_id": "n3", "ok": false, "error": "timed out"}
            ]}))
        }

        async fn agent_spawn(
            &self,
            agent_id: String,
            parent_pid: Option<u64>,
        ) -> Result<Value, String> {
            self.spawn_calls.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                return Err(daemon_unavailable_error());
            }
            Ok(json!({
                "pid": 99,
                "agent_id": agent_id,
                "parent_pid": parent_pid
            }))
        }

        async fn agent_stop(&self, pid: u64, force: bool) -> Result<Value, String> {
            self.stop_calls.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                return Err(daemon_unavailable_error());
            }
            Ok(json!({ "pid": pid, "stopped": true, "force": force }))
        }
    }

    #[test]
    fn attach_tool_definitions_public_names() {
        let defs = attach_tool_definitions();
        let names: Vec<&str> = defs.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ATTACH_TOOL_NAMES);
        for t in &defs {
            assert!(!t.description.is_empty());
            assert_eq!(t.input_schema["type"], "object");
        }
    }

    #[test]
    fn filter_attach_tools_honors_allow_predicate() {
        let filtered = filter_attach_tools_by_profile(attach_tool_definitions(), |n| {
            n == "status" || n == "agent_list"
        });
        let names: Vec<&str> = filtered.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["status", "agent_list"]);
    }

    #[test]
    fn daemon_unavailable_error_is_actionable() {
        let msg = daemon_unavailable_error();
        assert!(msg.contains("no kernel daemon"));
        assert!(msg.contains("weaver kernel start"));
        assert!(msg.contains("without --attach"));
    }

    fn pkgs(dir: &std::path::Path, ids: &[&str]) -> PathBuf {
        for id in ids {
            let d = dir.join(id);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("weftos-package.yaml"), format!("name: {id}\nkind: lane\n")).unwrap();
        }
        dir.to_path_buf()
    }

    fn text_of(r: &CallToolResult) -> String {
        match &r.content[0] {
            ContentBlock::Text { text } => text.clone(),
        }
    }

    fn json_of(r: &CallToolResult) -> Value {
        assert!(!r.is_error, "{}", text_of(r));
        serde_json::from_str(&text_of(r)).unwrap()
    }

    fn provider(facade: Arc<MockAttachFacade>, ids: &[&str]) -> (AttachToolProvider, tempfile::TempDir) {
        let t = tempfile::tempdir().unwrap();
        let dir = pkgs(t.path(), ids);
        (AttachToolProvider::with_profile(facade, |_| true).with_agents_dir(dir), t)
    }

    #[tokio::test]
    async fn provider_dispatches_status_and_agent_list() {
        let facade = Arc::new(MockAttachFacade::new());
        let (provider, _t) = provider(facade.clone(), &["zeta"]);

        assert_eq!(provider.namespace(), "");
        let listed: Vec<_> = provider.list_tools().iter().map(|t| t.name.clone()).collect();
        assert!(listed.contains(&"status".into()));
        assert!(listed.contains(&"agent_list".into()));
        assert!(listed.contains(&"agent_get".into()));

        let status = provider.call_tool("status", json!({})).await.unwrap();
        assert!(!status.is_error);
        match &status.content[0] {
            ContentBlock::Text { text } => {
                let v: Value = serde_json::from_str(text).unwrap();
                assert_eq!(v["mode"], "mock");
                assert_eq!(v["uptime_secs"], 42);
            }
        }
        assert_eq!(facade.status_calls.load(Ordering::SeqCst), 1);

        let v = json_of(&provider.call_tool("agent_list", json!({})).await.unwrap());
        // alpha (running + exposed), beta (running), zeta (declared)
        assert_eq!(v["count"], 3);
        assert_eq!(v["agents"][0]["id"], "weftos://agent/alpha");
        assert_eq!(v["agents"][0]["from"], json!(["running", "exposed"]));
        assert_eq!(facade.list_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn directory_joins_three_sources_and_agent_get_finds_by_stable_id() {
        let (provider, _t) = provider(Arc::new(MockAttachFacade::new()), &["alpha", "gamma"]);
        let v = json_of(&provider.call_tool("agent_list", json!({})).await.unwrap());
        assert_eq!(v["count"], 3);
        assert_eq!(v["sources"]["declared"]["ok"], true);
        assert_eq!(v["sources"]["declared"]["origin"], "disk");
        let by = |id: &str| v["agents"].as_array().unwrap().iter().find(|e| e["id"] == id).unwrap().clone();
        assert_eq!(by("weftos://agent/alpha")["from"], json!(["declared", "running", "exposed"]));
        assert_eq!(by("weftos://agent/alpha")["exposed"][0]["linked_by"], "field");
        assert_eq!(by("weftos://agent/gamma")["status"], "declared");
        assert_eq!(by("weftos://agent/beta")["status"], "running");

        for key in ["alpha", "weftos://agent/alpha"] {
            let e = json_of(&provider.call_tool("agent_get", json!({"id": key})).await.unwrap());
            assert_eq!(e["id"], "weftos://agent/alpha");
            assert_eq!(e["running"][0]["pid"], 1);
            assert_eq!(e["sources"]["nodes"][0]["ok"], true);
        }
    }

    #[tokio::test]
    async fn mesh_flag_asks_every_node_and_names_the_dead_one() {
        let (provider, _t) = provider(Arc::new(MockAttachFacade::new()), &["alpha", "gamma", "steward"]);
        let v = json_of(&provider.call_tool("agent_list", json!({"mesh": true})).await.unwrap());
        let by = |id: &str| v["agents"].as_array().unwrap().iter().find(|e| e["id"] == id).unwrap().clone();
        let alpha = by("weftos://agent/alpha");
        assert_eq!(alpha["running"].as_array().unwrap().len(), 2, "one key, an instance per node");
        assert_eq!(alpha["running"][0]["node"], "n1");
        assert_eq!(alpha["running"][1]["node"], "n2");
        assert_eq!(by("weftos://agent/gamma")["running"][0]["node"], "n2");
        // n3 did not answer: the list still comes back and says so.
        assert_eq!(v["sources"]["nodes"][2]["node"], "n3");
        assert_eq!(v["sources"]["nodes"][2]["ok"], false);
        assert_eq!(v["sources"]["nodes"][2]["running"]["error"], "timed out");
        assert_eq!(by("weftos://agent/steward")["status"], "unknown");

        let e = json_of(&provider.call_tool("agent_get", json!({"id": "gamma", "mesh": true})).await.unwrap());
        assert_eq!(e["running"][0]["node"], "n2");
    }

    #[tokio::test]
    async fn default_is_local_and_a_bad_mesh_argument_is_refused() {
        let facade = Arc::new(MockAttachFacade::new());
        let (provider, _t) = provider(facade, &["alpha"]);
        let v = json_of(&provider.call_tool("agent_list", json!({})).await.unwrap());
        assert_eq!(v["sources"]["nodes"].as_array().unwrap().len(), 1);
        assert!(v["agents"][0]["running"][0].get("node").is_none());
        for bad in [json!({"mesh": "yes"}), json!({"mesh": 1})] {
            assert!(matches!(
                provider.call_tool("agent_list", bad).await.unwrap_err(),
                ToolError::ExecutionFailed(m) if m.contains("mesh")
            ));
        }
    }

    #[tokio::test]
    async fn mesh_view_that_cannot_be_asked_is_an_error_not_a_local_answer() {
        let (provider, _t) = provider(Arc::new(MockAttachFacade::failing()), &["alpha"]);
        let r = provider.call_tool("agent_list", json!({"mesh": true})).await.unwrap();
        assert!(r.is_error);
        assert!(text_of(&r).contains("mesh directory unavailable"));
    }

    #[tokio::test]
    async fn agent_get_refuses_unknown_malformed_and_missing_id() {
        let (provider, _t) = provider(Arc::new(MockAttachFacade::new()), &["alpha"]);
        for bad in ["nope", "", "http://x/y", "weftos://agent/a/b"] {
            let r = provider.call_tool("agent_get", json!({"id": bad})).await.unwrap();
            assert!(r.is_error, "{bad:?} must be an error result");
        }
        assert!(matches!(
            provider.call_tool("agent_get", json!({})).await.unwrap_err(),
            ToolError::ExecutionFailed(m) if m.contains("'id'")
        ));
    }

    #[tokio::test]
    async fn directory_with_no_readable_source_is_an_error_not_empty() {
        let t = tempfile::tempdir().unwrap(); // no packages in it
        let provider = AttachToolProvider::with_profile(Arc::new(MockAttachFacade::failing()), |_| true)
            .with_agents_dir(t.path().to_path_buf());
        let r = provider.call_tool("agent_list", json!({})).await.unwrap();
        assert!(r.is_error);
        assert!(text_of(&r).contains("agent directory unavailable"));
    }

    #[tokio::test]
    async fn directory_degrades_when_daemon_is_down_but_packages_read() {
        let (provider, _t) = provider(Arc::new(MockAttachFacade::failing()), &["alpha"]);
        let v = json_of(&provider.call_tool("agent_list", json!({})).await.unwrap());
        assert_eq!(v["sources"]["nodes"][0]["ok"], false);
        assert_eq!(v["agents"][0]["status"], "unknown");
    }

    #[tokio::test]
    async fn provider_dispatches_spawn_and_stop() {
        let facade = Arc::new(MockAttachFacade::new());
        let provider = AttachToolProvider::new(facade.clone(), attach_tool_definitions());

        let spawn = provider
            .call_tool(
                "agent_spawn",
                json!({"agent_id": "coder", "parent_pid": 7}),
            )
            .await
            .unwrap();
        assert!(!spawn.is_error);
        match &spawn.content[0] {
            ContentBlock::Text { text } => {
                let v: Value = serde_json::from_str(text).unwrap();
                assert_eq!(v["agent_id"], "coder");
                assert_eq!(v["pid"], 99);
                assert_eq!(v["parent_pid"], 7);
            }
        }

        let stop = provider
            .call_tool("agent_stop", json!({"pid": 99, "force": true}))
            .await
            .unwrap();
        assert!(!stop.is_error);
        match &stop.content[0] {
            ContentBlock::Text { text } => {
                let v: Value = serde_json::from_str(text).unwrap();
                assert_eq!(v["pid"], 99);
                assert_eq!(v["force"], true);
            }
        }
        assert_eq!(facade.spawn_calls.load(Ordering::SeqCst), 1);
        assert_eq!(facade.stop_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn provider_spawn_requires_agent_id() {
        let facade = Arc::new(MockAttachFacade::new());
        let provider = AttachToolProvider::new(facade, attach_tool_definitions());
        let err = provider
            .call_tool("agent_spawn", json!({}))
            .await
            .unwrap_err();
        match err {
            ToolError::ExecutionFailed(msg) => assert!(msg.contains("agent_id")),
            other => panic!("expected ExecutionFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn provider_fails_clear_when_facade_unavailable() {
        let facade = Arc::new(MockAttachFacade::failing());
        let provider = AttachToolProvider::new(facade, attach_tool_definitions());

        let result = provider.call_tool("status", json!({})).await.unwrap();
        assert!(result.is_error, "must surface error, not empty success");
        match &result.content[0] {
            ContentBlock::Text { text } => {
                assert!(text.contains("no kernel daemon"));
                assert!(text.contains("weaver kernel start"));
            }
        }
    }

    #[tokio::test]
    async fn provider_not_found_for_unknown_tool() {
        let facade = Arc::new(MockAttachFacade::new());
        let provider = AttachToolProvider::new(facade, attach_tool_definitions());
        let err = provider
            .call_tool("read_file", json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)));
    }

    #[tokio::test]
    async fn standalone_facade_rejects_with_attach_hint() {
        let f = StandaloneAttachFacade;
        let err = f.status().await.unwrap_err();
        assert!(err.contains("--attach"));
        assert!(err.contains("weaver kernel start"));
    }

    #[tokio::test]
    async fn connect_path_isolated_socket_fails_clear() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("kernel.sock");
        let result = DaemonAttachFacade::connect_path(Some(sock)).await;
        let err = match result {
            Ok(_) => panic!("expected connect failure on empty socket path"),
            Err(e) => e,
        };
        assert!(err.contains("no kernel daemon"));
        assert!(err.contains("without --attach"));
    }

    #[tokio::test]
    async fn wrong_daemon_after_start_is_a_tool_error_not_an_exit() {
        use clawft_rpc::handshake::{BoundVia, handshake_value};
        use clawft_rpc::resolve::{ResolveFlags, ResolveInputs, resolve_with};
        use clawft_rpc::{Handshake, ProtoRange, Response};
        use std::sync::atomic::AtomicBool;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        const A: &str = "01J0000000000000000000000A";
        const B: &str = "01J0000000000000000000000B";
        let dir = tempfile::Builder::new()
            .prefix("ma")
            .tempdir_in(std::env::temp_dir())
            .unwrap();
        let swapped = Arc::new(AtomicBool::new(false));
        let flag = swapped.clone();
        let l = tokio::net::UnixListener::bind(dir.path().join("kernel.sock")).unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((s, _)) = l.accept().await else { return };
                let flag = flag.clone();
                tokio::spawn(async move {
                    let (r, mut w) = s.into_split();
                    let mut lines = BufReader::new(r).lines();
                    while let Ok(Some(_)) = lines.next_line().await {
                        let project = if flag.load(Ordering::SeqCst) { B } else { A };
                        let hs = Handshake {
                            proto: ProtoRange::supported(),
                            node_id: "n".into(),
                            user_id: None,
                            project_id: Some(project.into()),
                            bound_via: BoundVia::Project,
                            depth: 0,
                            parent: None,
                            runtime_dir: "/x".into(),
                            pid: 1,
                            version: "0.8.1".into(),
                            sha: "abcd1234".into(),
                            binary: None,
                            mesh: None,
                            user_key_id: None,
                            profile: None,
                            roles: Vec::new(),
                        };
                        let mut out =
                            serde_json::to_string(&Response::success(handshake_value(&hs)))
                                .unwrap();
                        out.push('\n');
                        if w.write_all(out.as_bytes()).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        let res = resolve_with(&ResolveInputs {
            flags: ResolveFlags {
                runtime: Some(dir.path().to_path_buf()),
                project: Some(A.into()),
            },
            ..Default::default()
        })
        .unwrap();
        let facade = DaemonAttachFacade::connect_resolution(res).await.unwrap();

        // The daemon "restarts" as another project; the old connection is gone.
        facade.client.lock().await.take();
        swapped.store(true, Ordering::SeqCst);
        let err = facade.status().await.unwrap_err();
        assert!(err.contains(A) && err.contains(B), "{err}");
        assert!(err.contains(&format!("--project {A}")), "{err}");

        // Still alive and recovers when the right daemon is back.
        swapped.store(false, Ordering::SeqCst);
        let _ = facade.status().await; // reconnects; the fake answers handshakes only
        assert!(facade.client.lock().await.is_some());
    }
}
