//! Transport seam and the in-memory fake used by tests and the loopback harness.
//!
//! Real transports (TCP+Noise over the hotspot, BLE) are shell-side and P1. Frames are
//! `[4-byte big-endian length][lane byte][body]` on a stream; the fake delivers whole
//! frames, so framing helpers are exercised separately.

use crate::audio::AudioFrame;
use crate::error::NodeError;
use crate::proto::Envelope;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

pub const LANE_RELIABLE: u8 = 0x01;
pub const LANE_REALTIME: u8 = 0x02;
pub const LANE_PAIR: u8 = 0x03;
pub const MAX_FRAME: usize = 64 * 1024;

pub trait Transport {
    fn send(&mut self, frame: Vec<u8>) -> Result<(), NodeError>;
    fn recv(&mut self) -> Option<Vec<u8>>;
}

type Queue = Arc<Mutex<VecDeque<Vec<u8>>>>;

/// One end of an in-memory duplex link.
pub struct FakeTransport {
    tx: Queue,
    rx: Queue,
    open: Arc<Mutex<bool>>,
}

impl FakeTransport {
    pub fn pair() -> (Self, Self) {
        let (a, b): (Queue, Queue) = Default::default();
        let open = Arc::new(Mutex::new(true));
        (
            Self {
                tx: a.clone(),
                rx: b.clone(),
                open: open.clone(),
            },
            Self { tx: b, rx: a, open },
        )
    }

    /// Simulate the link dropping: later sends fail on both ends.
    pub fn cut(&self) {
        *self.open.lock().unwrap() = false;
    }
}

impl Transport for FakeTransport {
    fn send(&mut self, frame: Vec<u8>) -> Result<(), NodeError> {
        if !*self.open.lock().unwrap() {
            return Err(NodeError::io("link down"));
        }
        self.tx.lock().unwrap().push_back(frame);
        Ok(())
    }

    fn recv(&mut self) -> Option<Vec<u8>> {
        self.rx.lock().unwrap().pop_front()
    }
}

/// Stream framing: prefix a payload with its big-endian length.
pub fn frame_encode(payload: &[u8]) -> Result<Vec<u8>, NodeError> {
    if payload.len() > MAX_FRAME {
        return Err(NodeError::Codec("frame too large".into()));
    }
    let mut out = (payload.len() as u32).to_be_bytes().to_vec();
    out.extend_from_slice(payload);
    Ok(out)
}

/// Pull one complete frame off the front of `buf`; `None` if more bytes are needed.
pub fn frame_decode(buf: &mut Vec<u8>) -> Result<Option<Vec<u8>>, NodeError> {
    if buf.len() < 4 {
        return Ok(None);
    }
    let len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if len > MAX_FRAME {
        return Err(NodeError::Codec("frame too large".into()));
    }
    if buf.len() < 4 + len {
        return Ok(None);
    }
    let payload = buf[4..4 + len].to_vec();
    buf.drain(..4 + len);
    Ok(Some(payload))
}

#[derive(Debug, PartialEq)]
pub enum Inbound {
    Envelope(Envelope),
    Audio(AudioFrame),
    /// Pairing-lane bytes, decoded by the pairing machines.
    Pair(Vec<u8>),
}

/// Lane multiplexer over a [`Transport`].
pub struct Link<T: Transport> {
    pub transport: T,
}

impl<T: Transport> Link<T> {
    pub fn new(transport: T) -> Self {
        Self { transport }
    }

    fn send_lane(&mut self, lane: u8, body: &[u8]) -> Result<(), NodeError> {
        let mut f = Vec::with_capacity(1 + body.len());
        f.push(lane);
        f.extend_from_slice(body);
        self.transport.send(f)
    }

    pub fn send_envelope(&mut self, e: &Envelope) -> Result<(), NodeError> {
        self.send_lane(LANE_RELIABLE, &e.to_cbor()?)
    }

    pub fn send_audio(&mut self, f: &AudioFrame) -> Result<(), NodeError> {
        self.send_lane(LANE_REALTIME, &f.encode()?)
    }

    pub fn send_pair(&mut self, body: &[u8]) -> Result<(), NodeError> {
        self.send_lane(LANE_PAIR, body)
    }

    pub fn recv(&mut self) -> Result<Option<Inbound>, NodeError> {
        let Some(f) = self.transport.recv() else {
            return Ok(None);
        };
        let (lane, body) = f
            .split_first()
            .ok_or_else(|| NodeError::Codec("empty frame".into()))?;
        Ok(Some(match *lane {
            LANE_RELIABLE => Inbound::Envelope(Envelope::from_cbor(body)?),
            LANE_REALTIME => Inbound::Audio(AudioFrame::decode(body)?),
            LANE_PAIR => Inbound::Pair(body.to_vec()),
            n => return Err(NodeError::Codec(format!("unknown lane {n}"))),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_framing_handles_partial_and_coalesced_reads() {
        let mut buf = Vec::new();
        let a = frame_encode(b"hello").unwrap();
        let b = frame_encode(b"world!").unwrap();
        buf.extend_from_slice(&a[..3]);
        assert_eq!(frame_decode(&mut buf).unwrap(), None);
        buf.extend_from_slice(&a[3..]);
        buf.extend_from_slice(&b);
        assert_eq!(frame_decode(&mut buf).unwrap().unwrap(), b"hello");
        assert_eq!(frame_decode(&mut buf).unwrap().unwrap(), b"world!");
        assert_eq!(frame_decode(&mut buf).unwrap(), None);
        let mut huge = ((MAX_FRAME + 1) as u32).to_be_bytes().to_vec();
        huge.push(0);
        assert!(frame_decode(&mut huge).is_err());
    }

    #[test]
    fn cut_link_fails_sends() {
        let (mut a, b) = FakeTransport::pair();
        b.cut();
        assert!(a.send(vec![1]).is_err());
    }
}
