//! Injected clock. `SystemTime::now` panics on `wasm32-unknown-unknown`, so
//! nothing in this crate reads the wall clock itself.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

pub trait Clock {
    /// Unix seconds.
    fn now(&self) -> u64;
}

/// A settable clock, shared by clones. Used by tests and by hosts that already
/// own a time source (a JS `Date.now()` shim on the browser build).
#[derive(Clone, Default)]
pub struct ManualClock(Arc<AtomicU64>);

impl ManualClock {
    pub fn new(now: u64) -> Self {
        Self(Arc::new(AtomicU64::new(now)))
    }
    pub fn set(&self, now: u64) {
        self.0.store(now, Ordering::SeqCst);
    }
    pub fn advance(&self, secs: u64) {
        self.0.fetch_add(secs, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}
