//! `notes.record`: a project-history note, a card comment and an entry in the
//! installation brain, written together or not at all.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::harness::{HarnessClient, is_uuid};
use super::{Gate, Tool, ToolOutcome, ToolSpec, arg_str, need_str, tool_err};
use crate::CosError;
use crate::brain::{Brain, NewEntry};

/// `notes.record`: a `kind: note` project-history entry on the project the
/// note is about, resolved from an explicit `project`, else the ticket's
/// project, else the configured default; refused when none resolves. With
/// a ticket reference the note also lands as that ticket's comment. With a
/// brain bound (ADR-121 decision 9) the note is also written to the
/// installation brain with its metadata; a note the brain's schema refuses
/// is refused before anything is written anywhere.
pub struct NotesRecord {
    pub client: Option<Arc<HarnessClient>>,
    pub brain: Option<Arc<Brain>>,
}

const NOTES: &str = "notes.record";

#[async_trait]
impl Tool for NotesRecord {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            id: NOTES,
            description: "Record a note: project history (kind: note) on the project it is about, the installation brain, and with a ticket also a comment on that card.",
            parameters: json!({
                "type": "object",
                "required": ["text"],
                "properties": {
                    "text": {"type": "string", "minLength": 3, "maxLength": 4000},
                    "ticket": {"type": "string", "description": "WEFT-N, source id or UUID of the card the note is about."},
                    "project": {"type": "string", "description": "Project UUID when the note is about a project other than the ticket's or the default."},
                    "kind": {"type": "string", "enum": ["decision", "finding", "note", "handoff", "learning"], "description": "Brain entry kind; default note."},
                    "why": {"type": "string", "description": "The reason, so a later reader can tell whether it still holds."},
                    "where": {"type": "array", "items": {"type": "string"}, "description": "Cards, repository paths or weftos:// names it applies to; never an absolute machine path."}
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
        let mut project = arg_str(&args, "project");
        if let Some(p) = &project
            && !is_uuid(p)
        {
            return Err(tool_err(NOTES, "project must be a UUID"));
        }
        let ticket = arg_str(&args, "ticket");
        let (title, summary, details_md) = split_note(&text);

        // Refuse a note the brain's schema refuses before writing anywhere.
        let entry = self.brain.as_ref().map(|_| brain_entry(&args, &text, &title, ticket.as_deref(), project.as_deref())).transpose()?;
        if let (Some(brain), Some(entry)) = (&self.brain, &entry) {
            brain.metadata(entry, 0).map_err(|e| tool_err(NOTES, e))?;
        }

        let mut board = Value::Null;
        let mut history = Value::Null;
        let mut on = String::new();
        if let Some(client) = &self.client {
            if let Some(reference) = &ticket {
                let t = client.resolve_ticket(reference).await?;
                let id = t["id"].as_str().unwrap_or_default().to_string();
                if project.is_none() {
                    project = t["project_id"].as_str().filter(|p| is_uuid(p)).map(str::to_string);
                }
                let body = json!({"ticket_id": id, "action": "comment", "value": format!("note: {text}")});
                board = json!({"ticket_id": id, "comment": client.post("/api/harness/tickets", &body).await?});
                on = format!(" on {reference}");
            }
            let mut body = json!({"kind": "note", "title": title, "summary": summary, "occurred_at": at});
            if let Some(d) = details_md {
                body["details_md"] = Value::String(d);
            }
            history = client.post_event(project.as_deref(), &body).await?;
        }
        let mut places = Vec::new();
        if self.client.is_some() {
            let dedupe = if history["created"] == false { " (already recorded)" } else { "" };
            places.push(format!("project history{on}{dedupe}"));
        }
        let mut brain_out = Value::Null;
        if let (Some(brain), Some(entry)) = (&self.brain, &entry) {
            match brain.record(entry) {
                Ok(r) => {
                    places.push("the installation brain".into());
                    brain_out = json!({"id": r.id, "installation": brain.installation()});
                }
                Err(e) => brain_out = json!({"error": e.to_string()}),
            }
        }
        if places.is_empty() {
            return Err(tool_err(NOTES, "nowhere to record: no harness credential and no brain bound"));
        }
        Ok(ToolOutcome {
            summary: format!("note recorded in {}", places.join(" and ")),
            result: json!({"kind": "note", "at": at, "project_id": project, "history": history, "board": board, "brain": brain_out}),
        })
    }
}

/// The brain entry for a note: `why` and `where` come from the arguments,
/// else from what the note is about.
fn brain_entry(args: &Value, text: &str, title: &str, ticket: Option<&str>, project: Option<&str>) -> Result<NewEntry, CosError> {
    let kind = arg_str(args, "kind").unwrap_or_else(|| "note".into());
    let mut where_: Vec<String> = args
        .get("where")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default();
    if where_.is_empty() {
        where_.push(ticket.map(str::to_string).or_else(|| project.map(|p| format!("project:{p}"))).unwrap_or_else(|| "cos-console".into()));
    }
    let summary = if text.chars().count() <= 500 { text.to_string() } else { title.chars().take(500).collect() };
    Ok(NewEntry {
        kind,
        summary,
        why: arg_str(args, "why").unwrap_or_else(|| "recorded at the owner's request in a console turn".into()),
        where_,
        card: ticket.map(str::to_string),
        ..Default::default()
    })
}

/// PR #19 limits: title <= 200, summary <= 1000, details_md <= 20000. The
/// first line (or the first 200 chars) is the title; the rest is the
/// summary, and the whole note goes to `details_md` when it does not fit.
fn split_note(text: &str) -> (String, String, Option<String>) {
    let clip = |s: &str, n: usize| -> String {
        if s.chars().count() <= n { s.to_string() } else { format!("{}…", s.chars().take(n - 1).collect::<String>()) }
    };
    let (first, rest) = match text.split_once('\n') {
        Some((f, r)) => (f.trim(), r.trim()),
        None => (text.trim(), ""),
    };
    let title = clip(first, 200);
    let summary = clip(rest, 1000);
    let fits = title == first && summary == rest;
    (title, summary, if fits { None } else { Some(clip(text.trim(), 20_000)) })
}
