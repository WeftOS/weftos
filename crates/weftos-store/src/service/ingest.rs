//! RustFS audit stream, log and bucket notifications into the activity log.

use super::StoreService;
use crate::audit::{self, AuditEvent, Outcome, Raw, Source};
use crate::name::StoreName;

impl StoreService {
    // ---- RustFS audit, log and notifications --------------------------------------------------

    pub(super) fn resolve(&self, raw: &Raw) -> (String, String) {
        let g = self.lock();
        let principal = g.keys.iter().find(|(_, k)| **k == raw.access_key).map_or_else(|| raw.access_key.clone(), |(p, _)| p.clone());
        let store = g
            .stores
            .keys()
            .filter_map(|n| StoreName::parse(n).ok())
            .find(|n| n.bucket() == raw.bucket)
            .map_or_else(|| raw.bucket.clone(), |n| n.to_string());
        (principal, store)
    }

    pub(super) fn event_from(&self, raw: Raw, source: Source) -> AuditEvent {
        let (principal, store) = self.resolve(&raw);
        let outcome = match raw.status {
            Some(s) if (200..300).contains(&s) => Outcome::Ok,
            Some(403) => Outcome::Refused(raw.reason.clone().unwrap_or_else(|| "403".into())),
            Some(s) => Outcome::Error(format!("status {s}")),
            None => Outcome::Ok,
        };
        AuditEvent { ts_ms: self.now(), principal, store, bucket: raw.bucket, op: raw.op, key: raw.object, bytes: raw.bytes, outcome, source }
    }

    /// One line of RustFS's audit stream into the activity log. False if it was not an audit line.
    pub fn ingest_audit_line(&self, line: &str) -> bool {
        audit::parse_audit(line).map(|r| self.audit.record(&self.event_from(r, Source::RustfsAudit))).is_some()
    }

    /// One line of RustFS's JSON log: refusals only.
    pub fn ingest_log_line(&self, line: &str) -> bool {
        audit::parse_denial(line).map(|r| self.audit.record(&self.event_from(r, Source::RustfsLog))).is_some()
    }

    /// A bucket notification: logged, and every changed object is passed to the invalidator.
    /// Returns the number of records.
    pub fn ingest_notification(&self, body: &str) -> usize {
        let recs = audit::parse_notification(body);
        let inv = self.invalidator.lock().unwrap_or_else(|e| e.into_inner()).clone();
        for r in &recs {
            let ev = self.event_from(r.clone(), Source::Notification);
            if let (Some(f), Some(k)) = (&inv, &r.object) {
                f(&ev.store, k);
            }
            self.audit.record(&ev);
        }
        recs.len()
    }
}
