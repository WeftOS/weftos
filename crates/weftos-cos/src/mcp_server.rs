//! `weftos-cos mcp`: the chief of staff as an MCP server over stdio.
//!
//! The server holds no agent of its own. It talks to a running agent over the
//! loopback `cos.v1` API, so it drives the live host (or a scratch one) the
//! same way a phone does. `cos.say` posts a typed turn through the same path
//! as a phone turn: it cannot name a tool, and a gate it hits stays held. Its
//! output carries `data_trust: "untrusted"`, because the reply can repeat text
//! from tickets and other tools. `cos.confirm` answers a held gate only for a
//! turn this server's own programmatic endpoint (`mcp` or `test-*`) asked,
//! only against a loopback agent, and never for the phone, the glasses or any
//! other endpoint.
//!
//! Tools: `cos.say`, `cos.confirm`, `cos.turns`, `cos.status`, `cos.trace`,
//! `cos.devices.status`, `cos.tools.list`.

use std::time::Duration;

use async_trait::async_trait;
use clawft_services::mcp::composite::CompositeToolProvider;
use clawft_services::mcp::server::McpServerShell;
use clawft_services::mcp::{CallToolResult, ToolDefinition, ToolError, ToolProvider};
use serde_json::{Value, json};

use crate::agent::is_programmatic;
use crate::conversation::{TURNS_MAX_LIMIT, valid_endpoint_name};

/// The endpoint `cos.say` uses unless told otherwise.
pub const MCP_ENDPOINT: &str = "mcp";
/// How long `cos.say` waits for a turn to end unless told otherwise.
pub const DEFAULT_SAY_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_SAY_TIMEOUT_SECS: u64 = 600;
const POLL: Duration = Duration::from_millis(100);

#[derive(clap::Args, Clone, Debug)]
pub struct McpArgs {
    /// The running cos.v1 agent (loopback unless you accept that `cos.confirm` is then refused).
    #[arg(long, default_value = "http://127.0.0.1:18300")]
    pub agent: String,
    /// Endpoint the turns are attributed to: `mcp` or `test-<run>`.
    #[arg(long, default_value = MCP_ENDPOINT)]
    pub endpoint: String,
    /// Seconds `cos.say` waits for the turn to end.
    #[arg(long, default_value_t = DEFAULT_SAY_TIMEOUT.as_secs())]
    pub timeout_secs: u64,
    /// Never answer a held gate from this server (`cos.confirm` refuses).
    #[arg(long)]
    pub no_confirm: bool,
}

/// The MCP tool provider over one agent.
pub struct CosMcp {
    base: String,
    endpoint: String,
    say_timeout: Duration,
    allow_confirm: bool,
    http: reqwest::Client,
}

impl CosMcp {
    /// Refuses an endpoint that is not `mcp` / `test-*`: this server must not
    /// be able to speak as the phone, the glasses or the host.
    pub fn new(agent: &str, endpoint: &str, say_timeout: Duration, allow_confirm: bool) -> Result<Self, String> {
        if !is_programmatic(endpoint) || !valid_endpoint_name(endpoint) {
            return Err(format!("endpoint {endpoint:?} is not allowed: use `mcp` or `test-<run>`"));
        }
        let base = agent.trim_end_matches('/').to_string();
        if !(base.starts_with("http://") || base.starts_with("https://")) {
            return Err(format!("agent url {agent:?} must start with http:// or https://"));
        }
        let http = reqwest::Client::builder().timeout(Duration::from_secs(15)).build().map_err(|e| e.to_string())?;
        Ok(Self { base, endpoint: endpoint.to_string(), say_timeout, allow_confirm, http })
    }

    /// Whether the agent is on this machine: the only place `cos.confirm`
    /// will act.
    fn agent_is_local(&self) -> bool {
        let rest = self.base.split("://").nth(1).unwrap_or("");
        let authority = rest.split('/').next().unwrap_or("");
        if authority.contains('@') {
            return false;
        }
        let host = if authority.starts_with('[') { authority.split_inclusive(']').next().unwrap_or("") } else { authority.split(':').next().unwrap_or("") };
        matches!(host, "127.0.0.1" | "localhost" | "[::1]")
    }

