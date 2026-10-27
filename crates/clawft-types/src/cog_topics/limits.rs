//! `[mesh_limits]` of a `cog.toml` (ADR-122 section 14, owner rule
//! 2026-10-27): what one cog lets another node take from its topics. Every
//! cog with `[[outputs]]` declares all four values; the defaults here are the
//! documented starting values, never applied silently.

use serde::{Deserialize, Serialize};

use super::{CogTopicError, fail};

/// Starting value of [`MeshLimits::readings_per_min`].
pub const DEFAULT_READINGS_PER_MIN: u32 = 600;
/// Starting value of [`MeshLimits::bytes_per_sec`] (4 MiB/s).
pub const DEFAULT_BYTES_PER_SEC: u64 = 4 * 1024 * 1024;
/// Starting value of [`MeshLimits::subscriptions_per_peer`].
pub const DEFAULT_SUBSCRIPTIONS_PER_PEER: u32 = 16;
/// Starting value of [`MeshLimits::credit_timeout_s`].
pub const DEFAULT_CREDIT_TIMEOUT_S: u32 = 30;

/// Per-cog limits on remote subscribers of its topics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MeshLimits {
    /// Readings one remote subscriber may receive per minute (1..=60000).
    pub readings_per_min: u32,
    /// Stream and blob bytes one remote subscriber may receive per second
    /// (1..=1 GiB).
    pub bytes_per_sec: u64,
    /// Remote subscriptions one peer may hold on this cog's topics (1..=256).
    pub subscriptions_per_peer: u32,
    /// Seconds a subscription lives without its credit renewed (5..=600).
    pub credit_timeout_s: u32,
}

impl MeshLimits {
    /// The documented starting values (templates, docs, tests).
    pub const STARTING: Self = Self {
        readings_per_min: DEFAULT_READINGS_PER_MIN,
        bytes_per_sec: DEFAULT_BYTES_PER_SEC,
        subscriptions_per_peer: DEFAULT_SUBSCRIPTIONS_PER_PEER,
        credit_timeout_s: DEFAULT_CREDIT_TIMEOUT_S,
    };

    /// Every value within its range.
    pub fn validate(&self) -> Result<(), CogTopicError> {
        if !(1..=60_000).contains(&self.readings_per_min) {
            fail("mesh_limits.readings_per_min must be within 1..=60000")?;
        }
        if !(1..=1 << 30).contains(&self.bytes_per_sec) {
            fail("mesh_limits.bytes_per_sec must be within 1..=1073741824")?;
        }
        if !(1..=256).contains(&self.subscriptions_per_peer) {
            fail("mesh_limits.subscriptions_per_peer must be within 1..=256")?;
        }
        if !(5..=600).contains(&self.credit_timeout_s) {
            fail("mesh_limits.credit_timeout_s must be within 5..=600")?;
        }
        Ok(())
    }

    /// These limits lowered by an operator's per-exposure values, where set
    /// (an exposure can narrow a cog's declaration, never widen it).
    pub fn narrowed(self, readings_per_min: Option<u32>, bytes_per_sec: Option<u64>) -> Self {
        Self {
            readings_per_min: readings_per_min.map_or(self.readings_per_min, |r| r.clamp(1, self.readings_per_min)),
            bytes_per_sec: bytes_per_sec.map_or(self.bytes_per_sec, |b| b.clamp(1, self.bytes_per_sec)),
            ..self
        }
    }
}

/// The error a manifest with `[[outputs]]` and no `[mesh_limits]` gets.
pub const MISSING_MESH_LIMITS: &str = "[mesh_limits] is required when a cog declares [[outputs]]: \
set readings_per_min, bytes_per_sec, subscriptions_per_peer and credit_timeout_s \
(starting values 600, 4194304, 16, 30; ADR-122 section 14)";
