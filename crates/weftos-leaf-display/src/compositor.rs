//! Generic layer compositor — see `docs/leaf-push-protocol.md` §7.
//!
//! Routes received `LeafPush` ops into per-`LayerSlot` op-lists and
//! composites them bottom-up (`Bg → Widget → Text → Alert`) into any
//! [`LeafSurface`]. Layers are op-lists, not framebuffers — they are
//! re-composited each frame, which keeps leaf memory light (no N
//! full-resolution layer buffers on a constrained device).

use alloc::vec::Vec;

use embedded_graphics::image::{Image, ImageRaw};
use embedded_graphics::mono_font::{ascii::FONT_10X20, MonoTextStyle};
use embedded_graphics::pixelcolor::Rgb888;
use embedded_graphics::prelude::*;
use embedded_graphics::text::Text;
use weftos_leaf_types::{LayerSlot, LeafPush};

use crate::surface::LeafSurface;

/// Holds the active `LeafPush` ops per `LayerSlot` and composites
/// them into a [`LeafSurface`].
#[derive(Default)]
pub struct Compositor {
    /// Ops for `LayerSlot::Bg`, composited first (bottom layer).
    bg: Vec<LeafPush>,
    /// Ops for `LayerSlot::Widget`, composited above `Bg`.
    widget: Vec<LeafPush>,
    /// Ops for `LayerSlot::Text`, composited above `Widget`.
    text: Vec<LeafPush>,
    /// Ops for `LayerSlot::Alert`, composited last (top layer).
    alert: Vec<LeafPush>,
    /// Pending brightness, applied to the surface on the next compose.
    brightness: Option<u32>,
}

impl Compositor {
    /// Create an empty compositor: no ops in any layer and no pending
    /// brightness.
    pub fn new() -> Self {
        Self::default()
    }

    fn layer_mut(&mut self, z: LayerSlot) -> &mut Vec<LeafPush> {
        match z {
            LayerSlot::Bg => &mut self.bg,
            LayerSlot::Widget => &mut self.widget,
            LayerSlot::Text => &mut self.text,
            LayerSlot::Alert => &mut self.alert,
        }
    }

    /// Route one received `LeafPush` into its layer's op-list (or,
    /// for brightness, stash it for the next compose).
    pub fn apply(&mut self, push: LeafPush) {
        match &push {
            LeafPush::DisplayClear(c) => {
                let z = c.z;
                self.layer_mut(z).clear();
            }
            LeafPush::DisplayText(t) => {
                let z = t.z;
                let clear_first = t.clear_first;
                if clear_first {
                    self.layer_mut(z).clear();
                }
                self.layer_mut(z).push(push);
            }
            LeafPush::DisplayImage(i) => {
                let z = i.z;
                self.layer_mut(z).push(push);
            }
            LeafPush::LayerEffect(e) => {
                // v1: the effect op is stored on its layer but the
                // compositor does not yet apply `LayerEffectKind`.
                let z = e.z;
                self.layer_mut(z).push(push);
            }
            LeafPush::DisplayBrightness { on_us } => {
                self.brightness = Some(*on_us);
            }
            LeafPush::Audio(_) => { /* not a display op */ }
            // `LeafPush` is #[non_exhaustive]; future variants an old
            // compositor doesn't understand are ignored.
            _ => {}
        }
    }

    /// Composite all layers bottom-up into the surface's frame and
    /// present. Applies any pending brightness first.
    pub fn compose<S: LeafSurface>(&mut self, surface: &mut S) -> Result<(), S::Error> {
        if let Some(on_us) = self.brightness.take() {
            surface.set_brightness(on_us)?;
        }
        let cap = surface.capability();
        {
            let mut frame = surface.frame();
            frame.clear(Rgb888::BLACK)?;
            for layer in [&self.bg, &self.widget, &self.text, &self.alert] {
                for push in layer {
                    draw_push(&mut frame, push, cap.width, cap.height);
                }
            }
        }
        surface.present()
    }
}

