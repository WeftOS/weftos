//! Board cards through the harness: `board.read` (one card in full),
//! `board.claim` and `board.note` (both change the shared board, so both wait
//! for the owner's yes), and `card.current` (the card the conversation is
//! about). The credential stays inside [`HarnessClient`].

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{Value, json};

use super::harness::{HarnessClient, bounded_rows, condense_ticket};
use super::{Gate, Tool, ToolOutcome, ToolSpec, need_str, tool_err};
use crate::CosError;

/// The card the conversation is currently about. Set by reading, claiming or
/// noting a card; `card.current` reports it.
#[derive(Clone, Default)]
pub struct Focus(Arc<Mutex<Option<String>>>, Arc<Mutex<Option<String>>>);

impl Focus {
    pub fn set(&self, reference: &str) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = Some(reference.to_string());
        *self.1.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// Remember the card's title for the status line.
    pub fn set_title(&self, title: &str) {
        *self.1.lock().unwrap_or_else(|e| e.into_inner()) = Some(title.to_string());
    }

    /// `WEFT-N: title` when both are known, else the reference alone.
    pub fn get_titled(&self) -> Option<String> {
        let r = self.get()?;
        let title = self.1.lock().unwrap_or_else(|e| e.into_inner()).clone();
        Some(match title {
            Some(t) => clip(&format!("{r}: {t}"), 48),
            None => r,
        })
    }

    pub fn get(&self) -> Option<String> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

fn full_ticket(t: &Value) -> Value {
    let mut v = condense_ticket(t);
    for k in ["description", "project_id", "goal_id", "source_url", "created_at"] {
        if let Some(x) = t.get(k) {
            v[k] = match (k, x.as_str()) {
                ("description", Some(d)) => json!(clip(d, 800)),
                _ => x.clone(),
            };
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
        if let Some(title) = t["title"].as_str() {
            self.focus.set_title(title);
        }
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
        if let Some(title) = t["title"].as_str() {
            self.focus.set_title(title);
        }
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
        if let Some(title) = t["title"].as_str() {
            self.focus.set_title(title);
        }
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

const STOP: &[&str] = &[
    "the", "and", "for", "are", "what", "whats", "where", "how", "with", "about", "our", "now", "status", "state", "going", "next",
    "on", "in", "of", "to", "is", "we", "it", "a", "an", "at", "do", "does", "any", "tell", "show", "please",
];

fn words(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_lowercase).collect()
}

/// Query terms: lowercase words without filler.
pub(crate) fn query_terms(q: &str) -> Vec<String> {
    words(q).into_iter().filter(|w| !STOP.contains(&w.as_str())).collect()
}

/// Score one card against `terms`: a term hits a title word (3) or a
/// description word (1) by prefix, or the card's `WEFT-N` ref (3). Zero
/// means no match.
pub(crate) fn score_ticket(t: &Value, terms: &[String]) -> usize {
    let title = words(t["title"].as_str().unwrap_or(""));
    let desc = words(t["description"].as_str().unwrap_or(""));
    let reference = condense_ticket(t)["ref"].as_str().unwrap_or("").to_lowercase();
    terms
        .iter()
        .map(|q| {
            let by_ref = reference == *q || reference.rsplit('-').next() == Some(q.as_str()) && q.chars().all(|c| c.is_ascii_digit());
            if by_ref || title.iter().any(|w| w.starts_with(q.as_str())) {
                3
            } else if desc.iter().any(|w| w.starts_with(q.as_str())) {
                1
            } else {
                0
            }
        })
        .sum()
}

/// `board.search`: cards about a topic, found by words in titles and
/// descriptions. The harness list has no search route, so the filter runs
/// here over the (paged) ticket list.
pub struct BoardSearch {
    pub client: Arc<HarnessClient>,
}

const SEARCH: &str = "board.search";

#[async_trait]
impl Tool for BoardSearch {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            id: SEARCH,
            description: "Find board cards about a topic (words in titles and descriptions), best match first. Use it for \"where are we on X\" and \"what is next for X\". Read-only.",
            parameters: json!({"type": "object", "required": ["query"], "properties": {
                "query": {"type": "string", "description": "Topic words, e.g. \"pad\" or \"mentra glasses\"."},
                "status": {"type": "string", "enum": ["todo", "in_progress", "review", "done"]},
                "limit": {"type": "integer", "minimum": 1, "maximum": 20, "default": 8}}}),
            gate: Gate::Open,
        }
    }

    async fn call(&self, args: Value) -> Result<ToolOutcome, CosError> {
        let query = need_str(SEARCH, &args, "query")?;
        let terms = query_terms(&query);
        if terms.is_empty() {
            return Err(tool_err(SEARCH, "query has no topic words; give the name of the thing to look for"));
        }
        let status = super::arg_str(&args, "status");
        let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(8).clamp(1, 20) as usize;
        let all = self.client.tickets(status.as_deref(), 2000).await?;
        let mut hits: Vec<(usize, Value)> = all.into_iter().map(|t| (score_ticket(&t, &terms), t)).filter(|(s, _)| *s > 0).collect();
        hits.sort_by(|(sa, a), (sb, b)| sb.cmp(sa).then_with(|| b["updated_at"].as_str().cmp(&a["updated_at"].as_str())));
        let tickets: Vec<Value> = hits.into_iter().map(|(_, t)| t).collect();
        let (rows, more) = bounded_rows(&tickets, limit);
        Ok(ToolOutcome {
            summary: format!("{} card(s) match {:?}", tickets.len(), terms.join(" ")),
            result: json!({"query": terms, "matches": tickets.len(), "shown": rows.len(), "tickets": rows, "more": more}),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(title: &str, desc: &str) -> Value {
        json!({"id": "11111111-0000-0000-0000-000000000000", "status": "todo", "title": title, "description": desc})
    }

    #[test]
    fn query_terms_drop_filler() {
        assert_eq!(query_terms("where are we on the pad"), vec!["pad"]);
        assert!(query_terms("what is the status").is_empty());
    }

    #[test]
    fn topic_matches_title_over_description_and_skips_unrelated() {
        let terms = query_terms("pad");
        let pad = card("Edge pad console host", "");
        let desc = card("Rotate keys", "also touches the pad firmware");
        let other = card("MeshId rollout", "mesh identity");
        assert!(score_ticket(&pad, &terms) > score_ticket(&desc, &terms));
        assert!(score_ticket(&desc, &terms) > 0);
        assert_eq!(score_ticket(&other, &terms), 0);
        assert_eq!(score_ticket(&card("Mentra glasses app", ""), &query_terms("mentra glasses")), 6);
    }
}
