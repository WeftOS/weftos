//! `cog.licence.import`, `cog.licence.status`, `cog.licence.claims`, and
//! `cog.licence.revoked` for `weftos.cog.v1`.
//!
//! Each method is an exact name. There is no `cog.` or `cog.licence.` prefix
//! and no generic kernel call. The grant decision, the stores, and the
//! revocation list stay in this daemon. Cog Host only forwards records and
//! reads the results.
//!
//! `Receipt::New` covers both `Applied` and `AppliedUnsaved`, so an unsaved
//! restrictive record is reported as `applied`. `Receipt::Known` covers both
//! a duplicate and a stale `seq`, so the exchange path reports `duplicate`
//! for a record the store would have called `ignored`. The store fallback
//! still reports `ignored` for a lower `seq`.

use std::sync::Arc;

use clawft_kernel::licence::{
    AdmissionPosture, NoExtraChecks, Outcome, Receipt, SignedApproval, SignedBinding, SignedGrant,
};
use clawft_kernel::mesh_swarm_revoke::SignedRevocation;
use clawft_platform::NativePlatform;
use clawft_rpc::Response;
use serde_json::{Value, json};
use tokio::sync::RwLock;
use weftos_cog_protocol::{
    claims_value, import_result_value, revoked_value, status_value, ImportOutcome, StatusParts,
    CLAIMS_METHOD, IMPORT_METHOD, MAX_IMPORT_RECORDS, PROTOCOL, REVOKED_METHOD, STATUS_METHOD,
};

use crate::licence_boot::LicenceRuntime;
use crate::rpc_ext::{ExtCall, ExtFuture};

/// Methods this module serves, in census order.
pub const METHODS: &[&str] = &[IMPORT_METHOD, STATUS_METHOD, CLAIMS_METHOD, REVOKED_METHOD];

const POSTURE: AdmissionPosture = AdmissionPosture {
    enforce: true,
    verdict_source_bound: true,
    open_membership: false,
};

/// True when `method` is one of [`METHODS`].
pub fn handles(method: &str) -> bool {
    METHODS.contains(&method)
}

/// Extension-route entry. The role gate has already run.
pub fn handle(call: ExtCall) -> ExtFuture {
    Box::pin(async move {
        route(
            crate::licence_boot::holder_refusal(),
            crate::licence_boot::runtime(),
            &call.method,
            &call.params,
        )
        .await
    })
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
    route(crate::licence_boot::holder_refusal(), crate::licence_boot::runtime(), method, &params).await
}

async fn route(
    holder: Option<String>,
    runtime: Option<Arc<LicenceRuntime>>,
    method: &str,
    params: &Value,
) -> Response {
    if let Some(why) = holder {
        return Response::error_with_kind("not_holder", why);
    }
    let Some(runtime) = runtime else {
        return Response::error_with_kind(
            "binding_inactive",
            "the licence runtime is not initialised on this node",
        );
    };
    if let Err(response) = check_protocol(params) {
        return response;
    }
    match method {
        IMPORT_METHOD => {
            if let Some(response) = import_cap_error(params) {
                return response;
            }
            import(&runtime, params).await
        }
        STATUS_METHOD => status(&runtime),
        CLAIMS_METHOD => claims(&runtime, params),
        REVOKED_METHOD => revoked(&runtime, params),
        other => Response::error(format!("unknown method: {other}")),
    }
}

fn check_protocol(params: &Value) -> Result<(), Response> {
    let protocol = params.get("protocol").and_then(Value::as_str).unwrap_or("");
    if protocol != PROTOCOL {
        return Err(Response::error_with_kind(
            "version_mismatch",
            format!("params protocol is {protocol}"),
        ));
    }
    Ok(())
}

/// Whole-call refusal when one kind exceeds [`MAX_IMPORT_RECORDS`].
pub(crate) fn import_cap_error(params: &Value) -> Option<Response> {
    let over = ["grants", "approvals", "revocations"].into_iter().any(|key| {
        params.get(key).and_then(Value::as_array).map(|rows| rows.len()).unwrap_or(0) > MAX_IMPORT_RECORDS
    });
    if over {
        Some(Response::error_with_kind(
            "malformed_reply",
            format!("at most {MAX_IMPORT_RECORDS} records of each kind per import"),
        ))
    } else {
        None
    }
}

