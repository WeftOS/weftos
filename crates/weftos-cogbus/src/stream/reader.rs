//! The reader side of a stream, and the client's routing of stream notices.
//!
//! ```no_run
//! # async fn f(bus: &weftos_cogbus::Client) -> Result<(), weftos_cogbus::ClientError> {
//! let mut r = bus.subscribe_stream("audio.pcm/venq-mic-0", "pcm.s16le.v1").await?;
//! while let Some(rec) = r.next().await { let _ = (rec.seq, rec.data); }
//! # Ok(()) }
//! ```

use std::collections::HashMap;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use clawft_types::cog_topics::InputPolicy;
use serde_json::Value;

use super::ring::{self, RingReader};
use super::shm::Mapping;
use super::wire::{Geometry, StreamNotice, StreamRecord};
use super::writer::WriterShared;
use crate::client::{Client, ClientError};
use crate::proto::Request;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// A reader's policy changed (the broker said so).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyChange {
    /// The policy from now on.
    pub policy: InputPolicy,
    /// Why.
    pub reason: String,
}

/// A ring handed to a reader: its id, the ring, and the seq the reader
/// starts at (the head when it was handed over, plus one).
type Attached = (u64, RingReader, u64);

#[derive(Default)]
struct ReaderShared {
    closed: AtomicBool,
    changed: AtomicBool,
    ring: Mutex<Option<Attached>>,
    cursor: Option<Mapping>,
    events: Mutex<Vec<PolicyChange>>,
}

impl ReaderShared {
    /// Attach `ring`: the reader starts after `head` (the broker's head when
    /// it handed the ring over), and a
    /// `block` reader's cursor says so before the writer can overwrite.
    fn attach(&self, ring_id: u64, ring: RingReader, head: u64) {
        let start = head + 1;
        if let Some(c) = &self.cursor {
            ring::cursor_store(c, start);
        }
        *lock(&self.ring) = Some((ring_id, ring, start));
        self.changed.store(true, Ordering::Release);
    }
}

fn map_ring(g: Geometry, fd: OwnedFd) -> Result<RingReader, String> {
    let map = Mapping::new(fd.as_fd(), g.bytes(), false).map_err(|e| e.to_string())?;
    RingReader::attach(map, g)
}

/// Where the client sends stream notices: writers by topic, readers by sub.
#[derive(Default)]
pub(crate) struct Routes {
    writers: Mutex<HashMap<String, Arc<WriterShared>>>,
    readers: Mutex<HashMap<u64, Arc<ReaderShared>>>,
}

impl Routes {
    pub(crate) fn add_writer(&self, topic: &str, w: Arc<WriterShared>) {
        lock(&self.writers).insert(topic.to_string(), w);
    }

    pub(crate) fn drop_writer(&self, topic: &str, w: &Arc<WriterShared>) {
        let mut g = lock(&self.writers);
        if g.get(topic).is_some_and(|x| Arc::ptr_eq(x, w)) {
            g.remove(topic);
        }
    }

    /// Apply one broker notice with its descriptors.
    pub(crate) fn notice(&self, n: StreamNotice, fds: Vec<OwnedFd>) {
        let mut fds = fds.into_iter();
        match n {
            StreamNotice::Reader {
                topic, sub, added, ..
            } => {
                let Some(w) = lock(&self.writers).get(&topic).cloned() else {
                    return;
                };
                match (added, fds.next()) {
                    (true, Some(fd)) => w.add(sub, fd),
                    (true, None) => {
                        tracing::warn!(target: "cogbus", sub, "reader notice without its cursor")
                    }
                    (false, _) => w.remove(sub),
                }
            }
            StreamNotice::Ring {
                sub,
                ring_id,
                head,
                geometry,
                ..
            } => {
                let Some(r) = lock(&self.readers).get(&sub).cloned() else {
                    return;
                };
                let Some(fd) = fds.next() else { return };
                match map_ring(geometry, fd) {
                    Ok(ring) => r.attach(ring_id, ring, head),
                    Err(e) => {
                        tracing::warn!(target: "cogbus", sub, error = %e, "cannot map a stream ring")
                    }
                }
            }
            StreamNotice::Policy {
                sub,
                policy,
                reason,
            } => {
                if let Some(r) = lock(&self.readers).get(&sub).cloned() {
                    lock(&r.events).push(PolicyChange { policy, reason });
                    r.changed.store(true, Ordering::Release);
                }
            }
        }
    }

    /// The connection is gone.
    pub(crate) fn close_all(&self) {
        for r in lock(&self.readers).values() {
            r.closed.store(true, Ordering::Release);
        }
    }
}

/// Reads one stream topic.
pub struct StreamReader {
    sub: u64,
    topic: String,
    shared: Arc<ReaderShared>,
    routes: Arc<Routes>,
    ring: Option<(u64, RingReader)>,
    policy: InputPolicy,
    next: u64,
    last_seq: u64,
    skipped: u64,
    events: Vec<PolicyChange>,
    poll: Duration,
}

impl std::fmt::Debug for StreamReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamReader")
            .field("sub", &self.sub)
            .field("topic", &self.topic)
            .field("policy", &self.policy)
            .field("next", &self.next)
            .finish_non_exhaustive()
    }
}

impl Drop for StreamReader {
    fn drop(&mut self) {
        lock(&self.routes.readers).remove(&self.sub);
    }
}

fn protocol(m: impl Into<String>) -> ClientError {
    ClientError::Protocol(m.into())
}

