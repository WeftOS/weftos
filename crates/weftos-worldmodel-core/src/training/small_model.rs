//! [`SmallModelKind`] and the LeWM RVF small-model segment constants.

/// Three trainable RVF-hosted small models per sensor class (WEFT-532).
///
/// Discriminants occupy the LeWM RVF segment domain `0x60`–`0x6F`
/// (after ECC `0x50`–`0x5F` and context-router `0x40`–`0x42`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
#[repr(u8)]
pub enum SmallModelKind {
    /// Collect-stage transmit gate (novelty / quality / salience / summary).
    TransmitGate = 0x60,
    /// Multi-sensor fusion / aggregate stage.
    Aggregate = 0x61,
    /// SIGReg-manifold encoder stage (ViT-tiny path when weights exist).
    Encode = 0x62,
}

impl SmallModelKind {
    /// Stable wire / log name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TransmitGate => "transmit_gate",
            Self::Aggregate => "aggregate",
            Self::Encode => "encode",
        }
    }

    /// RVF segment type byte.
    pub const fn segment_type(self) -> u8 {
        self as u8
    }

    /// All three model variants.
    pub const ALL: [SmallModelKind; 3] =
        [Self::TransmitGate, Self::Aggregate, Self::Encode];
}

impl TryFrom<u8> for SmallModelKind {
    type Error = u8;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0x60 => Ok(Self::TransmitGate),
            0x61 => Ok(Self::Aggregate),
            0x62 => Ok(Self::Encode),
            other => Err(other),
        }
    }
}

/// Start of the LeWM small-model RVF segment domain.
pub const LEWM_MODEL_SEGMENT_DOMAIN_START: u8 = 0x60;
/// Inclusive end of the LeWM small-model RVF segment domain.
pub const LEWM_MODEL_SEGMENT_DOMAIN_END: u8 = 0x6F;

/// JSON / wire codec byte for LeWM model segments (mirrors ECC JSON = `0x01`).
pub const LEWM_MODEL_SEGMENT_CODEC_JSON: u8 = 0x01;
/// Raw little-endian weight blob codec.
pub const LEWM_MODEL_SEGMENT_CODEC_RAW: u8 = 0x02;
/// Safetensors blob codec (candle ViT-tiny / AdaLN `pred_φ` checkpoints, plan W5).
pub const LEWM_MODEL_SEGMENT_CODEC_SAFETENSORS: u8 = 0x03;
