//! Media-shaped cog schemas: PCM audio, IQ samples, camera frame handles,
//! detections and spectra.

use serde::{Deserialize, Serialize};

use super::{Check, Envelope, b64_len, fail, in_range, is_lower_hex};

/// `pcm.s16le.v1`: a chunk of interleaved signed 16-bit little-endian PCM.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PcmS16le {
    /// Common fields.
    #[serde(flatten)]
    pub env: Envelope,
    /// Sample rate, 8000..=192000.
    pub rate_hz: u32,
    /// Interleaved channels, 1..=8.
    pub channels: u8,
    /// Samples per channel in this chunk.
    pub frames: u32,
    /// Chunk bytes, standard base64 (`frames * channels * 2` bytes).
    pub data_b64: String,
}

impl PcmS16le {
    /// Check ranges and that the payload length matches the header.
    pub fn validate(&self) -> Check {
        self.env.validate()?;
        in_range("rate_hz", f64::from(self.rate_hz), 8000.0, 192_000.0)?;
        in_range("channels", f64::from(self.channels), 1.0, 8.0)?;
        in_range("frames", f64::from(self.frames), 1.0, 1_048_576.0)?;
        let want = self.frames as usize * usize::from(self.channels) * 2;
        match b64_len(&self.data_b64) {
            Some(n) if n == want => Ok(()),
            Some(n) => fail(format!("data_b64 holds {n} bytes, header says {want}")),
            None => fail("data_b64 is not valid base64"),
        }
    }
}

/// `iq.cs16.v1`: complex samples as interleaved I/Q signed 16-bit LE.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IqCs16 {
    /// Common fields.
    #[serde(flatten)]
    pub env: Envelope,
    /// Tuned centre frequency.
    pub center_hz: u64,
    /// Complex sample rate.
    pub sample_rate_hz: u32,
    /// Complex samples in this chunk.
    pub samples: u32,
    /// Chunk bytes, standard base64 (`samples * 4` bytes).
    pub data_b64: String,
}

impl IqCs16 {
    /// Check ranges and that the payload length matches the header.
    pub fn validate(&self) -> Check {
        self.env.validate()?;
        in_range("center_hz", self.center_hz as f64, 1.0, 1.0e12)?;
        in_range("sample_rate_hz", f64::from(self.sample_rate_hz), 1.0, 1.0e8)?;
        in_range("samples", f64::from(self.samples), 1.0, 4_194_304.0)?;
        let want = self.samples as usize * 4;
        match b64_len(&self.data_b64) {
            Some(n) if n == want => Ok(()),
            Some(n) => fail(format!("data_b64 holds {n} bytes, header says {want}")),
            None => fail("data_b64 is not valid base64"),
        }
    }
}

/// Pixel formats a frame handle may name.
const PIXEL_FORMATS: [&str; 6] = ["nv12", "yuyv", "rgb24", "bgr24", "gray8", "mjpeg"];

/// `video.frame-ref.v1`: a handle to a frame in the node's content store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameRef {
    /// Common fields.
    #[serde(flatten)]
    pub env: Envelope,
    /// BLAKE3 of the frame bytes, lower-case hex.
    pub hash: String,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// One of `nv12 yuyv rgb24 bgr24 gray8 mjpeg`.
    pub pixel_format: String,
    /// How long the owner keeps the bytes, milliseconds.
    pub ttl_ms: u32,
}

impl FrameRef {
    /// Check the hash, geometry, format and ttl.
    pub fn validate(&self) -> Check {
        self.env.validate()?;
        if !is_lower_hex(&self.hash, 64) {
            return fail("hash must be 64 lower-case hex chars (blake3)");
        }
        in_range("width", f64::from(self.width), 1.0, 16384.0)?;
        in_range("height", f64::from(self.height), 1.0, 16384.0)?;
        if !PIXEL_FORMATS.contains(&self.pixel_format.as_str()) {
            return fail(format!("unknown pixel_format {:?}", self.pixel_format));
        }
        in_range("ttl_ms", f64::from(self.ttl_ms), 1.0, 3_600_000.0)
    }
}

/// One detected object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Detection {
    /// Class label, 1..=64 chars.
    pub label: String,
    /// Confidence, 0..=1.
    pub score: f64,
    /// `[x, y, w, h]`, normalized to the frame, top-left origin.
    pub bbox: [f64; 4],
}

/// `detections.v1`: boxes found in one frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Detections {
    /// Common fields.
    #[serde(flatten)]
    pub env: Envelope,
    /// BLAKE3 hex of the frame the boxes refer to.
    pub frame_hash: String,
    /// Model that produced them, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// At most 256 boxes.
    pub boxes: Vec<Detection>,
}

impl Detections {
    /// Check the frame hash and every box.
    pub fn validate(&self) -> Check {
        self.env.validate()?;
        if !is_lower_hex(&self.frame_hash, 64) {
            return fail("frame_hash must be 64 lower-case hex chars (blake3)");
        }
        if self.boxes.len() > 256 {
            return fail("more than 256 boxes");
        }
        for (i, d) in self.boxes.iter().enumerate() {
            if d.label.is_empty() || d.label.chars().count() > 64 {
                return fail(format!("boxes[{i}].label must be 1..=64 chars"));
            }
            in_range(&format!("boxes[{i}].score"), d.score, 0.0, 1.0)?;
            let [x, y, w, h] = d.bbox;
            for (n, v) in [("x", x), ("y", y), ("w", w), ("h", h)] {
                in_range(&format!("boxes[{i}].bbox.{n}"), v, 0.0, 1.0)?;
            }
            if x + w > 1.0 + 1e-9 || y + h > 1.0 + 1e-9 {
                return fail(format!("boxes[{i}].bbox leaves the frame"));
            }
        }
        Ok(())
    }
}

/// `spectrum.psd.v1`: one power spectral density sweep.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpectrumPsd {
    /// Common fields.
    #[serde(flatten)]
    pub env: Envelope,
    /// Centre frequency of the sweep.
    pub center_hz: u64,
    /// Total span covered by the bins.
    pub span_hz: u64,
    /// Power per bin in dB, 1..=4096 values, low frequency first.
    pub bins: Vec<f64>,
}

impl SpectrumPsd {
    /// Check span, bin count and bin range.
    pub fn validate(&self) -> Check {
        self.env.validate()?;
        in_range("center_hz", self.center_hz as f64, 1.0, 1.0e12)?;
        in_range("span_hz", self.span_hz as f64, 1.0, 1.0e11)?;
        if self.bins.is_empty() || self.bins.len() > 4096 {
            return fail("bins must hold 1..=4096 values");
        }
        for (i, b) in self.bins.iter().enumerate() {
            in_range(&format!("bins[{i}]"), *b, -300.0, 100.0)?;
        }
        Ok(())
    }
}
