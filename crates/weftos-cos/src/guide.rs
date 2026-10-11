//! Guide-view state (ADR-118 §1a): the current layout, live checklists and
//! step lists, and at most one pending survey question. The `ui.*` tools
//! write it and emit the matching events; touch answers and spoken numbers
//! resolve against it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::sync::broadcast;

use crate::CosError;
use crate::conversation::{AnswerKind, GuideAnswer, TurnId, UserUtterance};
use crate::events::{ChecklistItem, Choice, Event, Layout};

#[derive(Debug, Clone, Default)]
pub struct Checklist {
    pub id: String,
    pub title: String,
    pub items: Vec<ChecklistItem>,
}

#[derive(Debug, Clone, Default)]
pub struct Steps {
    pub id: String,
    pub title: String,
    pub steps: Vec<String>,
    pub current: usize,
}

#[derive(Debug, Clone, Default)]
pub struct Ask {
    pub id: String,
    pub question: String,
    pub explain: String,
    pub choices: Vec<Choice>,
    pub index: usize,
    pub total: usize,
}

#[derive(Debug, Default)]
pub struct GuideState {
    pub layout: Layout,
    pub checklists: HashMap<String, Checklist>,
    pub steps: HashMap<String, Steps>,
    pub pending_ask: Option<Ask>,
    /// Questions asked so far, oldest first, so `back` can re-ask one.
    pub asked: Vec<Ask>,
    pub current_turn: Option<TurnId>,
    next_id: u64,
}

impl GuideState {
    pub fn fresh_id(&mut self, prefix: &str) -> String {
        self.next_id += 1;
        format!("{prefix}{}", self.next_id)
    }
}

/// What the `ui.*` tools and the agent share: the state and the emitter.
#[derive(Clone)]
pub struct UiHandle {
    pub state: Arc<Mutex<GuideState>>,
    tx: broadcast::Sender<Event>,
}

impl UiHandle {
    pub fn new(tx: broadcast::Sender<Event>) -> Self {
        Self { state: Arc::new(Mutex::new(GuideState::default())), tx }
    }

    pub fn emit(&self, ev: Event) {
        let _ = self.tx.send(ev.redacted());
    }

    pub fn lock(&self) -> std::sync::MutexGuard<'_, GuideState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Switch layout and emit it (only when it changes).
    pub fn set_layout(&self, layout: Layout) {
        let changed = {
            let mut s = self.lock();
            let changed = s.layout != layout;
            s.layout = layout;
            changed
        };
        if changed {
            self.emit(Event::Layout { layout });
        }
    }

    pub fn emit_checklist(&self, c: &Checklist) {
        self.emit(Event::Checklist { id: c.id.clone(), title: c.title.clone(), items: c.items.clone() });
    }

    pub fn emit_steps(&self, s: &Steps) {
        self.emit(Event::Steps { id: s.id.clone(), title: s.title.clone(), steps: s.steps.clone(), current: s.current });
    }

    pub fn emit_ask(&self, a: &Ask) {
        self.emit(Event::Ask {
            id: a.id.clone(),
            question: a.question.clone(),
            explain: a.explain.clone(),
            choices: a.choices.clone(),
            index: a.index,
            total: a.total,
        });
    }

    /// Apply a touch interaction. `Turn` means the agent should run a model
    /// turn with the given utterance; `Updated` means the view changed and was
    /// re-emitted.
    pub fn apply_answer(&self, a: &GuideAnswer) -> Result<Resolution, CosError> {
        match a.kind {
            AnswerKind::Answer => {
                let n = a.value.as_u64().or_else(|| a.value.as_str().and_then(|s| s.parse().ok()));
                let Some(n) = n else { return Err(CosError::Config("answer.value must be a choice number".into())) };
                self.resolve_choice(Some(&a.id), n as usize)
            }
            AnswerKind::Skip => self.resolve_skip(Some(&a.id)),
            AnswerKind::Back => self.back(&a.id),
            AnswerKind::Toggle => {
                let (item_id, done) = match &a.value {
                    Value::String(s) => (s.clone(), None),
                    Value::Object(o) => (
                        o.get("item_id").and_then(Value::as_str).unwrap_or_default().to_string(),
                        o.get("done").and_then(Value::as_bool),
                    ),
                    _ => return Err(CosError::Config("toggle.value must be an item id or {item_id, done}".into())),
                };
                self.toggle(&a.id, &item_id, done)
            }
            AnswerKind::Step => self.step(&a.id, &a.value),
        }
    }

