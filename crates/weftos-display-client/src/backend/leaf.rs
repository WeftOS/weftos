//! Real-leaf backend over the certified-leaf channel.
//!
//! D1 ships the carrier types and a clear "not yet" at `open`. The
//! transport itself (signed `SignedPublish` frames on
//! `mesh.leaf.<pk>.push` / `.input`, session framing, the pad's
//! `display` service) is designed in
//! `docs/design/display-session-transport.md` and lands with D2, when
//! grants exist to put in the open frame.

use serde::{Deserialize, Serialize};
use weftos_leaf_scene::{DisplayId, InputEnvelope, SceneEnvelope};

use super::Backend;
use crate::error::DisplayError;
use crate::grant::DisplayGrant;
use crate::target::LeafEndpoint;

/// Leaf-assigned session number; 0 is never a live session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(pub u32);

/// The frames that ride inside signed leaf-channel publishes once the
/// carrier is built. Host → leaf on `.push`, leaf → host on `.input`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SessionFrame {
    /// Host asks for the screen. The leaf answers `Opened` or `Refused`.
    Open {
        subject: String,
        display: DisplayId,
        proof: Vec<u8>,
    },
    Opened {
        session: SessionId,
        max_nodes: u32,
        max_ops_per_sec: u32,
        image_budget_bytes: u32,
    },
    Refused {
        reason: String,
    },
    /// Scene ops for one session; `seq` lets the leaf drop replays.
    Ops {
        session: SessionId,
        seq: u32,
        envelope: SceneEnvelope,
    },
    /// Host releases the screen; the leaf clears the session's nodes.
    Close {
        session: SessionId,
    },
    /// Leaf → host: a hit-tested pointer event for this session.
    Input {
        session: SessionId,
        envelope: InputEnvelope,
    },
    /// Leaf → host: a sensor event (`ptt.down`, …).
    Sensor {
        session: SessionId,
        name: String,
        pressed: Option<bool>,
        value: Option<f64>,
    },
    /// Leaf → host: the session ended on the leaf's side (revoked,
    /// pre-empted, rebooted).
    Ended {
        session: SessionId,
        reason: String,
    },
}

/// Reason the D1 build refuses to open a real-leaf session.
pub const NOT_YET: &str = "real-leaf display sessions need the certified-leaf session carrier \
(docs/design/display-session-transport.md); use DisplayTarget::LeafSim or DisplayTarget::Window";

/// Entry point the session calls for [`crate::DisplayTarget::Leaf`].
pub(crate) fn open(_endpoint: &LeafEndpoint, _grant: &DisplayGrant) -> Result<Box<dyn Backend>, DisplayError> {
    Err(DisplayError::NotYet(NOT_YET))
}
