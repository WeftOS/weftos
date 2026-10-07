//! `cog.check_run` for `weftos.cog.v1`.
//!
//! Cog Host asks this before a Cognitum-origin start. The grant decision
//! stays in the kernel store this daemon already owns. This module does not
//! open a second store. A missing runtime is `binding_inactive`. A missing
//! socket never reaches here (`daemon_unavailable` is the client's code).

use std::sync::Arc;

use clawft_kernel::licence::{ApprovalStore, RunPermit, RunRefusal, RunRequest, RunVerdict, check_run};
use clawft_platform::NativePlatform;
use clawft_rpc::Response;
use serde_json::Value;
use tokio::sync::RwLock;

use crate::licence_boot::LicenceRuntime;
use crate::rpc_ext::{ExtCall, ExtFuture};
use weftos_cog_protocol::{
    result_value, CheckRunParams, CheckRunResult, RefusalCode, CHECK_RUN_METHOD, PROTOCOL,
};

/// The one method this module serves.
pub const METHOD: &str = CHECK_RUN_METHOD;

/// Same list, so the licence-verb census can union it.
pub const METHODS: &[&str] = &[METHOD];

/// True when `method` is [`METHOD`].
pub fn handles(method: &str) -> bool {
    method == METHOD
}

/// Extension-route entry. The role gate has already run.
pub fn handle(call: ExtCall) -> ExtFuture {
    Box::pin(async move { decide(&call.params) })
}

/// Direct dispatcher entry for callers that do not go through extension routes.
pub async fn dispatch(
    method: &str,
    params: Value,
    _kernel: Arc<RwLock<clawft_kernel::boot::Kernel<NativePlatform>>>,
) -> Response {
    if !handles(method) {
        return Response::error(format!("unknown method: {method}"));
    }
    decide(&params)
}

fn decide(params: &Value) -> Response {
    decide_with(crate::licence_boot::holder_refusal(), crate::licence_boot::runtime(), params)
}

fn decide_with(holder: Option<String>, runtime: Option<Arc<LicenceRuntime>>, params: &Value) -> Response {
    if let Some(why) = holder {
        return Response::error_with_kind("not_holder", why);
    }
    let Some(runtime) = runtime else {
        return Response::error_with_kind(
            "binding_inactive",
            "the licence runtime is not initialised on this node",
        );
    };
    let approvals = crate::workload_place_rpc::licence_exchange().map(|ex| Arc::clone(ex.approvals()));
    answer(&runtime, approvals.as_deref(), params)
}

pub(crate) fn answer(runtime: &LicenceRuntime, approvals: Option<&ApprovalStore>, params: &Value) -> Response {
    let parsed = match classify(params) {
        Ok(parsed) => parsed,
        Err(response) => return response,
    };
    let req = RunRequest {
        cog_id: &parsed.cog_id,
        version: &parsed.version,
        sha256: &parsed.sha256,
        blake3: &parsed.blake3,
    };
    let list_error = runtime.store().revocation_list_error();
    let verdict = check_run(runtime.store().as_ref(), approvals, &req);
    match list_error.as_deref() {
        None => map_verdict(&parsed.blake3, verdict),
        Some(err) => map_verdict_with_list(&parsed.blake3, verdict, Some(err)),
    }
}

fn classify(params: &Value) -> Result<CheckRunParams, Response> {
    let protocol = params.get("protocol").and_then(Value::as_str).unwrap_or("");
    if protocol != PROTOCOL {
        return Err(Response::error_with_kind(
            "version_mismatch",
            format!("params protocol is {protocol}"),
        ));
    }
    let field = |name: &str| params.get(name).and_then(Value::as_str).unwrap_or("");
    CheckRunParams::new(field("cog_id"), field("version"), field("sha256"), field("blake3")).map_err(|err| {
        Response::error_with_kind("malformed_reply", err.to_string())
    })
}

pub(crate) fn map_verdict(blake3: &str, verdict: Result<RunVerdict, RunRefusal>) -> Response {
    map_verdict_with_list(blake3, verdict, None)
}