    pub fn toggle(&self, id: &str, item_id: &str, done: Option<bool>) -> Result<Resolution, CosError> {
        let snapshot = {
            let mut s = self.lock();
            let Some(c) = s.checklists.get_mut(id) else { return Err(CosError::Config(format!("no checklist {id}"))) };
            let Some(it) = c.items.iter_mut().find(|i| i.id == item_id) else {
                return Err(CosError::Config(format!("no item {item_id} in {id}")));
            };
            it.done = done.unwrap_or(!it.done);
            let (is_done, text) = (it.done, it.text.clone());
            let done_count = c.items.iter().filter(|i| i.done).count();
            let note = format!("[touch] {} \"{}\" ({}/{} done)", if is_done { "checked" } else { "unchecked" }, text, done_count, c.items.len());
            (c.clone(), note)
        };
        self.set_layout(Layout::Guide);
        self.emit_checklist(&snapshot.0);
        Ok(Resolution::Updated { note: snapshot.1 })
    }

    pub fn step(&self, id: &str, value: &Value) -> Result<Resolution, CosError> {
        let snapshot = {
            let mut s = self.lock();
            let Some(st) = s.steps.get_mut(id) else { return Err(CosError::Config(format!("no steps {id}"))) };
            let last = st.steps.len().saturating_sub(1);
            st.current = match value {
                Value::String(v) if v == "next" => (st.current + 1).min(last),
                Value::String(v) if v == "back" => st.current.saturating_sub(1),
                Value::Number(n) => (n.as_u64().unwrap_or(1) as usize).clamp(1, last + 1) - 1,
                other => return Err(CosError::Config(format!("step.value {other} is not next, back or a number"))),
            };
            let note = format!("[touch] step {}/{}: {}", st.current + 1, st.steps.len(), st.steps.get(st.current).cloned().unwrap_or_default());
            (st.clone(), note)
        };
        self.set_layout(Layout::Guide);
        self.emit_steps(&snapshot.0);
        Ok(Resolution::Updated { note: snapshot.1 })
    }

    /// Answer the pending survey question by number (touch or voice).
    pub fn resolve_choice(&self, id: Option<&str>, n: usize) -> Result<Resolution, CosError> {
        let mut s = self.lock();
        let Some(ask) = s.pending_ask.as_ref() else { return Err(CosError::Config("no question is pending".into())) };
        if id.is_some_and(|i| i != ask.id) {
            return Err(CosError::Config(format!("question {} is not the pending one", id.unwrap_or_default())));
        }
        let Some(choice) = ask.choices.iter().find(|c| c.n == n) else {
            return Err(CosError::Config(format!("choice {n} is not one of 1..={}", ask.choices.len())));
        };
        let text = format!("Answer to Q{}/{} \"{}\": {} — {}", ask.index, ask.total, ask.question, choice.n, choice.caption);
        s.pending_ask = None;
        Ok(Resolution::Turn(UserUtterance::touch(text)))
    }

    pub fn resolve_skip(&self, id: Option<&str>) -> Result<Resolution, CosError> {
        let mut s = self.lock();
        let Some(ask) = s.pending_ask.as_ref() else { return Err(CosError::Config("no question is pending".into())) };
        if id.is_some_and(|i| i != ask.id) {
            return Err(CosError::Config("that question is not the pending one".into()));
        }
        let text = format!("Skipped Q{}/{} \"{}\"", ask.index, ask.total, ask.question);
        s.pending_ask = None;
        Ok(Resolution::Turn(UserUtterance::touch(text)))
    }

