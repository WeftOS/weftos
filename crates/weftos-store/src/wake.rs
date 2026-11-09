//! Wake-ups for the L1 follower view (ADR-126 §4a; owner answers on ADR-131, 2026-11-07).
//!
//! A bucket notification on the registry bucket reaches devices through the mesh relay from one
//! site subscriber. A follower treats it only as a hint to look sooner: it re-reads the registry,
//! which stays the source of truth, and a 30-60 s poll covers every lost, late or duplicated
//! notification. So nothing here needs to be reliable; it only has to be cheap and never wrong
//! about *what to read*.
//!
//! - [`registry_hints`]: turn one S3 event notification body into the registry slugs it touches.
//! - [`Waker`]: decides when the follower should poll, from hints and the fallback interval. Hints
//!   settle for a moment first, because the daemon writes an entry and then its signature object
//!   and a strict follower that polled between the two would see an unsigned entry.
//!
//! The site subscriber that feeds `registry_hints` is the daemon's notification receiver
//! (`clawft-weave::store_ingest`, S1); the relay hop to other devices is mesh code outside this crate.

use crate::audit::parse_notification;
use crate::registry_sign::SIG_PREFIX;
use crate::share_fetch::REGISTRY_PREFIX;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Hint {
    pub slug: String,
    /// The object was removed (a worktree left the registry) rather than written.
    pub removed: bool,
}

/// The registry slugs an event notification touches. Events for other buckets, other keys, or event
/// kinds other than create/remove are ignored; an unparseable body gives nothing. A slug that is
/// not a plain key segment is dropped, so a hint can never name a path. Sorted, no duplicates.
pub fn registry_hints(body: &str, registry_bucket: &str) -> Vec<Hint> {
    let mut out: Vec<Hint> = parse_notification(body)
        .into_iter()
        .filter(|r| r.bucket == registry_bucket)
        .filter_map(|r| {
            let removed = if r.op.starts_with("ObjectCreated") {
                false
            } else if r.op.starts_with("ObjectRemoved") {
                true
            } else {
                return None;
            };
            let key = r.object?;
            let slug = key
                .strip_prefix(REGISTRY_PREFIX)
                .or_else(|| key.strip_prefix(SIG_PREFIX))?
                .strip_suffix(".json")?;
            let ok = !slug.is_empty()
                && slug.len() <= 128
                && slug.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
                && slug != "."
                && slug != "..";
            ok.then(|| Hint { slug: slug.to_owned(), removed })
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

/// When a follower should poll. Pure state, no clock of its own: callers pass monotonic milliseconds.
#[derive(Debug, Clone)]
pub struct Waker {
    settle_ms: u64,
    fallback_ms: u64,
    last_poll: u64,
    pending_since: Option<u64>,
    last_hint: u64,
}

impl Waker {
    /// `settle_ms`: wait this long after the latest hint before polling (entry and signature
    /// arrive together). `fallback_ms`: poll at least this often with no hint at all (30-60 s).
    /// `now_ms` is when the follower last polled, or starts.
    pub fn new(settle_ms: u64, fallback_ms: u64, now_ms: u64) -> Self {
        Self { settle_ms, fallback_ms, last_poll: now_ms, pending_since: None, last_hint: now_ms }
    }

    /// A notification arrived.
    pub fn hint(&mut self, now_ms: u64) {
        self.pending_since.get_or_insert(now_ms);
        self.last_hint = now_ms;
    }

    /// Should the follower poll now?
    pub fn due(&self, now_ms: u64) -> bool {
        let hinted = self.pending_since.is_some() && now_ms.saturating_sub(self.last_hint) >= self.settle_ms;
        hinted || now_ms.saturating_sub(self.last_poll) >= self.fallback_ms
    }

    /// Milliseconds until [`Self::due`] turns true, for a sleep or a `recv_timeout`.
    pub fn wait_ms(&self, now_ms: u64) -> u64 {
        let fallback = (self.last_poll + self.fallback_ms).saturating_sub(now_ms);
        match self.pending_since {
            Some(_) => fallback.min((self.last_hint + self.settle_ms).saturating_sub(now_ms)),
            None => fallback,
        }
    }

    /// The follower polled: forget pending hints and restart the fallback timer. A hint that arrives
    /// during the poll is kept for the next one by calling [`Self::hint`] after this.
    pub fn polled(&mut self, now_ms: u64) {
        self.last_poll = now_ms;
        self.pending_since = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(bucket: &str, key: &str, name: &str) -> String {
        format!(
            r#"{{"EventName":"{name}","Key":"{bucket}/{key}","Records":[{{"eventName":"{name}","s3":{{"bucket":{{"name":"{bucket}"}},"object":{{"key":"{}","size":9}}}}}}]}}"#,
            key.replace('/', "%2F")
        )
    }

    #[test]
    fn registry_events_become_slug_hints_and_everything_else_is_ignored() {
        let h = |b: &str, k: &str, n: &str| registry_hints(&ev(b, k, n), "r-x");
        assert_eq!(h("r-x", "worktrees/feat-a.json", "s3:ObjectCreated:Put"), [Hint { slug: "feat-a".into(), removed: false }]);
        assert_eq!(h("r-x", "worktrees.sig/feat-a.json", "s3:ObjectCreated:Put")[0].slug, "feat-a");
        assert_eq!(h("r-x", "worktrees/feat-a.json", "s3:ObjectRemoved:Delete"), [Hint { slug: "feat-a".into(), removed: true }]);
        assert!(h("p-other", "worktrees/feat-a.json", "s3:ObjectCreated:Put").is_empty(), "another bucket");
        assert!(h("r-x", "docs/plan.md", "s3:ObjectCreated:Put").is_empty(), "not a registry key");
        assert!(h("r-x", "worktrees/feat-a.json", "s3:ObjectAccessed:Get").is_empty(), "reads are not changes");
        assert!(h("r-x", "worktrees/../x.json", "s3:ObjectCreated:Put").is_empty(), "no path in a slug");
        assert!(h("r-x", "worktrees/a/b.json", "s3:ObjectCreated:Put").is_empty());
        assert!(registry_hints("not json", "r-x").is_empty());
        // entry + signature of one worktree collapse to one hint
        let both = format!(
            r#"{{"Records":[{},{}]}}"#,
            r#"{"eventName":"s3:ObjectCreated:Put","s3":{"bucket":{"name":"r-x"},"object":{"key":"worktrees%2Fa.json"}}}"#,
            r#"{"eventName":"s3:ObjectCreated:Put","s3":{"bucket":{"name":"r-x"},"object":{"key":"worktrees.sig%2Fa.json"}}}"#
        );
        assert_eq!(registry_hints(&both, "r-x").len(), 1);
    }

    #[test]
    fn the_waker_settles_after_hints_and_falls_back_to_the_poll_interval() {
        let mut w = Waker::new(500, 30_000, 0);
        assert!(!w.due(1_000), "nothing to do yet");
        w.hint(1_000);
        assert!(!w.due(1_200), "settling: the signature may still be coming");
        w.hint(1_300);
        assert!(!w.due(1_700), "a later hint restarts the settle");
        assert!(w.due(1_800));
        assert_eq!(w.wait_ms(1_500), 300);
        w.polled(1_800);
        assert!(!w.due(1_900) && !w.due(31_000), "poll done, hints cleared");
        assert!(w.due(31_800), "no hint ever arrived: the fallback poll still runs");
        assert_eq!(w.wait_ms(2_000), 29_800);
    }
}
