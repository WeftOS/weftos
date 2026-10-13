//! MCP servers as chief-of-staff tools.
//!
//! Each `mcp_servers` entry is a [`McpServer`]: connect, list tools, expose the
//! allow-listed ones as `<server>.<tool>` with the server's own JSON schema.
//! The client is `clawft-services`' (`McpSession`, stdio and HTTP transports);
//! nothing here speaks the protocol itself.
//!
//! A server that is down is not an error at startup: its tools are absent from
//! the offer, the prompt catalog says it is unavailable, and it is retried at
//! turn starts (rate-limited by `retry_secs`) and whenever one of its tools is
//! called. Secrets live only in memory for the length of a connection and are
//! scrubbed from every error and result that leaves this module.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use clawft_services::error::ServiceError;
use clawft_services::mcp::transport::{HttpTransport, McpTransport, StdioTransport};
use clawft_services::mcp::{McpSession, ToolDefinition};
use serde_json::{Value, json};

use super::{Gate, Tool, ToolOutcome, ToolSource, ToolSpec, tool_err};
use crate::CosError;
use crate::config::{McpGate, McpServerConfig, McpTransportConfig};

const DEFAULT_CONNECT_SECS: u64 = 8;
const DEFAULT_CALL_SECS: u64 = 30;
const DEFAULT_RETRY_SECS: u64 = 30;
const DEFAULT_MAX_OUTPUT: usize = 8000;
/// A healthy server's tool list is re-read this often.
const RELIST_AFTER: Duration = Duration::from_secs(600);
/// Descriptions are clipped: a long list of long descriptions hurts a small model.
const MAX_DESCRIPTION: usize = 240;
/// Tool names a catalog line spells out before "+N more".
const CATALOG_NAMES: usize = 8;

/// The configured MCP servers, as one [`ToolSource`].
pub struct McpSource {
    servers: Vec<Arc<McpServer>>,
}

impl McpSource {
    pub fn new(configs: &[McpServerConfig]) -> Result<Self, CosError> {
        let mut servers = Vec::new();
        for c in configs {
            c.validate()?;
            servers.push(Arc::new(McpServer::new(c.clone())));
        }
        Ok(Self { servers })
    }
}

#[async_trait]
impl ToolSource for McpSource {
    async fn refresh(&self) {
        futures_util::future::join_all(self.servers.iter().map(|s| s.refresh())).await;
    }

    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.servers.iter().flat_map(|s| s.view().tools.into_iter().map(|t| t as Arc<dyn Tool>)).collect()
    }

    fn catalog(&self) -> Vec<String> {
        self.servers.iter().filter_map(|s| s.catalog_line()).collect()
    }

    fn unavailable(&self) -> Vec<(String, String)> {
        self.servers.iter().filter_map(|s| s.view().down.map(|why| (format!("mcp:{}", s.cfg.id), why))).collect()
    }
}

#[derive(Default, Clone)]
struct View {
    tools: Vec<Arc<McpTool>>,
    /// Why the server cannot be reached right now.
    down: Option<String>,
}

#[derive(Default)]
struct Conn {
    session: Option<Arc<McpSession>>,
    last_attempt: Option<Instant>,
    listed_at: Option<Instant>,
}

pub(crate) struct McpServer {
    cfg: McpServerConfig,
    conn: tokio::sync::Mutex<Conn>,
    view: RwLock<View>,
    /// Credential values of the live connection, for scrubbing.
    secrets: Mutex<Vec<String>>,
}

impl McpServer {
    fn new(cfg: McpServerConfig) -> Self {
        Self { cfg, conn: Default::default(), view: Default::default(), secrets: Default::default() }
    }

