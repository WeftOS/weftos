//! `weftos-sensor-pipeline-wire` — CBOR + Ed25519 framing for LeWM observation
//! topics (WEFT-523 / ADR-053 research stream).
//!
//! # Topics
//!
//! | Topic constant | Kind | Role |
//! |----------------|------|------|
//! | [`TOPIC_ENCODED`] | primary | Cultivated latent frames · ≤ 2 KB |
//! | [`TOPIC_CONSENSUS`] | conditional | Cluster latent + prediction + health |
//! | [`TOPIC_CONTROL`] | multi-pub | Drift / surprise / plan results |
//!
//! Packets are **observational-only** (no gradient proxies). Every frame is
//! signed with Ed25519; ExoChain indexing is a mesh-layer concern (WEFT-526).
//!
//! Latent width is pinned to [`weftos_worldmodel_core::LATENT_DIM`] (192).

#![deny(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::de::{self, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;
use weftos_worldmodel_core::{latent_dim_matches_v1, LATENT_DIM, LATENT_DIM_U16};

/// Byte buffer that always serializes as a CBOR/serde **byte string** (not
/// an array of integers). Critical for keeping 768-byte latents compact.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ByteString(pub Vec<u8>);

impl ByteString {
    /// Wrap raw bytes.
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    /// Borrow the inner slice.
    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }

    /// Inner length.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// True when empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl From<Vec<u8>> for ByteString {
    fn from(v: Vec<u8>) -> Self {
        Self(v)
    }
}

impl AsRef<[u8]> for ByteString {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl Serialize for ByteString {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(&self.0)
    }
}

impl<'de> Deserialize<'de> for ByteString {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ByteStringVisitor;
        impl<'de> Visitor<'de> for ByteStringVisitor {
            type Value = ByteString;
            fn expecting(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
                f.write_str("a byte string")
            }
            fn visit_bytes<E: de::Error>(self, v: &[u8]) -> Result<Self::Value, E> {
                Ok(ByteString(v.to_vec()))
            }
            fn visit_byte_buf<E: de::Error>(self, v: Vec<u8>) -> Result<Self::Value, E> {
                Ok(ByteString(v))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                // Accept integer arrays for interoperability with older frames.
                let mut out = Vec::new();
                while let Some(b) = seq.next_element::<u8>()? {
                    out.push(b);
                }
                Ok(ByteString(out))
            }
        }
        deserializer.deserialize_bytes(ByteStringVisitor)
    }
}

/// Crate version for diagnostics.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Wire schema major (CBOR envelope `version` field).
pub const WIRE_VERSION: u8 = 1;

/// Soft max encoded frame size for `mesh.sensor.v1.encoded` (diagram: ≤ 2 KB).
pub const MAX_ENCODED_FRAME_BYTES: usize = 2048;

/// Hard cap on any signed frame accepted by [`SignedSensorFrame::from_cbor`].
///
/// Checked before CBOR decode so hostile length prefixes cannot drive
/// allocation. The largest legitimate frame (consensus, two 192-wide latents)
/// is under 2 KiB; 16 KiB leaves room for control `detail` and extensions.
pub const MAX_FRAME_BYTES: usize = 16 * 1024;

/// Primary observation stream topic prefix (append `.{cluster}`).
pub const TOPIC_ENCODED: &str = "mesh.sensor.v1.encoded";
/// Conditional consensus stream (only when a world model is running).
pub const TOPIC_CONSENSUS: &str = "mesh.sensor.v1.consensus";
/// Multi-publisher control stream (drift / surprise / plan).
pub const TOPIC_CONTROL: &str = "mesh.sensor.v1.control";

/// The three observation topics in stable order.
pub const SENSOR_TOPICS: &[&str] = &[TOPIC_ENCODED, TOPIC_CONSENSUS, TOPIC_CONTROL];

