//! Dark-matter warped grid wallpaper.
//!
//! Draws a dense grid of short line segments. Each segment's endpoint
//! is displaced by the gravitational contribution of a handful of
//! invisible "masses" that drift slowly across the canvas — so the
//! grid visibly bends where a mass is without the mass ever being
//! drawn. Chromatic aberration is simulated by painting the grid three
//! times with tiny channel-offsets in R / B.

use eframe::egui;

const CELL: f32 = 44.0;
/// How strongly a mass bends the grid near it. Higher = sharper wells.
const MASS_STRENGTH: f32 = 1400.0;
/// Softening radius so points very close to a mass don't explode.
const SOFTEN: f32 = 50.0;
/// Maximum displacement so lines never travel absurd distances.
const MAX_DISP: f32 = 24.0;
/// Pixel offset used for the R/B aberration passes.
const ABERRATION: f32 = 0.5;
/// Line width for each pass.
const STROKE_W: f32 = 0.7;

/// A drifting "mass" that bends the grid.
#[derive(Clone, Copy)]
pub struct Mass {
    /// Phase offsets for Lissajous-like motion.
    seed: f32,
    speed_x: f32,
    speed_y: f32,
    scale_x: f32,
    scale_y: f32,
    strength: f32,
}

impl Mass {
    const fn new(seed: f32, sx: f32, sy: f32, ax: f32, ay: f32, str_: f32) -> Self {
        Self {
            seed,
            speed_x: sx,
            speed_y: sy,
            scale_x: ax,
            scale_y: ay,
            strength: str_,
        }
    }

    fn pos(&self, t: f32, rect: egui::Rect) -> egui::Pos2 {
        let cx = rect.center().x;
        let cy = rect.center().y;
        let hx = rect.width() * 0.5 * self.scale_x;
        let hy = rect.height() * 0.5 * self.scale_y;
        egui::pos2(
            cx + (t * self.speed_x + self.seed).sin() * hx,
            cy + (t * self.speed_y + self.seed * 1.7).cos() * hy,
        )
    }
}

const MASSES: [Mass; 4] = [
    Mass::new(0.0, 0.21, 0.17, 0.65, 0.55, 1.0),
    Mass::new(1.8, 0.14, 0.27, 0.55, 0.75, 0.8),
    Mass::new(3.4, 0.33, 0.12, 0.8, 0.5, 0.9),
    Mass::new(5.1, 0.18, 0.23, 0.45, 0.8, 0.7),
];

pub fn paint(ui: &mut egui::Ui, rect: egui::Rect, time: f32) {
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, crate::theming::palette::GRID_BG);

    // Compute current mass positions once per frame.
    let positions: [egui::Pos2; 4] = std::array::from_fn(|i| MASSES[i].pos(time, rect));

    // Grid dimensions in cells.
    let cols = (rect.width() / CELL).ceil() as i32 + 2;
    let rows = (rect.height() / CELL).ceil() as i32 + 2;

    // Three passes — R / neutral / B — with a small pixel offset to
    // fake chromatic aberration. Very subtle, only visible on the
    // bent lines.
    // Much dimmer than the initial pass — wallpaper is a whisper, not a grid overlay.
    let passes: [(egui::Vec2, egui::Color32); 3] = [
        (
            egui::vec2(-ABERRATION, 0.0),
            crate::theming::palette::GRID_TINT_ROSE,
        ),
        (
            egui::vec2(0.0, 0.0),
            crate::theming::palette::GRID_TINT_SLATE,
        ),
        (
            egui::vec2(ABERRATION, 0.0),
            crate::theming::palette::GRID_TINT_BLUE,
        ),
    ];

    // Draw horizontal and vertical line segments, displacing each
    // endpoint by the summed mass contribution.
    for (offset, color) in passes {
        let stroke = egui::Stroke::new(STROKE_W, color);
        // Horizontal segments
        for r in 0..rows {
            let y = rect.top() + (r as f32) * CELL;
            for c in 0..cols {
                let x0 = rect.left() + (c as f32) * CELL;
                let x1 = x0 + CELL;
                let a = displace(egui::pos2(x0, y), &positions) + offset;
                let b = displace(egui::pos2(x1, y), &positions) + offset;
                painter.line_segment([a, b], stroke);
            }
        }
        // Vertical segments
        for c in 0..cols {
            let x = rect.left() + (c as f32) * CELL;
            for r in 0..rows {
                let y0 = rect.top() + (r as f32) * CELL;
                let y1 = y0 + CELL;
                let a = displace(egui::pos2(x, y0), &positions) + offset;
                let b = displace(egui::pos2(x, y1), &positions) + offset;
                painter.line_segment([a, b], stroke);
            }
        }
    }

    // Subtle vignette so the edges feel darker.
    let vignette = crate::theming::palette::GRID_VIGNETTE;
    painter.rect(
        rect,
        0.0,
        egui::Color32::TRANSPARENT,
        egui::Stroke::new(60.0, vignette),
        egui::StrokeKind::Inside,
    );
}

fn displace(p: egui::Pos2, masses: &[egui::Pos2; 4]) -> egui::Pos2 {
    let mut dx = 0.0_f32;
    let mut dy = 0.0_f32;
    for (i, mp) in masses.iter().enumerate() {
        let vx = mp.x - p.x;
        let vy = mp.y - p.y;
        let dist2 = vx * vx + vy * vy + SOFTEN * SOFTEN;
        let inv = MASS_STRENGTH * MASSES[i].strength / dist2;
        dx += vx * inv / dist2.sqrt();
        dy += vy * inv / dist2.sqrt();
    }
    let mag2 = dx * dx + dy * dy;
    if mag2 > MAX_DISP * MAX_DISP {
        let s = MAX_DISP / mag2.sqrt();
        dx *= s;
        dy *= s;
    }
    egui::pos2(p.x + dx, p.y + dy)
}
