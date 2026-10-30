//! RVF frame codec for async stream I/O.
//!
//! Wraps rvf-wire's sync read/write functions with a length-prefixed
//! framing layer for use over tokio Unix sockets (which are stream-oriented
//! and have no message boundaries).
//!
//! ## Wire format per message
//!
//! ```text
//! [4 bytes: segment_size (LE u32)]   length prefix
//! [segment_size bytes: RVF segment]  64-byte header + payload + padding
//! ```
//!
//! The 4-byte length prefix is NOT part of the RVF spec -- it is stream
//! framing so the reader knows how many bytes to consume before handing
//! the buffer to `rvf_wire::read_segment`.

// Public API used by daemon, client, and tests.
#![allow(dead_code)]

use anyhow::{Context, anyhow};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, BufWriter};

/// A parsed RVF frame: segment header + owned payload.
pub struct RvfFrame {
    pub header: rvf_types::SegmentHeader,
    pub payload: Vec<u8>,
}

/// Reads length-prefixed RVF frames from an async stream.
pub struct RvfFrameReader<R> {
    reader: BufReader<R>,
}

impl<R: AsyncRead + Unpin> RvfFrameReader<R> {
    /// Wrap a raw reader in a buffered frame reader.
    pub fn new(reader: R) -> Self {
        Self {
            reader: BufReader::new(reader),
        }
    }

    /// Read the next length-prefixed RVF frame from the stream.
    ///
    /// Returns `Ok(None)` on clean EOF (no partial length prefix).
    /// Returns an error if the stream ends mid-frame or if the RVF
    /// segment is malformed / fails hash verification.
    pub async fn read_frame(&mut self) -> Result<Option<RvfFrame>, anyhow::Error> {
        // 1. Read 4-byte length prefix (LE u32).
        let mut len_buf = [0u8; 4];
        match self.reader.read_exact(&mut len_buf).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e).context("reading frame length prefix"),
        }
        let segment_size = u32::from_le_bytes(len_buf) as usize;

        if segment_size < rvf_types::SEGMENT_HEADER_SIZE {
            return Err(anyhow!(
                "frame segment_size ({segment_size}) is smaller than the \
                 minimum header size ({})",
                rvf_types::SEGMENT_HEADER_SIZE
            ));
        }

        // 2. Allocate and read the full segment bytes.
        let mut buf = vec![0u8; segment_size];
        self.reader
            .read_exact(&mut buf)
            .await
            .context("reading RVF segment bytes")?;

        // 3. Parse header + payload via rvf-wire.
        let (header, payload) = rvf_wire::read_segment(&buf)
            .map_err(|e| anyhow!("rvf_wire::read_segment failed: {e}"))?;

        // 4. Verify content hash.
        rvf_wire::validate_segment(&header, payload)
            .map_err(|e| anyhow!("rvf_wire::validate_segment failed: {e}"))?;

        Ok(Some(RvfFrame {
            header,
            payload: payload.to_vec(),
        }))
    }
}

/// Writes length-prefixed RVF frames to an async stream.
pub struct RvfFrameWriter<W> {
    writer: BufWriter<W>,
}

impl<W: AsyncWrite + Unpin> RvfFrameWriter<W> {
    /// Wrap a raw writer in a buffered frame writer.
    pub fn new(writer: W) -> Self {
        Self {
            writer: BufWriter::new(writer),
        }
    }

    /// Serialize an RVF segment and write it with a 4-byte length prefix.
    ///
    /// The segment bytes are produced by `rvf_wire::write_segment` (which
    /// computes the content hash, sets the timestamp, and pads to 64-byte
    /// alignment). The length prefix is the total size of those bytes.
    pub async fn write_frame(
        &mut self,
        seg_type: u8,
        payload: &[u8],
        flags: rvf_types::SegmentFlags,
        segment_id: u64,
    ) -> Result<(), anyhow::Error> {
        let segment_bytes = rvf_wire::write_segment(seg_type, payload, flags, segment_id);

        let len = segment_bytes.len() as u32;
        self.writer
            .write_all(&len.to_le_bytes())
            .await
            .context("writing frame length prefix")?;

        self.writer
            .write_all(&segment_bytes)
            .await
            .context("writing RVF segment bytes")?;

        self.writer.flush().await.context("flushing frame writer")?;

        Ok(())
    }

    /// Flush the underlying writer.
    pub async fn flush(&mut self) -> Result<(), anyhow::Error> {
        self.writer.flush().await.context("flushing frame writer")
    }
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use rvf_types::{SegmentFlags, SegmentType};

