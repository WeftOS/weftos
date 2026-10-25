//! The cogbus wire protocol (ADR-122 section 4).
//!
//! A frame is a `u32` big-endian length followed by that many bytes of JSON.
//! A frame longer than [`MAX_FRAME`] closes the connection. Requests carry an
//! `op` tag and a client-chosen `id` that the answer echoes:
//!
//! ```json
//! {"op":"hello","id":1,"token":"<64 hex>"}
//! {"op":"declare","id":2,"outputs":[{"topic":"telemetry","schema":"telemetry.v1","class":"reading"}],"inputs":[]}
//! {"op":"publish","id":3,"topic_kind":"telemetry","source_id":"venq-board","schema":"telemetry.v1","t_ns":17,"body":{"items":[]}}
//! {"op":"subscribe","id":4,"topic":"telemetry/*","schema":"telemetry.v1"}
//! {"op":"unsubscribe","id":5,"sub":1}
//! {"op":"list","id":6}
//! {"op":"stream_open","id":7,"topic_kind":"audio.pcm","source_id":"venq-mic-0","schema":"pcm.s16le.v1","max_record":640}
//! {"op":"stream_subscribe","id":8,"topic":"audio.pcm/venq-mic-0","schema":"pcm.s16le.v1"}
//! ```
//!
//! Stream answers and `{"type":"stream",...}` notices carry descriptors
//! (`SCM_RIGHTS`) and say how many in their `fds` field ([`crate::stream`]).
//!
//! The broker answers each with `{"type":"ok","id":..,"result":..}` or
//! `{"type":"error","id":..,"code":..,"message":..}` (`id` 0 for a frame it
//! could not parse), and pushes `{"type":"msg",...}` [`Delivery`] events for
//! subscriptions. Requests reject unknown fields; events do not, so new
//! optional fields stay compatible.

use clawft_types::cog_topics::{InputPolicy, TopicClass};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Largest reading body, serialized.
pub const MAX_BODY: usize = 64 * 1024;
/// Largest frame: a full body plus room for the header fields.
pub const MAX_FRAME: usize = MAX_BODY + 4 * 1024;

/// One `[[outputs]]` entry as the broker sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputSpec {
    /// Topic kind.
    pub topic: String,
    /// Registered schema id.
    pub schema: String,
    /// Size class.
    pub class: TopicClass,
}

/// One `[[inputs]]` entry as the broker sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputSpec {
    /// Topic kind.
    pub topic: String,
    /// Registered schema id.
    pub schema: String,
    /// What a slow reader does.
    #[serde(default)]
    pub policy: InputPolicy,
}

/// A request from a cog.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    /// First frame of every connection.
    Hello {
        /// Correlation id.
        id: u64,
        /// The instance's `WEFTOS_COGBUS_TOKEN`.
        token: String,
    },
    /// Confirm or narrow what this connection publishes and reads; every
    /// entry must match the instance's manifest.
    Declare {
        /// Correlation id.
        id: u64,
        /// Outputs this connection publishes.
        #[serde(default)]
        outputs: Vec<OutputSpec>,
        /// Inputs this connection reads.
        #[serde(default)]
        inputs: Vec<InputSpec>,
    },
    /// One reading on `<topic_kind>/<source_id>`.
    Publish {
        /// Correlation id.
        id: u64,
        /// Declared output kind.
        topic_kind: String,
        /// Installed sensor or source.
        source_id: String,
        /// The declared schema id.
        schema: String,
        /// Producer's capture clock, nanoseconds.
        t_ns: u64,
        /// The reading, at most [`MAX_BODY`] bytes serialized.
        body: Value,
    },
    /// Read `<kind>/<source_id>` or every source of a kind (`<kind>/*`).
    Subscribe {
        /// Correlation id.
        id: u64,
        /// Topic or wildcard.
        topic: String,
        /// The declared input schema.
        schema: String,
    },
    /// Drop a subscription.
    Unsubscribe {
        /// Correlation id.
        id: u64,
        /// Subscription id from the subscribe answer.
        sub: u64,
    },
    /// Topics this instance can see (its own project).
    List {
        /// Correlation id.
        id: u64,
    },
    /// Open a declared `stream` output as its writer; the answer carries
    /// the ring, read-write (B2).
    StreamOpen {
        /// Correlation id.
        id: u64,
        /// Declared output kind.
        topic_kind: String,
        /// Installed sensor or source.
        source_id: String,
        /// The declared schema id.
        schema: String,
        /// Largest record payload this writer writes.
        max_record: u64,
        /// Ring size; the broker's default when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ring_bytes: Option<u64>,
        /// Seq to continue after on a new ring.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        seq_base: Option<u64>,
    },
    /// Read stream `<kind>/<source_id>`; the answer carries the ring
    /// (read-only, once a writer opened it) and a `block` reader's cursor.
    StreamSubscribe {
        /// Correlation id.
        id: u64,
        /// One topic (no wildcard).
        topic: String,
        /// The declared input schema.
        schema: String,
    },
    /// From a writer: `block` reader `sub` held it past the deadline.
    StreamDemote {
        /// Correlation id.
        id: u64,
        /// The reader's subscription id.
        sub: u64,
    },
}

