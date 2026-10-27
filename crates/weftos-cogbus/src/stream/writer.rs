//! The writer side of a stream, as a cog sees it.
//!
//! ```no_run
//! # async fn f(bus: &weftos_cogbus::Client) -> Result<(), weftos_cogbus::ClientError> {
//! use weftos_cogbus::stream::StreamOptions;
//! let mut w = bus.stream_writer("audio.pcm", "venq-mic-0", "pcm.s16le.v1", StreamOptions::new(640)).await?;
//! let seq = w.write(17, &[0u8; 640])?;
//! # let _ = seq; Ok(()) }
//! ```

use std::os::fd::{AsFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::sync::mpsc;

use super::reader::Routes;
use super::ring::{self, RingWriter};
use super::shm::Mapping;
use super::wire::Geometry;
use crate::client::{Client, ClientError};
use crate::proto::{ErrorCode, Request, write_frame};

/// What a writer asks of its ring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamOptions {
    /// Largest record this writer writes.
    pub max_record: u64,
    /// Ring size; `None` is the broker's default (4 MiB).
    pub ring_bytes: Option<u64>,
    /// Seq to continue after (a writer reopening after a broker restart
    /// passes its last seq so seq keeps increasing).
    pub seq_base: Option<u64>,
}

impl StreamOptions {
    /// Records of up to `max_record` bytes in a default-sized ring.
    pub fn new(max_record: u64) -> Self {
        Self {
            max_record,
            ring_bytes: None,
            seq_base: None,
        }
    }
}

struct BlockCursor {
    sub: u64,
    map: Mapping,
    demoted: bool,
}

/// The `block` readers of one writer, kept current by broker notices.
#[derive(Default)]
pub(crate) struct WriterShared {
    any: AtomicBool,
    cursors: Mutex<Vec<BlockCursor>>,
}

impl WriterShared {
    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<BlockCursor>> {
        self.cursors.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub(crate) fn add(&self, sub: u64, fd: OwnedFd) {
        match Mapping::new(fd.as_fd(), ring::CURSOR_BYTES, false) {
            Ok(map) => {
                let mut g = self.lock();
                g.retain(|c| c.sub != sub);
                g.push(BlockCursor {
                    sub,
                    map,
                    demoted: false,
                });
                self.any.store(true, Ordering::Release);
            }
            Err(e) => {
                tracing::warn!(target: "cogbus", sub, error = %e, "cannot map a block reader's cursor")
            }
        }
    }

    pub(crate) fn remove(&self, sub: u64) {
        let mut g = self.lock();
        g.retain(|c| c.sub != sub);
        self.any.store(!g.is_empty(), Ordering::Release);
    }
}

/// Writes one stream topic.
pub struct StreamWriter {
    topic: String,
    ring: RingWriter,
    ring_id: u64,
    shared: Arc<WriterShared>,
    routes: Arc<Routes>,
    demote: mpsc::UnboundedSender<u64>,
    deadline: Duration,
    demotions: u64,
    waited: Duration,
}

impl std::fmt::Debug for StreamWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamWriter")
            .field("topic", &self.topic)
            .field("head", &self.ring.head())
            .finish_non_exhaustive()
    }
}

impl Drop for StreamWriter {
    fn drop(&mut self) {
        self.routes.drop_writer(&self.topic, &self.shared);
    }
}

fn protocol(m: impl Into<String>) -> ClientError {
    ClientError::Protocol(m.into())
}

impl Client {
    /// Open `<kind>/<source_id>` (a declared `stream` output) as its writer.
    pub async fn stream_writer(
        &self,
        kind: &str,
        source_id: &str,
        schema: &str,
        opts: StreamOptions,
    ) -> Result<StreamWriter, ClientError> {
        let topic = format!("{kind}/{source_id}");
        let shared = Arc::new(WriterShared::default());
        let routes = self.shared.streams.clone();
        // Registered before the request: block-reader notices follow the answer.
        routes.add_writer(&topic, shared.clone());
        let req = |id| Request::StreamOpen {
            id,
            topic_kind: kind.into(),
            source_id: source_id.into(),
            schema: schema.into(),
            max_record: opts.max_record,
            ring_bytes: opts.ring_bytes,
            seq_base: opts.seq_base,
        };
        let opened = self
            .call_hook(req, |r| r)
            .await
            .and_then(|(v, fds)| open_ring(&v, fds));
        let (ring, ring_id, deadline) = match opened {
            Ok(x) => x,
            Err(e) => {
                routes.drop_writer(&topic, &shared);
                return Err(e);
            }
        };
        let (demote, mut rx) = mpsc::unbounded_channel::<u64>();
        let w = self.writer.clone();
        tokio::spawn(async move {
            while let Some(sub) = rx.recv().await {
                let mut g = w.lock().await;
                if write_frame(&mut *g, &Request::StreamDemote { id: 0, sub })
                    .await
                    .is_err()
                {
                    return;
                }
            }
        });
        Ok(StreamWriter {
            topic,
            ring,
            ring_id,
            shared,
            routes,
            demote,
            deadline,
            demotions: 0,
            waited: Duration::ZERO,
        })
    }
}

