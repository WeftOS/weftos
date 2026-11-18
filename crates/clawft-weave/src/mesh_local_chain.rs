//! Chain events recorded by the mesh link: `mesh.journal.anchor` and
//! `mesh.service.bound` (ADR-103 P3-U).
//!
//! Events are queued and handed to a [`ChainSink`] in order. A sink that
//! refuses an event (chain busy or not ready) leaves it queued; the queue is
//! flushed again on the next tick, including while the service is down. The
//! queue is bounded: past [`MAX_QUEUED`] the oldest entry is dropped and
//! counted, so a daemon with a dead chain does not grow without limit.

use std::collections::VecDeque;
use std::sync::Mutex;

use serde_json::{Value, json};

/// Chain event kind: the service journal head, anchored into the user chain.
pub const KIND_ANCHOR: &str = "mesh.journal.anchor";
/// Chain event kind: this daemon bound to the service under a certificate.
pub const KIND_BOUND: &str = "mesh.service.bound";
/// Bound on queued events.
pub const MAX_QUEUED: usize = 1024;

/// Where chain events end up (the kernel's `ChainManager` in the daemon).
pub trait ChainSink: Send + Sync + 'static {
    /// Append one event. `Err` keeps it queued for the next flush.
    fn append(&self, kind: &str, payload: Value) -> Result<(), String>;
}

/// A bounded in-order queue in front of a [`ChainSink`].
pub struct ChainQueue {
    sink: Box<dyn ChainSink>,
    queue: Mutex<VecDeque<(String, Value)>>,
    /// Held while events go to the sink, so the queue lock is not.
    flushing: Mutex<()>,
    dropped: std::sync::atomic::AtomicU64,
}

impl ChainQueue {
    /// Queue in front of `sink`.
    pub fn new(sink: impl ChainSink) -> Self {
        Self {
            sink: Box::new(sink),
            queue: Mutex::new(VecDeque::new()),
            flushing: Mutex::new(()),
            dropped: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Queue an event and try to flush.
    pub fn push(&self, kind: &str, payload: Value) {
        {
            let mut q = self.queue.lock().expect("queue lock poisoned");
            if q.len() >= MAX_QUEUED {
                q.pop_front();
                if self.dropped.fetch_add(1, std::sync::atomic::Ordering::Relaxed) == 0 {
                    tracing::warn!(
                        max = MAX_QUEUED,
                        "mesh chain event queue is full: the chain is not accepting events; dropping the oldest"
                    );
                }
            }
            q.push_back((kind.to_owned(), payload));
        }
        self.flush();
    }

    /// Queue an anchor of the service journal head.
    pub fn anchor(&self, seq: u64, hash: &str, node_id: &str, ts: u64) {
        self.push(KIND_ANCHOR, json!({ "seq": seq, "hash": hash, "node_id": node_id, "ts": ts }));
    }

    /// Queue the record of a (re)connection.
    pub fn bound(&self, node_id: &str, cert_serial: u64) {
        self.push(KIND_BOUND, json!({ "node_id": node_id, "cert_serial": cert_serial }));
    }

    /// Hand queued events to the sink in order, stopping at the first
    /// refusal. Returns how many are still queued.
    pub fn flush(&self) -> usize {
        // One flusher at a time; the queue lock is taken only to peek and pop,
        // never across the sink call.
        let Ok(_flushing) = self.flushing.try_lock() else { return self.pending() };
        loop {
            let front = self.queue.lock().expect("queue lock poisoned").front().cloned();
            let Some((kind, payload)) = front else { return 0 };
            if let Err(why) = self.sink.append(&kind, payload) {
                let queued = self.pending();
                tracing::debug!(%kind, %why, queued, "chain event stays queued");
                return queued;
            }
            self.queue.lock().expect("queue lock poisoned").pop_front();
        }
    }

    /// Events waiting for the sink.
    pub fn pending(&self) -> usize {
        self.queue.lock().expect("queue lock poisoned").len()
    }

    /// Events dropped because the queue was full.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(std::sync::atomic::Ordering::Relaxed)
    }
}

#[cfg(feature = "exochain")]
impl ChainSink for std::sync::Arc<clawft_kernel::chain::ChainManager> {
    fn append(&self, kind: &str, payload: Value) -> Result<(), String> {
        clawft_kernel::chain::ChainManager::append(self, "mesh.link", kind, Some(payload));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct Gate {
        open: Arc<AtomicBool>,
        got: Arc<Mutex<Vec<String>>>,
    }

    impl ChainSink for Gate {
        fn append(&self, kind: &str, payload: Value) -> Result<(), String> {
            if !self.open.load(Ordering::SeqCst) {
                return Err("chain busy".into());
            }
            self.got.lock().unwrap().push(format!("{kind}:{}", payload["seq"]));
            Ok(())
        }
    }

    #[test]
    fn refused_events_stay_queued_in_order_and_flush_later() {
        let open = Arc::new(AtomicBool::new(false));
        let got = Arc::new(Mutex::new(Vec::new()));
        let q = ChainQueue::new(Gate { open: open.clone(), got: got.clone() });
        q.anchor(1, "aa", "n", 0);
        q.anchor(2, "bb", "n", 0);
        assert_eq!(q.pending(), 2);
        assert!(got.lock().unwrap().is_empty());
        open.store(true, Ordering::SeqCst);
        q.anchor(3, "cc", "n", 0);
        assert_eq!(q.pending(), 0);
        assert_eq!(
            *got.lock().unwrap(),
            vec!["mesh.journal.anchor:1", "mesh.journal.anchor:2", "mesh.journal.anchor:3"]
        );
    }

    #[test]
    fn the_queue_is_bounded() {
        let q = ChainQueue::new(Gate {
            open: Arc::new(AtomicBool::new(false)),
            got: Arc::default(),
        });
        for i in 0..(MAX_QUEUED as u64 + 5) {
            q.anchor(i, "h", "n", 0);
        }
        assert_eq!(q.pending(), MAX_QUEUED);
        assert_eq!(q.dropped(), 5);
    }
}
