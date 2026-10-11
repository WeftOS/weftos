//! The agent's events (`GET /v1/cos/events`, contract §3) as the host
//! reads them off the wire, and their conversion to console models.

use serde::Deserialize;

use crate::{AgentState, Checklist, ChecklistItem, Choice, Layout, PageBlock, PageDoc, Steps, Survey};

/// One server-sent event. Unknown kinds decode as [`AgentEvent::Unknown`]
/// so a newer agent never breaks an older host.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum AgentEvent {
    State {
        state: String,
        #[serde(default)]
        reason: Option<String>,
    },
    Token {
        turn_id: String,
        text: String,
    },
    Message {
        turn_id: String,
        text: String,
        #[serde(rename = "final", default)]
        is_final: bool,
    },
    Tool {
        name: String,
        summary: String,
        status: String,
    },
    Page {
        turn_id: String,
        markdown: String,
    },
    Status {
        task: String,
        model: String,
        #[serde(default)]
        lanes_running: u32,
    },
    Layout {
        layout: String,
    },
    Checklist {
        id: String,
        title: String,
        items: Vec<ItemWire>,
    },
    Steps {
        id: String,
        title: String,
        steps: Vec<String>,
        #[serde(default)]
        current: usize,
    },
    Ask {
        id: String,
        question: String,
        #[serde(default)]
        explain: String,
        choices: Vec<ChoiceWire>,
        #[serde(default)]
        index: u32,
        #[serde(default)]
        total: u32,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ItemWire {
    pub id: String,
    pub text: String,
    #[serde(default)]
    pub done: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ChoiceWire {
    pub n: u8,
    pub caption: String,
}

impl From<ItemWire> for ChecklistItem {
    fn from(w: ItemWire) -> Self {
        Self { id: w.id, text: w.text, done: w.done }
    }
}

impl From<ChoiceWire> for Choice {
    fn from(w: ChoiceWire) -> Self {
        Self { n: w.n, caption: w.caption }
    }
}

/// Decode one event's `data:` payload.
pub fn parse_event(data: &str) -> Option<AgentEvent> {
    serde_json::from_str(data).ok()
}

/// The agent's state word to the console's state.
pub fn agent_state(state: &str, reason: Option<&str>) -> AgentState {
    match state {
        "listening" => AgentState::Listening,
        "thinking" => AgentState::Thinking,
        "speaking" => AgentState::Speaking,
        "blocked" => AgentState::Blocked(reason.unwrap_or("blocked").to_string()),
        _ => AgentState::Idle,
    }
}

pub fn parse_layout(s: &str) -> Option<Layout> {
    match s {
        "split" => Some(Layout::Split),
        "page" => Some(Layout::Page),
        "guide" => Some(Layout::Guide),
        _ => None,
    }
}

pub fn checklist(id: String, title: String, items: Vec<ItemWire>) -> Checklist {
    Checklist { id, title, items: items.into_iter().map(Into::into).collect() }
}

pub fn steps(id: String, title: String, steps: Vec<String>, current: usize) -> Steps {
    Steps { id, title, steps, current }
}

pub fn survey(id: String, question: String, explain: String, choices: Vec<ChoiceWire>, index: u32, total: u32) -> Survey {
    Survey { id, question, explain, choices: choices.into_iter().map(Into::into).collect(), index, total }
}

/// A plain reading of the agent's markdown into the C2 page blocks: the
/// first `#` line is the title, other `#` lines are headings, `-` / `*`
/// lines are bullets, `---` is a rule, and blank lines separate
/// paragraphs. Inline markup is left as written; rendered markdown and
/// SVG are C5.
pub fn page_from_markdown(markdown: &str) -> PageDoc {
    let mut doc = PageDoc::default();
    let mut para: Vec<String> = Vec::new();
    let mut bullets: Vec<String> = Vec::new();
    fn flush(doc: &mut PageDoc, para: &mut Vec<String>, bullets: &mut Vec<String>) {
        if !para.is_empty() {
            doc.blocks.push(PageBlock::Paragraph(para.join(" ")));
            para.clear();
        }
        if !bullets.is_empty() {
            doc.blocks.push(PageBlock::Bullets(std::mem::take(bullets)));
        }
    }
    for raw in markdown.lines() {
        let line = raw.trim_end();
        let trimmed = line.trim_start();
        if trimmed.is_empty() {
            flush(&mut doc, &mut para, &mut bullets);
        } else if let Some(h) = trimmed.strip_prefix('#') {
            flush(&mut doc, &mut para, &mut bullets);
            let text = h.trim_start_matches('#').trim().to_string();
            if doc.title.is_empty() && doc.blocks.is_empty() {
                doc.title = text;
            } else {
                doc.blocks.push(PageBlock::Heading(text));
            }
        } else if trimmed == "---" || trimmed == "***" {
            flush(&mut doc, &mut para, &mut bullets);
            doc.blocks.push(PageBlock::Rule);
        } else if let Some(b) = trimmed.strip_prefix("- ").or_else(|| trimmed.strip_prefix("* ")) {
            if !para.is_empty() {
                doc.blocks.push(PageBlock::Paragraph(para.join(" ")));
                para.clear();
            }
            bullets.push(b.trim().to_string());
        } else {
            if !bullets.is_empty() {
                doc.blocks.push(PageBlock::Bullets(std::mem::take(&mut bullets)));
            }
            para.push(trimmed.to_string());
        }
    }
    flush(&mut doc, &mut para, &mut bullets);
    if doc.title.is_empty() {
        doc.title = "page".into();
    }
    doc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_decode_by_kind_and_unknown_kinds_do_not_fail() {
        let ev = parse_event(r#"{"kind":"message","turn_id":"t1","text":"hi","final":true}"#).unwrap();
        assert_eq!(ev, AgentEvent::Message { turn_id: "t1".into(), text: "hi".into(), is_final: true });
        let ev = parse_event(r#"{"kind":"state","state":"blocked","reason":"approval"}"#).unwrap();
        let AgentEvent::State { state, reason } = ev else { panic!() };
        assert_eq!(agent_state(&state, reason.as_deref()), AgentState::Blocked("approval".into()));
        assert_eq!(parse_event(r#"{"kind":"hologram","x":1}"#), Some(AgentEvent::Unknown));
        assert_eq!(parse_event("not json"), None);
    }

    #[test]
    fn markdown_becomes_title_headings_bullets_and_rules() {
        let doc = page_from_markdown("# Plan\n\nFirst para\nstill first.\n\n## Morning\n- one\n- two\n---\nlast");
        assert_eq!(doc.title, "Plan");
        assert_eq!(
            doc.blocks,
            vec![
                PageBlock::Paragraph("First para still first.".into()),
                PageBlock::Heading("Morning".into()),
                PageBlock::Bullets(vec!["one".into(), "two".into()]),
                PageBlock::Rule,
                PageBlock::Paragraph("last".into()),
            ]
        );
    }
}
