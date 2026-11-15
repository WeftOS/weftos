//! Governance parent push, update and reload (ADR-103 A6, D8, Phase 2 package E).
//!
//! | method | runs on | capability | what it does |
//! |---|---|---|---|
//! | `governance.parent.push` `{project_id, limits?}` | user daemon | `Admin` | snapshots the daemon's governance engine, signs it with the user key (the chain signing key, plan D-1), writes `<run>/<id>/parent-policy.json` and, when the child is running, calls its `governance.parent.update` |
//! | `governance.parent.update` `{policy}` | project kernel | `Admin` | accepts the policy only with a valid user signature and a version no older than the newest accepted; swaps the rules, appends `governance.overlay.applied` |
//! | `governance.reload` | project kernel | `Admin` | re-reads `overlay.toml` (and a newer parent policy file); the only way a disk edit takes effect |
//!
//! The child never calls the parent for policy: the direction is parent to
//! child only. A rejected update or reload leaves the running rules as they
//! were and appends `governance.overlay.rejected`.

// `clawft_rpc::Response` is the ready-made refusal returned as the `Err` early-out of these
// handlers; it is built once per refused request, so its size is not on a hot path.
#![allow(clippy::result_large_err)]

use std::path::Path;

use clawft_rpc::Response;
use clawft_types::config::overlay::Limits;
use serde_json::Value;

fn invalid(msg: impl Into<String>) -> Response {
    Response::error_with_kind("invalid_params", msg)
}

/// Parse the optional `limits` object of a push.
pub fn parse_limits(params: &Value) -> Result<Limits, String> {
    match params.get("limits") {
        None | Some(Value::Null) => Ok(Limits::default()),
        Some(v) => serde_json::from_value(v.clone()).map_err(|e| format!("`limits`: {e}")),
    }
}

/// `base` with `extra` laid on tighten-only: numbers take the smaller, the
/// approval flag is OR. An explicit push `limits` can never raise a cap the
/// parent snapshot carries.
pub fn tighten_limits(base: &Limits, extra: &Limits) -> Limits {
    fn lo<T: PartialOrd + Copy>(a: Option<T>, b: Option<T>) -> Option<T> {
        match (a, b) {
            (Some(x), Some(y)) => Some(if y < x { y } else { x }),
            (x, None) => x,
            (None, y) => y,
        }
    }
    Limits {
        risk_threshold: lo(base.risk_threshold, extra.risk_threshold),
        max_processes: lo(base.max_processes, extra.max_processes),
        spawn_budget: lo(base.spawn_budget, extra.spawn_budget),
        human_approval_required: match (base.human_approval_required, extra.human_approval_required) {
            (Some(true), _) | (_, Some(true)) => Some(true),
            (a, b) => a.or(b),
        },
    }
}

/// The project must be registered with this user daemon before it is sent
/// policy. (Revocation is checked by the child itself: it refuses policy once
/// the user daemon has dropped the `revoked` marker in its run dir.)
pub fn registered(dir: &Path, id: &str) -> Result<(), Response> {
    match clawft_types::project::find_by_id(dir, id) {
        Ok(Some(_)) => Ok(()),
        Ok(None) => Err(Response::error_with_kind(
            "project_not_found",
            format!("project {id} is not registered with this daemon"),
        )),
        Err(e) => Err(Response::error_with_kind("project_error", e.to_string())),
    }
}

/// `<run_root>/<id>`, refusing an id that is not a single safe path part.
pub fn run_dir(run_root: &Path, id: &str) -> Result<std::path::PathBuf, String> {
    clawft_types::project::validate_id(id).map_err(|_| "`project_id` is not a project id".to_owned())?;
    Ok(run_root.join(id))
}

/// The user daemon's run root, or the refusal when there is none (not the
/// user daemon). No fallback: a policy written anywhere else is a file no
/// child reads.
pub fn require_run_root(root: Option<std::path::PathBuf>) -> Result<std::path::PathBuf, Response> {
    root.ok_or_else(|| {
        Response::error_with_kind(
            "no_run_root",
            "no user-daemon run root: governance.parent.push runs only on the user daemon",
        )
    })
}

#[cfg(feature = "exochain")]
pub use imp::{handle_push, handle_reload, handle_update, push_policy};

#[cfg(feature = "exochain")]
mod imp {
    use std::path::Path;

