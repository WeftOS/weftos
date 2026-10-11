//! Frame lines, section labels, the rule and grid motif, the chat
//! touch target and scroll track (nodes 1–99), plus the simulator-only
//! system-strip placeholder. One set per layout.

use weftos_leaf_scene::{Layer, SceneOp};

use crate::prim::{hit_rect, line, text};
use crate::theme::{
    ids, Box, CHAT, CHAT_GUIDE, CHAT_LABEL_H, DIM, GREEN, GRID, GUIDE_HEAD, SCREEN_H, SCREEN_W,
    SMALL, STATUS, STATUS_GUIDE, SYSTEM_STRIP, VISUAL,
};

const ID_STRIP_RULE: u32 = 1;
const ID_V_DIVIDER: u32 = 2;
const ID_H_DIVIDER: u32 = 3;
const ID_LABELS: u32 = 4; // 4..7
const ID_LABEL_RULES: u32 = 7; // 7..10
const ID_CORNERS: u32 = 10; // 10..18
const ID_H_DIVIDER_2: u32 = 18;
const ID_GRID: u32 = 20; // 20..29
const ID_STRIP: u32 = 50; // 50..54

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chrome {
    /// Draw the firmware-owned system strip ourselves (simulator).
    pub system_strip_placeholder: bool,
}

impl Chrome {
    /// Everything shared by every layout: the strip placeholder, the
    /// rule under it and the corner ticks.
    pub fn common_ops(&self, title: &str, time: &str) -> Vec<SceneOp> {
        let mut ops = vec![SceneOp::Insert(line(ID_STRIP_RULE, 0, SYSTEM_STRIP.bottom(), SCREEN_W, SYSTEM_STRIP.bottom(), DIM))];
        if self.system_strip_placeholder {
            let y = SYSTEM_STRIP.y + 7;
            ops.push(SceneOp::Insert(text(ID_STRIP, &SMALL, 10, y, "WEFTOS", GREEN)));
            ops.push(SceneOp::Insert(text(ID_STRIP + 1, &SMALL, 58, y, title, DIM)));
            ops.push(SceneOp::Insert(text(ID_STRIP + 2, &SMALL, 372, y, "mesh: sim", DIM)));
            ops.push(SceneOp::Insert(text(ID_STRIP + 3, &SMALL, SCREEN_W - 10 - 6 * 5, y, time, DIM)));
        }
        let (t, l, r, b) = (VISUAL.y, 0, SCREEN_W - 1, SCREEN_H - 1);
        let ticks = [
            (l, t, l + 10, t),
            (l, t, l, t + 10),
            (r, t, r - 10, t),
            (r, t, r, t + 10),
            (l, b, l + 10, b),
            (l, b, l, b - 10),
            (r, b, r - 10, b),
            (r, b, r, b - 10),
        ];
        for (i, (x1, y1, x2, y2)) in ticks.into_iter().enumerate() {
            ops.push(SceneOp::Insert(line(ID_CORNERS + i as u32, x1, y1, x2, y2, GREEN)));
        }
        ops
    }

    fn labels(ops: &mut Vec<SceneOp>, labels: &[(&str, Box)]) {
        for (i, (label, b)) in labels.iter().enumerate() {
            let (x, y) = (b.x + 12, b.y + 6);
            ops.push(SceneOp::Insert(text(ID_LABELS + i as u32, &SMALL, x, y, *label, DIM)));
            let rx = x + label.len() as i32 * SMALL.cell_w + 8;
            ops.push(SceneOp::Insert(line(ID_LABEL_RULES + i as u32, rx, y + 5, b.right() - 12, y + 5, GRID)));
        }
    }

    fn chat_fixtures(ops: &mut Vec<SceneOp>, area: Box) {
        ops.push(SceneOp::Insert(hit_rect(ids::CHAT_INPUT, Layer::Bg, area.x, area.y, area.w, area.h)));
        let tx = area.right() - 6;
        ops.push(SceneOp::Insert(line(ids::CHAT_TRACK, tx, area.y + CHAT_LABEL_H, tx, area.bottom() - 10, GRID)));
    }

    /// Chrome for the split layout (visual, status, log).
    pub fn split_ops(&self) -> Vec<SceneOp> {
        let mut ops = Vec::new();
        ops.push(SceneOp::Insert(line(ID_V_DIVIDER, VISUAL.right(), VISUAL.y, VISUAL.right(), SCREEN_H, DIM)));
        ops.push(SceneOp::Insert(line(ID_H_DIVIDER, STATUS.x, STATUS.bottom(), SCREEN_W, STATUS.bottom(), DIM)));
        Self::labels(&mut ops, &[("AVATAR", VISUAL), ("STATUS", STATUS), ("LOG", CHAT)]);
        // Faint grid behind the avatar.
        let mut g = ID_GRID;
        for x in [60, 120, 180, 240] {
            ops.push(SceneOp::Insert(line(g, x, VISUAL.y + 24, x, VISUAL.bottom() - 10, GRID)));
            g += 1;
        }
        for y in [104, 184, 264, 344, 424] {
            ops.push(SceneOp::Insert(line(g, VISUAL.x + 8, y, VISUAL.right() - 8, y, GRID)));
            g += 1;
        }
        Self::chat_fixtures(&mut ops, CHAT);
        ops
    }

    /// Chrome for the guide layout (guide L, one-line status, log).
    pub fn guide_ops(&self) -> Vec<SceneOp> {
        let mut ops = Vec::new();
        ops.push(SceneOp::Insert(line(ID_V_DIVIDER, GUIDE_HEAD.x, GUIDE_HEAD.y, GUIDE_HEAD.x, SCREEN_H, DIM)));
        ops.push(SceneOp::Insert(line(ID_H_DIVIDER, STATUS_GUIDE.x, STATUS_GUIDE.y, SCREEN_W, STATUS_GUIDE.y, DIM)));
        ops.push(SceneOp::Insert(line(ID_H_DIVIDER_2, CHAT_GUIDE.x, CHAT_GUIDE.y, SCREEN_W, CHAT_GUIDE.y, GRID)));
        Self::labels(&mut ops, &[("GUIDE", VISUAL), ("LOG", CHAT_GUIDE)]);
        Self::chat_fixtures(&mut ops, CHAT_GUIDE);
        ops
    }
}