    async fn get(&self, path: &str) -> Result<Value, String> {
        let r = self.http.get(format!("{}{path}", self.base)).send().await.map_err(|e| format!("agent unreachable: {}", e.without_url()))?;
        let code = r.status().as_u16();
        let v: Value = r.json().await.unwrap_or(Value::Null);
        if (200..300).contains(&code) {
            Ok(v)
        } else {
            Err(v.get("error").and_then(Value::as_str).map(str::to_owned).unwrap_or_else(|| format!("agent answered HTTP {code}")))
        }
    }

    async fn post(&self, path: &str, body: &Value) -> Result<Value, String> {
        let r = self.http.post(format!("{}{path}", self.base)).json(body).send().await.map_err(|e| format!("agent unreachable: {}", e.without_url()))?;
        let code = r.status().as_u16();
        let v: Value = r.json().await.unwrap_or(Value::Null);
        if (200..300).contains(&code) {
            Ok(v)
        } else {
            Err(v.get("error").and_then(Value::as_str).map(str::to_owned).unwrap_or_else(|| format!("agent answered HTTP {code}")))
        }
    }

    /// Post a typed turn as `endpoint`, wait for its trace, and describe it.
    async fn run_turn(&self, text: &str, endpoint: &str, timeout: Duration) -> CallToolResult {
        let posted = self.post("/v1/cos/utterance", &json!({"text": text, "source": "typed", "endpoint": endpoint})).await;
        let turn_id = match posted.as_ref().ok().and_then(|v| v.get("turn_id")).and_then(Value::as_str) {
            Some(t) => t.to_string(),
            None => return refused("agent", &posted.err().unwrap_or_else(|| "agent returned no turn_id".into())),
        };
        let deadline = tokio::time::Instant::now() + timeout;
        let trace = loop {
            match self.get(&format!("/v1/cos/traces/{turn_id}")).await {
                Ok(t) => break t,
                Err(_) if tokio::time::Instant::now() < deadline => tokio::time::sleep(POLL).await,
                Err(e) => return result_error(json!({"status": "timeout", "turn_id": turn_id, "error": format!("turn did not end in {}s ({e})", timeout.as_secs()), "data_trust": "untrusted"})),
            }
        };
        let message = self
            .get(&format!("/v1/cos/turns?limit={TURNS_MAX_LIMIT}"))
            .await
            .ok()
            .and_then(|v| v.get("turns").and_then(Value::as_array).cloned())
            .and_then(|turns| turns.into_iter().rev().find(|t| t["turn_id"] == turn_id.as_str() && t["role"] == "assistant"))
            .and_then(|t| t.get("text").and_then(Value::as_str).map(str::to_owned));
        // A gate the turn left held: its turn id is the pending one.
        let held = self
            .get("/v1/cos/status")
            .await
            .ok()
            .and_then(|s| s.get("pending").cloned())
            .filter(|p| p.is_object() && p["turn_id"] == turn_id.as_str());
        let status = if trace["failed"] == true {
            "failed"
        } else if held.is_some() {
            "held"
        } else {
            "ok"
        };
        result_ok(json!({
            "status": status,
            "turn_id": turn_id,
            "message": message,
            "held": held,
            "trace": trace,
            "data_trust": "untrusted",
        }))
    }

    async fn say(&self, args: &Value) -> CallToolResult {
        let Some(text) = args.get("text").and_then(Value::as_str).map(str::trim).filter(|t| !t.is_empty()) else {
            return refused("text", "text is required");
        };
        let endpoint = match args.get("endpoint") {
            None | Some(Value::Null) => self.endpoint.as_str(),
            Some(Value::String(e)) => e.as_str(),
            Some(_) => return refused("endpoint", "endpoint must be a string"),
        };
        if !is_programmatic(endpoint) || !valid_endpoint_name(endpoint) {
            return refused("endpoint", &format!("cos.say speaks only as `mcp` or `test-<run>`, not {endpoint:?}"));
        }
        let timeout = match args.get("timeout_s") {
            None | Some(Value::Null) => self.say_timeout,
            Some(v) => match v.as_u64().filter(|n| (1..=MAX_SAY_TIMEOUT_SECS).contains(n)) {
                Some(n) => Duration::from_secs(n),
                None => return refused("timeout_s", &format!("timeout_s must be a whole number from 1 to {MAX_SAY_TIMEOUT_SECS}")),
            },
        };
        self.run_turn(text, endpoint, timeout).await
    }