    use clawft_kernel::gate::GovernanceSnapshot;
    use clawft_kernel::parent_policy::{
        ParentPolicy, ParentPolicyError, export_rules_to,
    };
    use clawft_rpc::{DaemonClient, Request, Response};
    use clawft_types::config::overlay::Limits;
    use clawft_types::runtime_paths::{PARENT_POLICY_FILE, SOCKET_NAME};
    use ed25519_dalek::SigningKey;
    use serde_json::{Value, json};

    use super::{invalid, parse_limits, run_dir};
    use crate::rpc_ext::{ExtCall, ExtFuture};

    fn refused(kind: &str, e: &clawft_kernel::governance_overlay::OverlayError) -> Response {
        Response::error_with_kind(kind, format!("`{}`: {e}", e.key()))
    }

    /// `governance.reload` (project kernel).
    pub fn handle_reload(call: ExtCall) -> ExtFuture {
        Box::pin(async move {
            #[cfg(unix)]
            if let Err(e) = crate::nested_rpc::quiesce().await {
                return Response::error_with_kind("nested_stop_failed", e.to_string());
            }
            let Some(rt) = call.ctx.kernel.read().await.governance_overlay().cloned() else {
                return Response::error_with_kind(
                    "not_a_project_kernel",
                    "governance.reload runs on a project kernel",
                );
            };
            match rt.reload() {
                Ok(a) => Response::success(a.to_json()),
                Err(e) => refused("overlay_rejected", &e),
            }
        })
    }

    /// `governance.parent.update` (project kernel).
    pub fn handle_update(call: ExtCall) -> ExtFuture {
        Box::pin(async move {
            #[cfg(unix)]
            if let Err(e) = crate::nested_rpc::quiesce().await {
                return Response::error_with_kind("nested_stop_failed", e.to_string());
            }
            let Some(rt) = call.ctx.kernel.read().await.governance_overlay().cloned() else {
                return Response::error_with_kind(
                    "not_a_project_kernel",
                    "governance.parent.update runs on a project kernel",
                );
            };
            let Some(policy) = call.params.get("policy") else {
                return invalid("governance.parent.update needs `policy`");
            };
            let policy: ParentPolicy = match serde_json::from_value(policy.clone()) {
                Ok(p) => p,
                Err(e) => return invalid(format!("`policy`: {e}")),
            };
            match rt.apply_parent_update(policy) {
                Ok(a) => Response::success(a.to_json()),
                Err(e) => refused("parent_policy_rejected", &e),
            }
        })
    }

    /// Sign `snapshot` and write `<run_root>/<id>/parent-policy.json`; when
    /// the child's socket answers, push the policy to it. The reply says
    /// whether the child applied it.
    pub async fn push_policy(
        run_root: &Path,
        id: &str,
        snapshot: GovernanceSnapshot,
        limits: &Limits,
        key: &SigningKey,
    ) -> Result<Value, Response> {
        let dir = run_dir(run_root, id).map_err(invalid)?;
        let policy = export_rules_to(
            &dir.join(PARENT_POLICY_FILE),
            snapshot.rules,
            snapshot.risk_threshold,
            snapshot.human_approval_required,
            &super::tighten_limits(&snapshot.limits, limits),
            key,
        )
        .map_err(|e: ParentPolicyError| {
            Response::error_with_kind("parent_policy_export_failed", format!("`{}`: {e}", e.key()))
        })?;
        let mut out = json!({
            "project_id": id,
            "version": policy.version,
            "rule_hash": policy.rule_hash,
            "written": true,
            "pushed": false,
        });
        let sock = dir.join(SOCKET_NAME);
        let Some(mut client) = DaemonClient::connect_path(&sock).await else {
            out["note"] = json!("child is not running; it reads the policy at its next boot");
            return Ok(out);
        };
        let req = Request::with_params("governance.parent.update", json!({ "policy": policy }));
        match client.call(req).await {
            Ok(r) if r.ok => {
                out["pushed"] = json!(true);
                out["applied"] = r.result.unwrap_or(Value::Null);
            }
            Ok(r) => {
                out["child_error"] = json!({"kind": r.error_kind, "message": r.error});
            }
            Err(e) => out["child_error"] = json!({"message": e.to_string()}),
        }
        Ok(out)
    }