/// What a stream subscribe answer opened, set up in the client's reader
/// task so no notice for the subscription can arrive before its route.
struct Opened {
    sub: u64,
    shared: Arc<ReaderShared>,
    policy: InputPolicy,
}

fn opened(routes: &Routes, v: &Value, fds: Vec<OwnedFd>) -> Result<Opened, ClientError> {
    let sub = v["sub"]
        .as_u64()
        .ok_or_else(|| protocol("stream subscribe answer without sub"))?;
    let policy: InputPolicy = serde_json::from_value(v["policy"].clone())
        .map_err(|e| protocol(format!("stream subscribe answer: {e}")))?;
    let mut fds = fds.into_iter();
    let ring = match v["ring"].is_null() {
        true => None,
        false => {
            let g: Geometry = serde_json::from_value(v["ring"]["geometry"].clone())
                .map_err(|e| protocol(format!("stream subscribe answer: {e}")))?;
            let fd = fds
                .next()
                .ok_or_else(|| protocol("stream subscribe answer without its ring"))?;
            Some(map_ring(g, fd).map_err(protocol)?)
        }
    };
    let cursor = match v["cursor"].as_bool() {
        Some(true) => {
            let fd = fds
                .next()
                .ok_or_else(|| protocol("block subscribe answer without its cursor"))?;
            Some(Mapping::new(fd.as_fd(), ring::CURSOR_BYTES, true).map_err(ClientError::Io)?)
        }
        _ => None,
    };
    let shared = Arc::new(ReaderShared {
        cursor,
        ..ReaderShared::default()
    });
    if let Some(ring) = ring {
        let head = v["ring"]["head"].as_u64().unwrap_or_else(|| ring.head());
        shared.attach(v["ring"]["ring_id"].as_u64().unwrap_or(0), ring, head);
    }
    lock(&routes.readers).insert(sub, shared.clone());
    Ok(Opened {
        sub,
        shared,
        policy,
    })
}

impl Client {
    /// Read stream `<kind>/<source_id>` (a declared input). The reader
    /// starts at the ring's head; if no writer has opened the topic yet, it
    /// starts when one does.
    pub async fn subscribe_stream(
        &self,
        topic: &str,
        schema: &str,
    ) -> Result<StreamReader, ClientError> {
        let routes = self.shared.streams.clone();
        let r2 = routes.clone();
        let req = |id| Request::StreamSubscribe {
            id,
            topic: topic.into(),
            schema: schema.into(),
        };
        let o = self
            .call_hook(req, move |r| r.and_then(|(v, fds)| opened(&r2, &v, fds)))
            .await?;
        Ok(StreamReader {
            sub: o.sub,
            topic: topic.to_string(),
            shared: o.shared,
            routes,
            ring: None,
            policy: o.policy,
            next: 0,
            last_seq: 0,
            skipped: 0,
            events: Vec::new(),
            poll: Duration::from_millis(1),
        })
    }
}

impl StreamReader {
    /// Subscription id (for [`Client::unsubscribe`]).
    pub fn id(&self) -> u64 {
        self.sub
    }

    /// `<kind>/<source_id>`.
    pub fn topic(&self) -> &str {
        &self.topic
    }

    /// The policy in force (a demoted `block` reader is `drop-oldest`).
    pub fn policy(&self) -> InputPolicy {
        self.policy
    }

    /// Records skipped so far (the sum of the `seq` gaps seen).
    pub fn skipped(&self) -> u64 {
        self.skipped
    }

    /// Seq of the last record returned (0 before the first).
    pub fn last_seq(&self) -> u64 {
        self.last_seq
    }

    /// How long [`next`](Self::next) sleeps when nothing is new (1 ms).
    pub fn set_poll_interval(&mut self, d: Duration) {
        self.poll = d.max(Duration::from_micros(50));
    }

    /// Policy changes the broker announced since the last call.
    pub fn take_events(&mut self) -> Vec<PolicyChange> {
        self.refresh();
        std::mem::take(&mut self.events)
    }

    fn refresh(&mut self) {
        if !self.shared.changed.swap(false, Ordering::AcqRel) {
            return;
        }
        if let Some((id, ring, start)) = lock(&self.shared.ring).take() {
            // A new ring: resume where it was handed over.
            self.next = start;
            self.ring = Some((id, ring));
        }
        for e in lock(&self.shared.events).drain(..) {
            self.policy = e.policy;
            self.events.push(e);
        }
    }

    /// The next record if one is committed.
    pub fn try_next(&mut self) -> Option<StreamRecord> {
        self.refresh();
        let (_, ring) = self.ring.as_ref()?;
        if self.policy == InputPolicy::Sample {
            self.next = self.next.max(ring.head());
        }
        let r = ring.read(&mut self.next)?;
        if self.last_seq != 0 && r.seq > self.last_seq + 1 {
            self.skipped += r.seq - self.last_seq - 1;
        }
        self.last_seq = r.seq;
        if self.policy == InputPolicy::Block
            && let Some(c) = &self.shared.cursor
        {
            ring::cursor_store(c, self.next);
        }
        Some(r)
    }

    /// The next record, waiting for it; `None` once the connection is gone
    /// and nothing is left to read.
    pub async fn next(&mut self) -> Option<StreamRecord> {
        loop {
            if let Some(r) = self.try_next() {
                return Some(r);
            }
            if self.shared.closed.load(Ordering::Acquire) {
                return self.try_next();
            }
            tokio::time::sleep(self.poll).await;
        }
    }
}
