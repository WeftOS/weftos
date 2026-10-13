//! `weftos-leaf-types` — shared wire schema for kernel → leaf push operations.
//!
//! Both the WeftOS kernel (publisher side) and leaf firmware (subscriber side)
//! depend on this crate so they agree on payload formats.
//!
//! Designed for `no_std` + `alloc` so it compiles on embedded targets
//! (xtensa-esp32-espidf). Enable the `std` feature on the kernel side.
//!
//! Wire format: CBOR via `ciborium`. See `docs/leaf-push-protocol.md` for
//! the full protocol specification.
//!
//! Spatial leaf tags (BVH registry) live in [`spatial`] (ADR-056 Phase B /
//! WEFT-717).

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

/// BVH spatial tag registry + narrow-phase payload schemas (ADR-056 / WEFT-717).
pub mod spatial;
pub mod link;
pub mod discovery;
pub mod frame_reader;

// ── Topic layout ──────────────────────────────────────────────────

pub const PUSH_TOPIC_PREFIX: &str = "mesh.leaf.";
pub const PUSH_TOPIC_SUFFIX: &str = ".push";
pub const ANNOUNCE_TOPIC_SUFFIX: &str = ".announce";

/// Build the push topic for a leaf: `mesh.leaf.<pubkey_hex>.push`.
pub fn push_topic(pubkey_hex: &str) -> String {
    let mut s =
        String::with_capacity(PUSH_TOPIC_PREFIX.len() + pubkey_hex.len() + PUSH_TOPIC_SUFFIX.len());
    s.push_str(PUSH_TOPIC_PREFIX);
    s.push_str(pubkey_hex);
    s.push_str(PUSH_TOPIC_SUFFIX);
    s
}

/// Build the announce topic for a leaf: `mesh.leaf.<pubkey_hex>.announce`.
pub fn announce_topic(pubkey_hex: &str) -> String {
    let mut s = String::with_capacity(
        PUSH_TOPIC_PREFIX.len() + pubkey_hex.len() + ANNOUNCE_TOPIC_SUFFIX.len(),
    );
    s.push_str(PUSH_TOPIC_PREFIX);
    s.push_str(pubkey_hex);
    s.push_str(ANNOUNCE_TOPIC_SUFFIX);
    s
}

/// True when `topic` is a leaf push topic: `mesh.leaf.<pubkey>.push`.
///
/// Used by the daemon / ExoChain audit path (WEFT-150) to tag
/// `ipc.publish` events that drive leaf devices.
pub fn is_push_topic(topic: &str) -> bool {
    topic.starts_with(PUSH_TOPIC_PREFIX) && topic.ends_with(PUSH_TOPIC_SUFFIX)
}

/// True when `topic` is a leaf announce topic: `mesh.leaf.<pubkey>.announce`.
pub fn is_announce_topic(topic: &str) -> bool {
    topic.starts_with(PUSH_TOPIC_PREFIX) && topic.ends_with(ANNOUNCE_TOPIC_SUFFIX)
}

// ── Subscribe ─────────────────────────────────────────────────────

/// Sent by a leaf to the kernel right after the Noise handshake completes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Subscribe {
    pub topic: String,
    /// For future replay — ignored by kernel v1.
    pub since_seq: Option<u64>,
}

// ── Display types ─────────────────────────────────────────────────

/// Z-layer slot for display compositing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LayerSlot {
    Bg,
    Widget,
    Text,
    Alert,
}