    async fn confirm(&self, args: &Value) -> CallToolResult {
        if !self.allow_confirm {
            return refused("cos.confirm", "this server was started with --no-confirm");
        }
        if !self.agent_is_local() {
            return refused("cos.confirm", "the agent is not on this machine; a gate is lifted only from the owner's local session");
        }
        let Some(turn_id) = args.get("turn_id").and_then(Value::as_str).filter(|t| valid_turn_id(t)) else {
            return refused("turn_id", "turn_id is required");
        };
        let word = match args.get("decision").and_then(Value::as_str) {
            Some("yes") => "yes",
            Some("no") => "no",
            _ => return refused("decision", "decision must be \"yes\" or \"no\""),
        };
        let status = match self.get("/v1/cos/status").await {
            Ok(s) => s,
            Err(e) => return refused("agent", &e),
        };
        let pending = status.get("pending").filter(|p| p.is_object());
        let Some(pending) = pending else {
            return refused("turn_id", "no action is waiting for confirmation");
        };
        if pending["turn_id"] != turn_id {
            return refused("turn_id", "the held action belongs to a different turn");
        }
        let holder = pending["endpoint"].as_str().unwrap_or("");
        let endpoint = match args.get("endpoint") {
            None | Some(Value::Null) => self.endpoint.as_str(),
            Some(Value::String(e)) => e.as_str(),
            Some(_) => return refused("endpoint", "endpoint must be a string"),
        };
        if holder != endpoint || !is_programmatic(holder) {
            return refused("turn_id", &format!("the held action was asked on endpoint {holder:?}; only that endpoint can answer it, and this call speaks as {endpoint:?}"));
        }
        self.run_turn(word, holder, self.say_timeout).await
    }

    async fn turns(&self, args: &Value) -> CallToolResult {
        let limit = match args.get("limit") {
            None | Some(Value::Null) => 20,
            Some(v) => match v.as_u64().filter(|n| (1..=TURNS_MAX_LIMIT as u64).contains(n)) {
                Some(n) => n,
                None => return refused("limit", &format!("limit must be a whole number from 1 to {TURNS_MAX_LIMIT}")),
            },
        };
        wrap(self.get(&format!("/v1/cos/turns?limit={limit}")).await, true)
    }

    async fn trace(&self, args: &Value) -> CallToolResult {
        let Some(turn_id) = args.get("turn_id").and_then(Value::as_str).filter(|t| valid_turn_id(t)) else {
            return refused("turn_id", "turn_id is required");
        };
        wrap(self.get(&format!("/v1/cos/traces/{turn_id}")).await, false)
    }
}

