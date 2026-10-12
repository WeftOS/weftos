//! The per-exposure activity log (ADR-120 section 5): time, caller node and
//! user label, exposure id, bytes in and out, duration, outcome. Never
//! request bodies. Records are hash-chained (each carries the previous
//! record's SHA-256), so a removed or edited line breaks [`ActivityLog::verify`].
//! Retention drops the oldest records and keeps the dropped one's hash as the
//! new chain base, so the retained part still verifies.
//!
//! Persistence is a JSON-lines file: an optional header
//! `{"base":"<hash>"}` then one record per line.

use std::collections::{BTreeMap, VecDeque};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Hash the first record chains from.
pub const GENESIS: &str = "0000000000000000000000000000000000000000000000000000000000000000";
/// Longest accepted outcome text.
const MAX_OUTCOME: usize = 96;

/// How long and how much to keep. Defaults pending the owner (ADR-120).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retention {
    /// Oldest record kept, in milliseconds.
    pub max_age_ms: u64,
    /// Serialized size kept, in bytes.
    pub max_bytes: u64,
}

impl Default for Retention {
    fn default() -> Self {
        Self { max_age_ms: 14 * 24 * 3600 * 1000, max_bytes: 50 * 1024 * 1024 }
    }
}

/// What a caller reports about one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewActivity {
    /// Exposure id.
    pub exposure: String,
    /// Calling node (the verified peer id, or `local`).
    pub caller_node: String,
    /// User label when known.
    pub caller_user: Option<String>,
    /// Request bytes.
    pub bytes_in: u64,
    /// Response bytes.
    pub bytes_out: u64,
    /// Wall time.
    pub duration_ms: u64,
    /// `ok`, `refused:<reason>` or `error:<reason>`.
    pub outcome: String,
}

/// One chained record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityRecord {
    /// Position in the chain.
    pub seq: u64,
    /// Unix milliseconds.
    pub ts_ms: u64,
    /// Exposure id.
    pub exposure: String,
    /// Calling node.
    pub caller_node: String,
    /// User label when known.
    pub caller_user: Option<String>,
    /// Request bytes.
    pub bytes_in: u64,
    /// Response bytes.
    pub bytes_out: u64,
    /// Wall time.
    pub duration_ms: u64,
    /// Outcome text.
    pub outcome: String,
    /// Previous record's hash.
    pub prev: String,
    /// This record's hash.
    pub hash: String,
}

impl ActivityRecord {
    fn digest(&self) -> String {
        let user = self.caller_user.as_deref().unwrap_or("");
        let text = format!(
            "{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}",
            self.prev, self.seq, self.ts_ms, self.exposure, self.caller_node, user, self.bytes_in, self.bytes_out, self.duration_ms, self.outcome
        );
        hex::encode(Sha256::digest(text.as_bytes()))
    }

    /// True if this record failed (`refused:` or `error:`).
    pub fn failed(&self) -> bool {
        self.outcome != "ok"
    }
}

/// Per-exposure counts over a window.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Counts {
    /// Requests in the window.
    pub total: u64,
    /// Of which refused or failed.
    pub failed: u64,
    /// Time of the last request ever kept.
    pub last_ts_ms: Option<u64>,
}

#[derive(Default)]
struct Inner {
    records: VecDeque<ActivityRecord>,
    bytes: u64,
    /// Hash the first retained record chains from.
    base: String,
    next_seq: u64,
}

impl Inner {
    fn tip(&self) -> &str {
        self.records.back().map_or(self.base.as_str(), |r| r.hash.as_str())
    }
}

/// The log.
pub struct ActivityLog {
    inner: Mutex<Inner>,
    path: Option<PathBuf>,
    retention: Retention,
}

fn line_len(r: &ActivityRecord) -> u64 {
    serde_json::to_string(r).map_or(0, |s| s.len() as u64 + 1)
}

fn clean(s: &str, max: usize) -> String {
    s.chars().filter(|c| !c.is_control()).take(max).collect()
}

impl ActivityLog {
    /// In-memory log.
    pub fn memory(retention: Retention) -> Self {
        Self { inner: Mutex::new(Inner { base: GENESIS.into(), ..Inner::default() }), path: None, retention }
    }

    /// Log persisted at `path`. A file whose chain does not verify is
    /// refused (`Err`), never silently replaced.
    pub fn open(path: &Path, retention: Retention, now_ms: u64) -> Result<Self, String> {
        let mut inner = Inner { base: GENESIS.into(), ..Inner::default() };
        if path.exists() {
            let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
            for (i, line) in text.lines().filter(|l| !l.trim().is_empty()).enumerate() {
                if i == 0
                    && let Ok(h) = serde_json::from_str::<serde_json::Value>(line)
                    && let Some(b) = h.get("base").and_then(|b| b.as_str())
                    && h.get("hash").is_none()
                {
                    inner.base = b.to_owned();
                    continue;
                }
                let r: ActivityRecord = serde_json::from_str(line).map_err(|e| format!("{}: line {}: {e}", path.display(), i + 1))?;
                inner.bytes += line_len(&r);
                inner.records.push_back(r);
            }
            Self::check(&inner).map_err(|e| format!("{}: {e}", path.display()))?;
            inner.next_seq = inner.records.back().map_or(0, |r| r.seq + 1);
        }
        let log = Self { inner: Mutex::new(inner), path: Some(path.to_owned()), retention };
        log.prune(now_ms);
        Ok(log)
    }

