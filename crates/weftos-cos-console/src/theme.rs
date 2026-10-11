//! Layout boxes, theme colours, fonts and node-id ranges for the
//! chief-of-staff console. Values are ADR-118 §1 and the lane contract
//! (`docs/plans/adr-118-cos-contract.md` §2), verbatim.

use core::ops::Range;

use weftos_leaf_scene::{BuiltinFont, DisplayId, NodeId, Rgba};

/// The console draws on display 0 of its session.
pub const DISPLAY: DisplayId = 0;

pub const SCREEN_W: i32 = 800;
pub const SCREEN_H: i32 = 480;

/// Integer-pixel box. The scene wire is Q24.8; everything in the
/// console is laid out in whole pixels and converted at the edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Box {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Box {
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self { x, y, w, h }
    }
    pub const fn right(&self) -> i32 {
        self.x + self.w
    }
    pub const fn bottom(&self) -> i32 {
        self.y + self.h
    }
    pub const fn contains(&self, px: i32, py: i32) -> bool {
        px >= self.x && px < self.right() && py >= self.y && py < self.bottom()
    }
    pub const fn overlaps(&self, o: &Box) -> bool {
        self.x < o.right() && o.x < self.right() && self.y < o.bottom() && o.y < self.bottom()
    }
}

/// Firmware-owned on the pad (ADR-117 §5); the console draws a
/// placeholder here only in the simulator.
pub const SYSTEM_STRIP: Box = Box::new(0, 0, 800, 24);
/// Avatar now; page mode later.
pub const VISUAL: Box = Box::new(0, 24, 300, 456);
/// Agent state, task, model, lanes, push-to-talk, time.
pub const STATUS: Box = Box::new(300, 24, 500, 64);
/// Chat log, newest at the bottom.
pub const CHAT: Box = Box::new(300, 88, 500, 392);
/// The visual panel expanded for page mode.
pub const PAGE: Box = Box::new(0, 24, 800, 456);

/// Phosphor green: primary text and strokes.
pub const GREEN: Rgba = Rgba::opaque(0x33, 0xFF, 0x66);
/// Dim green: history and secondary text.
pub const DIM: Rgba = Rgba::opaque(0x12, 0x7A, 0x36);
/// Amber: warnings and blocked states only.
pub const AMBER: Rgba = Rgba::opaque(0xFF, 0xB0, 0x00);
/// Amber at history weight, for the blocked pose's secondary strokes.
pub const AMBER_DIM: Rgba = Rgba::opaque(0x80, 0x58, 0x00);
/// Between dim and the grid: far-side wireframe strokes.
pub const FAINT: Rgba = Rgba::opaque(0x0B, 0x4C, 0x20);
/// Faint green for the background grid and rule motif.
pub const GRID: Rgba = Rgba::opaque(0x07, 0x2C, 0x14);
pub const BLACK: Rgba = Rgba::BLACK;

/// 1 px vector strokes everywhere (Q8.8).
pub const STROKE_1PX: u16 = 1 << 8;

/// Built-in monospaced font with its fixed cell metrics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FontMetrics {
    pub face: BuiltinFont,
    pub cell_w: i32,
    pub cell_h: i32,
    pub size_q8: u16,
}

impl FontMetrics {
    /// How many whole glyph cells fit in `width_px`.
    pub const fn cols(&self, width_px: i32) -> usize {
        if width_px <= 0 {
            0
        } else {
            (width_px / self.cell_w) as usize
        }
    }
}

/// Body text: chat lines, status headline, page paragraphs.
pub const BODY: FontMetrics = FontMetrics {
    face: BuiltinFont::Mono10x20,
    cell_w: 10,
    cell_h: 20,
    size_q8: 20 << 8,
};
/// Labels and secondary lines.
pub const SMALL: FontMetrics = FontMetrics {
    face: BuiltinFont::Mono6x10,
    cell_w: 6,
    cell_h: 10,
    size_q8: 10 << 8,
};

/// Horizontal padding inside a region.
pub const PAD: i32 = 14;
/// Vertical pitch of a body line (20 px glyph + 2 px leading).
pub const LINE_PITCH: i32 = 22;
/// Top of the first chat line (below the "LOG" label).
pub const CHAT_TEXT_TOP: i32 = CHAT.y + 26;
/// Bottom of the last chat line.
pub const CHAT_TEXT_BOTTOM: i32 = CHAT.bottom() - 10;
/// Text column of the chat log in body cells.
pub const CHAT_COLS: usize = BODY.cols(CHAT.w - 2 * PAD - 8);
/// Visible chat lines.
pub const CHAT_VISIBLE: usize = ((CHAT_TEXT_BOTTOM - CHAT_TEXT_TOP) / LINE_PITCH) as usize;

/// Page mode text geometry.
pub const PAGE_MARGIN: i32 = 16;
pub const PAGE_TEXT_TOP: i32 = PAGE.y + 58;
pub const PAGE_TEXT_BOTTOM: i32 = PAGE.bottom() - 22;
pub const PAGE_COLS: usize = BODY.cols(PAGE.w - 2 * PAGE_MARGIN - 8);
pub const PAGE_VISIBLE: usize = ((PAGE_TEXT_BOTTOM - PAGE_TEXT_TOP) / LINE_PITCH) as usize;

/// Node-id ranges, so lanes never collide (contract §2). `PAGE` is
/// this lane's claim on the gap the contract table leaves open.
pub mod ids {
    use core::ops::Range;

    pub const CHROME: Range<u32> = 1..100;
    pub const AVATAR: Range<u32> = 100..200;
    pub const STATUS: Range<u32> = 200..300;
    pub const PAGE: Range<u32> = 300..1000;
    pub const CHAT: Range<u32> = 1000..4096;

    /// Chat-line pool; the top of the range is kept for fixed chat
    /// fixtures (scroll thumb and the new-messages marker).
    pub const CHAT_LINES: Range<u32> = 1000..4090;
    pub const CHAT_THUMB: u32 = 4092;
    pub const CHAT_MARKER_BG: u32 = 4093;
    pub const CHAT_MARKER_RULE: u32 = 4094;
    pub const CHAT_MARKER_TEXT: u32 = 4095;

    /// Page-line pool, below the page chrome ids (300..400).
    pub const PAGE_LINES: Range<u32> = 400..1000;
    pub const PAGE_INPUT: u32 = 300;
    pub const PAGE_TITLE: u32 = 301;
    pub const PAGE_RULE: u32 = 302;
    pub const PAGE_HINT: u32 = 303;
    pub const PAGE_THUMB: u32 = 304;

    /// Chrome fixtures the console reacts to.
    pub const CHAT_INPUT: u32 = 30;
    pub const CHAT_TRACK: u32 = 40;
}

/// Node id on the console's display.
pub const fn nid(n: u32) -> NodeId {
    NodeId::from_parts(DISPLAY, n)
}

/// Which range a raw node number belongs to, for the collision tests.
pub fn range_of(n: u32) -> Option<&'static str> {
    const TABLE: [(&str, Range<u32>); 5] = [
        ("chrome", ids::CHROME),
        ("avatar", ids::AVATAR),
        ("status", ids::STATUS),
        ("page", ids::PAGE),
        ("chat", ids::CHAT),
    ];
    TABLE
        .iter()
        .find(|(_, r)| r.contains(&n))
        .map(|(name, _)| *name)
}
