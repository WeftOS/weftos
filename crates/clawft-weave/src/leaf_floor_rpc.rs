//! `leaf.reset_floor` (card e821c460): recover a reflashed leaf.
//!
//! A full flash wipes the device's sequence counter, so the parent refuses
//! its first frame (`expected 44, got 1`) against the durable replay floor
//! (ADR-103). `weaver leaf reset-floor <id> --confirm` removes that one floor
//! so the sequence may restart at 1; replay protection then runs again from
//! the new floor.
//!
//! Frames bind no epoch and the leaf listener is plaintext, so a reset alone
//! would let anyone who recorded the old frames 1..N replay them. The reset
//! is therefore allowed only after the leaf was re-issued a certificate with
//! a new serial and re-enrolled (same key, so the same `leaf_id`): every
//! frame signed under the old certificate then fails
//! `enrolled.cert == frame.cert`.
//!
//! | method | capability | params -> result |
//! |---|---|---|
//! | `leaf.reset_floor` | Admin | `{leaf_id, confirm: true}` -> `{leaf_id, old_seq, reset}` |
//!
//! Outside a project this is a user-level Admin act (`scope_gate`). Refusals
//! carry an `error_kind`: `leaf_bad_id`, `leaf_not_confirmed` (the caller must
//! say the pad was reflashed: a pad that was not would be locked out),
//! `leaf_no_ingress`, `leaf_connected` (the leaf holds a route, or a leaf
//! connection has not yet named itself: `leaf_connecting`), `leaf_no_chain`
//! (an unrecorded reset is refused), `leaf_cert_unchanged` /
//! `leaf_floor_unbound` / `leaf_not_enrolled` (the certificate rule above),
//! `leaf_chain_not_durable` (the record could not be made durable, so nothing
//! was removed). The reset is recorded on the chain as `leaf.floor.reset`
//! (who, leaf, old floor, time) and the chain is made durable on disk before
//! the floor is removed; a removal that then fails is recorded as
//! `leaf.floor.reset_failed`.

use serde::Deserialize;
#[cfg(feature = "exochain")]
use serde_json::json;

use crate::protocol::Response;
use crate::rpc_ext::{ExtCall, ExtFuture};

/// Chain source of both events.
pub const SOURCE: &str = "leaf";
/// Chain kind recorded before the floor is removed.
pub const KIND_RESET: &str = "leaf.floor.reset";
/// Chain kind recorded when the removal then failed.
pub const KIND_FAILED: &str = "leaf.floor.reset_failed";

#[derive(Deserialize)]
struct Params {
    leaf_id: String,
    /// The caller states the pad was reflashed (its counter restarted).
    #[serde(default)]
    confirm: bool,
}

pub fn handle(call: ExtCall) -> ExtFuture {
    Box::pin(async move {
        match call.method.as_str() {
            "leaf.reset_floor" => reset(&call).await,
            other => Response::error(format!("unknown leaf method {other}")),
        }
    })
}