    /// `governance.parent.push` (user daemon).
    pub fn handle_push(call: ExtCall) -> ExtFuture {
        Box::pin(async move {
            let Some(id) = call.params.get("project_id").and_then(Value::as_str) else {
                return invalid("governance.parent.push needs `project_id`");
            };
            match crate::project_rpc::configured_dir() {
                Some(dir) => {
                    if let Err(r) = super::registered(&dir, id) {
                        return r;
                    }
                }
                None => {
                    return Response::error_with_kind(
                        "project_store_unavailable",
                        "cannot locate the project manifest store",
                    );
                }
            }
            let limits = match parse_limits(&call.params) {
                Ok(l) => l,
                Err(m) => return invalid(m),
            };
            let (snapshot, key) = {
                let k = call.ctx.kernel.read().await;
                let snap = k.governance_gate().and_then(|g| g.governance_snapshot()).map(|mut s| {
                    s.limits = clawft_kernel::gate::parent_limits_of(k.kernel_config());
                    s
                });
                let key = k.chain_manager().and_then(|c| c.signing_key_clone());
                (snap, key)
            };
            let Some(snapshot) = snapshot else {
                return Response::error_with_kind(
                    "no_governance_engine",
                    "this daemon has no governance engine to export",
                );
            };
            let Some(key) = key else {
                return Response::error_with_kind(
                    "no_user_key",
                    "this daemon has no chain signing key to sign the policy with",
                );
            };
            // The supervisor's run root, where the child reads its policy.
            let run_root = match super::require_run_root(crate::project_cert_rpc::user_run_root()) {
                Ok(r) => r,
                Err(r) => return r,
            };
            match push_policy(&run_root, id, snapshot, &limits, &key).await {
                Ok(v) => Response::success(v),
                Err(r) => r,
            }
        })
    }
}

