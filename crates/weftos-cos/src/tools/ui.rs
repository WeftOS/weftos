//! `ui.*` tools: the agent drives the console's three layouts and the guide
//! views (ADR-118 §1a). All are open-gated: they change what is on screen and
//! nothing else. The agent registers them itself over its [`UiHandle`].

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{Gate, Tool, ToolOutcome, ToolRegistry, ToolSpec, arg_str, need_str, tool_err};
use crate::CosError;
use crate::events::{ChecklistItem, Choice, Event, Layout};
use crate::guide::{Ask, Checklist, Steps, UiHandle};

/// A placeholder page until C5 renders markdown.
pub const EXAMPLE_PAGE: &str = "# Chief of staff\n\nThis is the page layout. Rendered markdown and SVG arrive in C5.\n\n- Say \"back\" or swipe to return to the split view.\n";

pub fn register(reg: ToolRegistry, ui: &UiHandle) -> ToolRegistry {
    reg.with(Arc::new(UiLayout(ui.clone())))
        .with(Arc::new(UiPage(ui.clone())))
        .with(Arc::new(UiChecklist(ui.clone())))
        .with(Arc::new(UiCheck(ui.clone())))
        .with(Arc::new(UiSteps(ui.clone())))
        .with(Arc::new(UiAsk(ui.clone())))
}

fn strings(args: &Value, key: &str) -> Vec<String> {
    args.get(key)
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_str().or_else(|| v.get("caption").and_then(Value::as_str)).or_else(|| v.get("text").and_then(Value::as_str))).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
        .unwrap_or_default()
}

pub struct UiLayout(UiHandle);

#[async_trait]
impl Tool for UiLayout {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            id: "ui.layout",
            description: "Switch the console layout: split (avatar + chat), page (full page), guide (checklist, steps or survey).",
            parameters: json!({"type": "object", "required": ["layout"], "properties": {"layout": {"type": "string", "enum": ["split", "page", "guide"]}}}),
            gate: Gate::Open,
        }
    }
    async fn call(&self, args: Value) -> Result<ToolOutcome, CosError> {
        let s = need_str("ui.layout", &args, "layout")?;
        let layout = Layout::parse(&s).ok_or_else(|| tool_err("ui.layout", format!("unknown layout {s}")))?;
        self.0.set_layout(layout);
        Ok(ToolOutcome { summary: format!("layout {s}"), result: json!({"layout": layout}) })
    }
}

pub struct UiPage(UiHandle);

#[async_trait]
impl Tool for UiPage {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            id: "ui.page",
            description: "Show a full page of markdown in the page layout. Without markdown an example page is shown.",
            parameters: json!({"type": "object", "properties": {"markdown": {"type": "string", "maxLength": 20000}}}),
            gate: Gate::Open,
        }
    }
    async fn call(&self, args: Value) -> Result<ToolOutcome, CosError> {
        let markdown = arg_str(&args, "markdown").unwrap_or_else(|| EXAMPLE_PAGE.to_string());
        let turn_id = self.0.lock().current_turn.clone().unwrap_or_default();
        self.0.set_layout(Layout::Page);
        self.0.emit(Event::Page { turn_id, markdown: markdown.clone() });
        Ok(ToolOutcome { summary: format!("page shown ({} chars)", markdown.chars().count()), result: json!({"layout": "page"}) })
    }
}

pub struct UiChecklist(UiHandle);

#[async_trait]
impl Tool for UiChecklist {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            id: "ui.checklist",
            description: "Show a checklist in the guide area (release steps, setup, a plan for today). Items start unchecked.",
            parameters: json!({"type": "object", "required": ["title", "items"], "properties": {
                "title": {"type": "string", "maxLength": 120},
                "items": {"type": "array", "minItems": 1, "maxItems": 12, "items": {"type": "string", "maxLength": 160}}}}),
            gate: Gate::Open,
        }
    }
    async fn call(&self, args: Value) -> Result<ToolOutcome, CosError> {
        let title = need_str("ui.checklist", &args, "title")?;
        let items = strings(&args, "items");
        if items.is_empty() || items.len() > 12 {
            return Err(tool_err("ui.checklist", "items must hold 1..=12 entries"));
        }
        let c = {
            let mut s = self.0.lock();
            let id = s.fresh_id("c");
            let c = Checklist { id: id.clone(), title, items: items.iter().enumerate().map(|(i, t)| ChecklistItem { id: format!("i{}", i + 1), text: t.clone(), done: false }).collect() };
            s.checklists.insert(id, c.clone());
            c
        };
        self.0.set_layout(Layout::Guide);
        self.0.emit_checklist(&c);
        Ok(ToolOutcome { summary: format!("checklist {} ({} items)", c.id, c.items.len()), result: json!({"id": c.id, "item_ids": c.items.iter().map(|i| i.id.clone()).collect::<Vec<_>>()}) })
    }
}

