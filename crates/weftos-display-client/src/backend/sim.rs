//! In-process simulated leaf on `weftos-leaf-sim`.
//!
//! [`LeafSim`] is a cloneable handle. One clone goes into
//! [`DisplayTarget::LeafSim`](crate::DisplayTarget::LeafSim); a test
//! keeps another to inject touches, press the push-to-talk key and
//! inspect what the "leaf" holds. The scene store, hit-test and
//! rendering all live on the leaf side of the seam, exactly as they
//! will on the pad.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};

use embedded_graphics::geometry::Point as EgPoint;
use embedded_graphics::pixelcolor::{Rgb888, RgbColor};
use weftos_leaf_renderer::{render_damage, SceneSurface};
use weftos_leaf_scene::{
    px, DisplayId, InputEnvelope, InputEvent, NodeId, Rect, Rgba, Scene, SceneEnvelope,
    SceneStore,
};
use weftos_leaf_sim::SimSurface;

use super::{Backend, BackendEvent};
use crate::caps::{DisplayCapabilities, DisplaySize};
use crate::error::DisplayError;
use crate::event::SensorValue;

/// The one display a simulated leaf has.
pub const SIM_DISPLAY: DisplayId = 0;

pub(crate) struct SimInner {
    store: SceneStore,
    surface: SimSurface,
    size: DisplaySize,
    queue: VecDeque<BackendEvent>,
    frames: u64,
    closed: bool,
}

/// Handle to a simulated leaf. Clones share the same leaf.
#[derive(Clone)]
pub struct LeafSim {
    inner: Arc<Mutex<SimInner>>,
}

impl Default for LeafSim {
    fn default() -> Self {
        Self::new(DisplaySize::PAD.width, DisplaySize::PAD.height)
    }
}

impl LeafSim {
    /// A fresh leaf with a black `width`×`height` display.
    pub fn new(width: u32, height: u32) -> Self {
        let mut store = SceneStore::new();
        store.set_viewport(SIM_DISPLAY, Rect::from_px(0, 0, width as i32, height as i32));
        store.set_bg(SIM_DISPLAY, Rgba::BLACK);
        let surface = SimSurface::new(width, height, "WeftOS display");
        Self {
            inner: Arc::new(Mutex::new(SimInner {
                store,
                surface,
                size: DisplaySize { width, height },
                queue: VecDeque::new(),
                frames: 0,
                closed: false,
            })),
        }
    }

    pub(crate) fn lock(&self) -> MutexGuard<'_, SimInner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    // ── Input injection (what the GT911 / button would do) ───────────

    /// Touch down at whole-pixel `(x, y)`. Hit-tested like the pad does.
    pub fn touch_down(&self, x: i32, y: i32) {
        self.pointer(x, y, |x, y| InputEvent::PointerDown {
            pointer_id: 0,
            x,
            y,
            pressure_q8: 0xFF00,
        });
    }

    pub fn touch_move(&self, x: i32, y: i32) {
        self.pointer(x, y, |x, y| InputEvent::PointerMove {
            pointer_id: 0,
            x,
            y,
            pressure_q8: 0xFF00,
        });
    }

    pub fn touch_up(&self, x: i32, y: i32) {
        self.pointer(x, y, |x, y| InputEvent::PointerUp {
            pointer_id: 0,
            x,
            y,
        });
    }

    /// A clean down/up at one point.
    pub fn tap(&self, x: i32, y: i32) {
        self.touch_down(x, y);
        self.touch_up(x, y);
    }

    /// Push-to-talk press or release.
    pub fn ptt(&self, pressed: bool) {
        let name = if pressed {
            crate::event::SENSOR_PTT_DOWN
        } else {
            crate::event::SENSOR_PTT_UP
        };
        self.sensor(name, SensorValue::Bool(pressed));
    }

    /// Any leaf sensor reading.
    pub fn sensor(&self, name: &str, value: SensorValue) {
        self.lock().queue.push_back(BackendEvent::Sensor {
            name: name.to_string(),
            value,
        });
    }

    /// Make the leaf go away (link drop, power off).
    pub fn disconnect(&self) {
        let mut g = self.lock();
        g.closed = true;
        g.queue.push_back(BackendEvent::Closed);
    }

    fn pointer(&self, x: i32, y: i32, make: impl FnOnce(i32, i32) -> InputEvent) {
        let mut g = self.lock();
        let (xq, yq) = (px(x), px(y));
        let node = g.store.hit_test(SIM_DISPLAY, xq, yq);
        let env = InputEnvelope::new(SIM_DISPLAY, node, make(xq, yq));
        g.queue.push_back(BackendEvent::Input(env));
    }

    // ── Inspection (what the leaf holds) ─────────────────────────────

    pub fn node_count(&self) -> usize {
        self.lock()
            .store
            .display(SIM_DISPLAY)
            .map_or(0, |d| d.nodes.len())
    }

    pub fn has_node(&self, id: NodeId) -> bool {
        self.lock().store.node(id).is_some()
    }

    /// The leaf's own hit-test at whole-pixel `(x, y)`.
    pub fn hit_test(&self, x: i32, y: i32) -> Option<NodeId> {
        self.lock().store.hit_test(SIM_DISPLAY, px(x), px(y))
    }

    pub fn snapshot(&self) -> Scene {
        self.lock().store.to_snapshot(SIM_DISPLAY)
    }

    /// Rendered colour at whole-pixel `(x, y)`.
    pub fn pixel(&self, x: i32, y: i32) -> (u8, u8, u8) {
        let g = self.lock();
        let c: Rgb888 = g.surface.display().get_pixel(EgPoint::new(x, y));
        (c.r(), c.g(), c.b())
    }

    /// Frames rendered so far (one per applied envelope with damage).
    pub fn frames_rendered(&self) -> u64 {
        self.lock().frames
    }

    pub fn is_closed(&self) -> bool {
        self.lock().closed
    }

    /// Run `f` with the simulator display, for a window to blit from.
    #[cfg(feature = "window")]
    pub(crate) fn with_display<R>(
        &self,
        f: impl FnOnce(&embedded_graphics_simulator::SimulatorDisplay<Rgb888>) -> R,
    ) -> R {
        let g = self.lock();
        f(g.surface.display())
    }
}

/// The session-side wrapper that implements [`Backend`] over a handle.
pub(crate) struct SimBackend {
    sim: LeafSim,
}

impl SimBackend {
    pub(crate) fn new(sim: LeafSim) -> Self {
        Self { sim }
    }
}

impl Backend for SimBackend {
    fn display_id(&self) -> DisplayId {
        SIM_DISPLAY
    }

    fn size(&self) -> DisplaySize {
        self.sim.lock().size
    }

    fn capabilities(&self) -> DisplayCapabilities {
        DisplayCapabilities::sim(self.sim.lock().surface.capabilities())
    }

    fn apply(&mut self, env: &SceneEnvelope) -> Result<(), DisplayError> {
        let mut g = self.sim.lock();
        if g.closed {
            return Err(DisplayError::Closed);
        }
        let damage = g.store.apply(env);
        if damage.is_empty() {
            return Ok(());
        }
        let SimInner { store, surface, .. } = &mut *g;
        render_damage(store, SIM_DISPLAY, &damage, surface)
            .map_err(|e| DisplayError::Backend(format!("{e:?}")))?;
        g.frames += 1;
        Ok(())
    }

    fn poll(&mut self) -> Option<BackendEvent> {
        self.sim.lock().queue.pop_front()
    }

    fn close(&mut self) {
        let mut g = self.sim.lock();
        g.queue.clear();
    }
}
