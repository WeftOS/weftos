//! `SimSurface` — `SceneSurface` impl backed by an
//! `embedded_graphics_simulator::SimulatorDisplay<Rgb888>`.
//!
//! Renders every `weftos-leaf-scene` primitive using
//! `embedded-graphics`'s built-in rasterizer:
//!
//! - `Primitive::Rect` → `embedded_graphics::primitives::Rectangle`
//! - `Primitive::Line` → `embedded_graphics::primitives::Line`
//! - `Primitive::Circle` → `embedded_graphics::primitives::Circle`
//! - `Primitive::Text { face: Builtin(_), .. }` →
//!   `embedded_graphics::text::Text` with the matching MonoFont
//! - `Primitive::Bitmap` → straight-blit via the `decode_bitmap` helper
//! - `Primitive::Path` → skipped (v1.1)
//!
//! The sim declares
//! `ALPHA | SUBPIXEL | ANTIALIASED | BLEND_MODES`. AA is handled by
//! `embedded-graphics`'s integer rasterizer (mono-pixel AA pattern;
//! good enough for dev preview).
//!
//! Backed by a `SimulatorDisplay<Rgb888>` for rich colour. The
//! `show()` helper opens a windowed presenter; `pixels()` returns
//! the raw buffer for golden tests.

use std::convert::TryInto;

use embedded_graphics::{
    geometry::{Point as EgPoint, Size as EgSize},
    mono_font::{ascii::FONT_10X20, ascii::FONT_6X10, MonoFont, MonoTextStyle},
    pixelcolor::Rgb888,
    prelude::*,
    // Import the trait under a non-clashing alias so our local
    // `Primitive` (the scene-graph enum) doesn't shadow it.
    primitives::{
        Circle as EgCircle, Line as EgLine, Primitive as EgPrimitive,
        PrimitiveStyle as EgPrimStyle, PrimitiveStyleBuilder, Rectangle as EgRectangle,
    },
    text::{Baseline, Text as EgText},
    Pixel,
};
use embedded_graphics_simulator::{OutputSettingsBuilder, SimulatorDisplay};

use weftos_leaf_renderer::{decode_bitmap, CapabilityMask, SceneSurface};
use weftos_leaf_scene::{
    from_px_q8, BuiltinFont, DamageSet, FontFace, Primitive, Rect, Rgba, Style, Transform,
};

/// Backend error type. Most operations succeed unconditionally; the
/// error path exists for the few cases where the sim genuinely can't
/// render (e.g., a `Primitive::Path` in v1) so the renderer's
/// `RenderError::DrawPrimitive` propagates a useful diagnostic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SimError {
    /// Primitive variant not implemented in v1's sim (Path).
    UnsupportedPrimitive(&'static str),
    /// Bitmap decode failed (v1 supports Raw8888 + Raw565).
    BitmapDecode(weftos_leaf_renderer::BitmapError),
    /// Vector font requested — v1.1.
    UnsupportedFont(&'static str),
}

impl core::fmt::Display for SimError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::UnsupportedPrimitive(s) => write!(f, "unsupported primitive: {s}"),
            Self::BitmapDecode(e) => write!(f, "bitmap decode failed: {e:?}"),
            Self::UnsupportedFont(s) => write!(f, "unsupported font: {s}"),
        }
    }
}

impl std::error::Error for SimError {}

/// Desktop simulator surface.
///
/// Internally wraps a `SimulatorDisplay<Rgb888>` plus the window
/// title. Construction does **not** open a window — call
/// [`SimSurface::show`] after `render_damage` to display the result.
pub struct SimSurface {
    display: SimulatorDisplay<Rgb888>,
    title: String,
    /// Background colour for `begin_frame` clears on full-repaint.
    /// Tracks the most recent display.bg the renderer told us about,
    /// but defaults to opaque black so an empty frame is visible.
    clear_color: Rgba,
}

impl SimSurface {
    /// Create a sim surface of `width × height` pixels with the given
    /// window title. The display starts cleared to opaque black.
    pub fn new(width: u32, height: u32, title: impl Into<String>) -> Self {
        let display = SimulatorDisplay::new(EgSize::new(width, height));
        Self {
            display,
            title: title.into(),
            clear_color: Rgba::BLACK,
        }
    }

