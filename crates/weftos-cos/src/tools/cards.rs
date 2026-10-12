//! Board cards through the harness: `board.read` (one card in full),
//! `board.claim` and `board.note` (both change the shared board, so both wait
//! for the owner's yes), and `card.current` (the card the conversation is
//! about). The credential stays inside [`HarnessClient`].

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{Value, json};

use super::harness::{HarnessClient, condense_ticket};
use super::{Gate, Tool, ToolOutcome, ToolSpec, need_str, tool_err};
use crate::CosError;

/// The card the conversation is currently about. Set by reading, claiming or
/// noting a card; `card.current` reports it.
#[derive(Clone, Default)]
pub struct Focus(Arc<Mutex<Option<String>>>);

impl Focus {
    pub fn set(&self, reference: &str) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = Some(reference.to_string());
    }

    pub fn get(&self) -> Option<String> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

fn full_ticket(t: &Value) -> Value {
    let mut v = condense_ticket(t);
    for k in ["description", "project_id", "goal_id", "source_url", "created_at"] {
        if let Some(x) = t.get(k) {
            v[k] = x.clone();
        }
    }
    v
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { format!("{}…", s.chars().take(n - 1).collect::<String>()) }
}

pub struct BoardRead {
    pub client: Arc<HarnessClient>,
    pub focus: Focus,
}

const READ: &str = "board.read";

#[async_trait]
impl Tool for BoardRead {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            id: READ,
            description: "Read one board card in full (WEFT-N, source id or UUID). Read-only.",
            parameters: json!({"type": "object", "required": ["ticket"],
                "properties": {"ticket": {"type": "string", "description": "WEFT-N, source id or UUID."}}}),
            gate: Gate::Open,
        }
    }

    async fn call(&self, args: Value) -> Result<ToolOutcome, CosError> {
        let reference = need_str(READ, &args, "ticket")?;
        let t = self.client.resolve_ticket(&reference).await?;
        self.focus.set(&reference);
        let card = full_ticket(&t);
        let summary = format!("read {reference}: {}", card["title"].as_str().unwrap_or("(untitled)"));
        Ok(ToolOutcome { summary, result: json!({"card": card}) })
    }
}

pub struct BoardClaim {
    pub client: Arc<HarnessClient>,
    pub focus: Focus,
}

const CLAIM: &str = "board.claim";

#[async_trait]
impl Tool for BoardClaim {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            id: CLAIM,
            description: "Claim a board card for the owner's work (moves it in progress). Changes the shared board, so it waits for the owner's yes.",
            parameters: json!({"type": "object", "required": ["ticket"],
                "properties": {"ticket": {"type": "string", "description": "WEFT-N, source id or UUID."}}}),
            gate: Gate::Confirm,
        }
    }

    async fn call(&self, args: Value) -> Result<ToolOutcome, CosError> {
        let reference = need_str(CLAIM, &args, "ticket")?;
        let t = self.client.resolve_ticket(&reference).await?;
        let id = t["id"].as_str().ok_or_else(|| tool_err(CLAIM, "ticket has no id"))?;
        let res = self.client.post("/api/harness/tickets", &json!({"ticket_id": id, "action": "claim"})).await?;
        self.focus.set(&reference);
        Ok(ToolOutcome { summary: format!("claimed {reference}"), result: json!({"claimed": true, "ticket": reference, "board": res}) })
    }
}

pub struct BoardNote {
    pub client: Arc<HarnessClient>,
    pub focus: Focus,
}

const NOTE: &str = "board.note";

#[async_trait]
impl Tool for BoardNote {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            id: NOTE,
            description: "Add a comment to a board card. A comment on a shared card, so it waits for the owner's yes; use notes.record for the project's own history.",
            parameters: json!({"type": "object", "required": ["ticket", "text"], "properties": {
                "ticket": {"type": "string"},
                "text": {"type": "string", "minLength": 3, "maxLength": 4000}}}),
            gate: Gate::Confirm,
        }
    }

    async fn call(&self, args: Value) -> Result<ToolOutcome, CosError> {
        let reference = need_str(NOTE, &args, "ticket")?;
        let text = need_str(NOTE, &args, "text")?;
        if text.chars().count() > 4000 {
            return Err(tool_err(NOTE, "comment is longer than 4000 characters"));
        }
        let t = self.client.resolve_ticket(&reference).await?;
        let id = t["id"].as_str().ok_or_else(|| tool_err(NOTE, "ticket has no id"))?;
        let res = self.client.post("/api/harness/tickets", &json!({"ticket_id": id, "action": "comment", "value": text})).await?;
        self.focus.set(&reference);
        Ok(ToolOutcome { summary: format!("commented on {reference}: {}", clip(&text, 60)), result: json!({"commented": true, "ticket": reference, "board": res}) })
    }
}

pub struct CardCurrent {
    pub client: Arc<HarnessClient>,
    pub focus: Focus,
}

const CURRENT: &str = "card.current";

#[async_trait]
impl Tool for CardCurrent {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            id: CURRENT,
            description: "The card this conversation is about: the last one read, claimed or noted, else the most recently updated in-progress card. Read-only.",
            parameters: json!({"type": "object", "properties": {}}),
            gate: Gate::Open,
        }
    }

    async fn call(&self, _args: Value) -> Result<ToolOutcome, CosError> {
        if let Some(reference) = self.focus.get() {
            let t = self.client.resolve_ticket(&reference).await?;
            let card = full_ticket(&t);
            return Ok(ToolOutcome { summary: format!("current card {reference}"), result: json!({"source": "focus", "card": card}) });
        }
        let mut open = self.client.tickets(Some("in_progress"), 100).await?;
        open.sort_by(|a, b| b["updated_at"].as_str().cmp(&a["updated_at"].as_str()));
        match open.first() {
            Some(t) => {
                let card = full_ticket(t);
                let r = card["ref"].as_str().unwrap_or("?").to_string();
                Ok(ToolOutcome { summary: format!("current card {r} (most recent in progress)"), result: json!({"source": "in_progress", "card": card}) })
            }
            None => Ok(ToolOutcome { summary: "no current card".into(), result: json!({"source": "none", "card": null}) }),
        }
    }
}
