//! RustFS audit stream, log and bucket notifications into the activity log, and the rule that
//! turns a run of refusals into a suspended key.

use std::collections::{BTreeMap, VecDeque};

use super::StoreService;
use crate::access::Principal;
use crate::audit::{self, AuditEvent, Outcome, Raw, Source};
use crate::name::StoreName;

/// When refusals reported by RustFS suspend a principal: `max` refusals within `window_ms`.
/// A suspended principal's key is disabled through [`StoreService::suspend_principal`] (chain event,
/// activity line, backend key off), and stays off until `resume_principal`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DenialRule {
    pub max: u32,
    pub window_ms: u64,
}

impl Default for DenialRule {
    /// 20 refusals in a minute: a key that is wrong that often is broken or stolen, not used.
    fn default() -> Self {
        Self { max: 20, window_ms: 60_000 }
    }
}

/// Refusal times per principal, newest last, and the rule they are counted against.
#[derive(Debug)]
pub(super) struct Denials {
    rule: Option<DenialRule>,
    seen: BTreeMap<String, VecDeque<u64>>,
}

impl Default for Denials {
    fn default() -> Self {
        Self { rule: Some(DenialRule::default()), seen: BTreeMap::new() }
    }
}

impl StoreService {
    // ---- RustFS audit, log and notifications --------------------------------------------------

    /// Set (or, with `None`, turn off) the rule that suspends a principal after repeated refusals.
    /// On by default: [`DenialRule::default`].
    pub fn set_denial_rule(&self, rule: Option<DenialRule>) {
        let mut d = self.denials.lock().unwrap_or_else(|e| e.into_inner());
        d.rule = rule;
        d.seen.clear();
    }

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

    /// Record one record RustFS reported, and count it against the suspension rule if it is a
    /// refusal for a principal's own key.
    fn take(&self, raw: Raw, source: Source) {
        let refused = raw.status == Some(403);
        let who = self.resolve(&raw).0;
        if let Some(ev) = self.event_from(raw.clone(), source) {
            self.audit.record(&ev);
        } else if refused && raw.bucket.is_empty() {
            // A refusal before RustFS knew the caller (bad signature, unknown key): no store, no
            // principal to attribute it to, but it is kept in the activity log.
            self.audit.record(&AuditEvent {
                ts_ms: self.now(),
                principal: "(unknown key)".into(),
                store: "(none)".into(),
                bucket: String::new(),
                op: "Authenticate".into(),
                key: None,
                bytes: 0,
                outcome: Outcome::Refused(raw.reason.clone().unwrap_or_else(|| "403".into())),
                source,
            });
        }
        if refused && let Some(p) = who.as_deref().and_then(|p| Principal::parse(p).ok()) {
            self.count_refusal(&p, raw.reason.as_deref());
        }
    }

    /// One refusal of `p`'s own key: when the rule's count is reached inside its window, suspend
    /// `p` (once; a principal already suspended is left alone) and start counting afresh.
    fn count_refusal(&self, p: &Principal, reason: Option<&str>) {
        let now = self.now();
        let hit = {
            let mut d = self.denials.lock().unwrap_or_else(|e| e.into_inner());
            let Some(rule) = d.rule else { return };
            let q = d.seen.entry(p.to_string()).or_default();
            q.push_back(now);
            while q.front().is_some_and(|t| now.saturating_sub(*t) > rule.window_ms) {
                q.pop_front();
            }
            let hit = q.len() as u64 >= u64::from(rule.max);
            if hit {
                q.clear();
            }
            hit.then_some(rule)
        };
        let Some(rule) = hit else { return };
        if self.lock().suspended.contains(p) {
            return;
        }
        let why = format!("rule: {} refusals by RustFS within {} s (last: {})", rule.max, rule.window_ms / 1000, reason.unwrap_or("403"));
        if let Err(e) = self.suspend_principal(p, &why) {
            // The suspension is saved even when the backend push failed (see `change`); say so.
            let stores = self.lock().grants.for_principal(p).into_iter().filter_map(|g| StoreName::parse(&g.store).ok()).collect::<Vec<_>>();
            for s in stores {
                self.emit(&p.to_string(), &s, "Suspend", None, 0, Outcome::Error(e.to_string()), Source::Governed);
            }
        }
    }

    /// One line of RustFS's audit stream into the activity log. False if it was not an audit line.
    pub fn ingest_audit_line(&self, line: &str) -> bool {
        let Some(r) = audit::parse_audit(line) else { return false };
        self.take(r, Source::RustfsAudit);
        true
    }

    /// One line of RustFS's JSON log: refusals only. Both kinds RustFS writes are read: an
    /// `s3_authorization_denied` (a known key, denied by policy) and a `SignatureDoesNotMatch` or
    /// `InvalidAccessKeyId` (refused before the caller was known). False for any other line.
    pub fn ingest_log_line(&self, line: &str) -> bool {
        let Some(r) = audit::parse_denial(line).or_else(|| audit::parse_auth_failure(line)) else { return false };
        self.take(r, Source::RustfsLog);
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
