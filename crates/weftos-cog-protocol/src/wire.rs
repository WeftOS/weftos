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

/// One request line for a method other than [`crate::CHECK_RUN_METHOD`].
///
/// [`request_json`] stays the check-run builder. This one takes params the
/// caller already built, still sorted, still without auth.
pub fn method_request_json(id: &str, method: &str, params: &Value) -> String {
    serde_json::to_string(&json!({
        "method": method,
        "params": params,
        "id": id,
        "proto": CLIENT_PROTO,
    }))
    .expect("method request json")
}

/// [`method_request_json`] with an `auth` field. `cog.licence.import` sends `admin`.
pub fn authed_request_json(id: &str, method: &str, params: &Value, auth: &str) -> String {
    serde_json::to_string(&json!({
        "auth": auth,
        "method": method,
        "params": params,
        "id": id,
        "proto": CLIENT_PROTO,
    }))
    .expect("authed request json")
}

/// Fields of a `cog.licence.status` result. Nulls are kept.
pub struct StatusParts<'a> {
    /// Approval rows. Empty when the daemon has no approval store.
    pub approvals: &'a Value,
    /// Held binding, or none.
    pub binding: Option<&'a Value>,
    /// True when [`crate`] binding rules say a binding is in effect.
    pub binding_in_effect: bool,
    /// Grant rows.
    pub grants: &'a Value,
    /// How many times the revocation list has changed, or 0 when none is attached.
    pub list_generation: u64,
    /// Local mesh id, hex, or none.
    pub mesh_id: Option<&'a str>,
    /// Why the revocation list could not be read.
    pub revocations_error: Option<&'a str>,
    /// Artifact-hash revocations currently listed.
    pub revoked_artifacts: u64,
    /// Why the grant or approval store is poisoned.
    pub store_error: Option<&'a str>,
}

/// `cog.licence.status` result, including `protocol` and nulls.
pub fn status_value(parts: &StatusParts<'_>) -> Value {
    json!({
        "approvals": parts.approvals,
        "binding": parts.binding,
        "binding_in_effect": parts.binding_in_effect,
        "grants": parts.grants,
        "list_generation": parts.list_generation,
        "mesh_id": parts.mesh_id,
        "protocol": PROTOCOL,
        "revocations_error": parts.revocations_error,
        "revoked_artifacts": parts.revoked_artifacts,
        "store_error": parts.store_error,
    })
}

/// [`status_value`] as one JSON body.
pub fn status_json(parts: &StatusParts<'_>) -> String {
    serde_json::to_string(&status_value(parts)).expect("status json")
}

/// `cog.licence.claims` result.
pub fn claims_value(claims: bool) -> Value {
    json!({
        "claims": claims,
        "protocol": PROTOCOL,
    })
}

/// [`claims_value`] as one JSON body.
pub fn claims_json(claims: bool) -> String {
    serde_json::to_string(&claims_value(claims)).expect("claims json")
}

/// `cog.licence.revoked` result.
pub fn revoked_value(revoked: bool, list_generation: u64) -> Value {
    json!({
        "list_generation": list_generation,
        "protocol": PROTOCOL,
        "revoked": revoked,
    })
}

/// [`revoked_value`] as one JSON body.
pub fn revoked_json(revoked: bool, list_generation: u64) -> String {
    serde_json::to_string(&revoked_value(revoked, list_generation)).expect("revoked json")
}

/// One line of a `cog.licence.import` result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportOutcome {
    /// `binding`, `grant`, `approval`, or `revocation`.
    pub kind: &'static str,
    /// `applied`, `duplicate`, `ignored`, or `refused`.
    pub outcome: &'static str,
    /// Present only when `outcome` is `refused`.
    pub error: Option<String>,
}

/// `cog.licence.import` result. `applied_unsaved` is `applied`: the exchange
/// folds both store outcomes into one receipt before this line is built.
pub fn import_result_value(lines: &[ImportOutcome]) -> Value {
    let lines: Vec<Value> = lines
        .iter()
        .map(|line| {
            let mut obj = json!({
                "kind": line.kind,
                "outcome": line.outcome,
            });
            if let Some(error) = &line.error {
                obj.as_object_mut().expect("line object").insert("error".into(), json!(error));
            }
            obj
        })
        .collect();
    json!({
        "lines": lines,
        "protocol": PROTOCOL,
    })
}

/// [`import_result_value`] as one JSON body.
pub fn import_result_json(lines: &[ImportOutcome]) -> String {
    serde_json::to_string(&import_result_value(lines)).expect("import result json")
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

    #[test]
    fn licence_queries_match_their_golden_vectors() {
        let sha = "ab".repeat(32);
        let blake = "cd".repeat(32);
        let binding = json!({"payload": "{}", "public_key": "aa", "signature": "bb"});
        let import_params = json!({"protocol": PROTOCOL, "binding": binding});
        assert_eq!(
            authed_request_json("cog-licence-import-1", crate::IMPORT_METHOD, &import_params, "admin"),
            golden("licence-import-request")
        );
        assert_eq!(
            method_request_json("cog-licence-status-1", crate::STATUS_METHOD, &json!({"protocol": PROTOCOL})),
            golden("licence-status-request")
        );
        assert_eq!(
            method_request_json(
                "cog-licence-claims-1",
                crate::CLAIMS_METHOD,
                &json!({"protocol": PROTOCOL, "sha256": sha, "blake3": blake})
            ),
            golden("licence-claims-request")
        );
        assert_eq!(
            method_request_json(
                "cog-licence-revoked-1",
                crate::REVOKED_METHOD,
                &json!({"protocol": PROTOCOL, "blake3": blake})
            ),
            golden("licence-revoked-request")
        );
        let approvals = json!([]);
        let grants = json!([]);
        let status = StatusParts {
            approvals: &approvals,
            binding: None,
            binding_in_effect: false,
            grants: &grants,
            list_generation: 0,
            mesh_id: None,
            revocations_error: None,
            revoked_artifacts: 0,
            store_error: None,
        };
        assert_eq!(status_json(&status), golden("licence-status-empty"));
        assert_eq!(claims_json(false), golden("licence-claims-false"));
        assert_eq!(revoked_json(false, 0), golden("licence-revoked-false"));
        let line = ImportOutcome { kind: "binding", outcome: "applied", error: None };
        assert_eq!(import_result_json(&[line]), golden("licence-import-result"));
    }

    #[test]
    fn manifest_pins_every_golden_file() {
        use sha2::{Digest, Sha256};
        let dir = format!("{}/testdata", env!("CARGO_MANIFEST_DIR"));
        let manifest = std::fs::read_to_string(format!("{dir}/MANIFEST.sha256")).unwrap();
        let mut listed = std::collections::BTreeSet::new();
        for line in manifest.lines().filter(|line| !line.is_empty()) {
            let (hash, name) = line.split_once("  ").unwrap_or_else(|| panic!("manifest line {line}"));
            let bytes = std::fs::read(format!("{dir}/{name}")).unwrap_or_else(|err| panic!("{name}: {err}"));
            let digest = Sha256::digest(&bytes);
            let got: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
            assert_eq!(got, hash, "{name}");
            listed.insert(name.to_string());
        }
        for entry in std::fs::read_dir(&dir).unwrap() {
            let name = entry.unwrap().file_name().into_string().unwrap();
            if name == "MANIFEST.sha256" {
                continue;
            }
            assert!(listed.contains(&name), "{name} is missing from MANIFEST.sha256");
        }
    }
}
