//! Length-prefix framing for mesh wire protocol (K6.1).
//!
//! Each mesh frame is: `[4-byte big-endian length][1-byte message type][payload]`
//! Maximum frame size is [`MAX_MESSAGE_SIZE`] (16 MiB).
//!
//! ## Frame type vs IPC encoding (ADR-031 / WEFT-683)
//!
//! `FrameType` is the **message kind** discriminator. It is orthogonal to
//! how an `IpcMessage` payload is serialized — that is
//! [`crate::mesh_ipc::MeshIpcEncoding`] (JSON shipped; RVF deferred behind
//! `mesh-rvf`). See ADR-031 for the clarified layering.

use serde::{Deserialize, Serialize};

use crate::mesh::{MAX_MESSAGE_SIZE, MeshError, MeshStream};

/// Mesh message types for framing dispatch.
///
/// Type bytes identify *what* the payload is, not *how* it is encoded.
/// IPC payload encoding is [`crate::mesh_ipc::MeshIpcEncoding`].
///
/// # Wire `msg_type` (WEFT-115)
///
/// This enum **is** the exhaustive wire `msg_type` set. Prefer the
/// [`MsgType`] alias in documentation and external protocol maps.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
#[serde(rename_all = "snake_case")]
pub enum FrameType {
    /// WeftOS handshake payload.
    Handshake = 0x01,
    /// KernelMessage IPC envelope (payload encoding: see `mesh_ipc`).
    IpcMessage = 0x02,
    /// Chain sync request/response.
    ChainSync = 0x03,
    /// Tree sync request/response.
    TreeSync = 0x04,
    /// Service advertisement.
    ServiceAdvert = 0x05,
    /// Process advertisement.
    ProcessAdvert = 0x06,
    /// Heartbeat ping/pong.
    Heartbeat = 0x07,
    /// Join request.
    JoinRequest = 0x08,
    /// Join response.
    JoinResponse = 0x09,
    /// Sync state digest.
    SyncDigest = 0x0A,
    /// Artifact control (mesh-placement-11): `meta_request`, `announce`
    /// (have bitfield), `request` (piece indexes). See `mesh_artifact_wire`.
    ArtifactRequest = 0x0B,
    /// Artifact data (mesh-placement-11): `meta` (piece list), `piece`
    /// blocks, `no_piece`, `reject`. See `mesh_artifact_wire`.
    ArtifactResponse = 0x0C,
    /// Log aggregation (K6-G2).
    LogAggregation = 0x0D,
    /// Assessment sync (K6.6 -- cross-project assessment mesh).
    AssessmentSync = 0x0E,
    /// LeWM sensor observation frame (WEFT-526): CBOR `mesh.sensor.v1.*`
    /// signed payload (encoded / consensus / control). Topic lives in the
    /// IPC envelope or publisher metadata; payload is observational-only.
    SensorObservation = 0x0F,
    /// Inference request (mesh-placement-19, ADR-101 section 5): one
    /// OpenAI-compatible HTTP request for a role this node serves. See
    /// `infer_proxy::wire`. Served only to admitted, verified peers.
    InferRequest = 0x10,
    /// Inference response (mesh-placement-19): `head`, body `chunk`s,
    /// `end` or `error`. See `infer_proxy::wire`.
    InferResponse = 0x11,
    /// cogbus mesh topic request (ADR-122 section 14): subscribe, credit,
    /// unsubscribe, blob fetch, subscriber node to source node. Sent raw on
    /// the peer connection; handled only from admitted, verified peers.
    TopicRequest = 0x12,
    /// cogbus mesh topic data (ADR-122 section 14): adverts, readings,
    /// binary stream chunks and blob slices, end and error.
    TopicData = 0x13,
}

/// Wire protocol name for [`FrameType`] (WEFT-115).
pub type MsgType = FrameType;

/// Historical / protocol-doc name for a framed message (WEFT-115).
pub type Frame = MeshFrame;

impl FrameType {
    /// All known frame types in discriminant order (exhaustive for tests).
    pub const ALL: &'static [FrameType] = &[
        Self::Handshake,
        Self::IpcMessage,
        Self::ChainSync,
        Self::TreeSync,
        Self::ServiceAdvert,
        Self::ProcessAdvert,
        Self::Heartbeat,
        Self::JoinRequest,
        Self::JoinResponse,
        Self::SyncDigest,
        Self::ArtifactRequest,
        Self::ArtifactResponse,
        Self::LogAggregation,
        Self::AssessmentSync,
        Self::SensorObservation,
        Self::InferRequest,
        Self::InferResponse,
        Self::TopicRequest,
        Self::TopicData,
    ];

    /// Parse a byte into a known frame type, returning `None` for
    /// unrecognised discriminants.
    pub fn from_byte(b: u8) -> Option<Self> {
        match b {
            0x01 => Some(Self::Handshake),
            0x02 => Some(Self::IpcMessage),
            0x03 => Some(Self::ChainSync),
            0x04 => Some(Self::TreeSync),
            0x05 => Some(Self::ServiceAdvert),
            0x06 => Some(Self::ProcessAdvert),
            0x07 => Some(Self::Heartbeat),
            0x08 => Some(Self::JoinRequest),
            0x09 => Some(Self::JoinResponse),
            0x0A => Some(Self::SyncDigest),
            0x0B => Some(Self::ArtifactRequest),
            0x0C => Some(Self::ArtifactResponse),
            0x0D => Some(Self::LogAggregation),
            0x0E => Some(Self::AssessmentSync),
            0x0F => Some(Self::SensorObservation),
            0x10 => Some(Self::InferRequest),
            0x11 => Some(Self::InferResponse),
            0x12 => Some(Self::TopicRequest),
            0x13 => Some(Self::TopicData),
            _ => None,
        }
    }

    /// Wire discriminant byte.
    pub const fn as_byte(self) -> u8 {
        self as u8
    }
}

