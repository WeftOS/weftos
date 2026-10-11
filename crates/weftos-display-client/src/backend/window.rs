//! Desktop window backend: the simulated leaf shown in the simulator's
//! SDL2 window, with the mouse standing in for touch and the space bar
//! for the push-to-talk button.
//!
//! SDL wants its window and event pump on the main thread (macOS
//! enforces it), so open and poll the session from `main`.

use embedded_graphics_simulator::sdl2::{Keycode, MouseButton};
use embedded_graphics_simulator::{OutputSettingsBuilder, SimulatorEvent, Window};
use weftos_leaf_scene::{DisplayId, SceneEnvelope};

use super::sim::{LeafSim, SimBackend};
use super::{Backend, BackendEvent};
use crate::caps::{DisplayCapabilities, DisplaySize};
use crate::error::DisplayError;
use crate::target::WindowConfig;

pub(crate) struct WindowBackend {
    sim: LeafSim,
    inner: SimBackend,
    window: Window,
    mouse_down: bool,
    ptt_down: bool,
    dirty: bool,
    closed: bool,
}

impl WindowBackend {
    pub(crate) fn open(cfg: &WindowConfig) -> Result<Self, DisplayError> {
        let sim = LeafSim::new(cfg.width, cfg.height);
        let settings = OutputSettingsBuilder::new().scale(cfg.scale.max(1)).build();
        let mut window = Window::new(&cfg.title, &settings);
        // Show the blank panel straight away so the window exists before
        // the first `apply`.
        sim.with_display(|d| window.update(d));
        Ok(Self {
            inner: SimBackend::new(sim.clone()),
            sim,
            window,
            mouse_down: false,
            ptt_down: false,
            dirty: false,
            closed: false,
        })
    }

    /// Pump SDL and translate into leaf input.
    fn pump(&mut self) {
        let events: Vec<SimulatorEvent> = self.window.events().collect();
        for ev in events {
            match ev {
                SimulatorEvent::MouseButtonDown {
                    mouse_btn: MouseButton::Left,
                    point,
                } => {
                    self.mouse_down = true;
                    self.sim.touch_down(point.x, point.y);
                }
                SimulatorEvent::MouseMove { point } if self.mouse_down => {
                    self.sim.touch_move(point.x, point.y);
                }
                SimulatorEvent::MouseButtonUp {
                    mouse_btn: MouseButton::Left,
                    point,
                } => {
                    self.mouse_down = false;
                    self.sim.touch_up(point.x, point.y);
                }
                SimulatorEvent::KeyDown { keycode, repeat, .. }
                    if keycode == Keycode::Space && !repeat && !self.ptt_down =>
                {
                    self.ptt_down = true;
                    self.sim.ptt(true);
                }
                SimulatorEvent::KeyUp { keycode, .. }
                    if keycode == Keycode::Space && self.ptt_down =>
                {
                    self.ptt_down = false;
                    self.sim.ptt(false);
                }
                SimulatorEvent::Quit => {
                    self.closed = true;
                    self.sim.disconnect();
                }
                _ => {}
            }
        }
    }
}

impl Backend for WindowBackend {
    fn display_id(&self) -> DisplayId {
        self.inner.display_id()
    }

    fn size(&self) -> DisplaySize {
        self.inner.size()
    }

    fn capabilities(&self) -> DisplayCapabilities {
        self.inner.capabilities()
    }

    fn apply(&mut self, env: &SceneEnvelope) -> Result<(), DisplayError> {
        if self.closed {
            return Err(DisplayError::Closed);
        }
        self.inner.apply(env)?;
        self.dirty = true;
        Ok(())
    }

    fn poll(&mut self) -> Option<BackendEvent> {
        if !self.closed {
            self.pump();
            if self.dirty {
                let window = &mut self.window;
                self.sim.with_display(|d| window.update(d));
                self.dirty = false;
            }
        }
        self.inner.poll()
    }

    fn close(&mut self) {
        self.closed = true;
        self.inner.close();
    }
}