/// Topic kind carried inside a signed frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SensorTopic {
    /// `mesh.sensor.v1.encoded`
    Encoded,
    /// `mesh.sensor.v1.consensus`
    Consensus,
    /// `mesh.sensor.v1.control`
    Control,
}

impl SensorTopic {
    /// Full topic string without cluster suffix.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Encoded => TOPIC_ENCODED,
            Self::Consensus => TOPIC_CONSENSUS,
            Self::Control => TOPIC_CONTROL,
        }
    }

    /// Topic with `.{cluster}` suffix for mesh routing.
    pub fn with_cluster(self, cluster: &str) -> String {
        format!("{}.{}", self.as_str(), cluster)
    }

    /// Parse from bare topic string (with or without cluster suffix).
    pub fn parse(s: &str) -> Option<Self> {
        let base = s.split('.').take(4).collect::<Vec<_>>().join(".");
        // mesh.sensor.v1.encoded[.cluster...]
        if s.starts_with(TOPIC_ENCODED) || base == TOPIC_ENCODED {
            Some(Self::Encoded)
        } else if s.starts_with(TOPIC_CONSENSUS) || base == TOPIC_CONSENSUS {
            Some(Self::Consensus)
        } else if s.starts_with(TOPIC_CONTROL) || base == TOPIC_CONTROL {
            Some(Self::Control)
        } else {
            None
        }
    }
}

// ── Topic payloads ─────────────────────────────────────────────────────────

/// Packed latent size in bytes (`LATENT_DIM × 4` little-endian f32).
pub const LATENT_BYTES: usize = LATENT_DIM * 4;

/// Pack a latent into little-endian bytes (compact wire form; keeps frames ≤ 2 KB).
pub fn pack_latent(z: &[f32]) -> Result<Vec<u8>, WireError> {
    if z.len() != LATENT_DIM {
        return Err(WireError::LatentDimMismatch {
            expected: LATENT_DIM_U16,
            got: z.len() as u16,
        });
    }
    let mut out = Vec::with_capacity(LATENT_BYTES);
    for &v in z {
        out.extend_from_slice(&v.to_le_bytes());
    }
    Ok(out)
}

/// Unpack little-endian latent bytes into `Vec<f32>`.
pub fn unpack_latent(bytes: &[u8]) -> Result<Vec<f32>, WireError> {
    if bytes.len() != LATENT_BYTES {
        return Err(WireError::LatentDimMismatch {
            expected: LATENT_DIM_U16,
            got: (bytes.len() / 4) as u16,
        });
    }
    let mut z = Vec::with_capacity(LATENT_DIM);
    for chunk in bytes.chunks_exact(4) {
        z.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
    }
    Ok(z)
}

/// Primary encoded observation payload (cultivated latent + quality).
///
/// Latent is stored as packed LE f32 bytes so CBOR frames stay under the
/// diagram soft budget of [`MAX_ENCODED_FRAME_BYTES`] (192×4 = 768 B payload).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EncodedObservation {
    /// SIGReg latent `z_t` as little-endian f32 bytes (length [`LATENT_BYTES`]).
    pub latent_le: ByteString,
    /// Declared latent width (must match local contract).
    pub latent_dim: u16,
    /// Quality flag in `[0, 1]`.
    pub quality: f32,
    /// Deterministic burst episode id (diagram: burst_episode_id).
    pub burst_episode_id: u64,
    /// Sensor class label (rgb, imu, audio, …).
    pub sensor_class: String,
    /// Optional CBOR/opaque extension fields.
    #[serde(default, skip_serializing_if = "ByteString::is_empty")]
    pub extension: ByteString,
}

impl EncodedObservation {
    /// Build a v1 observation from an f32 latent; rejects wrong length.
    pub fn v1(
        latent: Vec<f32>,
        quality: f32,
        burst_episode_id: u64,
        sensor_class: impl Into<String>,
    ) -> Result<Self, WireError> {
        Ok(Self {
            latent_le: ByteString::new(pack_latent(&latent)?),
            latent_dim: LATENT_DIM_U16,
            quality,
            burst_episode_id,
            sensor_class: sensor_class.into(),
            extension: ByteString::default(),
        })
    }