    /// Set the colour `begin_frame` clears to when `damage.is_full()`.
    ///
    /// Producers normally set this via `SceneStore::set_bg` + the
    /// renderer reading `display.bg`; this hook exists for examples
    /// that want to override (e.g., for white-on-dark previews).
    pub fn set_clear_color(&mut self, color: Rgba) {
        self.clear_color = color;
    }

    /// Borrow the underlying `SimulatorDisplay`. Useful for golden
    /// tests that want to snapshot the raw pixel buffer.
    #[inline]
    pub fn display(&self) -> &SimulatorDisplay<Rgb888> {
        &self.display
    }

    /// Pop a window and block until the user closes it.
    ///
    /// Requires the `window` feature (which enables SDL2). On
    /// headless CI without SDL2, builds work but this function panics
    /// at runtime — which is fine because it's only called from
    /// `examples/`.
    pub fn show(&self) {
        use embedded_graphics_simulator::Window;
        let settings = OutputSettingsBuilder::new().scale(1).build();
        let mut window = Window::new(&self.title, &settings);
        window.show_static(&self.display);
    }
}

impl SceneSurface for SimSurface {
    type Error = SimError;

    fn capabilities(&self) -> CapabilityMask {
        // The sim "cheats" per spec — devs see rich rendering even
        // before backends catch up. Phase C DPI declares empty();
        // Phase D canvas adds BITMAP_PNG on top of this.
        CapabilityMask::ALPHA
            | CapabilityMask::SUBPIXEL
            | CapabilityMask::ANTIALIASED
            | CapabilityMask::BLEND_MODES
    }

    fn begin_frame(&mut self, damage: &DamageSet, _viewport: Rect) -> Result<(), Self::Error> {
        if damage.is_full() {
            // Conservative: clear to the configured background. v1.1
            // will skip this when the renderer indicates the
            // background is opaque and covered by drawn nodes.
            let _ = self
                .display
                .clear(Rgb888::new(self.clear_color.r, self.clear_color.g, self.clear_color.b));
        } else {
            // Partial damage: the renderer only redraws nodes that
            // overlap a rect, so pixels a node left behind (moved,
            // shrunk, removed) must be cleared first or they ghost.
            let bg = Rgb888::new(self.clear_color.r, self.clear_color.g, self.clear_color.b);
            for r in damage.rects() {
                let w = from_px_q8(r.w).max(0) as u32;
                let h = from_px_q8(r.h).max(0) as u32;
                let tl = EgPoint::new(from_px_q8(r.x), from_px_q8(r.y));
                let _ = EgRectangle::new(tl, EgSize::new(w, h))
                    .into_styled(EgPrimStyle::with_fill(bg))
                    .draw(&mut self.display);
            }
        }
        Ok(())
    }