fn rows<'a>(params: &'a Value, key: &str) -> Result<&'a [Value], Response> {
    match params.get(key) {
        None => Ok(&[]),
        Some(Value::Array(rows)) => Ok(rows.as_slice()),
        Some(_) => Err(Response::error_with_kind("malformed_reply", format!("{key} must be an array"))),
    }
}

async fn import(runtime: &LicenceRuntime, params: &Value) -> Response {
    let revocations = match rows(params, "revocations") {
        Ok(rows) => rows,
        Err(response) => return response,
    };
    let grants = match rows(params, "grants") {
        Ok(rows) => rows,
        Err(response) => return response,
    };
    let approvals = match rows(params, "approvals") {
        Ok(rows) => rows,
        Err(response) => return response,
    };
    if let Some(binding) = params.get("binding").filter(|value| !value.is_null()) {
        if !binding.is_object() {
            return Response::error_with_kind("malformed_reply", "binding must be an object");
        }
    }
    let exchange = crate::workload_place_rpc::licence_exchange();
    let mut lines = Vec::new();
    for row in revocations {
        lines.push(apply_revocation(runtime, row));
    }
    if let Some(binding) = params.get("binding").filter(|value| !value.is_null()) {
        lines.push(apply_binding(runtime, exchange.as_deref(), binding).await);
    }
    for row in grants {
        lines.push(apply_grant(runtime, exchange.as_deref(), row).await);
    }
    for row in approvals {
        lines.push(apply_approval(exchange.as_deref(), row).await);
    }
    Response::success(import_result_value(&lines))
}

fn apply_revocation(runtime: &LicenceRuntime, value: &Value) -> ImportOutcome {
    let signed: SignedRevocation = match serde_json::from_value(value.clone()) {
        Ok(signed) => signed,
        Err(err) => return refused("revocation", err.to_string()),
    };
    match runtime.store().apply_operator_revocation(&signed) {
        Ok(true) => applied("revocation"),
        Ok(false) => duplicate("revocation"),
        Err(err) => refused("revocation", err),
    }
}

async fn apply_binding(
    runtime: &LicenceRuntime,
    exchange: Option<&clawft_kernel::licence::LicenceExchange>,
    value: &Value,
) -> ImportOutcome {
    let signed: SignedBinding = match serde_json::from_value(value.clone()) {
        Ok(signed) => signed,
        Err(err) => return refused("binding", err.to_string()),
    };
    if let Some(exchange) = exchange {
        from_receipt("binding", exchange.issue_binding(signed).await.map_err(|err| err.to_string()))
    } else {
        from_outcome(
            "binding",
            runtime.store().accept_binding(&signed, POSTURE, &NoExtraChecks).map_err(|err| err.to_string()),
        )
    }
}

async fn apply_grant(
    runtime: &LicenceRuntime,
    exchange: Option<&clawft_kernel::licence::LicenceExchange>,
    value: &Value,
) -> ImportOutcome {
    let signed: SignedGrant = match serde_json::from_value(value.clone()) {
        Ok(signed) => signed,
        Err(err) => return refused("grant", err.to_string()),
    };
    if let Some(exchange) = exchange {
        from_receipt("grant", exchange.issue_grant(signed).await.map_err(|err| err.to_string()))
    } else {
        from_outcome("grant", runtime.store().accept_grant(&signed).map_err(|err| err.to_string()))
    }
}

async fn apply_approval(
    exchange: Option<&clawft_kernel::licence::LicenceExchange>,
    value: &Value,
) -> ImportOutcome {
    let signed: SignedApproval = match serde_json::from_value(value.clone()) {
        Ok(signed) => signed,
        Err(err) => return refused("approval", err.to_string()),
    };
    let Some(exchange) = exchange else {
        return refused("approval", "approval store is not initialised".into());
    };
    from_receipt("approval", exchange.issue_approval(signed).await.map_err(|err| err.to_string()))
}

