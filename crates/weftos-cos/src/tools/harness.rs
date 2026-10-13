//! Dashboard harness client and the two tools that use it:
//! `projects.activity` (read) and `notes.record` (a `kind: note` history
//! entry). The credential is loaded from a mode-0600 file whose path is in
//! config; it lives in [`SecretToken`], which never prints, and is sent only
//! as the `Authorization` header.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{Gate, Tool, ToolOutcome, ToolSpec, arg_str, tool_err};
use crate::CosError;
use crate::config::HarnessConfig;

/// The outcome of a project-history read.
#[derive(Debug, Clone, PartialEq)]
pub enum History {
    /// One page, newest first; `next_before` is passed back verbatim for more.
    Page { items: Vec<Value>, next_before: Option<String> },
    /// HTTP 404: the project is not reachable with this credential.
    NoAccess,
    /// HTTP 400: a parameter was refused.
    BadRequest(String),
    /// Network failure, 5xx or a rejected credential.
    Unavailable(String),
}

impl History {
    fn to_json(&self) -> Value {
        match self {
            Self::Page { items, next_before } => json!({"available": true, "items": items, "next_before": next_before}),
            Self::NoAccess => json!({"available": false, "reason": "no history access for this project"}),
            Self::BadRequest(m) => json!({"available": false, "reason": format!("history request refused: {m}")}),
            Self::Unavailable(m) => json!({"available": false, "reason": format!("history unavailable right now: {m}")}),
        }
    }
}

/// A harness credential. `Debug` and `Display` never show the value.
#[derive(Clone)]
pub struct SecretToken(String);

impl std::fmt::Debug for SecretToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretToken([redacted])")
    }
}

pub(crate) fn is_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
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
    project_id: Option<String>,
    token: SecretToken,
    http: reqwest::Client,
}

impl HarnessClient {
    pub fn from_config(cfg: &HarnessConfig) -> Result<Self, CosError> {
        let token = SecretToken::from_file(&cfg.token_file)?;
        Ok(Self::new(&cfg.base_url, &cfg.events_path, cfg.project_id.clone(), token))
    }

    /// `events_path` may contain `{project_id}`, filled from `project_id`
    /// (dashboard PR #19: `/api/harness/projects/[id]/events`).
    pub fn new(base_url: &str, events_path: &str, project_id: Option<String>, token: SecretToken) -> Self {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .expect("reqwest client");
        Self { base: base_url.trim_end_matches('/').to_string(), events_path: events_path.into(), project_id, token, http }
    }

    /// The project-history path for `project` (falling back to the
    /// configured default project), or a clear error when the template
    /// needs a project id and none can be resolved.
    pub fn events_path_for(&self, project: Option<&str>) -> Result<String, CosError> {
        if !self.events_path.contains("{project_id}") {
            return Ok(self.events_path.clone());
        }
        match project.map(str::to_string).or_else(|| self.project_id.clone()) {
            Some(id) if is_uuid(&id) => Ok(self.events_path.replace("{project_id}", &id)),
            Some(id) => Err(tool_err("harness", format!("project id {id:?} is not a UUID"))),
            None => Err(tool_err("harness", "no project to record against: give `project`, a ticket, or set harness.project_id")),
        }
    }

    /// The default project's history path.
    pub fn events_path(&self) -> Result<String, CosError> {
        self.events_path_for(None)
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

    /// Resolve `WEFT-N`, a `source_id`, a UUID or a unique UUID prefix of at
    /// least 8 characters (what `projects.activity` shows) to a ticket.
    pub async fn resolve_ticket(&self, reference: &str) -> Result<Value, CosError> {
        let upper = reference.to_ascii_uppercase();
        let all = self.tickets(None, 10_000).await?;
        if let Some(t) = all.iter().find(|t| {
            t["id"] == reference || t["source_id"] == reference || t["source_url"].as_str().is_some_and(|u| u.ends_with(&upper))
        }) {
            return Ok(t.clone());
        }
        let prefix = reference.to_ascii_lowercase();
        if prefix.len() >= 8 && prefix.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
            let hits: Vec<&Value> = all.iter().filter(|t| t["id"].as_str().is_some_and(|id| id.starts_with(&prefix))).collect();
            match hits.as_slice() {
                [one] => return Ok((*one).clone()),
                [] => {}
                _ => return Err(tool_err("harness", format!("ticket reference {reference} is ambiguous; give more characters"))),
            }
        }
        Err(tool_err("harness", format!("ticket not found: {reference}")))
    }

