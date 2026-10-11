//! The chat log model: bounded history, wrapped lines, and a scroll
//! position measured in lines from the bottom. No scene knowledge
//! here; `chat_view` turns the visible window into nodes.

use std::collections::VecDeque;

use crate::text::{wrap, wrap_prefixed};
use crate::theme::LINE_PITCH;

/// Oldest entries are dropped past these bounds.
pub const MAX_ENTRIES: usize = 256;
pub const MAX_LINES: usize = 4000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Agent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub role: Role,
    pub text: String,
    pub msg_id: Option<u64>,
    /// Monotonic id, stable while the entry lives.
    pub seq: u64,
}

/// One wrapped screen line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub text: String,
    pub role: Role,
    pub entry_seq: u64,
    pub index: u32,
    /// Part of the current exchange (bright) or history (dim).
    pub bright: bool,
}

impl Line {
    /// Identity of the line across relayouts.
    pub fn key(&self) -> (u64, u32) {
        (self.entry_seq, self.index)
    }
}

#[derive(Debug, Clone)]
pub struct ChatLog {
    entries: VecDeque<Entry>,
    next_seq: u64,
    lines: Vec<Line>,
    cols: usize,
    visible: usize,
    /// Lines scrolled back from the bottom; 0 is pinned to newest.
    scroll: usize,
    unread_below: bool,
    px_accum: i32,
}

