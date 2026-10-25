//! Per-subscriber delivery queues (ADR-122 section 6 for readings).
//!
//! A queue never blocks the publisher: when full, the oldest unread message
//! is dropped (the broker counts it per topic). `sample` is a queue of one, so the reader always
//! gets the newest message. Every push wakes the connection that owns the
//! queue.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use tokio::sync::Notify;

use crate::proto::Delivery;

/// One subscription's pending messages.
pub struct SubQueue {
    buf: Mutex<VecDeque<Delivery>>,
    cap: usize,
    wake: Arc<Notify>,
}

impl SubQueue {
    /// Queue of `cap` (at least 1) messages waking `wake` on push.
    pub fn new(cap: usize, wake: Arc<Notify>) -> Self {
        Self {
            buf: Mutex::new(VecDeque::new()),
            cap: cap.max(1),
            wake,
        }
    }

    /// Append `d`; true if an older message was dropped to make room.
    pub fn push(&self, d: Delivery) -> bool {
        let dropped = match self.buf.lock() {
            Ok(mut g) => {
                let dropped = g.len() >= self.cap;
                if dropped {
                    g.pop_front();
                }
                g.push_back(d);
                dropped
            }
            Err(_) => return false,
        };
        self.wake.notify_one();
        dropped
    }

    /// Take everything pending, oldest first.
    pub fn drain(&self) -> Vec<Delivery> {
        self.buf
            .lock()
            .map(|mut g| g.drain(..).collect())
            .unwrap_or_default()
    }
}