    fn check(inner: &Inner) -> Result<(), String> {
        let mut prev = inner.base.clone();
        for r in &inner.records {
            if r.prev != prev || r.digest() != r.hash {
                return Err(format!("chain broken at seq {}", r.seq));
            }
            prev = r.hash.clone();
        }
        Ok(())
    }

    /// Check the whole retained chain.
    pub fn verify(&self) -> Result<(), String> {
        Self::check(&self.inner.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// Append a record for a request at `ts_ms`, then apply retention.
    pub fn append(&self, a: NewActivity, ts_ms: u64) -> ActivityRecord {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut r = ActivityRecord {
            seq: g.next_seq,
            ts_ms,
            exposure: clean(&a.exposure, 96),
            caller_node: clean(&a.caller_node, 128),
            caller_user: a.caller_user.map(|u| clean(&u, 64)).filter(|u| !u.is_empty()),
            bytes_in: a.bytes_in,
            bytes_out: a.bytes_out,
            duration_ms: a.duration_ms,
            outcome: clean(&a.outcome, MAX_OUTCOME),
            prev: g.tip().to_owned(),
            hash: String::new(),
        };
        r.hash = r.digest();
        g.next_seq += 1;
        g.bytes += line_len(&r);
        g.records.push_back(r.clone());
        if let Some(p) = &self.path {
            let line = serde_json::to_string(&r).unwrap_or_default();
            let res = std::fs::OpenOptions::new().create(true).append(true).open(p).and_then(|mut f| writeln!(f, "{line}"));
            if let Err(e) = res {
                tracing::warn!(path = %p.display(), error = %e, "exposure activity log write failed");
            }
        }
        let dropped = Self::prune_locked(&mut g, &self.retention, ts_ms);
        if dropped {
            self.rewrite(&g);
        }
        r
    }

    fn prune_locked(g: &mut Inner, ret: &Retention, now_ms: u64) -> bool {
        let mut dropped = false;
        while let Some(front) = g.records.front() {
            let old = now_ms.saturating_sub(front.ts_ms) > ret.max_age_ms;
            // Always keep the newest record, even if it alone exceeds the size.
            if !(old || (g.bytes > ret.max_bytes && g.records.len() > 1)) {
                break;
            }
            if let Some(r) = g.records.pop_front() {
                g.bytes = g.bytes.saturating_sub(line_len(&r));
                g.base = r.hash;
                dropped = true;
            }
        }
        dropped
    }

    /// Drop what retention no longer allows; rewrites the file if anything went.
    pub fn prune(&self, now_ms: u64) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if Self::prune_locked(&mut g, &self.retention, now_ms) {
            self.rewrite(&g);
        }
    }

    fn rewrite(&self, g: &Inner) {
        let Some(p) = &self.path else { return };
        let mut out = format!("{{\"base\":\"{}\"}}\n", g.base);
        for r in &g.records {
            out.push_str(&serde_json::to_string(r).unwrap_or_default());
            out.push('\n');
        }
        let tmp = p.with_extension("jsonl.tmp");
        if let Err(e) = std::fs::write(&tmp, out).and_then(|()| std::fs::rename(&tmp, p)) {
            tracing::warn!(path = %p.display(), error = %e, "exposure activity log rewrite failed");
        }
    }

    /// The newest `limit` records, oldest first, optionally for one exposure.
    pub fn query(&self, exposure: Option<&str>, limit: usize) -> Vec<ActivityRecord> {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut v: Vec<ActivityRecord> =
            g.records.iter().rev().filter(|r| exposure.is_none_or(|e| r.exposure == e)).take(limit).cloned().collect();
        v.reverse();
        v
    }

    /// Per-exposure counts over the last `window_ms` before `now_ms`.
    pub fn counts(&self, now_ms: u64, window_ms: u64) -> BTreeMap<String, Counts> {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut m: BTreeMap<String, Counts> = BTreeMap::new();
        for r in &g.records {
            let c = m.entry(r.exposure.clone()).or_default();
            c.last_ts_ms = Some(c.last_ts_ms.map_or(r.ts_ms, |t| t.max(r.ts_ms)));
            if now_ms.saturating_sub(r.ts_ms) <= window_ms {
                c.total += 1;
                c.failed += u64::from(r.failed());
            }
        }
        m
    }

    /// Records held.
    pub fn len(&self) -> usize {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).records.len()
    }

    /// True when no record is held.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
