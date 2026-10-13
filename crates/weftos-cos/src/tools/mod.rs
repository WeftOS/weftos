//! Typed, logged tools for the chief of staff. Every tool declares a
//! [`Gate`]; `Confirm` tools run only after a confirmation turn from the owner.
//! Tool arguments never carry a secret (the harness credential lives in the
//! client, loaded from a 0600 file), and every call is recorded as a `tool`
//! event plus a JSONL line.

pub mod board;
pub mod cards;
pub mod harness;
pub mod kickoff;
pub mod mcp;
pub mod nodes;
pub mod notes;
pub mod ui;

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::CosError;
use crate::conversation::TurnId;
use crate::events::redact;

/// Whether a tool may run on the model's say-so or needs the owner's yes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    /// Read-only, or a proposal that someone else applies.
    Open,
    /// Outward-facing, destructive or production-touching: ask first.
    Confirm,
}

/// Static description of a tool, also rendered as an OpenAI function spec.
#[derive(Debug, Clone)]
pub struct ToolSpec {
    /// Dotted id as the owner sees it (`projects.activity`).
    pub id: &'static str,
    pub description: &'static str,
    /// JSON schema for the arguments object.
    pub parameters: Value,
    pub gate: Gate,
}

impl ToolSpec {
    /// Provider-safe function name: dots are not allowed in OpenAI or
    /// Anthropic function names, so `projects.activity` travels as
    /// `projects_activity`.
    pub fn wire_name(&self) -> String {
        self.id.replace('.', "_")
    }

    pub fn openai_spec(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": self.wire_name(),
                "description": self.description,
                "parameters": self.parameters,
            }
        })
    }
}

/// What a tool hands back: a one-line summary for the event stream and the
/// structured result for the model.
#[derive(Debug, Clone)]
pub struct ToolOutcome {
    pub summary: String,
    pub result: Value,
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    async fn call(&self, args: Value) -> Result<ToolOutcome, CosError>;
}

/// Append-only JSONL record of tool calls. Lines carry name, summary, status
/// and turn; never arguments.
#[derive(Clone, Default)]
pub struct ToolLog {
    path: Option<PathBuf>,
    lock: Arc<Mutex<()>>,
}

impl ToolLog {
    pub fn new(path: Option<PathBuf>) -> Self {
        Self { path, lock: Arc::new(Mutex::new(())) }
    }

    pub fn record(&self, turn: &TurnId, name: &str, summary: &str, status: &str) {
        tracing::info!(target: "cos.tool", turn = %turn, name, status, summary = %redact(summary), "tool call");
        let Some(path) = &self.path else { return };
        let line = json!({
            "at": chrono::Utc::now().to_rfc3339(),
            "turn_id": turn.0,
            "name": name,
            "summary": redact(summary),
            "status": status,
        });
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(f, "{line}");
        }
    }
}

/// A set of tools that can change while the host runs: MCP servers that come
/// up late, go down and come back. The registry asks it for a snapshot.
#[async_trait]
pub trait ToolSource: Send + Sync {
    /// Retry whatever is not connected. Cheap when everything is up and
    /// bounded in time; never fails the caller.
    async fn refresh(&self);
    /// The tools known right now.
    fn tools(&self) -> Vec<Arc<dyn Tool>>;
    /// Short lines for the system prompt: what exists and when to use it.
    fn catalog(&self) -> Vec<String>;
    /// `(source, reason)` for each source that cannot be reached right now.
    fn unavailable(&self) -> Vec<(String, String)>;
}

