//! The development typing line (ADR-118 §2: the Mac window may accept
//! typed input; the pad never does). Drawn over the last row of the
//! chat area on the alert layer, as `> text_`, and removed on send or
//! escape. Nodes `CHAT_TYPED_BG` / `CHAT_TYPED_TEXT`.

use weftos_leaf_scene::{Layer, SceneOp};

use crate::prim::{rect_fill, text};
use crate::theme::{ids, nid, Box, BLACK, BODY, GREEN, LINE_PITCH, PAD};

const PROMPT: &str = "> ";
const CURSOR: &str = "_";

#[derive(Debug, Clone)]
pub struct InputLine {
    text: String,
    area: Box,
}

impl InputLine {
    pub fn new(area: Box) -> Self {
        Self { text: String::new(), area }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// Re-home the line after a layout change (the chat area moved).
    pub fn set_area(&mut self, area: Box) {
        self.area = area;
    }

    /// Append typed text; control characters are dropped.
    pub fn push(&mut self, s: &str) -> Vec<SceneOp> {
        self.text.extend(s.chars().filter(|c| !c.is_control()));
        self.draw_ops()
    }

    pub fn backspace(&mut self) -> Vec<SceneOp> {
        self.text.pop();
        self.draw_ops()
    }

    /// Take the text out, leaving the line empty.
    pub fn take(&mut self) -> String {
        std::mem::take(&mut self.text)
    }

    fn frame(&self) -> (i32, i32, i32, i32) {
        let x = self.area.x + PAD;
        let w = self.area.w - 2 * PAD - 8;
        let h = LINE_PITCH + 4;
        let y = self.area.bottom() - h;
        (x, y, w, h)
    }

    /// Draw (or update) the line: a black band and the prompt with the
    /// tail of the text that fits, cursor last.
    pub fn draw_ops(&self) -> Vec<SceneOp> {
        let (x, y, w, h) = self.frame();
        let cols = BODY.cols(w - 8).saturating_sub(PROMPT.len() + CURSOR.len());
        let chars: Vec<char> = self.text.chars().collect();
        let start = chars.len().saturating_sub(cols);
        let shown: String = chars[start..].iter().collect();
        let label = format!("{PROMPT}{shown}{CURSOR}");
        let bg = rect_fill(ids::CHAT_TYPED_BG, Layer::Alert, x, y, w, h, BLACK);
        let mut t = text(ids::CHAT_TYPED_TEXT, &BODY, x + 4, y + 2, label, GREEN);
        t.layer = Layer::Alert;
        vec![SceneOp::Update(bg), SceneOp::Update(t)]
    }

    pub fn close_ops(&self) -> Vec<SceneOp> {
        vec![SceneOp::Remove(nid(ids::CHAT_TYPED_BG)), SceneOp::Remove(nid(ids::CHAT_TYPED_TEXT))]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::CHAT;
    use weftos_leaf_scene::Primitive;

    fn label(ops: &[SceneOp]) -> String {
        ops.iter()
            .find_map(|op| match op {
                SceneOp::Update(n) if n.id == nid(ids::CHAT_TYPED_TEXT) => match &n.primitive {
                    Primitive::Text { content, .. } => Some(content.clone()),
                    _ => None,
                },
                _ => None,
            })
            .unwrap()
    }

    #[test]
    fn shows_prompt_text_and_cursor_and_keeps_the_tail_when_long() {
        let mut l = InputLine::new(CHAT);
        assert_eq!(label(&l.push("hi")), "> hi_");
        assert_eq!(label(&l.backspace()), "> h_");
        let long = "x".repeat(200);
        let shown = label(&l.push(&long));
        assert!(shown.starts_with("> ") && shown.ends_with('_'));
        assert!(shown.chars().count() <= BODY.cols(CHAT.w - 2 * PAD - 16));
        assert_eq!(l.take().len(), 201);
        assert_eq!(l.text(), "");
    }

    #[test]
    fn sits_inside_the_chat_area_on_the_alert_layer() {
        let l = InputLine::new(CHAT);
        for op in l.draw_ops() {
            let SceneOp::Update(n) = op else { panic!() };
            assert_eq!(n.layer, Layer::Alert);
            let r = n.aabb().unwrap();
            assert!(weftos_leaf_scene::from_px_q8(r.bottom()) <= CHAT.bottom());
            assert!(weftos_leaf_scene::from_px_q8(r.x) >= CHAT.x);
        }
    }
}