fn status(runtime: &LicenceRuntime) -> Response {
    let store = runtime.store();
    let exchange = crate::workload_place_rpc::licence_exchange();
    let approval_store = exchange.as_ref().map(|ex| ex.approvals().clone());
    let store_error = match &approval_store {
        Some(approvals) => store.poisoned().or_else(|| approvals.poisoned()),
        None => store.poisoned(),
    };
    let binding = store.held_binding().and_then(|record| serde_json::to_value(record).ok());
    let grants = serde_json::to_value(store.grant_rows()).unwrap_or_else(|_| json!([]));
    let approvals = approval_store
        .as_ref()
        .map(|store| serde_json::to_value(store.rows()).unwrap_or_else(|_| json!([])))
        .unwrap_or_else(|| json!([]));
    let mesh = store.local_mesh_id().get().map(|id| id.to_hex());
    let revocations_error = store.revocation_list_error();
    Response::success(status_value(&StatusParts {
        approvals: &approvals,
        binding: binding.as_ref(),
        binding_in_effect: store.binding_status().is_ok(),
        grants: &grants,
        list_generation: store.revocation_generation(),
        mesh_id: mesh.as_deref(),
        revocations_error: revocations_error.as_deref(),
        revoked_artifacts: store.revoked_artifact_count() as u64,
        store_error: store_error.as_deref(),
    }))
}

fn claims(runtime: &LicenceRuntime, params: &Value) -> Response {
    let list_error = runtime.store().revocation_list_error();
    let sha256 = params.get("sha256").and_then(Value::as_str).unwrap_or("");
    let blake3 = params.get("blake3").and_then(Value::as_str).unwrap_or("");
    let claimed = runtime.store().claims_artifact(sha256, blake3) || runtime.store().is_hash_revoked(blake3);
    claims_response(list_error.as_deref(), claimed)
}

fn claims_response(list_error: Option<&str>, claims: bool) -> Response {
    if let Some(err) = list_error {
        return list_unreadable(err);
    }
    Response::success(claims_value(claims))
}

fn revoked(runtime: &LicenceRuntime, params: &Value) -> Response {
    let list_error = runtime.store().revocation_list_error();
    let blake3 = params.get("blake3").and_then(Value::as_str).unwrap_or("");
    let flag = runtime.store().is_hash_revoked(blake3);
    let generation = runtime.store().revocation_generation();
    revoked_response(list_error.as_deref(), flag, generation)
}

fn revoked_response(list_error: Option<&str>, revoked: bool, list_generation: u64) -> Response {
    if let Some(err) = list_error {
        return list_unreadable(err);
    }
    Response::success(revoked_value(revoked, list_generation))
}

fn list_unreadable(err: &str) -> Response {
    Response::error_with_kind(
        "binding_inactive",
        format!("no Seed binding is in effect on this node (revocation list unreadable: {err})"),
    )
}

fn from_receipt(kind: &'static str, receipt: Result<Receipt, String>) -> ImportOutcome {
    match receipt {
        Ok(Receipt::New) => applied(kind),
        Ok(Receipt::Known) => duplicate(kind),
        Ok(Receipt::Deferred) => ignored(kind),
        Err(err) => refused(kind, err),
    }
}

fn from_outcome(kind: &'static str, outcome: Result<Outcome, String>) -> ImportOutcome {
    match outcome {
        Ok(Outcome::Applied | Outcome::AppliedUnsaved) => applied(kind),
        Ok(Outcome::Duplicate) => duplicate(kind),
        Ok(Outcome::Ignored) => ignored(kind),
        Err(err) => refused(kind, err),
    }
}

fn applied(kind: &'static str) -> ImportOutcome {
    ImportOutcome { kind, outcome: "applied", error: None }
}

fn duplicate(kind: &'static str) -> ImportOutcome {
    ImportOutcome { kind, outcome: "duplicate", error: None }
}

