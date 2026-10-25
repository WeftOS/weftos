//! The stream ring layout and its one-writer, many-reader protocol.
//!
//! ```text
//! header (128 bytes, 8-byte words)
//!   0  magic "COGRING1"     8  version (1)      16 slot_count
//!   24 slot_size            32 max_record       40 ring_id
//!   64 head: seq of the newest committed record (0 = none yet)
//!   72 writer_epoch: bumped each time a writer attaches
//! slot i at 128 + i * slot_size
//!   0  seq (0 = empty, BUSY while being written)
//!   8  len   16 t_ns (producer clock)   24 w_ns (wall clock at write)
//!   32 payload, len bytes, as little-endian words
//! ```
//!
//! Record `seq` lives in slot `(seq - 1) % slot_count`. Every slot is a
//! seqlock: the writer marks it BUSY, writes the fields, then stores `seq`
//! and finally `head`; a reader copies the slot and keeps the copy only if
//! the slot's `seq` was the one it wanted before and after. Everything in
//! the ring is accessed through atomics.
//!
//! Readers take the geometry from the broker, never from the header, and
//! bound every loop, so a writer that scribbles on its ring can garble its
//! own records but cannot make a reader read out of bounds or spin forever.

use std::sync::atomic::{Ordering, fence};

use super::shm::Mapping;
use super::wire::{Geometry, SLOT_HEADER, StreamRecord};

const MAGIC: u64 = u64::from_le_bytes(*b"COGRING1");
const VERSION: u64 = 1;
const BUSY: u64 = u64::MAX;
const OFF_HEAD: usize = 64;
const OFF_EPOCH: usize = 72;

/// Write the header of a fresh ring (the broker does this once).
pub(crate) fn init(m: &Mapping, g: Geometry, ring_id: u64, head: u64) {
    for (off, v) in [
        (0, MAGIC),
        (8, VERSION),
        (16, g.slot_count),
        (24, g.slot_size),
        (32, g.max_record),
        (40, ring_id),
        (OFF_HEAD, head),
    ] {
        m.word(off).store(v, Ordering::Relaxed);
    }
    fence(Ordering::Release);
}

/// Newest committed seq of a ring.
pub(crate) fn head(m: &Mapping) -> u64 {
    m.word(OFF_HEAD).load(Ordering::Acquire)
}

/// Move `head` forward to `seq` (broker only, with no writer attached).
pub(crate) fn raise_head(m: &Mapping, seq: u64) {
    m.word(OFF_HEAD).fetch_max(seq, Ordering::AcqRel);
}

/// The writer's side of a ring.
#[derive(Debug)]
pub(crate) struct RingWriter {
    map: Mapping,
    g: Geometry,
    head: u64,
}

impl RingWriter {
    /// Attach to a broker-initialised, read-write mapping.
    pub(crate) fn attach(map: Mapping, g: Geometry) -> Result<Self, String> {
        if !g.valid() || map.len() < g.bytes() || !map.writable() {
            return Err("ring geometry does not match its mapping".into());
        }
        let head = head(&map);
        map.word(OFF_EPOCH).fetch_add(1, Ordering::AcqRel);
        Ok(Self { map, g, head })
    }

    /// Seq of the newest record this writer committed (or found).
    pub(crate) fn head(&self) -> u64 {
        self.head
    }

    pub(crate) fn geometry(&self) -> Geometry {
        self.g
    }

    /// Commit one record; returns its seq. `data` must fit `max_record`.
    pub(crate) fn write(&mut self, t_ns: u64, w_ns: u64, data: &[u8]) -> u64 {
        assert!(
            data.len() as u64 <= self.g.max_record,
            "record over max_record"
        );
        let seq = self.head + 1;
        let base = self.g.slot(seq);
        let m = &self.map;
        m.word(base).store(BUSY, Ordering::Relaxed);
        fence(Ordering::Release);
        m.word(base + 8).store(data.len() as u64, Ordering::Relaxed);
        m.word(base + 16).store(t_ns, Ordering::Relaxed);
        m.word(base + 24).store(w_ns, Ordering::Relaxed);
        for (i, chunk) in data.chunks(8).enumerate() {
            let mut w = [0u8; 8];
            w[..chunk.len()].copy_from_slice(chunk);
            m.word(base + SLOT_HEADER + i * 8)
                .store(u64::from_le_bytes(w), Ordering::Relaxed);
        }
        m.word(base).store(seq, Ordering::Release);
        m.word(OFF_HEAD).store(seq, Ordering::Release);
        self.head = seq;
        seq
    }
}

/// A reader's side of a ring (read-only mapping).
#[derive(Debug)]
pub(crate) struct RingReader {
    map: Mapping,
    g: Geometry,
}

impl RingReader {
    /// Attach to a ring with the broker-given geometry.
    pub(crate) fn attach(map: Mapping, g: Geometry) -> Result<Self, String> {
        if !g.valid() || map.len() < g.bytes() {
            return Err("ring geometry does not match its mapping".into());
        }
        Ok(Self { map, g })
    }

    pub(crate) fn head(&self) -> u64 {
        head(&self.map)
    }

    /// The record at `*next` or the oldest still safe to read, advancing
    /// `*next` past it; `None` when nothing new is committed.
    ///
    /// A reader more than `slot_count - 2` records behind jumps forward
    /// (one slot stays clear of the record the writer may be writing), so
    /// the returned seq is past `*next` and the gap is the drop.
    pub(crate) fn read(&self, next: &mut u64) -> Option<StreamRecord> {
        let n = self.g.slot_count;
        for _ in 0..(2 * n + 16) {
            let head = self.head();
            if head < *next {
                return None;
            }
            let oldest = (head + 2).saturating_sub(n).max(1);
            if *next < oldest {
                *next = oldest;
            }
            let base = self.g.slot(*next);
            let m = &self.map;
            let s1 = m.word(base).load(Ordering::Acquire);
            if s1 != *next {
                if s1 < *next {
                    // Committed per head but never in its slot: skip it.
                    *next += 1;
                }
                continue;
            }
            let len = m.word(base + 8).load(Ordering::Relaxed);
            let t_ns = m.word(base + 16).load(Ordering::Relaxed);
            let w_ns = m.word(base + 24).load(Ordering::Relaxed);
            let len = len.min(self.g.max_record) as usize;
            let mut data = Vec::with_capacity(len.div_ceil(8) * 8);
            for i in 0..len.div_ceil(8) {
                let w = m.word(base + SLOT_HEADER + i * 8).load(Ordering::Relaxed);
                data.extend_from_slice(&w.to_le_bytes());
            }
            data.truncate(len);
            fence(Ordering::Acquire);
            if m.word(base).load(Ordering::Relaxed) != s1 {
                continue;
            }
            let seq = *next;
            *next += 1;
            return Some(StreamRecord {
                seq,
                t_ns,
                w_ns,
                data,
            });
        }
        None
    }
}

/// A block reader's cursor page: one word, the next seq it wants (0 = not
/// attached yet). The reader maps it read-write, the writer read-only.
pub(crate) const CURSOR_BYTES: usize = 64;

pub(crate) fn cursor_load(m: &Mapping) -> u64 {
    m.word(0).load(Ordering::Acquire)
}

pub(crate) fn cursor_store(m: &Mapping, next: u64) {
    m.word(0).store(next, Ordering::Release);
}
