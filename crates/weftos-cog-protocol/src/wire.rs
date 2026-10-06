//! Canonical `weftos.cog.v1` JSON. Key order inside a [`serde_json::Value`]
//! follows `serde_json`'s map (sorted). Envelope structs follow the daemon
//! `Response` field order so a golden file matches what the socket carries.

use serde::Serialize;
use serde_json::{Value, json};

use crate::{CheckRunResult, HostEvent, PROTOCOL, CHECK_RUN_METHOD};

/// RPC `proto` this client speaks. The daemon's current protocol is 1.
pub const CLIENT_PROTO: u32 = 1;

/// How long a check waits for the daemon when the caller does not say.
pub const DEFAULT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Why a check did not return a verdict. Grant refusals stay in [`Denial`](CallError::Denial).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CallError {
    /// The socket could not be reached, or the daemon closed before a line.
    #[error("daemon unavailable: {0}")]
    DaemonUnavailable(String),
    /// The reply was not one JSON object of the expected shape.
    #[error("malformed reply: {0}")]
    MalformedReply(String),
    /// The daemon accepted the connection and did not answer in time.
    #[error("daemon timed out")]
    Timeout,
    /// `proto` or `result.protocol` is not `weftos.cog.v1` / protocol 1.
    #[error("version mismatch: {0}")]
    VersionMismatch(String),
    /// The daemon refused the run. `code` is a [`crate::RefusalCode`] spelling.
    #[error("{code}: {message}")]
    Denial {
        /// Wire refusal code.
        code: crate::RefusalCode,
        /// Daemon error string.
        message: String,
    },
}

impl CallError {
    /// Stable code for logs and [`crate`] host start refusals.
    pub fn code(&self) -> &'static str {
        match self {
            Self::DaemonUnavailable(_) => "daemon_unavailable",
            Self::MalformedReply(_) => "malformed_reply",
            Self::Timeout => "timeout",
            Self::VersionMismatch(_) => "version_mismatch",
            Self::Denial { code, .. } => code.as_str(),
        }
    }
}

/// One request line, without the trailing newline the socket adds.
pub fn request_json(id: &str, params: &crate::CheckRunParams) -> String {
    serde_json::to_string(&json!({
        "method": CHECK_RUN_METHOD,
        "params": {
            "protocol": PROTOCOL,
            "cog_id": params.cog_id,
            "version": params.version,
            "sha256": params.sha256,
            "blake3": params.blake3,
        },
        "id": id,
        "proto": CLIENT_PROTO,
    }))
    .expect("request json")
}

/// The `result` object, including `protocol`.
pub fn result_value(result: &CheckRunResult) -> Value {
    match result {
        CheckRunResult::NotSeedBound => json!({
            "protocol": PROTOCOL,
            "verdict": "not_seed_bound",
        }),
        CheckRunResult::Permit { grant_id, approval_id, blake3 } => json!({
            "protocol": PROTOCOL,
            "verdict": "permit",
            "grant_id": grant_id,
            "approval_id": approval_id,
            "blake3": blake3,
        }),
    }
}

/// [`result_value`] as one JSON line body.
pub fn result_json(result: &CheckRunResult) -> String {
    serde_json::to_string(&result_value(result)).expect("result json")
}

#[derive(Serialize)]
struct OkEnv<'a> {
    ok: bool,
    result: &'a Value,
    id: &'a str,
}

#[derive(Serialize)]
struct ErrEnv<'a> {
    ok: bool,
    error: &'a str,
    error_kind: &'a str,
    id: &'a str,
}

#[derive(Serialize)]
struct MismatchEnv<'a> {
    ok: bool,
    error: &'a str,
    error_kind: &'a str,
    id: &'a str,
    data: &'a Value,
}

/// Success envelope in daemon `Response` field order.
pub fn success_json(id: &str, result: &CheckRunResult) -> String {
    let result = result_value(result);
    serde_json::to_string(&OkEnv { ok: true, result: &result, id }).expect("success json")
}

/// Refusal envelope in daemon `Response` field order.
pub fn refusal_json(id: &str, error_kind: &str, error: &str) -> String {
    serde_json::to_string(&ErrEnv { ok: false, error, error_kind, id }).expect("refusal json")
}

/// A frozen `proto_mismatch` body. The real daemon fills sha and version;
/// this vector locks the shape the client classifies.
pub fn proto_mismatch_json(id: &str) -> String {
    let data = json!({
        "client": {"proto": 99},
        "daemon": {"proto": 1, "min": 1, "version": "0.8.3", "sha": "abc1234"},
    });
    let error = "protocol mismatch: client speaks 99, daemon accepts 1..=1 (abc1234)";
    serde_json::to_string(&MismatchEnv {
        ok: false,
        error,
        error_kind: "proto_mismatch",
        id,
        data: &data,
    })
    .expect("mismatch json")
}

/// Host event JSON. The daemon does not emit these; Cog Host records them.
pub fn event_json(event: &HostEvent) -> String {
    serde_json::to_string(event).expect("event json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CheckRunParams, HostEvent, RefusalCode};

    fn sample() -> CheckRunParams {
        CheckRunParams::new("ld2450-radar", "0.1.0", "ab".repeat(32), "cd".repeat(32)).unwrap()
    }

    fn golden(name: &str) -> String {
        let path = format!("{}/testdata/{name}.json", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(path).unwrap().trim_end().to_string()
    }

    #[test]
    fn golden_request_verdict_refusal_and_events() {
        let params = sample();
        assert_eq!(request_json("cog-check-1", &params), golden("request"));
        assert_eq!(result_json(&CheckRunResult::NotSeedBound), golden("verdict-not-seed-bound"));
        let permit = CheckRunResult::Permit {
            grant_id: "g1".into(),
            approval_id: "a1".into(),
            blake3: params.blake3.clone(),
        };
        assert_eq!(result_json(&permit), golden("verdict-permit"));
        assert_eq!(
            refusal_json("cog-check-1", "hash_revoked", "the binary's hash is revoked"),
            golden("refusal-hash-revoked")
        );
        assert_eq!(
            event_json(&HostEvent::RunPermitted {
                cog_id: "ld2450-radar".into(),
                version: "0.1.0".into(),
                grant_id: "g1".into(),
            }),
            golden("event-run-permitted")
        );
        assert_eq!(
            event_json(&HostEvent::RunRefused {
                cog_id: "ld2450-radar".into(),
                version: "0.1.0".into(),
                code: RefusalCode::HashRevoked,
            }),
            golden("event-run-refused")
        );
        assert_eq!(proto_mismatch_json("cog-check-1"), golden("proto-mismatch"));
    }
}
