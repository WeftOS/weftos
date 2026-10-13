//! Realtime-lane audio frame (glasses.v1 section 4.2). Unacknowledged, no per-frame
//! signature (the secure channel authenticates the session). Fixed 18-byte header.
//!
//! ```text
//! 0      1      2      3      4..8   8..12  12..16   16..18   18..
//! magic  stream codec  flags  utt    seq    ts_ms    len      payload
//! ```
//! Integers are big-endian.

use crate::error::NodeError;

pub const MAGIC: u8 = 0xA7;
pub const HEADER_LEN: usize = 18;
/// Largest payload accepted (one Opus frame is at most 1275 bytes; PCM debug frames fit).
pub const MAX_PAYLOAD: usize = 4000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[repr(u8)]
pub enum Stream {
    /// glasses to phone
    Up = 1,
    /// phone to glasses
    Down = 2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[repr(u8)]
pub enum Codec {
    Opus = 1,
    /// Debug codec on the local link only.
    Pcm16 = 2,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct AudioFrame {
    pub stream: Stream,
    pub codec: Codec,
    pub flags: u8,
    pub utt: u32,
    pub seq: u32,
    pub ts_ms: u32,
    pub payload: Vec<u8>,
}

impl AudioFrame {
    pub fn encode(&self) -> Result<Vec<u8>, NodeError> {
        if self.payload.len() > MAX_PAYLOAD {
            return Err(NodeError::Codec(format!(
                "audio payload {} > {MAX_PAYLOAD}",
                self.payload.len()
            )));
        }
        let mut out = Vec::with_capacity(HEADER_LEN + self.payload.len());
        out.extend_from_slice(&[MAGIC, self.stream as u8, self.codec as u8, self.flags]);
        out.extend_from_slice(&self.utt.to_be_bytes());
        out.extend_from_slice(&self.seq.to_be_bytes());
        out.extend_from_slice(&self.ts_ms.to_be_bytes());
        out.extend_from_slice(&(self.payload.len() as u16).to_be_bytes());
        out.extend_from_slice(&self.payload);
        Ok(out)
    }

    pub fn decode(b: &[u8]) -> Result<Self, NodeError> {
        if b.len() < HEADER_LEN {
            return Err(NodeError::Codec("audio frame truncated".into()));
        }
        if b[0] != MAGIC {
            return Err(NodeError::Codec("audio frame bad magic".into()));
        }
        let stream = match b[1] {
            1 => Stream::Up,
            2 => Stream::Down,
            n => return Err(NodeError::Codec(format!("audio frame bad stream {n}"))),
        };
        let codec = match b[2] {
            1 => Codec::Opus,
            2 => Codec::Pcm16,
            n => return Err(NodeError::Codec(format!("audio frame bad codec {n}"))),
        };
        let u32_at = |i: usize| u32::from_be_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        let len = u16::from_be_bytes([b[16], b[17]]) as usize;
        if len > MAX_PAYLOAD || b.len() != HEADER_LEN + len {
            return Err(NodeError::Codec("audio frame length mismatch".into()));
        }
        Ok(Self {
            stream,
            codec,
            flags: b[3],
            utt: u32_at(4),
            seq: u32_at(8),
            ts_ms: u32_at(12),
            payload: b[HEADER_LEN..].to_vec(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame() -> AudioFrame {
        AudioFrame {
            stream: Stream::Up,
            codec: Codec::Opus,
            flags: 0,
            utt: 7,
            seq: 0x0102_0304,
            ts_ms: 123_456,
            payload: vec![1, 2, 3, 4, 5],
        }
    }

    #[test]
    fn roundtrip_and_layout() {
        let f = frame();
        let b = f.encode().unwrap();
        assert_eq!(b.len(), HEADER_LEN + 5);
        assert_eq!(&b[..4], &[MAGIC, 1, 1, 0]);
        assert_eq!(&b[8..12], &[1, 2, 3, 4]);
        assert_eq!(AudioFrame::decode(&b).unwrap(), f);
    }

    #[test]
    fn empty_payload_ok() {
        let mut f = frame();
        f.payload.clear();
        assert_eq!(AudioFrame::decode(&f.encode().unwrap()).unwrap(), f);
    }

    #[test]
    fn rejects_malformed() {
        let good = frame().encode().unwrap();
        assert!(AudioFrame::decode(&good[..10]).is_err());
        let mut bad = good.clone();
        bad[0] = 0;
        assert!(AudioFrame::decode(&bad).is_err());
        let mut bad = good.clone();
        bad[1] = 9;
        assert!(AudioFrame::decode(&bad).is_err());
        let mut bad = good.clone();
        bad[2] = 9;
        assert!(AudioFrame::decode(&bad).is_err());
        let mut bad = good.clone();
        bad.push(0); // trailing byte
        assert!(AudioFrame::decode(&bad).is_err());
        let mut big = frame();
        big.payload = vec![0; MAX_PAYLOAD + 1];
        assert!(big.encode().is_err());
    }
}
