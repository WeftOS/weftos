//! Stream types shared by the wire protocol, the broker and the client:
//! ring [`Geometry`], [`StreamRecord`] and the broker-pushed
//! [`StreamNotice`] (`{"type":"stream","notice":...}`; a notice with
//! `fds > 0` arrives with that many descriptors attached).
//!
//! Ring layout constants live here so the geometry math compiles on every
//! platform; the ring itself is in `ring.rs` (Unix).

use clawft_types::cog_topics::InputPolicy;
use serde::{Deserialize, Serialize};

/// Header bytes before slot 0.
pub(crate) const HEADER: usize = 128;
/// Slot bytes before the payload.
pub(crate) const SLOT_HEADER: usize = 32;

/// Shape of a ring, fixed when the broker creates it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Geometry {
    /// Slots in the ring.
    pub slot_count: u64,
    /// Bytes per slot, header included (a multiple of 64).
    pub slot_size: u64,
    /// Largest record payload.
    pub max_record: u64,
}

impl Geometry {
    /// The geometry that fits `max_record` payloads in `ring_bytes`; `None`
    /// if fewer than two slots fit.
    pub fn fit(max_record: u64, ring_bytes: u64) -> Option<Self> {
        if max_record == 0 {
            return None;
        }
        let slot_size = (SLOT_HEADER as u64 + max_record.div_ceil(8) * 8).div_ceil(64) * 64;
        let slot_count = ring_bytes.checked_sub(HEADER as u64)? / slot_size;
        (slot_count >= 2).then_some(Self {
            slot_count,
            slot_size,
            max_record,
        })
    }

    /// Bytes the ring maps.
    pub fn bytes(&self) -> usize {
        HEADER + (self.slot_count * self.slot_size) as usize
    }

    pub(crate) fn valid(&self) -> bool {
        self.slot_count >= 2
            && self.slot_size.is_multiple_of(64)
            && self.slot_size >= SLOT_HEADER as u64 + self.max_record.div_ceil(8) * 8
            && self.slot_count.checked_mul(self.slot_size).is_some()
    }

    pub(crate) fn slot(&self, seq: u64) -> usize {
        HEADER + (((seq - 1) % self.slot_count) * self.slot_size) as usize
    }
}

/// One record read from a ring.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamRecord {
    /// Per-topic counter from 1; a gap is a drop.
    pub seq: u64,
    /// Producer's capture clock, nanoseconds.
    pub t_ns: u64,
    /// Writer's wall clock when it wrote the record (Unix nanoseconds).
    pub w_ns: u64,
    /// Payload.
    pub data: Vec<u8>,
}

/// A stream event the broker pushes to a writer or reader connection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "notice", rename_all = "snake_case")]
pub enum StreamNotice {
    /// To a writer: a `block` reader joined (`fds: 1`, its cursor page,
    /// read-only) or left (`fds: 0`).
    Reader {
        /// `<kind>/<source_id>`.
        topic: String,
        /// The reader's subscription id.
        sub: u64,
        /// Joined (true) or left.
        added: bool,
        /// Descriptors attached.
        fds: u32,
    },
    /// To a reader: its subscription now reads this ring (`fds: 1`,
    /// read-only), because a writer opened the topic or reopened it with a
    /// different geometry. The reader starts after `head`, the ring's
    /// head when the broker announced it, so records the writer commits
    /// before the notice is read are not missed.
    Ring {
        /// Subscription id.
        sub: u64,
        /// Ring id (changes when the ring is replaced).
        ring_id: u64,
        /// Head seq at announcement.
        #[serde(default)]
        head: u64,
        /// Shape of the ring.
        geometry: Geometry,
        /// Descriptors attached.
        fds: u32,
    },
    /// To a reader: its policy changed (a `block` reader that held the
    /// writer past the deadline is now `drop-oldest`).
    Policy {
        /// Subscription id.
        sub: u64,
        /// The policy from now on.
        policy: InputPolicy,
        /// Why.
        reason: String,
    },
}

impl StreamNotice {
    /// Descriptors attached to this notice.
    pub fn fds(&self) -> usize {
        match self {
            Self::Reader { fds, .. } | Self::Ring { fds, .. } => *fds as usize,
            Self::Policy { .. } => 0,
        }
    }
}