pub struct UiCheck(UiHandle);

#[async_trait]
impl Tool for UiCheck {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            id: "ui.check",
            description: "Check or uncheck one checklist item.",
            parameters: json!({"type": "object", "required": ["id", "item_id", "done"], "properties": {
                "id": {"type": "string"}, "item_id": {"type": "string"}, "done": {"type": "boolean"}}}),
            gate: Gate::Open,
        }
    }
    async fn call(&self, args: Value) -> Result<ToolOutcome, CosError> {
        let id = need_str("ui.check", &args, "id")?;
        let item = need_str("ui.check", &args, "item_id")?;
        let done = args.get("done").and_then(Value::as_bool).ok_or_else(|| tool_err("ui.check", "done must be a boolean"))?;
        self.0.toggle(&id, &item, Some(done)).map_err(|e| tool_err("ui.check", e))?;
        Ok(ToolOutcome { summary: format!("{} {item} in {id}", if done { "checked" } else { "unchecked" }), result: json!({"id": id, "item_id": item, "done": done}) })
    }
}

pub struct UiSteps(UiHandle);

#[async_trait]
impl Tool for UiSteps {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            id: "ui.steps",
            description: "Show numbered step-by-step instructions in the guide area with the current step highlighted.",
            parameters: json!({"type": "object", "required": ["title", "steps"], "properties": {
                "title": {"type": "string", "maxLength": 120},
                "steps": {"type": "array", "minItems": 1, "maxItems": 20, "items": {"type": "string", "maxLength": 200}},
                "current": {"type": "integer", "minimum": 1, "description": "1-based current step; default 1"}}}),
            gate: Gate::Open,
        }
    }
    async fn call(&self, args: Value) -> Result<ToolOutcome, CosError> {
        let title = need_str("ui.steps", &args, "title")?;
        let steps = strings(&args, "steps");
        if steps.is_empty() || steps.len() > 20 {
            return Err(tool_err("ui.steps", "steps must hold 1..=20 entries"));
        }
        let current = (args.get("current").and_then(Value::as_u64).unwrap_or(1) as usize).clamp(1, steps.len()) - 1;
        let st = {
            let mut s = self.0.lock();
            let id = s.fresh_id("s");
            let st = Steps { id: id.clone(), title, steps, current };
            s.steps.insert(id, st.clone());
            st
        };
        self.0.set_layout(Layout::Guide);
        self.0.emit_steps(&st);
        Ok(ToolOutcome { summary: format!("steps {} ({} steps, at {})", st.id, st.steps.len(), st.current + 1), result: json!({"id": st.id, "current": st.current + 1}) })
    }
}

pub struct UiAsk(UiHandle);

#[async_trait]
impl Tool for UiAsk {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            id: "ui.ask",
            description: "Ask one survey question with 2 to 6 numbered choices. Put any spoken framing in this same response; the host ends the turn as soon as the question is on screen, and the owner's tap or spoken number arrives as the next user turn. One question per turn.",
            parameters: json!({"type": "object", "required": ["question", "choices"], "properties": {
                "question": {"type": "string", "maxLength": 200},
                "explain": {"type": "string", "maxLength": 300},
                "choices": {"type": "array", "minItems": 2, "maxItems": 6, "items": {"type": "string", "maxLength": 60}},
                "index": {"type": "integer", "minimum": 1, "description": "This question's number in the survey"},
                "total": {"type": "integer", "minimum": 1, "description": "Questions in the survey"}}}),
            gate: Gate::Open,
        }
    }
    async fn call(&self, args: Value) -> Result<ToolOutcome, CosError> {
        let question = need_str("ui.ask", &args, "question")?;
        let captions = strings(&args, "choices");
        if !(2..=6).contains(&captions.len()) {
            return Err(tool_err("ui.ask", "choices must hold 2..=6 captions"));
        }
        let index = args.get("index").and_then(Value::as_u64).unwrap_or(1).max(1) as usize;
        let total = (args.get("total").and_then(Value::as_u64).unwrap_or(index as u64) as usize).max(index);
        let ask = {
            let mut s = self.0.lock();
            let id = s.fresh_id("q");
            let ask = Ask {
                id,
                question,
                explain: arg_str(&args, "explain").unwrap_or_default(),
                choices: captions.iter().enumerate().map(|(i, c)| Choice { n: i + 1, caption: c.clone() }).collect(),
                index,
                total,
            };
            s.pending_ask = Some(ask.clone());
            ask
        };
        self.0.set_layout(Layout::Guide);
        self.0.emit_ask(&ask);
        Ok(ToolOutcome {
            summary: format!("asked Q{}/{}: {}", ask.index, ask.total, ask.question),
            result: json!({"id": ask.id, "awaiting": "the owner's tap or spoken number arrives as the next user turn"}),
        })
    }
}
