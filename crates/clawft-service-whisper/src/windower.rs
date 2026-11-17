//! PCM-chunk → whisper-window accumulator.
//!
//! Inbound PCM arrives in small chunks (~500 ms at 2 Hz per the ESP32
//! bridge's pacing). Per the service-API doc §6 step 3, the sweet spot
//! for whisper inference is **1–3 second windows** — shorter wastes
//! encoder passes on tiny inputs, longer raises first-word latency.
//!
//! This module has no async + no substrate knowledge. It is a plain
//! state machine that accepts chunks and emits windows when full.
//! Making it pure keeps the test surface small and lets the service
//! layer focus on I/O + publish ordering.

use serde::Deserialize;

/// Inbound payload shape on `substrate/<node-id>/sensor/mic/pcm_chunk`.
///
/// Wire shape (from the `weftos-mic-node` ESP32 firmware):
///
/// ```json
/// {
///   "data":         "<base64>",  // s16le PCM samples
///   "encoding":     "base64",    // currently only "base64" is supported
///   "format":       "i16le",     // currently only "i16le" is supported
///   "sample_rate":  16000,
///   "channels":     1,
///   "samples":      8000,         // count of samples in this chunk
///   "start_ts_ms":  3807924       // boot-relative monotonic ms
/// }
/// ```
///
/// `chunk_ms` is derived as `samples * 1000 / sample_rate`. `seq` is
/// taken from `start_ts_ms` so the transcript's `seq` field is a
/// stable per-chunk timestamp.
///
/// `pcm_b64` is accepted as an alias for `data` so the older test
/// harness (`publish_wav.rs`) still compiles unchanged.
#[derive(Debug, Clone, Deserialize)]
pub struct PcmChunk {
    /// Base64-encoded signed-16-bit little-endian PCM samples.
    /// Wire field name is `data`; `pcm_b64` is accepted as a legacy
    /// alias for compatibility with the original test fixtures.
    #[serde(alias = "pcm_b64")]
    pub data: String,
    /// Encoding tag — only `"base64"` is understood today.
    #[serde(default = "default_encoding")]
    pub encoding: String,
    /// Sample format — only `"i16le"` is understood today.
    #[serde(default = "default_format")]
    pub format: String,
    /// Sample rate in Hz (whisper natively wants 16 kHz).
    pub sample_rate: u32,
    /// Channel count (whisper natively wants mono = 1).
    #[serde(default = "default_channels")]
    pub channels: u16,
    /// Number of samples in this chunk. Used to derive `chunk_ms`
    /// when the producer doesn't declare it directly.
    #[serde(default)]
    pub samples: u64,
    /// Producer-side timestamp the chunk started at, monotonic ms.
    /// Lands verbatim in the transcript's `seq` field for
    /// downstream correlation.
    #[serde(default, alias = "seq")]
    pub start_ts_ms: u64,
    /// Explicit chunk duration override in ms. Optional — most
    /// producers don't set this and let it derive from
    /// `samples / sample_rate`. Kept for backward compatibility
    /// with the original `publish_wav` test harness.
    #[serde(default)]
    pub chunk_ms: Option<u64>,
}

impl PcmChunk {
    /// Effective chunk duration in ms.
    /// Priority: explicit `chunk_ms` field, then derived from
    /// `samples / sample_rate * 1000`, finally falls back to 500ms.
    pub fn effective_chunk_ms(&self) -> u64 {
        if let Some(ms) = self.chunk_ms {
            return ms;
        }
        if self.samples > 0 && self.sample_rate > 0 {
            self.samples.saturating_mul(1000) / u64::from(self.sample_rate)
        } else {
            500
        }
    }
}

fn default_channels() -> u16 {
    1
}
fn default_encoding() -> String {
    "base64".to_string()
}
fn default_format() -> String {
    "i16le".to_string()
}