fn open_ring(v: &Value, fds: Vec<OwnedFd>) -> Result<(RingWriter, u64, Duration), ClientError> {
    let g: Geometry = serde_json::from_value(v["geometry"].clone())
        .map_err(|e| protocol(format!("stream_open answer: {e}")))?;
    let fd = fds
        .into_iter()
        .next()
        .ok_or_else(|| protocol("stream_open answer without its ring"))?;
    let map = Mapping::new(fd.as_fd(), g.bytes(), true).map_err(ClientError::Io)?;
    let ring = RingWriter::attach(map, g).map_err(protocol)?;
    let deadline = Duration::from_millis(v["block_deadline_ms"].as_u64().unwrap_or(50));
    Ok((ring, v["ring_id"].as_u64().unwrap_or(0), deadline))
}

impl StreamWriter {
    /// `<kind>/<source_id>`.
    pub fn topic(&self) -> &str {
        &self.topic
    }

    /// The ring's shape.
    pub fn geometry(&self) -> Geometry {
        self.ring.geometry()
    }

    /// Ring id from the broker.
    pub fn ring_id(&self) -> u64 {
        self.ring_id
    }

    /// Seq of the last record written (0 before the first).
    pub fn last_seq(&self) -> u64 {
        self.ring.head()
    }

    /// `block` readers this writer stopped waiting for.
    pub fn demotions(&self) -> u64 {
        self.demotions
    }

    /// Time this writer has spent waiting for lagging `block` readers.
    /// Readers of any other policy never add to it.
    pub fn waited(&self) -> Duration {
        self.waited
    }

    /// `block` readers this writer currently waits for.
    pub fn block_readers(&self) -> usize {
        self.shared.lock().iter().filter(|c| !c.demoted).count()
    }

    fn check(&self, data: &[u8]) -> Result<(), ClientError> {
        let max = self.ring.geometry().max_record;
        if data.len() as u64 > max {
            return Err(ClientError::Refused {
                code: ErrorCode::TooLarge,
                message: format!("record of {} bytes exceeds max_record {max}", data.len()),
            });
        }
        Ok(())
    }

    /// Oldest seq a reader may still need once `seq` is written: readers
    /// keep one slot clear of the writer, so it is `seq + 2 - slot_count`.
    fn keep_from(&self, seq: u64) -> u64 {
        (seq + 2).saturating_sub(self.ring.geometry().slot_count)
    }

    /// True if a live `block` reader still needs a record that writing
    /// `seq` takes out of its reach.
    fn lagging(&self, seq: u64) -> bool {
        if !self.shared.any.load(Ordering::Acquire) {
            return false;
        }
        let keep = self.keep_from(seq);
        self.shared.lock().iter().any(|c| {
            let next = ring::cursor_load(&c.map);
            !c.demoted && next != 0 && next < keep
        })
    }

    fn demote_lagging(&mut self, seq: u64) {
        let keep = self.keep_from(seq);
        for c in self.shared.lock().iter_mut() {
            let next = ring::cursor_load(&c.map);
            if !c.demoted && next != 0 && next < keep {
                c.demoted = true;
                self.demotions += 1;
                let _ = self.demote.send(c.sub);
                tracing::warn!(target: "cogbus", topic = %self.topic, sub = c.sub, "block reader past the deadline; writing on");
            }
        }
    }

    /// Write one record; returns its seq. Never waits unless a `block`
    /// reader lags a full ring behind, and then at most the deadline
    /// (50 ms by default), so call it off the async executor or use
    /// [`write_async`](Self::write_async) when `block` readers exist.
    pub fn write(&mut self, t_ns: u64, data: &[u8]) -> Result<u64, ClientError> {
        self.check(data)?;
        let seq = self.ring.head() + 1;
        if self.lagging(seq) {
            let start = Instant::now();
            let mut spins = 0u32;
            while self.lagging(seq) {
                if start.elapsed() >= self.deadline {
                    self.demote_lagging(seq);
                    break;
                }
                spins += 1;
                match spins {
                    0..64 => std::hint::spin_loop(),
                    64..128 => std::thread::yield_now(),
                    _ => std::thread::sleep(Duration::from_micros(50)),
                }
            }
            self.waited += start.elapsed();
        }
        Ok(self.ring.write(t_ns, super::now_ns(), data))
    }

    /// [`write`](Self::write) that waits for `block` readers without
    /// blocking the executor.
    pub async fn write_async(&mut self, t_ns: u64, data: &[u8]) -> Result<u64, ClientError> {
        self.check(data)?;
        let seq = self.ring.head() + 1;
        if self.lagging(seq) {
            let start = Instant::now();
            while self.lagging(seq) {
                if start.elapsed() >= self.deadline {
                    self.demote_lagging(seq);
                    break;
                }
                tokio::time::sleep(Duration::from_micros(200)).await;
            }
            self.waited += start.elapsed();
        }
        Ok(self.ring.write(t_ns, super::now_ns(), data))
    }
}