    /// Decode packed latent to `Vec<f32>`.
    pub fn latent(&self) -> Result<Vec<f32>, WireError> {
        unpack_latent(self.latent_le.as_slice())
    }

    /// Validate latent contract.
    pub fn check(&self) -> Result<(), WireError> {
        if !latent_dim_matches_v1(self.latent_dim) || self.latent_le.len() != LATENT_BYTES {
            return Err(WireError::LatentDimMismatch {
                expected: LATENT_DIM_U16,
                got: if self.latent_le.len() != LATENT_BYTES {
                    (self.latent_le.len() / 4) as u16
                } else {
                    self.latent_dim
                },
            });
        }
        Ok(())
    }
}

/// Consensus frame from a running world model (conditional stream).
///
/// Latents packed as LE f32 bytes (same as encoded).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConsensusFrame {
    /// Cluster-fused latent `z_cluster(t)` (packed LE f32).
    pub z_cluster_le: ByteString,
    /// One-step prediction `ẑ_{t+1}` (packed LE f32).
    pub z_hat_next_le: ByteString,
    /// Cluster SIGReg health in `[0, 1]`.
    pub health: f32,
    /// Declared latent width.
    pub latent_dim: u16,
}

impl ConsensusFrame {
    /// Construct v1 consensus; both latents must be 192-wide.
    pub fn v1(z_cluster: Vec<f32>, z_hat_next: Vec<f32>, health: f32) -> Result<Self, WireError> {
        Ok(Self {
            z_cluster_le: ByteString::new(pack_latent(&z_cluster)?),
            z_hat_next_le: ByteString::new(pack_latent(&z_hat_next)?),
            health,
            latent_dim: LATENT_DIM_U16,
        })
    }

    /// Unpack cluster latent.
    pub fn z_cluster(&self) -> Result<Vec<f32>, WireError> {
        unpack_latent(self.z_cluster_le.as_slice())
    }

    /// Unpack prediction latent.
    pub fn z_hat_next(&self) -> Result<Vec<f32>, WireError> {
        unpack_latent(self.z_hat_next_le.as_slice())
    }
}

/// Control-plane event kinds (multi-publisher).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ControlKind {
    /// Temporal / SIGReg drift signal.
    Drift,
    /// VoE surprise residual.
    Surprise,
    /// Latent planner result summary.
    PlanResult,
}

/// Control stream payload (drift / surprise / plan).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ControlFrame {
    /// Event kind.
    pub kind: ControlKind,
    /// Scalar magnitude (distance, residual, or score).
    pub magnitude: f32,
    /// Originating node id (0 = unspecified).
    pub node_id: u64,
    /// Optional opaque detail (CBOR or UTF-8 note).
    #[serde(default, skip_serializing_if = "ByteString::is_empty")]
    pub detail: ByteString,
}

// ── Signed envelope ────────────────────────────────────────────────────────

/// Signed observational frame on any of the three topics.
///
/// Signature covers the canonical CBOR of [`SignableBody`] (everything except
/// `pubkey` / `signature`). Packets are observational-only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SignedSensorFrame {
    /// Wire schema major.
    pub version: u8,
    /// Topic kind.
    pub topic: SensorTopic,
    /// Logical cluster name (mesh routing suffix).
    pub cluster: String,
    /// Observation / emission timestamp (ms).
    pub timestamp_ms: u64,
    /// Emitting node id.
    pub node_id: u64,
    /// CBOR-encoded topic payload ([`EncodedObservation`], [`ConsensusFrame`],
    /// or [`ControlFrame`]).
    pub payload: ByteString,
    /// Ed25519 public key of the signer (exactly 32 bytes).
    pub pubkey: ByteString,
    /// Ed25519 signature over the signable body (exactly 64 bytes).
    pub signature: ByteString,
}