/// The tools the agent may call, looked up by dotted id or wire name.
#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: Vec<Arc<dyn Tool>>,
    by_wire: HashMap<String, usize>,
    sources: Vec<Arc<dyn ToolSource>>,
    /// Most source-provided tools offered to the model per turn.
    dynamic_cap: Option<usize>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(mut self, tool: Arc<dyn Tool>) -> Self {
        let spec = tool.spec();
        self.by_wire.insert(spec.wire_name(), self.tools.len());
        self.by_wire.insert(spec.id.to_string(), self.tools.len());
        self.tools.push(tool);
        self
    }

    /// Add a changing set of tools. Built-in tools always win a name clash.
    pub fn with_source(mut self, source: Arc<dyn ToolSource>) -> Self {
        self.sources.push(source);
        self
    }

    /// Cap how many source-provided tools one turn offers the model.
    pub fn with_dynamic_cap(mut self, cap: usize) -> Self {
        self.dynamic_cap = Some(cap.max(1));
        self
    }

    /// Reconnect sources that are down. Called at the start of a turn.
    pub async fn refresh(&self) {
        for s in &self.sources {
            s.refresh().await;
        }
    }

    pub fn find(&self, name: &str) -> Option<Arc<dyn Tool>> {
        if let Some(i) = self.by_wire.get(name) {
            return Some(self.tools[*i].clone());
        }
        self.sources.iter().flat_map(|s| s.tools()).find(|t| {
            let spec = t.spec();
            spec.id == name || spec.wire_name() == name
        })
    }

    /// Tools from sources that do not shadow a built-in.
    fn dynamic_tools(&self) -> Vec<Arc<dyn Tool>> {
        self.sources
            .iter()
            .flat_map(|s| s.tools())
            .filter(|t| !self.by_wire.contains_key(&t.spec().wire_name()))
            .collect()
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        let mut v: Vec<ToolSpec> = self.tools.iter().map(|t| t.spec()).collect();
        v.extend(self.dynamic_tools().iter().map(|t| t.spec()));
        v
    }

    /// What one turn offers the model: every built-in, plus at most the cap
    /// of source tools, the best keyword matches for `request` when there
    /// are more than fit.
    pub fn specs_for(&self, request: &str) -> Vec<ToolSpec> {
        let mut v: Vec<ToolSpec> = self.tools.iter().map(|t| t.spec()).collect();
        let dynamic: Vec<ToolSpec> = self.dynamic_tools().iter().map(|t| t.spec()).collect();
        v.extend(mcp::select_relevant(dynamic, request, self.dynamic_cap.unwrap_or(usize::MAX)));
        v
    }

    pub fn openai_specs(&self) -> Vec<Value> {
        self.specs().iter().map(ToolSpec::openai_spec).collect()
    }

    /// Prompt lines describing the sources and their state.
    pub fn catalog(&self) -> Vec<String> {
        self.sources.iter().flat_map(|s| s.catalog()).collect()
    }

    pub fn unavailable_sources(&self) -> Vec<(String, String)> {
        self.sources.iter().flat_map(|s| s.unavailable()).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty() && self.sources.is_empty()
    }
}

/// Fail a tool call with a redacted reason.
pub(crate) fn tool_err(name: &str, reason: impl std::fmt::Display) -> CosError {
    CosError::Tool { name: name.into(), reason: redact(&reason.to_string()) }
}

/// Read an optional string argument.
pub(crate) fn arg_str(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(Value::as_str).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Read a required string argument.
pub(crate) fn need_str(name: &str, args: &Value, key: &str) -> Result<String, CosError> {
    arg_str(args, key).ok_or_else(|| tool_err(name, format!("missing argument `{key}`")))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Echo;
    #[async_trait]
    impl Tool for Echo {
        fn spec(&self) -> ToolSpec {
            ToolSpec { id: "echo.it", description: "echo", parameters: json!({"type":"object"}), gate: Gate::Open }
        }
        async fn call(&self, args: Value) -> Result<ToolOutcome, CosError> {
            Ok(ToolOutcome { summary: "echoed".into(), result: args })
        }
    }

    #[test]
    fn registry_resolves_dotted_and_wire_names() {
        let reg = ToolRegistry::new().with(Arc::new(Echo));
        assert!(reg.find("echo.it").is_some());
        assert!(reg.find("echo_it").is_some());
        assert!(reg.find("nope").is_none());
        assert_eq!(reg.openai_specs()[0]["function"]["name"], "echo_it");
    }

    #[test]
    fn log_lines_carry_no_arguments_and_are_redacted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.jsonl");
        let log = ToolLog::new(Some(path.clone()));
        let tok = format!("wfb_{}", "b".repeat(64));
        log.record(&TurnId::new(), "projects.activity", &format!("read with {tok}"), "ok");
        let text = std::fs::read_to_string(path).unwrap();
        assert!(!text.contains(&tok));
        assert!(text.contains("[redacted]"));
        assert!(!text.contains("\"args\""));
    }
}