/// [`map_verdict`], refusing a permit when the revocation list cannot be read.
///
/// The refusal is checked only on the permit path, after the BLAKE3 match.
/// The message is the full binding-inactive sentence, so Cog Host does not
/// wrap it a second time.
pub(crate) fn map_verdict_with_list(
    blake3: &str,
    verdict: Result<RunVerdict, RunRefusal>,
    list_error: Option<&str>,
) -> Response {
    match verdict {
        Ok(RunVerdict::NotSeedBound) => Response::success(result_value(&CheckRunResult::NotSeedBound)),
        Ok(RunVerdict::Permit(RunPermit { grant_id, approval_id, blake3: permit_blake3 })) => {
            if permit_blake3 != blake3 {
                return Response::error_with_kind("malformed_reply", "permit blake3 does not match the request");
            }
            if let Some(err) = list_error {
                return Response::error_with_kind(
                    "binding_inactive",
                    format!("no Seed binding is in effect on this node (revocation list unreadable: {err})"),
                );
            }
            Response::success(result_value(&CheckRunResult::Permit {
                grant_id,
                approval_id,
                blake3: permit_blake3,
            }))
        }
        Err(err) => {
            let kind = RefusalCode::parse(err.code()).map(|code| code.as_str()).unwrap_or("malformed_reply");
            Response::error_with_kind(kind, err.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clawft_kernel::licence::{RunPermit, RunRefusal, RunVerdict};
    use serde_json::json;

    fn golden(name: &str) -> Value {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join(format!("../weftos-cog-protocol/testdata/{name}.json"));
        let text = std::fs::read_to_string(&path).unwrap_or_else(|err| panic!("{}: {err}", path.display()));
        serde_json::from_str(text.trim_end()).unwrap()
    }

    fn sample_params() -> Value {
        json!({
            "protocol": PROTOCOL,
            "cog_id": "ld2450-radar",
            "version": "0.1.0",
            "sha256": "ab".repeat(32),
            "blake3": "cd".repeat(32),
        })
    }

    #[test]
    fn proto_mismatch_kind_matches_the_daemon_constant() {
        assert_eq!(clawft_rpc::handshake::PROTO_MISMATCH_KIND, "proto_mismatch");
    }

    #[test]
    fn a_missing_runtime_is_binding_inactive_and_a_holder_refusal_is_not_holder() {
        let missing = decide_with(None, None, &sample_params());
        assert_eq!(missing.error_kind.as_deref(), Some("binding_inactive"));
        assert_eq!(
            missing.error.as_deref(),
            Some("the licence runtime is not initialised on this node")
        );

        let held = decide_with(Some("not the holder".into()), None, &sample_params());
        assert_eq!(held.error_kind.as_deref(), Some("not_holder"));
        assert_eq!(held.error.as_deref(), Some("not the holder"));
        assert!(RefusalCode::parse("not_holder").is_some());
    }

    #[test]
    fn a_wrong_protocol_is_version_mismatch_and_a_bad_hash_is_malformed() {
        let mut params = sample_params();
        params["protocol"] = json!("weftos.cog.v0");
        let err = classify(&params).unwrap_err();
        assert_eq!(err.error_kind.as_deref(), Some("version_mismatch"));

        let mut params = sample_params();
        params["sha256"] = json!("AB");
        let err = classify(&params).unwrap_err();
        assert_eq!(err.error_kind.as_deref(), Some("malformed_reply"));
    }

    #[test]
    fn not_seed_bound_and_permit_match_the_golden_results() {
        let blake3 = "cd".repeat(32);
        let unbound = map_verdict(&blake3, Ok(RunVerdict::NotSeedBound));
        assert_eq!(unbound.result.unwrap(), golden("verdict-not-seed-bound"));

        let permit = map_verdict(
            &blake3,
            Ok(RunVerdict::Permit(RunPermit {
                grant_id: "g1".into(),
                approval_id: "a1".into(),
                blake3: blake3.clone(),
            })),
        );
        assert_eq!(permit.result.unwrap(), golden("verdict-permit"));

        let forged = map_verdict(
            &blake3,
            Ok(RunVerdict::Permit(RunPermit {
                grant_id: "g1".into(),
                approval_id: "a1".into(),
                blake3: "ab".repeat(32),
            })),
        );
        assert_eq!(forged.error_kind.as_deref(), Some("malformed_reply"));
    }

    #[test]
    fn a_permit_is_refused_when_the_revocation_list_is_unreadable() {
        let blake3 = "cd".repeat(32);
        let permit = Ok(RunVerdict::Permit(RunPermit {
            grant_id: "g1".into(),
            approval_id: "a1".into(),
            blake3: blake3.clone(),
        }));
        let response = map_verdict_with_list(&blake3, permit, Some("disk"));
        assert_eq!(response.error_kind.as_deref(), Some("binding_inactive"));
        assert_eq!(
            response.error.as_deref(),
            Some("no Seed binding is in effect on this node (revocation list unreadable: disk)")
        );
        let unbound = map_verdict_with_list(&blake3, Ok(RunVerdict::NotSeedBound), Some("disk"));
        assert_eq!(unbound.result.unwrap(), golden("verdict-not-seed-bound"));
    }

    #[test]
    fn a_hash_revocation_uses_the_golden_refusal() {
        let response = map_verdict(&"cd".repeat(32), Err(RunRefusal::HashRevoked));
        let golden = golden("refusal-hash-revoked");
        assert_eq!(response.ok, golden["ok"]);
        assert_eq!(response.error.as_deref(), golden["error"].as_str());
        assert_eq!(response.error_kind.as_deref(), golden["error_kind"].as_str());
    }
}