    /// Re-ask the question before the pending one in the same survey. At the
    /// first question (or when no earlier one is known) the pending question
    /// is shown again unchanged.
    pub fn back(&self, id: &str) -> Result<Resolution, CosError> {
        let ask = {
            let mut s = self.lock();
            let Some(cur) = s.pending_ask.clone() else { return Err(CosError::Config("no question is pending".into())) };
            if cur.id != id {
                return Err(CosError::Config(format!("question {id} is not the pending one")));
            }
            let pos = s.asked.iter().position(|a| a.id == cur.id);
            let prev = pos.and_then(|p| p.checked_sub(1)).map(|p| s.asked[p].clone()).filter(|p| p.index < cur.index);
            match prev {
                Some(prev) => {
                    s.asked.truncate(pos.unwrap_or(0));
                    s.asked.push(prev.clone());
                    s.pending_ask = Some(prev.clone());
                    prev
                }
                None => cur,
            }
        };
        self.emit_ask(&ask);
        Ok(Resolution::Updated { note: format!("[touch] back to Q{}/{} \"{}\"; the owner may answer it again", ask.index, ask.total, ask.question) })
    }

    /// A spoken or typed utterance that resolves a pending question
    /// ("two", "number 2", "option two", "skip") becomes the answer turn;
    /// anything else is returned unchanged.
    pub fn resolve_spoken(&self, utterance: UserUtterance) -> UserUtterance {
        if self.lock().pending_ask.is_none() {
            return utterance;
        }
        let lower = utterance.text.trim().trim_end_matches(['.', '!', '?']).to_lowercase();
        let resolved = if lower == "skip" || lower == "skip it" || lower == "skip that" {
            self.resolve_skip(None).ok()
        } else {
            spoken_number(&lower).and_then(|n| self.resolve_choice(None, n).ok())
        };
        match resolved {
            Some(Resolution::Turn(mut u)) => {
                u.source = utterance.source;
                u
            }
            _ => utterance,
        }
    }
}

/// Outcome of a touch interaction.
#[derive(Debug, Clone, PartialEq)]
pub enum Resolution {
    /// Run this as a user turn.
    Turn(UserUtterance),
    /// The view changed; `note` is appended to the model's history.
    Updated { note: String },
}

/// Parse "2", "two", "number two", "option 2", "the second one", "button 3".
pub fn spoken_number(lower: &str) -> Option<usize> {
    const WORDS: &[(&str, usize)] = &[
        ("one", 1), ("two", 2), ("three", 3), ("four", 4), ("five", 5), ("six", 6),
        ("first", 1), ("second", 2), ("third", 3), ("fourth", 4), ("fifth", 5), ("sixth", 6),
        ("1", 1), ("2", 2), ("3", 3), ("4", 4), ("5", 5), ("6", 6),
    ];
    let words: Vec<&str> = lower.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).collect();
    if words.is_empty() || words.len() > 4 {
        return None;
    }
    const FILLER: &[&str] = &["number", "option", "choice", "button", "the", "one", "please", "pick", "take", "go", "with", "answer"];
    let mut found = None;
    for w in &words {
        if let Some((_, n)) = WORDS.iter().find(|(t, _)| t == w) {
            if found.is_some() && *w != "one" {
                return None;
            }
            if found.is_none() || *w != "one" {
                found = Some(*n);
            }
        } else if !FILLER.contains(w) {
            return None;
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spoken_numbers_parse() {
        assert_eq!(spoken_number("two"), Some(2));
        assert_eq!(spoken_number("number 3"), Some(3));
        assert_eq!(spoken_number("the second one"), Some(2));
        assert_eq!(spoken_number("option four please"), Some(4));
        assert_eq!(spoken_number("where are we"), None);
        assert_eq!(spoken_number("two or three"), None);
    }
}
