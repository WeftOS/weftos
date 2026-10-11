//! Typed, logged tools for the chief of staff. Every tool declares a
//! [`Gate`]; `Confirm` tools run only after a confirmation turn from the owner.
//! Tool arguments never carry a secret (the harness credential lives in the
//! client, loaded from a 0600 file), and every call is recorded as a `tool`
//! event plus a JSONL line.

pub mod board;
pub mod harness;
pub mod kickoff;
pub mod nodes;

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

/// The tools the agent may call, looked up by dotted id or wire name.
#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: Vec<Arc<dyn Tool>>,
    by_wire: HashMap<String, usize>,
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

    pub fn find(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.by_wire.get(name).map(|i| &self.tools[*i])
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.iter().map(|t| t.spec()).collect()
    }

    pub fn openai_specs(&self) -> Vec<Value> {
        self.tools.iter().map(|t| t.spec().openai_spec()).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
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
