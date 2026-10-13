//! Resumable reader for `[4-byte big-endian length][payload]` frames.
//!
//! A poll on a socket with a short read timeout can expire in the middle of a
//! frame. That is "no more bytes yet", not a failure: the bytes already read
//! stay in the reader and the next poll continues from them. Only a closed
//! source, a real source error or an out-of-range length ends the connection.

use alloc::vec;
use alloc::vec::Vec;

/// What one read of the underlying source produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fill {
    /// This many bytes were read (non-zero).
    Data(usize),
    /// Nothing available yet (timeout / would-block); keep the connection.
    Idle,
    /// The peer closed the stream.
    Closed,
}

/// Result of one [`FrameReader::poll`].
#[derive(Debug, PartialEq, Eq)]
pub enum Poll<'a> {
    Frame(&'a [u8]),
    Idle,
    Closed,
}

#[derive(Debug, PartialEq, Eq)]
pub enum FrameError<E> {
    /// The source failed for real (not a timeout).
    Source(E),
    /// Length prefix was 0 or above the configured maximum.
    BadLength(usize),
}

pub struct FrameReader {
    buf: Vec<u8>,
    have: usize,
    max: usize,
    done: bool,
}

impl FrameReader {
    pub fn new(max_payload: usize) -> Self {
        Self { buf: vec![0u8; 4 + max_payload], have: 0, max: max_payload, done: false }
    }

    /// Bytes of a partly read frame currently held.
    pub fn buffered(&self) -> usize {
        if self.done { 0 } else { self.have }
    }

    /// Read until a whole frame is available, the source goes idle, or it
    /// ends. `read` fills the slice it is given.
    pub fn poll<E>(&mut self, mut read: impl FnMut(&mut [u8]) -> Result<Fill, E>) -> Result<Poll<'_>, FrameError<E>> {
        if self.done {
            self.have = 0;
            self.done = false;
        }
        loop {
            let need = if self.have < 4 {
                4
            } else {
                let len = u32::from_be_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]) as usize;
                if len == 0 || len > self.max {
                    self.have = 0;
                    return Err(FrameError::BadLength(len));
                }
                4 + len
            };
            if self.have == need && need > 4 {
                self.done = true;
                return Ok(Poll::Frame(&self.buf[4..need]));
            }
            match read(&mut self.buf[self.have..need]).map_err(FrameError::Source)? {
                Fill::Data(n) => self.have += n,
                Fill::Idle => return Ok(Poll::Idle),
                Fill::Closed => return Ok(Poll::Closed),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scripted source: each step is a chunk, an idle, or an error.
    enum Step { Bytes(Vec<u8>), Idle, Close, Fail }

    fn run<'a>(r: &'a mut FrameReader, steps: &mut Vec<Step>) -> Result<Poll<'a>, FrameError<&'static str>> {
        r.poll(|out| match steps.first_mut() {
            None => Ok(Fill::Idle),
            Some(Step::Idle) => { steps.remove(0); Ok(Fill::Idle) }
            Some(Step::Close) => Ok(Fill::Closed),
            Some(Step::Fail) => Err("boom"),
            Some(Step::Bytes(b)) => {
                let n = out.len().min(b.len());
                out[..n].copy_from_slice(&b[..n]);
                b.drain(..n);
                if b.is_empty() { steps.remove(0); }
                Ok(Fill::Data(n))
            }
        })
    }

    fn frame(payload: &[u8]) -> Vec<u8> {
        let mut v = (payload.len() as u32).to_be_bytes().to_vec();
        v.extend_from_slice(payload);
        v
    }

    #[test]
    fn idle_mid_body_keeps_the_partial_frame_and_resumes() {
        let mut r = FrameReader::new(64);
        let f = frame(b"hello world");
        let mut steps = vec![Step::Bytes(f[..7].to_vec()), Step::Idle, Step::Bytes(f[7..].to_vec())];
        assert_eq!(run(&mut r, &mut steps), Ok(Poll::Idle));
        assert_eq!(r.buffered(), 7);
        assert_eq!(run(&mut r, &mut steps), Ok(Poll::Frame(b"hello world")));
    }

    #[test]
    fn idle_inside_the_length_prefix_resumes_too() {
        let mut r = FrameReader::new(64);
        let f = frame(b"abc");
        let mut steps = vec![Step::Bytes(f[..2].to_vec()), Step::Idle, Step::Bytes(f[2..].to_vec())];
        assert_eq!(run(&mut r, &mut steps), Ok(Poll::Idle));
        assert_eq!(run(&mut r, &mut steps), Ok(Poll::Frame(b"abc")));
    }

    #[test]
    fn back_to_back_frames_come_out_in_order() {
        let mut r = FrameReader::new(64);
        let mut bytes = frame(b"one");
        bytes.extend(frame(b"two"));
        let mut steps = vec![Step::Bytes(bytes)];
        assert_eq!(run(&mut r, &mut steps), Ok(Poll::Frame(b"one")));
        assert_eq!(run(&mut r, &mut steps), Ok(Poll::Frame(b"two")));
        assert_eq!(run(&mut r, &mut steps), Ok(Poll::Idle));
    }

    #[test]
    fn close_and_real_errors_end_the_connection_idle_does_not() {
        let mut r = FrameReader::new(64);
        assert_eq!(run(&mut r, &mut vec![Step::Close]), Ok(Poll::Closed));
        let mut r = FrameReader::new(64);
        assert_eq!(run(&mut r, &mut vec![Step::Fail]), Err(FrameError::Source("boom")));
        let mut r = FrameReader::new(64);
        let f = frame(b"xy");
        assert_eq!(run(&mut r, &mut vec![Step::Bytes(f[..5].to_vec()), Step::Close]), Ok(Poll::Closed));
    }

    #[test]
    fn out_of_range_lengths_are_refused() {
        let mut r = FrameReader::new(8);
        assert_eq!(run(&mut r, &mut vec![Step::Bytes(vec![0, 0, 0, 0])]), Err(FrameError::BadLength(0)));
        let mut r = FrameReader::new(8);
        assert_eq!(run(&mut r, &mut vec![Step::Bytes(vec![0, 0, 0, 9])]), Err(FrameError::BadLength(9)));
        let mut r = FrameReader::new(8);
        assert_eq!(run(&mut r, &mut vec![Step::Bytes(frame(b"12345678"))]), Ok(Poll::Frame(b"12345678")));
    }
}
