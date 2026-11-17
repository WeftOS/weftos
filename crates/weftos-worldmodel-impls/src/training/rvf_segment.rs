//! RVF segment encode/decode for LeWM small models (WEFT-532).
//!
//! Wire framing (aligned with ECC helper style, independent of kernel):
//!
//! ```text
//! [type:u8][codec:u8][sensor_class:u8][version_tag:u64 LE]
//! [weight_digest:u64 LE][weight_len:u32 LE][weights...]
//! ```
//!
//! `type` is [`SmallModelKind`] (`0x60` transmit-gate, `0x61` aggregate,
//! `0x62` encode). Codec `0x02` = raw weight blob (the `LWAL` action-linear
//! model), `0x03` = safetensors blob (candle ViT-tiny / AdaLN checkpoints).
//! [`codec_for_weights`] picks the codec from the payload, so a
//! [`ModelCheckpoint`] needs no codec field: a safetensors file is
//! self-identifying (8-byte LE header length, then a JSON header).

extern crate alloc;

use alloc::vec::Vec;

use weftos_worldmodel_core::{
    fnv1a64, ModelCheckpoint, SensorClass, SmallModelKind, TrainingSurfaceKind, WorldModelError,
    WorldModelResult, LEWM_MODEL_SEGMENT_CODEC_RAW, LEWM_MODEL_SEGMENT_CODEC_SAFETENSORS,
};

/// Whether `bytes` is shaped like a safetensors file: an 8-byte little-endian header length
/// `n` with `8 + n <= len`, followed by a JSON object.
pub fn is_safetensors_blob(bytes: &[u8]) -> bool {
    let (Some(head), Some(&first)) = (bytes.first_chunk::<8>(), bytes.get(8)) else {
        return false;
    };
    // `bytes.len() > 8` here, so the subtraction cannot underflow.
    let n = u64::from_le_bytes(*head);
    n >= 2 && n <= (bytes.len() - 8) as u64 && first == b'{' && bytes.len() >= 10
}

/// Codec byte for a weight payload: safetensors (`0x03`) when the blob is shaped like one,
/// raw (`0x02`) otherwise.
pub fn codec_for_weights(weights: &[u8]) -> u8 {
    if is_safetensors_blob(weights) {
        LEWM_MODEL_SEGMENT_CODEC_SAFETENSORS
    } else {
        LEWM_MODEL_SEGMENT_CODEC_RAW
    }
}

/// Fixed header length before the weight blob.
pub const SEGMENT_HEADER_LEN: usize = 1 + 1 + 1 + 8 + 8 + 4; // 23

/// Framed LeWM model segment (RVF-deliverable).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LewmModelSegment {
    /// Segment type = [`SmallModelKind`] discriminant.
    pub kind: SmallModelKind,
    /// Codec byte (`0x02` raw, `0x03` safetensors).
    pub codec: u8,
    /// Sensor class that owns this model.
    pub sensor_class: SensorClass,
    /// Model version tag.
    pub version_tag: u64,
    /// Digest of weights.
    pub weight_digest: u64,
    /// Opaque weight payload.
    pub weights: Vec<u8>,
}

impl LewmModelSegment {
    /// Build from a [`ModelCheckpoint`].
    pub fn from_checkpoint(cp: &ModelCheckpoint) -> Self {
        Self {
            kind: cp.kind,
            codec: codec_for_weights(&cp.weights),
            sensor_class: cp.sensor_class,
            version_tag: cp.version_tag,
            weight_digest: cp.weight_digest,
            weights: cp.weights.clone(),
        }
    }

    /// Convert back to a checkpoint (surface defaults to offline edge).
    pub fn into_checkpoint(
        self,
        surface: TrainingSurfaceKind,
        target_tick: Option<u64>,
    ) -> ModelCheckpoint {
        ModelCheckpoint {
            sensor_class: self.sensor_class,
            kind: self.kind,
            version_tag: self.version_tag,
            surface,
            weights: self.weights,
            weight_digest: self.weight_digest,
            target_tick,
        }
    }

    /// Verify `weight_digest` matches payload.
    pub fn verify_digest(&self) -> bool {
        fnv1a64(&self.weights) == self.weight_digest
    }
}

