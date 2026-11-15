//! ANSI / xterm-256 terminal palette. Terminal colours are data
//! (Solarized-dark-ish, matching the native terminal), not UI chrome.

use eframe::egui::Color32;

/// Default terminal foreground.
pub const DEFAULT_FG: Color32 = Color32::from_rgb(220, 220, 225);
/// Terminal "bright foreground" (native backend only).
pub const BRIGHT_FG: Color32 = Color32::from_rgb(245, 245, 250);
/// Default terminal background (also the code-block background).
pub const DEFAULT_BG: Color32 = Color32::from_rgb(12, 12, 16);

/// The 16 base ANSI colours, indices 0..=15.
pub const PALETTE_16: [Color32; 16] = [
    Color32::from_rgb(7, 54, 66),
    Color32::from_rgb(220, 50, 47),
    Color32::from_rgb(133, 153, 0),
    Color32::from_rgb(181, 137, 0),
    Color32::from_rgb(38, 139, 210),
    Color32::from_rgb(211, 54, 130),
    Color32::from_rgb(42, 161, 152),
    Color32::from_rgb(238, 232, 213),
    Color32::from_rgb(88, 110, 117),
    Color32::from_rgb(255, 90, 87),
    Color32::from_rgb(180, 200, 0),
    Color32::from_rgb(220, 180, 30),
    Color32::from_rgb(100, 180, 240),
    Color32::from_rgb(240, 90, 170),
    Color32::from_rgb(80, 200, 190),
    Color32::from_rgb(253, 246, 227),
];

/// xterm-256 lookup: 0..16 base palette, 16..232 6x6x6 cube,
/// 232..256 greyscale ramp.
pub fn indexed(i: u8) -> Color32 {
    if i < 16 {
        return PALETTE_16[usize::from(i)];
    }
    if i < 232 {
        let i = i - 16;
        let conv = |v: u8| -> u8 { if v == 0 { 0 } else { 55 + v * 40 } };
        return super::palette::data_rgb(conv(i / 36), conv((i / 6) % 6), conv(i % 6));
    }
    let v = 8 + (i - 232) * 10;
    super::palette::data_rgb(v, v, v)
}