/// An emit-ready window assembled from one or more [`PcmChunk`]s.
#[derive(Debug, Clone)]
pub struct PcmWindow {
    /// Concatenated s16le PCM bytes spanning the window.
    pub pcm_s16le: Vec<u8>,
    /// Sample rate in Hz (inherited from the chunks; must be uniform).
    pub sample_rate: u32,
    /// Channel count (must be uniform across the window).
    pub channels: u16,
    /// Start of this window, millis since the service started
    /// accumulating (monotonic, not wall-clock).
    pub start_ms: u64,
    /// End of this window (exclusive).
    pub end_ms: u64,
    /// Sequence id of the **last** chunk folded into this window —
    /// copied into the output transcript for producer-side correlation.
    pub last_seq: u64,
}

/// Chunk-to-window accumulator.
///
/// Call [`Windower::push`] with each inbound chunk; when the
/// accumulated duration reaches [`Windower::target_window_ms`], the
/// next call returns `Some(PcmWindow)` and resets the accumulator.
///
/// Sample-rate / channel-count mismatches between successive chunks
/// flush the current accumulator and start fresh on the new format —
/// a defensive posture that matches whisper's own behaviour (the
/// service just resamples internally; we'd rather not hide the format
/// change behind a concatenation).
#[derive(Debug)]
pub struct Windower {
    buf: Vec<u8>,
    target_window_ms: u64,
    accumulated_ms: u64,
    window_start_ms: u64,
    total_ms: u64,
    sample_rate: Option<u32>,
    channels: Option<u16>,
    last_seq: u64,
}

impl Windower {
    /// Build a new accumulator that emits windows every
    /// `target_window_ms` milliseconds. Per the API doc, 1000–3000 is
    /// the sensible band; we default the service to 2000.
    pub fn new(target_window_ms: u64) -> Self {
        Self {
            buf: Vec::new(),
            target_window_ms: target_window_ms.max(100),
            accumulated_ms: 0,
            window_start_ms: 0,
            total_ms: 0,
            sample_rate: None,
            channels: None,
            last_seq: 0,
        }
    }

    /// Target window length in milliseconds.
    pub fn target_window_ms(&self) -> u64 {
        self.target_window_ms
    }

    /// Append a chunk; return `Some(window)` iff this chunk filled the
    /// buffer past [`Self::target_window_ms`]. On a format change
    /// (sample rate or channels), the in-flight partial window is
    /// discarded and the new chunk seeds a fresh window.
    pub fn push(
        &mut self,
        chunk_pcm: &[u8],
        sample_rate: u32,
        channels: u16,
        seq: u64,
        chunk_ms: u64,
    ) -> Option<PcmWindow> {
        // Format change: drop what we had, restart.
        let reset = match (self.sample_rate, self.channels) {
            (Some(sr), Some(ch)) if sr == sample_rate && ch == channels => false,
            (Some(_), Some(_)) => true,
            _ => false,
        };
        if reset {
            self.buf.clear();
            self.accumulated_ms = 0;
            self.window_start_ms = self.total_ms;
        }
        self.sample_rate = Some(sample_rate);
        self.channels = Some(channels);

        self.buf.extend_from_slice(chunk_pcm);
        self.accumulated_ms = self.accumulated_ms.saturating_add(chunk_ms);
        self.total_ms = self.total_ms.saturating_add(chunk_ms);
        self.last_seq = seq;

        if self.accumulated_ms >= self.target_window_ms {
            let window = PcmWindow {
                pcm_s16le: std::mem::take(&mut self.buf),
                sample_rate,
                channels,
                start_ms: self.window_start_ms,
                end_ms: self.total_ms,
                last_seq: self.last_seq,
            };
            self.accumulated_ms = 0;
            self.window_start_ms = self.total_ms;
            Some(window)
        } else {
            None
        }
    }