/// Canonical body hashed/signed (excludes pubkey + signature).
#[derive(Debug, Clone, Serialize)]
struct SignableBody<'a> {
    version: u8,
    topic: SensorTopic,
    cluster: &'a str,
    timestamp_ms: u64,
    node_id: u64,
    #[serde(serialize_with = "serialize_bytes_slice")]
    payload: &'a [u8],
}

fn serialize_bytes_slice<S: Serializer>(bytes: &&[u8], serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_bytes(bytes)
}

/// Wire-layer errors.
#[derive(Debug, Error, PartialEq)]
pub enum WireError {
    /// CBOR encode failed.
    #[error("cbor encode failed: {0}")]
    Encode(String),
    /// CBOR decode failed.
    #[error("cbor decode failed: {0}")]
    Decode(String),
    /// Ed25519 verification failed.
    #[error("signature verification failed")]
    BadSignature,
    /// Public key bytes rejected by dalek.
    #[error("invalid public key")]
    BadPublicKey,
    /// Frame `version` ≠ [`WIRE_VERSION`].
    #[error("wire version mismatch: found {found}, expected {expected}")]
    VersionMismatch {
        /// Observed version.
        found: u8,
        /// Expected version.
        expected: u8,
    },
    /// Latent width ≠ 192.
    #[error("latent dim mismatch: expected {expected}, got {got}")]
    LatentDimMismatch {
        /// Expected width.
        expected: u16,
        /// Got width.
        got: u16,
    },
    /// Encoded frame exceeds soft size budget.
    #[error("frame too large: {size} > {limit}")]
    FrameTooLarge {
        /// Encoded size.
        size: usize,
        /// Soft limit.
        limit: usize,
    },
    /// Topic payload kind does not match frame topic.
    #[error("payload topic mismatch")]
    TopicMismatch,
}

/// Encode any serde value to CBOR.
pub fn encode_cbor<T: Serialize>(value: &T) -> Result<Vec<u8>, WireError> {
    let mut out = Vec::new();
    ciborium::into_writer(value, &mut out).map_err(|e| WireError::Encode(e.to_string()))?;
    Ok(out)
}

/// Decode a serde value from CBOR.
pub fn decode_cbor<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T, WireError> {
    ciborium::from_reader(bytes).map_err(|e| WireError::Decode(e.to_string()))
}

impl SignedSensorFrame {
    /// Canonical signable bytes for this frame (without pubkey/signature).
    pub fn signable_bytes(&self) -> Result<Vec<u8>, WireError> {
        let body = SignableBody {
            version: self.version,
            topic: self.topic,
            cluster: &self.cluster,
            timestamp_ms: self.timestamp_ms,
            node_id: self.node_id,
            payload: self.payload.as_slice(),
        };
        encode_cbor(&body)
    }

    /// Build and sign a frame for the given topic payload bytes.
    pub fn sign(
        topic: SensorTopic,
        cluster: impl Into<String>,
        timestamp_ms: u64,
        node_id: u64,
        payload: Vec<u8>,
        key: &SigningKey,
    ) -> Result<Self, WireError> {
        let mut frame = Self {
            version: WIRE_VERSION,
            topic,
            cluster: cluster.into(),
            timestamp_ms,
            node_id,
            payload: ByteString::new(payload),
            pubkey: ByteString::new(key.verifying_key().to_bytes().to_vec()),
            signature: ByteString::new(vec![0u8; 64]),
        };
        let msg = frame.signable_bytes()?;
        let sig = key.sign(&msg);
        frame.signature = ByteString::new(sig.to_bytes().to_vec());
        Ok(frame)
    }

    /// Sign an [`EncodedObservation`] (primary stream).
    pub fn sign_encoded(
        obs: &EncodedObservation,
        cluster: impl Into<String>,
        timestamp_ms: u64,
        node_id: u64,
        key: &SigningKey,
    ) -> Result<Self, WireError> {
        obs.check()?;
        let payload = encode_cbor(obs)?;
        Self::sign(
            SensorTopic::Encoded,
            cluster,
            timestamp_ms,
            node_id,
            payload,
            key,
        )
    }

