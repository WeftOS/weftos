//! Payloads of the mesh topic frames (ADR-122 section 14, owner decision 4):
//! `TopicRequest` 0x12 (subscriber node to source node) and `TopicData` 0x13
//! (source to subscriber).
//!
//! ```text
//! payload  [ver u8 = 1][head_len u32 BE][head JSON][tail bytes]
//! ```
//!
//! The head is one [`TopicReq`] or [`TopicData`] (tagged by `op`); the tail
//! carries the binary bytes of a stream chunk or a blob slice and is empty
//! otherwise, so no stream byte is base64 or hex. Everything decoded here
//! came from a peer and is bounded before it is parsed.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Wire version.
pub const VERSION: u8 = 1;
/// Largest head (a reading body is at most 64 KiB of JSON).
pub const MAX_HEAD: usize = 96 * 1024;
/// Largest tail (one stream chunk or blob slice).
pub const MAX_TAIL: usize = 256 * 1024;

/// Which of the two frames a payload travels in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    /// `TopicRequest` 0x12.
    Request,
    /// `TopicData` 0x13.
    Data,
}

impl FrameKind {
    /// The mesh frame type byte.
    pub const fn byte(self) -> u8 {
        match self {
            Self::Request => 0x12,
            Self::Data => 0x13,
        }
    }

    /// The kind for a frame type byte.
    pub fn from_byte(b: u8) -> Option<Self> {
        match b {
            0x12 => Some(Self::Request),
            0x13 => Some(Self::Data),
            _ => None,
        }
    }
}

/// Credit a subscriber grants: readings, and bytes for streams and blobs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Credit {
    /// Readings (or stream records) the source may send.
    #[serde(default)]
    pub n: u64,
    /// Bytes of stream chunks the source may send.
    #[serde(default)]
    pub bytes: u64,
}

/// Subscriber node to source node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum TopicReq {
    /// Open subscription `sub` (the subscriber's id) on `topic`
    /// (`<kind>/<source_id>`).
    Subscribe {
        /// Subscriber-chosen id, unique per subscriber node.
        sub: u64,
        /// `<kind>/<source_id>`.
        topic: String,
        /// Schema the subscriber declared.
        schema: String,
        /// `drop-oldest` or `sample` (`block` is refused across nodes).
        policy: String,
        /// The subscriber wants raw data (owner decision 2).
        #[serde(default)]
        raw: bool,
        /// Initial credit.
        credit: Credit,
    },
    /// More credit for `sub`; any credit frame also renews it.
    Credit {
        /// Subscription.
        sub: u64,
        /// Added credit.
        credit: Credit,
    },
    /// End `sub`.
    Unsubscribe {
        /// Subscription.
        sub: u64,
    },
    /// Read `len` bytes at `offset` of the blob `hash` this node stores.
    Fetch {
        /// Subscriber-chosen request id.
        req: u64,
        /// BLAKE3 hex.
        hash: String,
        /// Offset.
        offset: u64,
        /// Bytes wanted (at most [`MAX_TAIL`]).
        len: u64,
    },
}

/// One topic a source node offers a peer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Advert {
    /// `<kind>/<source_id>`.
    pub topic: String,
    /// Schema id.
    pub schema: String,
    /// `reading`, `stream` or `blob`.
    pub class: String,
    /// The project the topic counts as on the subscriber (`project:<id>`
    /// audience), or `None` for the node bucket (`paired`, `nodes:`).
    #[serde(default)]
    pub project: Option<String>,
    /// The exposure allows raw subscribes.
    #[serde(default)]
    pub raw: bool,
}

/// Source node to subscriber node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum TopicData {
    /// Every topic this node offers the receiver now (replaces the last).
    Advert {
        /// Offered topics.
        topics: Vec<Advert>,
        /// Seconds the receiver may keep them without a new advert.
        ttl_s: u64,
    },
    /// One reading of `sub`.
    Reading {
        /// Subscription.
        sub: u64,
        /// The source topic's `seq` (a gap is a drop).
        seq: u64,
        /// Producer's capture clock.
        t_ns: u64,
        /// The reading.
        body: Value,
    },
    /// One stream record of `sub`; its bytes are the tail.
    Chunk {
        /// Subscription.
        sub: u64,
        /// The ring's `seq` (a gap is a drop).
        seq: u64,
        /// Producer's capture clock.
        t_ns: u64,
    },
    /// `sub` is over (unsubscribed, withdrawn, credit lapsed, writer gone).
    End {
        /// Subscription.
        sub: u64,
        /// Why.
        reason: String,
    },
    /// `sub` was refused.
    Error {
        /// Subscription.
        sub: u64,
        /// Snake-case error code.
        code: String,
        /// Reason.
        message: String,
    },
    /// A blob slice for `req`; its bytes are the tail.
    Blob {
        /// Request id.
        req: u64,
        /// Blob size.
        size: u64,
        /// No bytes after this slice.
        eof: bool,
    },
    /// A fetch was refused.
    BlobError {
        /// Request id.
        req: u64,
        /// Snake-case error code.
        code: String,
        /// Reason.
        message: String,
    },
}

/// Encode `head` and `tail` as one payload.
pub fn encode<T: Serialize>(head: &T, tail: &[u8]) -> Vec<u8> {
    let h = serde_json::to_vec(head).unwrap_or_default();
    let mut out = Vec::with_capacity(5 + h.len() + tail.len());
    out.push(VERSION);
    out.extend_from_slice(&(h.len() as u32).to_be_bytes());
    out.extend_from_slice(&h);
    out.extend_from_slice(tail);
    out
}

/// Decode a payload from a peer into its head and tail.
pub fn decode<T: for<'de> Deserialize<'de>>(payload: &[u8]) -> Result<(T, &[u8]), String> {
    if payload.len() < 5 || payload[0] != VERSION {
        return Err("unsupported topic frame version".into());
    }
    let n = u32::from_be_bytes([payload[1], payload[2], payload[3], payload[4]]) as usize;
    if n > MAX_HEAD || payload.len() < 5 + n {
        return Err("bad topic frame head length".into());
    }
    let tail = &payload[5 + n..];
    if tail.len() > MAX_TAIL {
        return Err("topic frame tail too large".into());
    }
    let head = serde_json::from_slice(&payload[5..5 + n]).map_err(|e| format!("bad topic frame head: {e}"))?;
    Ok((head, tail))
}
