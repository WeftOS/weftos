//! Claim expiry decided on time this process observed itself, not on a stored wall-clock stamp.
//!
//! A claim record carries `updated_ms` written by whoever claimed it, on their clock. Comparing it
//! with our clock lets any skew steal a live claim or keep a dead one. Instead a taker remembers
//! when it first saw a given `(epoch, updated_ms)` on its own monotonic clock and treats the claim
//! as expired only after seeing the same record, unchanged, for the full TTL. A claimer who is alive
//! refreshes the record (a new `updated_ms`), which resets the watch. Nothing here depends on the
//! two clocks agreeing. A restart forgets the watch, which only makes the wait longer (safe side).

use crate::overlay::{ObjectBackend, Result};
use crate::share::{Actor, CLAIM_TTL_MS, Handoff, claim_merge_with};
use std::collections::BTreeMap;

#[derive(Debug, Default)]
pub struct ClaimWatch {
    /// bucket -> (epoch, updated_ms, monotonic ms when first seen unchanged)
    seen: BTreeMap<String, (u64, u64, u64)>,
}

impl ClaimWatch {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record what we see now and return how long (ms, on our monotonic clock) this exact record
    /// has been observed unchanged. A first sighting is age 0.
    pub fn observe(&mut self, bucket: &str, h: &Handoff, mono_ms: u64) -> u64 {
        let rec = (h.epoch, h.updated_ms);
        match self.seen.get(bucket) {
            Some(&(e, u, first)) if (e, u) == rec => mono_ms.saturating_sub(first),
            _ => {
                self.seen.insert(bucket.into(), (rec.0, rec.1, mono_ms));
                0
            }
        }
    }
}

/// Take the merge; a live claim by someone else is taken over only after `ttl_ms` of observed
/// silence. `mono_ms` is any monotonic millisecond counter (e.g. time since process start);
/// `wall_ms` only stamps the new record.
pub fn claim_merge_observed<B: ObjectBackend>(
    b: &B,
    bucket: &str,
    me: &Actor,
    watch: &mut ClaimWatch,
    mono_ms: u64,
    wall_ms: u64,
) -> Result<Handoff> {
    claim_merge_observed_ttl(b, bucket, me, watch, mono_ms, wall_ms, CLAIM_TTL_MS)
}

pub fn claim_merge_observed_ttl<B: ObjectBackend>(
    b: &B,
    bucket: &str,
    me: &Actor,
    watch: &mut ClaimWatch,
    mono_ms: u64,
    wall_ms: u64,
    ttl_ms: u64,
) -> Result<Handoff> {
    let r = claim_merge_with(b, bucket, me, wall_ms, |cur| {
        watch.observe(bucket, cur, mono_ms) >= ttl_ms
    });
    if r.is_ok() {
        watch.seen.remove(bucket);
    }
    r
}
