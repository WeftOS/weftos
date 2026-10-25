//! Event shapes on `GET /v1/cos/events` (contract §3) and the redaction
//! pass every event goes through before it leaves the agent.

use serde::{Deserialize, Serialize};

use crate::conversation::TurnId;

/// Agent state for the console's avatar and status line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentState {
    Idle,
    Listening,
    Thinking,
    Speaking,
    Blocked,
}

/// One server-sent event. `kind` is the discriminator the contract names.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Event {
    State {
        state: AgentState,
        /// Only with `Blocked`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    Token {
        turn_id: TurnId,
        text: String,
    },
    Message {
        turn_id: TurnId,
        text: String,
        #[serde(rename = "final")]
        is_final: bool,
        /// The endpoint the turn belongs to. Absent on a turn the agent has
        /// no record of.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        endpoint: Option<String>,
        /// `true` when the endpoint named above is a remote one that speaks
        /// this reply itself (cos.v1.1). The agent host never speaks it.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        speak: bool,
    },
    /// A tool call record: name, a one-line summary and a status. Never the
    /// arguments.
    Tool {
        name: String,
        summary: String,
        status: String,
    },
    Page {
        turn_id: TurnId,
        markdown: String,
    },
    Status {
        task: String,
        model: String,
        lanes_running: u32,
    },
    /// Which of the three console layouts to show (ADR-118 §1a).
    Layout {
        layout: Layout,
    },
    /// A checklist in the guide area; sent whole on every change.
    Checklist {
        id: String,
        title: String,
        items: Vec<ChecklistItem>,
    },
    /// Numbered instructions with the current step highlighted (0-based).
    Steps {
        id: String,
        title: String,
        steps: Vec<String>,
        current: usize,
    },
    /// One survey question with 2..=6 numbered choices.
    Ask {
        id: String,
        question: String,
        explain: String,
        choices: Vec<Choice>,
        index: usize,
        total: usize,
    },
    /// A card the chief of staff put on the e-ink glance panel on purpose
    /// (additive; consumers that do not know it ignore it). `ttl_minutes` is
    /// the resolved lifetime; `reason` is why it was shown.
    Card {
        card: CardKind,
        /// Short random hex naming this card; answers refer to it.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        id: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        title: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        detail: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        alert: String,
        /// Up to two short answers the owner can tap.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        options: Vec<String>,
        #[serde(default, skip_serializing_if = "is_zero")]
        ttl_minutes: u32,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        reason: String,
    },
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

/// What a glance card is for. `Clear` takes the current card down.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CardKind {
    NeedsYou,
    Now,
    WaitingOn,
    Eod,
    Clear,
}