    /// Helper: write frames through a DuplexStream, then read them back.
    ///
    /// Uses `tokio::io::duplex` which creates a bidirectional in-memory
    /// stream pair. Data written to `tx` can be read from `rx`.
    async fn round_trip_frames(frames: &[(u8, Vec<u8>, SegmentFlags, u64)]) -> Vec<RvfFrame> {
        let (tx, rx) = tokio::io::duplex(64 * 1024);

        // Writer task: send all frames then drop the writer to signal EOF.
        let frames_owned: Vec<_> = frames.to_vec();
        let write_handle = tokio::spawn(async move {
            let mut writer = RvfFrameWriter::new(tx);
            for (seg_type, payload, flags, segment_id) in &frames_owned {
                writer
                    .write_frame(*seg_type, payload, *flags, *segment_id)
                    .await
                    .expect("write_frame failed");
            }
            // writer (and tx) dropped here, closing the write half -> reader sees EOF
        });

        // Reader task: collect all frames until EOF.
        let read_handle = tokio::spawn(async move {
            let mut reader = RvfFrameReader::new(rx);
            let mut result = Vec::new();
            while let Some(frame) = reader.read_frame().await.expect("read_frame failed") {
                result.push(frame);
            }
            result
        });

        write_handle.await.expect("writer task panicked");
        read_handle.await.expect("reader task panicked")
    }

    #[tokio::test]
    async fn frame_round_trip() {
        let payload = b"hello RVF frame codec".to_vec();
        let seg_type = SegmentType::Meta as u8;
        let flags = SegmentFlags::empty();
        let segment_id = 42u64;

        let frames = round_trip_frames(&[(seg_type, payload.clone(), flags, segment_id)]).await;

        assert_eq!(frames.len(), 1);
        let frame = &frames[0];
        assert_eq!(frame.header.seg_type, seg_type);
        assert_eq!(frame.header.segment_id, segment_id);
        assert_eq!(frame.payload, payload);
    }

    #[tokio::test]
    async fn empty_payload_frame() {
        let seg_type = SegmentType::Meta as u8;
        let flags = SegmentFlags::empty();
        let segment_id = 1u64;

        let frames = round_trip_frames(&[(seg_type, vec![], flags, segment_id)]).await;

        assert_eq!(frames.len(), 1);
        let frame = &frames[0];
        assert_eq!(frame.header.seg_type, seg_type);
        assert_eq!(frame.header.segment_id, segment_id);
        assert!(frame.payload.is_empty());
    }

    #[tokio::test]
    async fn content_hash_verified() {
        // Write a valid frame to a buffer, then corrupt the payload bytes
        // in the raw stream before the reader sees them.
        let payload = b"integrity check".to_vec();
        let seg_type = SegmentType::Meta as u8;
        let flags = SegmentFlags::empty();
        let segment_id = 99u64;

        // Build the raw wire bytes manually.
        let segment_bytes = rvf_wire::write_segment(seg_type, &payload, flags, segment_id);
        let len = segment_bytes.len() as u32;

        let mut raw = Vec::new();
        raw.extend_from_slice(&len.to_le_bytes());
        raw.extend_from_slice(&segment_bytes);

        // Corrupt one byte in the payload region (offset 64 is the first payload byte,
        // plus the 4-byte length prefix = offset 68).
        assert!(raw.len() > 68);
        raw[68] ^= 0xFF;

        // Try to read -- should fail hash verification.
        let cursor = std::io::Cursor::new(raw);
        let mut reader = RvfFrameReader::new(cursor);
        let result = reader.read_frame().await;
        assert!(result.is_err(), "expected hash verification error");
    }

    #[tokio::test]
    async fn multiple_frames() {
        let frames_in: Vec<(u8, Vec<u8>, SegmentFlags, u64)> = vec![
            (
                SegmentType::Meta as u8,
                b"first".to_vec(),
                SegmentFlags::empty(),
                1,
            ),
            (
                SegmentType::Meta as u8,
                b"second".to_vec(),
                SegmentFlags::empty().with(SegmentFlags::SEALED),
                2,
            ),
            (
                SegmentType::Meta as u8,
                b"third".to_vec(),
                SegmentFlags::empty(),
                3,
            ),
        ];

        let frames_out = round_trip_frames(&frames_in).await;

        assert_eq!(frames_out.len(), 3);
        assert_eq!(frames_out[0].payload, b"first");
        assert_eq!(frames_out[0].header.segment_id, 1);
        assert_eq!(frames_out[1].payload, b"second");
        assert_eq!(frames_out[1].header.segment_id, 2);
        assert!(SegmentFlags::from_raw(frames_out[1].header.flags).contains(SegmentFlags::SEALED));
        assert_eq!(frames_out[2].payload, b"third");
        assert_eq!(frames_out[2].header.segment_id, 3);
    }
}