    fn view(&self) -> View {
        self.view.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn secs(v: Option<u64>, default: u64) -> Duration {
        Duration::from_secs(v.unwrap_or(default).max(1))
    }

    /// Remove every credential value this server has used from `text`.
    fn scrub(&self, text: &str) -> String {
        let mut out = text.to_string();
        for s in self.secrets.lock().unwrap_or_else(|e| e.into_inner()).iter() {
            out = out.replace(s.as_str(), "[redacted]");
        }
        crate::events::redact(&out)
    }

    fn set_down(&self, why: &str) {
        self.view.write().unwrap_or_else(|e| e.into_inner()).down = Some(clip(why, 160));
    }

    /// Resolve credentials and open a handshaken session.
    async fn open(&self) -> Result<Arc<McpSession>, String> {
        let mut secrets = Vec::new();
        let transport: Box<dyn McpTransport> = match &self.cfg.transport {
            McpTransportConfig::Stdio { command, args, env } => {
                let mut vars = HashMap::new();
                for name in env {
                    let v = std::env::var(name).map_err(|_| format!("environment variable {name} is not set"))?;
                    if v.len() >= 6 {
                        secrets.push(v.clone());
                    }
                    vars.insert(name.clone(), v);
                }
                *self.secrets.lock().unwrap_or_else(|e| e.into_inner()) = secrets;
                Box::new(StdioTransport::new(command, args, &vars).await.map_err(|e| self.scrub(&format!("cannot start `{command}`: {e}")))?)
            }
            McpTransportConfig::Http { url } => {
                let token = self.bearer()?;
                if let Some(t) = &token {
                    secrets.push(t.clone());
                }
                *self.secrets.lock().unwrap_or_else(|e| e.into_inner()) = secrets;
                let t = HttpTransport::with_bearer(url.clone(), token.as_deref(), Self::secs(self.cfg.call_timeout_secs, DEFAULT_CALL_SECS))
                    .map_err(|e| self.scrub(&e.to_string()))?;
                Box::new(t)
            }
        };
        let wait = Self::secs(self.cfg.connect_timeout_secs, DEFAULT_CONNECT_SECS);
        match tokio::time::timeout(wait, McpSession::connect(transport)).await {
            Err(_) => Err(format!("no answer within {}s", wait.as_secs())),
            Ok(Err(e)) => Err(self.scrub(&e.to_string())),
            Ok(Ok(s)) => Ok(Arc::new(s)),
        }
    }

    fn bearer(&self) -> Result<Option<String>, String> {
        if let Some(name) = &self.cfg.auth.bearer_env {
            return match std::env::var(name) {
                Ok(v) if !v.trim().is_empty() => Ok(Some(v.trim().to_string())),
                _ => Err(format!("environment variable {name} is not set")),
            };
        }
        let Some(path) = &self.cfg.auth.bearer_file else { return Ok(None) };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(path).map_err(|e| format!("token file {}: {e}", path.display()))?.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                return Err(format!("token file {} has mode {mode:o}; it must be 0600", path.display()));
            }
        }
        let raw = std::fs::read_to_string(path).map_err(|e| format!("token file {}: {e}", path.display()))?;
        let raw = raw.trim();
        if raw.is_empty() {
            return Err(format!("token file {} is empty", path.display()));
        }
        Ok(Some(raw.to_string()))
    }

    /// Connect if needed and read the tool list into the view.
    async fn connect_and_list(self: &Arc<Self>, conn: &mut Conn) -> Result<(), String> {
        let session = match &conn.session {
            Some(s) if s.is_alive().await => s.clone(),
            _ => {
                let s = self.open().await?;
                conn.session = Some(s.clone());
                s
            }
        };
        let wait = Self::secs(self.cfg.connect_timeout_secs, DEFAULT_CONNECT_SECS);
        let defs = match tokio::time::timeout(wait, session.list_tools()).await {
            Err(_) => {
                conn.session = None;
                return Err(format!("tools/list timed out after {}s", wait.as_secs()));
            }
            Ok(Err(e)) => {
                conn.session = None;
                return Err(self.scrub(&e.to_string()));
            }
            Ok(Ok(d)) => d,
        };
        let tools = self.register(defs);
        let mut v = self.view.write().unwrap_or_else(|e| e.into_inner());
        v.tools = tools;
        v.down = None;
        conn.listed_at = Some(Instant::now());
        Ok(())
    }

    /// The allow-listed tools, named `<server>.<tool>`; first wins a clash.
    fn register(self: &Arc<Self>, defs: Vec<ToolDefinition>) -> Vec<Arc<McpTool>> {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for d in defs {
            if !self.cfg.allow.iter().any(|p| glob_match(p, &d.name)) {
                continue;
            }
            let local: String = d.name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
            let id = format!("{}.{local}", self.cfg.id);
            if !seen.insert(id.clone()) {
                tracing::warn!(server = %self.cfg.id, tool = %d.name, "duplicate MCP tool name after sanitising; skipped");
                continue;
            }
            let schema = if d.input_schema.is_object() { d.input_schema.clone() } else { json!({"type": "object"}) };
            out.push(Arc::new(McpTool {
                server: self.clone(),
                remote: d.name.clone(),
                id: intern(id),
                description: intern(clip(&format!("[{}] {}", self.cfg.id, d.description.trim()), MAX_DESCRIPTION)),
                parameters: schema,
                gate: self.gate_for(&d.name),
            }));
        }
        out
    }

    fn gate_for(&self, tool: &str) -> Gate {
        let pick = |g: McpGate| if g == McpGate::Open { Gate::Open } else { Gate::Confirm };
        if let Some(g) = self.cfg.tool_gates.get(tool) {
            return pick(*g);
        }
        // Longest matching glob wins; ties fall to map order.
        let best = self.cfg.tool_gates.iter().filter(|(p, _)| glob_match(p, tool)).max_by_key(|(p, _)| p.len());
        pick(best.map(|(_, g)| *g).unwrap_or(self.cfg.gate))
    }

    /// Turn-start reconnect: skipped while healthy and fresh, and while a
    /// recent attempt failed.
    async fn refresh(self: &Arc<Self>) {
        if self.cfg.allow.is_empty() {
            return;
        }
        let mut conn = self.conn.lock().await;
        let healthy = self.view().down.is_none() && conn.session.is_some();
        if healthy && conn.listed_at.is_some_and(|t| t.elapsed() < RELIST_AFTER) {
            return;
        }
        if !healthy && conn.last_attempt.is_some_and(|t| t.elapsed() < Duration::from_secs(self.cfg.retry_secs.unwrap_or(DEFAULT_RETRY_SECS))) {
            return;
        }
        conn.last_attempt = Some(Instant::now());
        match self.connect_and_list(&mut conn).await {
            Ok(()) => tracing::info!(server = %self.cfg.id, tools = self.view().tools.len(), "MCP server ready"),
            Err(e) => {
                tracing::warn!(server = %self.cfg.id, reason = %e, "MCP server unavailable");
                if !healthy || self.view().down.is_none() {
                    self.set_down(&e);
                }
            }
        }
    }

    async fn call(self: &Arc<Self>, tool: &McpTool, args: Value) -> Result<ToolOutcome, CosError> {
        let id = tool.id;
        let sid = &self.cfg.id;
        let session = {
            let mut conn = self.conn.lock().await;
            match &conn.session {
                Some(s) if s.is_alive().await => s.clone(),
                _ => match self.connect_and_list(&mut conn).await {
                    Ok(()) => conn.session.clone().expect("session set by connect_and_list"),
                    Err(e) => {
                        self.set_down(&e);
                        return Err(tool_err(id, format!("server `{sid}` is unavailable: {}", clip(&e, 160))));
                    }
                },
            }
        };
        let wait = Self::secs(self.cfg.call_timeout_secs, DEFAULT_CALL_SECS);
        let raw = match tokio::time::timeout(wait, session.call_tool(&tool.remote, args)).await {
            Err(_) => {
                self.drop_session(&session).await;
                return Err(tool_err(id, format!("server `{sid}` did not answer within {}s", wait.as_secs())));
            }
            Ok(Err(e)) => {
                if matches!(e, ServiceError::McpTransport(_)) {
                    self.drop_session(&session).await;
                }
                return Err(tool_err(id, self.scrub(&e.to_string())));
            }
            Ok(Ok(v)) => v,
        };
        let (text, is_error) = render(&raw);
        let text = self.scrub(&text);
        if is_error {
            return Err(tool_err(id, clip(&text, 400)));
        }
        let max = self.cfg.max_output_bytes.unwrap_or(DEFAULT_MAX_OUTPUT).max(200);
        let (shown, more) = truncate(&text, max);
        let summary = match more {
            Some(n) => format!("{id}: {} bytes (+{n} more)", shown.len()),
            None => format!("{id}: {} bytes", shown.len()),
        };
        Ok(ToolOutcome { summary, result: json!({"server": sid, "tool": tool.remote, "output": shown, "more": more.map(|n| format!("+{n} more bytes; ask for less"))}) })
    }

    async fn drop_session(&self, dead: &Arc<McpSession>) {
        let mut conn = self.conn.lock().await;
        if conn.session.as_ref().is_some_and(|s| Arc::ptr_eq(s, dead)) {
            conn.session = None;
        }
        self.set_down("connection lost");
    }

    fn catalog_line(&self) -> Option<String> {
        if self.cfg.allow.is_empty() {
            return None;
        }
        let v = self.view();
        let use_for = self.cfg.use_for.as_deref().map(|u| format!(" Use for: {}.", u.trim().trim_end_matches('.'))).unwrap_or_default();
        if let Some(why) = &v.down {
            return Some(format!("`{}` is unavailable ({}); its tools are not offered until it recovers.{use_for}", self.cfg.id, clip(why, 100)));
        }
        let mut names: Vec<String> = v.tools.iter().map(|t| format!("`{}`", t.id.replace('.', "_"))).collect();
        let more = names.len().saturating_sub(CATALOG_NAMES);
        names.truncate(CATALOG_NAMES);
        let tail = if more > 0 { format!(", +{more} more") } else { String::new() };
        Some(format!("`{}` ({} tools: {}{tail}).{use_for}", self.cfg.id, v.tools.len(), names.join(", ")))
    }
}

