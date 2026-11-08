//! RustFS audit stream, log and bucket notifications into the activity log.

use super::StoreService;
use crate::audit::{self, AuditEvent, Outcome, Raw, Source};
use crate::name::StoreName;

impl StoreService {
    // ---- RustFS audit, log and notifications --------------------------------------------------

    /// The principal behind an access key and the store behind a bucket, each `None` if this
    /// service did not make it.
    pub(super) fn resolve(&self, raw: &Raw) -> (Option<String>, Option<String>) {
        let g = self.lock();
        let principal = g.keys.iter().find(|(_, k)| **k == raw.access_key).map(|(p, _)| p.clone());
        let store = g.stores.keys().filter_map(|n| StoreName::parse(n).ok()).find(|n| n.bucket() == raw.bucket).map(|n| n.to_string());
        (principal, store)
    }

    /// The activity-log event for a record RustFS reported, or `None` if it must not be logged:
    /// - a bucket that is not one of this daemon's stores is dropped (nothing of ours);
    /// - an audit or notification record from a key that is not a principal's key is the daemon's
    ///   own root-key traffic, which the governed path already logged, so logging it again would
    ///   double every request. (Refusals from the RustFS log are kept whatever the key.)
    pub(super) fn event_from(&self, raw: Raw, source: Source) -> Option<AuditEvent> {
        let (principal, store) = self.resolve(&raw);
        let store = store?;
        let principal = match (principal, source) {
            (Some(p), _) => p,
            (None, Source::RustfsLog) => raw.access_key.clone(),
            (None, _) => return None,
        };
        let outcome = match raw.status {
            Some(s) if (200..300).contains(&s) => Outcome::Ok,
            Some(403) => Outcome::Refused(raw.reason.clone().unwrap_or_else(|| "403".into())),
            Some(s) => Outcome::Error(format!("status {s}")),
            None => Outcome::Ok,
        };
        Some(AuditEvent { ts_ms: self.now(), principal, store, bucket: raw.bucket, op: raw.op, key: raw.object, bytes: raw.bytes, outcome, source })
    }

    /// One line of RustFS's audit stream into the activity log. False if it was not an audit line.
    pub fn ingest_audit_line(&self, line: &str) -> bool {
        let Some(r) = audit::parse_audit(line) else { return false };
        if let Some(ev) = self.event_from(r, Source::RustfsAudit) {
            self.audit.record(&ev);
        }
        true
    }

    /// One line of RustFS's JSON log: refusals only.
    pub fn ingest_log_line(&self, line: &str) -> bool {
        let Some(r) = audit::parse_denial(line) else { return false };
        if let Some(ev) = self.event_from(r, Source::RustfsLog) {
            self.audit.record(&ev);
        }
        true
    }

    /// A bucket notification: logged, and every changed object is passed to the invalidator.
    /// Returns the number of records.
    pub fn ingest_notification(&self, body: &str) -> usize {
        let recs = audit::parse_notification(body);
        let inv = self.invalidator.lock().unwrap_or_else(|e| e.into_inner()).clone();
        for r in &recs {
            // Whoever changed an object, a cache of it is stale: invalidate for our stores always.
            let store = self.resolve(r).1;
            if let (Some(f), Some(st), Some(k)) = (&inv, &store, &r.object) {
                f(st, k);
            }
            if let Some(ev) = self.event_from(r.clone(), Source::Notification) {
                self.audit.record(&ev);
            }
        }
        recs.len()
    }
}