/// Without `exochain` there is no governance engine or chain.
#[cfg(not(feature = "exochain"))]
mod stubs {
    use super::*;
    use crate::rpc_ext::{ExtCall, ExtFuture};
    fn off(_c: ExtCall) -> ExtFuture {
        Box::pin(async { Response::error("exochain feature not enabled") })
    }
    pub fn handle_push(c: ExtCall) -> ExtFuture {
        off(c)
    }
    pub fn handle_update(c: ExtCall) -> ExtFuture {
        off(c)
    }
    pub fn handle_reload(c: ExtCall) -> ExtFuture {
        off(c)
    }
}
#[cfg(not(feature = "exochain"))]
pub use stubs::{handle_push, handle_reload, handle_update};

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn limits_parse_and_reject_unknown_keys() {
        assert_eq!(parse_limits(&json!({})).unwrap(), Limits::default());
        let l = parse_limits(&json!({"limits": {"max_processes": 4}})).unwrap();
        assert_eq!(l.max_processes, Some(4));
        assert!(parse_limits(&json!({"limits": {"max_procs": 4}})).is_err());
    }

    #[test]
    fn push_requires_a_registered_project() {
        let t = tempfile::tempdir().unwrap();
        let mdir = t.path().join("projects");
        let proj = t.path().join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let reg = crate::project_rpc::register(&mdir, None, &json!({"root": proj.to_str().unwrap()}));
        assert!(reg.ok, "{:?}", reg.error);
        let id = reg.result.unwrap()["project"]["id"].as_str().unwrap().to_owned();
        assert!(registered(&mdir, &id).is_ok());
        let other = clawft_types::project::new_id();
        let r = registered(&mdir, &other).unwrap_err();
        assert_eq!(r.error_kind.as_deref(), Some("project_not_found"));
    }

    #[test]
    fn push_without_a_run_root_is_refused_not_redirected() {
        let r = require_run_root(None).unwrap_err();
        assert_eq!(r.error_kind.as_deref(), Some("no_run_root"));
        assert_eq!(require_run_root(Some("/r".into())).unwrap(), Path::new("/r"));
    }

    #[test]
    fn run_dir_refuses_path_tricks() {
        let root = Path::new("/r");
        assert!(run_dir(root, "../x").is_err());
        assert!(run_dir(root, "").is_err());
        assert!(run_dir(root, "01JB8Z3Q0V6X9KQ4M2N7T5R1WD").is_ok());
    }

    #[cfg(all(unix, feature = "exochain"))]
    mod push {
        use super::*;
        use clawft_kernel::gate::GovernanceSnapshot;
        use clawft_kernel::governance::{
            GovernanceBranch, GovernanceRule, GovernanceRuleType, RuleSeverity,
        };
        use clawft_kernel::parent_policy::{ParentPolicy, verify_parent_policy};
        use ed25519_dalek::SigningKey;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        const ID: &str = "01JB8Z3Q0V6X9KQ4M2N7T5R1WD";

        fn snapshot() -> GovernanceSnapshot {
            GovernanceSnapshot {
                rules: vec![GovernanceRule {
                    id: "GOV-1".into(),
                    description: "d".into(),
                    branch: GovernanceBranch::Judicial,
                    severity: RuleSeverity::Blocking,
                    active: true,
                    reference_url: None,
                    sop_category: None,
                    rule_type: GovernanceRuleType::General,
                    action_selector: Some("net.*".into()),
                    tool_selector: None,
                    force_on_match: true,
                    principal_selector: None,
                }],
                risk_threshold: 0.8,
                human_approval_required: false,
                limits: Default::default(),
            }
        }

        #[tokio::test]
        async fn push_writes_a_signed_private_policy_and_calls_the_child() {
            let t = tempfile::tempdir().unwrap();
            let run = t.path().join("r");
            let dir = run.join(ID);
            std::fs::create_dir_all(&dir).unwrap();
            let key = SigningKey::from_bytes(&[7u8; 32]);

            // A fake child answering one `governance.parent.update`.
            let listener = tokio::net::UnixListener::bind(dir.join("kernel.sock")).unwrap();
            let child = tokio::spawn(async move {
                let (mut s, _) = listener.accept().await.unwrap();
                let mut line = String::new();
                BufReader::new(&mut s).read_line(&mut line).await.unwrap();
                s.write_all(b"{\"ok\":true,\"result\":{\"effective_hash\":\"ab\"}}\n")
                    .await
                    .unwrap();
                line
            });

            let out = push_policy(&run, ID, snapshot(), &Limits::default(), &key)
                .await
                .map_err(|r| r.error)
                .unwrap();
            assert_eq!(out["pushed"], true);
            assert_eq!(out["applied"]["effective_hash"], "ab");

            // The request carried a policy that verifies against the user key.
            let req: clawft_rpc::Request = serde_json::from_str(&child.await.unwrap()).unwrap();
            assert_eq!(req.method, "governance.parent.update");
            let sent: ParentPolicy = serde_json::from_value(req.params["policy"].clone()).unwrap();
            verify_parent_policy(&sent, &key.verifying_key().to_bytes()).unwrap();

            // The file on disk is the same policy, mode 0600.
            let path = dir.join("parent-policy.json");
            let disk: ParentPolicy =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            assert_eq!(disk.sig, sent.sig);
            assert_eq!(out["version"], disk.version);
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
                assert_eq!(mode, 0o600);
            }
        }

        #[tokio::test]
        async fn a_push_without_limits_keeps_the_parent_caps_and_explicit_limits_only_tighten() {
            let t = tempfile::tempdir().unwrap();
            let key = SigningKey::from_bytes(&[7u8; 32]);
            let mut snap = snapshot();
            snap.limits = Limits { max_processes: Some(12), spawn_budget: Some(3), ..Default::default() };
            push_policy(t.path(), ID, snap.clone(), &Limits::default(), &key).await.map_err(|r| r.error).unwrap();
            let read = || -> ParentPolicy {
                serde_json::from_slice(&std::fs::read(t.path().join(ID).join("parent-policy.json")).unwrap()).unwrap()
            };
            assert_eq!(read().limits.max_processes, Some(12));
            assert_eq!(read().limits.spawn_budget, Some(3));
            let ask = Limits { max_processes: Some(99), spawn_budget: Some(1), ..Default::default() };
            push_policy(t.path(), ID, snap, &ask, &key).await.map_err(|r| r.error).unwrap();
            assert_eq!(read().limits.max_processes, Some(12), "an explicit push cannot raise a parent cap");
            assert_eq!(read().limits.spawn_budget, Some(1));
        }

        #[tokio::test]
        async fn push_to_a_stopped_child_still_writes_the_policy() {
            let t = tempfile::tempdir().unwrap();
            let key = SigningKey::from_bytes(&[7u8; 32]);
            let out = push_policy(t.path(), ID, snapshot(), &Limits::default(), &key)
                .await
                .map_err(|r| r.error)
                .unwrap();
            assert_eq!(out["written"], true);
            assert_eq!(out["pushed"], false);
            assert!(t.path().join(ID).join("parent-policy.json").exists());
        }

        #[tokio::test]
        async fn push_refuses_an_id_that_is_not_a_project_id() {
            let t = tempfile::tempdir().unwrap();
            let key = SigningKey::from_bytes(&[7u8; 32]);
            for bad in ["..", "../x", "a/b", ""] {
                let r = push_policy(t.path(), bad, snapshot(), &Limits::default(), &key).await;
                assert!(r.is_err(), "{bad:?}");
            }
        }
    }
}
