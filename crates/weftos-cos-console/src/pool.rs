//! Recycled node-id pool for scrolling line views. Ids are handed out
//! round-robin from a fixed range and returned when a line leaves the
//! screen, so a long conversation walks the whole range without ever
//! reusing an id that is still on screen.

use std::collections::BTreeSet;
use std::ops::Range;

use weftos_leaf_scene::NodeId;

use crate::theme::nid;

#[derive(Debug, Clone)]
pub struct LinePool {
    range: Range<u32>,
    cursor: u32,
    live: BTreeSet<u32>,
    allocated_total: u64,
}

impl LinePool {
    pub fn new(range: Range<u32>) -> Self {
        assert!(!range.is_empty(), "pool range must not be empty");
        Self {
            cursor: range.start,
            range,
            live: BTreeSet::new(),
            allocated_total: 0,
        }
    }

    /// Next free id after the cursor. Panics only if every id in the
    /// range is live, which a bounded viewport can never reach.
    pub fn alloc(&mut self) -> NodeId {
        let len = self.range.end - self.range.start;
        assert!(
            (self.live.len() as u32) < len,
            "line pool exhausted: {} live ids",
            self.live.len()
        );
        loop {
            let n = self.cursor;
            self.cursor += 1;
            if self.cursor >= self.range.end {
                self.cursor = self.range.start;
            }
            if self.live.insert(n) {
                self.allocated_total += 1;
                return nid(n);
            }
        }
    }

    /// Return an id; a double free is ignored.
    pub fn free(&mut self, id: NodeId) {
        self.live.remove(&id.path_hash());
    }

    pub fn is_live(&self, id: NodeId) -> bool {
        self.live.contains(&id.path_hash())
    }

    pub fn live_count(&self) -> usize {
        self.live.len()
    }

    /// Total allocations so far; above the range length means the
    /// pool has wrapped at least once.
    pub fn allocated_total(&self) -> u64 {
        self.allocated_total
    }

    pub fn range(&self) -> &Range<u32> {
        &self.range
    }

    pub fn clear(&mut self) {
        self.live.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_and_skips_live_ids() {
        let mut p = LinePool::new(10..14);
        let a = p.alloc();
        let b = p.alloc();
        let _c = p.alloc();
        let _d = p.alloc();
        assert_eq!(a.path_hash(), 10);
        p.free(b);
        // Only 11 is free: the cursor wraps past 10 (live) and lands on 11.
        let e = p.alloc();
        assert_eq!(e.path_hash(), 11);
        assert_eq!(p.allocated_total(), 5);
        assert_eq!(p.live_count(), 4);
    }

    #[test]
    #[should_panic(expected = "exhausted")]
    fn exhaustion_panics() {
        let mut p = LinePool::new(0..2);
        p.alloc();
        p.alloc();
        p.alloc();
    }
}
