//! Dashboard harness client and the two tools that use it:
//! `projects.activity` (read) and `notes.record` (a `kind: note` history
//! entry). The credential is loaded from a mode-0600 file whose path is in
//! config; it lives in [`SecretToken`], which never prints, and is sent only
//! as the `Authorization` header.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{Gate, Tool, ToolOutcome, ToolSpec, arg_str, need_str, tool_err};
use crate::CosError;
use crate::config::HarnessConfig;

/// A harness credential. `Debug` and `Display` never show the value.
#[derive(Clone)]
pub struct SecretToken(String);

impl std::fmt::Debug for SecretToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretToken([redacted])")
    }
}

fn is_wfb(token: &str) -> bool {
    token.len() == 68
        && token.starts_with("wfb_")
        && token[4..].chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

impl SecretToken {
    /// Read the token from `path`. Refuses a file readable by group or
    /// others (anything but 0600/0400 on unix) and a value that is not a
    /// `wfb_` + 64 hex credential.
    pub fn from_file(path: &Path) -> Result<Self, CosError> {
        let meta = std::fs::metadata(path)
            .map_err(|e| CosError::Config(format!("token file {}: {e}", path.display())))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = meta.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                return Err(CosError::Config(format!(
                    "token file {} has mode {mode:o}; it must be 0600",
                    path.display()
                )));
            }
        }
        #[cfg(not(unix))]
        let _ = meta;
        let raw = std::fs::read_to_string(path)
            .map_err(|e| CosError::Config(format!("token file {}: {e}", path.display())))?;
        let token = raw.trim();
        if !is_wfb(token) {
            return Err(CosError::Config(format!(
                "token file {} does not hold a harness credential",
                path.display()
            )));
        }
        Ok(Self(token.to_string()))
    }

    /// Test-only constructor for a fake harness.
    pub fn for_tests(token: impl Into<String>) -> Self {
        Self(token.into())
    }
}

/// Thin JSON client over the harness routes.
#[derive(Clone)]
pub struct HarnessClient {
    base: String,
    events_path: String,
    token: SecretToken,
    http: reqwest::Client,
}

impl HarnessClient {
    pub fn from_config(cfg: &HarnessConfig) -> Result<Self, CosError> {
        let token = SecretToken::from_file(&cfg.token_file)?;
        Ok(Self::new(&cfg.base_url, &cfg.events_path, token))
    }