pub(crate) struct McpTool {
    server: Arc<McpServer>,
    remote: String,
    id: &'static str,
    description: &'static str,
    parameters: Value,
    gate: Gate,
}

#[async_trait]
impl Tool for McpTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec { id: self.id, description: self.description, parameters: self.parameters.clone(), gate: self.gate }
    }

    async fn call(&self, args: Value) -> Result<ToolOutcome, CosError> {
        self.server.call(self, args).await
    }
}

/// `*` matches any run of characters; everything else matches itself.
pub(crate) fn glob_match(pat: &str, s: &str) -> bool {
    let (p, s): (Vec<char>, Vec<char>) = (pat.chars().collect(), s.chars().collect());
    let (mut pi, mut si, mut star, mut mark) = (0, 0, None, 0);
    while si < s.len() {
        if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = si;
            pi += 1;
        } else if pi < p.len() && p[pi] == s[si] {
            pi += 1;
            si += 1;
        } else if let Some(st) = star {
            pi = st + 1;
            mark += 1;
            si = mark;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

/// Text of an MCP `tools/call` result and whether the server flagged an error.
fn render(raw: &Value) -> (String, bool) {
    let is_error = raw.get("isError").and_then(Value::as_bool).unwrap_or(false);
    let parts: Vec<String> = raw
        .get("content")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .map(|b| match (b.get("type").and_then(Value::as_str), b.get("text").and_then(Value::as_str)) {
                    (Some("text"), Some(t)) => t.to_string(),
                    (Some(kind), _) => format!("[{kind} content omitted]"),
                    _ => String::new(),
                })
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    if parts.is_empty() {
        (serde_json::to_string(raw).unwrap_or_default(), is_error)
    } else {
        (parts.join("\n"), is_error)
    }
}

/// At most `max` bytes, cut on a character boundary, and how many were dropped.
pub(crate) fn truncate(text: &str, max: usize) -> (String, Option<usize>) {
    if text.len() <= max {
        return (text.to_string(), None);
    }
    let mut cut = max;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    (text[..cut].to_string(), Some(text.len() - cut))
}

fn clip(s: &str, max: usize) -> String {
    let one_line = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= max {
        one_line
    } else {
        format!("{}...", one_line.chars().take(max).collect::<String>())
    }
}

/// Tool ids and descriptions live for the process (`ToolSpec` borrows
/// `'static`); interning keeps a re-listed server from growing that.
fn intern(s: String) -> &'static str {
    static POOL: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();
    let mut pool = POOL.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
    if let Some(hit) = pool.get(s.as_str()) {
        return hit;
    }
    let leaked: &'static str = Box::leak(s.into_boxed_str());
    pool.insert(leaked);
    leaked
}

fn words(text: &str) -> HashSet<String> {
    const STOP: &[&str] = &["the", "and", "for", "with", "that", "this", "from", "what", "into", "your", "can", "are", "any", "get"];
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() >= 3 && !STOP.contains(w))
        .map(|w| w.strip_suffix('s').filter(|r| r.len() >= 3).unwrap_or(w).to_string())
        .collect()
}