fn ignored(kind: &'static str) -> ImportOutcome {
    ImportOutcome { kind, outcome: "ignored", error: None }
}

fn refused(kind: &'static str, error: String) -> ImportOutcome {
    ImportOutcome { kind, outcome: "refused", error: Some(error) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn golden(name: &str) -> Value {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join(format!("../weftos-cog-protocol/testdata/{name}.json"));
        let text = std::fs::read_to_string(&path).unwrap_or_else(|err| panic!("{}: {err}", path.display()));
        serde_json::from_str(text.trim_end()).unwrap()
    }

    #[tokio::test]
    async fn a_missing_runtime_is_binding_inactive_even_when_the_protocol_is_wrong() {
        let params = json!({"protocol": "weftos.cog.v0"});
        let missing = route(None, None, STATUS_METHOD, &params).await;
        assert_eq!(missing.error_kind.as_deref(), Some("binding_inactive"));
        assert_eq!(
            missing.error.as_deref(),
            Some("the licence runtime is not initialised on this node")
        );
        let held = route(Some("not the holder".into()), None, IMPORT_METHOD, &json!({"protocol": PROTOCOL})).await;
        assert_eq!(held.error_kind.as_deref(), Some("not_holder"));
        assert_eq!(held.error.as_deref(), Some("not the holder"));
    }

    #[test]
    fn a_wrong_protocol_is_version_mismatch_and_the_cap_is_a_whole_call_refusal() {
        let err = check_protocol(&json!({"protocol": "weftos.cog.v0"})).unwrap_err();
        assert_eq!(err.error_kind.as_deref(), Some("version_mismatch"));
        assert!(import_cap_error(&json!({"grants": []})).is_none());
        let too_many = json!({"approvals": vec![json!({}); MAX_IMPORT_RECORDS + 1]});
        let err = import_cap_error(&too_many).unwrap();
        assert_eq!(err.error_kind.as_deref(), Some("malformed_reply"));
        assert!(err.error.unwrap().contains("512"));
    }

    #[test]
    fn empty_results_match_the_shared_goldens() {
        let approvals = json!([]);
        let grants = json!([]);
        let status = status_value(&StatusParts {
            approvals: &approvals,
            binding: None,
            binding_in_effect: false,
            grants: &grants,
            list_generation: 0,
            mesh_id: None,
            revocations_error: None,
            revoked_artifacts: 0,
            store_error: None,
        });
        assert_eq!(status, golden("licence-status-empty"));
        assert_eq!(claims_response(None, false).result.unwrap(), golden("licence-claims-false"));
        assert_eq!(revoked_response(None, false, 0).result.unwrap(), golden("licence-revoked-false"));
        assert_eq!(import_result_value(&[applied("binding")]), golden("licence-import-result"));
    }

    #[test]
    fn an_unreadable_list_fails_claims_and_revoked_closed() {
        let claims = claims_response(Some("disk"), false);
        assert_eq!(claims.error_kind.as_deref(), Some("binding_inactive"));
        assert!(claims.error.unwrap().contains("revocation list unreadable: disk"));
        let revoked = revoked_response(Some("disk"), false, 0);
        assert_eq!(revoked.error_kind.as_deref(), Some("binding_inactive"));
    }

    #[test]
    fn receipts_and_store_outcomes_use_the_host_line_spellings() {
        assert_eq!(from_receipt("grant", Ok(Receipt::New)).outcome, "applied");
        assert_eq!(from_receipt("grant", Ok(Receipt::Known)).outcome, "duplicate");
        assert_eq!(from_receipt("grant", Ok(Receipt::Deferred)).outcome, "ignored");
        assert_eq!(from_outcome("grant", Ok(Outcome::AppliedUnsaved)).outcome, "applied");
        assert_eq!(from_outcome("grant", Ok(Outcome::Ignored)).outcome, "ignored");
        let refused = from_outcome("grant", Err("bad signature".into()));
        assert_eq!(refused.outcome, "refused");
        assert_eq!(refused.error.as_deref(), Some("bad signature"));
    }
}