    pub fn new(base_url: &str, events_path: &str, token: SecretToken) -> Self {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .expect("reqwest client");
        Self { base: base_url.trim_end_matches('/').to_string(), events_path: events_path.into(), token, http }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    pub async fn get(&self, path: &str) -> Result<Value, CosError> {
        let resp = self
            .http
            .get(self.url(path))
            .bearer_auth(&self.token.0)
            .send()
            .await
            .map_err(|e| tool_err("harness", format!("GET {path}: {e}")))?;
        Self::body(path, resp).await
    }

    pub async fn post(&self, path: &str, body: &Value) -> Result<Value, CosError> {
        let resp = self
            .http
            .post(self.url(path))
            .bearer_auth(&self.token.0)
            .json(body)
            .send()
            .await
            .map_err(|e| tool_err("harness", format!("POST {path}: {e}")))?;
        Self::body(path, resp).await
    }

    async fn body(path: &str, resp: reqwest::Response) -> Result<Value, CosError> {
        let status = resp.status();
        let value: Value = resp.json().await.unwrap_or(Value::Null);
        if !status.is_success() {
            let msg = value.get("error").and_then(Value::as_str).unwrap_or("request failed");
            return Err(tool_err("harness", format!("{path} -> HTTP {}: {msg}", status.as_u16())));
        }
        Ok(value)
    }

    /// All tickets, optionally by status, following the 100-per-page rule.
    pub async fn tickets(&self, status: Option<&str>, max: usize) -> Result<Vec<Value>, CosError> {
        let mut items = Vec::new();
        let mut offset = 0;
        while items.len() < max {
            let mut path = format!("/api/harness/tickets?limit=100&offset={offset}");
            if let Some(s) = status {
                path.push_str(&format!("&status={s}"));
            }
            let page = self.get(&path).await?;
            let Some(arr) = page.get("items").and_then(Value::as_array) else {
                return Err(tool_err("harness", "tickets page has no items"));
            };
            let n = arr.len();
            items.extend(arr.iter().cloned());
            if n < 100 {
                break;
            }
            offset += 100;
        }
        items.truncate(max);
        Ok(items)
    }

    /// Resolve `WEFT-N`, a `source_id` or a UUID to a ticket.
    pub async fn resolve_ticket(&self, reference: &str) -> Result<Value, CosError> {
        let upper = reference.to_ascii_uppercase();
        let all = self.tickets(None, 10_000).await?;
        all.into_iter()
            .find(|t| {
                t["id"] == reference
                    || t["source_id"] == reference
                    || t["source_url"].as_str().is_some_and(|u| u.ends_with(&upper))
            })
            .ok_or_else(|| tool_err("harness", format!("ticket not found: {reference}")))
    }

    pub async fn events(&self, limit: usize) -> Result<Value, CosError> {
        self.get(&format!("{}?limit={limit}", self.events_path)).await
    }

    pub async fn post_event(&self, body: &Value) -> Result<Value, CosError> {
        let path = self.events_path.clone();
        self.post(&path, body).await
    }
}

/// `projects.activity`: board items, goals and (when the route exists)
/// project history, condensed for the model.
pub struct ProjectsActivity {
    pub client: Arc<HarnessClient>,
}

const ACTIVITY: &str = "projects.activity";

fn condense_ticket(t: &Value) -> Value {
    let short = t["source_url"]
        .as_str()
        .and_then(|u| u.rsplit('/').next())
        .filter(|s| s.starts_with("WEFT-"))
        .map(str::to_string)
        .unwrap_or_else(|| t["id"].as_str().unwrap_or("").to_string());
    json!({
        "ref": short,
        "status": t["status"],
        "priority": t["priority"],
        "title": t["title"],
        "assignee": t.get("assignee").cloned().unwrap_or(Value::Null),
        "updated_at": t.get("updated_at").cloned().unwrap_or(Value::Null),
    })
}

#[async_trait]
impl Tool for ProjectsActivity {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            id: ACTIVITY,
            description: "Read board items, goals and recent project history. Read-only.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "status": {"type": "string", "enum": ["todo", "in_progress", "review", "done"],
                               "description": "Only tickets in this status; all when omitted."},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 100, "default": 25}
                }
            }),
            gate: Gate::Open,
        }
    }

    async fn call(&self, args: Value) -> Result<ToolOutcome, CosError> {
        let status = arg_str(&args, "status");
        let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(25).clamp(1, 100) as usize;
        let tickets = self.client.tickets(status.as_deref(), limit).await?;
        let goals = self
            .client
            .get("/api/harness/goals?limit=100&offset=0")
            .await
            .ok()
            .and_then(|g| g.get("items").cloned())
            .unwrap_or(Value::Array(vec![]));
        let history = match self.client.events(20).await {
            Ok(v) => json!({"available": true, "items": v.get("items").cloned().unwrap_or(v)}),
            Err(e) => json!({"available": false, "reason": e.to_string()}),
        };
        let condensed: Vec<Value> = tickets.iter().map(condense_ticket).collect();
        let summary = format!(
            "read {} ticket(s){} and {} goal(s); history {}",
            condensed.len(),
            status.map(|s| format!(" in {s}")).unwrap_or_default(),
            goals.as_array().map_or(0, Vec::len),
            if history["available"] == true { "read" } else { "unavailable" }
        );
        Ok(ToolOutcome { summary, result: json!({"tickets": condensed, "goals": goals, "history": history}) })
    }
}

/// `notes.record`: a `kind: note` project-history entry. With a ticket
/// reference it lands as that ticket's comment (the board's existing write
/// path); without one it goes to the harness events route.
pub struct NotesRecord {
    pub client: Arc<HarnessClient>,
}

const NOTES: &str = "notes.record";

#[async_trait]
impl Tool for NotesRecord {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            id: NOTES,
            description: "Record a project-history note (kind: note). Optionally attached to a ticket.",
            parameters: json!({
                "type": "object",
                "required": ["text"],
                "properties": {
                    "text": {"type": "string", "minLength": 3, "maxLength": 4000},
                    "ticket": {"type": "string", "description": "WEFT-N, source id or UUID of the card the note is about."}
                }
            }),
            gate: Gate::Open,
        }
    }

    async fn call(&self, args: Value) -> Result<ToolOutcome, CosError> {
        let text = need_str(NOTES, &args, "text")?;
        if text.chars().count() > 4000 {
            return Err(tool_err(NOTES, "note is longer than 4000 characters"));
        }
        let at = chrono::Utc::now().to_rfc3339();
        if let Some(reference) = arg_str(&args, "ticket") {
            let ticket = self.client.resolve_ticket(&reference).await?;
            let id = ticket["id"].as_str().unwrap_or_default().to_string();
            let body = json!({"ticket_id": id, "action": "comment", "value": format!("note: {text}")});
            let res = self.client.post("/api/harness/tickets", &body).await?;
            return Ok(ToolOutcome {
                summary: format!("note recorded on {reference}"),
                result: json!({"kind": "note", "ticket_id": id, "at": at, "board": res}),
            });
        }
        let body = json!({"kind": "note", "text": text, "source": "chief-of-staff", "at": at});
        let res = self.client.post_event(&body).await?;
        Ok(ToolOutcome { summary: "note recorded in project history".into(), result: json!({"kind": "note", "at": at, "result": res}) })
    }
}