    /// Sign a [`ConsensusFrame`].
    pub fn sign_consensus(
        frame: &ConsensusFrame,
        cluster: impl Into<String>,
        timestamp_ms: u64,
        node_id: u64,
        key: &SigningKey,
    ) -> Result<Self, WireError> {
        let payload = encode_cbor(frame)?;
        Self::sign(
            SensorTopic::Consensus,
            cluster,
            timestamp_ms,
            node_id,
            payload,
            key,
        )
    }

    /// Sign a [`ControlFrame`].
    pub fn sign_control(
        frame: &ControlFrame,
        cluster: impl Into<String>,
        timestamp_ms: u64,
        node_id: u64,
        key: &SigningKey,
    ) -> Result<Self, WireError> {
        let payload = encode_cbor(frame)?;
        Self::sign(
            SensorTopic::Control,
            cluster,
            timestamp_ms,
            node_id,
            payload,
            key,
        )
    }

    /// Verify Ed25519 signature + wire version.
    pub fn verify(&self) -> Result<(), WireError> {
        if self.version != WIRE_VERSION {
            return Err(WireError::VersionMismatch {
                found: self.version,
                expected: WIRE_VERSION,
            });
        }
        if self.pubkey.len() != 32 {
            return Err(WireError::BadPublicKey);
        }
        if self.signature.len() != 64 {
            return Err(WireError::BadSignature);
        }
        let mut pk = [0u8; 32];
        pk.copy_from_slice(self.pubkey.as_slice());
        let mut sig_bytes = [0u8; 64];
        sig_bytes.copy_from_slice(self.signature.as_slice());
        let vk = VerifyingKey::from_bytes(&pk).map_err(|_| WireError::BadPublicKey)?;
        let msg = self.signable_bytes()?;
        let sig = Signature::from_bytes(&sig_bytes);
        // `verify_strict` also rejects small-order public keys / nonces, so a
        // weak key cannot forge a signature over an arbitrary payload.
        vk.verify_strict(&msg, &sig)
            .map_err(|_| WireError::BadSignature)?;
        Ok(())
    }

    /// Full CBOR encode of this signed frame (for mesh publish).
    pub fn to_cbor(&self) -> Result<Vec<u8>, WireError> {
        encode_cbor(self)
    }

    /// Decode + verify a signed frame from CBOR bytes.
    ///
    /// # Errors
    ///
    /// [`WireError::FrameTooLarge`] when `bytes` exceeds [`MAX_FRAME_BYTES`]
    /// (checked before any decoding); otherwise decode and verification errors.
    pub fn from_cbor(bytes: &[u8]) -> Result<Self, WireError> {
        if bytes.len() > MAX_FRAME_BYTES {
            return Err(WireError::FrameTooLarge {
                size: bytes.len(),
                limit: MAX_FRAME_BYTES,
            });
        }
        let frame: Self = decode_cbor(bytes)?;
        frame.verify()?;
        Ok(frame)
    }

    /// Soft size check for encoded topic frames.
    pub fn check_size_budget(&self) -> Result<(), WireError> {
        if self.topic == SensorTopic::Encoded {
            let size = self.to_cbor()?.len();
            if size > MAX_ENCODED_FRAME_BYTES {
                return Err(WireError::FrameTooLarge {
                    size,
                    limit: MAX_ENCODED_FRAME_BYTES,
                });
            }
        }
        Ok(())
    }

    /// Decode payload as [`EncodedObservation`] (topic must be Encoded).
    pub fn decode_encoded(&self) -> Result<EncodedObservation, WireError> {
        if self.topic != SensorTopic::Encoded {
            return Err(WireError::TopicMismatch);
        }
        let obs: EncodedObservation = decode_cbor(self.payload.as_slice())?;
        obs.check()?;
        Ok(obs)
    }