    fn draw_primitive(
        &mut self,
        primitive: &Primitive,
        style: &Style,
        transform: &Transform,
    ) -> Result<(), Self::Error> {
        // v1 honours translation only; rotation/scale fall back to
        // translation per design doc §5.4 (Phase A's `Transform`).
        let ox = from_px_q8(transform.x);
        let oy = from_px_q8(transform.y);

        match primitive {
            Primitive::Rect { w, h, radius_q8: _ } => {
                let w_px = from_px_q8(*w).max(0);
                let h_px = from_px_q8(*h).max(0);
                let rect = EgRectangle::new(
                    EgPoint::new(ox, oy),
                    EgSize::new(w_px as u32, h_px as u32),
                );
                let eg_style = build_eg_style(style);
                let _ = rect.into_styled(eg_style).draw(&mut self.display);
                Ok(())
            }
            Primitive::Line { x2, y2, thickness_q8 } => {
                let x2_px = from_px_q8(*x2);
                let y2_px = from_px_q8(*y2);
                let line = EgLine::new(
                    EgPoint::new(ox, oy),
                    EgPoint::new(ox + x2_px, oy + y2_px),
                );
                let thickness = ((*thickness_q8 as u32 + 128) >> 8).max(1);
                let color = style.stroke.or(style.fill).unwrap_or(Rgba::WHITE);
                let eg_style = EgPrimStyle::with_stroke(rgba_to_rgb888(color), thickness);
                let _ = line.into_styled(eg_style).draw(&mut self.display);
                Ok(())
            }
            Primitive::Circle { radius_q16 } => {
                // Q16.16 → Q24.8 → px.
                let r_q8: i32 = (*radius_q16 >> 8).try_into().unwrap_or(i32::MAX);
                let r_px = from_px_q8(r_q8).max(0) as u32;
                let diameter = r_px.saturating_mul(2);
                let top_left = EgPoint::new(ox - r_px as i32, oy - r_px as i32);
                let circle = EgCircle::new(top_left, diameter);
                let eg_style = build_eg_style(style);
                let _ = circle.into_styled(eg_style).draw(&mut self.display);
                Ok(())
            }
            Primitive::Text { content, face, .. } => {
                let mono = match face {
                    FontFace::Builtin(BuiltinFont::Mono6x10) => &FONT_6X10,
                    FontFace::Builtin(BuiltinFont::Mono10x20) => &FONT_10X20,
                    FontFace::Vector { .. } => {
                        return Err(SimError::UnsupportedFont("FontFace::Vector"));
                    }
                    FontFace::Inline { .. } => {
                        return Err(SimError::UnsupportedFont("FontFace::Inline"));
                    }
                };
                let color = style.fill.unwrap_or(Rgba::WHITE);
                let text_style: MonoTextStyle<'_, Rgb888> = MonoTextStyle::new(
                    mono as &MonoFont<'_>,
                    rgba_to_rgb888(color),
                );
                let _ = EgText::with_baseline(
                    content,
                    EgPoint::new(ox, oy),
                    text_style,
                    Baseline::Top,
                )
                .draw(&mut self.display);
                Ok(())
            }
            Primitive::Bitmap { w, h, format, data } => {
                let decoded = decode_bitmap(*w, *h, *format, data).map_err(SimError::BitmapDecode)?;
                // Iterate decoded pixels and draw — embedded-graphics
                // doesn't have a direct RGBA blit, so we project each
                // pixel via `Pixel` + `draw_iter`. Adequate for the
                // dev sim's primitive bitmap path.
                let mut pixels = Vec::with_capacity((decoded.w * decoded.h) as usize);
                for y in 0..decoded.h {
                    for x in 0..decoded.w {
                        let p = decoded.pixel(x, y);
                        if p.a == 0 {
                            // Skip fully transparent pixels — alpha
                            // compositing is sim-only; the DPI backend
                            // collapses anyway.
                            continue;
                        }
                        pixels.push(Pixel(
                            EgPoint::new(ox + x as i32, oy + y as i32),
                            rgba_to_rgb888(p),
                        ));
                    }
                }
                let _ = self.display.draw_iter(pixels);
                Ok(())
            }
            Primitive::Path { .. } => {
                // v1.1: rasterize the path via lyon or similar.
                Err(SimError::UnsupportedPrimitive("Primitive::Path"))
            }
        }
    }

    fn end_frame(&mut self) -> Result<(), Self::Error> {
        // No-op for the sim — pixels live in our `display`. Callers
        // who want windowed presentation invoke `show()` explicitly;
        // golden tests read `display()` directly.
        Ok(())
    }
}

/// Build an embedded-graphics `PrimitiveStyle` from a scene `Style`.
fn build_eg_style(style: &Style) -> EgPrimStyle<Rgb888> {
    let mut b = PrimitiveStyleBuilder::new();
    if let Some(fill) = style.fill {
        b = b.fill_color(rgba_to_rgb888(fill));
    }
    if let Some(stroke) = style.stroke {
        let w = ((style.stroke_width_q8 as u32 + 128) >> 8).max(1);
        b = b.stroke_color(rgba_to_rgb888(stroke)).stroke_width(w);
    }
    b.build()
}

