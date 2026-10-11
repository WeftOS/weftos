//! Object-level events — what an app gets back from a display.
//!
//! The leaf owns the scene and does the hit-test (ADR-117 §2), so the
//! app sees *which object* was touched, never raw coordinates. Leaf
//! sensors (the pad's push-to-talk button, later presence and light)
//! arrive as [`ObjectEvent::Sensor`] with a dotted name.

use weftos_leaf_scene::NodeId;

/// Push-to-talk pressed. The pad's button, or the space bar on the
/// desktop window. Value is `SensorValue::Bool(true)`.
pub const SENSOR_PTT_DOWN: &str = "ptt.down";
/// Push-to-talk released. Value is `SensorValue::Bool(false)`.
pub const SENSOR_PTT_UP: &str = "ptt.up";

/// A reading carried by [`ObjectEvent::Sensor`].
#[derive(Debug, Clone, PartialEq)]
pub enum SensorValue {
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
}

/// One event from the display, already resolved to an object.
#[derive(Debug, Clone, PartialEq)]
pub enum ObjectEvent {
    /// A pointer went down and came up on `node` without dragging.
    Tap { node: NodeId },
    /// The pointer moved while held on `node`. `dx` / `dy` are whole
    /// pixels since the previous `Drag` (or since the pointer went
    /// down, for the first one). A drag never also produces a `Tap`.
    Drag { node: NodeId, dx: i32, dy: i32 },
    /// A leaf sensor event, such as [`SENSOR_PTT_DOWN`].
    Sensor { name: String, value: SensorValue },
}

impl ObjectEvent {
    /// Build the push-to-talk event for a press (`true`) or release.
    pub fn ptt(pressed: bool) -> Self {
        let name = if pressed { SENSOR_PTT_DOWN } else { SENSOR_PTT_UP };
        Self::Sensor {
            name: name.to_string(),
            value: SensorValue::Bool(pressed),
        }
    }

    /// The node this event is about, if it is an object event.
    pub fn node(&self) -> Option<NodeId> {
        match self {
            Self::Tap { node } | Self::Drag { node, .. } => Some(*node),
            Self::Sensor { .. } => None,
        }
    }
}