/// A framed mesh message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeshFrame {
    /// Discriminant identifying the payload contents.
    pub frame_type: FrameType,
    /// Raw payload bytes (deserialized by the receiver).
    pub payload: Vec<u8>,
}

impl MeshFrame {
    /// Encode frame to wire format: `[4-byte len][1-byte type][payload]`.
    ///
    /// The 4-byte length covers the type byte **plus** the payload,
    /// so `len = 1 + payload.len()`.
    pub fn encode(&self) -> Result<Vec<u8>, MeshError> {
        let payload_len = self.payload.len();
        let total = 1 + payload_len; // type byte + payload
        if total > MAX_MESSAGE_SIZE {
            return Err(MeshError::MessageTooLarge {
                size: total,
                max: MAX_MESSAGE_SIZE,
            });
        }
        let mut buf = Vec::with_capacity(4 + total);
        buf.extend_from_slice(&(total as u32).to_be_bytes());
        buf.push(self.frame_type as u8);
        buf.extend_from_slice(&self.payload);
        Ok(buf)
    }

    /// Decode frame from wire bytes (after the 4-byte length prefix
    /// has already been consumed).
    pub fn decode(data: &[u8]) -> Result<Self, MeshError> {
        if data.is_empty() {
            return Err(MeshError::Transport("empty frame".into()));
        }
        let frame_type = FrameType::from_byte(data[0]).ok_or_else(|| {
            MeshError::Transport(format!("unknown frame type: 0x{:02x}", data[0]))
        })?;
        let payload = data[1..].to_vec();
        Ok(Self {
            frame_type,
            payload,
        })
    }
}

/// Read a single framed message from a mesh stream.
///
/// The transport delivers one message per `recv`, carrying exactly what
/// [`write_frame`] sent: `[4-byte len][type][payload]`. The declared length
/// must match the message and stay under the mesh cap.
pub async fn read_frame(stream: &mut dyn MeshStream) -> Result<MeshFrame, MeshError> {
    let data = stream.recv().await?;
    if data.len() > MAX_MESSAGE_SIZE + 4 {
        return Err(MeshError::MessageTooLarge {
            size: data.len(),
            max: MAX_MESSAGE_SIZE,
        });
    }
    if data.len() < 5 {
        return Err(MeshError::Transport("short frame".into()));
    }
    let declared = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
    if declared > MAX_MESSAGE_SIZE {
        return Err(MeshError::MessageTooLarge {
            size: declared,
            max: MAX_MESSAGE_SIZE,
        });
    }
    if declared != data.len() - 4 {
        return Err(MeshError::Transport("frame length mismatch".into()));
    }
    MeshFrame::decode(&data[4..])
}