impl ChatLog {
    pub fn new(cols: usize, visible: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            next_seq: 1,
            lines: Vec::new(),
            cols: cols.max(4),
            visible: visible.max(1),
            scroll: 0,
            unread_below: false,
            px_accum: 0,
        }
    }

    pub fn push_user(&mut self, text: impl Into<String>) {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.entries.push_back(Entry {
            role: Role::User,
            text: text.into(),
            msg_id: None,
            seq,
        });
        self.after_change();
    }

    /// Append `delta` to the agent message `msg_id`, or start a new
    /// agent entry when the last entry is not that message.
    pub fn push_agent(&mut self, delta: &str, msg_id: u64) {
        match self.entries.back_mut() {
            Some(e) if e.role == Role::Agent && e.msg_id == Some(msg_id) => e.text.push_str(delta),
            _ => {
                let seq = self.next_seq;
                self.next_seq += 1;
                self.entries.push_back(Entry {
                    role: Role::Agent,
                    text: delta.to_string(),
                    msg_id: Some(msg_id),
                    seq,
                });
            }
        }
        self.after_change();
    }

    fn after_change(&mut self) {
        while self.entries.len() > MAX_ENTRIES {
            self.entries.pop_front();
        }
        let before = self.lines.len();
        self.relayout();
        while self.lines.len() > MAX_LINES && self.entries.len() > 1 {
            self.entries.pop_front();
            self.relayout();
        }
        let added = self.lines.len().saturating_sub(before);
        if self.scroll > 0 && added > 0 {
            // Keep the reader's place and flag what arrived below.
            self.scroll += added;
            self.unread_below = true;
        }
        self.clamp();
    }

    fn relayout(&mut self) {
        // The current exchange is the newest user line and everything
        // after it; older entries read as history.
        let current_from = self
            .entries
            .iter()
            .rev()
            .find(|e| e.role == Role::User)
            .map(|e| e.seq)
            .unwrap_or(0);
        self.lines.clear();
        for e in &self.entries {
            let wrapped = match e.role {
                Role::User => wrap_prefixed(&e.text, self.cols, "> ", "  "),
                Role::Agent => wrap(&e.text, self.cols),
            };
            for (i, text) in wrapped.into_iter().enumerate() {
                self.lines.push(Line {
                    text,
                    role: e.role,
                    entry_seq: e.seq,
                    index: i as u32,
                    bright: e.seq >= current_from,
                });
            }
        }
    }

    fn clamp(&mut self) {
        let max = self.max_scroll();
        if self.scroll > max {
            self.scroll = max;
        }
        if self.scroll == 0 {
            self.unread_below = false;
        }
    }

    pub fn entries(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter()
    }

    pub fn lines(&self) -> &[Line] {
        &self.lines
    }

    pub fn total_lines(&self) -> usize {
        self.lines.len()
    }

    pub fn visible(&self) -> usize {
        self.visible
    }

    /// Change the window height (a layout switch); the scroll
    /// position is clamped to the new range.
    pub fn set_visible(&mut self, visible: usize) {
        self.visible = visible.max(1);
        self.clamp();
    }

    pub fn max_scroll(&self) -> usize {
        self.lines.len().saturating_sub(self.visible)
    }

    pub fn scroll(&self) -> usize {
        self.scroll
    }

    pub fn is_at_bottom(&self) -> bool {
        self.scroll == 0
    }

    pub fn unread_below(&self) -> bool {
        self.unread_below
    }

    /// The lines on screen, oldest first.
    pub fn window(&self) -> &[Line] {
        let end = self.lines.len().saturating_sub(self.scroll);
        let start = end.saturating_sub(self.visible);
        &self.lines[start..end]
    }

    /// Scroll by whole lines; positive is back into history. Returns
    /// whether the window moved.
    pub fn scroll_lines(&mut self, delta: i32) -> bool {
        let before = self.scroll;
        let target = self.scroll as i64 + delta as i64;
        self.scroll = target.clamp(0, self.max_scroll() as i64) as usize;
        if self.scroll == 0 {
            self.unread_below = false;
        }
        before != self.scroll
    }

    /// Drag by `dy` pixels (finger down the screen is positive and
    /// reveals older lines). Sub-line remainders accumulate.
    pub fn scroll_by_px(&mut self, dy: i32) -> bool {
        self.px_accum += dy;
        let lines = self.px_accum / LINE_PITCH;
        self.px_accum -= lines * LINE_PITCH;
        if lines == 0 {
            return false;
        }
        self.scroll_lines(lines)
    }

    pub fn scroll_to_bottom(&mut self) -> bool {
        self.px_accum = 0;
        self.scroll_lines(-(self.scroll as i32))
    }

    /// End of a gesture: drop the sub-line remainder.
    pub fn end_gesture(&mut self) {
        self.px_accum = 0;
    }

    /// Scroll thumb as fractions of the track: `(start, length)`.
    pub fn thumb(&self) -> (f32, f32) {
        let total = self.lines.len().max(1) as f32;
        let len = (self.visible as f32 / total).min(1.0);
        let end = (self.lines.len() - self.scroll) as f32 / total;
        ((end - len).max(0.0), len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log_with(n: usize) -> ChatLog {
        let mut l = ChatLog::new(20, 5);
        for i in 0..n {
            l.push_user(format!("line {i}"));
        }
        l
    }

    #[test]
    fn window_is_newest_at_bottom() {
        let l = log_with(8);
        let w = l.window();
        assert_eq!(w.len(), 5);
        assert_eq!(w[4].text, "> line 7");
    }

    #[test]
    fn scroll_clamps_both_ends() {
        let mut l = log_with(8);
        assert!(l.scroll_by_px(LINE_PITCH * 100));
        assert_eq!(l.scroll(), 3);
        assert!(!l.scroll_by_px(LINE_PITCH));
        assert!(l.scroll_by_px(-LINE_PITCH * 100));
        assert_eq!(l.scroll(), 0);
    }

    #[test]
    fn new_lines_while_scrolled_keep_place_and_flag_unread() {
        let mut l = log_with(8);
        l.scroll_lines(2);
        let top_before = l.window()[0].text.clone();
        l.push_agent("reply", 1);
        assert_eq!(l.window()[0].text, top_before);
        assert!(l.unread_below());
        l.scroll_to_bottom();
        assert!(!l.unread_below());
        assert_eq!(l.window().last().unwrap().text, "reply");
    }

    #[test]
    fn streaming_appends_to_same_message() {
        let mut l = ChatLog::new(20, 5);
        l.push_agent("hel", 7);
        l.push_agent("lo", 7);
        l.push_agent("new", 8);
        assert_eq!(l.entries().count(), 2);
        assert_eq!(l.lines()[0].text, "hello");
    }

    #[test]
    fn history_is_bounded() {
        let l = log_with(MAX_ENTRIES + 50);
        assert_eq!(l.entries().count(), MAX_ENTRIES);
    }

    #[test]
    fn current_exchange_is_bright() {
        let mut l = ChatLog::new(20, 5);
        l.push_user("old");
        l.push_agent("old reply", 1);
        l.push_user("new");
        l.push_agent("new reply", 2);
        let b: Vec<bool> = l.lines().iter().map(|x| x.bright).collect();
        assert_eq!(b, vec![false, false, true, true]);
    }
}