/// Draw a single display `LeafPush` op onto a frame. Per-op draw
/// errors are deliberately swallowed — a malformed or off-screen op
/// must not abort compositing of the rest of the frame.
fn draw_push<D>(target: &mut D, push: &LeafPush, w: u32, h: u32)
where
    D: DrawTarget<Color = Rgb888>,
{
    match push {
        LeafPush::DisplayText(t) => {
            let color = Rgb888::new(t.color[0], t.color[1], t.color[2]);
            let style = MonoTextStyle::new(&FONT_10X20, color);
            let _ = Text::new(&t.text, Point::new(t.x, t.y), style).draw(target);
        }
        LeafPush::DisplayImage(i) => {
            // `rgb` is RGB888 row-major at the display's own
            // dimensions — the wire type carries no width/height of
            // its own (see weftos-leaf-types::DisplayImage).
            if i.rgb.len() as u32 == w.saturating_mul(h).saturating_mul(3) {
                let raw = ImageRaw::<Rgb888>::new(&i.rgb, w);
                let _ = Image::new(&raw, Point::zero()).draw(target);
            }
        }
        // DisplayClear is handled in `apply` (it empties the layer's
        // op-list). LayerEffect / DisplayBrightness / Audio are not
        // drawn here.
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use weftos_leaf_types::{DisplayClear, DisplayText};

    /// Minimal no_std mock surface for compositor unit tests — a
    /// Vec-backed frame, so these run under default features.
    struct MockSurface {
        w: u32,
        h: u32,
        buf: Vec<Rgb888>,
        presents: u32,
    }
    impl MockSurface {
        fn new(w: u32, h: u32) -> Self {
            Self {
                w,
                h,
                buf: alloc::vec![Rgb888::BLACK; (w * h) as usize],
                presents: 0,
            }
        }
    }
    struct MockFrame<'a> {
        w: u32,
        h: u32,
        buf: &'a mut [Rgb888],
    }
    impl OriginDimensions for MockFrame<'_> {
        fn size(&self) -> Size {
            Size::new(self.w, self.h)
        }
    }
    impl DrawTarget for MockFrame<'_> {
        type Color = Rgb888;
        type Error = core::convert::Infallible;
        fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
        where
            I: IntoIterator<Item = Pixel<Self::Color>>,
        {
            for Pixel(coord, color) in pixels {
                if let (Ok(x), Ok(y)) = (u32::try_from(coord.x), u32::try_from(coord.y)) {
                    if x < self.w && y < self.h {
                        self.buf[(y * self.w + x) as usize] = color;
                    }
                }
            }
            Ok(())
        }
    }
    impl LeafSurface for MockSurface {
        type Frame<'a> = MockFrame<'a>;
        type Error = core::convert::Infallible;
        fn capability(&self) -> weftos_leaf_types::DisplaySinkCap {
            weftos_leaf_types::DisplaySinkCap {
                width: self.w,
                height: self.h,
                pixel_format: alloc::string::String::from("rgb888"),
                layers: 4,
                blend_modes: alloc::vec![alloc::string::String::from("normal")],
            }
        }
        fn frame(&mut self) -> MockFrame<'_> {
            MockFrame {
                w: self.w,
                h: self.h,
                buf: &mut self.buf,
            }
        }
        fn present(&mut self) -> Result<(), Self::Error> {
            self.presents += 1;
            Ok(())
        }
    }

    #[test]
    fn compose_clears_and_presents() {
        let mut comp = Compositor::new();
        let mut surf = MockSurface::new(64, 32);
        comp.compose(&mut surf).unwrap();
        assert_eq!(surf.presents, 1);
        assert!(surf.buf.iter().all(|p| *p == Rgb888::BLACK));
    }

    #[test]
    fn display_clear_empties_layer() {
        let mut comp = Compositor::new();
        comp.apply(LeafPush::DisplayText(DisplayText {
            z: LayerSlot::Text,
            text: alloc::string::String::from("hi"),
            x: 0,
            y: 12,
            color: [255, 255, 255],
            clear_first: false,
        }));
        assert_eq!(comp.text.len(), 1);
        comp.apply(LeafPush::DisplayClear(DisplayClear { z: LayerSlot::Text }));
        assert_eq!(comp.text.len(), 0);
    }

    #[test]
    fn brightness_routes_to_surface() {
        let mut comp = Compositor::new();
        comp.apply(LeafPush::DisplayBrightness { on_us: 42 });
        assert_eq!(comp.brightness, Some(42));
        let mut surf = MockSurface::new(8, 8);
        comp.compose(&mut surf).unwrap();
        // brightness consumed on compose
        assert_eq!(comp.brightness, None);
    }
}