/// Encode a checkpoint into a framed segment.
pub fn encode_model_segment(cp: &ModelCheckpoint) -> LewmModelSegment {
    LewmModelSegment::from_checkpoint(cp)
}

/// Decode segment into a checkpoint with the given surface / target tick.
pub fn decode_model_segment(
    segment: &LewmModelSegment,
    surface: TrainingSurfaceKind,
    target_tick: Option<u64>,
) -> WorldModelResult<ModelCheckpoint> {
    if !segment.verify_digest() {
        return Err(WorldModelError::SegmentCodec);
    }
    Ok(ModelCheckpoint {
        sensor_class: segment.sensor_class,
        kind: segment.kind,
        version_tag: segment.version_tag,
        surface,
        weights: segment.weights.clone(),
        weight_digest: segment.weight_digest,
        target_tick,
    })
}

/// Wire bytes: header + weights.
pub fn model_segment_to_wire(segment: &LewmModelSegment) -> Vec<u8> {
    let mut out = Vec::with_capacity(SEGMENT_HEADER_LEN + segment.weights.len());
    out.push(segment.kind as u8);
    out.push(segment.codec);
    out.push(segment.sensor_class as u8);
    out.extend_from_slice(&segment.version_tag.to_le_bytes());
    out.extend_from_slice(&segment.weight_digest.to_le_bytes());
    let len = segment.weights.len() as u32;
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&segment.weights);
    out
}

/// Copy `N` bytes of `src` starting at `at`; `SegmentCodec` when out of range.
fn le_array<const N: usize>(src: &[u8], at: usize) -> WorldModelResult<[u8; N]> {
    src.get(at..)
        .and_then(|s| s.first_chunk::<N>())
        .copied()
        .ok_or(WorldModelError::SegmentCodec)
}