impl Request {
    /// The correlation id.
    pub fn id(&self) -> u64 {
        match self {
            Self::Hello { id, .. }
            | Self::Declare { id, .. }
            | Self::Publish { id, .. }
            | Self::Subscribe { id, .. }
            | Self::Unsubscribe { id, .. }
            | Self::List { id }
            | Self::StreamOpen { id, .. }
            | Self::StreamSubscribe { id, .. }
            | Self::StreamDemote { id, .. } => *id,
        }
    }
}

/// Why a request was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// Unparseable frame, wrong first frame, bad topic or source id.
    BadRequest,
    /// Unknown or revoked token.
    Unauthorized,
    /// The peer's uid is not the uid the instance was started as.
    UidMismatch,
    /// The topic kind is not in the instance's outputs or inputs.
    Undeclared,
    /// The schema differs from the declaration or the live topic.
    SchemaMismatch,
    /// A class or policy this phase does not carry.
    Unsupported,
    /// The topic belongs to another project (grants are not in B1).
    CrossProject,
    /// Another instance writes this topic.
    WriterConflict,
    /// Body or frame over the limit.
    TooLarge,
    /// No such subscription on this connection.
    UnknownSubscription,
    /// The instance was unloaded or stopped.
    Revoked,
    /// A code this client does not know.
    #[serde(other)]
    Unknown,
}

/// One message delivered to a subscriber.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Delivery {
    /// Subscription it was delivered on.
    pub sub: u64,
    /// `<kind>/<source_id>`.
    pub topic: String,
    /// Schema id of the topic.
    pub schema: String,
    /// Installed sensor or source.
    pub source_id: String,
    /// Node whose broker took the message.
    pub node_id: String,
    /// Producer's capture clock.
    pub t_ns: u64,
    /// Per-topic counter assigned by the broker, from 1; a gap is a drop.
    pub seq: u64,
    /// Broker receive time (Unix nanoseconds), beside `t_ns`.
    pub recv_ns: u64,
    /// The reading.
    pub body: Value,
}

/// A frame from the broker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// A request succeeded.
    Ok {
        /// Correlation id.
        id: u64,
        /// What the request returns.
        #[serde(default)]
        result: Value,
    },
    /// A request was refused.
    Error {
        /// Correlation id (0: the frame could not be read).
        id: u64,
        /// Machine code.
        code: ErrorCode,
        /// Human reason.
        message: String,
    },
    /// A subscribed message.
    Msg(Delivery),
    /// A stream event for a writer or reader (B2).
    Stream(crate::stream::StreamNotice),
}

/// A frame could not be read or written.
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    /// Transport failure.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// Length prefix over the limit.
    #[error("frame of {0} bytes exceeds {MAX_FRAME}")]
    Oversize(usize),
    /// Not the expected JSON.
    #[error("bad frame: {0}")]
    Json(#[from] serde_json::Error),
}

/// Read one frame. `Ok(None)` is a clean end of stream at a frame boundary.
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> Result<Option<Vec<u8>>, FrameError> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let n = u32::from_be_bytes(len) as usize;
    if n > MAX_FRAME {
        return Err(FrameError::Oversize(n));
    }
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf).await?;
    Ok(Some(buf))
}

/// Serialize `v` as one frame (length prefix included).
pub fn encode<T: Serialize>(v: &T) -> Result<Vec<u8>, FrameError> {
    let body = serde_json::to_vec(v)?;
    if body.len() > MAX_FRAME {
        return Err(FrameError::Oversize(body.len()));
    }
    let mut out = Vec::with_capacity(body.len() + 4);
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

/// Write `v` as one frame and flush.
pub async fn write_frame<W: AsyncWrite + Unpin, T: Serialize>(
    w: &mut W,
    v: &T,
) -> Result<(), FrameError> {
    w.write_all(&encode(v)?).await?;
    w.flush().await?;
    Ok(())
}
