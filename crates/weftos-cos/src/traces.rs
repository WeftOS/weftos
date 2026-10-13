//! Per-turn traces: how one turn was answered. The agent records one per
//! turn (`GET /v1/cos/traces`), the bridge pushes new ones to the dashboard
//! (`cos_traces`), and `weftos-cos mcp` returns them from `cos.say` and
//! `cos.trace`. A trace holds a digest of each tool call's arguments, never
//! the arguments.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Traces kept for `GET /v1/cos/traces`.
pub const TRACE_CAP: usize = 200;
/// Default and maximum `limit` of `GET /v1/cos/traces`.
pub const TRACES_DEFAULT_LIMIT: usize = 50;
pub const TRACES_MAX_LIMIT: usize = 200;
/// Most tool names listed as offered; the rest are counted in `offered_total`.
const MAX_OFFERED: usize = 64;
/// Most calls and gates listed per trace.
const MAX_CALLED: usize = 24;
const MAX_GATES: usize = 8;

/// One tool call in a turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TraceCall {
    pub name: String,
    /// First 16 hex digits of SHA-256 over the canonical argument JSON.
    pub args_digest: String,
    pub ok: bool,
    pub ms: u64,
}

/// How one turn was answered.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Trace {
    pub turn_id: String,
    /// The endpoint the turn belongs to.
    pub endpoint: String,
    /// Where the agent ran: always `local` for this host.
    pub runtime: String,
    /// The model that actually answered (after any fallback).
    pub model: String,
    /// `quick`, `plan`, or `confirmation` for the turn that answers a held action.
    pub role: String,
    pub tools_offered: Vec<String>,
    pub offered_total: usize,
    pub tools_called: Vec<TraceCall>,
    /// `<tool>:held`, `<tool>:confirmed` or `<tool>:cancelled`.
    pub gates: Vec<String>,
    /// Model calls made (tool rounds + the answer).
    pub rounds: usize,
    pub latency_ms: u64,
    /// Zero for a local model; no price table is applied to cloud models here.
    pub cost_usd: f64,
    /// Final message was an error report (`I hit a problem: ...`).
    pub failed: bool,
    /// RFC 3339, UTC, when the turn ended.
    pub ts: String,
}

/// Digest of tool arguments: stable across key order.
pub fn args_digest(args: &Value) -> String {
    let canonical = canonical(args);
    let sum = Sha256::digest(canonical.as_bytes());
    sum.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

fn canonical(v: &Value) -> String {
    match v {
        Value::Object(o) => {
            let mut keys: Vec<&String> = o.keys().collect();
            keys.sort();
            let parts: Vec<String> = keys.iter().map(|k| format!("{}:{}", serde_json::to_string(k).unwrap_or_default(), canonical(&o[*k]))).collect();
            format!("{{{}}}", parts.join(","))
        }
        Value::Array(a) => format!("[{}]", a.iter().map(canonical).collect::<Vec<_>>().join(",")),
        other => other.to_string(),
    }
}

/// Collects a turn's facts while it runs.
#[derive(Debug, Default)]
pub struct TraceBuilder {
    pub model: String,
    pub role: String,
    offered: Vec<String>,
    called: Vec<TraceCall>,
    gates: Vec<String>,
    pub rounds: usize,
    pub failed: bool,
    /// Staged file of the last successful `board.propose` this turn.
    pub proposal: Option<String>,
    /// A tool that really writes (claim, comment, kickoff) succeeded this turn.
    pub applied: bool,
}

impl TraceBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn offered(&mut self, names: impl IntoIterator<Item = String>) {
        self.offered = names.into_iter().collect();
    }

    pub fn called(&mut self, name: &str, args: &Value, ok: bool, ms: u64) {
        self.called.push(TraceCall { name: name.to_string(), args_digest: args_digest(args), ok, ms });
    }

    pub fn gate(&mut self, tool: &str, outcome: &str) {
        self.gates.push(format!("{tool}:{outcome}"));
    }

    pub fn finish(self, turn_id: &str, endpoint: &str, latency_ms: u64) -> Trace {
        let offered_total = self.offered.len();
        let mut called = self.called;
        called.truncate(MAX_CALLED);
        let mut gates = self.gates;
        gates.truncate(MAX_GATES);
        Trace {
            turn_id: turn_id.to_string(),
            endpoint: endpoint.to_string(),
            runtime: "local".into(),
            model: self.model,
            role: self.role,
            tools_offered: self.offered.into_iter().take(MAX_OFFERED).collect(),
            offered_total,
            tools_called: called,
            gates,
            rounds: self.rounds,
            latency_ms,
            cost_usd: 0.0,
            failed: self.failed,
            ts: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        }
    }
}

/// The last [`TRACE_CAP`] traces, oldest first.
#[derive(Debug, Default)]
pub struct TraceStore {
    items: VecDeque<Trace>,
}

impl TraceStore {
    pub fn push(&mut self, t: Trace) {
        // One trace per turn: a repeat replaces the earlier one.
        self.items.retain(|x| x.turn_id != t.turn_id);
        self.items.push_back(t);
        while self.items.len() > TRACE_CAP {
            self.items.pop_front();
        }
    }

    /// Traces whose `ts` is at or after `since` (RFC 3339 strings compare in
    /// time order when both are UTC with the same precision), oldest first,
    /// the newest `limit` of them. Consumers dedupe by `turn_id`.
    pub fn since(&self, since: Option<&str>, limit: usize) -> Vec<Trace> {
        let matching: Vec<&Trace> = self.items.iter().filter(|t| since.is_none_or(|s| t.ts.as_str() >= s)).collect();
        matching.iter().skip(matching.len().saturating_sub(limit)).map(|t| (*t).clone()).collect()
    }

    pub fn get(&self, turn_id: &str) -> Option<Trace> {
        self.items.iter().find(|t| t.turn_id == turn_id).cloned()
    }
}

/// Whether `s` is an RFC 3339 timestamp.
pub fn valid_since(s: &str) -> bool {
    chrono::DateTime::parse_from_rfc3339(s).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn digest_ignores_key_order_and_hides_values() {
        let a = args_digest(&json!({"q": "secret thing", "n": 3}));
        let b = args_digest(&json!({"n": 3, "q": "secret thing"}));
        assert_eq!(a, b);
        assert_eq!(a.len(), 16);
        assert!(!a.contains("secret"));
        assert_ne!(a, args_digest(&json!({"q": "other", "n": 3})));
    }

    #[test]
    fn store_keeps_one_per_turn_and_caps() {
        let mut s = TraceStore::default();
        for i in 0..(TRACE_CAP + 5) {
            s.push(TraceBuilder::new().finish(&format!("t{i}"), "host", 1));
        }
        assert_eq!(s.since(None, TRACES_MAX_LIMIT).len(), TRACE_CAP);
        assert!(s.get("t0").is_none());
        s.push(TraceBuilder::new().finish("t204", "host", 9));
        assert_eq!(s.get("t204").unwrap().latency_ms, 9);
        assert_eq!(s.since(None, 1000).iter().filter(|t| t.turn_id == "t204").count(), 1);
    }
}