/// Parse wire bytes produced by [`model_segment_to_wire`].
pub fn model_segment_from_wire(bytes: &[u8]) -> WorldModelResult<LewmModelSegment> {
    let Some((head, rest)) = bytes.split_first_chunk::<SEGMENT_HEADER_LEN>() else {
        return Err(WorldModelError::SegmentCodec);
    };
    let kind = SmallModelKind::try_from(head[0]).map_err(|_| WorldModelError::SegmentCodec)?;
    let codec = head[1];
    let sensor_class =
        SensorClass::try_from(head[2]).map_err(|_| WorldModelError::SegmentCodec)?;
    // Header offsets: 3..11 version tag, 11..19 digest, 19..23 weight length.
    let version_tag = u64::from_le_bytes(le_array(head, 3)?);
    let weight_digest = u64::from_le_bytes(le_array(head, 11)?);
    let weight_len = usize::try_from(u32::from_le_bytes(le_array(head, 19)?))
        .map_err(|_| WorldModelError::SegmentCodec)?;
    let weights = rest
        .get(..weight_len)
        .ok_or(WorldModelError::SegmentCodec)?
        .to_vec();
    if codec == LEWM_MODEL_SEGMENT_CODEC_SAFETENSORS && !is_safetensors_blob(&weights) {
        return Err(WorldModelError::SegmentCodec);
    }
    let seg = LewmModelSegment {
        kind,
        codec,
        sensor_class,
        version_tag,
        weight_digest,
        weights,
    };
    if !seg.verify_digest() {
        return Err(WorldModelError::SegmentCodec);
    }
    Ok(seg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use weftos_worldmodel_core::{SensorClass, SmallModelKind, TrainingSurfaceKind};

    #[test]
    fn roundtrip_wire() {
        let cp = ModelCheckpoint::new(
            SensorClass::Lidar,
            SmallModelKind::TransmitGate,
            42,
            TrainingSurfaceKind::OfflineEdge,
            alloc::vec![9, 8, 7, 6],
            Some(100),
        );
        let seg = encode_model_segment(&cp);
        let wire = model_segment_to_wire(&seg);
        assert_eq!(wire[0], 0x60);
        let restored = model_segment_from_wire(&wire).expect("wire");
        assert_eq!(restored.version_tag, 42);
        assert_eq!(restored.sensor_class, SensorClass::Lidar);
        assert_eq!(restored.weights, cp.weights);
        let cp2 = decode_model_segment(&restored, TrainingSurfaceKind::OfflineEdge, Some(100))
            .expect("decode");
        assert_eq!(cp2.weight_digest, cp.weight_digest);
        assert_eq!(cp2.target_tick, Some(100));
    }

    #[test]
    fn bad_digest_rejected() {
        let mut seg = LewmModelSegment {
            kind: SmallModelKind::Encode,
            codec: LEWM_MODEL_SEGMENT_CODEC_RAW,
            sensor_class: SensorClass::Generic,
            version_tag: 1,
            weight_digest: 0xDEAD,
            weights: alloc::vec![1, 2, 3],
        };
        assert!(!seg.verify_digest());
        let wire = model_segment_to_wire(&seg);
        assert!(model_segment_from_wire(&wire).is_err());
        seg.weight_digest = fnv1a64(&seg.weights);
        let wire = model_segment_to_wire(&seg);
        assert!(model_segment_from_wire(&wire).is_ok());
    }

    #[test]
    fn safetensors_payload_gets_codec_0x03_and_is_checked_on_the_wire() {
        // Minimal well-formed safetensors file: empty header object padded to 8 bytes.
        let mut blob = 8u64.to_le_bytes().to_vec();
        blob.extend_from_slice(b"{}      ");
        assert!(is_safetensors_blob(&blob));
        let cp = ModelCheckpoint::new(
            SensorClass::RgbD,
            SmallModelKind::Encode,
            7,
            TrainingSurfaceKind::OfflineEdge,
            blob,
            None,
        );
        let seg = encode_model_segment(&cp);
        assert_eq!(seg.codec, LEWM_MODEL_SEGMENT_CODEC_SAFETENSORS);
        let wire = model_segment_to_wire(&seg);
        assert_eq!(wire[1], 0x03);
        assert_eq!(model_segment_from_wire(&wire).unwrap().weights, cp.weights);

        // A raw blob that claims the safetensors codec is refused.
        let mut bad = LewmModelSegment::from_checkpoint(&ModelCheckpoint::new(
            SensorClass::RgbD,
            SmallModelKind::Encode,
            8,
            TrainingSurfaceKind::OfflineEdge,
            alloc::vec![1, 2, 3, 4],
            None,
        ));
        assert_eq!(bad.codec, LEWM_MODEL_SEGMENT_CODEC_RAW);
        bad.codec = LEWM_MODEL_SEGMENT_CODEC_SAFETENSORS;
        assert!(model_segment_from_wire(&model_segment_to_wire(&bad)).is_err());
    }

    #[test]
    fn truncated_wire_err() {
        assert!(model_segment_from_wire(&[0x60, 0x02]).is_err());
    }

    #[test]
    fn hostile_wire_bytes_are_errors_not_panics() {
        let cp = ModelCheckpoint::new(
            SensorClass::Lidar,
            SmallModelKind::TransmitGate,
            1,
            TrainingSurfaceKind::OfflineEdge,
            alloc::vec![1, 2, 3, 4],
            None,
        );
        let good = model_segment_to_wire(&encode_model_segment(&cp));
        // Every strict prefix of a valid segment is rejected.
        for n in 0..good.len() {
            assert_eq!(
                model_segment_from_wire(&good[..n]),
                Err(WorldModelError::SegmentCodec),
                "prefix of {n} bytes"
            );
        }
        // Declared weight length far beyond the buffer.
        let mut huge = good.clone();
        huge[19..23].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(
            model_segment_from_wire(&huge),
            Err(WorldModelError::SegmentCodec)
        );
        // Header-only input and garbage kind byte.
        assert_eq!(
            model_segment_from_wire(&[0xff; SEGMENT_HEADER_LEN]),
            Err(WorldModelError::SegmentCodec)
        );
    }

    #[test]
    fn safetensors_probe_is_total_on_short_input() {
        for n in 0..12usize {
            let _ = is_safetensors_blob(&alloc::vec![b'{'; n]);
        }
        assert!(!is_safetensors_blob(&[]));
    }
}
