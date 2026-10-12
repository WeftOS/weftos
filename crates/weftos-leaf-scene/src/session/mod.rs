//! Display sessions over the certified-leaf channel (ADR-117 D1/D2).
//!
//! One session is one app's hold on one display. The frames below ride
//! as CBOR inside the leaf channel's JSON carrier, in both directions:
//!
//! | Direction | Topic | Carrier |
//! |---|---|---|
//! | host -> leaf | `mesh.leaf.<id>.push` | `{"type":"display_session","cbor_b64":"..","target_pubkey":".."}` |
//! | leaf -> host | `mesh.leaf.<id>.input` | the same JSON, inside the signed `ipc.publish` envelope |
//!
//! The same module carries the leaf-side state machine
//! ([`DisplayService`]) so the pad firmware and the host-side leaf
//! emulator run identical session logic. See
//! `docs/design/display-session-transport.md`.

pub mod b64;
pub mod envelope;
pub mod service;

use alloc::string::String;
use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use crate::codec::{self, CodecError};
use crate::envelope::{InputEnvelope, SceneEnvelope};
use crate::id::DisplayId;

pub use service::{Action, DisplayService, ServiceConfig, SYSTEM_ID_BASE};

/// `type` member of the carrier JSON.
pub const WIRE_TYPE: &str = "display_session";

/// Largest CBOR body one host -> leaf frame may carry. The pad reads
/// frames into an 8 KiB buffer and base64 plus the JSON wrapper cost
/// about a third more, so 4.5 KiB of CBOR keeps every push inside it.
pub const MAX_PUSH_CBOR: usize = 4608;

/// Leaf-assigned session number; 0 is never a live session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SessionId(pub u32);

/// What the leaf reports about its display when it opens a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeafInfo {
    pub width: u16,
    pub height: u16,
    /// `weftos_leaf_renderer::CapabilityMask` bits.
    pub render_caps: u32,
    pub touch: bool,
    /// The push-to-talk button exists and reports as `ptt.down` / `ptt.up`.
    pub ptt: bool,
}

/// The frames of a display session. Host -> leaf: `Open`, `Ops`,
/// `Close`. Leaf -> host: the rest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SessionFrame {
    /// Host asks for the screen. The leaf answers `Opened` or `Refused`.
    Open { subject: String, display: DisplayId, proof: Vec<u8> },
    Opened {
        session: SessionId,
        info: LeafInfo,
        max_nodes: u32,
        max_ops_per_sec: u32,
        image_budget_bytes: u32,
    },
    Refused { reason: String },
    /// Scene ops for one session. `seq` lets the leaf drop replays; a
    /// frame with `more` set is part of a batch the leaf renders only
    /// once the last fragment (`more == false`) has been applied.
    Ops { session: SessionId, seq: u32, more: bool, envelope: SceneEnvelope },
    /// Host releases the screen; the leaf removes the session's nodes.
    Close { session: SessionId },
    /// Leaf -> host: a hit-tested pointer event.
    Input { session: SessionId, envelope: InputEnvelope },
    /// Leaf -> host: a sensor event (`ptt.down`, `ptt.up`, ...).
    Sensor { session: SessionId, name: String, pressed: Option<bool>, value: Option<f64> },
    /// Leaf -> host: a batch was refused whole; the session continues.
    Rejected { session: SessionId, seq: u32, reason: String },
    /// Leaf -> host: the session is over (revoked, idle, rebooted).
    Ended { session: SessionId, reason: String },
}

impl SessionFrame {
    pub fn encode(&self) -> Result<Vec<u8>, CodecError> {
        codec::encode(self)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        codec::decode(bytes)
    }

    /// The carrier JSON for this frame. `target` is the leaf id (hex).
    pub fn to_carrier(&self, target: &str) -> Result<String, CodecError> {
        let cbor = self.encode()?;
        let mut out = String::with_capacity(cbor.len() * 4 / 3 + 96);
        out.push_str("{\"type\":\"");
        out.push_str(WIRE_TYPE);
        out.push_str("\",\"cbor_b64\":\"");
        out.push_str(&b64::encode(&cbor));
        out.push_str("\",\"target_pubkey\":\"");
        out.push_str(target);
        out.push_str("\"}");
        Ok(out)
    }

    /// Decode a frame from the carrier JSON text (any whitespace). Needs
    /// `"type":"display_session"` and a `cbor_b64` member; anything else
    /// is `None`, so a plain `leaf_push` or `leaf_input` falls through.
    pub fn from_carrier(text: &[u8]) -> Option<Result<Self, CodecError>> {
        if !contains(text, WIRE_TYPE.as_bytes()) {
            return None;
        }
        let cbor = b64::extract_member(text, b"cbor_b64")?;
        Some(Self::decode(&cbor))
    }
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}