/// Visual effect applied to a display layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LayerEffectKind {
    None,
    Static { keep_ratio: u8 },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DisplayText {
    pub z: LayerSlot,
    /// Soft cap 64 chars.
    pub text: String,
    pub x: i32,
    pub y: i32,
    /// RGB color.
    pub color: [u8; 3],
    pub clear_first: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DisplayImage {
    pub z: LayerSlot,
    /// RGB888 row-major. Must be WIDTH * HEIGHT * 3 = 6144 bytes for a 64x32 display.
    pub rgb: Vec<u8>,
    pub effect: Option<LayerEffectKind>,
    /// 0..=255; 255 = fully opaque.
    pub alpha: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DisplayClear {
    pub z: LayerSlot,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LayerEffectCmd {
    pub z: LayerSlot,
    pub effect: LayerEffectKind,
}

// ── Audio types ───────────────────────────────────────────────────

/// Audio payload variants.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum AudioDrop {
    /// Sum of sine voices — chord bed.
    Chord {
        /// Hz per voice.
        freqs: Vec<f32>,
        /// 0.0 – 1.0 per voice.
        peak_gain: f32,
        duration_ms: u32,
        present_ms_from_now: u32,
    },
    /// Procedural crab scuttle(s).
    Scuttle {
        /// Number of back-to-back ~800ms scuttles.
        scuttles: u32,
        /// 0.0 – 1.0.
        gain: f32,
        present_ms_from_now: u32,
    },
    /// Raw PCM buffer.
    ///
    /// Producers should keep CBOR-encoded envelopes under 32 KB per send.
    /// For longer audio, send multiple envelopes with staggered
    /// `present_ms_from_now`.
    Pcm {
        /// Interleaved per-channel.
        samples: Vec<i16>,
        sample_rate: u32,
        /// 1 or 2.
        channels: u8,
        /// 0.0 – 1.0.
        gain: f32,
        present_ms_from_now: u32,
    },
}

// ── LeafPush — the main push envelope ─────────────────────────────

/// Everything pushable to a leaf device.
///
/// Serialized via CBOR inside a `MeshIpcEnvelope`'s opaque payload bytes.
/// New variants are additive — old leaves ignore unknown variants.
#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum LeafPush {
    Audio(AudioDrop),
    DisplayText(DisplayText),
    DisplayImage(DisplayImage),
    DisplayClear(DisplayClear),
    DisplayBrightness { on_us: u32 },
    LayerEffect(LayerEffectCmd),
}

// ── Service advertisement ─────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AudioSinkCap {
    pub sample_rate: u32,
    pub channels: u8,
    pub bit_depth: u8,
    pub max_voices: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DisplaySinkCap {
    pub width: u32,
    pub height: u32,
    pub pixel_format: String,
    pub layers: u8,
    pub blend_modes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ComputeCap {
    pub cpu_mhz: u32,
    pub free_heap_bytes: u32,
    pub eml_core: bool,
}

/// Augmented leaf announce — tells the kernel what this leaf can do.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LeafServices {
    pub node_pubkey: [u8; 32],
    pub hostname: String,
    pub firmware_version: String,
    pub audio_sink: Option<AudioSinkCap>,
    pub display_sink: Option<DisplaySinkCap>,
    pub compute: Option<ComputeCap>,
}

// ── CBOR encode/decode helpers ────────────────────────────────────

/// CBOR encoding error.
#[derive(Debug)]
pub struct EncodeError(pub String);

impl core::fmt::Display for EncodeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "cbor encode: {}", self.0)
    }
}

/// CBOR decoding error.
#[derive(Debug)]
pub struct DecodeError(pub String);

impl core::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "cbor decode: {}", self.0)
    }
}

#[cfg(feature = "std")]
impl std::error::Error for EncodeError {}
#[cfg(feature = "std")]
impl std::error::Error for DecodeError {}

/// CBOR-serialize a value.
pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, EncodeError> {
    let mut buf = Vec::new();
    ciborium::ser::into_writer(value, &mut buf).map_err(|e| EncodeError(alloc::format!("{e}")))?;
    Ok(buf)
}

/// CBOR-deserialize a value.
pub fn decode<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T, DecodeError> {
    ciborium::de::from_reader(bytes).map_err(|e| DecodeError(alloc::format!("{e}")))
}

