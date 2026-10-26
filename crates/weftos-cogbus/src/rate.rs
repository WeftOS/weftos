//! Publish budgets from `[[outputs]].rate` (ADR-122 gap 5).
//!
//! `rate` is free text in the manifest (`1 Hz`, `16000 Hz, 20 ms chunks`).
//! Only a message rate that can be read from it is enforced:
//!
//! - `<n> Hz` is `n` messages per second, unless the text also says
//!   `<m> ms chunks` (or `<m> ms frames`, `<m> ms packets`), in which case the
//!   `Hz` is a sample rate and a message goes out every `m` ms;
//! - `<n>/s`, `<n>/sec`, `<n>/min`, `<n>/h` and `<n> msg/s` are what they
//!   say.
//!
//! Anything else (no number, a rate in samples with no chunk size) has no
//! budget; the manifest check, governance and the refusal stop still apply.
//!
//! The budget is a token bucket per (instance, topic): the declared rate times
//! [`SLACK`] sustained, a burst of [`MIN_BURST`] messages or one second of it,
//! whichever is more. A cog that follows its manifest never meets it.

use std::time::Instant;

/// How far over the declared rate a cog may run before it is refused.
pub const SLACK: f64 = 2.0;
/// Smallest burst allowed.
pub const MIN_BURST: f64 = 8.0;

/// Messages per second the manifest text declares, if it says.
pub fn parse_rate(text: &str) -> Option<f64> {
    let t = text.to_ascii_lowercase();
    let num_before = |at: usize| -> Option<f64> {
        let mut head = t[..at].trim_end();
        for w in ["messages", "message", "msgs", "msg"] {
            if let Some(h) = head.strip_suffix(w) {
                head = h.trim_end();
                break;
            }
        }
        let start = head
            .rfind(|c: char| !(c.is_ascii_digit() || c == '.'))
            .map_or(0, |i| i + 1);
        head[start..].parse::<f64>().ok().filter(|v| v.is_finite() && *v > 0.0)
    };
    // `<m> ms chunks`: the period of a message.
    for unit in ["chunks", "chunk", "frames", "frame", "packets", "packet"] {
        if let Some(at) = t.find(&format!("ms {unit}")).or_else(|| t.find(&format!("ms/{unit}")))
            && let Some(ms) = num_before(at)
        {
            return Some(1000.0 / ms);
        }
    }
    for (pat, per) in [("/s", 1.0), ("/sec", 1.0), ("/min", 1.0 / 60.0), ("/h", 1.0 / 3600.0)] {
        if let Some(at) = t.find(pat)
            && let Some(n) = num_before(at)
        {
            return Some(n * per);
        }
    }
    if let Some(at) = t.find("hz")
        && let Some(n) = num_before(at)
    {
        return Some(n);
    }
    None
}

/// A token bucket.
#[derive(Debug)]
pub(crate) struct Bucket {
    per_sec: f64,
    cap: f64,
    tokens: f64,
    last: Instant,
}

impl Bucket {
    pub(crate) fn new(declared_per_sec: f64, now: Instant) -> Self {
        let per_sec = declared_per_sec * SLACK;
        let cap = per_sec.max(MIN_BURST);
        Self { per_sec, cap, tokens: cap, last: now }
    }

    /// Take one message's worth; false when the budget is spent.
    pub(crate) fn take(&mut self, now: Instant) -> bool {
        let dt = now.saturating_duration_since(self.last).as_secs_f64();
        self.last = now;
        self.tokens = (self.tokens + dt * self.per_sec).min(self.cap);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn reads_the_rates_the_manifests_write() {
        for (text, want) in [
            ("1 Hz", Some(1.0)),
            ("0.5 Hz", Some(0.5)),
            ("10hz", Some(10.0)),
            ("16000 Hz, 20 ms chunks", Some(50.0)),
            ("20 ms frames at 16 kHz", Some(50.0)),
            ("30/s", Some(30.0)),
            ("6/min", Some(0.1)),
            ("2 msg/s", Some(2.0)),
            ("on change", None),
            ("16000 samples", None),
            ("Hz", None),
            ("0 Hz", None),
            ("", None),
        ] {
            let got = parse_rate(text);
            match (got, want) {
                (Some(g), Some(w)) => assert!((g - w).abs() < 1e-9, "{text}: {g}"),
                (g, w) => assert_eq!(g, w, "{text}"),
            }
        }
    }

    #[test]
    fn the_bucket_allows_the_declared_rate_with_slack_and_a_burst() {
        let t0 = Instant::now();
        let mut b = Bucket::new(1.0, t0);
        // A burst of MIN_BURST at once, then the sustained 2/s.
        let burst = (0..20).filter(|_| b.take(t0)).count();
        assert_eq!(burst, 8);
        assert!(!b.take(t0 + Duration::from_millis(400)));
        assert!(b.take(t0 + Duration::from_millis(500)), "2/s refills one token in 500 ms");
        let mut b = Bucket::new(100.0, t0);
        assert_eq!((0..400).filter(|_| b.take(t0)).count(), 200, "a second of 2x the rate");
    }
}