    /// Read one page of project history (dashboard PR #21: GET on the events
    /// route, newest first). `query` holds the optional `kind`, `since`,
    /// `limit` and `before` parameters, sent URL-encoded and unmodified.
    /// Never an `Err`: the outcome says plainly whether history was read,
    /// is not accessible, or is unavailable right now. The token is only
    /// ever the Authorization header and appears in no message here.
    pub async fn history(&self, project: Option<&str>, query: &[(&str, String)]) -> History {
        let path = match self.events_path_for(project) {
            Ok(p) => p,
            Err(e) => return History::Unavailable(format!("no project to read history for ({e})")),
        };
        let resp = match self.http.get(self.url(&path)).bearer_auth(&self.token.0).query(query).send().await {
            Ok(r) => r,
            Err(_) => return History::Unavailable("the dashboard could not be reached".into()),
        };
        let status = resp.status().as_u16();
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        match status {
            200 => History::Page {
                items: body.get("items").and_then(Value::as_array).cloned().unwrap_or_default(),
                next_before: body.get("next_before").and_then(Value::as_str).map(str::to_string),
            },
            404 => History::NoAccess,
            400 => History::BadRequest(body.get("error").and_then(Value::as_str).unwrap_or("invalid parameter").to_string()),
            401 => History::Unavailable("the dashboard rejected the credential".into()),
            n => History::Unavailable(format!("the dashboard answered HTTP {n}")),
        }
    }

    /// Add one project-history event to `project` (or the default project):
    /// `{kind, title, summary?, details_md?, dedupe_key?, occurred_at?}`
    /// (PR #19 body shape).
    pub async fn post_event(&self, project: Option<&str>, body: &Value) -> Result<Value, CosError> {
        let path = self.events_path_for(project)?;
        self.post(&path, body).await
    }
}

/// `projects.activity`: board items, goals and (when the route exists)
/// project history, condensed for the model.
pub struct ProjectsActivity {
    pub client: Arc<HarnessClient>,
}

const ACTIVITY: &str = "projects.activity";

pub(crate) fn condense_ticket(t: &Value) -> Value {
    // `WEFT-N` when there is one, else the first 8 characters of the id: a
    // full UUID in a prompt invites a small model to loop on its hex, and
    // `board.read` resolves the short form.
    let short = t["source_url"]
        .as_str()
        .and_then(|u| u.rsplit('/').next())
        .filter(|s| s.starts_with("WEFT-"))
        .map(str::to_string)
        .unwrap_or_else(|| t["id"].as_str().unwrap_or("").chars().take(8).collect());
    json!({
        "ref": short,
        "status": t["status"],
        "priority": t["priority"],
        "title": t["title"],
        "assignee": t.get("assignee").cloned().unwrap_or(Value::Null),
        "updated_at": t.get("updated_at").cloned().unwrap_or(Value::Null),
    })
}

/// One short list row: real ref, status and a clipped title. No nulls, no
/// timestamps; a list of these stays small enough for a local model to copy
/// faithfully instead of continuing the pattern with filler.
pub(crate) fn row_ticket(t: &Value) -> Value {
    let c = condense_ticket(t);
    let full = c["title"].as_str().unwrap_or("");
    let title = if full.chars().count() > 90 { format!("{}…", full.chars().take(89).collect::<String>()) } else { full.to_string() };
    let mut row = json!({"ref": c["ref"], "status": c["status"], "title": title});
    if !c["priority"].is_null() {
        row["priority"] = c["priority"].clone();
    }
    row
}

/// The first `limit` rows plus an honest count of the rest.
pub(crate) fn bounded_rows(tickets: &[Value], limit: usize) -> (Vec<Value>, Option<String>) {
    let rows = tickets.iter().take(limit).map(row_ticket).collect();
    let more = tickets.len().saturating_sub(limit);
    (rows, (more > 0).then(|| format!("+{more} more; narrow with a status or use board.search")))
}