impl CardKind {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "needs_you" => Some(Self::NeedsYou),
            "now" => Some(Self::Now),
            "waiting_on" => Some(Self::WaitingOn),
            "eod" => Some(Self::Eod),
            "clear" => Some(Self::Clear),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::NeedsYou => "needs_you",
            Self::Now => "now",
            Self::WaitingOn => "waiting_on",
            Self::Eod => "eod",
            Self::Clear => "clear",
        }
    }

    /// How long a card of this kind stays up when the caller names no ttl.
    pub fn default_ttl_minutes(self) -> u32 {
        match self {
            Self::NeedsYou => 240,
            Self::Now => 120,
            Self::WaitingOn => 480,
            Self::Eod => 720,
            Self::Clear => 0,
        }
    }

    /// Short label the panel shows before the title.
    pub fn prefix(self) -> &'static str {
        match self {
            Self::NeedsYou => "NEEDS YOU",
            Self::Now => "NOW",
            Self::WaitingOn => "WAITING",
            Self::Eod => "EOD",
            Self::Clear => "",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Layout {
    #[default]
    Split,
    Page,
    Guide,
}

impl Layout {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "split" => Some(Self::Split),
            "page" => Some(Self::Page),
            "guide" => Some(Self::Guide),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChecklistItem {
    pub id: String,
    pub text: String,
    pub done: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Choice {
    /// 1-based button number.
    pub n: usize,
    pub caption: String,
}

impl Event {
    pub fn state(state: AgentState) -> Self {
        Self::State { state, reason: None }
    }

    pub fn blocked(reason: impl Into<String>) -> Self {
        Self::State { state: AgentState::Blocked, reason: Some(reason.into()) }
    }

    /// Redact every string field in place (see [`redact`]).
    pub fn redacted(mut self) -> Self {
        match &mut self {
            Event::State { reason: Some(r), .. } => *r = redact(r),
            Event::State { .. } => {}
            Event::Token { text, .. } | Event::Message { text, .. } => *text = redact(text),
            Event::Tool { summary, status, .. } => {
                *summary = redact(summary);
                *status = redact(status);
            }
            Event::Page { markdown, .. } => *markdown = redact(markdown),
            Event::Status { task, .. } => *task = redact(task),
            Event::Layout { .. } => {}
            Event::Checklist { title, items, .. } => {
                *title = redact(title);
                for it in items {
                    it.text = redact(&it.text);
                }
            }
            Event::Steps { title, steps, .. } => {
                *title = redact(title);
                for s in steps {
                    *s = redact(s);
                }
            }
            Event::Card { title, detail, alert, reason, options, .. } => {
                for f in [title, detail, alert, reason].into_iter().chain(options.iter_mut()) {
                    *f = redact(f);
                }
            }
            Event::Ask { question, explain, choices, .. } => {
                *question = redact(question);
                *explain = redact(explain);
                for c in choices {
                    c.caption = redact(&c.caption);
                }
            }
        }
        self
    }
}

const REDACTED: &str = "[redacted]";

/// Prefixes that mark a credential-shaped token: harness and node bearer
/// tokens, common API-key shapes, and an explicit `Bearer` header.
const SECRET_PREFIXES: &[&str] = &["wfb_", "wft_", "wfe_", "sk-", "sk_", "ghp_", "xoxb-", "AKIA"];

fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | '+' | '=')
}

/// Replace credential-shaped substrings with `[redacted]`.
///
/// Conservative on purpose: a prefix followed by at least 16 token characters
/// is treated as a secret; `Bearer <token>` is redacted whole. Everything else
/// passes unchanged, so normal prose and ids are not mangled.
pub fn redact(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while !rest.is_empty() {
        if let Some((start, len)) = find_secret(rest) {
            out.push_str(&rest[..start]);
            out.push_str(REDACTED);
            rest = &rest[start + len..];
        } else {
            out.push_str(rest);
            break;
        }
    }
    out
}

/// Byte offset and length of the earliest secret-shaped run in `s`.
fn find_secret(s: &str) -> Option<(usize, usize)> {
    let mut best: Option<(usize, usize)> = None;
    for (idx, _) in s.char_indices() {
        let tail = &s[idx..];
        let at_boundary = idx == 0 || !is_token_char(s[..idx].chars().next_back().unwrap_or(' '));
        if !at_boundary {
            continue;
        }
        let mut hit: Option<usize> = None;
        if let Some(after) = tail.strip_prefix("Bearer ") {
            let run = after.chars().take_while(|c| is_token_char(*c)).count();
            if run >= 8 {
                hit = Some("Bearer ".len() + after.chars().take(run).map(char::len_utf8).sum::<usize>());
            }
        }
        if hit.is_none() {
            for p in SECRET_PREFIXES {
                if let Some(after) = tail.strip_prefix(p) {
                    let run = after.chars().take_while(|c| is_token_char(*c)).count();
                    if run >= 16 {
                        hit = Some(p.len() + after.chars().take(run).map(char::len_utf8).sum::<usize>());
                    }
                    break;
                }
            }
        }
        if let Some(len) = hit {
            best = Some((idx, len));
            break;
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_harness_token_shapes() {
        let tok = format!("wfb_{}", "a".repeat(64));
        let s = format!("token is {tok} ok");
        assert_eq!(redact(&s), "token is [redacted] ok");
        let s = format!("Authorization: Bearer {tok}");
        assert_eq!(redact(&s), "Authorization: [redacted]");
    }

    #[test]
    fn leaves_prose_and_short_ids_alone() {
        assert_eq!(redact("WEFT-684 is done; sk-1 is not a key"), "WEFT-684 is done; sk-1 is not a key");
        assert_eq!(redact("subtask_sk_name"), "subtask_sk_name");
    }

    #[test]
    fn event_serializes_with_kind_and_final() {
        let e = Event::Message { turn_id: TurnId::new(), text: "hi".into(), is_final: true, endpoint: None, speak: false };
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["kind"], "message");
        assert_eq!(v["final"], true);
    }
}
