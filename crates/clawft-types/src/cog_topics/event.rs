//! `event.v1`: something a cog noticed that a person or another cog should hear about
//! (Ventuno cog plan 5.1, `can-anomaly`): a kind, a severity, what it is about and one line of
//! detail. Not a stream of values (that is `telemetry.v1`) and not a decision: a consumer
//! decides what an event means.

use serde::{Deserialize, Serialize};

use super::{Check, Envelope, fail, in_range};

/// Longest `kind`, `subject`.
pub const MAX_KIND: usize = 64;
/// Longest `detail`, characters.
pub const MAX_DETAIL: usize = 256;

/// `event.v1`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventMsg {
    /// Common fields.
    #[serde(flatten)]
    pub env: Envelope,
    /// What happened, `[a-z][a-z0-9_.]*`, at most 64 chars (`can.new_id`).
    pub kind: String,
    /// `info`, `warn` or `alert`.
    pub severity: String,
    /// What it is about, 1..=64 chars (`0x123`, `can0`).
    pub subject: String,
    /// One line of explanation, at most 256 chars.
    pub detail: String,
    /// A number that goes with it (a rate, a count), when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<f64>,
}

impl EventMsg {
    /// Check kind, severity, subject, detail and value.
    pub fn validate(&self) -> Check {
        self.env.validate()?;
        let b = self.kind.as_bytes();
        let kind_ok = !b.is_empty()
            && b.len() <= MAX_KIND
            && b[0].is_ascii_lowercase()
            && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"_.".contains(c));
        if !kind_ok {
            return fail("kind must be [a-z][a-z0-9_.]* of at most 64 chars");
        }
        if !["info", "warn", "alert"].contains(&self.severity.as_str()) {
            return fail("severity must be info, warn or alert");
        }
        let n = self.subject.chars().count();
        if n == 0 || n > MAX_KIND {
            return fail("subject must be 1..=64 chars");
        }
        if self.detail.chars().count() > MAX_DETAIL {
            return fail("detail is longer than 256 chars");
        }
        match self.value {
            Some(v) => in_range("value", v, -1.0e18, 1.0e18),
            None => Ok(()),
        }
    }
}