/// Write a single framed message to a mesh stream.
pub async fn write_frame(stream: &mut dyn MeshStream, frame: &MeshFrame) -> Result<(), MeshError> {
    let encoded = frame.encode()?;
    stream.send(&encoded).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_type_from_byte_all_variants() {
        let expected = [
            (0x01, FrameType::Handshake),
            (0x02, FrameType::IpcMessage),
            (0x03, FrameType::ChainSync),
            (0x04, FrameType::TreeSync),
            (0x05, FrameType::ServiceAdvert),
            (0x06, FrameType::ProcessAdvert),
            (0x07, FrameType::Heartbeat),
            (0x08, FrameType::JoinRequest),
            (0x09, FrameType::JoinResponse),
            (0x0A, FrameType::SyncDigest),
            (0x0B, FrameType::ArtifactRequest),
            (0x0C, FrameType::ArtifactResponse),
            (0x0D, FrameType::LogAggregation),
            (0x0E, FrameType::AssessmentSync),
            (0x0F, FrameType::SensorObservation),
            (0x10, FrameType::InferRequest),
            (0x11, FrameType::InferResponse),
            (0x12, FrameType::TopicRequest),
            (0x13, FrameType::TopicData),
        ];
        for (byte, variant) in expected {
            assert_eq!(FrameType::from_byte(byte), Some(variant));
        }
    }

    #[tokio::test]
    async fn write_then_read_frame_round_trips_and_checks_the_length() {
        use crate::mesh_test_support::connected_pair;
        let (mut a, mut b) = connected_pair().await.unwrap();
        let f = MeshFrame {
            frame_type: FrameType::InferRequest,
            payload: vec![9, 8, 7],
        };
        write_frame(&mut a, &f).await.unwrap();
        let g = read_frame(&mut b).await.unwrap();
        assert_eq!(g.frame_type, FrameType::InferRequest);
        assert_eq!(g.payload, vec![9, 8, 7]);
        // Wrong declared length, short message, and over-cap length are refused.
        let mut bad = f.encode().unwrap();
        bad[3] = bad[3].wrapping_add(1);
        a.send(&bad).await.unwrap();
        assert!(read_frame(&mut b).await.is_err());
        a.send(&[0, 0, 0]).await.unwrap();
        assert!(read_frame(&mut b).await.is_err());
        let mut huge = vec![0u8; 8];
        huge[..4].copy_from_slice(&(MAX_MESSAGE_SIZE as u32 + 1).to_be_bytes());
        a.send(&huge).await.unwrap();
        assert!(matches!(
            read_frame(&mut b).await,
            Err(MeshError::MessageTooLarge { .. })
        ));
    }

    #[test]
    fn frame_type_from_byte_unknown() {
        assert!(FrameType::from_byte(0x00).is_none());
        assert!(FrameType::from_byte(0x14).is_none());
        assert!(FrameType::from_byte(0xFF).is_none());
    }

    #[test]
    fn encode_decode_roundtrip() {
        let frame = MeshFrame {
            frame_type: FrameType::Heartbeat,
            payload: vec![1, 2, 3, 4],
        };
        let encoded = frame.encode().unwrap();

        // First 4 bytes are the length prefix: 1 (type) + 4 (payload) = 5
        let len = u32::from_be_bytes([encoded[0], encoded[1], encoded[2], encoded[3]]);
        assert_eq!(len, 5);

        // Decode the data portion (after length prefix)
        let decoded = MeshFrame::decode(&encoded[4..]).unwrap();
        assert_eq!(decoded.frame_type, FrameType::Heartbeat);
        assert_eq!(decoded.payload, vec![1, 2, 3, 4]);
    }

    #[test]
    fn encode_decode_empty_payload() {
        let frame = MeshFrame {
            frame_type: FrameType::Handshake,
            payload: vec![],
        };
        let encoded = frame.encode().unwrap();
        let decoded = MeshFrame::decode(&encoded[4..]).unwrap();
        assert_eq!(decoded.frame_type, FrameType::Handshake);
        assert!(decoded.payload.is_empty());
    }

    #[test]
    fn decode_empty_data_fails() {
        let result = MeshFrame::decode(&[]);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("empty frame"));
    }

    #[test]
    fn decode_unknown_type_fails() {
        let result = MeshFrame::decode(&[0xFF, 0x01, 0x02]);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("unknown frame type: 0xff"));
    }

    #[test]
    fn encode_oversized_frame_fails() {
        let frame = MeshFrame {
            frame_type: FrameType::IpcMessage,
            payload: vec![0u8; MAX_MESSAGE_SIZE], // payload alone = MAX, +1 for type byte
        };
        let result = frame.encode();
        assert!(result.is_err());
        match result.unwrap_err() {
            MeshError::MessageTooLarge { size, max } => {
                assert_eq!(size, MAX_MESSAGE_SIZE + 1);
                assert_eq!(max, MAX_MESSAGE_SIZE);
            }
            other => panic!("expected MessageTooLarge, got: {other:?}"),
        }
    }

    #[test]
    fn all_frame_types_encode_decode() {
        for ft in FrameType::ALL {
            let frame = MeshFrame {
                frame_type: *ft,
                payload: vec![0xCA, 0xFE],
            };
            let encoded = frame.encode().unwrap();
            let decoded = MeshFrame::decode(&encoded[4..]).unwrap();
            assert_eq!(decoded.frame_type, *ft);
            assert_eq!(decoded.payload, vec![0xCA, 0xFE]);
        }
    }

    #[test]
    fn frame_type_msg_type_alias_and_serde() {
        // MsgType is the protocol name for FrameType (WEFT-115).
        let t: MsgType = FrameType::ChainSync;
        assert_eq!(t.as_byte(), 0x03);
        let json = serde_json::to_string(&t).unwrap();
        let restored: FrameType = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, FrameType::ChainSync);
    }

    #[test]
    fn mesh_frame_serde_roundtrip() {
        let frame = MeshFrame {
            frame_type: FrameType::IpcMessage,
            payload: vec![1, 2, 3],
        };
        let json = serde_json::to_string(&frame).unwrap();
        let restored: Frame = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.frame_type, FrameType::IpcMessage);
        assert_eq!(restored.payload, vec![1, 2, 3]);
    }

    #[test]
    fn msg_type_all_variants_serde_roundtrip() {
        for ft in FrameType::ALL {
            let json = serde_json::to_string(ft).unwrap();
            let restored: MsgType = serde_json::from_str(&json).unwrap();
            assert_eq!(restored, *ft);
        }
    }
}