// ── Tests ─────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topic_construction() {
        assert_eq!(push_topic("abc123"), "mesh.leaf.abc123.push");
        assert_eq!(announce_topic("abc123"), "mesh.leaf.abc123.announce");
    }

    #[test]
    fn is_push_and_announce_topic_predicates() {
        assert!(is_push_topic("mesh.leaf.deadbeef.push"));
        assert!(!is_push_topic("mesh.leaf.deadbeef.announce"));
        assert!(!is_push_topic("public.events"));
        assert!(!is_push_topic("mesh.other.push"));
        // prefix+suffix shape only; empty pubkey still matches (caller validates hex)
        assert!(is_push_topic(&push_topic("")));
        assert!(is_push_topic("mesh.leaf.push")); // edge: empty middle segment
        assert!(is_announce_topic("mesh.leaf.aa.announce"));
        assert!(!is_announce_topic("mesh.leaf.aa.push"));
    }

    #[test]
    fn roundtrip_chord() {
        let original = LeafPush::Audio(AudioDrop::Chord {
            freqs: alloc::vec![440.0, 554.37, 659.25],
            peak_gain: 0.2,
            duration_ms: 1500,
            present_ms_from_now: 400,
        });
        let bytes = encode(&original).unwrap();
        let decoded: LeafPush = decode(&bytes).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn roundtrip_scuttle() {
        let original = LeafPush::Audio(AudioDrop::Scuttle {
            scuttles: 2,
            gain: 0.5,
            present_ms_from_now: 100,
        });
        let bytes = encode(&original).unwrap();
        let decoded: LeafPush = decode(&bytes).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn roundtrip_display_text() {
        let original = LeafPush::DisplayText(DisplayText {
            z: LayerSlot::Alert,
            text: String::from("hi"),
            x: 0,
            y: 13,
            color: [255, 255, 255],
            clear_first: true,
        });
        let bytes = encode(&original).unwrap();
        let decoded: LeafPush = decode(&bytes).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn roundtrip_display_image() {
        let original = LeafPush::DisplayImage(DisplayImage {
            z: LayerSlot::Bg,
            rgb: alloc::vec![0xFF; 64 * 32 * 3],
            effect: Some(LayerEffectKind::Static { keep_ratio: 51 }),
            alpha: 255,
        });
        let bytes = encode(&original).unwrap();
        let decoded: LeafPush = decode(&bytes).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn roundtrip_display_clear() {
        let original = LeafPush::DisplayClear(DisplayClear {
            z: LayerSlot::Widget,
        });
        let bytes = encode(&original).unwrap();
        let decoded: LeafPush = decode(&bytes).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn roundtrip_brightness() {
        let original = LeafPush::DisplayBrightness { on_us: 8 };
        let bytes = encode(&original).unwrap();
        let decoded: LeafPush = decode(&bytes).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn roundtrip_layer_effect() {
        let original = LeafPush::LayerEffect(LayerEffectCmd {
            z: LayerSlot::Bg,
            effect: LayerEffectKind::Static { keep_ratio: 80 },
        });
        let bytes = encode(&original).unwrap();
        let decoded: LeafPush = decode(&bytes).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn roundtrip_pcm() {
        let original = LeafPush::Audio(AudioDrop::Pcm {
            samples: alloc::vec![0i16, 1000, -1000, 32767, -32768],
            sample_rate: 44100,
            channels: 2,
            gain: 0.8,
            present_ms_from_now: 0,
        });
        let bytes = encode(&original).unwrap();
        let decoded: LeafPush = decode(&bytes).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn roundtrip_subscribe() {
        let original = Subscribe {
            topic: String::from("mesh.leaf.abc123.push"),
            since_seq: Some(42),
        };
        let bytes = encode(&original).unwrap();
        let decoded: Subscribe = decode(&bytes).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn roundtrip_leaf_services() {
        let original = LeafServices {
            node_pubkey: [0x42; 32],
            hostname: String::from("tidbyt-kitchen"),
            firmware_version: String::from("0.1.0"),
            audio_sink: Some(AudioSinkCap {
                sample_rate: 44100,
                channels: 2,
                bit_depth: 16,
                max_voices: 8,
            }),
            display_sink: Some(DisplaySinkCap {
                width: 64,
                height: 32,
                pixel_format: String::from("rgb888"),
                layers: 4,
                blend_modes: alloc::vec![String::from("normal"), String::from("additive")],
            }),
            compute: Some(ComputeCap {
                cpu_mhz: 240,
                free_heap_bytes: 180_000,
                eml_core: true,
            }),
        };
        let bytes = encode(&original).unwrap();
        let decoded: LeafServices = decode(&bytes).unwrap();
        assert_eq!(decoded, original);
    }
}