async fn reset(call: &ExtCall) -> Response {
    let p: Params = match serde_json::from_value(call.params.clone()) {
        Ok(p) => p,
        Err(e) => return Response::error(format!("invalid params: {e}")),
    };
    if !clawft_kernel::mesh_leaf::is_leaf_id(&p.leaf_id) {
        return Response::error_with_kind("leaf_bad_id", "leaf_id must be 32 lowercase hex characters");
    }
    if !p.confirm {
        return Response::error_with_kind(
            "leaf_not_confirmed",
            "pass confirm: true (--confirm) to state this pad was reflashed and its counter restarted; \
             resetting a pad that still holds its counter locks it out",
        );
    }
    let id = p.leaf_id;
    let k = call.ctx.kernel.read().await;
    let Some(rt) = k.a2a_router().mesh_runtime().cloned() else {
        return Response::error_with_kind("leaf_no_ingress", "this daemon runs no leaf listener");
    };
    let Some(ingress) = rt.leaf_ingress().cloned() else {
        return Response::error_with_kind("leaf_no_ingress", "this daemon runs no leaf listener");
    };
    #[cfg(feature = "exochain")]
    let chain = k.chain_manager().cloned();
    drop(k);

    // The same lock serialises a leaf's check, delivery and floor commit, so
    // no frame is mid-commit while the floor is removed.
    let _guard = ingress.lock.lock().await;
    let topic = weftos_leaf_types::push_topic(&id);
    if rt.peer_ids().contains(&id) || !rt.peers_for_topic(&topic).is_empty() {
        return Response::error_with_kind(
            "leaf_connected",
            format!("leaf {id} is connected; disconnect it (power it off) before resetting its replay floor"),
        );
    }
    // A leaf connection learns its id only from its first frame, so one that
    // has connected but not yet spoken cannot be matched to this leaf.
    if rt.leaf_pending() > 0 {
        return Response::error_with_kind(
            "leaf_connecting",
            "a leaf connection has not yet identified itself; power the pad off, wait for it to drop and retry",
        );
    }
    #[cfg(feature = "exochain")]
    let Some(chain) = chain else {
        return Response::error_with_kind("leaf_no_chain", "no chain is running; a reset must be recorded");
    };
    #[cfg(not(feature = "exochain"))]
    return Response::error_with_kind("leaf_no_chain", "built without a chain; a reset must be recorded");

    #[cfg(feature = "exochain")]
    {
        use clawft_kernel::mesh_leaf::LeafError;
        // Every refusal comes before anything is recorded.
        let old = match ingress.check_reset(&id) {
            Ok(Some(old)) => old,
            Ok(None) => return Response::success(json!({ "leaf_id": id, "old_seq": null, "reset": false })),
            Err(e) => {
                let kind = match e {
                    LeafError::CertUnchanged => "leaf_cert_unchanged",
                    LeafError::FloorUnbound => "leaf_floor_unbound",
                    LeafError::NotEnrolled => "leaf_not_enrolled",
                    _ => "leaf_reset_failed",
                };
                return Response::error_with_kind(kind, e.to_string());
            }
        };
        let payload = json!({
            "leaf_id": id,
            "by": call.ctx.peer_key.clone().unwrap_or_else(|| "local".into()),
            "connection": call.ctx.connection,
            "project": call.ctx.verified_project.as_ref().map(|p| p.as_str()),
            "authenticated": call.ctx.auth.is_some(),
            "old_seq": old.0,
            "old_digest": hex_lower(&old.1),
            "at": chrono::Utc::now().to_rfc3339(),
        });
        chain.append(SOURCE, KIND_RESET, Some(payload.clone()));
        // The record is on disk before the floor goes: a crash after the
        // removal can never leave a reset that the chain does not show.
        let durable = call.ctx.kernel.read().await.persist_chain_durable();
        if let Err(e) = durable {
            let mut failed = payload;
            failed["error"] = json!(e);
            chain.append(SOURCE, KIND_FAILED, Some(failed));
            return Response::error_with_kind(
                "leaf_chain_not_durable",
                format!("the reset could not be made durable on the chain, so the floor was not removed: {e}"),
            );
        }
        if let Err(e) = ingress.reset_floor(&id) {
            let mut failed = payload;
            failed["error"] = json!(e.to_string());
            chain.append(SOURCE, KIND_FAILED, Some(failed));
            let _ = call.ctx.kernel.read().await.persist_chain_durable();
            return Response::error_with_kind("leaf_reset_failed", format!("could not remove the floor: {e}"));
        }
        Response::success(json!({ "leaf_id": id, "old_seq": old.0, "reset": true }))
    }
}

#[cfg(feature = "exochain")]
fn hex_lower(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[cfg(test)]
#[path = "leaf_floor_rpc_tests.rs"]
mod tests;