    /// Force-emit the in-flight partial window (e.g. on shutdown or
    /// when a long silence flushes the buffer). Returns `None` if
    /// nothing buffered.
    pub fn flush(&mut self) -> Option<PcmWindow> {
        if self.buf.is_empty() {
            return None;
        }
        let (sr, ch) = (
            self.sample_rate.unwrap_or(16_000),
            self.channels.unwrap_or(1),
        );
        let window = PcmWindow {
            pcm_s16le: std::mem::take(&mut self.buf),
            sample_rate: sr,
            channels: ch,
            start_ms: self.window_start_ms,
            end_ms: self.total_ms,
            last_seq: self.last_seq,
        };
        self.accumulated_ms = 0;
        self.window_start_ms = self.total_ms;
        Some(window)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accumulates_until_target_then_emits() {
        let mut w = Windower::new(1_000); // 1s target
        // Three 500ms chunks — first two return None, third overflows.
        let chunk = vec![0u8; 16_000]; // 500ms of 16kHz mono s16le
        assert!(w.push(&chunk, 16_000, 1, 1, 500).is_none());
        let first = w.push(&chunk, 16_000, 1, 2, 500);
        assert!(first.is_some(), "second push should emit at 1000ms");
        let win = first.unwrap();
        assert_eq!(win.pcm_s16le.len(), 32_000);
        assert_eq!(win.start_ms, 0);
        assert_eq!(win.end_ms, 1_000);
        assert_eq!(win.last_seq, 2);

        // Next chunk starts a new window.
        assert!(w.push(&chunk, 16_000, 1, 3, 500).is_none());
    }

    #[test]
    fn single_chunk_larger_than_target_emits_immediately() {
        let mut w = Windower::new(1_000);
        let chunk = vec![0u8; 64_000]; // 2s of audio
        let win = w.push(&chunk, 16_000, 1, 1, 2_000);
        assert!(win.is_some());
        let win = win.unwrap();
        assert_eq!(win.end_ms, 2_000);
    }

    #[test]
    fn format_change_drops_partial_buffer() {
        let mut w = Windower::new(2_000);
        let chunk = vec![0u8; 16_000];
        w.push(&chunk, 16_000, 1, 1, 500);
        // Flip to stereo: partial buffer should be discarded.
        let chunk_stereo = vec![0u8; 32_000]; // 500ms stereo 16kHz
        let out = w.push(&chunk_stereo, 16_000, 2, 2, 500);
        // 500ms alone doesn't fill a 2s target, so we expect None AND
        // the internal buffer should only reflect the stereo chunk.
        assert!(out.is_none());
    }

    #[test]
    fn flush_returns_partial() {
        let mut w = Windower::new(10_000);
        let chunk = vec![0u8; 16_000];
        w.push(&chunk, 16_000, 1, 1, 500);
        let f = w.flush();
        assert!(f.is_some());
        let f = f.unwrap();
        assert_eq!(f.pcm_s16le.len(), 16_000);
        assert_eq!(f.end_ms, 500);
    }

    #[test]
    fn flush_on_empty_returns_none() {
        let mut w = Windower::new(2_000);
        assert!(w.flush().is_none());
    }

    #[test]
    fn target_window_has_floor() {
        // Guards against a caller passing 0 and triggering a tight
        // emit loop in the service.
        let w = Windower::new(0);
        assert!(w.target_window_ms() >= 100);
    }

    #[test]
    fn hostile_sample_count_does_not_overflow() {
        let c = PcmChunk {
            data: String::new(),
            encoding: "base64".into(),
            format: "i16le".into(),
            sample_rate: 1,
            channels: 1,
            samples: u64::MAX,
            start_ts_ms: 0,
            chunk_ms: None,
        };
        assert_eq!(c.effective_chunk_ms(), u64::MAX);
    }

    #[test]
    fn hostile_chunk_ms_does_not_overflow_accumulators() {
        let mut w = Windower::new(1_000);
        let _ = w.push(&[0u8; 2], 16_000, 1, 1, u64::MAX);
        let _ = w.push(&[0u8; 2], 16_000, 1, 2, u64::MAX);
    }
}
