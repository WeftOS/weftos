//! The display session the console draws through.
//!
//! `DisplaySession` has the shape of contract §1 (`apply`,
//! `poll_event`, `size`). Lane A's `weftos-display-client` provides the
//! real one; until merge the console codes against this trait and the
//! tests use [`SimSession`], which runs a `SceneStore` and a
//! `SimSurface` in process.

use std::fmt::Debug;

use weftos_leaf_scene::{NodeId, SceneOp};

/// Object-level input from the leaf (contract §1). `Release` is the
/// console's addition: the end of a touch gesture, so a swipe can be
/// judged per gesture. A session that never emits it still works; the
/// console then resets its gesture on the next `Tap`.
#[derive(Debug, Clone, PartialEq)]
pub enum ObjectEvent {
    Tap { node: NodeId },
    Drag { node: NodeId, dx: i32, dy: i32 },
    Release { node: NodeId },
    Sensor { name: String, value: f64 },
}

pub trait DisplaySession {
    type Error: Debug;

    /// Apply a batch of scene ops to display 0.
    fn apply(&mut self, ops: &[SceneOp]) -> Result<(), Self::Error>;
    /// Next pending object event, if any.
    fn poll_event(&mut self) -> Option<ObjectEvent>;
    /// Display size in pixels.
    fn size(&self) -> (u32, u32);
}

#[cfg(feature = "sim")]
pub use sim::SimSession;

#[cfg(feature = "sim")]
mod sim {
    use std::collections::VecDeque;
    use std::path::Path;

    use embedded_graphics::{geometry::Point as EgPoint, pixelcolor::Rgb888, prelude::RgbColor};
    use embedded_graphics_simulator::OutputSettingsBuilder;
    use weftos_leaf_renderer::{render_damage, RenderError};
    use weftos_leaf_scene::{px, DamageSet, NodeId, Rect, Rgba, SceneOp, SceneStore};
    use weftos_leaf_sim::{SimError, SimSurface};

    use super::{DisplaySession, ObjectEvent};
    use crate::theme::{BLACK, DISPLAY};

    /// Movement under this many pixels between down and up is a tap.
    const TAP_SLOP_PX: i32 = 6;

    /// In-process session: a `SceneStore` plus a headless `SimSurface`.
    /// Touch is synthesised with `touch_down` / `touch_move` /
    /// `touch_up`, which hit-test the store and queue object events the
    /// way a leaf would.
    pub struct SimSession {
        store: SceneStore,
        surface: SimSurface,
        events: VecDeque<ObjectEvent>,
        size: (u32, u32),
        gesture: Option<Gesture>,
        now_ms: u32,
    }

    struct Gesture {
        node: NodeId,
        last: (i32, i32),
        moved: i32,
    }

    impl SimSession {
        pub fn new(width: u32, height: u32) -> Self {
            let mut store = SceneStore::new();
            store.set_viewport(DISPLAY, Rect::from_px(0, 0, width as i32, height as i32));
            let _ = store.set_bg(DISPLAY, BLACK);
            let mut surface = SimSurface::new(width, height, "WeftOS chief-of-staff console");
            surface.set_clear_color(BLACK);
            Self {
                store,
                surface,
                events: VecDeque::new(),
                size: (width, height),
                gesture: None,
                now_ms: 0,
            }
        }

        pub fn store(&self) -> &SceneStore {
            &self.store
        }

        /// Advance the leaf clock (snaps pending tweens) and redraw
        /// the whole frame into the framebuffer.
        pub fn render(&mut self, now_ms: u32) -> Result<(), RenderError<SimError>> {
            self.now_ms = now_ms;
            let _ = self.store.tick(now_ms);
            render_damage(&self.store, DISPLAY, &DamageSet::full(), &mut self.surface).map(|_| ())
        }

        /// Write the current framebuffer as a PNG.
        pub fn save_png(&self, path: impl AsRef<Path>) -> Result<(), String> {
            let settings = OutputSettingsBuilder::new().scale(1).build();
            self.surface
                .display()
                .to_rgb_output_image(&settings)
                .save_png(path.as_ref())
                .map_err(|e| e.to_string())
        }

        /// Colour of one framebuffer pixel.
        pub fn pixel(&self, x: i32, y: i32) -> Rgba {
            let p: Rgb888 = self.surface.display().get_pixel(EgPoint::new(x, y));
            Rgba::opaque(p.r(), p.g(), p.b())
        }

        /// Queue a leaf sensor event (the pad's push button, or the
        /// simulator's space bar).
        pub fn push_sensor(&mut self, name: &str, value: f64) {
            self.events.push_back(ObjectEvent::Sensor {
                name: name.to_string(),
                value,
            });
        }

        /// Finger down at `(x, y)`; the topmost interactive node under
        /// it owns the gesture.
        pub fn touch_down(&mut self, x: i32, y: i32) {
            let node = self.store.hit_test(DISPLAY, px(x), px(y));
            self.gesture = node.map(|node| Gesture {
                node,
                last: (x, y),
                moved: 0,
            });
        }

        /// Finger moved; emits a `Drag` with the delta since the last
        /// point.
        pub fn touch_move(&mut self, x: i32, y: i32) {
            if let Some(g) = self.gesture.as_mut() {
                let dx = x - g.last.0;
                let dy = y - g.last.1;
                g.last = (x, y);
                g.moved += dx.abs() + dy.abs();
                if dx != 0 || dy != 0 {
                    self.events.push_back(ObjectEvent::Drag {
                        node: g.node,
                        dx,
                        dy,
                    });
                }
            }
        }

        /// Finger up; a short gesture becomes a `Tap`, and every
        /// gesture ends with a `Release`.
        pub fn touch_up(&mut self) {
            if let Some(g) = self.gesture.take() {
                if g.moved < TAP_SLOP_PX {
                    self.events.push_back(ObjectEvent::Tap { node: g.node });
                }
                self.events.push_back(ObjectEvent::Release { node: g.node });
            }
        }

        /// Whole gesture in one call: down, a straight move, up.
        pub fn swipe(&mut self, from: (i32, i32), to: (i32, i32), steps: u32) {
            self.touch_down(from.0, from.1);
            let steps = steps.max(1) as i32;
            for i in 1..=steps {
                let x = from.0 + (to.0 - from.0) * i / steps;
                let y = from.1 + (to.1 - from.1) * i / steps;
                self.touch_move(x, y);
            }
            self.touch_up();
        }

        pub fn now_ms(&self) -> u32 {
            self.now_ms
        }
    }

    impl DisplaySession for SimSession {
        type Error = RenderError<SimError>;

        fn apply(&mut self, ops: &[SceneOp]) -> Result<(), Self::Error> {
            for op in ops {
                let _ = self.store.apply_op(DISPLAY, op);
            }
            Ok(())
        }

        fn poll_event(&mut self) -> Option<ObjectEvent> {
            self.events.pop_front()
        }

        fn size(&self) -> (u32, u32) {
            self.size
        }
    }
}