/// At most `cap` of `specs`: all of them when they fit, otherwise the best
/// keyword matches for `request` (name hits count double), original order
/// breaking ties.
pub(crate) fn select_relevant(specs: Vec<ToolSpec>, request: &str, cap: usize) -> Vec<ToolSpec> {
    if specs.len() <= cap {
        return specs;
    }
    let ask = words(request);
    let mut scored: Vec<(usize, usize, ToolSpec)> = specs
        .into_iter()
        .enumerate()
        .map(|(i, s)| {
            let name = words(s.id);
            let desc = words(s.description);
            let score = ask.iter().map(|w| 2 * usize::from(name.contains(w)) + usize::from(desc.contains(w))).sum();
            (score, i, s)
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.truncate(cap);
    scored.sort_by_key(|(_, i, _)| *i);
    scored.into_iter().map(|(_, _, s)| s).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs_match_stars_only() {
        assert!(glob_match("search*", "search_flights") && glob_match("*", "x") && glob_match("a*c*e", "abcde"));
        assert!(glob_match("echo", "echo") && !glob_match("echo", "echoes") && !glob_match("get_*", "set_x"));
        assert!(!glob_match("", "x") && glob_match("", ""));
    }

    #[test]
    fn truncation_keeps_char_boundaries_and_counts_the_rest() {
        assert_eq!(truncate("short", 10), ("short".into(), None));
        let (t, more) = truncate("ééééé", 5);
        assert_eq!((t.as_str(), more), ("éé", Some(6)));
    }

    #[test]
    fn render_joins_text_and_flags_errors() {
        let (t, e) = render(&json!({"content": [{"type": "text", "text": "a"}, {"type": "image", "data": "x"}, {"type": "text", "text": "b"}], "isError": true}));
        assert_eq!((t.as_str(), e), ("a\n[image content omitted]\nb", true));
        assert!(!render(&json!({"weird": 1})).1);
    }

    #[test]
    fn relevance_keeps_matching_tools_when_over_the_cap() {
        let spec = |id: &'static str, d: &'static str| ToolSpec { id, description: d, parameters: json!({}), gate: Gate::Open };
        let all = vec![spec("x.alpha", "weather"), spec("x.flight_search", "find flights"), spec("x.beta", "stocks"), spec("x.hotel_search", "find hotels")];
        let got: Vec<_> = select_relevant(all.clone(), "any flights to Lisbon?", 2).iter().map(|s| s.id).collect();
        assert_eq!(got.len(), 2);
        assert!(got.contains(&"x.flight_search"));
        assert_eq!(select_relevant(all, "anything", 9).len(), 4);
    }
}