    /// Decode payload as [`ConsensusFrame`].
    pub fn decode_consensus(&self) -> Result<ConsensusFrame, WireError> {
        if self.topic != SensorTopic::Consensus {
            return Err(WireError::TopicMismatch);
        }
        decode_cbor(self.payload.as_slice())
    }

    /// Decode payload as [`ControlFrame`].
    pub fn decode_control(&self) -> Result<ControlFrame, WireError> {
        if self.topic != SensorTopic::Control {
            return Err(WireError::TopicMismatch);
        }
        decode_cbor(self.payload.as_slice())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    fn key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    fn zero_latent_vec() -> Vec<f32> {
        vec![0.0; LATENT_DIM]
    }

    fn unit_latent(i: usize) -> Vec<f32> {
        let mut z = zero_latent_vec();
        z[i] = 1.0;
        z
    }

    #[test]
    fn topic_constants_stable() {
        assert_eq!(SENSOR_TOPICS.len(), 3);
        assert_eq!(SensorTopic::Encoded.as_str(), "mesh.sensor.v1.encoded");
        assert_eq!(
            SensorTopic::Consensus.with_cluster("alpha"),
            "mesh.sensor.v1.consensus.alpha"
        );
        assert_eq!(
            SensorTopic::parse("mesh.sensor.v1.control.east"),
            Some(SensorTopic::Control)
        );
    }

    #[test]
    fn encoded_roundtrip() {
        let k = key(1);
        let obs = EncodedObservation::v1(unit_latent(3), 0.95, 42, "rgb")
            .expect("obs");
        let frame = SignedSensorFrame::sign_encoded(&obs, "alpha", 1_000, 7, &k).expect("sign");
        frame.verify().expect("verify");
        frame.check_size_budget().expect("size");

        let cbor = frame.to_cbor().expect("cbor");
        let back = SignedSensorFrame::from_cbor(&cbor).expect("from_cbor");
        let obs2 = back.decode_encoded().expect("decode");
        assert_eq!(obs2.burst_episode_id, 42);
        assert_eq!(obs2.latent_dim, LATENT_DIM_U16);
        assert_eq!(obs2.latent().unwrap()[3], 1.0);
        assert!(cbor.len() <= MAX_ENCODED_FRAME_BYTES);
        assert_eq!(back.topic, SensorTopic::Encoded);
    }

    #[test]
    fn consensus_roundtrip() {
        let k = key(2);
        let body = ConsensusFrame::v1(unit_latent(0), unit_latent(1), 0.91).expect("body");
        let frame =
            SignedSensorFrame::sign_consensus(&body, "alpha", 2_000, 1, &k).expect("sign");
        let cbor = frame.to_cbor().expect("cbor");
        let back = SignedSensorFrame::from_cbor(&cbor).expect("verify");
        let body2 = back.decode_consensus().expect("decode");
        assert!((body2.health - 0.91).abs() < f32::EPSILON);
        assert_eq!(body2.z_cluster().unwrap()[0], 1.0);
        assert_eq!(back.topic, SensorTopic::Consensus);
    }

    #[test]
    fn control_roundtrip_all_kinds() {
        let k = key(3);
        for kind in [ControlKind::Drift, ControlKind::Surprise, ControlKind::PlanResult] {
            let body = ControlFrame {
                kind,
                magnitude: 0.33,
                node_id: 9,
                detail: ByteString::new(b"note".to_vec()),
            };
            let frame =
                SignedSensorFrame::sign_control(&body, "beta", 3_000, 9, &k).expect("sign");
            let cbor = frame.to_cbor().expect("cbor");
            let back = SignedSensorFrame::from_cbor(&cbor).expect("verify");
            let body2 = back.decode_control().expect("decode");
            assert_eq!(body2.kind, kind);
            assert_eq!(back.topic, SensorTopic::Control);
        }
    }

    #[test]
    fn bad_signature_rejected() {
        let k = key(4);
        let obs = EncodedObservation::v1(zero_latent_vec(), 1.0, 0, "imu").unwrap();
        let mut frame = SignedSensorFrame::sign_encoded(&obs, "c", 0, 0, &k).unwrap();
        frame.signature.0[0] ^= 0xff;
        assert!(matches!(frame.verify(), Err(WireError::BadSignature)));
    }

    #[test]
    fn wrong_latent_dim_rejected() {
        let err = EncodedObservation::v1(vec![0.0; 64], 1.0, 0, "x").unwrap_err();
        assert!(matches!(
            err,
            WireError::LatentDimMismatch {
                expected: 192,
                got: 64
            }
        ));
    }

    #[test]
    fn topic_mismatch_on_decode() {
        let k = key(5);
        let ctrl = ControlFrame {
            kind: ControlKind::Drift,
            magnitude: 0.1,
            node_id: 0,
            detail: ByteString::default(),
        };
        let frame = SignedSensorFrame::sign_control(&ctrl, "c", 0, 0, &k).unwrap();
        assert_eq!(frame.decode_encoded(), Err(WireError::TopicMismatch));
    }

    #[test]
    fn three_topics_end_to_end() {
        let k = key(6);
        let frames = [
            SignedSensorFrame::sign_encoded(
                &EncodedObservation::v1(zero_latent_vec(), 1.0, 1, "rgb").unwrap(),
                "mesh",
                1,
                1,
                &k,
            )
            .unwrap(),
            SignedSensorFrame::sign_consensus(
                &ConsensusFrame::v1(zero_latent_vec(), zero_latent_vec(), 1.0).unwrap(),
                "mesh",
                2,
                1,
                &k,
            )
            .unwrap(),
            SignedSensorFrame::sign_control(
                &ControlFrame {
                    kind: ControlKind::Surprise,
                    magnitude: 0.5,
                    node_id: 1,
                    detail: ByteString::default(),
                },
                "mesh",
                3,
                1,
                &k,
            )
            .unwrap(),
        ];
        for f in &frames {
            let bytes = f.to_cbor().unwrap();
            let back = SignedSensorFrame::from_cbor(&bytes).unwrap();
            assert_eq!(back.topic, f.topic);
        }
        assert_eq!(frames[0].topic.as_str(), TOPIC_ENCODED);
        assert_eq!(frames[1].topic.as_str(), TOPIC_CONSENSUS);
        assert_eq!(frames[2].topic.as_str(), TOPIC_CONTROL);
    }

    // ── RT-07-4 hardening ───────────────────────────────────────────────

    /// Frame whose pubkey is the identity point and signature is (identity, 0).
    /// Cofactorless `verify` accepts this for ANY message; `verify_strict`
    /// rejects small-order keys.
    fn weak_key_forgery(payload: Vec<u8>) -> SignedSensorFrame {
        let mut identity = vec![0u8; 32];
        identity[0] = 1;
        let mut sig = identity.clone();
        sig.extend_from_slice(&[0u8; 32]);
        SignedSensorFrame {
            version: WIRE_VERSION,
            topic: SensorTopic::Control,
            cluster: "c".into(),
            timestamp_ms: 1,
            node_id: 1,
            payload: ByteString::new(payload),
            pubkey: ByteString::new(identity),
            signature: ByteString::new(sig),
        }
    }

    #[test]
    fn small_order_pubkey_forgery_rejected() {
        let frame = weak_key_forgery(vec![1, 2, 3]);
        assert_eq!(frame.verify(), Err(WireError::BadSignature));
        let cbor = frame.to_cbor().expect("encode");
        assert!(SignedSensorFrame::from_cbor(&cbor).is_err());
    }

    #[test]
    fn non_canonical_s_rejected() {
        // S + L: same point equation, different bytes (malleable signature).
        let k = key(7);
        let frame = SignedSensorFrame::sign(SensorTopic::Control, "c", 1, 1, vec![9], &k)
            .expect("sign");
        let mut sig = frame.signature.as_slice().to_vec();
        const L: [u8; 32] = [
            0xed, 0xd3, 0xf5, 0x5c, 0x1a, 0x63, 0x12, 0x58, 0xd6, 0x9c, 0xf7, 0xa2, 0xde, 0xf9,
            0xde, 0x14, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x10,
        ];
        let mut carry = 0u16;
        for (b, l) in sig[32..].iter_mut().zip(L.iter()) {
            let sum = u16::from(*b) + u16::from(*l) + carry;
            *b = (sum & 0xff) as u8;
            carry = sum >> 8;
        }
        let mut bad = frame.clone();
        bad.signature = ByteString::new(sig);
        assert_eq!(bad.verify(), Err(WireError::BadSignature));
    }

    #[test]
    fn oversized_frame_rejected_before_decode() {
        // Not valid CBOR at all: a size-capped reader must fail with
        // FrameTooLarge, not attempt to decode.
        let big = vec![0xffu8; MAX_FRAME_BYTES + 1];
        assert!(matches!(
            SignedSensorFrame::from_cbor(&big),
            Err(WireError::FrameTooLarge { .. })
        ));
    }

    #[test]
    fn oversized_but_validly_signed_frame_rejected() {
        let k = key(3);
        let frame = SignedSensorFrame::sign(
            SensorTopic::Control,
            "c",
            1,
            1,
            vec![0u8; MAX_FRAME_BYTES],
            &k,
        )
        .expect("sign");
        let cbor = frame.to_cbor().expect("encode");
        assert!(matches!(
            SignedSensorFrame::from_cbor(&cbor),
            Err(WireError::FrameTooLarge { .. })
        ));
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    fn golden_frame() -> SignedSensorFrame {
        let obs = EncodedObservation::v1(unit_latent(5), 0.5, 42, "imu").expect("obs");
        SignedSensorFrame::sign_encoded(&obs, "lab", 1_700_000_000_000, 7, &key(1)).expect("sign")
    }

    /// Pins the signed bytes of a fixed frame (seed key `[1; 32]`). Ed25519 is
    /// deterministic, so the signature fixes the whole signable body; a change
    /// to field order, CBOR encoding or the signing domain breaks this test.
    #[test]
    fn golden_vector_encoded_frame() {
        const PUBKEY: &str = "8a88e3dd7409f195fd52db2d3cba5d72ca6709bf1d94121bf3748801b40f6f5c";
        const SIGNATURE: &str = "d337be1e186bd3baf5e521293cf0120dec94587e10cf68aca6ea452f12712e0c\
                                 91d45c66c5c69e9db1eec2e1e80e891994d7202610b98a985e891c695291d500";
        const SIGNABLE_PREFIX: &str = "a66776657273696f6e0165746f70696367656e636f64656467636c7573746572\
                                       636c61626c74696d657374616d705f6d731b0000018bcfe56800676e6f64655f\
                                       696407677061796c6f616459034aa5696c6174656e745f6c655903";
        const SIGNABLE_TAIL: &str = "6a6c6174656e745f64696d18c0677175616c697479f93800\
                                     7062757273745f657069736f64655f6964182a6c73656e736f725f636c61737363696d75";
        let f = golden_frame();
        let signable = f.signable_bytes().expect("signable");
        let signable_hex = hex(&signable);
        assert_eq!(signable.len(), 920);
        assert!(signable_hex.starts_with(SIGNABLE_PREFIX), "{signable_hex}");
        assert!(signable_hex.ends_with(SIGNABLE_TAIL), "{signable_hex}");
        assert_eq!(hex(f.pubkey.as_slice()), PUBKEY);
        assert_eq!(hex(f.signature.as_slice()), SIGNATURE);
        let cbor = f.to_cbor().expect("cbor");
        assert_eq!(cbor.len(), 1037);
        // The frame decodes and verifies strictly.
        SignedSensorFrame::from_cbor(&cbor).expect("golden frame verifies");
    }
}