/// A goal for the model: the outcome text stays on the board (a goal's
/// description runs to a kilobyte and buried the tickets in the prompt).
fn condense_goal(g: &Value) -> Value {
    json!({"title": g["title"], "status": g["status"], "target_date": g.get("target_date").cloned().unwrap_or(Value::Null)})
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
                    "limit": {"type": "integer", "minimum": 1, "maximum": 25, "default": 10},
                    "project": {"type": "string", "description": "Project UUID for history; the configured default when omitted."},
                    "history_limit": {"type": "integer", "minimum": 1, "maximum": 200, "default": 10,
                                      "description": "History entries to read, newest first."},
                    "history_kind": {"type": "string", "enum": ["milestone", "install", "route", "release", "ticket", "pair", "note"]},
                    "history_since": {"type": "string", "description": "ISO 8601 with timezone; only entries strictly after."},
                    "history_before": {"type": "string", "description": "The next_before of the previous page, verbatim. Only when asked for more."}
                }
            }),
            gate: Gate::Open,
        }
    }

    async fn call(&self, args: Value) -> Result<ToolOutcome, CosError> {
        let status = arg_str(&args, "status");
        let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(10).clamp(1, 25) as usize;
        let tickets = self.client.tickets(status.as_deref(), 500).await?;
        let goals = self
            .client
            .get("/api/harness/goals?limit=100&offset=0")
            .await
            .ok()
            .and_then(|g| g.get("items").and_then(Value::as_array).cloned())
            .unwrap_or_default()
            .iter()
            .map(condense_goal)
            .collect::<Vec<_>>();
        let project = arg_str(&args, "project");
        if let Some(p) = &project
            && !is_uuid(p)
        {
            return Err(tool_err(ACTIVITY, "project must be a UUID"));
        }
        let hlimit = args.get("history_limit").and_then(Value::as_u64).unwrap_or(10).clamp(1, 200);
        let mut query = vec![("limit", hlimit.to_string())];
        for (arg, param) in [("history_kind", "kind"), ("history_since", "since"), ("history_before", "before")] {
            if let Some(v) = arg_str(&args, arg) {
                query.push((param, v));
            }
        }
        let history = self.client.history(project.as_deref(), &query).await.to_json();
        let (condensed, more) = bounded_rows(&tickets, limit);
        let summary = format!(
            "read {} of {} ticket(s){} and {} goal(s); history {}",
            condensed.len(),
            tickets.len(),
            status.map(|s| format!(" in {s}")).unwrap_or_default(),
            goals.len(),
            if history["available"] == true { "read" } else { history["reason"].as_str().unwrap_or("unavailable") }
        );
        Ok(ToolOutcome { summary, result: json!({"total": tickets.len(), "shown": condensed.len(), "tickets": condensed, "more": more, "goals": goals, "history": history}) })
    }
}

pub use super::notes::NotesRecord;

#[cfg(test)]
mod tests {
    use super::*;

    fn t(n: usize) -> Value {
        json!({"id": format!("{n:08x}-0000-0000-0000-000000000000"), "status": "in_progress", "priority": null,
               "title": format!("Real card number {n}"), "source_url": format!("https://x/WEFT-{n}")})
    }

    #[test]
    fn rows_are_bounded_real_and_counted() {
        let all: Vec<Value> = (1..=26).map(t).collect();
        let (rows, more) = bounded_rows(&all, 10);
        assert_eq!(rows.len(), 10);
        assert_eq!(rows[0]["ref"], "WEFT-1");
        assert_eq!(rows[0]["title"], "Real card number 1");
        assert!(rows[0].get("priority").is_none() && rows[0].get("updated_at").is_none());
        assert_eq!(more.as_deref(), Some("+16 more; narrow with a status or use board.search"));
        let (rows, more) = bounded_rows(&all[..3], 10);
        assert_eq!((rows.len(), more), (3, None));
    }

    #[test]
    fn long_titles_are_clipped() {
        let mut c = t(1);
        c["title"] = json!("x".repeat(400));
        assert_eq!(row_ticket(&c)["title"].as_str().unwrap().chars().count(), 90);
    }
}
