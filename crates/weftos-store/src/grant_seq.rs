//! The sequence number on the daemon's grant events (ADR-126 section 3, "Grant event sequence").
//!
//! Every chain event that changes who may do what on a store (`store.grant`, `store.revoke`,
//! `store.grant.enable|disable`, `store.principal.suspend|resume`, `store.close`, `store.archive`,
//! `store.node.retired`) carries a `seq`. The consumer (S3's `GrantLedger`) applies an event only if
//! its `seq` is above the last one it applied, so ONE event with a huge `seq` would make every later
//! event, revocations included, look stale: the `u64::MAX` freeze. Two rules close it.
//!
//! 1. **The stamp is issued here, by the single writer.** `StoreService` bumps a persisted counter by
//!    exactly one, under the same lock as the change it describes, and records the event to the chain
//!    before releasing that lock. So `seq` order is the order the changes were made, the chain sees
//!    the events in that order, there is no gap, and nobody else can name a `seq`. It starts at 1 and
//!    is never reused (it is saved with the rest of the state, ahead of the event it stamps).
//!    The service refuses further grant changes at [`CEILING`] instead of stamping past it.
//! 2. **The consumer bounds what it accepts.** [`admit`]: `last < seq <= last + MAX_GAP` and
//!    `seq <= CEILING`. A forged or corrupt `seq` far ahead is refused (not applied, `last` unmoved),
//!    so the next honest event, `last + 1`, still applies. The gap allowance exists because a consumer
//!    may legitimately miss events (filtered to its own principals, or lost with a gap it then
//!    repairs from a snapshot); [`MAX_GAP`] is far larger than any real burst and far smaller than the
//!    distance to the ceiling, so the freeze would need about `CEILING / MAX_GAP` forged-but-admitted
//!    events, none of which can be forged without the writer's key.
//!
//! A consumer that is refused an event must not advance and must resync from a snapshot (S3's
//! compaction `restore`) rather than relax the bound.

/// The highest `seq` the service will stamp: 2^62. The counter is a `u64`, so there are two
/// more bits of room above it before any arithmetic can overflow.
pub const CEILING: u64 = 1 << 62;

/// The most a consumer admits `seq` to jump past the last one it applied.
pub const MAX_GAP: u64 = 1_000_000;

/// Whether a consumer whose last applied event had `last` should apply an event stamped `seq`.
pub fn admit(last: u64, seq: u64) -> bool {
    seq > last && seq <= CEILING && seq - last <= MAX_GAP
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_next_event_and_a_bounded_gap_are_admitted_and_the_rest_are_not() {
        assert!(admit(0, 1) && admit(41, 42));
        assert!(admit(10, 10 + MAX_GAP), "a gap of exactly the bound");
        assert!(!admit(10, 11 + MAX_GAP));
        assert!(!admit(10, 10) && !admit(10, 9) && !admit(10, 0), "a repeat or a step back is stale");
        // The freeze: a forged value near the top is refused at every plausible `last`.
        for last in [0, 1, 1_000, CEILING - 1, CEILING] {
            assert!(!admit(last, u64::MAX), "u64::MAX after {last}");
            assert!(!admit(last, u64::MAX - 1));
        }
        assert!(!admit(CEILING, CEILING + 1), "nothing above the ceiling");
        assert!(admit(CEILING - 1, CEILING), "the ceiling itself is the last valid number");
        // And an honest event still applies after a refused forgery, because `last` did not move.
        let last = 500;
        assert!(!admit(last, u64::MAX) && admit(last, last + 1));
    }
}
