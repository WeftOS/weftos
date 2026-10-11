//! The seam between a session and whatever is holding the screen.
//!
//! A backend is "the leaf" from the session's point of view: it keeps
//! the scene, renders it, hit-tests touches and reports raw input back.
//! Three exist: the in-process simulator, the desktop window on top of
//! it, and (later) the real leaf over the certified-leaf channel.

pub mod sim;

#[cfg(feature = "leaf")]
pub mod leaf;
#[cfg(feature = "window")]
pub mod window;

use weftos_leaf_scene::{DisplayId, InputEnvelope, SceneEnvelope};

use crate::caps::{DisplayCapabilities, DisplaySize};
use crate::error::DisplayError;
use crate::event::SensorValue;

/// What a backend hands up on `poll`.
#[derive(Debug, Clone, PartialEq)]
pub enum BackendEvent {
    /// A raw pointer event, already hit-tested by the leaf.
    Input(InputEnvelope),
    /// A leaf sensor reading (push-to-talk, later presence and light).
    Sensor { name: String, value: SensorValue },
    /// The backend went away (window closed, link dropped, grant
    /// revoked). The session closes itself on this.
    Closed,
}

pub trait Backend {
    fn display_id(&self) -> DisplayId;
    fn size(&self) -> DisplaySize;
    fn capabilities(&self) -> DisplayCapabilities;
    /// Deliver one envelope. The session has already bounded it.
    fn apply(&mut self, env: &SceneEnvelope) -> Result<(), DisplayError>;
    /// Next pending event, if any. Never blocks.
    fn poll(&mut self) -> Option<BackendEvent>;
    /// Release the display. Idempotent.
    fn close(&mut self);
}