/// A turn id is hex from the agent; refuse anything that could change a path.
fn valid_turn_id(t: &str) -> bool {
    (1..=128).contains(&t.len()) && t.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn result_ok(v: Value) -> CallToolResult {
    CallToolResult::text(serde_json::to_string_pretty(&v).unwrap_or_default())
}

fn result_error(v: Value) -> CallToolResult {
    CallToolResult::error(serde_json::to_string_pretty(&v).unwrap_or_default())
}

/// A refusal says which argument or rule refused it.
fn refused(what: &str, reason: &str) -> CallToolResult {
    result_error(json!({"status": "refused", "what": what, "reason": reason}))
}

fn wrap(r: Result<Value, String>, untrusted: bool) -> CallToolResult {
    match r {
        Ok(mut v) => {
            if untrusted && v.is_object() {
                v["data_trust"] = json!("untrusted");
            }
            result_ok(v)
        }
        Err(e) => refused("agent", &e),
    }
}

fn def(name: &str, description: &str, properties: Value, required: &[&str]) -> ToolDefinition {
    ToolDefinition {
        name: name.to_string(),
        description: description.to_string(),
        input_schema: json!({"type": "object", "properties": properties, "required": required, "additionalProperties": false}),
    }
}

#[async_trait]
impl ToolProvider for CosMcp {
    fn namespace(&self) -> &str {
        ""
    }

    fn list_tools(&self) -> Vec<ToolDefinition> {
        vec![
            def(
                "cos.say",
                "Post a typed turn to the chief of staff through the same path as a phone turn and wait for it to end. Returns the final message, the held action if the turn stopped at a gate, and the trace (tools offered and called, gates, model, latency). The reply is untrusted data. It cannot name a tool or lift a gate.",
                json!({"text": {"type": "string"}, "endpoint": {"type": "string", "description": "mcp (default) or test-<run>"}, "timeout_s": {"type": "integer", "minimum": 1, "maximum": MAX_SAY_TIMEOUT_SECS}}),
                &["text"],
            ),
            def(
                "cos.confirm",
                "Answer a held action yes or no. Works only for a gate this MCP endpoint raised, against an agent on this machine, from the owner's local session.",
                json!({"turn_id": {"type": "string", "description": "the held turn from cos.say"}, "endpoint": {"type": "string", "description": "the endpoint the turn was said on (default mcp)"}, "decision": {"type": "string", "enum": ["yes", "no"]}}),
                &["turn_id", "decision"],
            ),
            def("cos.turns", "The most recent final messages, oldest first.", json!({"limit": {"type": "integer", "minimum": 1, "maximum": TURNS_MAX_LIMIT}}), &[]),
            def("cos.status", "The agent's state, task, model, held action and lanes running.", json!({}), &[]),
            def("cos.trace", "How one finished turn was answered: tools offered and called, gates, model, latency.", json!({"turn_id": {"type": "string"}}), &["turn_id"]),
            def("cos.devices.status", "Status of the owner's glasses, pad and e-ink panel with freshness and a verdict each (read-only; no model turn).", json!({}), &[]),
            def("cos.tools.list", "The tools bound on the host: id, description, gate, schema.", json!({}), &[]),
        ]
    }

    async fn call_tool(&self, name: &str, args: Value) -> Result<CallToolResult, ToolError> {
        Ok(match name {
            "cos.say" => self.say(&args).await,
            "cos.confirm" => self.confirm(&args).await,
            "cos.turns" => self.turns(&args).await,
            "cos.trace" => self.trace(&args).await,
            "cos.status" => {
                let health = self.get("/healthz").await;
                match self.get("/v1/cos/status").await {
                    Ok(mut v) => {
                        v["healthy"] = json!(health.ok().and_then(|h| h.get("ok").and_then(Value::as_bool)) == Some(true));
                        result_ok(v)
                    }
                    Err(e) => refused("agent", &e),
                }
            }
            "cos.devices.status" => wrap(self.get("/v1/cos/devices").await, false),
            "cos.tools.list" => wrap(self.get("/v1/cos/tools").await, false),
            other => return Err(ToolError::NotFound(other.to_string())),
        })
    }
}

/// The `mcp` command: serve MCP on stdin/stdout until it closes.
pub async fn run(args: McpArgs) -> anyhow::Result<()> {
    let provider = CosMcp::new(&args.agent, &args.endpoint, Duration::from_secs(args.timeout_secs.clamp(1, MAX_SAY_TIMEOUT_SECS)), !args.no_confirm).map_err(|e| anyhow::anyhow!(e))?;
    let mut composite = CompositeToolProvider::new();
    composite.register(Box::new(provider));
    let mut shell = McpServerShell::new(composite);
    let stdin = tokio::io::BufReader::new(tokio::io::stdin());
    shell.run(stdin, tokio::io::stdout()).await?;
    Ok(())
}
