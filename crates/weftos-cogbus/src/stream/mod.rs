//! Stream topics (ADR-122 sections 3, 6 and 11; phase B2).
//!
//! A `stream` output is a shared-memory ring per topic: one writer, many
//! readers, each reader with its own cursor. No stream byte crosses the
//! broker socket; the broker creates the ring, checks the manifest, and
//! hands descriptors over the socket (`SCM_RIGHTS`): the ring read-write to
//! the writer, read-only to each reader, and to each `block` reader a
//! cursor page it writes and the writer reads.
//!
//! Reader policies:
//!
//! - `drop-oldest`: a reader that falls a ring behind jumps forward; the
//!   writer never waits and never knows. The jump is a `seq` gap.
//! - `sample`: the reader takes the newest record each time it asks.
//! - `block`: before overwriting a record a `block` reader has not read,
//!   the writer waits for it up to [`StreamConfig::block_deadline`]. Past it
//!   the writer stops waiting for that reader, reports it to the broker,
//!   which logs the switch and tells the reader (a `policy` notice); from
//!   then on it is `drop-oldest`.
//!
//! A ring outlives its writer: when the writer's connection ends, readers
//! keep reading what is there, and the next writer for the topic (the same
//! instance reconnecting) continues in the same ring at the next `seq`. A
//! broker restart loses its rings; a writer that reopens with `seq_base`
//! (its last seq) keeps `seq` increasing, and readers resume at the head.
//!
//! Platforms: Linux (memfd, sealed) for nodes, macOS (`shm_open`, unlinked
//! at once) for development. Elsewhere `stream_open` and `stream_subscribe`
//! are refused as `unsupported`.

#[cfg(unix)]
pub(crate) mod fdpass;
#[cfg(unix)]
mod reader;
#[cfg(unix)]
mod registry;
#[cfg(unix)]
pub(crate) mod ring;
#[cfg(unix)]
pub(crate) mod serve;
#[cfg(unix)]
pub(crate) mod shm;
#[cfg(all(test, unix))]
mod tests;
mod wire;
#[cfg(unix)]
mod writer;

use std::time::Duration;

#[cfg(unix)]
pub(crate) use reader::Routes;
#[cfg(unix)]
pub use reader::{PolicyChange, StreamReader};
#[cfg(unix)]
pub(crate) use registry::Streams;
pub use wire::{Geometry, StreamNotice, StreamRecord};
#[cfg(unix)]
pub use writer::{StreamOptions, StreamWriter};

/// Stream subscription ids start here, apart from reading subscriptions.
pub const STREAM_SUB_BASE: u64 = 1 << 40;

/// Stream tunables of a broker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamConfig {
    /// Ring size when the writer does not ask for one (ADR-122: 4 MiB).
    pub default_ring_bytes: u64,
    /// Largest ring a writer may ask for.
    pub max_ring_bytes: u64,
    /// All rings on this node together.
    pub node_cap_bytes: u64,
    /// Largest record payload.
    pub max_record: u64,
    /// How long a writer waits for its slowest `block` reader.
    pub block_deadline: Duration,
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            default_ring_bytes: 4 << 20,
            max_ring_bytes: 64 << 20,
            node_cap_bytes: 512 << 20,
            max_record: 1 << 20,
            block_deadline: Duration::from_millis(50),
        }
    }
}

pub(crate) fn now_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// A frame the broker pushes to a connection, with its descriptors.
#[cfg(unix)]
pub(crate) struct Outgoing {
    pub ev: crate::proto::Event,
    pub fds: Vec<std::os::fd::OwnedFd>,
}

/// Per-connection stream state, kept by the server beside the session.
#[cfg(unix)]
pub(crate) struct SessionStream {
    /// Connection id, unique in this process.
    pub conn: u64,
    pub tx: tokio::sync::mpsc::UnboundedSender<Outgoing>,
    rx: Option<tokio::sync::mpsc::UnboundedReceiver<Outgoing>>,
}

#[cfg(unix)]
impl SessionStream {
    /// Stream state for the connection whose session has id `conn`, so the
    /// broker counts a connection's readings and streams together.
    pub(crate) fn for_conn(conn: u64) -> Self {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        Self { conn, tx, rx: Some(rx) }
    }
}

#[cfg(unix)]
impl Default for SessionStream {
    fn default() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            conn: NEXT.fetch_add(1, Ordering::Relaxed),
            tx,
            rx: Some(rx),
        }
    }
}

#[cfg(unix)]
impl SessionStream {
    /// The connection's pushed frames (once; the server takes it).
    pub(crate) fn take_events(&mut self) -> Option<tokio::sync::mpsc::UnboundedReceiver<Outgoing>> {
        self.rx.take()
    }
}

/// Without shared memory there are no rings; the broker's hooks are no-ops.
#[cfg(not(unix))]
#[derive(Default)]
pub(crate) struct Streams;

#[cfg(not(unix))]
impl Streams {
    pub(crate) fn new(_cfg: StreamConfig) -> Self {
        Self
    }

    /// Stream readers held by a connection (none without rings).
    pub(crate) fn reader_count(&self, _conn: u64) -> usize {
        0
    }
}

#[cfg(not(unix))]
impl crate::Broker {
    /// This broker with stream tunables `cfg` (no rings on this platform).
    pub fn with_stream_config(self, _cfg: StreamConfig) -> Self {
        self
    }

    /// Shared memory held by stream rings on this node (none here).
    pub fn stream_bytes(&self) -> u64 {
        0
    }

    pub(crate) fn stream_rows(
        &self,
        _keep: &dyn Fn(Option<&String>) -> bool,
    ) -> Vec<crate::TopicRow> {
        Vec::new()
    }
}