#[inline]
fn rgba_to_rgb888(c: Rgba) -> Rgb888 {
    // Drop alpha; the sim "supports" alpha in the capability sense
    // (a partially-transparent value won't be collapsed by the
    // renderer), but our downconvert here is straight RGB. v1.1 would
    // do an actual src-over blend against the destination pixel.
    Rgb888::new(c.r, c.g, c.b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use weftos_leaf_renderer::render_damage;
    use weftos_leaf_scene::{
        px, Layer, Node, NodeId, Primitive, Rgba, SceneOp, SceneStore, Style,
    };

    fn nid(d: u8, n: u32) -> NodeId {
        NodeId::from_parts(d, n)
    }

    #[test]
    fn capabilities_declare_rich_set() {
        let s = SimSurface::new(100, 100, "test");
        let c = s.capabilities();
        assert!(c.has_alpha());
        assert!(c.has_blend_modes());
        assert!(c.is_antialiased());
    }

    #[test]
    fn rect_draw_pipeline_runs_clean() {
        let mut store = SceneStore::new();
        store.set_viewport(0, Rect::from_px(0, 0, 800, 480));
        let node = Node {
            id: nid(0, 1),
            layer: Layer::Widget,
            transform: Transform::translate(px(10), px(20)),
            primitive: Primitive::Rect {
                w: px(40),
                h: px(30),
                radius_q8: 0,
            },
            style: Style::filled(Rgba::RED),
            input: None,
        };
        store.apply_op(0, &SceneOp::Insert(node));
        let mut surface = SimSurface::new(800, 480, "test");
        let stats = render_damage(&store, 0, &DamageSet::full(), &mut surface).unwrap();
        assert_eq!(stats.drawn, 1);
    }

    #[test]
    fn text_draw_pipeline_runs_clean() {
        use weftos_leaf_scene::{FontFace, KerningHint};
        let mut store = SceneStore::new();
        store.set_viewport(0, Rect::from_px(0, 0, 800, 480));
        let node = Node {
            id: nid(0, 1),
            layer: Layer::Text,
            transform: Transform::translate(px(10), px(20)),
            primitive: Primitive::Text {
                content: String::from("hello"),
                face: FontFace::Builtin(BuiltinFont::Mono6x10),
                size_q8: 10 << 8,
                weight: 400,
                kerning: KerningHint::Auto,
            },
            style: Style::filled(Rgba::WHITE),
            input: None,
        };
        store.apply_op(0, &SceneOp::Insert(node));
        let mut surface = SimSurface::new(800, 480, "test");
        let stats = render_damage(&store, 0, &DamageSet::full(), &mut surface).unwrap();
        assert_eq!(stats.drawn, 1);
    }

    #[test]
    fn vector_font_returns_unsupported() {
        use weftos_leaf_scene::{FontFace, FontStyle, KerningHint};
        let mut store = SceneStore::new();
        store.set_viewport(0, Rect::from_px(0, 0, 800, 480));
        let node = Node {
            id: nid(0, 1),
            layer: Layer::Text,
            transform: Transform::translate(px(10), px(20)),
            primitive: Primitive::Text {
                content: String::from("hello"),
                face: FontFace::Vector {
                    family: String::from("Inter"),
                    style: FontStyle::Normal,
                },
                size_q8: 10 << 8,
                weight: 400,
                kerning: KerningHint::Auto,
            },
            style: Style::filled(Rgba::WHITE),
            input: None,
        };
        store.apply_op(0, &SceneOp::Insert(node));
        let mut surface = SimSurface::new(800, 480, "test");
        let r = render_damage(&store, 0, &DamageSet::full(), &mut surface);
        // Renderer wraps surface errors in DrawPrimitive.
        match r {
            Err(weftos_leaf_renderer::RenderError::DrawPrimitive(SimError::UnsupportedFont(_))) => {}
            other => panic!("expected DrawPrimitive(UnsupportedFont), got {other:?}"),
        }
    }

    #[test]
    fn path_primitive_returns_unsupported() {
        let mut store = SceneStore::new();
        store.set_viewport(0, Rect::from_px(0, 0, 800, 480));
        let node = Node {
            id: nid(0, 1),
            layer: Layer::Widget,
            transform: Transform::IDENTITY,
            primitive: Primitive::Path {
                commands: vec![
                    weftos_leaf_scene::PathCmd::MoveTo { x: 0, y: 0 },
                    weftos_leaf_scene::PathCmd::LineTo {
                        x: px(10),
                        y: px(10),
                    },
                ],
            },
            style: Style::filled(Rgba::RED),
            input: None,
        };
        store.apply_op(0, &SceneOp::Insert(node));
        let mut surface = SimSurface::new(800, 480, "test");
        let r = render_damage(&store, 0, &DamageSet::full(), &mut surface);
        match r {
            Err(weftos_leaf_renderer::RenderError::DrawPrimitive(SimError::UnsupportedPrimitive(s))) => {
                assert!(s.contains("Path"));
            }
            other => panic!("expected DrawPrimitive(UnsupportedPrimitive), got {other:?}"),
        }
    }
}
